//! ptp4l が合わせる PHC を表示するための口: ptp4l の管理用ソケットと、PHC（`/dev/ptpN`）。Linux のみ。
//!
//! - 管理用ソケットは Unix ドメインソケット（データグラム）。ptp4l は問い合わせの送り元のアドレスに応答を
//!   返すので、こちらもアドレスを持つ必要がある。Linux の抽象名前空間の名前で bind する（ファイルを作らないので、
//!   終了時に消す必要がなく、残骸も残らない）。抽象名前空間の名前はネットワーク名前空間ごとなので、ptp4l と
//!   同じネットワーク名前空間で動かすこと（違うと応答が届かず、時間切れになる）
//! - 管理用ソケットの場所は、指定がなければ linuxptp の既定の場所を新しい版から順に探す
//!   （4.3 から `/var/run/ptp/ptp4l`、それより前は `/var/run/ptp4l`）。読み取り専用のソケット（`ptp4lro`）は
//!   ポートの一覧に答えないので使わない
//! - ソケットに書き込むには、ptp4l が作ったソケットのファイルへの書き込みの権限（既定で 0660。ふつうは root）が要る
//! - ptp4l が起動し直したら、同じ場所につなぎ直す（つないでいた前のソケットはなくなるため。`uds` の先頭）

#[cfg(target_os = "linux")]
mod uds;

use alloc::sync::Arc;
use core::fmt;
use std::io;
use std::path::{Path, PathBuf};

use application::port::{ClockPort, PacketPort, PhcPort};

#[cfg(target_os = "linux")]
use crate::phc::PhcDevice;
#[cfg(target_os = "linux")]
use crate::transport;

/// 管理用ソケットの既定の場所（探す順）。
pub const DEFAULT_SOCKET_PATHS: [&str; 2] = ["/var/run/ptp/ptp4l", "/var/run/ptp4l"];

/// Linux 以外でのエラーの文言。
#[cfg(not(target_os = "linux"))]
const LINUX_ONLY: &str = "ptp4l が合わせる PHC の表示は Linux のみ対応しています";

/// ptp4l の管理用ソケットにつないだ口。
pub struct Ptp4lConnection {
    pub port: Box<dyn PacketPort>,
    /// つないだソケットの場所
    pub path: PathBuf,
}

impl fmt::Debug for Ptp4lConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ptp4lConnection")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// 開いた PHC。
pub struct OpenedPhc {
    pub port: Box<dyn PhcPort>,
    /// "/dev/ptp0"
    pub path: String,
    /// 読み方の名前（"同時読み取り" など）
    pub method: &'static str,
}

impl fmt::Debug for OpenedPhc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedPhc")
            .field("path", &self.path)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// ptp4l の管理用ソケットにつなぐ。`path` がなければ既定の場所を探す。
///
/// # Errors
///
/// ソケットが見つからないとき、つなげないとき（ptp4l が動いていない・権限がない）。
#[cfg(target_os = "linux")]
pub fn connect(path: Option<&Path>, clock: Arc<dyn ClockPort>) -> io::Result<Ptp4lConnection> {
    let target = match path {
        Some(given) => given.to_path_buf(),
        None => DEFAULT_SOCKET_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.exists())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "ptp4l の管理用ソケットが見つかりません（{}）",
                        DEFAULT_SOCKET_PATHS.join("・")
                    ),
                )
            })?,
    };
    let socket = uds::Ptp4lSocket::connect(&target, clock)?;
    Ok(Ptp4lConnection {
        port: Box::new(socket),
        path: target,
    })
}

/// ptp4l の管理用ソケットにつなぐ（Linux のみなので、ほかの OS ではエラー）。
///
/// # Errors
///
/// 常に（Linux 以外）。
#[cfg(not(target_os = "linux"))]
pub fn connect(_path: Option<&Path>, _clock: Arc<dyn ClockPort>) -> io::Result<Ptp4lConnection> {
    Err(io::Error::new(io::ErrorKind::Unsupported, LINUX_ONLY))
}

/// PHC（`/dev/ptp{index}`）を開く。
///
/// # Errors
///
/// 開けないとき（権限がない）、システムの時計と組で読めないとき。
#[cfg(target_os = "linux")]
pub fn open_phc(index: u32, clock: Arc<dyn ClockPort>) -> io::Result<OpenedPhc> {
    let device = PhcDevice::open(index, clock)?;
    Ok(OpenedPhc {
        path: device.path().to_owned(),
        method: device.method().label(),
        port: Box::new(device),
    })
}

/// PHC を開く（Linux のみなので、ほかの OS ではエラー）。
///
/// # Errors
///
/// 常に（Linux 以外）。
#[cfg(not(target_os = "linux"))]
pub fn open_phc(_index: u32, _clock: Arc<dyn ClockPort>) -> io::Result<OpenedPhc> {
    Err(io::Error::new(io::ErrorKind::Unsupported, LINUX_ONLY))
}

/// インターフェースの PHC の番号（`ethtool -T` の値）。分からなければ `None`。
/// ptp4l が PHC の番号を知らせない古い linuxptp のときだけ使う。
#[cfg(target_os = "linux")]
#[must_use]
pub fn interface_phc(interface: &str) -> Option<u32> {
    transport::interface_phc(interface)
}

/// インターフェースの PHC の番号（Linux 以外では分からない）。
#[cfg(not(target_os = "linux"))]
#[must_use]
pub const fn interface_phc(_interface: &str) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn is_linux_only() {
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let Err(error) = connect(None, Arc::clone(&clock)) else {
            panic!("Linux 以外ではつなげないはず");
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            open_phc(0, clock).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(interface_phc("eth0"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_socket_is_reported_with_its_path() {
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let path = Path::new("/nonexistent/ntpterminal-test/ptp4l");
        let Err(error) = connect(Some(path), clock) else {
            panic!("ないソケットにはつなげないはず");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
    }
}
