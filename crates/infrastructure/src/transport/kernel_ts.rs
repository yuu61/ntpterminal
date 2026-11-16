//! Linux のカーネルのタイムスタンプ（`SO_TIMESTAMPING`）。ソフトウェアの時刻と、NIC のハードウェアの時刻。
//!
//! - 受信: `recvmsg` の制御メッセージ（`SCM_TIMESTAMPING`）から、カーネルが受け取った時刻を取る
//!   （ハードウェアのときは NIC が受け取った時刻も）
//! - 送信: エラーキュー（`MSG_ERRQUEUE`）から、ドライバに渡した時刻を取る。`OPT_ID` の番号で
//!   「どの送信の時刻か」を照合し、遅れて届いた古い送信の時刻を T1 にしない
//! - ハードウェアのとき（[`super::hardware_ts`]）は、`OPT_TX_SWHW` でソフトウェアとハードウェアの送信時刻を
//!   両方受け取る。ソフトウェアの方が先に届くので、ハードウェアの方を待ち（上限 `TX_WAIT`）、来なければ
//!   ソフトウェアの方を使う。続けて来なければ、ソケットにハードウェアの送信時刻を頼むのをやめる
//!   （NIC の送信が off にされたときに、ドライバを待たせ続けないため）
//! - **T1 と T4 の出どころをそろえる**: T1 がハードウェアなら T4 もハードウェア、T1 がソフトウェアなら T4 も
//!   ソフトウェアにし、送信時刻が取れなかった問い合わせでは受信もユーザ空間の時刻にする
//!   （片方だけ違うと、offset が送信側と受信側の遅れの差の半分ほど偏る）。
//!   例外: T1 を決めた後で受信側に同じ出どころの時刻がない（制御メッセージの欠落・時計の飛び。まれ）ときは、
//!   T1 を後から変えられないので混在になる。T4 には次に良い時刻を使い、その標本は低い方の出どころとして数える
//!
//! カーネルの時刻は `CLOCK_REALTIME`、ハードウェアの時刻は NIC の時計（PHC）なので、REALTIME に直してから
//! [`rebase_kernel_time`] でモノトニック時計に写す。ドメインには常にモノトニック時計の時刻だけを渡す。
//!
//! 未確認: 32 bit の Linux（32 bit 版 Raspberry Pi OS など）では制御メッセージの時刻の形式が違う可能性がある。

use alloc::sync::Arc;
use core::time::Duration;
use std::io::{self, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd as _, OwnedFd};
use std::time::Instant;

use application::port::ClockPort;
use application::timestamping::{TimestampSource, TimestampingStatus};
use domain::shared_kernel::LocalStamp;
use nix::libc::{self, sock_extended_err, sockaddr_in6};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{
    ControlMessageOwned, MsgFlags, SockaddrLike, TimestampingFlag, Timestamps, recvmsg, setsockopt,
    sockopt,
};
use nix::sys::time::TimeSpec;

use super::hardware_ts::HardwareClock;
use crate::clock::rebase_kernel_time;

/// 送信時刻がエラーキューに届くのを待つ上限（e1000e のハードウェアの送信時刻は 30〜140µs で届いた）。
const TX_WAIT: Duration = Duration::from_millis(2);
/// 送信時刻がこの回数続けて取れなければ、その送信時刻の取得をやめる（毎回待たないように）。
const MAX_CONSECUTIVE_TX_MISSES: u32 = 3;
/// `sock_extended_err.ee_origin`: タイムスタンプ
const SO_EE_ORIGIN_TIMESTAMPING: u8 = 4;
/// `sock_extended_err.ee_info`: 送信時刻
const SCM_TSTAMP_SND: u32 = 0;
/// `AF_PACKET` のエラーキューでの制御メッセージの種類（`SOL_PACKET` / `PACKET_TX_TIMESTAMP`）。
/// nix は解釈しないので、中身（`sock_extended_err`）を自分で読む
const SOL_PACKET: i32 = 263;
const PACKET_TX_TIMESTAMP: i32 = 16;

fn rx_flags(hardware: bool) -> TimestampingFlag {
    let software = TimestampingFlag::SOF_TIMESTAMPING_SOFTWARE
        | TimestampingFlag::SOF_TIMESTAMPING_RX_SOFTWARE;
    if hardware {
        software
            | TimestampingFlag::SOF_TIMESTAMPING_RX_HARDWARE
            | TimestampingFlag::SOF_TIMESTAMPING_RAW_HARDWARE
    } else {
        software
    }
}

/// `hardware` ならハードウェアの送信時刻も頼む（NIC の送信が on になってから使うこと）。
fn tx_flags(hardware: bool) -> TimestampingFlag {
    let software = TimestampingFlag::SOF_TIMESTAMPING_SOFTWARE
        | TimestampingFlag::SOF_TIMESTAMPING_TX_SOFTWARE
        | TimestampingFlag::SOF_TIMESTAMPING_OPT_ID
        | TimestampingFlag::SOF_TIMESTAMPING_OPT_TSONLY;
    if hardware {
        // OPT_TX_SWHW: ハードウェアの時刻を待っている送信にも、ソフトウェアの時刻を付けてもらう（nix に定数がない）
        software
            | TimestampingFlag::SOF_TIMESTAMPING_TX_HARDWARE
            | TimestampingFlag::SOF_TIMESTAMPING_RAW_HARDWARE
            | TimestampingFlag::from_bits_retain(libc::SOF_TIMESTAMPING_OPT_TX_SWHW)
    } else {
        software
    }
}

/// カーネルが付けた時刻（どちらも Unix ナノ秒。ハードウェアは PHC の時刻）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct KernelTimes {
    software: Option<i128>,
    hardware: Option<i128>,
}

impl KernelTimes {
    const fn merge(self, other: Self) -> Self {
        Self {
            software: if self.software.is_some() {
                self.software
            } else {
                other.software
            },
            hardware: if self.hardware.is_some() {
                self.hardware
            } else {
                other.hardware
            },
        }
    }

    fn from_timestamps(stamps: &Timestamps) -> Self {
        Self {
            software: to_unix_nanos(&stamps.system),
            hardware: to_unix_nanos(&stamps.hw_raw),
        }
    }

    /// カーネルのソフトウェアの時刻（Unix ナノ秒）。
    pub(super) const fn software(self) -> Option<i128> {
        self.software
    }
}

/// PTP 用: ソケットに、ソフトウェアの受信時刻と送信時刻（`OPT_ID` の番号は有効にした時点から 0, 1, 2, …）を頼む。
///
/// NTP の [`KernelTimestamper`] は「送ってから受け取る」順で T1 と T4 の出どころをそろえるが、PTP は
/// 「受け取って（Sync）から送る（`Delay_Req`）」順なので、出どころのそろえ方は使う側（`ptp_clock`）に任せる。
pub(super) fn enable_software_timestamps(socket: &impl AsFd) -> io::Result<()> {
    setsockopt(
        socket,
        sockopt::Timestamping,
        &(rx_flags(false) | tx_flags(false)),
    )?;
    Ok(())
}

/// PTP 用: 番号 `id` の送信の、カーネルのソフトウェアの送信時刻（Unix ナノ秒）。届かなければ `None`。
pub(super) fn software_send_time(socket: &impl AsFd, id: u32) -> Option<i128> {
    wait_tx_timestamps(socket, id, TX_WAIT, false).software
}

/// カーネルのタイムスタンプの取得と、T1 / T4 の出どころの管理。
pub(super) struct KernelTimestamper {
    status: Arc<TimestampingStatus>,
    /// 受信と送信が同じソケットか（UDP モード）
    shared_socket: bool,
    tx_enabled: bool,
    /// ソケットにハードウェアの送信時刻を頼んでいるか
    tx_hardware: bool,
    consecutive_misses: u32,
    consecutive_hardware_misses: u32,
    /// 次の送信に付く `OPT_ID` の番号（有効にした時点から 0, 1, 2, …）
    next_tx_id: u32,
    /// 直近の送信の T1 の出どころ
    last_send: TimestampSource,
    /// ハードウェアのタイムスタンプ。drop すると、こちらで有効にした NIC の設定を戻す
    hardware: Option<HardwareClock>,
    /// 後片付けで、NIC を戻す前に送信側のソケットのハードウェアの設定を外すための複製
    tx_socket: Option<OwnedFd>,
}

impl KernelTimestamper {
    /// 受信用・送信用のソケットでカーネルのタイムスタンプを有効にする（UDP モードでは同じソケットを渡す）。
    pub(super) fn enable<R, T>(rx: &R, tx: &T, status: Arc<TimestampingStatus>) -> io::Result<Self>
    where
        R: AsFd,
        T: AsFd,
    {
        Self::enable_with(rx, tx, status, None)
    }

    /// [`Self::enable`] と同じだが、NIC のハードウェアの時刻も使う（`hardware` は NIC を有効にした後のもの）。
    pub(super) fn enable_hardware<R, T>(
        rx: &R,
        tx: &T,
        status: Arc<TimestampingStatus>,
        hardware: HardwareClock,
    ) -> io::Result<Self>
    where
        R: AsFd,
        T: AsFd,
    {
        Self::enable_with(rx, tx, status, Some(hardware))
    }

    fn enable_with<R, T>(
        rx: &R,
        tx: &T,
        status: Arc<TimestampingStatus>,
        hardware: Option<HardwareClock>,
    ) -> io::Result<Self>
    where
        R: AsFd,
        T: AsFd,
    {
        let use_hardware = hardware.is_some();
        let shared_socket = rx.as_fd().as_raw_fd() == tx.as_fd().as_raw_fd();
        // 失敗したときは hardware が drop され、NIC の設定が戻る
        let tx_socket = if use_hardware {
            Some(tx.as_fd().try_clone_to_owned()?)
        } else {
            None
        };
        if shared_socket {
            setsockopt(
                rx,
                sockopt::Timestamping,
                &(rx_flags(use_hardware) | tx_flags(use_hardware)),
            )?;
        } else {
            setsockopt(rx, sockopt::Timestamping, &rx_flags(use_hardware))?;
            setsockopt(tx, sockopt::Timestamping, &tx_flags(use_hardware))?;
        }
        status.set_kernel_enabled(true);
        status.set_hardware_enabled(use_hardware);
        Ok(Self {
            status,
            shared_socket,
            tx_enabled: true,
            tx_hardware: use_hardware,
            consecutive_misses: 0,
            consecutive_hardware_misses: 0,
            next_tx_id: 0,
            last_send: TimestampSource::User,
            hardware,
            tx_socket,
        })
    }

    /// 送信が成功した直後に呼ぶ。カーネルの送信時刻（ハードウェアを優先）が取れればそれを、
    /// 取れなければ `fallback` を T1 として返す。
    pub(super) fn after_send<T>(
        &mut self,
        tx: &T,
        clock: &dyn ClockPort,
        fallback: LocalStamp,
    ) -> LocalStamp
    where
        T: AsFd,
    {
        let id = self.next_tx_id;
        self.next_tx_id = self.next_tx_id.wrapping_add(1);
        if !self.tx_enabled {
            self.last_send = TimestampSource::User;
            return fallback;
        }
        let times = wait_tx_timestamps(tx, id, TX_WAIT, self.tx_hardware);
        let observed = clock.now();
        if self.tx_hardware {
            if let Some(stamp) = times
                .hardware
                .and_then(|phc| self.hardware_stamp(phc, observed))
            {
                self.consecutive_hardware_misses = 0;
                self.consecutive_misses = 0;
                self.last_send = TimestampSource::Hardware;
                return stamp;
            }
            self.consecutive_hardware_misses += 1;
            if self.consecutive_hardware_misses >= MAX_CONSECUTIVE_TX_MISSES {
                self.stop_tx_hardware(tx);
            }
        }
        if let Some(stamp) = times
            .software
            .and_then(|kernel| rebase_kernel_time(kernel, observed))
        {
            self.consecutive_misses = 0;
            self.last_send = TimestampSource::Kernel;
            return stamp;
        }
        self.status.record_tx_miss();
        self.consecutive_misses += 1;
        if self.consecutive_misses >= MAX_CONSECUTIVE_TX_MISSES {
            self.disable_tx(tx);
        }
        self.last_send = TimestampSource::User;
        fallback
    }

    /// 受け入れた受信の T4 を決める（T1 と出どころをそろえる）。
    pub(super) fn choose_received(&self, times: KernelTimes, observed: LocalStamp) -> LocalStamp {
        let software = times
            .software
            .and_then(|kernel| rebase_kernel_time(kernel, observed));
        let (chosen, source) = select_receive(self.last_send, software, || {
            self.hardware_stamp(times.hardware?, observed)
        });
        let (stamp, recorded) =
            chosen.map_or((observed, TimestampSource::User), |stamp| (stamp, source));
        self.status.record_receive(recorded);
        stamp
    }

    /// PHC の時刻を REALTIME に直し、モノトニック時計に写す。
    fn hardware_stamp(&self, phc_nanos: i128, observed: LocalStamp) -> Option<LocalStamp> {
        let realtime = self.hardware.as_ref()?.to_realtime(phc_nanos)?;
        rebase_kernel_time(realtime, observed)
    }

    fn socket_flags(&self, tx_enabled: bool, tx_hardware: bool) -> TimestampingFlag {
        let tx = if tx_enabled {
            tx_flags(tx_hardware)
        } else {
            TimestampingFlag::empty()
        };
        if self.shared_socket {
            rx_flags(self.hardware.is_some()) | tx
        } else {
            tx
        }
    }

    /// ハードウェアの送信時刻を頼むのをやめる（ソフトウェアの送信時刻は続ける）。
    #[expect(
        clippy::let_underscore_must_use,
        reason = "外せなくても、ハードウェアの送信時刻を待たなくなるだけなので続ける"
    )]
    fn stop_tx_hardware(&mut self, tx: &impl AsFd) {
        let flags = self.socket_flags(true, false);
        let _: nix::Result<()> = setsockopt(tx, sockopt::Timestamping, &flags);
        drain_error_queue(tx);
        self.tx_hardware = false;
    }

    /// 送信時刻の取得をやめる。エラーキューに溜まらないよう、カーネル側の設定も外す。
    #[expect(
        clippy::let_underscore_must_use,
        reason = "後片付けなので、設定を外せなくても続ける（溜まった通知はこの後の drain で捨てる）"
    )]
    fn disable_tx(&mut self, tx: &impl AsFd) {
        let flags = self.socket_flags(false, false);
        let _: nix::Result<()> = setsockopt(tx, sockopt::Timestamping, &flags);
        drain_error_queue(tx);
        self.tx_enabled = false;
        self.tx_hardware = false;
    }
}

impl Drop for KernelTimestamper {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "後片付けなので、外せなくても NIC の設定は戻す"
    )]
    fn drop(&mut self) {
        // NIC の設定を戻す前に、ソケットにハードウェアの送信時刻を頼むのをやめる。順番が逆だと、
        // 送信 off の NIC に時刻を頼むことになる（e1000e はドライバを 1 秒待たせる）
        if let Some(socket) = self.tx_socket.take() {
            let _: nix::Result<()> =
                setsockopt(&socket, sockopt::Timestamping, &TimestampingFlag::empty());
        }
        drop(self.hardware.take());
    }
}

/// T1 の出どころ `last_send` に合わせて T4 を選び、その標本の出どころ（T1 と T4 の低い方）と一緒に返す。
/// 選べなければ `None`（T4 はユーザ空間の時刻になる）。ハードウェアの時刻は要るときだけ読む（ioctl を伴う）。
fn select_receive(
    last_send: TimestampSource,
    software: Option<LocalStamp>,
    hardware: impl FnOnce() -> Option<LocalStamp>,
) -> (Option<LocalStamp>, TimestampSource) {
    match last_send {
        TimestampSource::Hardware => hardware()
            .map_or((software, TimestampSource::Kernel), |stamp| {
                (Some(stamp), TimestampSource::Hardware)
            }),
        TimestampSource::Kernel => (software.or_else(hardware), TimestampSource::Kernel),
        TimestampSource::User => (None, TimestampSource::User),
    }
}

/// 受信して、カーネルが付けた受信時刻（Unix ナノ秒）も返す。
pub(super) fn recv_with_timestamp<F>(socket: &F, buf: &mut [u8]) -> io::Result<(usize, KernelTimes)>
where
    F: AsFd,
{
    recv_with_timestamp_from::<(), _>(socket, buf).map(|(len, times, _)| (len, times))
}

/// [`recv_with_timestamp`] と同じだが、送信元のアドレスも返す（`AF_PACKET` でパケットの種別を見るため）。
pub(super) fn recv_with_timestamp_from<S, F>(
    socket: &F,
    buf: &mut [u8],
) -> io::Result<(usize, KernelTimes, Option<S>)>
where
    S: SockaddrLike,
    F: AsFd,
{
    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsg = nix::cmsg_space!(Timestamps);
    let message = recvmsg::<S>(
        socket.as_fd().as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )?;
    let times = if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        KernelTimes::default()
    } else {
        message
            .cmsgs()
            .ok()
            .and_then(|mut cmsgs| {
                cmsgs.find_map(|control| {
                    if let ControlMessageOwned::ScmTimestampsns(stamps) = control {
                        Some(KernelTimes::from_timestamps(&stamps))
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default()
    };
    Ok((message.bytes, times, message.address))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TxEntry {
    /// 送信時刻の通知なら、その送信の `OPT_ID` 番号
    id: Option<u32>,
    times: KernelTimes,
}

impl TxEntry {
    /// 期待している送信の時刻なら返す（ほかの送信の時刻や、時刻のない通知は使わない）。
    fn times_for(self, expected_id: u32) -> KernelTimes {
        if self.id == Some(expected_id) {
            self.times
        } else {
            KernelTimes::default()
        }
    }
}

/// 番号 `expected_id` の送信時刻がエラーキューに届くまで、最大 `wait` 待つ。
/// `want_hardware` ならハードウェアの時刻が届くまで待つ（先に届くソフトウェアの時刻も返す）。
fn wait_tx_timestamps(
    socket: &impl AsFd,
    expected_id: u32,
    wait: Duration,
    want_hardware: bool,
) -> KernelTimes {
    let deadline = Instant::now() + wait;
    let mut found = KernelTimes::default();
    loop {
        // 届いているものはすべて読む（古い送信の時刻などは捨てる）
        while let Some(entry) = read_error_entry(socket) {
            found = found.merge(entry.times_for(expected_id));
        }
        let complete = if want_hardware {
            found.hardware.is_some()
        } else {
            found.software.is_some()
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if complete || remaining.is_zero() {
            return found;
        }
        // エラーキューに何か届くと POLLERR で起きる
        let timeout = PollTimeout::try_from(remaining.max(Duration::from_millis(1)))
            .unwrap_or(PollTimeout::ZERO);
        let mut fds = [PollFd::new(socket.as_fd(), PollFlags::empty())];
        if poll(&mut fds, timeout).is_err() {
            return found;
        }
    }
}

/// エラーキューから 1 件読む。空（またはエラー）なら `None`。
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "nix の ControlMessageOwned は non_exhaustive で OS ごとに変種が違うため、列挙できない"
)]
fn read_error_entry(socket: &impl AsFd) -> Option<TxEntry> {
    let mut data = [0u8; 64];
    let mut iov = [IoSliceMut::new(&mut data)];
    let mut cmsg = nix::cmsg_space!(Timestamps, sock_extended_err, sockaddr_in6);
    let message = recvmsg::<()>(
        socket.as_fd().as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::MSG_ERRQUEUE | MsgFlags::MSG_DONTWAIT,
    )
    .ok()?;
    let mut entry = TxEntry::default();
    if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        // 制御メッセージが切れている: 時刻は使わない
        return Some(entry);
    }
    if let Ok(cmsgs) = message.cmsgs() {
        for control in cmsgs {
            match control {
                ControlMessageOwned::ScmTimestampsns(stamps) => {
                    entry.times = KernelTimes::from_timestamps(&stamps);
                }
                // 送信時刻の通知だけを使う（ICMP エラーなどは無視）
                ControlMessageOwned::Ipv4RecvErr(error, _)
                | ControlMessageOwned::Ipv6RecvErr(error, _)
                    if error.ee_origin == SO_EE_ORIGIN_TIMESTAMPING
                        && error.ee_info == SCM_TSTAMP_SND =>
                {
                    entry.id = Some(error.ee_data);
                }
                // AF_PACKET では sock_extended_err が SOL_PACKET / PACKET_TX_TIMESTAMP で届く
                ControlMessageOwned::Unknown(unknown)
                    if unknown.cmsg_header.cmsg_level == SOL_PACKET
                        && unknown.cmsg_header.cmsg_type == PACKET_TX_TIMESTAMP =>
                {
                    if let Some(id) = send_timestamp_id(&unknown.data_bytes) {
                        entry.id = Some(id);
                    }
                }
                _ => {}
            }
        }
    }
    Some(entry)
}

/// `sock_extended_err` のバイト列が送信時刻の通知なら、その `OPT_ID` 番号を返す。
///
/// 並び（Linux）: `ee_errno` u32 / `ee_origin` u8 / `ee_type` u8 / `ee_code` u8 / `ee_pad` u8 / `ee_info` u32 / `ee_data` u32
fn send_timestamp_id(bytes: &[u8]) -> Option<u32> {
    let header = bytes.first_chunk::<16>()?;
    let origin = header[4];
    let info = u32::from_ne_bytes([header[8], header[9], header[10], header[11]]);
    let data = u32::from_ne_bytes([header[12], header[13], header[14], header[15]]);
    (origin == SO_EE_ORIGIN_TIMESTAMPING && info == SCM_TSTAMP_SND).then_some(data)
}

/// エラーキューを空にする（溜まると受信バッファを圧迫して応答が落ちるため）。
fn drain_error_queue(socket: &impl AsFd) {
    while read_error_entry(socket).is_some() {}
}

/// timespec を Unix ナノ秒にする。0 は「時刻なし」。
fn to_unix_nanos(time: &TimeSpec) -> Option<i128> {
    let secs = i128::from(time.tv_sec());
    let nanos = i128::from(time.tv_nsec());
    (secs != 0 || nanos != 0).then_some(secs * 1_000_000_000 + nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::shared_kernel::{NtpTimestamp, PcTime};

    fn stamp(mono_nanos: u64) -> LocalStamp {
        LocalStamp {
            mono: PcTime::monotonic(mono_nanos),
            wall: NtpTimestamp::from_unix_nanos(1_763_281_545_000_000_000),
        }
    }

    #[test]
    fn uses_only_the_expected_send_timestamp() {
        let times = KernelTimes {
            software: Some(123),
            hardware: Some(456),
        };
        let entry = TxEntry { id: Some(7), times };
        assert_eq!(entry.times_for(7), times);
        assert_eq!(
            entry.times_for(8),
            KernelTimes::default(),
            "別の送信の時刻は使わない"
        );
        let without_id = TxEntry { id: None, times };
        assert_eq!(
            without_id.times_for(0),
            KernelTimes::default(),
            "番号のない通知は使わない"
        );
    }

    #[test]
    fn merges_software_and_hardware_entries_of_one_send() {
        // OPT_TX_SWHW では、同じ送信の時刻がソフトウェア・ハードウェアの 2 件に分かれて届く
        let software = KernelTimes {
            software: Some(100),
            hardware: None,
        };
        let hardware = KernelTimes {
            software: None,
            hardware: Some(900),
        };
        let merged = KernelTimes::default().merge(software).merge(hardware);
        assert_eq!(
            merged,
            KernelTimes {
                software: Some(100),
                hardware: Some(900),
            }
        );
        // 先に届いた方を後から上書きしない
        let later = KernelTimes {
            software: Some(200),
            hardware: Some(1_000),
        };
        assert_eq!(merged.merge(later), merged);
    }

    #[test]
    fn receive_follows_the_source_of_t1() {
        let software = Some(stamp(1_000));
        let hardware = || Some(stamp(900));
        assert_eq!(
            select_receive(TimestampSource::Hardware, software, hardware),
            (Some(stamp(900)), TimestampSource::Hardware)
        );
        assert_eq!(
            select_receive(TimestampSource::Kernel, software, hardware),
            (Some(stamp(1_000)), TimestampSource::Kernel),
            "T1 がソフトウェアなら、ハードウェアの時刻があっても T4 はソフトウェア"
        );
        assert_eq!(
            select_receive(TimestampSource::User, software, hardware),
            (None, TimestampSource::User),
            "T1 がユーザ空間なら T4 もユーザ空間"
        );
    }

    #[test]
    fn mixed_samples_are_counted_as_the_lower_source() {
        let no_hardware = || None;
        assert_eq!(
            select_receive(TimestampSource::Hardware, Some(stamp(1_000)), no_hardware),
            (Some(stamp(1_000)), TimestampSource::Kernel),
            "T1 がハードウェアで T4 がソフトウェアなら、カーネルとして数える"
        );
        assert_eq!(
            select_receive(TimestampSource::Hardware, None, no_hardware).0,
            None,
            "どちらもなければユーザ空間の時刻にする"
        );
        assert_eq!(
            select_receive(TimestampSource::Kernel, None, || Some(stamp(900))),
            (Some(stamp(900)), TimestampSource::Kernel)
        );
    }

    #[test]
    fn reads_hardware_time_only_when_needed() {
        let mut read = false;
        let (_, source) = select_receive(TimestampSource::Kernel, Some(stamp(1_000)), || {
            read = true;
            None
        });
        assert_eq!(source, TimestampSource::Kernel);
        assert!(!read, "T1 がソフトウェアなら PHC を読まない");
    }

    #[test]
    fn requests_hardware_send_time_only_when_asked() {
        let hardware_bits = TimestampingFlag::SOF_TIMESTAMPING_TX_HARDWARE;
        assert!(!tx_flags(false).contains(hardware_bits));
        assert!(tx_flags(true).contains(hardware_bits));
        assert_eq!(
            tx_flags(true).bits() & libc::SOF_TIMESTAMPING_OPT_TX_SWHW,
            libc::SOF_TIMESTAMPING_OPT_TX_SWHW
        );
        assert!(!rx_flags(false).contains(TimestampingFlag::SOF_TIMESTAMPING_RAW_HARDWARE));
        assert!(rx_flags(true).contains(TimestampingFlag::SOF_TIMESTAMPING_RAW_HARDWARE));
    }

    #[test]
    fn reads_send_timestamp_id_from_extended_error_bytes() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&42u32.to_ne_bytes()); // ee_errno（ENOMSG）
        bytes.extend_from_slice(&[SO_EE_ORIGIN_TIMESTAMPING, 0, 0, 0]);
        bytes.extend_from_slice(&SCM_TSTAMP_SND.to_ne_bytes());
        bytes.extend_from_slice(&9u32.to_ne_bytes()); // OPT_ID 番号
        assert_eq!(send_timestamp_id(&bytes), Some(9));

        let mut other_origin = bytes.clone();
        other_origin[4] = 2; // ICMP
        assert_eq!(send_timestamp_id(&other_origin), None);
        assert_eq!(
            send_timestamp_id(&bytes[..15]),
            None,
            "短すぎるものは読まない"
        );
    }

    #[test]
    fn zero_timespec_means_no_timestamp() {
        assert_eq!(to_unix_nanos(&TimeSpec::new(0, 0)), None);
        assert_eq!(to_unix_nanos(&TimeSpec::new(2, 5)), Some(2_000_000_005));
    }
}
