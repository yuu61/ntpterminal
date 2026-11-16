//! 同期サービス: 時刻源（集約）とポートをつなぎ、問い合わせを続ける。

use std::sync::Arc;
use std::time::Duration;

use domain::packet_crafting::{RequestTemplate, decode, encode};
use domain::shared_kernel::TimeDelta;
use domain::time_sync::{SourceConfig, SyncEvent, TimeSource};

use crate::acl;
use crate::port::{ClockPort, EventSink, Inbound, NonceSource, PacketPort, TransportError};
use crate::snapshot::SyncSnapshot;

/// 1 回の受信待ちの上限。何も起きなくてもこの間隔でループが回る。
const DEFAULT_MAX_WAIT: Duration = Duration::from_millis(100);

/// 時刻源 1 台ぶんの同期処理。1 つのスレッドが所有して回す。
pub struct SyncService<P, N> {
    source: TimeSource,
    port: P,
    nonce: N,
    clock: Arc<dyn ClockPort>,
    template: RequestTemplate,
}

impl<P: PacketPort, N: NonceSource> SyncService<P, N> {
    pub fn new(
        config: SourceConfig,
        port: P,
        nonce: N,
        clock: Arc<dyn ClockPort>,
        template: RequestTemplate,
    ) -> Self {
        let source = TimeSource::new(config, clock.now().mono);
        Self {
            source,
            port,
            nonce,
            clock,
            template,
        }
    }

    pub fn source(&self) -> &TimeSource {
        &self.source
    }

    pub fn snapshot(&self) -> SyncSnapshot {
        SyncSnapshot::capture(&self.source)
    }

    /// 1 ステップ進める: 期限切れの処理 → （`allow_poll` なら）送信 → 最大 `max_wait` 受信を待つ。
    pub fn step(&mut self, allow_poll: bool, max_wait: Duration) -> Vec<SyncEvent> {
        let mut events = self.source.on_tick(self.clock.now().mono);
        if allow_poll && self.source.is_poll_due(self.clock.now().mono) {
            events.extend(self.send_query());
        }
        if !allow_poll && !self.source.is_awaiting_reply() {
            // もう送らず、応答待ちもない: 待つものがない
            return events;
        }
        let wait = self.wait_time(allow_poll, max_wait);
        match self.port.recv(wait) {
            Ok(Some(inbound)) => events.extend(self.handle_inbound(&inbound)),
            Ok(None) => {}
            Err(error @ TransportError::Unreachable(_)) => {
                events.extend(self.source.fail_exchange(error.to_string()));
            }
            Err(error @ TransportError::Io(_)) => {
                events.extend(self.source.fail_exchange(error.to_string()));
                // 壊れたソケットで空回りしないよう、待つはずだった時間だけ休む
                std::thread::sleep(wait);
            }
        }
        events
    }

    /// 問い合わせを続け、起きたことを `sink` に通知する。
    /// `max_attempts` を指定すると、その回数だけ送って最後の応答（かタイムアウト）を待って終わる。
    pub fn run<S: EventSink + ?Sized>(&mut self, sink: &mut S, max_attempts: Option<u64>) {
        loop {
            let allow_poll = max_attempts.is_none_or(|max| self.source.stats().attempts < max);
            if !allow_poll && !self.source.is_awaiting_reply() {
                return;
            }
            let events = self.step(allow_poll, DEFAULT_MAX_WAIT);
            if events.is_empty() {
                continue;
            }
            let snapshot = self.snapshot();
            for event in &events {
                sink.publish(event, &snapshot);
            }
        }
    }

    fn send_query(&mut self) -> Vec<SyncEvent> {
        let nonce = self.nonce.next_nonce();
        let payload = encode(&self.template.build(nonce));
        match self.port.send(&payload) {
            Ok(sent) => self.source.begin_exchange(nonce, sent),
            Err(error) => {
                let mut events = self.source.begin_exchange(nonce, self.clock.now());
                events.extend(self.source.fail_exchange(error.to_string()));
                events
            }
        }
    }

    fn handle_inbound(&mut self, inbound: &Inbound) -> Vec<SyncEvent> {
        match decode(&inbound.payload) {
            Ok(message) => self
                .source
                .complete_exchange(acl::to_server_reply(&message), inbound.received),
            Err(_) => self.source.reject_unparsable(),
        }
    }

    /// 次に何か（送信・応答の期限・自走への移行）が起きるまで待つ時間。
    fn wait_time(&self, allow_poll: bool, max_wait: Duration) -> Duration {
        let now = self.clock.now().mono;
        let deadlines = self.source.deadlines();
        [
            deadlines.next_poll.filter(|_| allow_poll),
            deadlines.response,
            deadlines.stale,
        ]
        .into_iter()
        .flatten()
        .filter_map(|at| at.since(now).ok())
        .map(TimeDelta::to_std_saturating)
        .fold(max_wait, Duration::min)
    }
}
