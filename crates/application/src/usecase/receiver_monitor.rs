//! 受信機の監視: タイムサーバの GNSS 受信機の状態を、SNMP v2c の GET で定期的に読む。
//!
//! - 送受信は [`PacketPort`]（宛先は SNMP のポート）。BER の読み書きは domain の SNMP コンテキスト、
//!   値の解釈は ACL（[`crate::acl::mib`]）が担う
//! - MIB を指定しなければ、各 MIB の判定用の値を 1 回の GET でまとめて問い合わせ、応答した MIB を使う。
//!   エラー応答（genErr など。まとめての問い合わせを受け付けないエージェントがある）なら、1 つずつ問い合わせ直す。
//!   使っていた MIB の値が返らなくなったら、次の回にもう一度判定する
//! - MIB を指定したときは判定せず、その MIB の値が返らなければ失敗として知らせる（ほかの MIB に切り替えない）
//! - 応答は request-id と PDU の種類（Response）で照合する。時間切れの後に届いた前の問い合わせの応答は捨てる
//! - 応答を待つ間も、問い合わせの間に眠る間も、100ms ごとに停止の合図を確かめる（TUI を閉じたらすぐ終わるように）
//! - コミュニティ名は秘密なので、`Debug` の出力にもエラーの文言にも入れない

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::thread;

use domain::receiver::ReceiverStatus;
use domain::shared_kernel::{LocalStamp, TimeDelta};
use domain::snmp::{ObjectId, Pdu, PduKind, SnmpMessage, VarBind, decode, encode};

use crate::acl::mib::{self, MibError, MibProfile};
use crate::port::{ClockPort, PacketPort, TransportError};
use crate::snapshot::ReceiverSnapshot;

/// 応答を待つとき・問い合わせの間に眠るとき、停止の合図を確かめる間隔。
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct ReceiverMonitorConfig {
    /// SNMP v2c のコミュニティ名
    pub community: Vec<u8>,
    /// 読む MIB。`None` なら応答する MIB を探す
    pub profile: Option<MibProfile>,
    /// 問い合わせの間隔
    pub interval: Duration,
    /// 1 回の GET で応答を待つ上限
    pub timeout: Duration,
}

/// コミュニティ名は出さない。
impl fmt::Debug for ReceiverMonitorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiverMonitorConfig")
            .field("profile", &self.profile)
            .field("interval", &self.interval)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MonitorError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(
        "SNMP の応答がありません（コミュニティ名・ポート・機器の SNMP の設定を確認してください）"
    )]
    Timeout,
    #[error("SNMP のエラー応答です（error-status {status}、error-index {index}）")]
    ErrorStatus { status: i64, index: i64 },
    #[error("対応している MIB（Meinberg・Microchip S650）が見つかりません")]
    UnknownMib,
    #[error(transparent)]
    Mib(#[from] MibError),
    /// 停止の合図で、応答を待つのをやめた
    #[error("停止しました")]
    Stopped,
}

/// 受信機 1 台ぶんの監視。1 つのスレッドが所有して回す。
pub struct ReceiverMonitor<P> {
    port: P,
    clock: Arc<dyn ClockPort>,
    config: ReceiverMonitorConfig,
    /// 使っている MIB（指定されたもの、または判定したもの）
    profile: Option<MibProfile>,
    last_request_id: i32,
    snapshot: ReceiverSnapshot,
    /// 停止の合図（[`Self::run_until`] で受け取る）
    stop: Arc<AtomicBool>,
}

impl<P> fmt::Debug for ReceiverMonitor<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiverMonitor")
            .field("config", &self.config)
            .field("profile", &self.profile)
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}

impl<P: PacketPort> ReceiverMonitor<P> {
    pub fn new(config: ReceiverMonitorConfig, port: P, clock: Arc<dyn ClockPort>) -> Self {
        let profile = config.profile;
        Self {
            port,
            clock,
            config,
            profile,
            last_request_id: 0,
            snapshot: ReceiverSnapshot {
                profile,
                ..ReceiverSnapshot::default()
            },
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub const fn snapshot(&self) -> &ReceiverSnapshot {
        &self.snapshot
    }

    /// 1 回問い合わせて、スナップショットを更新する。
    pub fn poll(&mut self) -> &ReceiverSnapshot {
        self.snapshot.attempts += 1;
        match self.read_status() {
            Ok(status) => {
                self.snapshot.status = Some(status);
                self.snapshot.last_success = Some(self.clock.now().mono);
                self.snapshot.last_error = None;
                self.snapshot.successes += 1;
            }
            Err(error) => {
                if matches!(error, MonitorError::Mib(_)) && self.config.profile.is_none() {
                    // 判定した MIB の値が返らなくなった。次の回にもう一度判定する
                    self.profile = None;
                }
                self.snapshot.last_error = Some(error);
            }
        }
        self.snapshot.profile = self.profile;
        &self.snapshot
    }

    /// 間隔ごとに問い合わせ、そのたびに `publish` にスナップショットを渡す。`stop` が立ったら終わる。
    /// 応答を待つ間も眠る間も 100ms ごとに `stop` を確かめるので、立ててから 100ms ほどで戻る。
    pub fn run_until<F>(&mut self, publish: &mut F, stop: &Arc<AtomicBool>)
    where
        F: FnMut(&ReceiverSnapshot),
    {
        self.stop = Arc::clone(stop);
        while !stop.load(Ordering::Relaxed) {
            let started = self.clock.now().mono;
            let snapshot = self.poll();
            if stop.load(Ordering::Relaxed) {
                // 応答待ちを途中でやめた結果は知らせない
                return;
            }
            publish(snapshot);
            let next = started.saturating_add(TimeDelta::from_std(self.config.interval));
            while !stop.load(Ordering::Relaxed) {
                let remaining = next.since(self.clock.now().mono).unwrap_or(TimeDelta::ZERO);
                if remaining <= TimeDelta::ZERO {
                    break;
                }
                thread::sleep(remaining.to_std_saturating().min(STOP_CHECK_INTERVAL));
            }
        }
    }

    fn read_status(&mut self) -> Result<ReceiverStatus, MonitorError> {
        let profile = if let Some(known) = self.profile {
            known
        } else {
            let detected = self.detect()?;
            self.profile = Some(detected);
            detected
        };
        let varbinds = self.get(&profile.status_oids())?;
        Ok(profile.interpret(&varbinds)?)
    }

    fn detect(&mut self) -> Result<MibProfile, MonitorError> {
        let probes: Vec<ObjectId> = MibProfile::DETECTION_ORDER
            .iter()
            .map(|profile| profile.probe_oid())
            .collect();
        match self.get(&probes) {
            Ok(varbinds) => mib::detect(&varbinds).ok_or(MonitorError::UnknownMib),
            Err(MonitorError::ErrorStatus { .. }) => self.detect_one_by_one(),
            Err(other) => Err(other),
        }
    }

    fn detect_one_by_one(&mut self) -> Result<MibProfile, MonitorError> {
        for profile in MibProfile::DETECTION_ORDER {
            match self.get(&[profile.probe_oid()]) {
                Ok(varbinds) if mib::detect(&varbinds) == Some(profile) => return Ok(profile),
                Ok(_) | Err(MonitorError::ErrorStatus { .. }) => {}
                Err(other) => return Err(other),
            }
        }
        Err(MonitorError::UnknownMib)
    }

    /// GET を送り、同じ request-id の Response を待つ。
    fn get(&mut self, names: &[ObjectId]) -> Result<Vec<VarBind>, MonitorError> {
        let request_id = self.next_request_id();
        let request =
            SnmpMessage::get_request(&self.config.community, i64::from(request_id), names);
        let _: LocalStamp = self.port.send(&encode(&request))?;
        let deadline = self
            .clock
            .now()
            .mono
            .saturating_add(TimeDelta::from_std(self.config.timeout));
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(MonitorError::Stopped);
            }
            let remaining = deadline
                .since(self.clock.now().mono)
                .unwrap_or(TimeDelta::ZERO);
            if remaining <= TimeDelta::ZERO {
                return Err(MonitorError::Timeout);
            }
            let wait = remaining.to_std_saturating().min(STOP_CHECK_INTERVAL);
            let Some(inbound) = self.port.recv(wait)? else {
                continue;
            };
            // 読めないもの・前の問い合わせの応答は捨てて、待ち続ける
            let Ok(message) = decode(&inbound.payload) else {
                continue;
            };
            if message.pdu.kind != PduKind::Response
                || message.pdu.request_id != i64::from(request_id)
            {
                continue;
            }
            if message.pdu.error_status != Pdu::NO_ERROR {
                return Err(MonitorError::ErrorStatus {
                    status: message.pdu.error_status,
                    index: message.pdu.error_index,
                });
            }
            return Ok(message.pdu.varbinds);
        }
    }

    /// 1 から `i32::MAX` まで順に使い、最後まで来たら 1 に戻る。
    const fn next_request_id(&mut self) -> i32 {
        self.last_request_id = if self.last_request_id == i32::MAX {
            1i32
        } else {
            self.last_request_id + 1i32
        };
        self.last_request_id
    }
}
