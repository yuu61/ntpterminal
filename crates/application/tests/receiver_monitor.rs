//! `ReceiverMonitor` の結合テスト。
//!
//! 偽の時計（仮想時間）と、送られた GET を読んで値を返す模擬 SNMP エージェントを使う。
//! 実際の時間は待たない。送られた GET の中身（コミュニティ名・PDU の種類・request-id・OID の並び）も確かめる。

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Mutex, MutexGuard, PoisonError};

use application::acl::mib::meinberg::{self, ltng, os};
use application::acl::mib::microchip::s650;
use application::acl::mib::{MibError, MibProfile};
use application::port::{ClockPort, Inbound, PacketPort, TransportError};
use application::snapshot::ReceiverSnapshot;
use application::usecase::{MonitorError, ReceiverMonitor, ReceiverMonitorConfig};
use domain::receiver::{ReceiverState, SatelliteCounts};
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};
use domain::snmp::{ObjectId, Pdu, PduKind, SnmpMessage, SnmpValue, VarBind, decode, encode};

const COMMUNITY: &[u8] = b"secret-community";

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
}

impl ClockPort for FakeClock {
    fn now(&self) -> LocalStamp {
        let nanos = self.mono_nanos.load(Ordering::SeqCst);
        LocalStamp {
            mono: PcTime::monotonic(nanos),
            wall: NtpTimestamp::from_parts(3_900_000_000, 0)
                .wrapping_add(TimeDelta::from_nanos(nanos.cast_signed())),
        }
    }
}

/// 模擬エージェントの状態。テストから値や振る舞いを変えられるよう、ポートと共有する。
#[derive(Default)]
struct Agent {
    /// 持っているオブジェクト。ない OID には noSuchObject を返す
    objects: Vec<VarBind>,
    /// 最初の何回の GET に genErr を返すか
    reject_first: usize,
    /// 応答しない
    silent: bool,
    /// 正しい応答の前に、別の request-id の応答と壊れたバイト列を返す
    noisy: bool,
    /// 届いた GET
    requests: Vec<SnmpMessage>,
}

struct AgentPort {
    clock: Arc<FakeClock>,
    agent: Arc<Mutex<Agent>>,
    pending: VecDeque<Vec<u8>>,
}

impl PacketPort for AgentPort {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let request = decode(payload).map_err(|error| TransportError::Io(error.to_string()))?;
        let (silent, rejected, noisy, varbinds) = {
            let mut agent = self.agent.lock().unwrap_or_else(PoisonError::into_inner);
            agent.requests.push(request.clone());
            let varbinds: Vec<VarBind> = request
                .pdu
                .varbinds
                .iter()
                .map(|asked| {
                    agent
                        .objects
                        .iter()
                        .find(|held| held.name == asked.name)
                        .cloned()
                        .unwrap_or_else(|| VarBind {
                            name: asked.name.clone(),
                            value: SnmpValue::NoSuchObject,
                        })
                })
                .collect();
            (
                agent.silent,
                agent.requests.len() <= agent.reject_first,
                agent.noisy,
                varbinds,
            )
        };
        if silent {
            return Ok(self.clock.now());
        }
        let response = SnmpMessage {
            version: request.version,
            community: request.community,
            pdu: Pdu {
                kind: PduKind::Response,
                request_id: request.pdu.request_id,
                error_status: if rejected {
                    Pdu::GEN_ERR
                } else {
                    Pdu::NO_ERROR
                },
                error_index: 0,
                varbinds,
            },
        };
        let bytes = encode(&response);
        if noisy {
            let mut stale = response;
            stale.pdu.request_id -= 1;
            self.pending.push_back(encode(&stale));
            self.pending.push_back(vec![0x30, 0x80, 0x00]);
        }
        self.pending.push_back(bytes);
        Ok(self.clock.now())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        if let Some(payload) = self.pending.pop_front() {
            return Ok(Some(Inbound {
                payload,
                received: self.clock.now(),
            }));
        }
        self.clock.advance(timeout);
        Ok(None)
    }
}

const fn integer(oid: ObjectId, value: i64) -> VarBind {
    VarBind {
        name: oid,
        value: SnmpValue::Integer(value),
    }
}

fn text(oid: ObjectId, value: &str) -> VarBind {
    VarBind {
        name: oid,
        value: SnmpValue::OctetString(value.as_bytes().to_vec()),
    }
}

/// LANTIME（LTNG）: gps180・primary・同期・gpsSync・良好 5・可視 9。
fn lantime() -> Vec<VarBind> {
    vec![
        integer(ltng::column(ltng::COLUMN_TYPE, 1), 30),
        integer(ltng::column(ltng::COLUMN_USAGE, 1), 3),
        integer(ltng::column(ltng::COLUMN_STATE, 1), 1),
        integer(ltng::column(ltng::COLUMN_SUBSTATE, 1), 1),
        integer(ltng::column(ltng::COLUMN_STATUS_A, 1), 5),
        integer(ltng::column(ltng::COLUMN_MAX_STATUS_A, 1), 9),
    ]
}

/// `SyncServer` S650: 追尾 3。
fn syncserver() -> Vec<VarBind> {
    vec![
        text(s650::object(&s650::SYSTEM_MODEL), "SyncServer S650\0"),
        text(s650::object(&s650::CLOCK_STATUS), "NormalLock"),
        text(s650::object(&s650::GNSS_RECEIVER_STATUS), "Tracking"),
        text(
            s650::object(&s650::GNSS_SATLIST),
            "3,X1,Y1,CGPS,X3,Y2,CGPS,X7,Y3,CGalileo",
        ),
    ]
}

struct Harness {
    clock: Arc<FakeClock>,
    agent: Arc<Mutex<Agent>>,
    monitor: ReceiverMonitor<AgentPort>,
}

impl Harness {
    fn new(objects: Vec<VarBind>, profile: Option<MibProfile>) -> Self {
        let clock = Arc::new(FakeClock::default());
        let agent = Arc::new(Mutex::new(Agent {
            objects,
            ..Agent::default()
        }));
        let port = AgentPort {
            clock: Arc::clone(&clock),
            agent: Arc::clone(&agent),
            pending: VecDeque::new(),
        };
        let config = ReceiverMonitorConfig {
            community: COMMUNITY.to_vec(),
            profile,
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(1),
        };
        let shared: Arc<dyn ClockPort> = Arc::<FakeClock>::clone(&clock);
        Self {
            clock,
            agent: Arc::clone(&agent),
            monitor: ReceiverMonitor::new(config, port, shared),
        }
    }

    fn agent(&self) -> MutexGuard<'_, Agent> {
        self.agent.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 届いた GET の varbind の数。
    fn request_sizes(&self) -> Vec<usize> {
        self.agent()
            .requests
            .iter()
            .map(|request| request.pdu.varbinds.len())
            .collect()
    }
}

fn names(request: &SnmpMessage) -> Vec<ObjectId> {
    request
        .pdu
        .varbinds
        .iter()
        .map(|varbind| varbind.name.clone())
        .collect()
}

#[test]
fn detects_the_mib_with_one_get_and_then_reads_only_the_status() {
    let mut harness = Harness::new(lantime(), None);
    let snapshot = harness.monitor.poll().clone();
    assert_eq!(snapshot.profile, Some(MibProfile::MeinbergLtng));
    assert_eq!(snapshot.last_error, None);
    assert_eq!(snapshot.last_success, Some(harness.clock.now().mono));
    let status = snapshot.status.unwrap();
    assert_eq!(status.state, Some(ReceiverState::Synchronized));
    assert_eq!(status.name.as_deref(), Some("gps180"));
    assert_eq!(
        status.satellites,
        SatelliteCounts {
            good: Some(5),
            in_view: Some(9),
            ..SatelliteCounts::default()
        }
    );

    // 2 回目は判定し直さず、状態だけを読む
    let _: &_ = harness.monitor.poll();
    let requests = harness.agent().requests.clone();
    assert_eq!(requests.len(), 3);
    for (request, expected_id) in requests.iter().zip(1i64..) {
        assert_eq!(request.version, SnmpMessage::VERSION_2C);
        assert_eq!(request.community, COMMUNITY);
        assert_eq!(request.pdu.kind, PduKind::GetRequest);
        assert_eq!(request.pdu.request_id, expected_id);
        assert!(
            request
                .pdu
                .varbinds
                .iter()
                .all(|varbind| varbind.value == SnmpValue::Null)
        );
    }
    let probes: Vec<ObjectId> = MibProfile::DETECTION_ORDER
        .iter()
        .map(|profile| profile.probe_oid())
        .collect();
    assert_eq!(names(&requests[0]), probes);
    assert_eq!(names(&requests[1]), MibProfile::MeinbergLtng.status_oids());
    assert_eq!(names(&requests[2]), MibProfile::MeinbergLtng.status_oids());
}

#[test]
fn asks_one_mib_at_a_time_after_an_error_response() {
    let mut harness = Harness::new(syncserver(), None);
    harness.agent().reject_first = 1;
    let snapshot = harness.monitor.poll().clone();
    assert_eq!(snapshot.profile, Some(MibProfile::MicrochipS650));
    assert_eq!(snapshot.last_error, None);
    let status = snapshot.status.unwrap();
    assert_eq!(status.satellites.tracking, Some(3));
    assert_eq!(status.constellations.len(), 2);
    // まとめての判定（genErr）→ 1 つずつ 4 回 → 状態
    assert_eq!(harness.request_sizes(), [4, 1, 1, 1, 1, 4]);
}

#[test]
fn a_specified_mib_is_never_replaced_by_another() {
    let mut harness = Harness::new(lantime(), Some(MibProfile::MeinbergOs));
    for _ in 0..2u32 {
        let snapshot = harness.monitor.poll().clone();
        assert_eq!(snapshot.profile, Some(MibProfile::MeinbergOs));
        assert_eq!(
            snapshot.last_error,
            Some(MonitorError::Mib(MibError::NoValues(
                MibProfile::MeinbergOs
            )))
        );
        assert_eq!(snapshot.status, None);
    }
    // 判定の GET は送らない
    let requests = harness.agent().requests.clone();
    assert!(
        requests
            .iter()
            .all(|request| names(request) == MibProfile::MeinbergOs.status_oids())
    );
}

#[test]
fn a_timeout_keeps_the_last_status_but_reports_the_error() {
    let mut harness = Harness::new(lantime(), None);
    let first = harness.monitor.poll().clone();
    let succeeded_at = first.last_success.unwrap();

    harness.agent().silent = true;
    let failed = harness.monitor.poll().clone();
    assert_eq!(failed.last_error, Some(MonitorError::Timeout));
    assert_eq!(failed.status, first.status);
    assert_eq!(failed.last_success, Some(succeeded_at));
    assert_eq!((failed.attempts, failed.successes), (2, 1));
    // 応答を待った分だけ（仮想の）時間が進む
    assert_eq!(
        harness.clock.now().mono.since(succeeded_at).unwrap(),
        TimeDelta::from_secs(1)
    );

    harness.agent().silent = false;
    let recovered = harness.monitor.poll().clone();
    assert_eq!(recovered.last_error, None);
    assert_eq!((recovered.attempts, recovered.successes), (3, 2));
}

#[test]
fn ignores_stale_and_unreadable_responses() {
    let mut harness = Harness::new(lantime(), None);
    harness.agent().noisy = true;
    let snapshot = harness.monitor.poll().clone();
    assert_eq!(snapshot.last_error, None);
    assert_eq!(snapshot.profile, Some(MibProfile::MeinbergLtng));
}

#[test]
fn detects_again_when_the_detected_mib_stops_answering() {
    let mut harness = Harness::new(lantime(), None);
    assert_eq!(
        harness.monitor.poll().profile,
        Some(MibProfile::MeinbergLtng)
    );

    harness.agent().objects = syncserver();
    let failed = harness.monitor.poll().clone();
    assert_eq!(
        failed.last_error,
        Some(MonitorError::Mib(MibError::NoValues(
            MibProfile::MeinbergLtng
        )))
    );
    assert_eq!(failed.profile, None);

    let redetected = harness.monitor.poll().clone();
    assert_eq!(redetected.profile, Some(MibProfile::MicrochipS650));
    assert_eq!(redetected.last_error, None);
}

#[test]
fn unknown_agents_are_reported() {
    let mut harness = Harness::new(Vec::new(), None);
    let snapshot = harness.monitor.poll().clone();
    assert_eq!(snapshot.last_error, Some(MonitorError::UnknownMib));
    assert_eq!(snapshot.profile, None);
}

/// LANTIME V6 の実機の SNMP の記録にあった OID だけを持つエージェント（値は手で書いたもの）。
/// 参照時計の表はあるが、`mbgLtNgNtpCurrentState` と `MaxStatusA` はない。
#[test]
fn detects_ltng_from_the_refclock_table_alone() {
    let gauge = |oid: ObjectId, value: u32| VarBind {
        name: oid,
        value: SnmpValue::Gauge32(value),
    };
    let recorded = vec![
        integer(ltng::column(ltng::COLUMN_TYPE, 1), 30),
        integer(ltng::column(ltng::COLUMN_USAGE, 1), 3),
        integer(ltng::column(ltng::COLUMN_STATE, 1), 1),
        integer(ltng::column(ltng::COLUMN_SUBSTATE, 1), 1),
        gauge(ltng::column(ltng::COLUMN_STATUS_A, 1), 5),
        gauge(ltng::column(8, 1), 0),
    ];
    let mut harness = Harness::new(recorded, None);
    let snapshot = harness.monitor.poll().clone();
    assert_eq!(snapshot.last_error, None);
    assert_eq!(snapshot.profile, Some(MibProfile::MeinbergLtng));
    let status = snapshot.status.unwrap();
    assert_eq!(status.state, Some(ReceiverState::Synchronized));
    assert_eq!(status.satellites.good, Some(5));
    assert_eq!(status.satellites.in_view, None);
}

/// meinbergOS の実機の SNMP の記録にあった OID だけを持つエージェント（値は手で書いたもの）。
/// `IsGnss` がないので、GNSS の受信機と確かめられず、衛星の数は出さない（MIB の説明どおりの保守的な扱い）。
#[test]
fn detects_meinberg_os_and_withholds_counts_without_is_gnss() {
    let recorded = vec![
        integer(meinberg::oid(&os::SYS_SYNC), 1),
        integer(os::column(os::COLUMN_STATE, 1), 6),
        integer(os::column(os::COLUMN_GOOD_SV, 1), 24),
        integer(os::column(os::COLUMN_SV, 1), 33),
    ];
    let mut harness = Harness::new(recorded, None);
    let snapshot = harness.monitor.poll().clone();
    assert_eq!(snapshot.last_error, None);
    assert_eq!(snapshot.profile, Some(MibProfile::MeinbergOs));
    let status = snapshot.status.unwrap();
    assert_eq!(status.state, Some(ReceiverState::Synchronized));
    assert!(status.satellites.is_empty());
}

/// `run_until` は問い合わせごとに知らせ、停止の合図で戻る（間隔の待ちに入っても合図を見て抜ける）。
#[test]
fn run_until_publishes_each_poll_and_stops_on_request() {
    let mut harness = Harness::new(lantime(), None);
    let stop = Arc::new(AtomicBool::new(false));
    let mut published = Vec::new();
    harness.monitor.run_until(
        &mut |snapshot: &ReceiverSnapshot| {
            published.push(snapshot.clone());
            stop.store(true, Ordering::Relaxed);
        },
        &stop,
    );
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].profile, Some(MibProfile::MeinbergLtng));
}

#[test]
fn the_community_never_appears_in_messages() {
    let mut harness = Harness::new(Vec::new(), None);
    harness.agent().silent = true;
    let snapshot = harness.monitor.poll().clone();
    let community = String::from_utf8_lossy(COMMUNITY).into_owned();
    assert!(
        !snapshot
            .last_error
            .unwrap()
            .to_string()
            .contains(&community)
    );
    assert!(!format!("{:?}", harness.monitor).contains(&community));
}
