//! link モードの結合テスト（Linux のみ・AF_PACKET が必要）。
//!
//! 権限が必要なので `#[ignore]`。raw-ip のテストと同じく `unshare -rn` の中で `--ignored` を付けて動かす。
//! ループバック（lo）では MAC はすべて 0 で、ARP は使わない（ARP と実際の MAC は scripts/link_veth_check.sh）。
//!
//! AF_PACKET から入れたフレームには経路の情報が付いていないので、カーネルは受信側の経路判定を行う。
//! 127.0.0.1 あて・自分のアドレスが送信元のパケットはその判定で捨てられるため、テストの名前空間の中でだけ
//! lo の `route_localnet` と `accept_local` を 1 にする（実際の運用で別のホストに送るときは不要）:
//!
//! ```sh
//! unshare -rn sh -c 'ip link set lo up && echo 1 > /proc/sys/net/ipv4/conf/lo/route_localnet //!   && echo 1 > /proc/sys/net/ipv4/conf/lo/accept_local && <テストバイナリ> --ignored'
//! ```
#![cfg(target_os = "linux")]

use std::io::Read;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use application::port::{ClockPort, PacketPort};
use application::timestamping::TimestampingStatus;
use application::usecase::query_once;
use domain::packet_crafting::{
    ETHERTYPE_IPV4, HeaderTemplate, Ipv4Spec, NtpMessage, RequestTemplate, decode_ethernet,
    decode_ipv4_udp, encode, internet_checksum, udp_checksum,
};
use domain::shared_kernel::{NtpTimestamp, TimeDelta};
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::sim_server::{SimServer, SimServerConfig};
use infrastructure::transport::{
    self, LinkOptions, LinkTransport, TimestampingMode, TransportMode, TransportOptions,
};
use nix::sys::socket::{AddressFamily, SockFlag, SockProtocol, SockType, socket};
use socket2::Socket;

const NEEDS_RAW: &str = "AF_PACKET が必要（root か unshare -rn の中で --ignored を付けて実行する）";

/// 送られた IPv4 パケット（IP ヘッダから）。
struct CapturedIp {
    bytes: Vec<u8>,
}

/// すべてのフレームを受け取る AF_PACKET のソケット（送信の写しも見える）。
fn capture_socket() -> Socket {
    let fd = socket(
        AddressFamily::Packet,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::EthAll,
    )
    .unwrap();
    let socket = Socket::from(fd);
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    socket
}

/// `destination` あての IPv4 / UDP パケットを、フレームの中から探す。
fn find_ip_to(capture: &Socket, destination: SocketAddrV4) -> Option<CapturedIp> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = vec![0u8; 65_535];
    while Instant::now() < deadline {
        let Ok(len) = (&*capture).read(&mut buf) else {
            continue;
        };
        let Ok(frame) = decode_ethernet(&buf[..len]) else {
            continue;
        };
        if frame.ethertype != ETHERTYPE_IPV4 {
            continue;
        }
        if let Ok(packet) = decode_ipv4_udp(frame.payload)
            && packet.destination == destination
        {
            let total = usize::from(u16::from_be_bytes([frame.payload[2], frame.payload[3]]));
            return Some(CapturedIp {
                bytes: frame.payload[..total].to_vec(),
            });
        }
    }
    None
}

fn spawn_server(offset_ms: i64) -> (SimServer, SocketAddrV4) {
    let server = SimServer::spawn(
        "127.0.0.1:0".parse().unwrap(),
        SimServerConfig {
            offset: TimeDelta::from_millis(offset_ms),
            ..SimServerConfig::default()
        },
    )
    .unwrap();
    let SocketAddr::V4(addr) = server.local_addr() else {
        panic!("IPv4 ではない");
    };
    (server, addr)
}

#[test]
#[ignore = "AF_PACKET が必要（root か unshare -rn の中で --ignored を付けて実行する）"]
fn query_over_link_sends_ip_header_exactly_as_specified() {
    let _ = NEEDS_RAW;
    let (server, server_addr) = spawn_server(5);
    let capture = capture_socket();
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let headers = HeaderTemplate {
        ipv4: Ipv4Spec {
            dscp: 34,
            identification: 0, // raw-ip ではカーネルが値を入れるが、link ではそのまま 0 で届く
            ttl: 9,
            ..Ipv4Spec::default()
        },
        ..HeaderTemplate::default()
    };
    let opened = transport::open(
        TransportMode::Link,
        server.local_addr(),
        TransportOptions {
            headers: Some(headers),
            timestamping: TimestampingMode::User,
            ..TransportOptions::default()
        },
        Arc::clone(&clock),
        Arc::new(TimestampingStatus::new()),
    )
    .unwrap();
    assert!(opened.description.contains("lo"), "{}", opened.description);

    let outcome = query_once(
        opened.port,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    let offset_ms = outcome.sample.offset().delta().as_millis_f64();
    assert!((offset_ms - 5.0).abs() < 2.0, "offset = {offset_ms} ms");

    let request = find_ip_to(&capture, server_addr).expect("送信した要求が見つからない");
    let ip = &request.bytes;
    assert_eq!(ip[1] >> 2, 34, "DSCP");
    assert_eq!(&ip[4..6], &[0, 0], "Identification が 0 のまま届いていない");
    assert_eq!(ip[8], 9, "TTL");
    assert_eq!(
        internet_checksum(&ip[..20]),
        0,
        "IP ヘッダのチェックサムが正しくない"
    );
    let source = std::net::Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    assert_eq!(
        udp_checksum(source, *server_addr.ip(), &ip[20..]),
        0,
        "UDP チェックサムが正しくない"
    );
}

#[test]
#[ignore = "AF_PACKET が必要（root か unshare -rn の中で --ignored を付けて実行する）"]
fn fixed_ip_checksum_is_sent_unchanged() {
    let (server, server_addr) = spawn_server(0);
    let capture = capture_socket();
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let headers = HeaderTemplate {
        ipv4: Ipv4Spec {
            header_checksum: Some(0xBEEF),
            ..Ipv4Spec::default()
        },
        ..HeaderTemplate::default()
    };
    let mut transport = LinkTransport::connect(
        server.local_addr(),
        0,
        headers,
        LinkOptions::default(),
        clock,
    )
    .unwrap();
    // チェックサムが不正なのでサーバには届かない（届かないことが正しい）。送ったものだけを確かめる
    let request = RequestTemplate::default().build(NtpTimestamp::from_bits(1));
    transport.send(&encode(&request)).unwrap();

    let captured = find_ip_to(&capture, server_addr).expect("送信した要求が見つからない");
    assert_eq!(
        &captured.bytes[10..12],
        &[0xBE, 0xEF],
        "カーネルに書き換えられた"
    );
    assert_eq!(
        NtpMessage::MODE_CLIENT,
        captured.bytes[28] & 0b111,
        "NTP の要求ではない"
    );
}

#[test]
#[ignore = "AF_PACKET が必要（root か unshare -rn の中で --ignored を付けて実行する）"]
fn link_uses_kernel_time_for_both_send_and_receive() {
    let (server, _) = spawn_server(0);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let status = Arc::new(TimestampingStatus::new());
    let opened = transport::open(
        TransportMode::Link,
        server.local_addr(),
        TransportOptions {
            timestamping: TimestampingMode::Kernel,
            ..TransportOptions::default()
        },
        Arc::clone(&clock),
        Arc::clone(&status),
    )
    .unwrap();
    query_once(
        opened.port,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    let counts = status.counts();
    println!("link + カーネル: {counts:?}");
    assert_eq!(counts.kernel_samples, 1, "{counts:?}");
    assert_eq!(counts.tx_misses, 0, "送信時刻が取れていない: {counts:?}");
}
