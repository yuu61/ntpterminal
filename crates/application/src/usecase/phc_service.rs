//! PHC の同期サービス: ptp4l の状態と PHC を問い合わせ続け、時刻源（集約 [`PhcSource`]）に渡す。
//!
//! - 1 回の問い合わせでは、先に ptp4l の状態を読み、次に PHC を読む。ptp4l が答えなくても PHC は読む
//!   （最後に読めた UTC との差で換算する。ptp4l の状態を一度も読めていなければ換算できないので読まない）
//! - 問い合わせの間は 100ms ごとに停止の合図を確かめる（TUI を閉じたらすぐ終わるように）

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::thread;

use domain::ptp_clock::{PhcConfig, PhcEvent, PhcSource};
use domain::shared_kernel::TimeDelta;

use super::{Ptp4lClient, Ptp4lError};
use crate::port::{ClockPort, PacketPort, PhcEventSink, PhcPort};
use crate::snapshot::PhcSnapshot;

/// 問い合わせを待つとき、停止の合図を確かめる間隔。
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PhcOnceError {
    #[error("ptp4l の状態を読めません: {0}")]
    Status(String),
    #[error("PHC を読めません: {0}")]
    Reading(String),
}

/// PHC 1 つぶんの同期処理。1 つのスレッドが所有して回す。
pub struct PhcService<P, M> {
    source: PhcSource,
    phc: P,
    ptp4l: Ptp4lClient<M>,
    /// 表示する PHC を使う ptp4l のポートの番号
    ports: Vec<u16>,
    clock: Arc<dyn ClockPort>,
}

impl<P, M> fmt::Debug for PhcService<P, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhcService")
            .field("source", &self.source)
            .field("ptp4l", &self.ptp4l)
            .field("ports", &self.ports)
            .finish_non_exhaustive()
    }
}

impl<P: PhcPort, M: PacketPort> PhcService<P, M> {
    pub fn new(
        config: PhcConfig,
        phc: P,
        ptp4l: Ptp4lClient<M>,
        ports: Vec<u16>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        let source = PhcSource::new(config, clock.now().mono);
        Self {
            source,
            phc,
            ptp4l,
            ports,
            clock,
        }
    }

    pub const fn source(&self) -> &PhcSource {
        &self.source
    }

    pub fn snapshot(&self) -> PhcSnapshot {
        PhcSnapshot::capture(&self.source)
    }

    /// 1 回問い合わせる（予定の時刻かどうかは見ない）: ptp4l の状態 → PHC。
    pub fn poll(&mut self) -> Vec<PhcEvent> {
        let seq = self.source.begin_poll(self.clock.now().mono);
        let mut events = match self.ptp4l.status(&self.ports) {
            Ok(status) => self.source.status_received(seq, status),
            // 止めるときは知らせない
            Err(Ptp4lError::Stopped) => return Vec::new(),
            Err(error) => self.source.status_failed(seq, error.to_string()),
        };
        if self.source.can_convert() {
            events.extend(match self.phc.read() {
                Ok(reading) => self.source.reading_received(seq, reading),
                Err(error) => self.source.reading_failed(seq, error.to_string()),
            });
        }
        events
    }

    /// 1 回だけ問い合わせる（`once`）。ptp4l の状態と PHC の両方を読めたときだけ成功にする。
    ///
    /// # Errors
    ///
    /// ptp4l の状態か PHC を読めなかったとき。
    pub fn poll_once(&mut self) -> Result<PhcSnapshot, PhcOnceError> {
        for event in self.poll() {
            match event {
                PhcEvent::StatusFailed { detail, .. } => return Err(PhcOnceError::Status(detail)),
                PhcEvent::ReadingFailed { detail, .. } => {
                    return Err(PhcOnceError::Reading(detail));
                }
                PhcEvent::StatusReceived { .. }
                | PhcEvent::ReadingAccepted { .. }
                | PhcEvent::ClockStepDetected { .. }
                | PhcEvent::StateChanged { .. } => {}
            }
        }
        let snapshot = self.snapshot();
        if snapshot.last_sample.is_none() {
            return Err(PhcOnceError::Status("停止しました".to_owned()));
        }
        Ok(snapshot)
    }

    /// 間隔ごとに問い合わせ、起きたことを `sink` に通知する。`stop` が立ったら終わる。
    /// `max_attempts` を指定すると、その回数だけ問い合わせて終わる。
    pub fn run_until<S>(&mut self, sink: &mut S, max_attempts: Option<u64>, stop: &Arc<AtomicBool>)
    where
        S: PhcEventSink + ?Sized,
    {
        self.ptp4l.set_stop(Arc::clone(stop));
        while !stop.load(Ordering::Relaxed) {
            if max_attempts.is_some_and(|max| self.source.stats().attempts >= max) {
                return;
            }
            let now = self.clock.now().mono;
            if !self.source.is_poll_due(now) {
                let remaining = self
                    .source
                    .next_poll()
                    .since(now)
                    .unwrap_or(TimeDelta::ZERO);
                thread::sleep(remaining.to_std_saturating().min(STOP_CHECK_INTERVAL));
                continue;
            }
            let events = self.poll();
            if stop.load(Ordering::Relaxed) {
                // 応答待ちを途中でやめた結果は知らせない
                return;
            }
            let snapshot = self.snapshot();
            for event in &events {
                sink.publish(event, &snapshot);
            }
        }
    }
}
