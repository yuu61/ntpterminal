//! `PtpSlaveService` の結合テスト。
//!
//! 偽の時計（仮想時間）と、Announce・Sync・`Follow_Up` を決まった間隔で送り、`Delay_Req` に `Delay_Resp` で答える
//! 模擬マスタを使う。実際の時間は待たない。送られた `Delay_Req` の中身（種類・長さ・ドメイン・識別子・フラグ）と、
//! 送った回数・間隔も確かめる。

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Mutex, MutexGuard, PoisonError};

use application::port::{ClockPort, MasterEventSink, PtpInbound, PtpPort, TransportError};
use application::snapshot::MasterSnapshot;
use application::timestamping::{TimestampSource, TimestampingStatus};
use application::usecase::{PtpOnceError, PtpSlaveConfig, PtpSlaveService, own_port_identity};
use domain::ptp_clock::{EventStamp, MasterEvent, MasterState};
use domain::ptp_message::{
    Announce, Body, Correction, Flags, Header, MessageType, PortIdentity, PtpMessage, PtpTimestamp,
    decode, encode,
};
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};

const NANOS_PER_SEC: u64 = 1_000_000_000;
/// PC の壁時計の起点（2026-11-17 00:00:00 UTC の Unix 秒）
const WALL_BASE_SECS: u64 = 1_794_873_600;
const MASTER: PortIdentity = PortIdentity {
    clock: [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56],
    port: 1,
};
const MASTER_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 10, 50)), 319);

#[derive(Default)]
struct FakeClock {
    mono_nanos: AtomicU64,
}

impl FakeClock {
    fn nanos(&self) -> u64 {
        self.mono_nanos.load(Ordering::SeqCst)
    }

    fn set(&self, nanos: u64) {
        self.mono_nanos.store(nanos, Ordering::SeqCst);
    }
}

impl ClockPort for FakeClock {
    fn now(&self) -> LocalStamp {
        let nanos = self.nanos();
        LocalStamp {
            mono: PcTime::monotonic(nanos),
            wall: NtpTimestamp::from_unix_nanos(i128::from(WALL_BASE_SECS * NANOS_PER_SEC))
                .wrapping_add(TimeDelta::from_nanos(nanos.cast_signed())),
        }
    }
}

/// 模擬マスタの設定と、届いた `Delay_Req`。
struct Master {
    domain: u8,
    two_step: bool,
    /// マスタの時計が PC より進んでいる量
    ahead_nanos: u64,
    one_way_nanos: u64,
    sync_interval_nanos: u64,
    log_sync_interval: i8,
    announce_interval_nanos: u64,
    log_announce_interval: i8,
    /// `Delay_Resp` に入れる最小間隔
    log_min_delay_req: i8,
    answers_delay_req: bool,
    /// Sync（と `Follow_Up`）を送るか（偽なら Announce だけ）
    sends_sync: bool,
    /// この PC の時刻から後は何も送らない（マスタが止まった）
    silent_from: Option<u64>,
    /// 届いた `Delay_Req`（PC の時刻と中身）
    requests: Vec<(u64, PtpMessage)>,
    /// これから届くメッセージ（PC の時刻の順。同じ時刻なら入れた順）
    pending: BTreeMap<(u64, u64), (Vec<u8>, bool)>,
    /// 何時まで Announce・Sync を作ったか
    generated_until: u64,
    next_key: u64,
    sequence: u16,
}

impl Default for Master {
    fn default() -> Self {
        Self {
            domain: 0,
            two_step: true,
            ahead_nanos: 5_000_000,
            one_way_nanos: 100_000,
            sync_interval_nanos: NANOS_PER_SEC,
            log_sync_interval: 0,
            announce_interval_nanos: NANOS_PER_SEC,
            log_announce_interval: 0,
            log_min_delay_req: 0,
            answers_delay_req: true,
            sends_sync: true,
            silent_from: None,
            requests: Vec::new(),
            pending: BTreeMap::new(),
            generated_until: 0,
            next_key: 0,
            sequence: 0,
        }
    }
}

impl Master {
    /// PC の時刻 `pc_nanos` の瞬間のマスタの時計（TAI = UTC + 37 秒）。
    const fn time_at(&self, pc_nanos: u64) -> PtpTimestamp {
        let total = (WALL_BASE_SECS + 37) * NANOS_PER_SEC + pc_nanos + self.ahead_nanos;
        PtpTimestamp {
            seconds: total / NANOS_PER_SEC,
            nanos: (total % NANOS_PER_SEC) as u32,
        }
    }

    const fn header(&self, flags: Flags, sequence_id: u16, log_message_interval: i8) -> Header {
        Header {
            transport_specific: 0,
            version: Header::VERSION_2,
            domain: self.domain,
            minor_sdo_id: 0,
            flags,
            correction: Correction(0),
            source: MASTER,
            sequence_id,
            log_message_interval,
        }
    }

    fn deliver(&mut self, at: u64, message: &PtpMessage, event: bool) {
        let _old = self
            .pending
            .insert((at, self.next_key), (encode(message), event));
        self.next_key += 1;
    }

    /// PC の時刻 `now` に届いた `Delay_Req` を記録し、答えるなら `Delay_Resp` を送る。
    fn answer(&mut self, now: u64, request: PtpMessage) {
        if self.answers_delay_req && request.header.domain == self.domain {
            let received = now + self.one_way_nanos;
            let response = PtpMessage {
                header: self.header(Flags(0), request.header.sequence_id, self.log_min_delay_req),
                body: Body::DelayResp {
                    receive: self.time_at(received),
                    requesting: request.header.source,
                },
            };
            self.deliver(received + self.one_way_nanos, &response, false);
        }
        self.requests.push((now, request));
    }

    /// `until` までに届くメッセージのうち、最も早いもの（PC の時刻・バイト列・イベント用のポートか）。
    fn next_until(&mut self, until: u64) -> Option<(u64, Vec<u8>, bool)> {
        self.generate(until);
        let key = self
            .pending
            .first_key_value()
            .map(|(&key, _)| key)
            .filter(|&(at, _)| at <= until)?;
        let (payload, event) = self.pending.remove(&key)?;
        Some((key.0, payload, event))
    }

    /// 送られた `Delay_Req` の、PC の時刻。
    fn request_times(&self) -> Vec<u64> {
        self.requests.iter().map(|&(at, _)| at).collect()
    }

    /// `until` までに送る Announce と Sync（と `Follow_Up`）を作る。
    fn generate(&mut self, until: u64) {
        while self.generated_until <= until {
            let at = self.generated_until;
            if self.silent_from.is_some_and(|from| at >= from) {
                self.generated_until += 1_000_000;
                continue;
            }
            let arrive = at + self.one_way_nanos;
            if at.is_multiple_of(self.announce_interval_nanos) {
                let announce = PtpMessage {
                    header: self.header(
                        Flags::PTP_TIMESCALE.with(Flags::UTC_OFFSET_VALID),
                        self.sequence,
                        self.log_announce_interval,
                    ),
                    body: Body::Announce(Announce {
                        origin: self.time_at(at),
                        current_utc_offset: 37,
                        grandmaster_priority1: 128,
                        grandmaster_class: 6,
                        grandmaster_accuracy: 0x21,
                        grandmaster_variance: 0x4E5D,
                        grandmaster_priority2: 128,
                        grandmaster_identity: MASTER.clock,
                        steps_removed: 0,
                        time_source: 0x20,
                    }),
                };
                self.deliver(arrive, &announce, false);
            }
            // Sync は Announce から 50ms ずらす
            let sync_at = at + 50_000_000;
            if self.sends_sync && at.is_multiple_of(self.sync_interval_nanos) {
                let flags = if self.two_step {
                    Flags::TWO_STEP
                } else {
                    Flags(0)
                };
                let sync = PtpMessage {
                    header: self.header(flags, self.sequence, self.log_sync_interval),
                    body: Body::Sync {
                        origin: self.time_at(sync_at),
                    },
                };
                self.deliver(sync_at + self.one_way_nanos, &sync, true);
                if self.two_step {
                    let follow_up = PtpMessage {
                        header: self.header(Flags(0), self.sequence, self.log_sync_interval),
                        body: Body::FollowUp {
                            precise_origin: self.time_at(sync_at),
                        },
                    };
                    self.deliver(sync_at + self.one_way_nanos + 20_000, &follow_up, false);
                }
                self.sequence = self.sequence.wrapping_add(1);
            }
            self.generated_until += 1_000_000; // 1ms 刻みで見る
        }
    }
}

/// 模擬マスタにつながったポート。
struct FakeNetwork {
    clock: Arc<FakeClock>,
    master: Arc<Mutex<Master>>,
}

fn lock(master: &Mutex<Master>) -> MutexGuard<'_, Master> {
    master.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PtpPort for FakeNetwork {
    fn send_event(&mut self, payload: &[u8]) -> Result<EventStamp, TransportError> {
        let now = self.clock.nanos();
        let request = decode(payload).map_err(|error| TransportError::Io(error.to_string()))?;
        lock(&self.master).answer(now, request);
        Ok(EventStamp {
            precise: Some(self.clock.now()),
            user: self.clock.now(),
        })
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<PtpInbound>, TransportError> {
        let now = self.clock.nanos();
        let until = now + u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        let Some((at, payload, event)) = lock(&self.master).next_until(until) else {
            self.clock.set(until);
            return Ok(None);
        };
        self.clock.set(at.max(now));
        let stamp = self.clock.now();
        Ok(Some(PtpInbound {
            payload,
            event,
            received: EventStamp {
                precise: Some(stamp),
                user: stamp,
            },
            from: MASTER_ADDRESS,
        }))
    }
}

const fn config(domain: u8, request_interval_ms: i64, unicast: bool) -> PtpSlaveConfig {
    PtpSlaveConfig {
        domain,
        own: own_port_identity(0x0011_2233_4455_6677),
        request_interval: TimeDelta::from_millis(request_interval_ms),
        unicast,
        precise_source: TimestampSource::Kernel,
    }
}

fn slave(
    settings: Master,
    config: PtpSlaveConfig,
) -> (
    PtpSlaveService<FakeNetwork>,
    Arc<Mutex<Master>>,
    Arc<TimestampingStatus>,
) {
    let clock = Arc::new(FakeClock::default());
    let master = Arc::new(Mutex::new(settings));
    let status = Arc::new(TimestampingStatus::new());
    let network = FakeNetwork {
        clock: Arc::clone(&clock),
        master: Arc::clone(&master),
    };
    let service = PtpSlaveService::new(config, network, clock, Arc::clone(&status));
    (service, master, status)
}

fn assert_close(actual: TimeDelta, expected: TimeDelta) {
    assert!(
        (actual - expected).abs() <= TimeDelta::from_nanos(10),
        "actual {actual:?}, expected {expected:?}"
    );
}

#[test]
fn measures_a_two_step_master() {
    let (mut service, network, status) = slave(Master::default(), config(0, 1_000, false));
    let snapshot = service.run_once(Duration::from_secs(10)).unwrap();
    assert_eq!(snapshot.state, MasterState::Synchronized);
    let sample = snapshot.last_sample.unwrap();
    // マスタは 5ms 進み（TAI から 37 秒引いた UTC で）、片道 100µs
    assert_close(sample.offset().delta(), TimeDelta::from_millis(5));
    assert_close(sample.delay().delta(), TimeDelta::from_micros(200));
    assert_eq!(snapshot.master_address, Some(MASTER_ADDRESS));
    assert_eq!(snapshot.status.unwrap().two_step, Some(true));
    assert!(snapshot.anchor.is_some());
    assert_eq!(status.counts().kernel_samples, 1);

    // Announce を 2 通（0 秒・1 秒）受け取ってマスタを選び、次の Sync（1.05 秒）で 1 回だけ送る
    let requests = lock(&network).requests.clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let (sent_at, request) = requests[0].clone();
    assert_eq!(sent_at, 1_050_100_000);
    let bytes = encode(&request);
    assert_eq!(request.message_type(), MessageType::DelayReq);
    assert_eq!(bytes.len(), 44);
    assert_eq!(bytes[32], 0x01, "controlField");
    assert_eq!(bytes[33], 0x7F, "logMessageInterval");
    assert_eq!(request.header.correction, Correction(0));
    assert_eq!(
        request.header.flags,
        Flags(0),
        "マルチキャストではフラグなし"
    );
    // 自分の識別子は乱数から作り、ローカルに割り当てたユニキャストのアドレスにする
    assert_eq!(request.header.source.clock[0], 0x02);
    assert_eq!(request.header.source.port, 1);
}

#[test]
fn a_one_step_master_needs_no_follow_up() {
    let master = Master {
        two_step: false,
        ..Master::default()
    };
    let (mut service, _master, _status) = slave(master, config(0, 1_000, false));
    let snapshot = service.run_once(Duration::from_secs(10)).unwrap();
    assert_close(
        snapshot.last_sample.unwrap().offset().delta(),
        TimeDelta::from_millis(5),
    );
    assert_eq!(snapshot.status.unwrap().two_step, Some(false));
}

#[test]
fn unicast_requests_carry_the_flag() {
    let (mut service, network, _status) = slave(Master::default(), config(0, 1_000, true));
    drop(service.run_once(Duration::from_secs(10)).unwrap());
    let flags = lock(&network).requests[0].1.header.flags;
    assert_eq!(flags, Flags::UNICAST);
}

struct Collect {
    events: Vec<MasterEvent>,
}

impl MasterEventSink for Collect {
    fn publish(&mut self, event: &MasterEvent, _snapshot: &MasterSnapshot) {
        self.events.push(event.clone());
    }
}

/// Sync は 250ms ごと、利用者の指定は 100ms でも、マスタの最小間隔（1 秒）より短くは送らない。
/// マスタが 250ms を知らせたら、利用者の指定より短い Sync の間隔（250ms）で送る。
#[test]
fn requests_follow_the_masters_minimum_interval() {
    let master = Master {
        sync_interval_nanos: 250_000_000,
        log_sync_interval: -2,
        ..Master::default()
    };
    let (mut service, network, _status) = slave(master, config(0, 100, false));
    let mut sink = Collect { events: Vec::new() };
    service.run_until(&mut sink, Some(4), &AtomicBool::new(false));
    let times = lock(&network).request_times();
    assert_eq!(times.len(), 4);
    let gaps: Vec<u64> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(gaps, [NANOS_PER_SEC, NANOS_PER_SEC, NANOS_PER_SEC]);
    assert_eq!(service.snapshot().request_interval, TimeDelta::from_secs(1));

    let fast = Master {
        sync_interval_nanos: 250_000_000,
        log_sync_interval: -2,
        log_min_delay_req: -2,
        ..Master::default()
    };
    let (mut fast_service, fast_network, _fast_status) = slave(fast, config(0, 100, false));
    fast_service.run_until(&mut sink, Some(4), &AtomicBool::new(false));
    let fast_times = lock(&fast_network).request_times();
    let fast_gaps: Vec<u64> = fast_times
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect();
    assert_eq!(fast_gaps, [250_000_000, 250_000_000, 250_000_000]);
    let snapshot = fast_service.snapshot();
    assert_eq!(snapshot.request_interval, TimeDelta::from_millis(250));
    assert_eq!(
        snapshot.master_min_interval,
        Some(TimeDelta::from_millis(250))
    );
    assert!(
        sink.events
            .iter()
            .any(|event| matches!(*event, MasterEvent::SampleAccepted { .. }))
    );
}

#[test]
fn a_wrong_domain_times_out_with_a_hint() {
    let master = Master {
        domain: 24,
        ..Master::default()
    };
    let (mut service, network, _status) = slave(master, config(0, 1_000, false));
    let error = service.run_once(Duration::from_secs(5)).unwrap_err();
    let PtpOnceError::NoMaster(reason) = error.clone() else {
        panic!("{error}");
    };
    assert!(reason.contains("ドメイン 24"), "{reason}");
    assert!(reason.contains("--ptp-domain"), "{reason}");
    assert_eq!(lock(&network).request_times(), [], "何も送らない");
}

#[test]
fn a_master_that_ignores_requests_is_reported() {
    let master = Master {
        answers_delay_req: false,
        ..Master::default()
    };
    let (mut service, network, _status) = slave(master, config(0, 1_000, false));
    let error = service.run_once(Duration::from_secs(5)).unwrap_err();
    assert_eq!(
        error,
        PtpOnceError::NoSample("Delay_Resp が届きません".to_owned())
    );
    // 応答がなくても、間隔（1 秒）を守って送り続ける
    let times = lock(&network).request_times();
    assert_eq!(times.len(), 4);
    assert!(
        times
            .windows(2)
            .all(|pair| pair[1] - pair[0] >= NANOS_PER_SEC)
    );
}

/// 回数を決めて測るとき、マスタがいなければ 30 秒で諦める。
#[test]
fn a_bounded_run_gives_up_without_a_master() {
    let master = Master {
        domain: 24,
        ..Master::default()
    };
    let (mut service, network, _status) = slave(master, config(0, 1_000, false));
    let mut sink = Collect { events: Vec::new() };
    service.run_until(&mut sink, Some(5), &AtomicBool::new(false));
    assert_eq!(service.snapshot().state, MasterState::Listening);
    assert_eq!(service.snapshot().other_domains, [24]);
    assert_eq!(lock(&network).request_times(), []);
}

/// 回数を決めて測るとき、マスタを選べても Sync が届かなければ（交換を始められないので）30 秒で諦める。
#[test]
fn a_bounded_run_gives_up_without_syncs() {
    let master = Master {
        sends_sync: false,
        ..Master::default()
    };
    let (mut service, network, _status) = slave(master, config(0, 1_000, false));
    let mut sink = Collect { events: Vec::new() };
    service.run_until(&mut sink, Some(5), &AtomicBool::new(false));
    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, MasterState::Uncalibrated);
    assert_eq!(snapshot.stats.samples + snapshot.stats.failures, 0);
    assert_eq!(
        lock(&network).request_times(),
        [],
        "Sync がなければ送らない"
    );
}

/// 回数を決めて測るとき、途中でマスタを見失ったら（自走中になり、交換が進まない）30 秒で諦める。
/// 見失ったマスタのアドレスは出さない。
#[test]
fn a_bounded_run_gives_up_after_losing_the_master() {
    let master = Master {
        silent_from: Some(3 * NANOS_PER_SEC),
        ..Master::default()
    };
    let (mut service, _network, _status) = slave(master, config(0, 1_000, false));
    let mut sink = Collect { events: Vec::new() };
    service.run_until(&mut sink, Some(10), &AtomicBool::new(false));
    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, MasterState::Holdover);
    assert_eq!(snapshot.stats.samples, 2, "1.05 秒と 2.05 秒の Sync で測る");
    assert!(
        sink.events
            .iter()
            .any(|event| matches!(*event, MasterEvent::MasterLost { .. }))
    );
    assert_eq!(snapshot.status, None);
    assert_eq!(snapshot.master_address, None);
}

#[test]
fn run_until_returns_when_stopped() {
    let (mut service, _master, _status) = slave(Master::default(), config(0, 1_000, false));
    let mut sink = Collect { events: Vec::new() };
    service.run_until(&mut sink, None, &AtomicBool::new(true));
    assert_eq!(sink.events, []);
}
