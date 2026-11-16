//! 読み取り用モデル: 表示側（CLI / TUI）が集約を直接触らずに済むようにする。

use domain::shared_kernel::{PcTime, TimeDelta};
use domain::time_sync::{Anchor, ExchangeStats, Sample, ServerInfo, SyncState, TimeSource};

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

impl SyncSnapshot {
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
