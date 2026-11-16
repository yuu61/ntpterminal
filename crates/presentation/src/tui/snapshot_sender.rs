//! 同期スレッド → UI スレッドへスナップショットを送る通知先。

use std::sync::mpsc::Sender;

use application::port::EventSink;
use application::snapshot::SyncSnapshot;
use domain::time_sync::SyncEvent;

/// イベントが起きるたびに最新のスナップショットをチャネルへ送る。
pub struct SnapshotSender {
    tx: Sender<SyncSnapshot>,
}

impl SnapshotSender {
    pub fn new(tx: Sender<SyncSnapshot>) -> Self {
        Self { tx }
    }
}

impl EventSink for SnapshotSender {
    fn publish(&mut self, _event: &SyncEvent, snapshot: &SyncSnapshot) {
        // 受け手（UI）が終了していたら送れないが、同期スレッドもまもなく止まるので無視する
        let _ = self.tx.send(*snapshot);
    }
}
