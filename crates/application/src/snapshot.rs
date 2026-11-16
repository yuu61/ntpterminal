//! 読み取り用モデル: 表示側（CLI / TUI）が集約を直接触らずに済むようにする。

use domain::ptp_clock::{PhcSource, PhcState, PhcStats, PtpStatus};
use domain::receiver::ReceiverStatus;
use domain::shared_kernel::{PcTime, TimeDelta};
use domain::time_sync::{
    Anchor, ExchangeStats, FrequencyOffset, Sample, ServerInfo, SyncState, TimeSource,
};

use crate::acl::mib::MibProfile;
use crate::usecase::MonitorError;

/// ある瞬間の時刻源の状態。コピーして別スレッド（TUI）へ渡せる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncSnapshot {
    pub state: SyncState,
    pub stats: ExchangeStats,
    pub poll_interval: TimeDelta,
    /// 大きい時計の基準点
    pub anchor: Option<Anchor>,
    pub last_sample: Option<Sample>,
    /// クロックフィルタが選んだ標本（直近で「往復遅延の半分 + 古さ × 15 ppm」が最小）
    pub selected_sample: Option<Sample>,
    /// サーバの時計がモノトニック時計より速く進む割合の推定値（基準点の補正に使っている値。推定前は `None`）
    pub frequency: Option<FrequencyOffset>,
    pub server_info: Option<ServerInfo>,
    pub last_success: Option<PcTime>,
    pub next_poll: Option<PcTime>,
    pub awaiting_reply: bool,
}

/// ptp4l が合わせる PHC（[`PhcSource`]）の状態。問い合わせのたびに別スレッド（TUI）へ渡す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhcSnapshot {
    pub state: PhcState,
    pub stats: PhcStats,
    pub poll_interval: TimeDelta,
    /// 大きい時計の基準点（ptp4l が同期を知らせるまでは `None`）
    pub anchor: Option<Anchor>,
    pub last_sample: Option<Sample>,
    pub selected_sample: Option<Sample>,
    pub frequency: Option<FrequencyOffset>,
    /// 最後に読めた ptp4l の状態。直近の問い合わせが失敗しても残るので、`status_error` も見ること
    pub status: Option<PtpStatus>,
    /// 直近の ptp4l への問い合わせの失敗。読めたら `None` に戻る
    pub status_error: Option<String>,
    pub last_success: Option<PcTime>,
    pub next_poll: PcTime,
}

impl PhcSnapshot {
    #[must_use]
    pub fn capture(source: &PhcSource) -> Self {
        Self {
            state: source.state(),
            stats: source.stats(),
            poll_interval: source.poll_interval(),
            anchor: source.anchor(),
            last_sample: source.last_sample(),
            selected_sample: source.selected_sample(),
            frequency: source.frequency(),
            status: source.status().cloned(),
            status_error: source.status_error().map(str::to_owned),
            last_success: source.last_success(),
            next_poll: source.next_poll(),
        }
    }
}

/// 受信機の監視（[`crate::usecase::ReceiverMonitor`]）の状態。問い合わせのたびに TUI へ渡す。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiverSnapshot {
    /// 使っている MIB（判定する前と、判定できなかったときは `None`）
    pub profile: Option<MibProfile>,
    /// 最後に取れた受信機の状態。直近の問い合わせが失敗しても残るので、表示側は `last_error` を見て、
    /// 今の値として出さないこと
    pub status: Option<ReceiverStatus>,
    /// 最後に取れた時刻（モノトニック時計）
    pub last_success: Option<PcTime>,
    /// 直近の問い合わせの失敗。成功したら `None` に戻る
    pub last_error: Option<MonitorError>,
    pub attempts: u64,
    pub successes: u64,
}

impl SyncSnapshot {
    #[must_use]
    pub fn capture(source: &TimeSource) -> Self {
        Self {
            state: source.state(),
            stats: source.stats(),
            poll_interval: source.poll_interval(),
            anchor: source.anchor(),
            last_sample: source.last_sample(),
            selected_sample: source.selected_sample(),
            frequency: source.frequency(),
            server_info: source.server_info(),
            last_success: source.last_success(),
            next_poll: source.deadlines().next_poll,
            awaiting_reply: source.is_awaiting_reply(),
        }
    }
}
