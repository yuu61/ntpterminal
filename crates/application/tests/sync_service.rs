//! SyncService / query_once の結合テスト。
//!
//! 偽の時計（仮想時間）と、遅延・offset・パケットロスを設定できる模擬ネットワークを使う。
//! 実際の時間は待たない。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use application::port::{ClockPort, EventSink, Inbound, NonceSource, PacketPort, TransportError};
use application::snapshot::SyncSnapshot;
use application::usecase::{OnceError, SyncService, query_once};
use domain::packet_crafting::{NtpMessage, RequestTemplate, decode, encode};
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};
use domain::time_sync::{SourceConfig, SyncEvent, SyncState};

const WALL_BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);

/// 仮想時間の時計。PC の壁時計はモノトニック時計と同じ速さで進む。
#[derive(Default)]
struct FakeClock {
    mono_nanos: AtomicU64,
}

impl FakeClock {
    fn advance(&self, duration: Duration) {
        self.mono_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::SeqCst);
    }

    fn advance_to(&self, nanos: u64) {
        self.mono_nanos.fetch_max(nanos, Ordering::SeqCst);
    }
}

impl ClockPort for FakeClock {
    fn now(&self) -> LocalStamp {
        let nanos = self.mono_nanos.load(Ordering::SeqCst);
        LocalStamp {
            mono: PcTime::monotonic(nanos),
            wall: WALL_BASE.wrapping_add(TimeDelta::from_nanos(nanos as i64)),
        }
    }
}

/// 模擬ネットワーク + 模擬サーバ。
struct SimPort {
    clock: Arc<FakeClock>,
    server_offset: TimeDelta,
    one_way: Duration,
    /// n 回目（1 始まり）の要求を落とすかどうか
    drop_request: fn(u64) -> bool,
    sent: u64,
    in_flight: VecDeque<(u64, Vec<u8>)>,
}

impl SimPort {
    fn new(clock: Arc<FakeClock>, server_offset: TimeDelta, one_way: Duration) -> Self {
        Self {
            clock,
            server_offset,
            one_way,
            drop_request: |_| false,
            sent: 0,
            in_flight: VecDeque::new(),
        }
    }
}

impl PacketPort for SimPort {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let sent = self.clock.now();
        self.sent += 1;
        if (self.drop_request)(self.sent) {
            return Ok(sent);
        }
        let request = decode(payload).expect("要求を解析できない");
        let server_time = sent
            .wall
            .wrapping_add(TimeDelta::from_std(self.one_way) + self.server_offset);
        let reply = NtpMessage {
            version: 4,
            mode: NtpMessage::MODE_SERVER,
            stratum: 1,
            reference_id: *b"GNSS",
            origin_timestamp: request.transmit_timestamp,
            receive_timestamp: server_time,
            transmit_timestamp: server_time,
            ..NtpMessage::default()
        };
        let arrival = sent.mono.nanos() + 2 * self.one_way.as_nanos() as u64;
        self.in_flight.push_back((arrival, encode(&reply)));
        Ok(sent)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        let now = self.clock.now().mono.nanos();
        let deadline = now + timeout.as_nanos() as u64;
        if let Some(&(arrival, _)) = self.in_flight.front()
            && arrival <= deadline
        {
            let (arrival, payload) = self.in_flight.pop_front().unwrap();
            self.clock.advance_to(arrival);
            return Ok(Some(Inbound {
                payload,
                received: self.clock.now(),
            }));
        }
        self.clock.advance(timeout);
        Ok(None)
    }
}

/// 連番のノンス。
#[derive(Default)]
struct SeqNonce(u64);

impl NonceSource for SeqNonce {
    fn next_nonce(&mut self) -> NtpTimestamp {
        self.0 += 1;
        NtpTimestamp::from_bits(self.0)
    }
}

#[derive(Default)]
struct Recorder {
    events: Vec<SyncEvent>,
}

impl EventSink for Recorder {
    fn publish(&mut self, event: &SyncEvent, _snapshot: &SyncSnapshot) {
        self.events.push(event.clone());
    }
}

fn service(port: SimPort, clock: Arc<FakeClock>) -> SyncService<SimPort, SeqNonce> {
    SyncService::new(
        SourceConfig::with_poll_interval(TimeDelta::from_millis(500)),
        port,
        SeqNonce::default(),
        clock,
        RequestTemplate::default(),
    )
}

fn assert_close_ms(actual: TimeDelta, expected_ms: f64) {
    let diff = (actual.as_millis_f64() - expected_ms).abs();
    assert!(
        diff < 1e-5,
        "actual {} ms, expected {expected_ms} ms",
        actual.as_millis_f64()
    );
}

#[test]
fn polls_at_interval_and_measures_offset() {
    let clock = Arc::new(FakeClock::default());
    let port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::from_millis(5),
        Duration::from_micros(200),
    );
    let mut service = service(port, clock.clone());
    let mut recorder = Recorder::default();
    service.run(&mut recorder, Some(5));

    let snapshot = service.snapshot();
    assert_eq!(snapshot.stats.attempts, 5);
    assert_eq!(snapshot.stats.successes, 5);
    assert_eq!(snapshot.state, SyncState::Synchronized);
    let sample = snapshot.last_sample.unwrap();
    assert_close_ms(sample.offset().delta(), 5.0);
    assert_close_ms(sample.delay().delta(), 0.4);

    // 5 回目の送信は 4 × 500ms 後
    assert_eq!(sample.sent().mono, PcTime::monotonic(2_000_000_000));

    // 基準点から推定したサーバ時刻 = PC の壁時計 + 5ms
    let now = clock.now();
    let projected = snapshot.anchor.unwrap().project(now.mono).unwrap();
    assert_close_ms(projected.wrapping_since(now.wall), 5.0);
}

#[test]
fn counts_lost_packets_as_timeouts() {
    let clock = Arc::new(FakeClock::default());
    let mut port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::ZERO,
        Duration::from_micros(200),
    );
    port.drop_request = |n| n % 2 == 0;
    let mut service = service(port, clock);
    let mut recorder = Recorder::default();
    service.run(&mut recorder, Some(6));

    let stats = service.snapshot().stats;
    assert_eq!(stats.attempts, 6);
    assert_eq!(stats.successes, 3);
    assert_eq!(stats.timeouts, 3);
    assert!(
        recorder
            .events
            .contains(&SyncEvent::ExchangeTimedOut { seq: 6 })
    );
}

#[test]
fn enters_holdover_when_server_stops_answering() {
    let clock = Arc::new(FakeClock::default());
    let mut port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::ZERO,
        Duration::from_micros(200),
    );
    port.drop_request = |n| n > 2;
    let mut service = service(port, clock);
    let mut recorder = Recorder::default();
    // 2 回成功したあと応答が止まる。自走への移行（2 秒）まで回す
    service.run(&mut recorder, Some(8));

    assert!(recorder.events.contains(&SyncEvent::StateChanged {
        from: SyncState::Synchronized,
        to: SyncState::Holdover,
    }));
    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, SyncState::Holdover);
    assert!(snapshot.anchor.is_some(), "自走中も基準点は残る");
}

#[test]
fn query_once_returns_sample() {
    let clock = Arc::new(FakeClock::default());
    let port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::from_micros(-978),
        Duration::from_micros(211),
    );
    let outcome = query_once(
        port,
        SeqNonce::default(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_close_ms(outcome.sample.offset().delta(), -0.978);
    assert_close_ms(outcome.sample.delay().delta(), 0.422);
    assert_eq!(outcome.server_info.reference_id.to_string(), "GNSS");
}

#[test]
fn query_once_times_out() {
    let clock = Arc::new(FakeClock::default());
    let mut port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::ZERO,
        Duration::from_micros(200),
    );
    port.drop_request = |_| true;
    let result = query_once(
        port,
        SeqNonce::default(),
        clock.clone(),
        RequestTemplate::default(),
        Duration::from_millis(300),
    );
    assert_eq!(result, Err(OnceError::Timeout));
    // 仮想時間で 300ms 待ったところで諦めている
    assert_eq!(clock.now().mono, PcTime::monotonic(300_000_000));
}
