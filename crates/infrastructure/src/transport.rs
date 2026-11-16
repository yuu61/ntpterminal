//! パケットの送受信（[`application::port::PacketPort`] の実装）。
//!
//! - `udp`: 標準ライブラリの UDP ソケット。権限不要・全 OS 対応（M1）
//! - `raw_ip`: raw ソケットで IPv4 / UDP ヘッダから作る。Linux のみ（M3）
//! - `link`: Ethernet フレームから作る（M4 予定）

#[cfg(target_os = "linux")]
mod raw_ip;
mod udp;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use application::port::{ClockPort, PacketPort};
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

/// 送受信の設定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportOptions {
    /// 送信元ポート（0 なら OS に任せる）
    pub source_port: u16,
    /// IPv4 / UDP ヘッダの指定。raw-ip モードでのみ使える
    pub headers: Option<HeaderTemplate>,
}

/// 指定したモードで送受信の口を開く。
///
/// 使えない組み合わせは黙って無視せず、エラーにする（UDP モードでのヘッダ指定、Linux 以外での raw-ip）。
pub fn open(
    mode: TransportMode,
    server: SocketAddr,
    options: TransportOptions,
    clock: Arc<dyn ClockPort>,
) -> io::Result<Box<dyn PacketPort>> {
    match mode {
        TransportMode::Udp => {
            if options.headers.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "IP / UDP ヘッダの指定は raw-ip モード（--transport raw-ip）でのみ使えます",
                ));
            }
            Ok(Box::new(UdpTransport::connect_from(
                server,
                options.source_port,
                clock,
            )?))
        }
        TransportMode::RawIp => open_raw_ip(server, options, clock),
    }
}

#[cfg(target_os = "linux")]
fn open_raw_ip(
    server: SocketAddr,
    options: TransportOptions,
    clock: Arc<dyn ClockPort>,
) -> io::Result<Box<dyn PacketPort>> {
    Ok(Box::new(RawIpTransport::connect(
        server,
        options.source_port,
        options.headers.unwrap_or_default(),
        clock,
    )?))
}

#[cfg(not(target_os = "linux"))]
fn open_raw_ip(
    _server: SocketAddr,
    _options: TransportOptions,
    _clock: Arc<dyn ClockPort>,
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
        );
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidInput)
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
        );
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::Unsupported)
        );
    }
}
