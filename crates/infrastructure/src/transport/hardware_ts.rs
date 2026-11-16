//! NIC のハードウェアタイムスタンプ（Linux）。`--timestamping hardware` のときだけ使う。
//!
//! 決まりごと（理由は、Intel I219-V（e1000e）と Linux ブリッジで確かめた結果から）:
//! - 使う NIC: 送信元 IP を持つインターフェースが PHC（NIC の時計）を持てばそれ。持たなければ sysfs の `lower_*` を
//!   下へたどり、使える NIC が 1 つだけならそれを使う（ブリッジ vmbr0 → nic0 など）。0 個・2 個以上ならエラー。
//!   ブリッジ越しでも、ソケットは送信元 IP のインターフェースのままでハードウェアの時刻が届く
//! - NIC の設定（`SIOCGHWTSTAMP`）が「送信 on・受信 `ALL` か `NTP_ALL`」なら何もしない（終了時も戻さない）。
//!   そうでなく、NIC が対応していれば有効にして（`SIOCSHWTSTAMP`）、終了時に元の設定（flags も含む）に戻す。
//!   戻すのは drop のときと、SIGINT / SIGTERM / SIGHUP を受けたとき（ハンドラで戻してから、同じシグナルで終わる）と、
//!   メインスレッドが panic したとき（プロセスが終わり、同期スレッドの送受信の口は drop されないため）。
//!   起動前に無視する設定だったシグナル（`nohup` の SIGHUP など）は、無視のまま変えない
//! - ソケットに `TX_HARDWARE` を頼むのは、NIC の送信が on になってから。e1000e は送信 off のまま頼まれると、
//!   1 秒待って「clearing Tx timestamp hang」を出し、その間はほかの送信の時刻も取れなくなる
//! - PHC の時刻は、その都度 PHC − REALTIME を測って REALTIME の時刻に直し、ソフトウェアの時刻と同じ道
//!   （`rebase_kernel_time`）でモノトニック時計に写す。測り方は `PTP_SYS_OFFSET_PRECISE`（NIC とシステムの時計を
//!   同時に読む）で、使えなければ `PTP_SYS_OFFSET_EXTENDED`（前後の読みの幅が最小の標本）
//!
//! 限界:
//! - root（NIC の設定に `CAP_NET_ADMIN`、`/dev/ptpN` を開く権限）が要る
//! - SIGKILL・`panic = "abort"` などで戻せなかった NIC は、次の起動では「有効になっている」と見て、何もせず戻しもしない
//! - 後から起動したものは、先に起動したものが有効にした NIC を「有効」と見る。先のものが終わって NIC を戻すと、
//!   後のものはハードウェアの送信時刻が取れなくなり、続けて取れなければソフトウェアの時刻に切り替える
//! - シグナルで戻せる NIC は 1 プロセスに 1 つ（送受信の口は 1 つしか開かないため）

use alloc::collections::BTreeSet;
use core::ffi::c_int;
use core::iter;
use core::net::IpAddr;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd as _;
use std::sync::Once;
use std::{panic, thread};

use nix::errno::Errno;
use nix::ifaddrs::getifaddrs;
use nix::libc;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, raise, sigaction};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};

/// `linux/net_tstamp.h` の `HWTSTAMP_TX_ON`
const HWTSTAMP_TX_ON: c_int = 1;
/// `linux/net_tstamp.h`: 受信したすべてのパケットに時刻を付ける
const HWTSTAMP_FILTER_ALL: c_int = 1;
/// `linux/net_tstamp.h`: NTP のパケットに時刻を付ける（対応している NIC は少ない）
const HWTSTAMP_FILTER_NTP_ALL: c_int = 15;
/// `linux/ethtool.h` の `ETHTOOL_GET_TS_INFO`
const ETHTOOL_GET_TS_INFO: u32 = 0x41;
/// NIC がハードウェアの時刻をソケットに渡せることを示す `SOF_TIMESTAMPING_*` の組
const HARDWARE_REPORTING: u32 = libc::SOF_TIMESTAMPING_TX_HARDWARE
    | libc::SOF_TIMESTAMPING_RX_HARDWARE
    | libc::SOF_TIMESTAMPING_RAW_HARDWARE;
/// `PTP_SYS_OFFSET_EXTENDED` で読む標本の数
const EXTENDED_SAMPLES: u32 = 5;
/// `linux/ptp_clock.h` の `PTP_MAX_SAMPLES`（`ptp_sys_offset_extended` の配列の大きさ）
const PTP_MAX_SAMPLES: usize = 25;
/// `lower_*` をたどる深さの上限（ループの安全網）
const MAX_LOWER_DEPTH: usize = 8;
const NANOS_PER_SEC: i128 = 1_000_000_000;
const ZERO_TIME: libc::ptp_clock_time = libc::ptp_clock_time {
    sec: 0,
    nsec: 0,
    reserved: 0,
};

/// NIC のハードウェアタイムスタンプ。drop すると、有効にした NIC の設定を元に戻す。
pub(super) struct HardwareClock {
    device: String,
    phc: PhcClock,
    setting: NicSetting,
}

impl HardwareClock {
    /// 送信元 IP `local` の経路にある NIC を探し、必要なら有効にして、PHC を開く。
    pub(super) fn prepare(local: IpAddr) -> io::Result<Self> {
        let egress = interface_with_address(local)?;
        let device = find_timestamping_device(&egress, lower_devices, inspect)?;
        let capabilities = ts_info(&device)?;
        let phc_index = capabilities.phc().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{device} は PHC（NIC の時計）を持っていません"),
            )
        })?;
        // 権限が足りないときに NIC の設定を変えてしまわないよう、PHC を先に開く
        let phc = PhcClock::open(phc_index)?;
        let setting = NicSetting::apply(&device, capabilities)?;
        Ok(Self {
            device,
            phc,
            setting,
        })
    }

    /// PHC の時刻を、いまの PHC − REALTIME を使って REALTIME（Unix ナノ秒）に直す。測れなければ `None`。
    pub(super) fn to_realtime(&self, phc_nanos: i128) -> Option<i128> {
        let offset = self.phc.phc_minus_realtime().ok()?;
        Some(phc_nanos - offset)
    }

    /// 画面に出す説明（"nic0 の HW 時刻" など。こちらで有効にしたときは、終了時に戻すことも示す）。
    pub(super) fn description(&self) -> String {
        if self.setting.changed() {
            // link モードの説明に付けても 80 桁に収まる長さにする
            format!("{} の HW 時刻（一時的に有効化）", self.device)
        } else {
            format!("{} の HW 時刻", self.device)
        }
    }
}

/// NIC のタイムスタンプの設定（`hwtstamp_config`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NicConfig {
    flags: c_int,
    tx_type: c_int,
    rx_filter: c_int,
}

impl NicConfig {
    /// NTP の送信・受信の両方に時刻が付く設定か。
    const fn timestamps_ntp(self) -> bool {
        self.tx_type == HWTSTAMP_TX_ON
            && matches!(
                self.rx_filter,
                HWTSTAMP_FILTER_ALL | HWTSTAMP_FILTER_NTP_ALL
            )
    }

    const fn to_raw(self) -> libc::hwtstamp_config {
        libc::hwtstamp_config {
            flags: self.flags,
            tx_type: self.tx_type,
            rx_filter: self.rx_filter,
        }
    }

    const fn from_raw(raw: libc::hwtstamp_config) -> Self {
        Self {
            flags: raw.flags,
            tx_type: raw.tx_type,
            rx_filter: raw.rx_filter,
        }
    }
}

/// NIC が対応しているタイムスタンプ（`ETHTOOL_GET_TS_INFO`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Capabilities {
    so_timestamping: u32,
    phc_index: i32,
    tx_types: u32,
    rx_filters: u32,
}

impl Capabilities {
    /// PHC の番号（`/dev/ptpN` の N）。持たなければ `None`。
    fn phc(self) -> Option<u32> {
        u32::try_from(self.phc_index).ok()
    }

    /// NTP の受信に使える受信フィルタ（`NTP_ALL` があればそれ、なければ `ALL`）。
    fn rx_filter_for_ntp(self) -> Option<c_int> {
        [HWTSTAMP_FILTER_NTP_ALL, HWTSTAMP_FILTER_ALL]
            .into_iter()
            .find(|filter| self.rx_filters & (1u32 << filter) != 0)
    }

    /// NTP にハードウェアタイムスタンプを使えない理由。使えれば `None`。
    fn problem(self) -> Option<&'static str> {
        if self.so_timestamping & HARDWARE_REPORTING != HARDWARE_REPORTING {
            Some("ハードウェアの時刻をソケットに渡せない")
        } else if self.tx_types & (1u32 << HWTSTAMP_TX_ON) == 0 {
            Some("送信のハードウェアタイムスタンプに対応していない")
        } else if self.rx_filter_for_ntp().is_none() {
            Some("受信フィルタに ALL・NTP_ALL がない（PTP のパケットにしか時刻を付けられない）")
        } else {
            None
        }
    }
}

/// NTP に使えるようにするための新しい設定。今のままで使えるなら `Ok(None)`。
fn plan(current: NicConfig, capabilities: Capabilities) -> Result<Option<NicConfig>, &'static str> {
    if current.timestamps_ntp() {
        return Ok(None);
    }
    if let Some(problem) = capabilities.problem() {
        return Err(problem);
    }
    let rx_filter = capabilities
        .rx_filter_for_ntp()
        .ok_or("受信フィルタに ALL・NTP_ALL がない")?;
    Ok(Some(NicConfig {
        flags: current.flags,
        tx_type: HWTSTAMP_TX_ON,
        rx_filter,
    }))
}

/// 1 つのインターフェースを調べた結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inspection {
    /// PHC を持たない（ブリッジなど）。下位のインターフェースを調べる
    NoClock,
    Usable,
    /// PHC を持つが、NTP には使えない（理由）
    Unusable(&'static str),
}

fn inspect(name: &str) -> Inspection {
    match ts_info(name) {
        Ok(capabilities) if capabilities.phc().is_some() => capabilities
            .problem()
            .map_or(Inspection::Usable, Inspection::Unusable),
        Ok(_) | Err(_) => Inspection::NoClock,
    }
}

/// `egress` から下位（`lower`）へたどり、ハードウェアタイムスタンプを使える NIC を 1 つに決める。
fn find_timestamping_device(
    egress: &str,
    lowers: impl Fn(&str) -> Vec<String>,
    inspect_device: impl Fn(&str) -> Inspection,
) -> io::Result<String> {
    let mut usable = BTreeSet::new();
    let mut unusable = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending = vec![(egress.to_owned(), 0usize)];
    while let Some((name, depth)) = pending.pop() {
        if depth > MAX_LOWER_DEPTH || !seen.insert(name.clone()) {
            continue;
        }
        match inspect_device(&name) {
            Inspection::Usable => {
                let _new = usable.insert(name);
            }
            Inspection::Unusable(reason) => unusable.push(format!("{name}: {reason}")),
            Inspection::NoClock => {
                pending.extend(lowers(&name).into_iter().map(|lower| (lower, depth + 1)));
            }
        }
    }
    let mut candidates: Vec<String> = usable.into_iter().collect();
    if candidates.len() > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{egress} の下にハードウェアタイムスタンプを使える NIC が複数あり、どれを使うか決められません（{}）",
                candidates.join("・")
            ),
        ));
    }
    if let Some(device) = candidates.pop() {
        return Ok(device);
    }
    if unusable.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{egress} とその下のインターフェースに、PHC（NIC の時計）を持つ NIC がありません"
            ),
        ))
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{egress} では NTP にハードウェアタイムスタンプを使えません（{}）",
                unusable.join("、")
            ),
        ))
    }
}

/// sysfs の `lower_*`（ブリッジのポート、VLAN・bond の下位の NIC など）。
fn lower_devices(name: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(format!("/sys/class/net/{name}")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix("lower_")
                .map(str::to_owned)
        })
        .collect()
}

/// 送信元 IP `local` を持つインターフェースの名前。
fn interface_with_address(local: IpAddr) -> io::Result<String> {
    getifaddrs()?
        .find(|entry| {
            entry.address.as_ref().is_some_and(|address| match local {
                IpAddr::V4(v4) => address.as_sockaddr_in().is_some_and(|sin| sin.ip() == v4),
                IpAddr::V6(v6) => address
                    .as_sockaddr_in6()
                    .is_some_and(|sin6| sin6.ip() == v6),
            })
        })
        .map(|entry| entry.interface_name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("送信元 {local} を持つインターフェースが見つかりません"),
            )
        })
}

/// 有効にした NIC の設定を、drop で元に戻す。
struct NicSetting {
    device: String,
    /// こちらで設定を変えたときの、元の設定
    previous: Option<NicConfig>,
}

impl NicSetting {
    fn apply(device: &str, capabilities: Capabilities) -> io::Result<Self> {
        let current = get_config(device).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{device} のタイムスタンプの設定を読めません: {error}"),
            )
        })?;
        let planned = plan(current, capabilities).map_err(|problem| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{device} は NTP にハードウェアタイムスタンプを使えません（{problem}）"),
            )
        })?;
        let Some(wanted) = planned else {
            return Ok(Self {
                device: device.to_owned(),
                previous: None,
            });
        };
        // 設定を変える前に、シグナルで戻せるようにしておく
        arm_signal_restore(device, current);
        let applied = match set_config(device, wanted) {
            Ok(applied) => applied,
            Err(error) => {
                disarm_signal_restore();
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{device} のハードウェアタイムスタンプを有効にできません（root か CAP_NET_ADMIN が必要です）: {error}"
                    ),
                ));
            }
        };
        // ここから先は drop で元に戻る
        let setting = Self {
            device: device.to_owned(),
            previous: Some(current),
        };
        if !applied.timestamps_ntp() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{device} のドライバが設定を受け付けません（送信 {}、受信フィルタ {}）",
                    applied.tx_type, applied.rx_filter
                ),
            ));
        }
        Ok(setting)
    }

    const fn changed(&self) -> bool {
        self.previous.is_some()
    }
}

impl Drop for NicSetting {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            // 戻している途中でシグナルが来ても、ハンドラが同じ設定を書くだけなので、戻してから外す。
            // 後片付けなので、戻せなくても続ける（戻せないときにできることはない）
            drop(set_config(&self.device, previous));
            disarm_signal_restore();
        }
    }
}

fn get_config(device: &str) -> io::Result<NicConfig> {
    let mut raw = NicConfig {
        flags: 0,
        tx_type: 0,
        rx_filter: 0,
    }
    .to_raw();
    interface_ioctl(device.as_bytes(), InterfaceRequest::GetConfig(&mut raw))?;
    Ok(NicConfig::from_raw(raw))
}

/// 設定して、ドライバが実際に入れた設定を返す（ドライバは受信フィルタを広げることがある）。
fn set_config(device: &str, config: NicConfig) -> io::Result<NicConfig> {
    let mut raw = config.to_raw();
    interface_ioctl(device.as_bytes(), InterfaceRequest::SetConfig(&mut raw))?;
    Ok(NicConfig::from_raw(raw))
}

fn ts_info(device: &str) -> io::Result<Capabilities> {
    let mut info = EthtoolTsInfo {
        _cmd: ETHTOOL_GET_TS_INFO,
        ..EthtoolTsInfo::default()
    };
    interface_ioctl(device.as_bytes(), InterfaceRequest::TsInfo(&mut info))?;
    Ok(Capabilities {
        so_timestamping: info.so_timestamping,
        phc_index: info.phc_index,
        tx_types: info.tx_types,
        rx_filters: info.rx_filters,
    })
}

/// `linux/ethtool.h` の `struct ethtool_ts_info`。
/// 先頭が `_` の項目は、カーネルに渡すだけで読まない。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct EthtoolTsInfo {
    _cmd: u32,
    so_timestamping: u32,
    phc_index: i32,
    tx_types: u32,
    _tx_reserved: [u32; 3],
    rx_filters: u32,
    _rx_reserved: [u32; 3],
}

/// インターフェースへの ioctl の種類と、`ifr_data` が指す構造体（種類ごとに型が決まる）。
enum InterfaceRequest<'data> {
    GetConfig(&'data mut libc::hwtstamp_config),
    SetConfig(&'data mut libc::hwtstamp_config),
    TsInfo(&'data mut EthtoolTsInfo),
}

/// インターフェース `name` への ioctl。ヒープを使わない（シグナルハンドラからも呼ぶ）。
#[expect(
    unsafe_code,
    reason = "NIC のタイムスタンプの設定と対応状況は、ifreq を使う ioctl でしか読み書きできない"
)]
fn interface_ioctl(name: &[u8], request: InterfaceRequest<'_>) -> Result<(), Errno> {
    if name.is_empty() || name.len() >= libc::IFNAMSIZ || name.contains(&0) {
        return Err(Errno::EINVAL);
    }
    let mut ifr_name: [libc::c_char; libc::IFNAMSIZ] = [0; libc::IFNAMSIZ];
    for (slot, byte) in ifr_name.iter_mut().zip(name) {
        *slot = libc::c_char::from_ne_bytes([*byte]);
    }
    let (code, data): (libc::Ioctl, *mut libc::c_char) = match request {
        InterfaceRequest::GetConfig(config) => (libc::SIOCGHWTSTAMP, ptr::from_mut(config).cast()),
        InterfaceRequest::SetConfig(config) => (libc::SIOCSHWTSTAMP, ptr::from_mut(config).cast()),
        InterfaceRequest::TsInfo(info) => (libc::SIOCETHTOOL, ptr::from_mut(info).cast()),
    };
    let mut block = libc::ifreq {
        ifr_name,
        ifr_ifru: libc::__c_anonymous_ifr_ifru { ifru_data: data },
    };
    let control = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    // SAFETY: `block` は名前と ifr_data を入れた ifreq で、この呼び出しの間は生きている。ifr_data は
    // InterfaceRequest の種類ごとにカーネルが期待する構造体（hwtstamp_config / ethtool_ts_info）を指し、
    // その構造体も借用でこの呼び出しの間は生きている。カーネルが書くのはその構造体の大きさの範囲だけ。
    let result = unsafe { libc::ioctl(control.as_raw_fd(), code, &raw mut block) };
    if result.is_negative() {
        return Err(Errno::last());
    }
    Ok(())
}

/// PHC（`/dev/ptpN`）と、システムの時計との差の測り方。
struct PhcClock {
    file: File,
    method: OffsetMethod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OffsetMethod {
    /// NIC とシステムの時計を同時に読む（対応している NIC だけ）
    Precise,
    /// システムの時計で PHC の読みを挟み、幅が最小の標本を使う
    Extended,
}

impl PhcClock {
    fn open(index: u32) -> io::Result<Self> {
        let path = format!("/dev/ptp{index}");
        let file = File::open(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{path} を開けません（root が必要です）: {error}"),
            )
        })?;
        let mut clock = Self {
            file,
            method: OffsetMethod::Precise,
        };
        if clock.phc_minus_realtime().is_err() {
            clock.method = OffsetMethod::Extended;
            let _offset = clock.phc_minus_realtime().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("{path} とシステムの時計の差を測れません: {error}"),
                )
            })?;
        }
        Ok(clock)
    }

    /// いまの PHC − REALTIME（ナノ秒）。
    fn phc_minus_realtime(&self) -> io::Result<i128> {
        match self.method {
            OffsetMethod::Precise => {
                let mut request = libc::ptp_sys_offset_precise {
                    device: ZERO_TIME,
                    sys_realtime: ZERO_TIME,
                    sys_monoraw: ZERO_TIME,
                    rsv: [0; 4],
                };
                ptp_ioctl(&self.file, PtpRequest::Precise(&mut request))?;
                Ok(nanos(request.device) - nanos(request.sys_realtime))
            }
            OffsetMethod::Extended => {
                let mut request = libc::ptp_sys_offset_extended {
                    n_samples: EXTENDED_SAMPLES,
                    clockid: libc::CLOCK_REALTIME,
                    rsv: [0; 2],
                    ts: [[ZERO_TIME; 3]; PTP_MAX_SAMPLES],
                };
                ptp_ioctl(&self.file, PtpRequest::Extended(&mut request))?;
                let samples: Vec<[i128; 3]> = request
                    .ts
                    .iter()
                    .take(usize::try_from(request.n_samples).unwrap_or(0))
                    .map(|sample| sample.map(nanos))
                    .collect();
                narrowest_offset(&samples).ok_or_else(|| {
                    io::Error::other("PHC とシステムの時計の差を測れる標本がありません")
                })
            }
        }
    }
}

/// `[前のシステムの時刻, PHC の時刻, 後のシステムの時刻]` の標本から、前後の幅が最小のものを選び、
/// PHC − システムの時刻（前後の中点）を返す。
fn narrowest_offset(samples: &[[i128; 3]]) -> Option<i128> {
    samples
        .iter()
        .filter(|&&[before, _, after]| after >= before)
        .min_by_key(|&&[before, _, after]| after - before)
        .map(|&[before, phc, after]| phc - before.midpoint(after))
}

fn nanos(time: libc::ptp_clock_time) -> i128 {
    i128::from(time.sec) * NANOS_PER_SEC + i128::from(time.nsec)
}

/// PHC への ioctl の種類と、渡す構造体（種類ごとに型が決まる）。
enum PtpRequest<'data> {
    Precise(&'data mut libc::ptp_sys_offset_precise),
    Extended(&'data mut libc::ptp_sys_offset_extended),
}

#[expect(
    unsafe_code,
    reason = "PHC とシステムの時計の差は、/dev/ptpN への ioctl でしか読めない"
)]
fn ptp_ioctl(file: &File, request: PtpRequest<'_>) -> io::Result<()> {
    let (code, data): (libc::Ioctl, *mut libc::c_void) = match request {
        PtpRequest::Precise(offset) => (libc::PTP_SYS_OFFSET_PRECISE, ptr::from_mut(offset).cast()),
        PtpRequest::Extended(offset) => {
            (libc::PTP_SYS_OFFSET_EXTENDED, ptr::from_mut(offset).cast())
        }
    };
    // SAFETY: `file` は開いている /dev/ptpN。`data` は要求の種類ごとにカーネルが期待する構造体
    // （ptp_sys_offset_precise / ptp_sys_offset_extended）を指し、借用でこの呼び出しの間は生きている。
    let result = unsafe { libc::ioctl(file.as_raw_fd(), code, data) };
    if result.is_negative() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---- シグナルを受けたときに NIC の設定を戻す ----
// ハンドラの中ではヒープも Mutex も使えないので、戻す内容を atomic に置いておく。

static RESTORE_ARMED: AtomicBool = AtomicBool::new(false);
static RESTORE_NAME: [AtomicU8; libc::IFNAMSIZ] = [const { AtomicU8::new(0) }; libc::IFNAMSIZ];
static RESTORE_FLAGS: AtomicI32 = AtomicI32::new(0);
static RESTORE_TX_TYPE: AtomicI32 = AtomicI32::new(0);
static RESTORE_RX_FILTER: AtomicI32 = AtomicI32::new(0);
static INSTALL_HANDLERS: Once = Once::new();

fn arm_signal_restore(device: &str, previous: NicConfig) {
    for (slot, byte) in RESTORE_NAME
        .iter()
        .zip(device.bytes().chain(iter::repeat(0)))
    {
        slot.store(byte, Ordering::Relaxed);
    }
    RESTORE_FLAGS.store(previous.flags, Ordering::Relaxed);
    RESTORE_TX_TYPE.store(previous.tx_type, Ordering::Relaxed);
    RESTORE_RX_FILTER.store(previous.rx_filter, Ordering::Relaxed);
    RESTORE_ARMED.store(true, Ordering::SeqCst);
    INSTALL_HANDLERS.call_once(install_signal_handlers);
}

fn disarm_signal_restore() {
    RESTORE_ARMED.store(false, Ordering::SeqCst);
}

#[expect(
    unsafe_code,
    reason = "シグナルハンドラの登録（sigaction）は unsafe な関数でしかできない"
)]
#[expect(
    clippy::let_underscore_must_use,
    reason = "登録できなくても NIC の設定は drop で戻る（シグナルで終わったときだけ戻らない）ので、続ける"
)]
fn install_signal_handlers() {
    let action = SigAction::new(
        SigHandler::Handler(restore_then_reraise),
        SaFlags::SA_RESETHAND,
        SigSet::empty(),
    );
    for signal in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        // SAFETY: ハンドラ（restore_then_reraise）はヒープもロックも使わず、atomic の読み書き・socket・ioctl・
        // close・raise だけを行う（どれも async-signal-safe）。
        let Ok(previous) = (unsafe { sigaction(signal, &action) }) else {
            continue;
        };
        if matches!(previous.handler(), SigHandler::SigIgn) {
            // 無視する設定だった（nohup の SIGHUP など）なら、無視のまま戻す
            // SAFETY: 元の設定（無視）に戻すだけ。
            let _: nix::Result<SigAction> = unsafe { sigaction(signal, &previous) };
        }
    }
    // メインスレッドの panic ではプロセスが終わり、同期スレッドの送受信の口は drop されないので、ここで戻す。
    // ほかのスレッドの panic では戻さない（同期スレッドなら drop で戻り、それ以外では計測が続くため）
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if thread::current().name() == Some("main") {
            restore_if_armed();
        }
        previous_hook(info);
    }));
}

/// NIC の設定を戻してから、同じシグナルを出し直す。`SA_RESETHAND` で既定の動作に戻っているので、
/// ハンドラから戻った後にそのシグナルの既定の動作（終了）になる。
#[expect(
    clippy::let_underscore_must_use,
    reason = "シグナルハンドラの中なので、失敗しても知らせる手段がない"
)]
extern "C" fn restore_then_reraise(signal: c_int) {
    restore_if_armed();
    if let Ok(received) = Signal::try_from(signal) {
        let _: nix::Result<()> = raise(received);
    }
}

/// 戻す予定の設定があれば、NIC に書いて予定を消す。ヒープを使わない（シグナルハンドラからも呼ぶ）。
#[expect(
    clippy::let_underscore_must_use,
    reason = "シグナルハンドラや panic の中から呼ぶので、失敗しても知らせる手段がない"
)]
fn restore_if_armed() {
    if !RESTORE_ARMED.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut name = [0u8; libc::IFNAMSIZ];
    for (byte, slot) in name.iter_mut().zip(&RESTORE_NAME) {
        *byte = slot.load(Ordering::Relaxed);
    }
    let len = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    let mut config = NicConfig {
        flags: RESTORE_FLAGS.load(Ordering::Relaxed),
        tx_type: RESTORE_TX_TYPE.load(Ordering::Relaxed),
        rx_filter: RESTORE_RX_FILTER.load(Ordering::Relaxed),
    }
    .to_raw();
    let _: Result<(), Errno> =
        interface_ioctl(&name[..len], InterfaceRequest::SetConfig(&mut config));
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    const OFF: NicConfig = NicConfig {
        flags: 0,
        tx_type: 0,
        rx_filter: 0,
    };

    /// I219-V（e1000e）: pve-home の `ethtool -T` の表示（送信 off / on、受信フィルタ none・all・PTP の各種）から
    const E1000E: Capabilities = Capabilities {
        so_timestamping: 0x5f,
        phc_index: 0,
        tx_types: 0b11,
        rx_filters: 0x7db3,
    };

    /// X710（i40e）: 同じく `ethtool -T` の表示から（受信フィルタが none と PTP の各種だけ）
    const I40E: Capabilities = Capabilities {
        so_timestamping: 0x5f,
        phc_index: 1,
        tx_types: 0b11,
        rx_filters: 0x7ff1,
    };

    #[test]
    fn leaves_an_already_usable_nic_alone() {
        for rx_filter in [HWTSTAMP_FILTER_ALL, HWTSTAMP_FILTER_NTP_ALL] {
            let current = NicConfig {
                flags: 0,
                tx_type: HWTSTAMP_TX_ON,
                rx_filter,
            };
            assert_eq!(plan(current, E1000E), Ok(None));
            // 対応状況が読めなくても、使える設定なら触らない
            assert_eq!(plan(current, Capabilities::default()), Ok(None));
        }
    }

    #[test]
    fn enables_a_disabled_nic_keeping_its_flags() {
        let current = NicConfig { flags: 1, ..OFF };
        assert_eq!(
            plan(current, E1000E),
            Ok(Some(NicConfig {
                flags: 1,
                tx_type: HWTSTAMP_TX_ON,
                rx_filter: HWTSTAMP_FILTER_ALL,
            }))
        );
    }

    #[test]
    fn widens_a_ptp_only_receive_filter() {
        // ptp4l などが送信 on・受信 PTP だけにしていた
        let current = NicConfig {
            flags: 0,
            tx_type: HWTSTAMP_TX_ON,
            rx_filter: 12,
        };
        let wanted = plan(current, E1000E).unwrap().unwrap();
        assert_eq!(wanted.rx_filter, HWTSTAMP_FILTER_ALL);
        assert!(wanted.timestamps_ntp());
    }

    #[test]
    fn prefers_the_ntp_only_filter_when_offered() {
        let capabilities = Capabilities {
            rx_filters: E1000E.rx_filters | (1 << HWTSTAMP_FILTER_NTP_ALL),
            ..E1000E
        };
        assert_eq!(
            plan(OFF, capabilities).unwrap().unwrap().rx_filter,
            HWTSTAMP_FILTER_NTP_ALL
        );
    }

    #[test]
    fn rejects_nics_that_cannot_timestamp_ntp() {
        let ptp_only = plan(OFF, I40E).unwrap_err();
        assert!(ptp_only.contains("受信フィルタ"), "{ptp_only}");
        let no_tx = Capabilities {
            tx_types: 0b01,
            ..E1000E
        };
        assert!(plan(OFF, no_tx).unwrap_err().contains("送信"));
        let no_raw = Capabilities {
            so_timestamping: libc::SOF_TIMESTAMPING_TX_HARDWARE
                | libc::SOF_TIMESTAMPING_RX_HARDWARE,
            ..E1000E
        };
        assert!(plan(OFF, no_raw).unwrap_err().contains("ソケット"));
        assert_eq!(I40E.phc(), Some(1));
        assert_eq!(
            Capabilities {
                phc_index: -1,
                ..E1000E
            }
            .phc(),
            None
        );
    }

    /// 偽のインターフェースの構成で、使う NIC を決める。
    fn resolve(
        topology: &[(&str, &[&str])],
        clock_table: &[(&str, Inspection)],
        egress: &str,
    ) -> io::Result<String> {
        let lowers: BTreeMap<&str, Vec<String>> = topology
            .iter()
            .map(|&(name, lower)| {
                let names = lower.iter().map(|&lower_name| lower_name.to_owned());
                (name, names.collect())
            })
            .collect();
        let clocks: BTreeMap<&str, Inspection> = clock_table.iter().copied().collect();
        find_timestamping_device(
            egress,
            |name| lowers.get(name).cloned().unwrap_or_default(),
            |name| clocks.get(name).copied().unwrap_or(Inspection::NoClock),
        )
    }

    #[test]
    fn uses_the_egress_nic_when_it_has_a_clock() {
        assert_eq!(
            resolve(&[], &[("eth0", Inspection::Usable)], "eth0").unwrap(),
            "eth0"
        );
    }

    #[test]
    fn finds_the_physical_port_under_a_bridge() {
        // pve-home の vmbr0: 物理ポート nic0、VM の tap と veth
        let topology: &[(&str, &[&str])] = &[("vmbr0", &["nic0", "tap222i0", "fwpr101p0"])];
        let clocks = [("nic0", Inspection::Usable)];
        assert_eq!(resolve(topology, &clocks, "vmbr0").unwrap(), "nic0");
        // VLAN の上のブリッジ（vmbr0.10 → vmbr0 → nic0）
        let stacked: &[(&str, &[&str])] = &[("vmbr0.10", &["vmbr0"]), ("vmbr0", &["nic0"])];
        assert_eq!(resolve(stacked, &clocks, "vmbr0.10").unwrap(), "nic0");
    }

    #[test]
    fn refuses_to_guess_between_several_nics() {
        let topology: &[(&str, &[&str])] = &[("br0", &["eth0", "eth1"])];
        let clocks = [("eth0", Inspection::Usable), ("eth1", Inspection::Usable)];
        let error = resolve(topology, &clocks, "br0").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("eth0・eth1"), "{error}");
    }

    #[test]
    fn explains_why_no_nic_can_be_used() {
        let topology: &[(&str, &[&str])] = &[("br0", &["eth0"])];
        let none = resolve(topology, &[], "br0").unwrap_err();
        assert_eq!(none.kind(), io::ErrorKind::Unsupported);
        let ptp_only = resolve(
            topology,
            &[("eth0", Inspection::Unusable("受信フィルタ"))],
            "br0",
        )
        .unwrap_err();
        assert!(
            ptp_only.to_string().contains("eth0: 受信フィルタ"),
            "{ptp_only}"
        );
    }

    #[test]
    fn survives_loops_in_the_topology() {
        let topology: &[(&str, &[&str])] = &[("a", &["b"]), ("b", &["a", "eth0"])];
        let clocks = [("eth0", Inspection::Usable)];
        assert_eq!(resolve(topology, &clocks, "a").unwrap(), "eth0");
    }

    #[test]
    fn picks_the_narrowest_extended_sample() {
        let samples = [
            [1_000, 5_000_600, 1_200], // 幅 200
            [2_000, 5_001_550, 2_100], // 幅 100 → 中点 2_050、PHC − システム = 4_999_500
            [3_000, 5_002_000, 2_900], // 前後が逆（時計が戻された）: 使わない
        ];
        assert_eq!(narrowest_offset(&samples), Some(4_999_500));
        assert_eq!(narrowest_offset(&[]), None);
    }

    #[test]
    fn converts_ptp_clock_time_to_nanoseconds() {
        let time = libc::ptp_clock_time {
            sec: 2,
            nsec: 5,
            reserved: 0,
        };
        assert_eq!(nanos(time), 2_000_000_005);
    }

    #[test]
    fn rejects_interface_names_that_do_not_fit() {
        let mut raw = OFF.to_raw();
        assert_eq!(
            interface_ioctl(b"", InterfaceRequest::GetConfig(&mut raw)),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            interface_ioctl(b"0123456789abcdef", InterfaceRequest::GetConfig(&mut raw)),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn loopback_has_no_hardware_clock() {
        assert_eq!(inspect("lo"), Inspection::NoClock);
        let Err(error) = HardwareClock::prepare(IpAddr::from([127, 0, 0, 1])) else {
            panic!("ループバックではハードウェアタイムスタンプを使えないはず");
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported, "{error}");
    }
}
