//! raw-ip モードの結合テスト（Linux のみ・raw ソケットが必要）。
//!
//! 権限が必要なので、通常の `cargo test` では実行しない（`#[ignore]`）。
//! `sh scripts/privileged_tests.sh` がネットワーク名前空間の中で実行する（root 不要）。
#![cfg(target_os = "linux")]

extern crate alloc;

use alloc::sync::Arc;
use core::net::{SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::io::Read as _;
use std::net::UdpSocket;
use std::thread;
use std::time::Instant;

use application::port::ClockPort;
use application::usecase::{OnceOutcome, query_once};
use domain::packet_crafting::{
    HeaderTemplate, Ipv4Spec, NtpMessage, RequestTemplate, UdpChecksum, decode, decode_ipv4_udp,
    udp_checksum,
};
use domain::shared_kernel::TimeDelta;
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::sim_server::{SimServer, SimServerConfig};
use infrastructure::transport::RawIpTransport;
use socket2::{Domain, Protocol, Socket, Type};

/// 横から観察した IPv4 / UDP パケット。
struct Captured {
    ttl: u8,
    dscp: u8,
    identification: u16,
    dont_fragment: bool,
    source: SocketAddrV4,
    destination: SocketAddrV4,
    udp_segment: Vec<u8>,
    payload: Vec<u8>,
}

/// `capture` で受け取った UDP のうち、条件に合う最初のものを返す。
fn find_packet(capture: &Socket, wanted: impl Fn(&Captured) -> bool) -> Option<Captured> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = vec![0u8; 0xFFFF];
    while Instant::now() < deadline {
        let Ok(len) = (&*capture).read(&mut buf) else {
            continue;
        };
        let Ok(packet) = decode_ipv4_udp(&buf[..len]) else {
            continue;
        };
        let captured = Captured {
            ttl: packet.ttl,
            dscp: packet.dscp,
            identification: packet.identification,
            dont_fragment: packet.dont_fragment,
            source: packet.source,
            destination: packet.destination,
            udp_segment: packet.udp_segment.to_vec(),
            payload: packet.payload.to_vec(),
        };
        if wanted(&captured) {
            return Some(captured);
        }
    }
    None
}

#[test]
#[ignore = "raw ソケットが必要（root か unshare -rn の中で --ignored を付けて実行する）"]
fn query_over_raw_ip_sends_crafted_headers() {
    let server = SimServer::spawn(
        "127.0.0.1:0".parse().unwrap(),
        SimServerConfig {
            offset: TimeDelta::from_millis(5),
            ..SimServerConfig::default()
        },
    )
    .unwrap();
    let SocketAddr::V4(server_addr) = server.local_addr() else {
        panic!("疑似サーバが IPv4 ではない");
    };

    // 自分の送信を横から観察するための raw ソケット（送信より前に開く）
    let capture = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::UDP)).unwrap();
    capture
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    let headers = HeaderTemplate {
        ipv4: Ipv4Spec {
            dscp: 46,
            ecn: 0,
            identification: 0x4242,
            dont_fragment: true,
            ttl: 7,
            header_checksum: None,
        },
        udp_checksum: UdpChecksum::Compute,
    };
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport =
        RawIpTransport::connect(server.local_addr(), 0, headers, Arc::clone(&clock)).unwrap();
    let local = transport.local_addr();

    // 1. 問い合わせが成功し、offset が正しく測れる
    let outcome = query_once(
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    let offset_ms = outcome.sample.offset().delta().as_millis_f64();
    assert!(
        (offset_ms - 5.0).abs() < 2.0f64,
        "offset = {offset_ms} ms（+5ms のはず）"
    );

    // 2. 実際に送られたパケットのヘッダが指定どおり
    let request = find_packet(&capture, |packet| {
        packet.source == local && packet.destination == server_addr
    })
    .expect("送信した要求が見つからない");
    assert_eq!(request.ttl, 7);
    assert_eq!(request.dscp, 46);
    assert_eq!(request.identification, 0x4242);
    assert!(request.dont_fragment);
    assert_eq!(
        udp_checksum(
            *request.source.ip(),
            *request.destination.ip(),
            &request.udp_segment
        ),
        0,
        "UDP チェックサムが正しくない"
    );
    let ntp = decode(&request.payload).unwrap();
    assert_eq!(ntp.mode, NtpMessage::MODE_CLIENT);
    assert_eq!(ntp.version, 4);
}

#[test]
#[ignore = "raw ソケットが必要（root か unshare -rn の中で --ignored を付けて実行する）"]
fn ignores_unrelated_udp_traffic_while_waiting() {
    let server =
        SimServer::spawn("127.0.0.1:0".parse().unwrap(), SimServerConfig::default()).unwrap();

    // 問い合わせ中、ほかのポートあての UDP を大量に流し続ける
    let noise = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let sink_addr = sink.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let noise_thread = thread::spawn({
        let thread_stop = Arc::clone(&stop);
        move || {
            while !thread_stop.load(Ordering::Relaxed) {
                drop(noise.send_to(&[0u8; 64], sink_addr));
            }
        }
    });

    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport = RawIpTransport::connect(
        server.local_addr(),
        0,
        HeaderTemplate::default(),
        Arc::clone(&clock),
    )
    .unwrap();
    let result = query_once(
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    );
    stop.store(true, Ordering::Relaxed);
    noise_thread.join().unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[test]
#[ignore = "raw ソケットが必要（root か unshare -rn の中で --ignored を付けて実行する）"]
fn raw_ip_uses_kernel_time_for_both_send_and_receive() {
    use application::timestamping::TimestampingStatus;
    use infrastructure::transport::{self, TimestampingMode, TransportMode, TransportOptions};

    let server =
        SimServer::spawn("127.0.0.1:0".parse().unwrap(), SimServerConfig::default()).unwrap();
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let status = Arc::new(TimestampingStatus::new());
    let port = transport::open(
        TransportMode::RawIp,
        server.local_addr(),
        TransportOptions {
            timestamping: TimestampingMode::Kernel,
            ..TransportOptions::default()
        },
        Arc::clone(&clock),
        Arc::clone(&status),
    )
    .unwrap()
    .port;
    let _: OnceOutcome = query_once(
        port,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    )
    .unwrap();

    let counts = status.counts();
    println!("raw-ip + カーネル: {counts:?}");
    assert!(counts.kernel_enabled);
    // Linux では IPPROTO_RAW の送信でも送信時刻が取れることを確認済み（WSL2 のカーネル 6.18）
    assert_eq!(counts.kernel_samples, 1, "{counts:?}");
    assert_eq!(counts.tx_misses, 0, "{counts:?}");
}
