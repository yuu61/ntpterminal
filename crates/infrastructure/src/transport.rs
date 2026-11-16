//! パケットの送受信（[`application::port::PacketPort`] の実装）。
//!
//! - `udp`: 標準ライブラリの UDP ソケット。権限不要・全 OS 対応（M1）
//! - `raw_ip`: raw ソケットで IPv4 / UDP ヘッダから作る。Linux のみ（M3）
//! - `kernel_ts`: カーネルのタイムスタンプ（SO_TIMESTAMPING）。Linux のみ（M4）
//! - `link`: Ethernet フレームから作る（M4 で予定）

#[cfg(target_os = "linux")]
mod kernel_ts;
#[cfg(target_os = "linux")]
mod raw_ip;
mod udp;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use application::port::{ClockPort, PacketPort};
use application::timestamping::TimestampingStatus;
use domain::packet_crafting::HeaderTemplate;

#[cfg(target_os = "linux")]
pub use raw_ip::RawIpTransport;
pub use udp::UdpTransport;

/// 送信モード。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    Udp,
    RawIp,
}

impl TransportMode {
    /// 画面に出す名前。
    pub fn label(self) -> &'static str {
        match self {
            Self::Udp => "UDP ソケット",
            Self::RawIp => "raw-ip（IP / UDP ヘッダを自作）",
        }
    }
}

/// タイムスタンプの取り方。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimestampingMode {
    /// ユーザ空間で時計を読む
    User,
    /// カーネルのタイムスタンプ（Linux のみ）。使えなければエラー
    Kernel,
    /// 使えればカーネル、使えなければユーザ空間
    #[default]
    Auto,
}

/// 送受信の設定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportOptions {
    /// 送信元ポート（0 なら OS に任せる）
    pub source_port: u16,
    /// IPv4 / UDP ヘッダの指定。raw-ip モードでのみ使える
    pub headers: Option<HeaderTemplate>,
    pub timestamping: TimestampingMode,
}

/// 指定したモードで送受信の口を開く。
///
/// 使えない組み合わせは黙って無視せず、エラーにする（UDP モードでのヘッダ指定、Linux 以外での raw-ip や
/// カーネルのタイムスタンプ）。実際に使えているタイムスタンプは `status` に記録される。
pub fn open(
    mode: TransportMode,
    server: SocketAddr,
    options: TransportOptions,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
) -> io::Result<Box<dyn PacketPort>> {
    #[cfg(not(target_os = "linux"))]
    if options.timestamping == TimestampingMode::Kernel {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "カーネルのタイムスタンプは Linux のみ対応しています",
        ));
    }
    match mode {
        TransportMode::Udp => {
            if options.headers.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "IP / UDP ヘッダの指定は raw-ip モード（--transport raw-ip）でのみ使えます",
                ));
            }
            #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
            let mut transport = UdpTransport::connect_from(server, options.source_port, clock)?;
            #[cfg(target_os = "linux")]
            apply_timestamping(options.timestamping, || {
                transport.enable_kernel_timestamps(Arc::clone(&status))
            })?;
            #[cfg(not(target_os = "linux"))]
            let _ = status;
            Ok(Box::new(transport))
        }
        TransportMode::RawIp => open_raw_ip(server, options, clock, status),
    }
}

/// カーネルのタイムスタンプを、モードに応じて有効にする（Auto では失敗しても続ける）。
#[cfg(target_os = "linux")]
fn apply_timestamping(
    mode: TimestampingMode,
    enable: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match mode {
        TimestampingMode::User => Ok(()),
        TimestampingMode::Kernel => enable().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("カーネルのタイムスタンプを有効にできません: {error}"),
            )
        }),
        TimestampingMode::Auto => {
            let _ = enable();
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn open_raw_ip(
    server: SocketAddr,
    options: TransportOptions,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
) -> io::Result<Box<dyn PacketPort>> {
    let mut transport = RawIpTransport::connect(
        server,
        options.source_port,
        options.headers.unwrap_or_default(),
        clock,
    )?;
    apply_timestamping(options.timestamping, || {
        transport.enable_kernel_timestamps(status)
    })?;
    Ok(Box::new(transport))
}

#[cfg(not(target_os = "linux"))]
fn open_raw_ip(
    _server: SocketAddr,
    _options: TransportOptions,
    _clock: Arc<dyn ClockPort>,
    _status: Arc<TimestampingStatus>,
) -> io::Result<Box<dyn PacketPort>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "raw-ip モードは Linux のみ対応しています",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;

    #[test]
    fn udp_mode_rejects_header_options() {
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let options = TransportOptions {
            headers: Some(HeaderTemplate::default()),
            ..TransportOptions::default()
        };
        let result = open(
            TransportMode::Udp,
            "127.0.0.1:123".parse().unwrap(),
            options,
            clock,
            Arc::new(TimestampingStatus::new()),
        );
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn kernel_timestamping_is_linux_only() {
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let options = TransportOptions {
            timestamping: TimestampingMode::Kernel,
            ..TransportOptions::default()
        };
        let result = open(
            TransportMode::Udp,
            "127.0.0.1:123".parse().unwrap(),
            options,
            clock,
            Arc::new(TimestampingStatus::new()),
        );
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::Unsupported)
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn raw_ip_is_linux_only() {
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let result = open(
            TransportMode::RawIp,
            "127.0.0.1:123".parse().unwrap(),
            TransportOptions::default(),
            clock,
            Arc::new(TimestampingStatus::new()),
        );
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::Unsupported)
        );
    }
}
