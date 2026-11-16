//! `Ptp4lClient`・`PhcService` の結合テスト。
//!
//! 偽の時計（仮想時間）と、届いた管理メッセージを読んで答える模擬 ptp4l、模擬 PHC を使う。実際の時間は待たない。
//! 送られた GET の中身（ドメイン番号・boundaryHops・宛先のポート・送り元の識別子・空のデータ）も確かめる。

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Mutex, MutexGuard, PoisonError};

use application::port::{
    ClockPort, Inbound, PacketPort, PhcEventSink, PhcPort, PhcReadError, TransportError,
};
use application::snapshot::PhcSnapshot;
use application::usecase::{PhcOnceError, PhcService, Ptp4lClient, Ptp4lClientConfig, Ptp4lError};
use domain::ptp_clock::{
    LeapWarning, PhcConfig, PhcEvent, PhcReading, PhcState, PhcTime, PortState, PortStatus,
    Timescale,
};
use domain::ptp_management::{
    Action, ClockQuality, CurrentDataSet, DefaultDataSet, ManagementErrorId, ManagementId,
    ManagementMessage, ManagementTlv, ParentDataSet, PortDataSet, PortHwclockNp, PortIdentity,
    PortPropertiesNp, TimePropertiesDataSet, decode, encode,
};
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};

const NANOS_PER_SEC: i128 = 1_000_000_000;
/// PC の壁時計の起点（2026-11-17 00:00:00 UTC の Unix 秒）
const WALL_BASE_SECS: i128 = 1_794_873_600;
const PTP4L_IDENTITY: [u8; 8] = [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56];
const GRANDMASTER: [u8; 8] = [0xEC, 0x46, 0x70, 0xFF, 0xFE, 0x00, 0x00, 0x01];
const CLIENT_ID: u32 = 0x0001_2345;
const TIMEOUT: Duration = Duration::from_millis(200);

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
            wall: NtpTimestamp::from_unix_nanos(WALL_BASE_SECS * NANOS_PER_SEC)
                .wrapping_add(TimeDelta::from_nanos(nanos.cast_signed())),
        }
    }
}

/// 模擬 ptp4l の答え方。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Replies {
    Normal,
    /// 応答しない
    Silent,
    /// 正しい応答の前に、前の sequenceId の応答と壊れたバイト列を返す
    Noisy,
}

/// 模擬 ptp4l の状態。テストから値や振る舞いを変えられるよう、ポートと共有する。
struct Ptp4l {
    domain: u8,
    /// ポートごとの（インターフェース名・タイムスタンプの方式・PHC・状態）
    ports: Vec<(&'static str, u8, i32, u8)>,
    /// `PORT_HWCLOCK_NP` に答える（linuxptp 4.0 以降）
    hwclock: bool,
    /// `PORT_PROPERTIES_NP` に `NOT_SUPPORTED` を返す（読み取り専用のソケット）
    read_only: bool,
    offset_from_master_ns: i64,
    utc_offset: i16,
    time_flags: u8,
    replies: Replies,
    /// 届いた問い合わせ
    requests: Vec<ManagementMessage>,
}

impl Default for Ptp4l {
    fn default() -> Self {
        Self {
            domain: 0,
            ports: vec![("eno1", 1, 0i32, 9)],
            hwclock: true,
            read_only: false,
            offset_from_master_ns: -12,
            utc_offset: 37,
            time_flags: TimePropertiesDataSet::UTC_OFFSET_VALID
                | TimePropertiesDataSet::PTP_TIMESCALE
                | TimePropertiesDataSet::TIME_TRACEABLE,
            replies: Replies::Normal,
            requests: Vec::new(),
        }
    }
}

impl Ptp4l {
    /// 問い合わせへの応答の TLV。`None` なら答えない（ドメイン番号が違うなど）。
    fn answer(&self, request: &ManagementMessage) -> Option<ManagementTlv> {
        if request.domain != self.domain || request.action != Action::Get {
            return None;
        }
        let ManagementTlv::Management { id, .. } = request.tlv else {
            return None;
        };
        let port = usize::from(request.target.port);
        let port_entry = (port >= 1).then(|| self.ports.get(port - 1)).flatten();
        let identity = |number: u16| PortIdentity {
            clock: PTP4L_IDENTITY,
            port: number,
        };
        let refuse = |error| ManagementTlv::ErrorStatus {
            error,
            id,
            display: Vec::new(),
        };
        let data = match id {
            ManagementId::DEFAULT_DATA_SET => DefaultDataSet {
                number_ports: u16::try_from(self.ports.len()).unwrap_or(u16::MAX),
                clock_identity: PTP4L_IDENTITY,
                domain: self.domain,
                ..DefaultDataSet::default()
            }
            .to_bytes(),
            ManagementId::CURRENT_DATA_SET => CurrentDataSet {
                steps_removed: 1,
                offset_from_master_scaled: self.offset_from_master_ns << 16i32,
                mean_path_delay_scaled: 1_234 << 16i32,
            }
            .to_bytes(),
            ManagementId::PARENT_DATA_SET => ParentDataSet {
                grandmaster_identity: GRANDMASTER,
                grandmaster_quality: ClockQuality {
                    class: 6,
                    accuracy: 0x21,
                    offset_scaled_log_variance: 0x4E5D,
                },
                ..ParentDataSet::default()
            }
            .to_bytes(),
            ManagementId::TIME_PROPERTIES_DATA_SET => TimePropertiesDataSet {
                current_utc_offset: self.utc_offset,
                flags: self.time_flags,
                time_source: 0x20,
            }
            .to_bytes(),
            ManagementId::PORT_DATA_SET => {
                let Some(&(_, _, _, state)) = port_entry else {
                    return Some(refuse(ManagementErrorId::WrongValue));
                };
                PortDataSet {
                    port: identity(request.target.port),
                    port_state: state,
                    ..PortDataSet::default()
                }
                .to_bytes()
            }
            ManagementId::PORT_PROPERTIES_NP => {
                if self.read_only {
                    return Some(refuse(ManagementErrorId::NotSupported));
                }
                let Some(&(name, timestamping, _, state)) = port_entry else {
                    return Some(refuse(ManagementErrorId::WrongValue));
                };
                PortPropertiesNp {
                    port: identity(request.target.port),
                    port_state: state,
                    timestamping,
                    interface: name.as_bytes().to_vec(),
                }
                .to_bytes()
            }
            ManagementId::PORT_HWCLOCK_NP => {
                if !self.hwclock {
                    return Some(refuse(ManagementErrorId::NoSuchId));
                }
                let Some(&(_, _, phc_index, _)) = port_entry else {
                    return Some(refuse(ManagementErrorId::WrongValue));
                };
                PortHwclockNp {
                    port: identity(request.target.port),
                    phc_index,
                    flags: 0,
                }
                .to_bytes()
            }
            _ => return Some(refuse(ManagementErrorId::NoSuchId)),
        };
        Some(ManagementTlv::Management { id, data })
    }
}

struct Ptp4lSocket {
    clock: Arc<FakeClock>,
    ptp4l: Arc<Mutex<Ptp4l>>,
    pending: VecDeque<Vec<u8>>,
}

impl PacketPort for Ptp4lSocket {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        let request = decode(payload).map_err(|error| TransportError::Io(error.to_string()))?;
        let (replies, answer) = {
            let mut ptp4l = lock(&self.ptp4l);
            ptp4l.requests.push(request.clone());
            (ptp4l.replies, ptp4l.answer(&request))
        };
        if replies == Replies::Silent {
            return Ok(self.clock.now());
        }
        let Some(tlv) = answer else {
            return Ok(self.clock.now());
        };
        // ptp4l の応答: 宛先は問い合わせの送り元、sequenceId は同じ、actionField は RESPONSE
        let reply = ManagementMessage {
            version: 0x12,
            source: PortIdentity {
                clock: PTP4L_IDENTITY,
                port: 0,
            },
            target: request.source,
            action: Action::Response,
            tlv,
            ..request
        };
        let bytes = encode(&reply);
        if replies == Replies::Noisy {
            let mut stale = reply;
            stale.sequence_id = stale.sequence_id.wrapping_sub(1);
            self.pending.push_back(encode(&stale));
            self.pending.push_back(vec![0x0D, 0x02, 0x00]);
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

/// 模擬 PHC: UTC で PC より `ahead_ns` 進んだ時刻を TAI（+37 秒）で持ち、2 µs の幅で読める。
struct FakePhc {
    clock: Arc<FakeClock>,
    ahead_ns: i64,
    fail: bool,
    reads: Arc<AtomicU64>,
}

impl PhcPort for FakePhc {
    fn read(&mut self) -> Result<PhcReading, PhcReadError> {
        let _previous = self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(PhcReadError("/dev/ptp0 を読めません".to_owned()));
        }
        let before = self.clock.now();
        self.clock.advance(Duration::from_micros(2));
        let after = self.clock.now();
        let middle = before
            .wall
            .wrapping_add(TimeDelta::from_micros(1) + TimeDelta::from_nanos(self.ahead_ns));
        Ok(PhcReading {
            phc: PhcTime::from_nanos(middle.to_unix_nanos() + 37 * NANOS_PER_SEC),
            before,
            after,
        })
    }
}

fn lock(ptp4l: &Mutex<Ptp4l>) -> MutexGuard<'_, Ptp4l> {
    ptp4l.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Harness {
    clock: Arc<FakeClock>,
    ptp4l: Arc<Mutex<Ptp4l>>,
    phc_reads: Arc<AtomicU64>,
}

impl Harness {
    fn new(ptp4l: Ptp4l) -> Self {
        Self {
            clock: Arc::new(FakeClock::default()),
            ptp4l: Arc::new(Mutex::new(ptp4l)),
            phc_reads: Arc::new(AtomicU64::new(0)),
        }
    }

    fn ptp4l(&self) -> MutexGuard<'_, Ptp4l> {
        lock(&self.ptp4l)
    }

    fn client(&self, domain: u8) -> Ptp4lClient<Ptp4lSocket> {
        let shared: Arc<dyn ClockPort> = Arc::<FakeClock>::clone(&self.clock);
        Ptp4lClient::new(
            Ptp4lSocket {
                clock: Arc::clone(&self.clock),
                ptp4l: Arc::clone(&self.ptp4l),
                pending: VecDeque::new(),
            },
            shared,
            Ptp4lClientConfig {
                domain,
                timeout: TIMEOUT,
                client_id: CLIENT_ID,
            },
        )
    }

    fn service(&self, ahead_ns: i64, fail: bool) -> PhcService<FakePhc, Ptp4lSocket> {
        let shared: Arc<dyn ClockPort> = Arc::<FakeClock>::clone(&self.clock);
        PhcService::new(
            PhcConfig::with_poll_interval(TimeDelta::from_millis(500)),
            FakePhc {
                clock: Arc::clone(&self.clock),
                ahead_ns,
                fail,
                reads: Arc::clone(&self.phc_reads),
            },
            self.client(0),
            vec![1],
            shared,
        )
    }

    /// 届いた問い合わせの（managementId・宛先のポート番号）。
    fn asked(&self) -> Vec<(ManagementId, u16)> {
        self.ptp4l()
            .requests
            .iter()
            .filter_map(|request| match request.tlv {
                ManagementTlv::Management { id, .. } => Some((id, request.target.port)),
                ManagementTlv::ErrorStatus { .. } | ManagementTlv::Other { .. } => None,
            })
            .collect()
    }
}

#[test]
fn sends_local_only_gets_like_pmc() {
    let harness = Harness::new(Ptp4l::default());
    let ports = harness.client(0).discover_ports().unwrap();
    let ptp4l = harness.ptp4l();
    assert_eq!(ptp4l.requests.len(), 3);
    for (sequence, request) in (1u16..).zip(&ptp4l.requests) {
        assert_eq!(request.action, Action::Get);
        assert_eq!(request.domain, 0);
        assert_eq!(request.sequence_id, sequence);
        // ptp4l の外（ネットワーク）へ転送させない
        assert_eq!(
            (request.starting_boundary_hops, request.boundary_hops),
            (0, 0)
        );
        assert_eq!(request.transport_specific, 0);
        // pmc と同じく、プロセス番号から作った送り元
        assert_eq!(
            request.source,
            PortIdentity {
                clock: [0, 0, 0, 0, 0, 0, 0x00, 0x01],
                port: 0x2345,
            }
        );
        let ManagementTlv::Management { data, .. } = request.tlv.clone() else {
            panic!("MANAGEMENT TLV のはず");
        };
        assert!(data.is_empty(), "GET のデータは空");
    }
    drop(ptp4l);
    assert_eq!(
        harness.asked(),
        [
            (ManagementId::DEFAULT_DATA_SET, 0xFFFF),
            (ManagementId::PORT_PROPERTIES_NP, 1),
            (ManagementId::PORT_HWCLOCK_NP, 1),
        ]
    );
    assert_eq!(ports.len(), 1);
    let port = &ports[0];
    assert_eq!(
        (port.number, port.interface.as_str(), port.state),
        (1, "eno1", PortState::Slave)
    );
    assert_eq!(port.hwclock.map(|clock| clock.index), Some(0i32));
}

#[test]
fn old_linuxptp_does_not_report_the_phc() {
    let harness = Harness::new(Ptp4l {
        hwclock: false,
        ports: vec![("eth0", 1, 0i32, 6), ("eth1", 0, -1i32, 4)],
        ..Ptp4l::default()
    });
    let ports = harness.client(0).discover_ports().unwrap();
    assert_eq!(ports.len(), 2);
    assert!(ports.iter().all(|port| port.hwclock.is_none()));
    assert!(ports[0].timestamping.uses_phc());
    assert!(!ports[1].timestamping.uses_phc());
}

#[test]
fn a_read_only_socket_cannot_list_the_ports() {
    let harness = Harness::new(Ptp4l {
        read_only: true,
        ..Ptp4l::default()
    });
    let error = harness.client(0).discover_ports().unwrap_err();
    assert_eq!(error, Ptp4lError::ReadOnlySocket);
    assert!(error.to_string().contains("uds_address"), "{error}");
}

#[test]
fn a_different_domain_gets_no_answer() {
    let harness = Harness::new(Ptp4l {
        domain: 24,
        ..Ptp4l::default()
    });
    let error = harness.client(0).discover_ports().unwrap_err();
    assert_eq!(
        error,
        Ptp4lError::Timeout {
            id: ManagementId::DEFAULT_DATA_SET,
        }
    );
    assert!(error.to_string().contains("ドメイン番号"), "{error}");
    // 時間切れまで（仮想の）時間が進む。問い合わせは 1 つで、やめる
    assert_eq!(harness.clock.now().mono, PcTime::monotonic(200_000_000));
    assert_eq!(harness.ptp4l().requests.len(), 1);
    let _ports = harness.client(24).discover_ports().unwrap();
}

#[test]
fn polls_ptp4l_then_reads_the_phc() {
    let harness = Harness::new(Ptp4l::default());
    let mut service = harness.service(300_000, false);
    let events = service.poll();
    assert!(events.contains(&PhcEvent::StateChanged {
        from: PhcState::Starting,
        to: PhcState::Synchronized,
    }));
    assert_eq!(
        harness.asked(),
        [
            (ManagementId::CURRENT_DATA_SET, 0xFFFF),
            (ManagementId::PARENT_DATA_SET, 0xFFFF),
            (ManagementId::TIME_PROPERTIES_DATA_SET, 0xFFFF),
            (ManagementId::PORT_DATA_SET, 1),
        ]
    );
    let snapshot = service.snapshot();
    let status = snapshot.status.unwrap();
    assert_eq!(
        status.ports,
        [PortStatus {
            number: 1,
            state: PortState::Slave,
        }]
    );
    assert_eq!(status.offset_from_master, TimeDelta::from_nanos(-12));
    assert_eq!(status.mean_path_delay, TimeDelta::from_nanos(1_234));
    assert_eq!(status.grandmaster.to_string(), "ec4670.fffe.000001");
    assert_eq!(status.time.timescale, Timescale::Ptp);
    assert_eq!(status.time.leap, LeapWarning::None);
    // TAI の 37 秒を引いた UTC で、PC より 0.3 ms 進んでいる
    let offset = snapshot.last_sample.unwrap().offset().delta();
    assert!(
        (offset - TimeDelta::from_micros(300)).abs() <= TimeDelta::from_nanos(10),
        "{offset:?}"
    );
    assert!(snapshot.anchor.is_some());
    assert_eq!(snapshot.stats.readings, 1);
    assert_eq!(snapshot.stats.status_updates, 1);
    assert_eq!(harness.phc_reads.load(Ordering::SeqCst), 1);
}

/// ptp4l が止まっても、時間切れは 1 回分だけ待ち、PHC は読み続ける。
#[test]
fn a_silent_ptp4l_costs_one_timeout_and_the_phc_is_still_read() {
    let harness = Harness::new(Ptp4l::default());
    let mut service = harness.service(0, false);
    drop(service.poll());
    let asked_before = harness.ptp4l().requests.len();
    harness.ptp4l().replies = Replies::Silent;
    let started = harness.clock.now().mono;
    let events = service.poll();
    assert!(events.contains(&PhcEvent::StateChanged {
        from: PhcState::Synchronized,
        to: PhcState::Holdover,
    }));
    assert_eq!(harness.ptp4l().requests.len(), asked_before + 1);
    let waited = harness.clock.now().mono.since(started).unwrap();
    assert_eq!(
        waited,
        TimeDelta::from_millis(200) + TimeDelta::from_micros(2)
    );
    let snapshot = service.snapshot();
    assert!(snapshot.status_error.unwrap().contains("CURRENT_DATA_SET"));
    assert_eq!(snapshot.stats.readings, 2);
    assert_eq!(snapshot.stats.status_failures, 1);
    assert!(snapshot.anchor.is_some(), "自走中も時計を出す");

    harness.ptp4l().replies = Replies::Normal;
    drop(service.poll());
    assert_eq!(service.snapshot().state, PhcState::Synchronized);
    assert_eq!(service.snapshot().status_error, None);
}

#[test]
fn does_not_read_the_phc_before_ptp4l_answers_once() {
    let harness = Harness::new(Ptp4l {
        replies: Replies::Silent,
        ..Ptp4l::default()
    });
    let mut service = harness.service(0, false);
    drop(service.poll());
    assert_eq!(harness.phc_reads.load(Ordering::SeqCst), 0);
    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, PhcState::Starting);
    assert_eq!(snapshot.stats.read_failures, 0);
    assert!(
        matches!(service.poll_once(), Err(PhcOnceError::Status(detail)) if detail.contains("応答がありません"))
    );
}

#[test]
fn ignores_stale_and_unreadable_replies() {
    let harness = Harness::new(Ptp4l {
        replies: Replies::Noisy,
        ..Ptp4l::default()
    });
    let mut service = harness.service(0, false);
    drop(service.poll());
    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, PhcState::Synchronized);
    assert_eq!(snapshot.stats.status_failures, 0);
}

#[test]
fn hides_the_clock_while_ptp4l_is_listening() {
    let harness = Harness::new(Ptp4l {
        ports: vec![("eno1", 1, 0i32, 4)],
        ..Ptp4l::default()
    });
    let mut service = harness.service(650_000_000, false);
    drop(service.poll());
    let snapshot = service.snapshot();
    assert_eq!(snapshot.state, PhcState::Acquiring);
    assert_eq!(snapshot.anchor, None);
    // PC との差は出せる（PHC が UTC から 0.65 秒ずれていることが分かる）
    let offset = snapshot.last_sample.unwrap().offset().delta();
    assert!((offset - TimeDelta::from_millis(650)).abs() <= TimeDelta::from_nanos(10));
}

/// 通知のたびに仮想の時計を問い合わせの間隔だけ進める通知先（`run_until` が実時間を待たないように）。
/// `stop_after` 回目の通知で停止の合図を立てる。
struct AdvancingSink {
    clock: Arc<FakeClock>,
    stop: Arc<AtomicBool>,
    stop_after: Option<usize>,
    events: Vec<PhcEvent>,
}

impl PhcEventSink for AdvancingSink {
    fn publish(&mut self, event: &PhcEvent, _snapshot: &PhcSnapshot) {
        self.events.push(event.clone());
        self.clock.advance(Duration::from_millis(500));
        if self.stop_after == Some(self.events.len()) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }
}

impl AdvancingSink {
    fn readings(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, PhcEvent::ReadingAccepted { .. }))
            .count()
    }
}

#[test]
fn run_until_stops_after_the_given_attempts() {
    let harness = Harness::new(Ptp4l::default());
    let mut service = harness.service(0, false);
    let stop = Arc::new(AtomicBool::new(false));
    let mut sink = AdvancingSink {
        clock: Arc::clone(&harness.clock),
        stop: Arc::clone(&stop),
        stop_after: None,
        events: Vec::new(),
    };
    service.run_until(&mut sink, Some(3), &stop);
    assert_eq!(service.snapshot().stats.attempts, 3);
    assert_eq!(sink.readings(), 3);
    assert_eq!(harness.phc_reads.load(Ordering::SeqCst), 3);
}

#[test]
fn run_until_returns_when_stopped() {
    let harness = Harness::new(Ptp4l::default());
    let mut service = harness.service(0, false);
    let stop = Arc::new(AtomicBool::new(false));
    let mut sink = AdvancingSink {
        clock: Arc::clone(&harness.clock),
        stop: Arc::clone(&stop),
        stop_after: Some(1),
        events: Vec::new(),
    };
    service.run_until(&mut sink, None, &stop);
    // 1 回目の問い合わせの通知の途中で止めた。2 回目は問い合わせない
    assert_eq!(service.snapshot().stats.attempts, 1);
    assert_eq!(sink.readings(), 1);

    // 止まっていれば、問い合わせずにすぐ戻る
    service.run_until(&mut sink, None, &stop);
    assert_eq!(service.snapshot().stats.attempts, 1);
}

#[test]
fn once_reports_phc_failures() {
    let harness = Harness::new(Ptp4l::default());
    let mut failing = harness.service(0, true);
    assert_eq!(
        failing.poll_once(),
        Err(PhcOnceError::Reading("/dev/ptp0 を読めません".to_owned()))
    );
    let mut working = harness.service(0, false);
    let snapshot = working.poll_once().unwrap();
    assert_eq!(snapshot.state, PhcState::Synchronized);
    assert!(snapshot.last_sample.is_some());
}
