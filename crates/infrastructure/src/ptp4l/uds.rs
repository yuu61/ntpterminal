//! ptp4l の管理用ソケットへの送受信（Unix ドメインソケットのデータグラム。抽象名前空間の名前で bind する）。
//!
//! ptp4l が起動し直すと、つないでいた相手（前の ptp4l のソケット）はなくなり、送ると `ECONNREFUSED`（その後は
//! `ENOTCONN`）になる。そのときは同じ場所につなぎ直して 1 回だけ送り直す（つなぎ直さないと、ptp4l が戻っても
//! 応答なしのままになる）。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use std::io;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::process;

use application::port::{ClockPort, Inbound, PacketPort, TransportError};
use domain::shared_kernel::LocalStamp;

/// 管理メッセージの最大の大きさ（ptp4l の応答はどれも 100 バイト程度）
const MAX_MESSAGE_LEN: usize = 1500;
/// 受信を待つ最短の時間（0 を渡すと、ソケットは「待たない」ではなくエラーになる）
const MIN_WAIT: Duration = Duration::from_micros(1);

/// 同じプロセスで何度つないでも名前が重ならないようにする番号。
static NEXT_NAME: AtomicU32 = AtomicU32::new(0);

/// ptp4l の管理用ソケットへの送受信（[`PacketPort`] の実装）。
pub(super) struct Ptp4lSocket {
    socket: UnixDatagram,
    /// ptp4l の管理用ソケットの場所（ptp4l が起動し直したら、つなぎ直す）
    path: PathBuf,
    clock: Arc<dyn ClockPort>,
    buffer: Vec<u8>,
}

impl Ptp4lSocket {
    pub(super) fn connect(path: &Path, clock: Arc<dyn ClockPort>) -> io::Result<Self> {
        let name = format!(
            "ntpterminal.{}.{}",
            process::id(),
            NEXT_NAME.fetch_add(1, Ordering::Relaxed)
        );
        let address = SocketAddr::from_abstract_name(name.as_bytes())?;
        let socket = UnixDatagram::bind_addr(&address)?;
        socket.connect(path).map_err(|error| {
            let hint = if error.kind() == io::ErrorKind::PermissionDenied {
                "（root が必要です）"
            } else if error.kind() == io::ErrorKind::ConnectionRefused {
                "（ptp4l が動いていません）"
            } else {
                ""
            };
            io::Error::new(
                error.kind(),
                format!(
                    "ptp4l の管理用ソケット {} につなげません{hint}: {error}",
                    path.display()
                ),
            )
        })?;
        Ok(Self {
            socket,
            path: path.to_path_buf(),
            clock,
            buffer: vec![0; MAX_MESSAGE_LEN],
        })
    }
}

impl PacketPort for Ptp4lSocket {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let io_error = |error: io::Error| TransportError::Io(error.to_string());
        let sent = self.clock.now();
        match self.socket.send(payload) {
            Ok(_len) => Ok(sent),
            // ptp4l が起動し直した: つなぎ直して、1 回だけ送り直す
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotConnected
                ) =>
            {
                self.socket.connect(&self.path).map_err(io_error)?;
                let resent = self.clock.now();
                let _len: usize = self.socket.send(payload).map_err(io_error)?;
                Ok(resent)
            }
            Err(error) => Err(io_error(error)),
        }
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        let io_error = |error: io::Error| TransportError::Io(error.to_string());
        self.socket
            .set_read_timeout(Some(timeout.max(MIN_WAIT)))
            .map_err(io_error)?;
        match self.socket.recv(&mut self.buffer) {
            Ok(len) => Ok(Some(Inbound {
                payload: self.buffer[..len].to_vec(),
                received: self.clock.now(),
            })),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(io_error(error)),
        }
    }
}
