//! PTP のスレーブの同期サービス: マスタのメッセージを受け取り続けて時刻源（集約 [`MasterSource`]）に渡し、
//! 集約が求めたときに `Delay_Req` を送る。
//!
//! - ドメイン番号が違う・`transportSpecific` が 0 でない（gPTP など）メッセージは使わない（ptp4l と同じく黙って
//!   捨てる）。違うドメインは、どのドメインに何かがいるかを知らせるために番号だけ覚える
//! - Sync を受け取ったら、その場で `Delay_Req` を送るか集約に聞く（Sync との間を空けない）
//! - 受け取った時刻を測定に使うのは、イベント用のポート（319 番）に届いた Sync だけ
//! - `Delay_Req` はマルチキャストで送るとほかのスレーブにも届き、自分にも戻ってくる。マスタだけが使うものなので、
//!   受け取っても数えない
//! - 1 回の受信待ちは最大 100ms（停止の合図を確かめるため）

use alloc::sync::Arc;
use core::fmt;
use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::thread;

use domain::ptp_clock::{
    ClockIdentity, MasterConfig, MasterEvent, MasterSource, MasterState, PortIdentity,
};
use domain::ptp_message::{Body, PtpMessage, decode, encode};
use domain::shared_kernel::{PcTime, TimeDelta};

use crate::acl::ptp_message as acl;
use crate::port::{ClockPort, MasterEventSink, PtpInbound, PtpPort, TransportError};
use crate::snapshot::MasterSnapshot;
use crate::timestamping::{TimestampSource, TimestampingStatus};

/// 1 回の受信待ちの上限。何も起きなくてもこの間隔でループが回る。
const DEFAULT_MAX_WAIT: Duration = Duration::from_millis(100);
/// 回数を決めて測るとき、マスタが見つからないまま諦めるまでの時間
const NO_MASTER_LIMIT: TimeDelta = TimeDelta::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtpSlaveConfig {
    pub domain: u8,
    /// 自分のポートの識別子（[`own_port_identity`] で作る）
    pub own: PortIdentity,
    /// `Delay_Req` を送る間隔の下限（利用者の指定）
    pub request_interval: TimeDelta,
    /// `Delay_Req` にユニキャストのフラグを立てるか（マスタにユニキャストで送るとき）
    pub unicast: bool,
    /// 設定した方式で取れた時刻（[`domain::ptp_clock::EventStamp::precise`]）の出どころ（カーネルか NIC）。
    /// ユーザ空間の設定なら `User`
    pub precise_source: TimestampSource,
}

/// 自分のポートの識別子: 乱数から、ローカルに割り当てた（U/L ビットを立てた）ユニキャストの EUI-64 を作る。
///
/// NIC の MAC から作らないのは、同じ NIC で動く ptp4l と同じ識別子になると、マルチキャストで届く互いの
/// `Delay_Resp` を自分宛てと取り違えるため。ポート番号は 1。
#[must_use]
pub const fn own_port_identity(random: u64) -> PortIdentity {
    let mut clock = random.to_be_bytes();
    clock[0] = (clock[0] | 0x02) & !0x01;
    PortIdentity {
        clock: ClockIdentity(clock),
        port: 1,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PtpOnceError {
    #[error("PTP のマスタが見つかりません（{0}）")]
    NoMaster(String),
    #[error("マスタの時刻を測れません（{0}）")]
    NoSample(String),
    #[error("受信できません: {0}")]
    Transport(String),
}

/// PTP のマスタ 1 つぶんの同期処理。1 つのスレッドが所有して回す。
pub struct PtpSlaveService<P> {
    source: MasterSource,
    port: P,
    clock: Arc<dyn ClockPort>,
    config: PtpSlaveConfig,
    timestamping: Arc<TimestampingStatus>,
    master_address: Option<SocketAddr>,
    receive_error: Option<String>,
}

impl<P> fmt::Debug for PtpSlaveService<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtpSlaveService")
            .field("source", &self.source)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<P: PtpPort> PtpSlaveService<P> {
    pub fn new(
        config: PtpSlaveConfig,
        port: P,
        clock: Arc<dyn ClockPort>,
        timestamping: Arc<TimestampingStatus>,
    ) -> Self {
        let source = MasterSource::new(MasterConfig::new(
            config.domain,
            config.own,
            config.request_interval,
        ));
        Self {
            source,
            port,
            clock,
            config,
            timestamping,
            master_address: None,
            receive_error: None,
        }
    }

    pub const fn source(&self) -> &MasterSource {
        &self.source
    }

    pub fn snapshot(&self) -> MasterSnapshot {
        MasterSnapshot::capture(
            &self.source,
            self.master_address,
            self.receive_error.as_deref(),
        )
    }

    /// 1 ステップ進める: 期限切れの処理 → 最大 `max_wait` 受信を待つ → 受け取ったものを渡す（Sync なら
    /// 必要に応じて `Delay_Req` を送る）。
    pub fn step(&mut self, max_wait: Duration) -> Vec<MasterEvent> {
        let mut events = self.source.on_tick(self.clock.now().mono);
        let wait = self.wait_time(max_wait);
        match self.port.recv(wait) {
            Ok(Some(inbound)) => {
                self.receive_error = None;
                events.extend(self.handle_inbound(&inbound));
            }
            Ok(None) => {}
            Err(error @ (TransportError::Unreachable(_) | TransportError::Io(_))) => {
                self.receive_error = Some(error.to_string());
                // 壊れたソケットで空回りしないよう、待つはずだった時間だけ休む
                thread::sleep(wait);
            }
        }
        for event in &events {
            if let MasterEvent::SampleAccepted { precise, .. } = *event {
                self.timestamping.record_receive(if precise {
                    self.config.precise_source
                } else {
                    TimestampSource::User
                });
            }
        }
        events
    }

    /// 測り続け、起きたことを `sink` に通知する。`stop` が立ったら、最大 100ms ほどで戻る。
    ///
    /// `max_exchanges` を指定すると、その数の交換（標本にできたものと失敗したもの）を終えたら戻る。
    /// そのときは、30 秒たってもマスタが見つからなければ諦めて戻る（マスタがいないと終わらなくなるため）。
    pub fn run_until<S>(&mut self, sink: &mut S, max_exchanges: Option<u64>, stop: &AtomicBool)
    where
        S: MasterEventSink + ?Sized,
    {
        let started = self.clock.now().mono;
        while !stop.load(Ordering::Relaxed) {
            if let Some(max) = max_exchanges {
                let stats = self.source.stats();
                if stats.samples + stats.failures >= max {
                    return;
                }
                let waited = self.clock.now().mono.since(started).unwrap_or_default();
                if self.source.state() == MasterState::Listening && waited > NO_MASTER_LIMIT {
                    return;
                }
            }
            let events = self.step(DEFAULT_MAX_WAIT);
            if events.is_empty() {
                continue;
            }
            let snapshot = self.snapshot();
            for event in &events {
                sink.publish(event, &snapshot);
            }
        }
    }

    /// 最初の標本ができるまで（最大 `timeout`）測る（`once`）。
    ///
    /// # Errors
    ///
    /// 時間内に標本ができなかったとき。理由（マスタが見つからない・応答がないなど）を添える。
    pub fn run_once(&mut self, timeout: Duration) -> Result<MasterSnapshot, PtpOnceError> {
        let deadline = self
            .clock
            .now()
            .mono
            .saturating_add(TimeDelta::from_std(timeout));
        loop {
            let events = self.step(DEFAULT_MAX_WAIT);
            if events
                .iter()
                .any(|event| matches!(*event, MasterEvent::SampleAccepted { .. }))
            {
                return Ok(self.snapshot());
            }
            if self.clock.now().mono.is_at_or_after(deadline) != Ok(false) {
                return Err(self.once_failure());
            }
        }
    }

    /// 時間内に標本ができなかった理由。
    fn once_failure(&self) -> PtpOnceError {
        let stats = self.source.stats();
        if let Some(error) = self.receive_error.as_ref() {
            return PtpOnceError::Transport(error.clone());
        }
        if self.source.state() == MasterState::Listening {
            let domain = self.config.domain;
            let reason = if !self.source.other_domains().is_empty() {
                let others: Vec<String> = self
                    .source
                    .other_domains()
                    .iter()
                    .map(u8::to_string)
                    .collect();
                format!(
                    "ドメイン {domain} のメッセージは届かず、ドメイン {} のメッセージが届いています。--ptp-domain で指定できます",
                    others.join("・")
                )
            } else if stats.announces == 0 {
                format!(
                    "ドメイン {domain} の Announce が届きません。マスタが動いているか、ネットワーク・ファイアウォール・インターフェースの指定を確かめてください"
                )
            } else {
                "Announce は届いていますが、マスタを選べるだけの数（間隔 4 回分の中に 2 通）がまだ届きません".to_owned()
            };
            return PtpOnceError::NoMaster(reason);
        }
        let reason = if stats.syncs == 0 {
            "選んだマスタから Sync が届きません".to_owned()
        } else {
            self.source.last_failure().map_or_else(
                || "時間内に交換が終わりませんでした".to_owned(),
                str::to_owned,
            )
        };
        PtpOnceError::NoSample(reason)
    }

    fn handle_inbound(&mut self, inbound: &PtpInbound) -> Vec<MasterEvent> {
        let Ok(message) = decode(&inbound.payload) else {
            self.source.note_ignored();
            return Vec::new();
        };
        let header = message.header;
        if header.domain != self.config.domain {
            self.source.note_other_domain(header.domain);
            return Vec::new();
        }
        if header.transport_specific != 0 {
            self.source.note_ignored();
            return Vec::new();
        }
        let now = self.clock.now().mono;
        match message.body {
            Body::Announce(announce) => self.source.announce_received(
                inbound.received.user.mono,
                acl::to_announce(&header, &announce),
            ),
            Body::Sync { origin } if inbound.event => {
                let sync = acl::to_sync(&header, origin, inbound.received);
                let from_selected = self
                    .source
                    .status()
                    .is_some_and(|status| status.master == sync.master);
                self.source.sync_received(sync);
                if from_selected {
                    self.master_address = Some(inbound.from);
                }
                self.request_if_due(now)
            }
            Body::FollowUp { precise_origin } => self
                .source
                .follow_up_received(acl::to_follow_up(&header, precise_origin)),
            Body::DelayResp {
                receive,
                requesting,
            } => self
                .source
                .delay_resp_received(acl::to_delay_resp(&header, receive, requesting)),
            // マスタだけが使う（自分の送ったものもマルチキャストで戻ってくる）
            Body::DelayReq { .. } => Vec::new(),
            Body::Sync { .. } | Body::Other { .. } => {
                self.source.note_ignored();
                Vec::new()
            }
        }
    }

    /// `Delay_Req` を送る時なら送る。
    fn request_if_due(&mut self, now: PcTime) -> Vec<MasterEvent> {
        let Some(seq) = self.source.request_due(now) else {
            return Vec::new();
        };
        let request = PtpMessage::delay_req(
            self.config.domain,
            acl::to_wire_identity(self.config.own),
            seq,
            self.config.unicast,
        );
        match self.port.send_event(&encode(&request)) {
            Ok(sent) => self.source.request_sent(seq, sent),
            Err(error) => self.source.request_failed(seq, error.to_string()),
        }
    }

    /// 次に何か（応答の期限）が起きるまで待つ時間。
    fn wait_time(&self, max_wait: Duration) -> Duration {
        let now = self.clock.now().mono;
        self.source
            .response_deadline()
            .and_then(|at| at.since(now).ok())
            .map_or(max_wait, |until| until.to_std_saturating().min(max_wait))
    }
}
