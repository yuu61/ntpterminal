//! `SyncService` / `query_once` の結合テスト。
//!
//! 偽の時計（仮想時間）と、遅延・offset・パケットロスを設定できる模擬ネットワークを使う。
//! 実際の時間は待たない。

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use application::port::{ClockPort, EventSink, Inbound, NonceSource, PacketPort, TransportError};
use application::snapshot::SyncSnapshot;
use application::usecase::{OnceError, SyncService, query_once};
use domain::packet_crafting::{NtpMessage, RequestTemplate, decode, encode};
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};
use domain::time_sync::{FrequencyOffset, SourceConfig, SyncEvent, SyncState};

const WALL_BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);

/// 仮想時間の時計。PC の壁時計はモノトニック時計と同じ速さで進む。
#[derive(Default)]
struct FakeClock {
    mono_nanos: AtomicU64,
}

impl FakeClock {
    fn advance(&self, duration: Duration) {
        let _previous = self.mono_nanos.fetch_add(
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
    }

    fn advance_to(&self, nanos: u64) {
        let _previous = self.mono_nanos.fetch_max(nanos, Ordering::SeqCst);
    }
}

impl ClockPort for FakeClock {
    fn now(&self) -> LocalStamp {
        let nanos = self.mono_nanos.load(Ordering::SeqCst);
        LocalStamp {
            mono: PcTime::monotonic(nanos),
            wall: WALL_BASE.wrapping_add(TimeDelta::from_nanos(nanos.cast_signed())),
        }
    }
}

/// 模擬ネットワーク + 模擬サーバ。
struct SimPort {
    clock: Arc<FakeClock>,
    server_offset: TimeDelta,
    /// サーバの時計が PC のモノトニック時計より速く進む割合
    server_rate: FrequencyOffset,
    one_way: Duration,
    /// n 回目（1 始まり）の要求を落とすかどうか
    drop_request: fn(u64) -> bool,
    /// n 回目（1 始まり）の要求のときに、サーバの時刻を `server_offset` からさらにずらす量（時刻の段差）
    step: fn(u64) -> TimeDelta,
    sent: u64,
    in_flight: VecDeque<(u64, Vec<u8>)>,
}

impl SimPort {
    fn new(clock: Arc<FakeClock>, server_offset: TimeDelta, one_way: Duration) -> Self {
        Self {
            clock,
            server_offset,
            server_rate: FrequencyOffset::ZERO,
            one_way,
            drop_request: |_| false,
            step: |_| TimeDelta::ZERO,
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
        let request = decode(payload).map_err(|error| TransportError::Io(error.to_string()))?;
        // サーバが要求を受け取った瞬間のサーバ時刻（PC の時刻 + offset + 進み方の差の累積）
        let at_server = TimeDelta::from_nanos(sent.mono.nanos().cast_signed())
            + TimeDelta::from_std(self.one_way);
        let server_time = sent.wall.wrapping_add(
            TimeDelta::from_std(self.one_way)
                + self.server_offset
                + (self.step)(self.sent)
                + self.server_rate.drift_over(at_server),
        );
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
        let one_way = u64::try_from(self.one_way.as_nanos()).unwrap_or(u64::MAX);
        let arrival = sent.mono.nanos() + 2 * one_way;
        self.in_flight.push_back((arrival, encode(&reply)));
        Ok(sent)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        let now = self.clock.now().mono.nanos();
        let deadline = now + u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        if self
            .in_flight
            .front()
            .is_some_and(|&(arrival, _)| arrival <= deadline)
            && let Some((arrived_at, payload)) = self.in_flight.pop_front()
        {
            self.clock.advance_to(arrived_at);
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
        diff < 1e-5f64,
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
    let mut service = service(port, Arc::clone(&clock));
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
        Arc::<FakeClock>::clone(&clock),
        RequestTemplate::default(),
        Duration::from_millis(300),
    );
    assert_eq!(result, Err(OnceError::Timeout));
    // 仮想時間で 300ms 待ったところで諦めている
    assert_eq!(clock.now().mono, PcTime::monotonic(300_000_000));
}

/// 一定数の標本を受け取ったら停止フラグを立てる通知先。
struct StopAfter {
    accepted: u64,
    limit: u64,
    stop: Arc<AtomicBool>,
}

impl EventSink for StopAfter {
    fn publish(&mut self, event: &SyncEvent, _snapshot: &SyncSnapshot) {
        if matches!(event, SyncEvent::SampleAccepted { .. }) {
            self.accepted += 1;
            if self.accepted >= self.limit {
                self.stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

#[test]
fn run_until_returns_when_stop_flag_is_set() {
    let clock = Arc::new(FakeClock::default());
    let port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::ZERO,
        Duration::from_micros(200),
    );
    let mut service = service(port, clock);
    let stop = Arc::new(AtomicBool::new(false));
    let mut sink = StopAfter {
        accepted: 0,
        limit: 3,
        stop: Arc::clone(&stop),
    };
    // 回数無制限でも、停止フラグで戻ってくる
    service.run_until(&mut sink, None, &stop);
    assert_eq!(service.snapshot().stats.successes, 3);
}

/// サーバの時計が PC のモノトニック時計より 35 ppm 速い。応答が止まった後も、推定した周波数で時刻を進める。
#[test]
fn holdover_corrects_for_the_frequency_offset() {
    let rate = FrequencyOffset::from_ppb(35_000);
    let clock = Arc::new(FakeClock::default());
    let mut port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::ZERO,
        Duration::from_micros(200),
    );
    port.server_rate = rate;
    // 2 分（500ms × 240 回）で応答が止まる
    port.drop_request = |n| n > 240;
    let mut service = service(port, Arc::clone(&clock));
    let mut recorder = Recorder::default();
    service.run(&mut recorder, Some(250));

    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, SyncState::Holdover);
    let ppm = snapshot.frequency.unwrap().as_ppm_f64();
    assert!((ppm - 35.0).abs() < 0.2f64, "推定値 {ppm} ppm");
    assert!(
        !recorder
            .events
            .iter()
            .any(|event| matches!(event, SyncEvent::ClockStepDetected { .. })),
        "進み方の差を時刻の段差と取り違えた"
    );

    // 応答が止まってから 10 分後。補正しなければ 35 ppm × 600 秒 = 21ms ずれる
    let later = TimeDelta::from_secs(720);
    let truth = WALL_BASE.wrapping_add(later + rate.drift_over(later));
    let projected = snapshot
        .anchor
        .unwrap()
        .project(PcTime::monotonic(later.as_nanos().cast_unsigned()))
        .unwrap();
    let error = projected.wrapping_since(truth);
    assert!(
        error.abs() < TimeDelta::from_micros(200),
        "自走 10 分後の誤差 {error:?}"
    );
}

/// サーバの時刻が途中で 300ms 戻ったら、2 回の応答で確かめてから新しい時刻に合わせる。
#[test]
fn follows_a_step_of_the_server_time() {
    let clock = Arc::new(FakeClock::default());
    let mut port = SimPort::new(
        Arc::clone(&clock),
        TimeDelta::ZERO,
        Duration::from_micros(200),
    );
    port.step = |n| {
        if n > 6 {
            TimeDelta::from_millis(-300)
        } else {
            TimeDelta::ZERO
        }
    };
    let mut service = service(port, Arc::clone(&clock));
    let mut recorder = Recorder::default();
    service.run(&mut recorder, Some(10));

    // 7 回目（段差の後の 1 回目）は保留し、8 回目で段差とみなす。段差の検出は 1 回だけ
    let steps: Vec<(usize, TimeDelta)> = recorder
        .events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            if let SyncEvent::ClockStepDetected { step } = *event {
                Some((index, step))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(steps.len(), 1, "{:?}", recorder.events);
    let (step_at, step) = steps[0];
    assert_eq!(
        recorder.events[step_at - 1],
        recorder
            .events
            .iter()
            .find(|event| matches!(event, SyncEvent::SampleAccepted { seq: 8, .. }))
            .unwrap()
            .clone(),
        "8 回目の応答で段差とみなす"
    );
    assert_close_ms(step, -300.0);

    let snapshot = service.snapshot();
    let now = clock.now();
    let projected = snapshot.anchor.unwrap().project(now.mono).unwrap();
    assert_close_ms(projected.wrapping_since(now.wall), -300.0);
    assert_close_ms(snapshot.selected_sample.unwrap().offset().delta(), -300.0);
}
