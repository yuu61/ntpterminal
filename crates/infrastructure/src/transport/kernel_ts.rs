//! Linux のカーネル（ソフトウェア）タイムスタンプ（SO_TIMESTAMPING）。
//!
//! - 受信: `recvmsg` の制御メッセージ（SCM_TIMESTAMPING）から、カーネルが受け取った時刻を取る
//! - 送信: エラーキュー（MSG_ERRQUEUE）から、ドライバに渡した時刻を取る。OPT_ID の番号で
//!   「どの送信の時刻か」を照合し、遅れて届いた古い送信の時刻を T1 にしない
//! - **T1 と T4 の出どころをそろえる**: 送信時刻が取れなかった問い合わせでは、受信もユーザ空間の時刻にする
//!   （片方だけカーネルの時刻だと、offset が片道の遅れの半分ほど偏る）。
//!   例外: T1 がカーネルの時刻なのに受信時刻が取れない（制御メッセージの欠落・時計の飛び。まれ）ときは
//!   T1 を後から変えられないので混在になる。その標本は「ユーザ空間」として数える
//!
//! カーネルの時刻は CLOCK_REALTIME なので、[`rebase_kernel_time`] でモノトニック時計に写してから使う。
//! ドメインには常にモノトニック時計の時刻だけを渡す。
//!
//! 未確認: 32 bit の Linux（32 bit 版 Raspberry Pi OS など）では制御メッセージの時刻の形式が違う可能性がある。

use std::io::{self, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use application::port::ClockPort;
use application::timestamping::TimestampingStatus;
use domain::shared_kernel::LocalStamp;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::socket::{
    ControlMessageOwned, MsgFlags, SockaddrLike, TimestampingFlag, Timestamps, recvmsg, setsockopt,
    sockopt,
};
use nix::sys::time::TimeSpec;

use crate::clock::rebase_kernel_time;

/// 送信時刻がエラーキューに届くのを待つ上限。
const TX_WAIT: Duration = Duration::from_millis(2);
/// 送信時刻がこの回数続けて取れなければ、送信時刻の取得をやめる（毎回待たないように）。
const MAX_CONSECUTIVE_TX_MISSES: u32 = 3;
/// sock_extended_err.ee_origin: タイムスタンプ
const SO_EE_ORIGIN_TIMESTAMPING: u8 = 4;
/// sock_extended_err.ee_info: 送信時刻
const SCM_TSTAMP_SND: u32 = 0;
/// AF_PACKET のエラーキューでの制御メッセージの種類（SOL_PACKET / PACKET_TX_TIMESTAMP）。
/// nix は解釈しないので、中身（sock_extended_err）を自分で読む
const SOL_PACKET: i32 = 263;
const PACKET_TX_TIMESTAMP: i32 = 16;

fn rx_flags() -> TimestampingFlag {
    TimestampingFlag::SOF_TIMESTAMPING_SOFTWARE | TimestampingFlag::SOF_TIMESTAMPING_RX_SOFTWARE
}

fn tx_flags() -> TimestampingFlag {
    TimestampingFlag::SOF_TIMESTAMPING_SOFTWARE
        | TimestampingFlag::SOF_TIMESTAMPING_TX_SOFTWARE
        | TimestampingFlag::SOF_TIMESTAMPING_OPT_ID
        | TimestampingFlag::SOF_TIMESTAMPING_OPT_TSONLY
}

/// カーネルのタイムスタンプの取得と、T1 / T4 の出どころの管理。
pub(crate) struct KernelTimestamper {
    status: Arc<TimestampingStatus>,
    /// 受信と送信が同じソケットか（UDP モード）
    shared_socket: bool,
    tx_enabled: bool,
    consecutive_misses: u32,
    /// 次の送信に付く OPT_ID の番号（有効にした時点から 0, 1, 2, …）
    next_tx_id: u32,
    /// 直近の送信の T1 がカーネルの時刻だったか
    last_send_kernel: bool,
}

impl KernelTimestamper {
    /// 受信用・送信用のソケットでカーネルのタイムスタンプを有効にする（UDP モードでは同じソケットを渡す）。
    pub fn enable(
        rx: &impl AsFd,
        tx: &impl AsFd,
        status: Arc<TimestampingStatus>,
    ) -> io::Result<Self> {
        let shared_socket = rx.as_fd().as_raw_fd() == tx.as_fd().as_raw_fd();
        if shared_socket {
            setsockopt(rx, sockopt::Timestamping, &(rx_flags() | tx_flags()))?;
        } else {
            setsockopt(rx, sockopt::Timestamping, &rx_flags())?;
            setsockopt(tx, sockopt::Timestamping, &tx_flags())?;
        }
        status.set_kernel_enabled(true);
        Ok(Self {
            status,
            shared_socket,
            tx_enabled: true,
            consecutive_misses: 0,
            next_tx_id: 0,
            last_send_kernel: false,
        })
    }

    /// 送信が成功した直後に呼ぶ。カーネルの送信時刻が取れればそれを、取れなければ `fallback` を T1 として返す。
    pub fn after_send(
        &mut self,
        tx: &impl AsFd,
        clock: &dyn ClockPort,
        fallback: LocalStamp,
    ) -> LocalStamp {
        let id = self.next_tx_id;
        self.next_tx_id = self.next_tx_id.wrapping_add(1);
        if self.tx_enabled {
            if let Some(kernel) = wait_tx_timestamp(tx, id, TX_WAIT)
                && let Some(stamp) = rebase_kernel_time(kernel, clock.now())
            {
                self.consecutive_misses = 0;
                self.last_send_kernel = true;
                return stamp;
            }
            self.status.record_tx_miss();
            self.consecutive_misses += 1;
            if self.consecutive_misses >= MAX_CONSECUTIVE_TX_MISSES {
                self.disable_tx(tx);
            }
        }
        self.last_send_kernel = false;
        fallback
    }

    /// 受け入れた受信の T4 を決める。T1 がカーネルの時刻だったときだけ、カーネルの受信時刻を使う。
    pub fn choose_received(&self, kernel: Option<i128>, observed: LocalStamp) -> LocalStamp {
        let kernel_stamp = kernel.and_then(|nanos| rebase_kernel_time(nanos, observed));
        match (self.last_send_kernel, kernel_stamp) {
            (true, Some(stamp)) => {
                self.status.record_receive(true);
                stamp
            }
            _ => {
                self.status.record_receive(false);
                observed
            }
        }
    }

    /// 送信時刻の取得をやめる。エラーキューに溜まらないよう、カーネル側の設定も外す。
    fn disable_tx(&mut self, tx: &impl AsFd) {
        let flags = if self.shared_socket {
            rx_flags()
        } else {
            TimestampingFlag::empty()
        };
        let _ = setsockopt(tx, sockopt::Timestamping, &flags);
        drain_error_queue(tx);
        self.tx_enabled = false;
    }
}

/// 受信して、カーネルが付けた受信時刻（Unix ナノ秒）も返す。
pub(crate) fn recv_with_timestamp(
    socket: &impl AsFd,
    buf: &mut [u8],
) -> io::Result<(usize, Option<i128>)> {
    recv_with_timestamp_from::<()>(socket, buf).map(|(len, timestamp, _)| (len, timestamp))
}

/// [`recv_with_timestamp`] と同じだが、送信元のアドレスも返す（AF_PACKET でパケットの種別を見るため）。
pub(crate) fn recv_with_timestamp_from<S: SockaddrLike>(
    socket: &impl AsFd,
    buf: &mut [u8],
) -> io::Result<(usize, Option<i128>, Option<S>)> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsg = nix::cmsg_space!(Timestamps);
    let message = recvmsg::<S>(
        socket.as_fd().as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )?;
    let timestamp = if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        None
    } else {
        message.cmsgs().ok().and_then(|mut cmsgs| {
            cmsgs.find_map(|cmsg| match cmsg {
                ControlMessageOwned::ScmTimestampsns(stamps) => to_unix_nanos(&stamps.system),
                _ => None,
            })
        })
    };
    Ok((message.bytes, timestamp, message.address))
}

/// エラーキューの 1 件。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TxEntry {
    /// 送信時刻の通知なら、その送信の OPT_ID 番号
    id: Option<u32>,
    timestamp: Option<i128>,
}

impl TxEntry {
    /// 期待している送信の時刻なら返す（ほかの送信の時刻や、時刻のない通知は使わない）。
    fn timestamp_for(self, expected_id: u32) -> Option<i128> {
        if self.id == Some(expected_id) {
            self.timestamp
        } else {
            None
        }
    }
}

/// 番号 `expected_id` の送信時刻がエラーキューに届くまで、最大 `wait` 待つ。
fn wait_tx_timestamp(socket: &impl AsFd, expected_id: u32, wait: Duration) -> Option<i128> {
    let deadline = Instant::now() + wait;
    loop {
        while let Some(entry) = read_error_entry(socket) {
            if let Some(timestamp) = entry.timestamp_for(expected_id) {
                drain_error_queue(socket);
                return Some(timestamp);
            }
            // 古い送信の時刻などは捨てる
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        // エラーキューに何か届くと POLLERR で起きる
        let timeout = PollTimeout::try_from(remaining.max(Duration::from_millis(1)))
            .unwrap_or(PollTimeout::ZERO);
        let mut fds = [PollFd::new(socket.as_fd(), PollFlags::empty())];
        if poll(&mut fds, timeout).is_err() {
            return None;
        }
    }
}

/// エラーキューから 1 件読む。空（またはエラー）なら `None`。
fn read_error_entry(socket: &impl AsFd) -> Option<TxEntry> {
    let mut data = [0u8; 64];
    let mut iov = [IoSliceMut::new(&mut data)];
    let mut cmsg = nix::cmsg_space!(
        Timestamps,
        nix::libc::sock_extended_err,
        nix::libc::sockaddr_in6
    );
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
        for cmsg in cmsgs {
            match cmsg {
                ControlMessageOwned::ScmTimestampsns(stamps) => {
                    entry.timestamp = to_unix_nanos(&stamps.system);
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

/// sock_extended_err のバイト列が送信時刻の通知なら、その OPT_ID 番号を返す。
///
/// 並び（Linux）: ee_errno u32 / ee_origin u8 / ee_type u8 / ee_code u8 / ee_pad u8 / ee_info u32 / ee_data u32
fn send_timestamp_id(bytes: &[u8]) -> Option<u32> {
    let bytes = bytes.get(..16)?;
    let origin = bytes[4];
    let info = u32::from_ne_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let data = u32::from_ne_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
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

    #[test]
    fn uses_only_the_expected_send_timestamp() {
        let entry = TxEntry {
            id: Some(7),
            timestamp: Some(123),
        };
        assert_eq!(entry.timestamp_for(7), Some(123));
        assert_eq!(entry.timestamp_for(8), None, "別の送信の時刻は使わない");
        let without_id = TxEntry {
            id: None,
            timestamp: Some(123),
        };
        assert_eq!(
            without_id.timestamp_for(0),
            None,
            "番号のない通知は使わない"
        );
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
