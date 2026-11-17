//! パケットの送受信（[`application::port::PacketPort`] の実装）。
//!
//! - `udp`: 標準ライブラリの UDP ソケット。権限不要・全 OS 対応
//! - `raw_ip`: raw ソケットで IPv4 / UDP ヘッダから作る。Linux のみ
//! - `link`: `AF_PACKET` で Ethernet フレームから作る。Linux のみ
//! - `kernel_ts`: カーネルのタイムスタンプ（`SO_TIMESTAMPING`）。Linux のみ
//! - `hardware_ts`: NIC のハードウェアのタイムスタンプ（PHC と NIC の設定）。Linux のみ
//! - `ptp_udp`: PTP のスレーブの送受信（[`application::port::PtpPort`] の実装。UDP の 319・320 番）
//!
//! ヘッダを自由に作れるモードでも、送信元 IP は経路に合った自分のアドレスを入れる。偽る手段は作らない。

#[cfg(target_os = "linux")]
mod hardware_ts;
#[cfg(target_os = "linux")]
mod kernel_ts;
#[cfg(target_os = "linux")]
mod link;
mod ptp_udp;
#[cfg(target_os = "linux")]
mod raw_ip;
mod udp;

use alloc::sync::Arc;
use core::fmt;
use core::net::SocketAddr;
use std::io;

use application::description::{HardwareTimestampInfo, NtpTransportInfo, NtpTransportKind};
use application::port::{ClockPort, PacketPort};
use application::timestamping::TimestampingStatus;
use domain::packet_crafting::{EthernetSpec, HeaderTemplate, MacAddr};

#[cfg(target_os = "linux")]
pub(crate) use hardware_ts::interface_phc;
#[cfg(target_os = "linux")]
pub use link::LinkTransport;
pub use ptp_udp::{
    OpenedPtp, PTP_EVENT_PORT, PTP_PRIMARY_GROUP, PtpInterface, PtpUdp, PtpUdpOptions, open_ptp,
};
#[cfg(target_os = "linux")]
pub use raw_ip::RawIpTransport;
pub use udp::UdpTransport;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    Udp,
    RawIp,
    Link,
}

impl TransportMode {
    /// エラーの文言に使う名前。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Udp => "UDP ソケット",
            Self::RawIp => "raw-ip（IP / UDP ヘッダを自作）",
            Self::Link => "link（Ethernet フレームを自作）",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimestampingMode {
    /// ユーザ空間で時計を読む
    User,
    /// カーネルのタイムスタンプ（Linux のみ）。使えなければエラー
    Kernel,
    /// 使えればカーネル、使えなければユーザ空間
    #[default]
    Auto,
    /// NIC のハードウェアのタイムスタンプ（Linux のみ・root）。NIC で無効なら有効にして、終了時に戻す。
    /// 使えなければエラー（ハードウェアの時刻が取れない回は、ソフトウェアかユーザ空間の時刻になり、取得状況に出る）
    Hardware,
}

/// link モードの指定（Ethernet ヘッダ）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkOptions {
    /// 宛先 MAC。`None` なら近隣表か ARP で調べる
    pub destination_mac: Option<MacAddr>,
    pub ethernet: EthernetSpec,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportOptions {
    /// 送信元ポート（0 なら OS に任せる）
    pub source_port: u16,
    /// IPv4 / UDP ヘッダの指定。raw-ip / link モードでのみ使える
    pub headers: Option<HeaderTemplate>,
    /// Ethernet ヘッダの指定。link モードでのみ使える
    pub link: Option<LinkOptions>,
    pub timestamping: TimestampingMode,
}

pub struct OpenedTransport {
    pub port: Box<dyn PacketPort>,
    /// 開いたもの（link モードではインターフェースと宛先 MAC、ハードウェアのタイムスタンプでは NIC も）
    pub info: NtpTransportInfo,
}

impl fmt::Debug for OpenedTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedTransport")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// 指定したモードで送受信の口を開く。
///
/// 使えない組み合わせは黙って無視せず、エラーにする。実際に使えているタイムスタンプは `status` に記録される。
///
/// # Errors
///
/// 使えないモードとオプションの組み合わせを指定したとき、またはソケットを開けないとき。
pub fn open(
    mode: TransportMode,
    server: SocketAddr,
    options: TransportOptions,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
) -> io::Result<OpenedTransport> {
    validate(mode, &options)?;
    match mode {
        TransportMode::Udp => {
            let (transport, hardware) = open_udp(server, &options, clock, status)?;
            Ok(OpenedTransport {
                port: Box::new(transport),
                info: NtpTransportInfo {
                    kind: NtpTransportKind::Udp,
                    hardware,
                },
            })
        }
        TransportMode::RawIp | TransportMode::Link => {
            open_linux_only(mode, server, options, clock, status)
        }
    }
}

/// UDP の送受信の口を開き、モードに応じてカーネル（とハードウェア）のタイムスタンプを有効にする。
/// ハードウェアを使うときは、使う NIC も返す。
#[cfg(target_os = "linux")]
fn open_udp(
    server: SocketAddr,
    options: &TransportOptions,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
) -> io::Result<(UdpTransport, Option<HardwareTimestampInfo>)> {
    let mut transport = UdpTransport::connect_from(server, options.source_port, clock)?;
    let hardware = apply_timestamping(options.timestamping, &mut transport, status)?;
    Ok((transport, hardware))
}

/// UDP の送受信の口を開く（カーネルのタイムスタンプは Linux のみなので使わない）。
#[cfg(not(target_os = "linux"))]
fn open_udp(
    server: SocketAddr,
    options: &TransportOptions,
    clock: Arc<dyn ClockPort>,
    _status: Arc<TimestampingStatus>,
) -> io::Result<(UdpTransport, Option<HardwareTimestampInfo>)> {
    Ok((
        UdpTransport::connect_from(server, options.source_port, clock)?,
        None,
    ))
}

fn validate(mode: TransportMode, options: &TransportOptions) -> io::Result<()> {
    let invalid = |message: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            message.to_owned(),
        ))
    };
    if cfg!(not(target_os = "linux")) {
        if options.timestamping == TimestampingMode::Kernel {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "カーネルのタイムスタンプは Linux のみ対応しています",
            ));
        }
        if options.timestamping == TimestampingMode::Hardware {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ハードウェアのタイムスタンプは Linux のみ対応しています",
            ));
        }
        if mode != TransportMode::Udp {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{} は Linux のみ対応しています", mode.label()),
            ));
        }
    }
    if mode == TransportMode::Udp && options.headers.is_some() {
        return invalid(
            "IP / UDP ヘッダの指定は raw-ip / link モード（--transport raw-ip|link）でのみ使えます",
        );
    }
    if mode != TransportMode::Link && options.link.is_some() {
        return invalid(
            "Ethernet の指定（--dst-mac / --vlan / --pcp）は link モード（--transport link）でのみ使えます",
        );
    }
    if mode != TransportMode::Link
        && options
            .headers
            .is_some_and(|headers| headers.ipv4.header_checksum.is_some())
    {
        return invalid(
            "IP ヘッダのチェックサムの指定は link モードでのみ届きます（raw-ip ではカーネルが書き換えます）",
        );
    }
    Ok(())
}

/// タイムスタンプを有効にできる送受信の口（Linux）。
#[cfg(target_os = "linux")]
trait TimestampingTarget {
    fn enable_kernel(&mut self, status: Arc<TimestampingStatus>) -> io::Result<()>;
    /// 使う NIC を返す。
    fn enable_hardware(
        &mut self,
        status: Arc<TimestampingStatus>,
    ) -> io::Result<HardwareTimestampInfo>;
}

#[cfg(target_os = "linux")]
impl TimestampingTarget for UdpTransport {
    fn enable_kernel(&mut self, status: Arc<TimestampingStatus>) -> io::Result<()> {
        self.enable_kernel_timestamps(status)
    }

    fn enable_hardware(
        &mut self,
        status: Arc<TimestampingStatus>,
    ) -> io::Result<HardwareTimestampInfo> {
        self.enable_hardware_timestamps(status)
    }
}

#[cfg(target_os = "linux")]
impl TimestampingTarget for RawIpTransport {
    fn enable_kernel(&mut self, status: Arc<TimestampingStatus>) -> io::Result<()> {
        self.enable_kernel_timestamps(status)
    }

    fn enable_hardware(
        &mut self,
        status: Arc<TimestampingStatus>,
    ) -> io::Result<HardwareTimestampInfo> {
        self.enable_hardware_timestamps(status)
    }
}

#[cfg(target_os = "linux")]
impl TimestampingTarget for LinkTransport {
    fn enable_kernel(&mut self, status: Arc<TimestampingStatus>) -> io::Result<()> {
        self.enable_kernel_timestamps(status)
    }

    fn enable_hardware(
        &mut self,
        status: Arc<TimestampingStatus>,
    ) -> io::Result<HardwareTimestampInfo> {
        self.enable_hardware_timestamps(status)
    }
}

/// タイムスタンプを、モードに応じて有効にする（Auto では失敗しても続ける）。
/// ハードウェアを使うときは、使う NIC を返す。
#[cfg(target_os = "linux")]
fn apply_timestamping(
    mode: TimestampingMode,
    target: &mut impl TimestampingTarget,
    status: Arc<TimestampingStatus>,
) -> io::Result<Option<HardwareTimestampInfo>> {
    match mode {
        TimestampingMode::User => Ok(None),
        TimestampingMode::Kernel => target
            .enable_kernel(status)
            .map(|()| None)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("カーネルのタイムスタンプを有効にできません: {error}"),
                )
            }),
        TimestampingMode::Hardware => target.enable_hardware(status).map(Some).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("ハードウェアのタイムスタンプを使えません: {error}"),
            )
        }),
        TimestampingMode::Auto => {
            drop(target.enable_kernel(status));
            Ok(None)
        }
    }
}

#[cfg(target_os = "linux")]
fn open_linux_only(
    mode: TransportMode,
    server: SocketAddr,
    options: TransportOptions,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
) -> io::Result<OpenedTransport> {
    let headers = options.headers.unwrap_or_default();
    if mode == TransportMode::Link {
        let mut transport = LinkTransport::connect(
            server,
            options.source_port,
            headers,
            options.link.unwrap_or_default(),
            clock,
        )?;
        let hardware = apply_timestamping(options.timestamping, &mut transport, status)?;
        return Ok(OpenedTransport {
            info: NtpTransportInfo {
                kind: transport.kind(),
                hardware,
            },
            port: Box::new(transport),
        });
    }
    let mut transport = RawIpTransport::connect(server, options.source_port, headers, clock)?;
    let hardware = apply_timestamping(options.timestamping, &mut transport, status)?;
    Ok(OpenedTransport {
        port: Box::new(transport),
        info: NtpTransportInfo {
            kind: NtpTransportKind::RawIp,
            hardware,
        },
    })
}

#[cfg(not(target_os = "linux"))]
fn open_linux_only(
    mode: TransportMode,
    _server: SocketAddr,
    _options: TransportOptions,
    _clock: Arc<dyn ClockPort>,
    _status: Arc<TimestampingStatus>,
) -> io::Result<OpenedTransport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{} は Linux のみ対応しています", mode.label()),
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
            result.err().map(|error| error.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn hardware_timestamping_never_falls_back_silently() {
        // Linux ではループバックに PHC がなく、Linux 以外ではそもそも使えないので、どちらもエラーになる
        // （ソフトウェアの時刻に黙って切り替えない）
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let status = Arc::new(TimestampingStatus::new());
        let options = TransportOptions {
            timestamping: TimestampingMode::Hardware,
            ..TransportOptions::default()
        };
        let result = open(
            TransportMode::Udp,
            "127.0.0.1:123".parse().unwrap(),
            options,
            clock,
            Arc::clone(&status),
        );
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(io::ErrorKind::Unsupported)
        );
        let counts = status.counts();
        assert!(!counts.hardware_enabled, "{counts:?}");
        assert!(!counts.kernel_enabled, "{counts:?}");
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
            result.err().map(|error| error.kind()),
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
            result.err().map(|error| error.kind()),
            Some(io::ErrorKind::Unsupported)
        );
    }
    #[test]
    fn link_only_options_are_rejected_in_other_modes() {
        use domain::packet_crafting::Ipv4Spec;
        let link = TransportOptions {
            link: Some(LinkOptions::default()),
            ..TransportOptions::default()
        };
        assert!(validate(TransportMode::Udp, &link).is_err());
        assert!(validate(TransportMode::RawIp, &link).is_err());
        let ip_checksum = TransportOptions {
            headers: Some(HeaderTemplate {
                ipv4: Ipv4Spec {
                    header_checksum: Some(0x1234),
                    ..Ipv4Spec::default()
                },
                ..HeaderTemplate::default()
            }),
            ..TransportOptions::default()
        };
        assert!(validate(TransportMode::RawIp, &ip_checksum).is_err());
        if cfg!(target_os = "linux") {
            validate(TransportMode::Link, &ip_checksum).unwrap();
            validate(TransportMode::Link, &link).unwrap();
        }
    }
}
