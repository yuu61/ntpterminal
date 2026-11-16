use alloc::sync::Arc;
use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use core::time::Duration;
use std::io::{self, ErrorKind};
use std::net::UdpSocket;

use application::port::{ClockPort, Inbound, PacketPort, TransportError};
#[cfg(target_os = "linux")]
use application::timestamping::TimestampingStatus;
use domain::shared_kernel::LocalStamp;

#[cfg(target_os = "linux")]
use super::hardware_ts::HardwareClock;
#[cfg(target_os = "linux")]
use super::kernel_ts::{KernelTimestamper, recv_with_timestamp};

/// 受信バッファの大きさ。拡張フィールド付きの応答も入る。
const RECV_BUFFER_LEN: usize = 2048;

/// 1 台のサーバに `connect` した UDP ソケット。
///
/// `connect` しているので、OS がそのサーバ以外からのデータグラムを捨ててくれる。
/// 時刻は既定ではユーザ空間で、送信の直前（T1）と受信の直後（T4）に読む。
/// Linux ではカーネルのタイムスタンプも使える（`enable_kernel_timestamps`）。
pub struct UdpTransport {
    socket: UdpSocket,
    clock: Arc<dyn ClockPort>,
    buf: Vec<u8>,
    #[cfg(target_os = "linux")]
    kernel: Option<KernelTimestamper>,
}

impl fmt::Debug for UdpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpTransport")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl UdpTransport {
    /// 送信元ポートは OS に任せる。
    ///
    /// # Errors
    ///
    /// ソケットを用意できない、または connect できないとき。
    pub fn connect(server: SocketAddr, clock: Arc<dyn ClockPort>) -> io::Result<Self> {
        Self::connect_from(server, 0, clock)
    }

    /// 送信元ポートを指定する（0 なら OS に任せる）。
    ///
    /// # Errors
    ///
    /// ソケットを用意できない（送信元ポートが使用中など）、または connect できないとき。
    pub fn connect_from(
        server: SocketAddr,
        source_port: u16,
        clock: Arc<dyn ClockPort>,
    ) -> io::Result<Self> {
        let socket = bind_connected(server, source_port)?;
        Ok(Self {
            socket,
            clock,
            buf: vec![0; RECV_BUFFER_LEN],
            #[cfg(target_os = "linux")]
            kernel: None,
        })
    }

    /// 権限は不要。
    ///
    /// # Errors
    ///
    /// ソケットにタイムスタンプの設定をできないとき。
    #[cfg(target_os = "linux")]
    pub fn enable_kernel_timestamps(&mut self, status: Arc<TimestampingStatus>) -> io::Result<()> {
        self.kernel = Some(KernelTimestamper::enable(
            &self.socket,
            &self.socket,
            status,
        )?);
        Ok(())
    }

    /// NIC のハードウェアのタイムスタンプを使う（NIC で無効なら有効にし、drop で戻す）。
    /// 画面に出す説明（"nic0 の HW 時刻" など）を返す。root が必要。
    ///
    /// # Errors
    ///
    /// 経路の NIC がハードウェアタイムスタンプに対応していない、権限がない、またはソケットに設定できないとき。
    #[cfg(target_os = "linux")]
    pub fn enable_hardware_timestamps(
        &mut self,
        status: Arc<TimestampingStatus>,
    ) -> io::Result<String> {
        let hardware = HardwareClock::prepare(self.socket.local_addr()?.ip())?;
        let description = hardware.description();
        self.kernel = Some(KernelTimestamper::enable_hardware(
            &self.socket,
            &self.socket,
            status,
            hardware,
        )?);
        Ok(description)
    }

    /// bind したローカルのアドレス。
    ///
    /// # Errors
    ///
    /// OS からアドレスを取得できないとき。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl PacketPort for UdpTransport {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let sent = self.clock.now();
        let _: usize = self
            .socket
            .send(payload)
            .map_err(|error| to_transport_error(&error))?;
        #[cfg(target_os = "linux")]
        if let Some(kernel) = self.kernel.as_mut() {
            return Ok(kernel.after_send(&self.socket, self.clock.as_ref(), sent));
        }
        Ok(sent)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        // set_read_timeout は 0 を受け付けないので、最低 1ms 待つ
        let wait = timeout.max(Duration::from_millis(1));
        self.socket
            .set_read_timeout(Some(wait))
            .map_err(|error| to_transport_error(&error))?;
        #[cfg(target_os = "linux")]
        if let Some(kernel) = self.kernel.as_ref() {
            return match recv_with_timestamp(&self.socket, &mut self.buf) {
                Ok((len, kernel_times)) => {
                    let received = kernel.choose_received(kernel_times, self.clock.now());
                    Ok(Some(Inbound {
                        payload: self.buf[..len].to_vec(),
                        received,
                    }))
                }
                Err(error) => recv_error(&error),
            };
        }
        match self.socket.recv(&mut self.buf) {
            Ok(len) => {
                let received = self.clock.now();
                Ok(Some(Inbound {
                    payload: self.buf[..len].to_vec(),
                    received,
                }))
            }
            Err(error) => recv_error(&error),
        }
    }
}

/// タイムアウトは Unix では `WouldBlock`、Windows では `TimedOut` になる。
fn recv_error(error: &io::Error) -> Result<Option<Inbound>, TransportError> {
    if matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    ) {
        Ok(None)
    } else {
        Err(to_transport_error(error))
    }
}

/// `server` に connect した UDP ソケットを作る。
///
/// ループバック宛てならループバックに bind する（Windows のファイアウォール確認を避ける）。
/// connect すると、OS が経路に合った送信元 IP を選び、そのサーバ以外からの受信を捨てる。
pub(super) fn bind_connected(server: SocketAddr, source_port: u16) -> io::Result<UdpSocket> {
    let bind: SocketAddr = match server {
        SocketAddr::V4(v4) if v4.ip().is_loopback() => (Ipv4Addr::LOCALHOST, source_port).into(),
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, source_port).into(),
        SocketAddr::V6(v6) if v6.ip().is_loopback() => (Ipv6Addr::LOCALHOST, source_port).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, source_port).into(),
    };
    let socket = UdpSocket::bind(bind).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("送信元 {bind} を確保できません（ほかのプログラムが使っていないか確認してください）: {error}"),
        )
    })?;
    socket.connect(server)?;
    Ok(socket)
}

/// ICMP 到達不能の後、Linux は `ConnectionRefused`、Windows は `ConnectionReset` を返す。
/// どちらも「その問い合わせの失敗」であって、プログラムを止める理由ではない。
fn to_transport_error(error: &io::Error) -> TransportError {
    if matches!(
        error.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
    ) {
        TransportError::Unreachable(error.to_string())
    } else {
        TransportError::Io(error.to_string())
    }
}
