//! 本物の ptp4l（linuxptp）と PHC を使う確認（Linux のみ・`#[ignore]`）。手順は docs/testing.md。
//!
//! - `NTPTERMINAL_PTP4L_SOCKET`（ptp4l の `uds_address`）と `NTPTERMINAL_PTP_DOMAIN`（省略時 0）: ptp4l に問い合わせ、
//!   ポートの一覧と状態を読む。ptp4l がマスタを選ぶ（SLAVE か UNCALIBRATED）まで最大 60 秒待つ。`pmc -u -b 0` の表示と見比べるため、読んだ値を出す
//! - `NTPTERMINAL_PHC`（/dev/ptpN の N）: PHC を読み、PC の時計との関係が正しいか確かめる（`/dev/ptpN` を読む権限が要る）
//!
//! 管理用ソケットの応答は抽象名前空間の名前に届くので、ptp4l と同じネットワーク名前空間で実行すること。
#![cfg(target_os = "linux")]

extern crate alloc;

use alloc::sync::Arc;
use core::time::Duration;
use std::path::PathBuf;
use std::{env, process, thread};

use application::port::ClockPort;
use application::usecase::{Ptp4lClient, Ptp4lClientConfig};
use domain::ptp_clock::ClockRole;
use domain::shared_kernel::TimeDelta;
use infrastructure::clock::SystemClock;
use infrastructure::ptp4l;

#[test]
#[ignore = "ptp4l が要る（NTPTERMINAL_PTP4L_SOCKET）"]
fn reads_a_real_ptp4l() {
    let socket = PathBuf::from(env::var("NTPTERMINAL_PTP4L_SOCKET").unwrap());
    let domain = env::var("NTPTERMINAL_PTP_DOMAIN").map_or(0, |text| text.parse().unwrap());
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let connection = ptp4l::connect(Some(&socket), Arc::clone(&clock)).unwrap();
    let mut client = Ptp4lClient::new(
        connection.port,
        clock,
        Ptp4lClientConfig {
            domain,
            timeout: Duration::from_secs(1),
            client_id: process::id(),
        },
    );
    let ports = client.discover_ports().unwrap();
    println!("ポート: {ports:#?}");
    assert_ne!(ports.len(), 0, "ポートがない");
    let numbers: Vec<u16> = ports.iter().map(|port| port.number).collect();
    for _ in 0u8..60 {
        let status = client.status(&numbers).unwrap();
        println!(
            "状態: {:?}  マスタとの差 {:?}  経路遅延 {:?}  GM {}  clockClass {}  UTC との差 {}（確か: {}）  {:?}",
            status.ports,
            status.offset_from_master,
            status.mean_path_delay,
            status.grandmaster,
            status.grandmaster_quality.class,
            status.time.utc_offset,
            status.time.utc_offset_valid,
            status.time.timescale,
        );
        // free_running の ptp4l（時計を合わせない）は SLAVE にならず、マスタを選んだ UNCALIBRATED のまま
        if matches!(status.role(), ClockRole::Slave | ClockRole::Uncalibrated) {
            assert_eq!(status.domain, domain);
            assert_ne!(
                status.steps_removed, 0,
                "マスタを選んだのに stepsRemoved が 0"
            );
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
    panic!("60 秒待ってもマスタを選びませんでした");
}

#[test]
#[ignore = "PHC が要る（NTPTERMINAL_PHC）"]
fn reads_a_real_phc() {
    let index: u32 = env::var("NTPTERMINAL_PHC").unwrap().parse().unwrap();
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let mut opened = ptp4l::open_phc(index, Arc::clone(&clock)).unwrap();
    println!("{}（{}）", opened.path, opened.method);
    let mut previous = None;
    for _ in 0u8..5 {
        let reading = opened.port.read().unwrap();
        let width = reading.after.mono.since(reading.before.mono).unwrap();
        assert!(width >= TimeDelta::ZERO, "前後が逆: {reading:?}");
        assert!(
            width < TimeDelta::from_millis(1),
            "読み取りの幅が大きすぎる: {width:?}"
        );
        // PHC − PC の壁時計（PHC は TAI のことも、任意の時刻のこともあるので、値は表示するだけ）
        let wall = reading.before.wall.to_unix_nanos();
        println!(
            "PHC − PC の壁時計 {:.6} 秒、読み取りの幅 {width:?}",
            (reading.phc.nanos() - wall) as f64 / 1e9f64
        );
        if let Some((before_mono, before_phc)) = previous {
            // PHC はモノトニック時計とほぼ同じ速さで進む（0.1% 以内）
            let mono = reading.before.mono.since(before_mono).unwrap().as_nanos();
            let phc = i64::try_from(reading.phc.nanos() - before_phc).unwrap();
            assert!(
                (phc - mono).abs() <= mono / 1_000 + 10_000,
                "PHC {phc} ns / PC {mono} ns"
            );
        }
        previous = Some((reading.before.mono, reading.phc.nanos()));
        thread::sleep(Duration::from_millis(100));
    }
}
