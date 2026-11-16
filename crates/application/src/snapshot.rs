//! 読み取り用モデル: 表示側（CLI / TUI）が集約を直接触らずに済むようにする。

use domain::receiver::ReceiverStatus;
use domain::shared_kernel::{PcTime, TimeDelta};
use domain::time_sync::{Anchor, ExchangeStats, Sample, ServerInfo, SyncState, TimeSource};

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
    /// クロックフィルタが選んだ標本（直近で往復遅延が最小）
    pub selected_sample: Option<Sample>,
    pub server_info: Option<ServerInfo>,
    pub last_success: Option<PcTime>,
    pub next_poll: Option<PcTime>,
    pub awaiting_reply: bool,
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
            server_info: source.server_info(),
            last_success: source.last_success(),
            next_poll: source.deadlines().next_poll,
            awaiting_reply: source.is_awaiting_reply(),
        }
    }
}
