//! 同期スレッド → UI スレッドへスナップショットを送る通知先。

use std::sync::mpsc::Sender;

use application::port::{EventSink, MasterEventSink, PhcEventSink};
use application::snapshot::{MasterSnapshot, PhcSnapshot, SyncSnapshot};
use domain::ptp_clock::{MasterEvent, PhcEvent};
use domain::time_sync::SyncEvent;

use super::SourceSnapshot;

/// NTP の同期スレッド: イベントが起きるたびに最新のスナップショットをチャネルへ送る。
#[derive(Debug)]
pub struct SnapshotSender {
    tx: Sender<SourceSnapshot>,
}

impl SnapshotSender {
    #[must_use]
    pub const fn new(tx: Sender<SourceSnapshot>) -> Self {
        Self { tx }
    }
}

/// 受け手（UI）が終了した後は送れないが、同期スレッドもまもなく止まるので、送れなくても捨ててよい。
impl EventSink for SnapshotSender {
    fn publish(&mut self, _event: &SyncEvent, snapshot: &SyncSnapshot) {
        drop(self.tx.send(SourceSnapshot::Ntp(Box::new(*snapshot))));
    }
}

/// PHC の同期スレッド: イベントが起きるたびに最新のスナップショットをチャネルへ送る。
#[derive(Debug)]
pub struct PhcSnapshotSender {
    tx: Sender<SourceSnapshot>,
}

impl PhcSnapshotSender {
    #[must_use]
    pub const fn new(tx: Sender<SourceSnapshot>) -> Self {
        Self { tx }
    }
}

/// 送れなくても捨ててよい（[`SnapshotSender`] と同じ）。
impl PhcEventSink for PhcSnapshotSender {
    fn publish(&mut self, _event: &PhcEvent, snapshot: &PhcSnapshot) {
        drop(
            self.tx
                .send(SourceSnapshot::Phc(Box::new(snapshot.clone()))),
        );
    }
}

/// PTP のマスタの同期スレッド: イベントが起きるたびに最新のスナップショットをチャネルへ送る。
#[derive(Debug)]
pub struct MasterSnapshotSender {
    tx: Sender<SourceSnapshot>,
}

impl MasterSnapshotSender {
    #[must_use]
    pub const fn new(tx: Sender<SourceSnapshot>) -> Self {
        Self { tx }
    }
}

/// 送れなくても捨ててよい（[`SnapshotSender`] と同じ）。
impl MasterEventSink for MasterSnapshotSender {
    fn publish(&mut self, _event: &MasterEvent, snapshot: &MasterSnapshot) {
        drop(
            self.tx
                .send(SourceSnapshot::Ptp(Box::new(snapshot.clone()))),
        );
    }
}
