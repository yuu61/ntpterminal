//! 同期スレッド → UI スレッドへスナップショットを送る通知先。

use std::sync::mpsc::{SendError, Sender};

use application::port::EventSink;
use application::snapshot::SyncSnapshot;
use domain::time_sync::SyncEvent;

/// イベントが起きるたびに最新のスナップショットをチャネルへ送る。
#[derive(Debug)]
pub struct SnapshotSender {
    tx: Sender<SyncSnapshot>,
}

impl SnapshotSender {
    #[must_use]
    pub const fn new(tx: Sender<SyncSnapshot>) -> Self {
        Self { tx }
    }
}

impl EventSink for SnapshotSender {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "受け手（UI）が終了した後は送れないが、同期スレッドもまもなく止まるので捨ててよい"
    )]
    fn publish(&mut self, _event: &SyncEvent, snapshot: &SyncSnapshot) {
        let _: Result<(), SendError<SyncSnapshot>> = self.tx.send(*snapshot);
    }
}
