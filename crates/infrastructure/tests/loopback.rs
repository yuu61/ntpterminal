//! 実際の UDP ソケット（ループバック）を使った端から端までのテスト。
//!
//! 疑似サーバは 127.0.0.1 に bind する（Windows のファイアウォール確認を避けるため）。

extern crate alloc;

use alloc::sync::Arc;
use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;
use std::io;
use std::net::UdpSocket;

use application::port::{ClockPort, EventSink};
use application::snapshot::SyncSnapshot;
use application::usecase::{OnceError, OnceOutcome, SyncService, query_once};
use domain::packet_crafting::RequestTemplate;
use domain::shared_kernel::TimeDelta;
use domain::time_sync::{SourceConfig, SyncEvent, SyncState};
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::sim_server::{SimServer, SimServerConfig};
use infrastructure::transport::UdpTransport;

fn loopback_any_port() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn spawn_server(offset_ms: i64) -> io::Result<SimServer> {
    SimServer::spawn(
        loopback_any_port(),
        SimServerConfig {
            offset: TimeDelta::from_millis(offset_ms),
            ..SimServerConfig::default()
        },
    )
}

fn once(server: SocketAddr, timeout: Duration) -> Result<OnceOutcome, OnceError> {
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport = UdpTransport::connect(server, Arc::clone(&clock))
        .map_err(|error| OnceError::Transport(error.to_string()))?;
    query_once(
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        timeout,
    )
}

#[test]
fn measures_positive_offset_when_server_is_ahead() {
    let server = spawn_server(5).unwrap();
    let outcome = once(server.local_addr(), Duration::from_secs(2)).unwrap();
    let offset_ms = outcome.sample.offset().delta().as_millis_f64();
    assert!(
        (offset_ms - 5.0).abs() < 2.0f64,
        "offset = {offset_ms} ms（+5ms のはず）"
    );
    assert_eq!(outcome.server_info.stratum.value(), 1);
    assert_eq!(outcome.server_info.reference_id.to_string(), "SIM");
}

#[test]
fn measures_negative_offset_when_server_is_behind() {
    let server = spawn_server(-7).unwrap();
    let outcome = once(server.local_addr(), Duration::from_secs(2)).unwrap();
    let offset_ms = outcome.sample.offset().delta().as_millis_f64();
    assert!(
        (offset_ms + 7.0).abs() < 2.0f64,
        "offset = {offset_ms} ms（-7ms のはず）"
    );
}

#[test]
fn times_out_when_nobody_answers() {
    // ポートは開いているが応答しない（ICMP 到達不能も返らない）
    let silent = UdpSocket::bind(loopback_any_port()).unwrap();
    let result = once(silent.local_addr().unwrap(), Duration::from_millis(300));
    assert_eq!(result, Err(OnceError::Timeout));
}

#[test]
fn closed_port_is_reported_without_crashing() {
    let addr = {
        let socket = UdpSocket::bind(loopback_any_port()).unwrap();
        socket.local_addr().unwrap()
    }; // ここでソケットを閉じる
    let result = once(addr, Duration::from_millis(300));
    // OS によって「到達不能」か「タイムアウト」になる
    assert!(
        matches!(result, Err(OnceError::Transport(_) | OnceError::Timeout)),
        "{result:?}"
    );
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

#[test]
fn continuous_polling_collects_every_sample() {
    let server = spawn_server(3).unwrap();
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport = UdpTransport::connect(server.local_addr(), Arc::clone(&clock)).unwrap();
    let mut service = SyncService::new(
        SourceConfig::with_poll_interval(TimeDelta::from_millis(20)),
        transport,
        RandomNonce::new(),
        Arc::clone(&clock),
        RequestTemplate::default(),
    );
    let mut recorder = Recorder::default();
    service.run(&mut recorder, Some(10));

    let snapshot = service.snapshot();
    assert_eq!(snapshot.stats.attempts, 10);
    assert_eq!(snapshot.stats.successes, 10, "{:?}", recorder.events);
    assert_eq!(snapshot.state, SyncState::Synchronized);

    // 基準点から推定したサーバ時刻は、PC の壁時計 + 約 3ms
    let now = clock.now();
    let projected = snapshot.anchor.unwrap().project(now.mono).unwrap();
    let diff_ms = projected.wrapping_since(now.wall).as_millis_f64();
    assert!(
        (diff_ms - 3.0).abs() < 2.0f64,
        "diff = {diff_ms} ms（+3ms のはず）"
    );
}
