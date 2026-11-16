//! link モード（Linux）: Ethernet フレームから自分で組み立てて、AF_PACKET のソケットで送る。
//!
//! raw-ip と違い、カーネルは IP ヘッダに手を加えない（チェックサム・全長・Identification も指定どおり届く）。
//! 802.1Q の VLAN タグ（PCP / VID）も付けられる。root か CAP_NET_RAW が必要。
//!
//! - 宛先 MAC: `--dst-mac` の指定 → カーネルの近隣表（/proc/net/arp）→ 自分で ARP 要求、の順に決める
//! - 受信: 同じインターフェースの IPv4 フレームがすべて来るので、自分の送信（PACKET_OUTGOING）を除き、
//!   送信元・宛先で絞り込む。BPF のフィルタは使っていないので、通信量の多いインターフェースでは負荷が増える
//! - ガード用の UDP ソケット: 送信元ポートの確保、送信元 IP の決定、ICMP 到達不能の抑止（raw-ip と同じ）
//! - ポリシールーティング（ip rule）には対応しない。合わないときは `--dst-mac` で宛先 MAC を指定する

mod interface;
mod neighbor;

use std::fs;
use std::io::{self, ErrorKind, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use application::port::{ClockPort, Inbound, PacketPort, TransportError};
use application::timestamping::TimestampingStatus;
use domain::packet_crafting::{
    ARP_REPLY, ArpPacket, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetSpec, HeaderTemplate, MacAddr,
    decode_arp, decode_ethernet, decode_ipv4_udp, encode_arp, encode_ethernet, encode_ipv4_udp,
};
use domain::shared_kernel::LocalStamp;
use nix::sys::socket::{
    AddressFamily, LinkAddr, MsgFlags, SockFlag, SockType, bind, recvfrom, sendto, socket,
};
use socket2::Socket;

use self::interface::{Interface, LinkKind, find_interface, link_addr};
use self::neighbor::{find_neighbor, next_hop, parse_routes};
use super::LinkOptions;
use super::kernel_ts::{KernelTimestamper, recv_with_timestamp_from};
use super::udp::bind_connected;

/// sockaddr_ll.sll_pkttype: 自分が送ったフレームの写し
const PACKET_OUTGOING: u8 = 4;
/// 受信バッファ（ジャンボフレームまで）
const RECV_BUFFER_LEN: usize = 65_535;
/// ARP の応答を待つ合計時間と、要求を送る回数
const ARP_TIMEOUT: Duration = Duration::from_secs(1);
const ARP_ATTEMPTS: u32 = 3;

pub struct LinkTransport {
    socket: Socket,
    guard: UdpSocket,
    interface: Interface,
    destination: LinkAddr,
    destination_mac: MacAddr,
    local: SocketAddrV4,
    server: SocketAddrV4,
    headers: HeaderTemplate,
    ethernet: EthernetSpec,
    clock: Arc<dyn ClockPort>,
    buf: Vec<u8>,
    kernel: Option<KernelTimestamper>,
}

impl LinkTransport {
    /// `source_port` が 0 なら OS に任せる。
    pub fn connect(
        server: SocketAddr,
        source_port: u16,
        headers: HeaderTemplate,
        link: LinkOptions,
        clock: Arc<dyn ClockPort>,
    ) -> io::Result<Self> {
        let SocketAddr::V4(server) = server else {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "link モードは IPv4 のサーバだけに対応しています",
            ));
        };
        let guard = bind_connected(SocketAddr::V4(server), source_port)?;
        guard.set_nonblocking(true)?;
        let SocketAddr::V4(local) = guard.local_addr()? else {
            return Err(io::Error::other("送信元アドレスが IPv4 ではありません"));
        };
        let interface = find_interface(*local.ip())?;
        let destination_mac = match (link.destination_mac, interface.kind) {
            (Some(mac), _) => mac,
            (None, LinkKind::Loopback) => MacAddr::ZERO,
            (None, LinkKind::Ethernet) => {
                resolve_mac(&interface, &link.ethernet, *local.ip(), *server.ip())?
            }
        };
        let socket = packet_socket(&interface, ETHERTYPE_IPV4)?;
        let destination = link_addr(interface.index, ETHERTYPE_IPV4, destination_mac)?;
        Ok(Self {
            socket,
            guard,
            interface,
            destination,
            destination_mac,
            local,
            server,
            headers,
            ethernet: link.ethernet,
            clock,
            buf: vec![0; RECV_BUFFER_LEN],
            kernel: None,
        })
    }

    /// カーネルのタイムスタンプ（SO_TIMESTAMPING）を使う。
    pub fn enable_kernel_timestamps(&mut self, status: Arc<TimestampingStatus>) -> io::Result<()> {
        self.kernel = Some(KernelTimestamper::enable(
            &self.socket,
            &self.socket,
            status,
        )?);
        Ok(())
    }

    /// 送信元（IP ヘッダに入れているアドレスとポート）。
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.local
    }

    /// 画面に出す説明（"link（eth0 → 00:11:22:33:44:55）"）。
    pub fn description(&self) -> String {
        format!("link（{} → {}）", self.interface.name, self.destination_mac)
    }

    fn drain_guard(&mut self) {
        while self.guard.recv(&mut self.buf).is_ok() {}
    }

    /// 1 フレーム受信する。送信元のリンク層アドレス（パケットの種別を見るため）と、
    /// カーネルのタイムスタンプを使うときは受信時刻も返す。
    fn receive_once(&mut self) -> io::Result<(usize, Option<i128>, Option<LinkAddr>)> {
        if self.kernel.is_some() {
            recv_with_timestamp_from::<LinkAddr>(&self.socket, &mut self.buf)
        } else {
            let (len, address) = recvfrom::<LinkAddr>(self.socket.as_raw_fd(), &mut self.buf)?;
            Ok((len, None, address))
        }
    }
}

/// AF_PACKET のソケットを作り、インターフェースと EtherType に bind する。
///
/// 作るときはプロトコル 0（まだ何も受け取らない）にして、bind した時点から受け取り始める。
/// こうしないと、bind する前にほかのインターフェースのフレームが溜まることがある。
fn packet_socket(interface: &Interface, ethertype: u16) -> io::Result<Socket> {
    let fd = socket(
        AddressFamily::Packet,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|error| {
        io::Error::new(
            io::Error::from(error).kind(),
            format!("AF_PACKET のソケットを開けません（root か CAP_NET_RAW が必要です）: {error}"),
        )
    })?;
    bind(
        fd.as_raw_fd(),
        &link_addr(interface.index, ethertype, MacAddr::ZERO)?,
    )?;
    Ok(Socket::from(fd))
}

/// 次の転送先（同じネットワークならサーバ、それ以外はゲートウェイ）の MAC アドレスを決める。
///
/// VLAN を指定しているときは、ARP 要求にも同じ VLAN タグを付ける（相手は VLAN の中にいるため）。
fn resolve_mac(
    interface: &Interface,
    ethernet: &EthernetSpec,
    local_ip: Ipv4Addr,
    server_ip: Ipv4Addr,
) -> io::Result<MacAddr> {
    let routes = fs::read_to_string("/proc/net/route")
        .map(|text| parse_routes(&text))
        .unwrap_or_default();
    let hop = next_hop(&routes, &interface.name, server_ip).unwrap_or(server_ip);
    // 近隣表はタグなしのネットワークのものなので、VLAN を使うときは見ない
    if ethernet.vlan.is_none()
        && let Ok(table) = fs::read_to_string("/proc/net/arp")
        && let Some(mac) = find_neighbor(&table, &interface.name, hop)
    {
        return Ok(mac);
    }
    arp_resolve(interface, ethernet, local_ip, hop).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{hop} の MAC アドレスを ARP で調べられません（--dst-mac で指定できます）: {error}"
            ),
        )
    })
}

/// 自分で ARP 要求を送って、`target` の MAC アドレスを調べる。
fn arp_resolve(
    interface: &Interface,
    ethernet: &EthernetSpec,
    local_ip: Ipv4Addr,
    target: Ipv4Addr,
) -> io::Result<MacAddr> {
    let socket = packet_socket(interface, ETHERTYPE_ARP)?;
    let request = encode_ethernet(
        ethernet,
        MacAddr::BROADCAST,
        interface.mac,
        ETHERTYPE_ARP,
        &encode_arp(&ArpPacket::request(interface.mac, local_ip, target)),
    );
    let broadcast = link_addr(interface.index, ETHERTYPE_ARP, MacAddr::BROADCAST)?;
    let mut buf = [0u8; 2048];
    for _ in 0..ARP_ATTEMPTS {
        sendto(socket.as_raw_fd(), &request, &broadcast, MsgFlags::empty())?;
        let deadline = Instant::now() + ARP_TIMEOUT / ARP_ATTEMPTS;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(remaining.max(Duration::from_millis(1))))?;
            let len = match (&socket).read(&mut buf) {
                Ok(len) => len,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if let Ok(frame) = decode_ethernet(&buf[..len])
                && frame.ethertype == ETHERTYPE_ARP
                && let Ok(arp) = decode_arp(frame.payload)
                && arp.operation == ARP_REPLY
                && arp.sender_ip == target
            {
                return Ok(arp.sender_mac);
            }
        }
    }
    Err(io::Error::new(
        ErrorKind::TimedOut,
        "ARP の応答がありません",
    ))
}

impl PacketPort for LinkTransport {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let packet = encode_ipv4_udp(&self.headers, self.local, self.server, payload);
        let frame = encode_ethernet(
            &self.ethernet,
            self.destination_mac,
            self.interface.mac,
            ETHERTYPE_IPV4,
            &packet,
        );
        let sent = self.clock.now();
        sendto(
            self.socket.as_raw_fd(),
            &frame,
            &self.destination,
            MsgFlags::empty(),
        )
        .map_err(|error| TransportError::Io(error.to_string()))?;
        if let Some(kernel) = &mut self.kernel {
            return Ok(kernel.after_send(&self.socket, self.clock.as_ref(), sent));
        }
        Ok(sent)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        self.drain_guard();
        // 関係のないフレームが来ても待ち時間が延びないよう、期限は最初に 1 回だけ決める
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.socket
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .map_err(|error| TransportError::Io(error.to_string()))?;
            let (len, kernel_time, address) = match self.receive_once() {
                Ok(received) => received,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(TransportError::Io(error.to_string())),
            };
            let observed = self.clock.now();
            // 自分が送ったフレームの写し（ループバックでは応答も 2 回見える）は捨てる
            if address.is_some_and(|address| address.pkttype() == PACKET_OUTGOING) {
                continue;
            }
            let Ok(frame) = decode_ethernet(&self.buf[..len]) else {
                continue;
            };
            if frame.ethertype != ETHERTYPE_IPV4 {
                continue;
            }
            let Ok(packet) = decode_ipv4_udp(frame.payload) else {
                continue;
            };
            if packet.source == self.server && packet.destination == self.local {
                let received = match &self.kernel {
                    Some(kernel) => kernel.choose_received(kernel_time, observed),
                    None => observed,
                };
                return Ok(Some(Inbound {
                    payload: packet.payload.to_vec(),
                    received,
                }));
            }
        }
    }
}
