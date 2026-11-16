//! 集約ルート [`TimeSource`]（時刻源 = サーバ 1 台）。

use std::ops::RangeInclusive;

use super::{
    Anchor, ClockFilter, KissCode, LeapIndicator, ReferenceId, RejectReason, Sample, ServerInfo,
    ServerReply, Stratum, SyncEvent, SyncState,
};
use crate::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};

// このコンテキスト独自の定義（packet_crafting には依存しない）
const MODE_SERVER: u8 = 4;
const SUPPORTED_VERSIONS: RangeInclusive<u8> = 3..=4;
const LEAP_UNSYNCHRONIZED: u8 = 3;
const STRATUM_KISS_OF_DEATH: u8 = 0;
const STRATUM_UNSYNCHRONIZED: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub poll_interval: TimeDelta,
    /// KoD RATE で間隔を延ばすときの上限
    pub max_poll_interval: TimeDelta,
    /// 応答を待つ上限
    pub response_timeout: TimeDelta,
    /// これだけ有効な応答がなければ自走に移る
    pub stale_after: TimeDelta,
    /// クロックフィルタが保持する標本の数
    pub filter_len: usize,
}

impl SourceConfig {
    /// ポーリング間隔から、ほかの値を妥当な既定値で決める。
    pub fn with_poll_interval(poll_interval: TimeDelta) -> Self {
        let poll_interval = poll_interval.max(TimeDelta::from_millis(10));
        Self {
            poll_interval,
            max_poll_interval: poll_interval.max(TimeDelta::from_secs(64)),
            response_timeout: poll_interval.min(TimeDelta::from_secs(1)),
            stale_after: (poll_interval * 4).max(TimeDelta::from_secs(2)),
            filter_len: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExchangeStats {
    pub attempts: u64,
    pub successes: u64,
    pub timeouts: u64,
    /// 送受信のエラー（到達不能など）
    pub failures: u64,
    /// 自分の問い合わせへの応答だったが、内容が不正で採用しなかった回数
    pub rejections: u64,
    /// 自分の問い合わせと関係のない受信（遅れて届いた応答・壊れたパケットなど）
    pub strays: u64,
}

/// 次に何かが起きる時刻の一覧（application 層が待ち時間を決めるのに使う）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    /// 次に問い合わせる時刻（`None` = もう問い合わせない）
    pub next_poll: Option<PcTime>,
    pub response: Option<PcTime>,
    /// 自走に移る時刻
    pub stale: Option<PcTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingExchange {
    seq: u64,
    nonce: NtpTimestamp,
    sent: LocalStamp,
    deadline: PcTime,
}

/// 時刻はすべて引数で受け取り（モノトニック時計）、起きたことを [`SyncEvent`] で返す。
#[derive(Clone, Debug)]
pub struct TimeSource {
    config: SourceConfig,
    poll_interval: TimeDelta,
    state: SyncState,
    next_poll: Option<PcTime>,
    pending: Option<PendingExchange>,
    filter: ClockFilter,
    anchor: Option<Anchor>,
    last_sample: Option<Sample>,
    server_info: Option<ServerInfo>,
    last_success: Option<PcTime>,
    stats: ExchangeStats,
    next_seq: u64,
}

impl TimeSource {
    /// `now` の時点ですぐに最初の問い合わせを行う状態で作る。
    pub fn new(config: SourceConfig, now: PcTime) -> Self {
        Self {
            config,
            poll_interval: config.poll_interval,
            state: SyncState::Unsynchronized,
            next_poll: Some(now),
            pending: None,
            filter: ClockFilter::new(config.filter_len),
            anchor: None,
            last_sample: None,
            server_info: None,
            last_success: None,
            stats: ExchangeStats::default(),
            next_seq: 1,
        }
    }

    // ---- 参照 ----

    pub fn config(&self) -> &SourceConfig {
        &self.config
    }

    pub fn state(&self) -> SyncState {
        self.state
    }

    pub fn stats(&self) -> ExchangeStats {
        self.stats
    }

    /// 現在のポーリング間隔（KoD RATE で延びることがある）。
    pub fn poll_interval(&self) -> TimeDelta {
        self.poll_interval
    }

    /// 大きい時計の表示に使う基準点（クロックフィルタが選んだ標本から作る）。
    pub fn anchor(&self) -> Option<Anchor> {
        self.anchor
    }

    pub fn last_sample(&self) -> Option<Sample> {
        self.last_sample
    }

    /// クロックフィルタが選んだ標本（直近の中で往復遅延が最小）。
    pub fn selected_sample(&self) -> Option<Sample> {
        self.filter.best().copied()
    }

    pub fn server_info(&self) -> Option<ServerInfo> {
        self.server_info
    }

    pub fn last_success(&self) -> Option<PcTime> {
        self.last_success
    }

    pub fn is_awaiting_reply(&self) -> bool {
        self.pending.is_some()
    }

    pub fn deadlines(&self) -> Deadlines {
        Deadlines {
            next_poll: self.next_poll,
            response: self.pending.map(|pending| pending.deadline),
            stale: match (self.state, self.last_success) {
                (SyncState::Synchronized, Some(last)) => {
                    Some(last.saturating_add(self.config.stale_after))
                }
                _ => None,
            },
        }
    }

    pub fn is_poll_due(&self, now: PcTime) -> bool {
        self.next_poll
            .is_some_and(|at| now.is_at_or_after(at).unwrap_or(false))
    }

    // ---- コマンド ----

    /// 問い合わせを送った（`nonce` を Transmit Timestamp に入れて、`sent` の時刻に送った）。
    ///
    /// 前の問い合わせがまだ応答待ちなら、それはタイムアウトとして数える。
    pub fn begin_exchange(&mut self, nonce: NtpTimestamp, sent: LocalStamp) -> Vec<SyncEvent> {
        let mut events = Vec::new();
        if let Some(previous) = self.pending.take() {
            self.stats.timeouts += 1;
            events.push(SyncEvent::ExchangeTimedOut { seq: previous.seq });
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.stats.attempts += 1;
        self.pending = Some(PendingExchange {
            seq,
            nonce,
            sent,
            deadline: sent.mono.saturating_add(self.config.response_timeout),
        });
        if self.state != SyncState::Denied {
            self.next_poll = Some(sent.mono.saturating_add(self.poll_interval));
        }
        events.push(SyncEvent::ExchangeStarted { seq });
        events
    }

    /// 応答を受け取った（`received` の時刻に）。検証して採用するか決める。
    pub fn complete_exchange(
        &mut self,
        reply: ServerReply,
        received: LocalStamp,
    ) -> Vec<SyncEvent> {
        let Some(pending) = self.pending else {
            return self.stray(RejectReason::NoPendingExchange);
        };
        if reply.origin != pending.nonce {
            // 自分の問い合わせへの応答ではない。応答待ちはそのまま続ける
            return self.stray(RejectReason::OriginMismatch);
        }
        self.pending = None;
        match validate(&reply, &pending, received) {
            Ok((sample, server_info)) => self.accept(pending.seq, sample, server_info),
            Err(reason) => self.reject(&pending, reason),
        }
    }

    /// 受信したが NTP パケットとして解析できなかった。
    pub fn reject_unparsable(&mut self) -> Vec<SyncEvent> {
        self.stray(RejectReason::Malformed)
    }

    /// 送受信でエラーが起きた。応答待ちの問い合わせは失敗として扱う。
    pub fn fail_exchange(&mut self, detail: String) -> Vec<SyncEvent> {
        let seq = self.pending.take().map(|pending| {
            self.stats.failures += 1;
            pending.seq
        });
        vec![SyncEvent::ExchangeFailed { seq, detail }]
    }

    /// 時間が経った。応答待ちのタイムアウトと、自走への移行を判定する。
    pub fn on_tick(&mut self, now: PcTime) -> Vec<SyncEvent> {
        let mut events = Vec::new();
        if let Some(pending) = self.pending
            && now.is_at_or_after(pending.deadline).unwrap_or(false)
        {
            self.pending = None;
            self.stats.timeouts += 1;
            events.push(SyncEvent::ExchangeTimedOut { seq: pending.seq });
        }
        if self.state == SyncState::Synchronized
            && let Some(last) = self.last_success
            && now
                .since(last)
                .is_ok_and(|elapsed| elapsed >= self.config.stale_after)
        {
            self.transition(SyncState::Holdover, &mut events);
        }
        events
    }

    // ---- 内部 ----

    fn accept(&mut self, seq: u64, sample: Sample, server_info: ServerInfo) -> Vec<SyncEvent> {
        let mut events = Vec::new();
        self.stats.successes += 1;
        self.filter.push(sample);
        if let Some(Ok(anchor)) = self.filter.best().map(Anchor::from_sample) {
            self.anchor = Some(anchor);
        }
        self.last_sample = Some(sample);
        self.server_info = Some(server_info);
        self.last_success = Some(sample.received().mono);
        events.push(SyncEvent::SampleAccepted {
            seq,
            sample,
            server_info,
        });
        self.transition(SyncState::Synchronized, &mut events);
        events
    }

    fn reject(&mut self, pending: &PendingExchange, reason: RejectReason) -> Vec<SyncEvent> {
        self.stats.rejections += 1;
        let mut events = vec![SyncEvent::SampleRejected {
            seq: Some(pending.seq),
            reason,
        }];
        if let RejectReason::KissOfDeath(code) = reason {
            match code {
                KissCode::RateExceeded => {
                    let from = self.poll_interval;
                    let to = (from * 2).min(self.config.max_poll_interval);
                    if to != from {
                        self.poll_interval = to;
                        self.next_poll = Some(pending.sent.mono.saturating_add(to));
                        events.push(SyncEvent::PollIntervalChanged { from, to });
                    }
                }
                KissCode::Deny | KissCode::Restrict => {
                    self.next_poll = None;
                    self.transition(SyncState::Denied, &mut events);
                }
                KissCode::Other(_) => {}
            }
        }
        events
    }

    fn stray(&mut self, reason: RejectReason) -> Vec<SyncEvent> {
        self.stats.strays += 1;
        vec![SyncEvent::SampleRejected { seq: None, reason }]
    }

    fn transition(&mut self, to: SyncState, events: &mut Vec<SyncEvent>) {
        if self.state != to {
            events.push(SyncEvent::StateChanged {
                from: self.state,
                to,
            });
            self.state = to;
        }
    }
}

/// 自分の問い合わせへの応答（Originate 一致済み）を検証する。
fn validate(
    reply: &ServerReply,
    pending: &PendingExchange,
    received: LocalStamp,
) -> Result<(Sample, ServerInfo), RejectReason> {
    if reply.mode != MODE_SERVER {
        return Err(RejectReason::UnexpectedMode(reply.mode));
    }
    if !SUPPORTED_VERSIONS.contains(&reply.version) {
        return Err(RejectReason::UnsupportedVersion(reply.version));
    }
    if reply.stratum == STRATUM_KISS_OF_DEATH {
        return Err(RejectReason::KissOfDeath(KissCode::from_bytes(
            reply.reference_id,
        )));
    }
    if reply.stratum > STRATUM_UNSYNCHRONIZED {
        return Err(RejectReason::InvalidStratum(reply.stratum));
    }
    if reply.leap == LEAP_UNSYNCHRONIZED || reply.stratum == STRATUM_UNSYNCHRONIZED {
        return Err(RejectReason::ServerUnsynchronized);
    }
    if reply.transmit.is_zero() {
        return Err(RejectReason::ZeroTransmitTimestamp);
    }
    let sample = Sample::new(pending.sent, reply.receive, reply.transmit, received)
        .map_err(RejectReason::InvalidSample)?;
    let stratum = Stratum::new(reply.stratum);
    let server_info = ServerInfo {
        leap: LeapIndicator::from_bits(reply.leap),
        version: reply.version,
        stratum,
        poll: reply.poll,
        precision: reply.precision,
        root_delay: TimeDelta::from_ntp_short(reply.root_delay),
        root_dispersion: TimeDelta::from_ntp_short(reply.root_dispersion),
        reference_id: ReferenceId::interpret(stratum, reply.reference_id),
        reference_time: reply.reference_time,
    };
    Ok((sample, server_info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_sync::SampleError;

    const WALL_BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);

    /// PC の時刻（モノトニック時計と壁時計は同じ速さで進むものとする）。
    fn stamp(micros: i64) -> LocalStamp {
        let delta = TimeDelta::from_micros(micros);
        LocalStamp {
            mono: PcTime::monotonic(delta.as_nanos() as u64),
            wall: WALL_BASE.wrapping_add(delta),
        }
    }

    fn nonce(n: u64) -> NtpTimestamp {
        NtpTimestamp::from_bits(n)
    }

    fn source() -> TimeSource {
        TimeSource::new(
            SourceConfig::with_poll_interval(TimeDelta::from_millis(500)),
            stamp(0).mono,
        )
    }

    /// サーバが PC より `offset_us` 進んでいて、片道 `one_way_us` かかる場合の正常な応答。
    fn reply(nonce: NtpTimestamp, sent_us: i64, offset_us: i64, one_way_us: i64) -> ServerReply {
        let server_time =
            WALL_BASE.wrapping_add(TimeDelta::from_micros(sent_us + one_way_us + offset_us));
        ServerReply {
            leap: 0,
            version: 4,
            mode: 4,
            stratum: 1,
            poll: 0,
            precision: -20,
            root_delay: 0,
            root_dispersion: 0,
            reference_id: *b"GNSS",
            reference_time: server_time,
            origin: nonce,
            receive: server_time,
            transmit: server_time,
        }
    }

    fn assert_close(actual: TimeDelta, expected: TimeDelta) {
        let diff = (actual - expected).abs();
        assert!(
            diff <= TimeDelta::from_nanos(10),
            "actual {actual:?}, expected {expected:?}"
        );
    }

    fn rejected_reason(events: &[SyncEvent]) -> RejectReason {
        match events.first() {
            Some(SyncEvent::SampleRejected { reason, .. }) => *reason,
            other => panic!("SampleRejected ではありません: {other:?}"),
        }
    }

    #[test]
    fn accepts_matching_reply_and_synchronizes() {
        let mut src = source();
        assert!(src.is_poll_due(stamp(0).mono));
        assert_eq!(
            src.begin_exchange(nonce(1), stamp(0)),
            vec![SyncEvent::ExchangeStarted { seq: 1 }]
        );
        assert!(!src.is_poll_due(stamp(499_999).mono));
        assert!(src.is_poll_due(stamp(500_000).mono));

        let events = src.complete_exchange(reply(nonce(1), 0, 5_000, 1_000), stamp(2_000));
        assert!(matches!(
            events[0],
            SyncEvent::SampleAccepted { seq: 1, .. }
        ));
        assert_eq!(
            events[1],
            SyncEvent::StateChanged {
                from: SyncState::Unsynchronized,
                to: SyncState::Synchronized,
            }
        );
        let sample = src.last_sample().unwrap();
        assert_close(sample.offset().delta(), TimeDelta::from_millis(5));
        assert_close(sample.delay().delta(), TimeDelta::from_millis(2));
        assert_eq!(src.server_info().unwrap().reference_id.to_string(), "GNSS");
        assert_eq!(src.stats().attempts, 1);
        assert_eq!(src.stats().successes, 1);
        assert!(!src.is_awaiting_reply());

        // 基準点から 1 秒後のサーバ時刻 = その瞬間の PC の壁時計 + 5ms
        let later = stamp(1_002_000);
        let projected = src.anchor().unwrap().project(later.mono).unwrap();
        assert_close(
            projected.wrapping_since(later.wall),
            TimeDelta::from_millis(5),
        );
    }

    #[test]
    fn ignores_reply_with_wrong_origin_and_keeps_waiting() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        let events = src.complete_exchange(reply(nonce(99), 0, 0, 100), stamp(200));
        assert_eq!(
            events,
            vec![SyncEvent::SampleRejected {
                seq: None,
                reason: RejectReason::OriginMismatch,
            }]
        );
        assert!(src.is_awaiting_reply());
        assert_eq!(src.stats().strays, 1);

        let events = src.complete_exchange(reply(nonce(1), 0, 0, 100), stamp(300));
        assert!(matches!(
            events[0],
            SyncEvent::SampleAccepted { seq: 1, .. }
        ));
    }

    #[test]
    fn rejects_reply_without_pending_exchange() {
        let mut src = source();
        let events = src.complete_exchange(reply(nonce(1), 0, 0, 100), stamp(200));
        assert_eq!(rejected_reason(&events), RejectReason::NoPendingExchange);
        assert_eq!(src.stats().strays, 1);
    }

    /// 正常な応答の一部を壊す操作。
    type Corruption = fn(&mut ServerReply);

    #[test]
    fn validates_reply_fields() {
        let cases: Vec<(Corruption, RejectReason)> = vec![
            (|r| r.mode = 3, RejectReason::UnexpectedMode(3)),
            (|r| r.version = 2, RejectReason::UnsupportedVersion(2)),
            (|r| r.leap = 3, RejectReason::ServerUnsynchronized),
            (|r| r.stratum = 16, RejectReason::ServerUnsynchronized),
            (|r| r.stratum = 17, RejectReason::InvalidStratum(17)),
            (
                |r| r.transmit = NtpTimestamp::ZERO,
                RejectReason::ZeroTransmitTimestamp,
            ),
            (
                |r| r.transmit = r.receive.wrapping_add(TimeDelta::from_micros(-1)),
                RejectReason::InvalidSample(SampleError::ServerTimeReversed),
            ),
        ];
        for (mutate, expected) in cases {
            let mut src = source();
            src.begin_exchange(nonce(1), stamp(0));
            let mut bad = reply(nonce(1), 0, 0, 100);
            mutate(&mut bad);
            let events = src.complete_exchange(bad, stamp(200));
            assert_eq!(
                events,
                vec![SyncEvent::SampleRejected {
                    seq: Some(1),
                    reason: expected,
                }]
            );
            assert_eq!(src.stats().rejections, 1);
            assert_eq!(src.state(), SyncState::Unsynchronized);
        }
    }

    #[test]
    fn kiss_of_death_rate_doubles_poll_interval() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        let mut kod = reply(nonce(1), 0, 0, 100);
        kod.stratum = 0;
        kod.reference_id = *b"RATE";
        let events = src.complete_exchange(kod, stamp(200));
        assert_eq!(
            rejected_reason(&events),
            RejectReason::KissOfDeath(KissCode::RateExceeded)
        );
        assert_eq!(
            events[1],
            SyncEvent::PollIntervalChanged {
                from: TimeDelta::from_millis(500),
                to: TimeDelta::from_secs(1),
            }
        );
        assert!(!src.is_poll_due(stamp(999_999).mono));
        assert!(src.is_poll_due(stamp(1_000_000).mono));
    }

    #[test]
    fn kiss_of_death_deny_stops_polling() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        let mut kod = reply(nonce(1), 0, 0, 100);
        kod.stratum = 0;
        kod.reference_id = *b"DENY";
        let events = src.complete_exchange(kod, stamp(200));
        assert!(events.contains(&SyncEvent::StateChanged {
            from: SyncState::Unsynchronized,
            to: SyncState::Denied,
        }));
        assert_eq!(src.state(), SyncState::Denied);
        assert!(!src.is_poll_due(stamp(3_600_000_000).mono));
        assert_eq!(src.deadlines().next_poll, None);
    }

    #[test]
    fn times_out_pending_exchange_on_tick() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        assert!(src.on_tick(stamp(499_999).mono).is_empty());
        assert_eq!(
            src.on_tick(stamp(500_000).mono),
            vec![SyncEvent::ExchangeTimedOut { seq: 1 }]
        );
        assert_eq!(src.stats().timeouts, 1);
        assert!(!src.is_awaiting_reply());
    }

    #[test]
    fn new_poll_while_pending_counts_previous_as_timeout() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        assert_eq!(
            src.begin_exchange(nonce(2), stamp(500_000)),
            vec![
                SyncEvent::ExchangeTimedOut { seq: 1 },
                SyncEvent::ExchangeStarted { seq: 2 },
            ]
        );
        // 遅れて届いた 1 回目の応答は無関係なパケットとして扱う
        let events = src.complete_exchange(reply(nonce(1), 0, 0, 100), stamp(500_100));
        assert_eq!(rejected_reason(&events), RejectReason::OriginMismatch);
        assert_eq!(src.stats().attempts, 2);
        assert_eq!(src.stats().timeouts, 1);
    }

    #[test]
    fn transport_failure_ends_pending_exchange() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        assert_eq!(
            src.fail_exchange("到達不能".into()),
            vec![SyncEvent::ExchangeFailed {
                seq: Some(1),
                detail: "到達不能".into(),
            }]
        );
        assert_eq!(src.stats().failures, 1);
        assert!(!src.is_awaiting_reply());
    }

    #[test]
    fn enters_holdover_when_replies_stop() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        src.complete_exchange(reply(nonce(1), 0, 0, 100), stamp(200));
        assert_eq!(src.state(), SyncState::Synchronized);

        let stale_at = stamp(200 + 2_000_000).mono;
        assert_eq!(src.deadlines().stale, Some(stale_at));
        assert!(src.on_tick(stamp(200 + 1_999_999).mono).is_empty());
        assert_eq!(
            src.on_tick(stale_at),
            vec![SyncEvent::StateChanged {
                from: SyncState::Synchronized,
                to: SyncState::Holdover,
            }]
        );
        // 基準点は残る（自走中も時計は進め続けられる）
        assert!(src.anchor().is_some());
    }

    #[test]
    fn anchor_follows_minimum_delay_sample() {
        let mut src = source();
        src.begin_exchange(nonce(1), stamp(0));
        src.complete_exchange(reply(nonce(1), 0, 0, 1_000), stamp(2_000)); // delay 2ms
        src.begin_exchange(nonce(2), stamp(500_000));
        src.complete_exchange(reply(nonce(2), 500_000, 0, 2_000), stamp(504_000)); // delay 4ms
        assert_close(
            src.selected_sample().unwrap().delay().delta(),
            TimeDelta::from_millis(2),
        );
        assert_eq!(src.anchor().unwrap().mono(), stamp(2_000).mono);
        assert_close(
            src.last_sample().unwrap().delay().delta(),
            TimeDelta::from_millis(4),
        );
    }
}
