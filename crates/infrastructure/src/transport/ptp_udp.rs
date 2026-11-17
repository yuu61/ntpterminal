//! PTP の UDP の送受信（[`PtpPort`] の実装）。PTP のスレーブ（`--ptp`）で使う。
//!
//! - イベント用（既定 319 番）と一般用（その次の番号）の 2 つのソケットを、どちらも全アドレスに bind する
//!   （インターフェースがループバックなら、ループバックに）。
//!   `SO_REUSEADDR` を付けるので、同じホストの ptp4l と同じポートを使える（マルチキャストは両方に届く。
//!   ユニキャストで届く `Delay_Resp` は、どちらか一方にしか届かないことがある）。1024 未満のポートなので、
//!   Linux では root か `CAP_NET_BIND_SERVICE` が要る
//! - マルチキャストのグループ（224.0.1.129）に、使うインターフェースで参加する。Linux では `IP_MULTICAST_ALL` を切り、
//!   ほかのインターフェース（ほかのプログラムが参加したもの）に届いたマルチキャストを受け取らない（全アドレスに bind した
//!   ソケットには、既定ではそれも届き、別の NIC のマスタを選んでしまう）。`Delay_Req` もそのインターフェースから
//!   TTL 1 で送る（既定のプロファイルと ptp4l の既定と同じ）。インターフェースがループバックのときは参加しない
//!   （疑似マスタとユニキャストで試すとき。Linux のループバックはマルチキャストに対応していない）
//! - `Delay_Req` の宛先は、マスタのアドレスを指定すればそこ（ユニキャスト。フラグも立てるので、マスタは
//!   ユニキャストで応える）、しなければマルチキャストのグループ
//! - 受信はソケットごとのスレッドで待ち、受け取った直後に時計を読む（ユーザ空間の時刻）。Linux でカーネルの
//!   タイムスタンプを使うときは、イベント用のソケットの受信時刻と、送信時刻（エラーキュー。送ったスレッドで読む）も
//!   取り、`rebase_kernel_time` でモノトニック時計に写す
//! - 受信のスレッドを止める合図: Linux は 100ms ごとの受信タイムアウトで確かめる。Windows はタイムアウトと到着が
//!   重なるとパケットを取りこぼすので（`sim_server` の先頭）、無期限に待ち、止めるときは空のデータグラムで起こす

use alloc::sync::Arc;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::io::{self, ErrorKind};
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};

use application::description::PtpTransportInfo;
use application::port::{ClockPort, PtpInbound, PtpPort, TransportError};
use application::timestamping::{TimestampSource, TimestampingStatus};
use domain::ptp_clock::EventStamp;
#[cfg(target_os = "linux")]
use domain::shared_kernel::LocalStamp;
use socket2::{Domain, Protocol, SockRef, Socket, Type};

use super::TimestampingMode;
#[cfg(target_os = "linux")]
use nix::ifaddrs::getifaddrs;
#[cfg(target_os = "linux")]
use nix::sys::socket::SockaddrIn;

#[cfg(target_os = "linux")]
use super::kernel_ts::{enable_software_timestamps, recv_with_timestamp_from, software_send_time};
#[cfg(target_os = "linux")]
use crate::clock::rebase_kernel_time;

/// PTP のイベント用のポート（一般用は 320）
pub const PTP_EVENT_PORT: u16 = 319;
/// PTP の既定のマルチキャストのグループ（224.0.1.129）
pub const PTP_PRIMARY_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
/// 受信バッファ（PTP のメッセージは TLV を含めても数百バイト）
const RECV_BUFFER_LEN: usize = 1500;
/// Linux で受信のスレッドが停止の合図を確かめる間隔
#[cfg(not(windows))]
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// 使うインターフェースの指定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PtpInterface {
    /// インターフェースの IPv4 アドレス
    Address(Ipv4Addr),
    /// インターフェースの名前（Linux のみ）
    Name(String),
}

impl PtpInterface {
    /// IPv4 アドレスならアドレス、そうでなければ名前とみなす。
    #[must_use]
    pub fn parse(text: &str) -> Self {
        text.parse()
            .map_or_else(|_| Self::Name(text.to_owned()), Self::Address)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtpUdpOptions {
    /// 使うインターフェース。`None` なら経路（マスタか、マルチキャストのグループへの経路）で決める
    pub interface: Option<PtpInterface>,
    /// イベント用のポート（一般用はその次の番号）。自分が bind するポートと、送る宛先のポートの両方
    pub event_port: u16,
    /// マスタのアドレス。指定すると `Delay_Req` をユニキャストで送る（ポートを省いたらイベント用のポート）
    pub master: Option<SocketAddr>,
    pub timestamping: TimestampingMode,
}

/// 開いた送受信の口。
pub struct OpenedPtp {
    pub port: PtpUdp,
    /// 開いたもの（インターフェース・自分のアドレス・ポート・`Delay_Req` の宛先）
    pub info: PtpTransportInfo,
    /// 設定した方式で取れる時刻の出どころ（カーネルの時刻を使わなければ `User`）
    pub precise_source: TimestampSource,
    /// `Delay_Req` をユニキャストで送るか
    pub unicast: bool,
}

impl fmt::Debug for OpenedPtp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedPtp")
            .field("info", &self.info)
            .field("precise_source", &self.precise_source)
            .field("unicast", &self.unicast)
            .finish_non_exhaustive()
    }
}

/// PTP の送受信の口を開く。
///
/// # Errors
///
/// ポートを使えない（権限がない・使用中）、インターフェースが見つからない、使えないタイムスタンプの方式を
/// 指定したとき。
pub fn open_ptp(
    options: &PtpUdpOptions,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
) -> io::Result<OpenedPtp> {
    let general_port = options.event_port.checked_add(1).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "イベント用のポートの次の番号（一般用）がありません",
        )
    })?;
    let use_kernel = kernel_timestamps(options.timestamping)?;
    let local = interface_address(options.interface.as_ref(), options.master)?;
    let multicast = !local.is_loopback();
    // ループバックではマルチキャストのグループに参加しないので、マルチキャストの `Delay_Req` には誰も応えない
    if !multicast && options.master.is_none() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "ループバック（{local}）ではマルチキャストを使えません。マスタのアドレスを --ptp-master で指定してください"
            ),
        ));
    }
    // ループバックではマルチキャストを使わないので、ループバックに bind する（Windows のファイアウォールの確認を避ける）
    let bind_ip = if multicast {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    };
    let event = bind_port(bind_ip, options.event_port)?;
    let general = bind_port(bind_ip, general_port)?;
    if multicast {
        for socket in [&event, &general] {
            socket.join_multicast_v4(&PTP_PRIMARY_GROUP, &local).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "マルチキャストのグループ {PTP_PRIMARY_GROUP} に {local} で参加できません: {error}"
                    ),
                )
            })?;
        }
        SockRef::from(&event).set_multicast_if_v4(&local)?;
        event.set_multicast_ttl_v4(1)?;
        #[cfg(target_os = "linux")]
        for socket in [&event, &general] {
            SockRef::from(socket).set_multicast_all_v4(false)?;
        }
    }
    #[cfg(target_os = "linux")]
    let kernel = use_kernel && enable_kernel(&event, options.timestamping)?;
    // Linux 以外では、カーネルのタイムスタンプを使う指定はここまでにエラーか false になっている
    #[cfg(not(target_os = "linux"))]
    let kernel = use_kernel;
    status.set_kernel_enabled(kernel);
    let destination = options.master.map_or_else(
        || SocketAddr::V4(SocketAddrV4::new(PTP_PRIMARY_GROUP, options.event_port)),
        |master| {
            if master.port() == 0 {
                SocketAddr::new(master.ip(), options.event_port)
            } else {
                master
            }
        },
    );
    let info = describe(
        options.interface.as_ref(),
        local,
        options.event_port,
        destination,
        options.master.is_some(),
    );
    let port = PtpUdp::start(event, general, destination, clock, status, kernel)?;
    Ok(OpenedPtp {
        port,
        info,
        precise_source: if kernel {
            TimestampSource::Kernel
        } else {
            TimestampSource::User
        },
        unicast: options.master.is_some(),
    })
}

/// 表示に渡す説明。タイムスタンプの方式は含めない（実際に取れた方式は取得状況として別に出す）。
fn describe(
    interface: Option<&PtpInterface>,
    local: Ipv4Addr,
    event_port: u16,
    destination: SocketAddr,
    unicast: bool,
) -> PtpTransportInfo {
    PtpTransportInfo {
        interface: match interface.cloned() {
            Some(PtpInterface::Name(name)) => Some(name),
            Some(PtpInterface::Address(_)) | None => None,
        },
        local,
        event_port,
        unicast_to: unicast.then_some(destination),
    }
}

/// カーネルのタイムスタンプを使うか（使えない方式ならエラー）。`Auto` は「使えれば使う」。
fn kernel_timestamps(mode: TimestampingMode) -> io::Result<bool> {
    match mode {
        TimestampingMode::User => Ok(false),
        TimestampingMode::Auto => Ok(cfg!(target_os = "linux")),
        TimestampingMode::Kernel if cfg!(target_os = "linux") => Ok(true),
        TimestampingMode::Kernel => Err(io::Error::new(
            ErrorKind::Unsupported,
            "カーネルのタイムスタンプは Linux のみ対応しています",
        )),
        TimestampingMode::Hardware => Err(io::Error::new(
            ErrorKind::Unsupported,
            "PTP のスレーブ（--ptp）では、ハードウェアのタイムスタンプにまだ対応していません（--timestamping kernel を使ってください）",
        )),
    }
}

/// イベント用のソケットでカーネルのタイムスタンプを有効にする。`Auto` で使えなければ `false`。
#[cfg(target_os = "linux")]
fn enable_kernel(socket: &UdpSocket, mode: TimestampingMode) -> io::Result<bool> {
    match enable_software_timestamps(socket) {
        Ok(()) => Ok(true),
        Err(_) if mode == TimestampingMode::Auto => Ok(false),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("カーネルのタイムスタンプを有効にできません: {error}"),
        )),
    }
}

/// 使うインターフェースの IPv4 アドレス。指定がなければ、マスタ（なければマルチキャストのグループ）への経路で決める。
fn interface_address(
    interface: Option<&PtpInterface>,
    master: Option<SocketAddr>,
) -> io::Result<Ipv4Addr> {
    match interface.cloned() {
        Some(PtpInterface::Address(address)) => Ok(address),
        Some(PtpInterface::Name(name)) => address_of(&name),
        None => {
            let target = master.map_or(IpAddr::V4(PTP_PRIMARY_GROUP), |address| address.ip());
            let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
            let local = probe
                .connect((target, PTP_EVENT_PORT))
                .and_then(|()| probe.local_addr())
                .map(|local| local.ip());
            match local {
                Ok(IpAddr::V4(address)) if !address.is_unspecified() => Ok(address),
                Ok(_) | Err(_) => Err(io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "{target} への経路がありません（使うインターフェースを --ptp-interface で指定してください）"
                    ),
                )),
            }
        }
    }
}

/// インターフェース `name` の IPv4 アドレス（Linux）。
#[cfg(target_os = "linux")]
fn address_of(name: &str) -> io::Result<Ipv4Addr> {
    getifaddrs()?
        .filter(|entry| entry.interface_name == name)
        .find_map(|entry| Some(entry.address.as_ref()?.as_sockaddr_in()?.ip()))
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("{name} に IPv4 アドレスがありません（インターフェースの名前を確かめてください）"),
            )
        })
}

#[cfg(not(target_os = "linux"))]
fn address_of(name: &str) -> io::Result<Ipv4Addr> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!(
            "インターフェースの名前（{name}）での指定は Linux のみ対応しています。IPv4 アドレスで指定してください"
        ),
    ))
}

/// `ip` の `port` に、`SO_REUSEADDR` を付けて bind する。
fn bind_port(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    let address = SocketAddr::from((ip, port));
    socket.bind(&address.into()).map_err(|error| {
        let hint = if error.kind() == ErrorKind::PermissionDenied {
            "（1024 未満のポートは root か CAP_NET_BIND_SERVICE が要ります）"
        } else if error.kind() == ErrorKind::AddrInUse {
            "（ほかのプログラムが使っています）"
        } else {
            ""
        };
        io::Error::new(
            error.kind(),
            format!("UDP の {port} 番を使えません{hint}: {error}"),
        )
    })?;
    Ok(socket.into())
}

/// PTP の送受信の口。drop すると受信のスレッドを止める。
pub struct PtpUdp {
    event: UdpSocket,
    destination: SocketAddr,
    clock: Arc<dyn ClockPort>,
    status: Arc<TimestampingStatus>,
    kernel: bool,
    /// 次の送信に付く `OPT_ID` の番号
    #[cfg(target_os = "linux")]
    next_tx_id: u32,
    inbox: Receiver<Result<PtpInbound, String>>,
    stop: Arc<AtomicBool>,
    threads: Vec<(u16, JoinHandle<()>)>,
}

impl fmt::Debug for PtpUdp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtpUdp")
            .field("event", &self.event)
            .field("destination", &self.destination)
            .field("kernel", &self.kernel)
            .finish_non_exhaustive()
    }
}

impl PtpUdp {
    fn start(
        event: UdpSocket,
        general: UdpSocket,
        destination: SocketAddr,
        clock: Arc<dyn ClockPort>,
        status: Arc<TimestampingStatus>,
        kernel: bool,
    ) -> io::Result<Self> {
        let (sender, inbox) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for (socket, is_event) in [(event.try_clone()?, true), (general, false)] {
            let port = socket.local_addr()?.port();
            #[cfg(not(windows))]
            socket.set_read_timeout(Some(READ_TIMEOUT))?;
            let receiver = Receiving {
                socket,
                event: is_event,
                kernel: kernel && is_event,
                clock: Arc::clone(&clock),
                sender: sender.clone(),
                stop: Arc::clone(&stop),
            };
            let name = if is_event { "ptp-event" } else { "ptp-general" };
            let handle = thread::Builder::new()
                .name(name.to_owned())
                .spawn(move || receiver.run())?;
            threads.push((port, handle));
        }
        Ok(Self {
            event,
            destination,
            clock,
            status,
            kernel,
            #[cfg(target_os = "linux")]
            next_tx_id: 0,
            inbox,
            stop,
            threads,
        })
    }

    /// カーネルの送信時刻をモノトニック時計に写す。取れなければ `None`。
    #[cfg(target_os = "linux")]
    fn kernel_send_time(&mut self) -> Option<LocalStamp> {
        let id = self.next_tx_id;
        self.next_tx_id = self.next_tx_id.wrapping_add(1);
        let kernel = software_send_time(&self.event, id)?;
        rebase_kernel_time(kernel, self.clock.now())
    }
}

impl PtpPort for PtpUdp {
    fn send_event(&mut self, payload: &[u8]) -> Result<EventStamp, TransportError> {
        let user = self.clock.now();
        let _: usize = self
            .event
            .send_to(payload, self.destination)
            .map_err(|error| TransportError::Io(error.to_string()))?;
        #[cfg(target_os = "linux")]
        let precise = if self.kernel {
            self.kernel_send_time()
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let precise = None;
        if self.kernel && precise.is_none() {
            self.status.record_tx_miss();
        }
        Ok(EventStamp { precise, user })
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<PtpInbound>, TransportError> {
        match self.inbox.recv_timeout(timeout) {
            Ok(Ok(inbound)) => Ok(Some(inbound)),
            Ok(Err(error)) => Err(TransportError::Io(error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(TransportError::Io(
                "受信のスレッドが止まりました".to_owned(),
            )),
        }
    }
}

impl Drop for PtpUdp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for (port, handle) in self.threads.drain(..) {
            // Windows は無期限に待っているので起こす。起こせなかったら待たない（プロセスの終了で片付く）
            if cfg!(windows) && wake(port).is_err() {
                continue;
            }
            drop(handle.join());
        }
    }
}

/// `port` で受信を待っているソケットに、ループバックから空のデータグラムを送って起こす。
fn wake(port: u16) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    let _: usize = socket.send_to(&[], (Ipv4Addr::LOCALHOST, port))?;
    Ok(())
}

/// 1 つのソケットの受信のスレッド。
struct Receiving {
    socket: UdpSocket,
    event: bool,
    kernel: bool,
    clock: Arc<dyn ClockPort>,
    sender: Sender<Result<PtpInbound, String>>,
    stop: Arc<AtomicBool>,
}

impl Receiving {
    fn run(self) {
        let mut buf = vec![0u8; RECV_BUFFER_LEN];
        while !self.stop.load(Ordering::Relaxed) {
            let message = match self.receive(&mut buf) {
                Ok(Some(inbound)) if !inbound.payload.is_empty() => Ok(inbound),
                // 時間切れと、止めるときの空のデータグラム
                Ok(_) => continue,
                Err(error) => Err(error.to_string()),
            };
            let failed = message.is_err();
            if self.sender.send(message).is_err() {
                return;
            }
            if failed {
                // 壊れたソケットで空回りしない
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// 1 つ受信する。時間切れなら `Ok(None)`。
    fn receive(&self, buf: &mut [u8]) -> io::Result<Option<PtpInbound>> {
        let received = if self.kernel {
            self.receive_with_kernel_time(buf)
        } else {
            self.socket.recv_from(buf).map(|(len, from)| {
                let user = self.clock.now();
                (
                    len,
                    from,
                    EventStamp {
                        precise: None,
                        user,
                    },
                )
            })
        };
        match received {
            Ok((len, from, stamp)) => Ok(Some(PtpInbound {
                payload: buf[..len].to_vec(),
                event: self.event,
                received: stamp,
                from,
            })),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock
                        | ErrorKind::TimedOut
                        | ErrorKind::Interrupted
                        | ErrorKind::ConnectionReset
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(target_os = "linux")]
    fn receive_with_kernel_time(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, EventStamp)> {
        let (len, times, sender) = recv_with_timestamp_from::<SockaddrIn, _>(&self.socket, buf)?;
        let user = self.clock.now();
        let precise = times
            .software()
            .and_then(|kernel| rebase_kernel_time(kernel, user));
        let from = sender.map_or_else(
            || SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            |sin| SocketAddr::V4(SocketAddrV4::new(sin.ip(), sin.port())),
        );
        Ok((len, from, EventStamp { precise, user }))
    }

    #[cfg(not(target_os = "linux"))]
    fn receive_with_kernel_time(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, EventStamp)> {
        let (len, from) = self.socket.recv_from(buf)?;
        let user = self.clock.now();
        Ok((
            len,
            from,
            EventStamp {
                precise: None,
                user,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;

    #[test]
    fn interfaces_are_addresses_or_names() {
        assert_eq!(
            PtpInterface::parse("192.168.10.200"),
            PtpInterface::Address(Ipv4Addr::new(192, 168, 10, 200))
        );
        assert_eq!(
            PtpInterface::parse("nic0"),
            PtpInterface::Name("nic0".to_owned())
        );
    }

    #[test]
    fn hardware_timestamps_are_not_supported_yet() {
        let error = kernel_timestamps(TimestampingMode::Hardware).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(error.to_string().contains("--timestamping kernel"));
        assert!(!kernel_timestamps(TimestampingMode::User).unwrap());
    }

    /// ループバックではマルチキャストを使わないので、マスタの指定がなければ（ソケットを開く前に）エラーにする。
    #[test]
    fn loopback_needs_a_master_address() {
        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let Err(error) = open_ptp(
            &PtpUdpOptions {
                interface: Some(PtpInterface::Address(Ipv4Addr::LOCALHOST)),
                event_port: 43_319,
                master: None,
                timestamping: TimestampingMode::User,
            },
            clock,
            Arc::new(TimestampingStatus::new()),
        ) else {
            panic!("ループバックでマスタの指定がなければエラーのはず");
        };
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("--ptp-master"), "{error}");
    }

    #[test]
    fn descriptions_name_the_mode() {
        let local = Ipv4Addr::new(192, 168, 10, 200);
        let group = SocketAddr::from((PTP_PRIMARY_GROUP, 319));
        assert_eq!(
            describe(
                Some(&PtpInterface::Name("nic0".to_owned())),
                local,
                319,
                group,
                false
            ),
            PtpTransportInfo {
                interface: Some("nic0".to_owned()),
                local,
                event_port: 319,
                unicast_to: None,
            }
        );
        // アドレスで指定したインターフェースは、自分のアドレスと同じなので名前として出さない
        let master = SocketAddr::from(([192, 168, 10, 50], 319));
        assert_eq!(
            describe(
                Some(&PtpInterface::Address(local)),
                local,
                319,
                master,
                true
            ),
            PtpTransportInfo {
                interface: None,
                local,
                event_port: 319,
                unicast_to: Some(master),
            }
        );
    }
}
