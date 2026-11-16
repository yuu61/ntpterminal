//! raw-ip モード（Linux）: IPv4 / UDP ヘッダを自分で組み立てて送る。
//!
//! - 送信: `SOCK_RAW` + `IPPROTO_RAW`（`IP_HDRINCL`）。ヘッダは `packet_crafting` で作る
//! - 受信: `SOCK_RAW` + `IPPROTO_UDP`。Linux は届いた UDP の写しをすべてここに渡すので、
//!   送信元・宛先で絞り込む（自分の送信の写しやほかの通信も来る）
//! - ガード用の UDP ソケット: 送信元ポートの確保、経路に合った送信元 IP の決定、
//!   カーネルが応答に対して ICMP 到達不能を返すのを防ぐ
//!
//! root か CAP_NET_RAW が必要。Linux のカーネルは IP ヘッダのチェックサムと全長を常に書き換え、
//! Identification が 0 なら値を入れる。

use std::io::{self, ErrorKind, Read};
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use application::port::{ClockPort, Inbound, PacketPort, TransportError};
use domain::packet_crafting::{HeaderTemplate, decode_ipv4_udp, encode_ipv4_udp};
use domain::shared_kernel::LocalStamp;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::udp::bind_connected;

/// IPPROTO_RAW（IP ヘッダを含めて送る raw ソケット）。
const IPPROTO_RAW: i32 = 255;
/// 受信バッファ（IPv4 の最大長）。
const RECV_BUFFER_LEN: usize = 65_535;

pub struct RawIpTransport {
    send_socket: Socket,
    recv_socket: Socket,
    guard: UdpSocket,
    local: SocketAddrV4,
    server: SocketAddrV4,
    headers: HeaderTemplate,
    clock: Arc<dyn ClockPort>,
    buf: Vec<u8>,
}

impl RawIpTransport {
    /// `source_port` が 0 なら OS に任せる。
    pub fn connect(
        server: SocketAddr,
        source_port: u16,
        headers: HeaderTemplate,
        clock: Arc<dyn ClockPort>,
    ) -> io::Result<Self> {
        let SocketAddr::V4(server) = server else {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "raw-ip モードは IPv4 のサーバだけに対応しています",
            ));
        };
        let guard = bind_connected(SocketAddr::V4(server), source_port)?;
        guard.set_nonblocking(true)?;
        let SocketAddr::V4(local) = guard.local_addr()? else {
            return Err(io::Error::other("送信元アドレスが IPv4 ではありません"));
        };

        let send_socket = raw_socket(Protocol::from(IPPROTO_RAW))?;
        send_socket.set_header_included_v4(true)?;
        let recv_socket = raw_socket(Protocol::UDP)?;

        Ok(Self {
            send_socket,
            recv_socket,
            guard,
            local,
            server,
            headers,
            clock,
            buf: vec![0; RECV_BUFFER_LEN],
        })
    }

    /// 送信元（IP ヘッダに入れているアドレスとポート）。
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.local
    }

    /// ガード用ソケットに溜まった受信（カーネルが通常の経路で届けた応答の写し）を捨てる。
    fn drain_guard(&mut self) {
        while self.guard.recv(&mut self.buf).is_ok() {}
    }
}

fn raw_socket(protocol: Protocol) -> io::Result<Socket> {
    Socket::new(Domain::IPV4, Type::RAW, Some(protocol)).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("raw ソケットを開けません（root か CAP_NET_RAW が必要です）: {error}"),
        )
    })
}

impl PacketPort for RawIpTransport {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let packet = encode_ipv4_udp(&self.headers, self.local, self.server, payload);
        let destination = SockAddr::from(self.server);
        let sent = self.clock.now();
        self.send_socket
            .send_to(&packet, &destination)
            .map_err(|error| TransportError::Io(error.to_string()))?;
        Ok(sent)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        self.drain_guard();
        // 関係のないパケットが来ても待ち時間が延びないよう、期限は最初に 1 回だけ決める
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.recv_socket
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .map_err(|error| TransportError::Io(error.to_string()))?;
            match (&self.recv_socket).read(&mut self.buf) {
                Ok(len) => {
                    let received = self.clock.now();
                    let Ok(packet) = decode_ipv4_udp(&self.buf[..len]) else {
                        continue;
                    };
                    if packet.source == self.server && packet.destination == self.local {
                        return Ok(Some(Inbound {
                            payload: packet.payload.to_vec(),
                            received,
                        }));
                    }
                    // 自分の送信の写しや、ほかの通信の UDP は捨てて待ち続ける
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(TransportError::Io(error.to_string())),
            }
        }
    }
}
