//! 実際の UDP ソケット（ループバック）で、疑似 PTP マスタに PTP のスレーブとして問い合わせるテスト。
//!
//! ループバックではマルチキャストを使わないので、疑似マスタは Announce・Sync をスレーブのポートへユニキャストで送り、
//! スレーブは `Delay_Req` をマスタへユニキャストで送る。どちらも 127.0.0.1 に bind する（Windows のファイアウォールの
//! 確認を避けるため）。ポートはテストごとに別の番号を使う（一般用はイベント用の次の番号）。
//! 疑似マスタの Announce と Sync は 0.125 秒ごとにし、マスタを選んで測るまでを 1 秒未満にする。

extern crate alloc;

use alloc::sync::Arc;
use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;

use application::port::{ClockPort, NonceSource as _};
use application::snapshot::MasterSnapshot;
use application::timestamping::TimestampingStatus;
use application::usecase::{PtpSlaveConfig, PtpSlaveService, own_port_identity};
use domain::ptp_clock::{MasterState, TimeSourceKind, Timescale};
use domain::shared_kernel::TimeDelta;
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::sim_ptp_master::{SimPtpMaster, SimPtpMasterConfig};
use infrastructure::transport::{PtpInterface, PtpUdpOptions, TimestampingMode, open_ptp};

/// 疑似マスタの時刻を PC よりこれだけ進める
const MASTER_OFFSET_MS: i64 = 250;

/// 疑似マスタ（`master_port`）を起動し、スレーブ（`slave_port`）で最初の標本ができるまで測る。
fn measure(master_port: u16, slave_port: u16, two_step: bool) -> Result<MasterSnapshot, String> {
    let master = SimPtpMaster::spawn(SimPtpMasterConfig {
        event_port: master_port,
        destination: SocketAddr::from((Ipv4Addr::LOCALHOST, slave_port)),
        interface: Some(Ipv4Addr::LOCALHOST),
        domain: 7,
        offset: TimeDelta::from_millis(MASTER_OFFSET_MS),
        two_step,
        log_sync_interval: -3,
        log_announce_interval: -3,
    })
    .map_err(|error| format!("疑似マスタ: {error}"))?;
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let timestamping = Arc::new(TimestampingStatus::new());
    let opened = open_ptp(
        &PtpUdpOptions {
            interface: Some(PtpInterface::Address(Ipv4Addr::LOCALHOST)),
            event_port: slave_port,
            master: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, master_port))),
            timestamping: TimestampingMode::Auto,
        },
        Arc::clone(&clock),
        Arc::clone(&timestamping),
    )
    .map_err(|error| format!("スレーブ: {error}"))?;
    if !opened.unicast {
        return Err("マスタを指定したのでユニキャストのはず".to_owned());
    }
    let mut service = PtpSlaveService::new(
        PtpSlaveConfig {
            domain: 7,
            own: own_port_identity(RandomNonce::new().next_nonce().to_bits()),
            request_interval: TimeDelta::from_millis(250),
            unicast: opened.unicast,
            precise_source: opened.precise_source,
        },
        opened.port,
        clock,
        timestamping,
    );
    let snapshot = service
        .run_once(Duration::from_secs(10))
        .map_err(|error| error.to_string())?;
    drop(master);
    Ok(snapshot)
}

/// two-step と one-step の疑似マスタを、別々のポートで順に測る。
#[test]
fn measures_sim_masters_over_loopback() {
    for (master_port, slave_port, two_step) in [(41_319, 42_319, true), (41_419, 42_419, false)] {
        let snapshot = measure(master_port, slave_port, two_step).unwrap();
        assert_eq!(snapshot.state, MasterState::Synchronized);
        let sample = snapshot.last_sample.unwrap();
        // 疑似マスタはユーザ空間で時刻を読むので、数 ms の幅を見る
        let offset = sample.offset().delta();
        assert!(
            (offset - TimeDelta::from_millis(MASTER_OFFSET_MS)).abs() < TimeDelta::from_millis(5),
            "offset {offset:?}"
        );
        assert!(
            sample.delay().delta() < TimeDelta::from_millis(5),
            "{:?}",
            sample.delay()
        );
        let master = snapshot.status.unwrap();
        assert_eq!(master.two_step, Some(two_step));
        // 疑似マスタは、何にも合わせていない時計として知らせる
        assert_eq!(master.grandmaster_quality.class, 248);
        assert_eq!(master.time.time_source, TimeSourceKind::InternalOscillator);
        assert_eq!(master.time.utc_offset, 37);
        assert_eq!(master.time.timescale, Timescale::Ptp);
        assert_eq!(
            snapshot.master_address,
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, master_port)))
        );
    }
}
