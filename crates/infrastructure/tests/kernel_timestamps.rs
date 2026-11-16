//! カーネルのタイムスタンプ（SO_TIMESTAMPING、ソフトウェア）の結合テスト。Linux のみ・権限不要。
//!
//! 「問い合わせが成功した」だけでは、ユーザ空間の時刻に黙って戻っていても通ってしまう。
//! 取得状況のカウンタで、T1 と T4 の両方にカーネルの時刻が使われたことを確かめる。
#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use application::port::{ClockPort, EventSink};
use application::snapshot::SyncSnapshot;
use application::timestamping::TimestampingStatus;
use application::usecase::{SyncService, query_once};
use domain::packet_crafting::RequestTemplate;
use domain::shared_kernel::TimeDelta;
use domain::time_sync::{SourceConfig, SyncEvent};
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::sim_server::{SimServer, SimServerConfig};
use infrastructure::transport::{self, TimestampingMode, TransportMode, TransportOptions};

fn spawn_server(offset_ms: i64) -> SimServer {
    SimServer::spawn(
        "127.0.0.1:0".parse().unwrap(),
        SimServerConfig {
            offset: TimeDelta::from_millis(offset_ms),
            ..SimServerConfig::default()
        },
    )
    .unwrap()
}

fn open(
    server: SocketAddr,
    timestamping: TimestampingMode,
    clock: &Arc<dyn ClockPort>,
    status: &Arc<TimestampingStatus>,
) -> Box<dyn application::port::PacketPort> {
    transport::open(
        TransportMode::Udp,
        server,
        TransportOptions {
            timestamping,
            ..TransportOptions::default()
        },
        Arc::clone(clock),
        Arc::clone(status),
    )
    .unwrap()
    .port
}

#[test]
fn query_uses_kernel_time_for_both_send_and_receive() {
    let server = spawn_server(5);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let status = Arc::new(TimestampingStatus::new());
    let port = open(
        server.local_addr(),
        TimestampingMode::Kernel,
        &clock,
        &status,
    );

    let outcome = query_once(
        port,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    let offset_ms = outcome.sample.offset().delta().as_millis_f64();
    assert!(
        (offset_ms - 5.0).abs() < 2.0,
        "offset = {offset_ms} ms（+5ms のはず）"
    );

    let counts = status.counts();
    assert!(counts.kernel_enabled);
    assert_eq!(counts.kernel_samples, 1, "{counts:?}");
    assert_eq!(counts.user_samples, 0, "{counts:?}");
    assert_eq!(counts.tx_misses, 0, "送信時刻が取れていない: {counts:?}");
    assert!(counts.last_was_kernel);
}

struct Ignore;

impl EventSink for Ignore {
    fn publish(&mut self, _event: &SyncEvent, _snapshot: &SyncSnapshot) {}
}

#[test]
fn continuous_polling_keeps_using_kernel_time() {
    let server = spawn_server(0);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let status = Arc::new(TimestampingStatus::new());
    let port = open(
        server.local_addr(),
        TimestampingMode::Kernel,
        &clock,
        &status,
    );
    let mut service = SyncService::new(
        SourceConfig::with_poll_interval(TimeDelta::from_millis(20)),
        port,
        RandomNonce::new(),
        Arc::clone(&clock),
        RequestTemplate::default(),
    );
    service.run(&mut Ignore, Some(20));

    assert_eq!(service.snapshot().stats.successes, 20);
    let counts = status.counts();
    assert_eq!(counts.kernel_samples, 20, "{counts:?}");
    assert_eq!(counts.tx_misses, 0, "{counts:?}");
}

#[test]
fn user_mode_leaves_kernel_timestamps_off() {
    let server = spawn_server(0);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let status = Arc::new(TimestampingStatus::new());
    let port = open(server.local_addr(), TimestampingMode::User, &clock, &status);
    query_once(
        port,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(!status.counts().kernel_enabled);
}
