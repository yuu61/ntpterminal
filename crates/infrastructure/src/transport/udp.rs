//! 標準ライブラリの UDP ソケットによる送受信。

use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use application::port::{ClockPort, Inbound, PacketPort, TransportError};
use domain::shared_kernel::LocalStamp;

/// 受信バッファの大きさ。拡張フィールド付きの応答も入る。
const RECV_BUFFER_LEN: usize = 2048;

/// 1 台のサーバに `connect` した UDP ソケット。
///
/// `connect` しているので、OS がそのサーバ以外からのデータグラムを捨ててくれる。
/// 時刻はユーザ空間で、送信の直前（T1）と受信の直後（T4）に読む。
pub struct UdpTransport {
    socket: UdpSocket,
    clock: Arc<dyn ClockPort>,
    buf: Vec<u8>,
}

impl UdpTransport {
    pub fn connect(server: SocketAddr, clock: Arc<dyn ClockPort>) -> io::Result<Self> {
        // ループバック宛てならループバックに bind する（Windows のファイアウォール確認を避ける）
        let bind: SocketAddr = match server {
            SocketAddr::V4(v4) if v4.ip().is_loopback() => (Ipv4Addr::LOCALHOST, 0).into(),
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(v6) if v6.ip().is_loopback() => (Ipv6Addr::LOCALHOST, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind(bind)?;
        socket.connect(server)?;
        Ok(Self {
            socket,
            clock,
            buf: vec![0; RECV_BUFFER_LEN],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl PacketPort for UdpTransport {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let sent = self.clock.now();
        self.socket.send(payload).map_err(to_transport_error)?;
        Ok(sent)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        // set_read_timeout は 0 を受け付けないので、最低 1ms 待つ
        let timeout = timeout.max(Duration::from_millis(1));
        self.socket
            .set_read_timeout(Some(timeout))
            .map_err(to_transport_error)?;
        match self.socket.recv(&mut self.buf) {
            Ok(len) => {
                let received = self.clock.now();
                Ok(Some(Inbound {
                    payload: self.buf[..len].to_vec(),
                    received,
                }))
            }
            // タイムアウトは Unix では WouldBlock、Windows では TimedOut になる
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(to_transport_error(error)),
        }
    }
}

/// ICMP 到達不能の後、Linux は ConnectionRefused、Windows は ConnectionReset を返す。
/// どちらも「その問い合わせの失敗」であって、プログラムを止める理由ではない。
fn to_transport_error(error: io::Error) -> TransportError {
    match error.kind() {
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset => {
            TransportError::Unreachable(error.to_string())
        }
        _ => TransportError::Io(error.to_string()),
    }
}
