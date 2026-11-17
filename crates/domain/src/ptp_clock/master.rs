//! 集約ルート [`MasterSource`]（PTP のスレーブとして問い合わせる、ネットワークの PTP マスタ 1 つ）。
//!
//! 既定のプロファイル（E2E）のスレーブの手順のうち、時刻を測るのに要る部分だけを行う。PC の時計は合わせない。
//! - マスタの選び方（簡易な BMCA）: 同じ送り手から Announce が 4 回分の間隔の中に 2 通届いたら候補にし、
//!   priority1・clockClass・clockAccuracy・offsetScaledLogVariance・priority2・グランドマスタの識別子・
//!   stepsRemoved・送り手の識別子の順に小さいものを選ぶ。Announce が 3 回分の間隔届かない候補は外す
//! - 1 回の交換: 選んだマスタの Sync を受け取ったら、すぐに `Delay_Req` を送り、`Delay_Resp` でマスタが受け取った
//!   時刻を知る。two-step のマスタなら、Sync を送った時刻は `Follow_Up` で知る。4 つの時刻から標本を作る
//!   （[`Sample::from_ptp_exchange`]）。Sync の直後に送るのは、その間の PC とマスタの時計の進み方の差を
//!   往復遅延に入れないため
//! - `Delay_Req` の間隔は、利用者の指定とマスタの最小間隔（`Delay_Resp` の logMessageInterval）の長い方。
//!   最初の `Delay_Resp` が届くまでは、マスタの最小間隔を 1 秒（既定のプロファイルの既定値）とみなす。
//!   送るのは Sync を受け取ったときだけなので、間隔は Sync の到着の間隔で測る（揺らぎの分だけ 1/16 の余裕を取る）
//! - Sync はイベント用、`Follow_Up` は一般用のポートに届くので、受け取る順は入れ替わりうる。先に届いた `Follow_Up` は
//!   1 つだけ取っておき、同じ番号の Sync が来たら使う
//! - 時刻の出どころ: t2（Sync の受信）と t3（`Delay_Req` の送信）が、設定した方式（カーネル・NIC）でそろって
//!   取れたときだけその時刻を使う。そろわなければ両方ともユーザ空間の時刻にする（片方だけ違うと offset が偏る）
//! - マスタの時刻は PTP の時刻系（TAI）なので、選んだマスタの Announce の UTC との差を引いて UTC にする
//!   （ARB は換算しない）
//! - マスタが替わっても基準点は捨てない（同じグランドマスタなら時刻は続いている。違えば段差として検出する）。
//!   マスタを見失ったら、同期していたなら自走中として表示を続ける

use core::fmt;

use super::{ClockIdentity, ClockQuality, PhcTime, TimeProperties};
use crate::shared_kernel::{LocalStamp, PcTime, TimeDelta};
use crate::time_sync::{Anchor, ClockTracker, FrequencyOffset, Sample};

/// Announce の受信を数える区間（Announce の間隔の何回分か。IEEE 1588 の `FOREIGN_MASTER_TIME_WINDOW`）
const FOREIGN_MASTER_TIME_WINDOW: i64 = 4;
/// Announce が何回分の間隔届かなければマスタを見失ったとするか（announceReceiptTimeout の既定値）
const ANNOUNCE_RECEIPT_TIMEOUT: i64 = 3;
/// Announce の間隔が分からないときの log2（既定のプロファイルの既定値 2 秒）
const DEFAULT_LOG_ANNOUNCE_INTERVAL: i8 = 1;
/// マスタの `Delay_Req` の最小間隔が分からないときの log2（既定のプロファイルの既定値 1 秒）
const DEFAULT_LOG_MIN_DELAY_REQ_INTERVAL: i8 = 0;
/// 標本が届かなくなってから自走中にするまでの、`Delay_Req` の間隔の回数
const STALE_INTERVALS: i64 = 4;
/// 覚えておくほかのドメインの数
const MAX_OTHER_DOMAINS: usize = 8;

/// 間隔の log2（秒）を時間にする。極端な値は -7（約 8ms）〜 6（64 秒）に収める。
#[must_use]
pub fn log_interval(log: i8) -> TimeDelta {
    let clamped = log.clamp(-7, 6);
    if clamped >= 0 {
        TimeDelta::from_secs(1i64 << clamped.unsigned_abs())
    } else {
        TimeDelta::from_nanos(1_000_000_000 >> clamped.unsigned_abs())
    }
}

/// ポートの識別子（clockIdentity とポート番号）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortIdentity {
    pub clock: ClockIdentity,
    pub port: u16,
}

/// linuxptp と同じ書き方: "001b21.fffe.123456-1"
impl fmt::Display for PortIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.clock, self.port)
    }
}

/// イベントメッセージ（Sync・`Delay_Req`）を送受信した PC の時刻。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventStamp {
    /// 設定した方式（カーネル・NIC のタイムスタンプ）で取れた時刻。ユーザ空間の設定や、取れなかったときは `None`
    pub precise: Option<LocalStamp>,
    /// ユーザ空間で読んだ時刻（受信の直後・送信の直前）
    pub user: LocalStamp,
}

/// マスタが Announce で知らせる情報。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnnounceInfo {
    /// 送り手（マスタのポート）
    pub master: PortIdentity,
    /// Announce を送る間隔の log2（秒）。「指定なし」なら `None`
    pub log_interval: Option<i8>,
    pub grandmaster: ClockIdentity,
    pub priority1: u8,
    pub quality: ClockQuality,
    pub priority2: u8,
    pub steps_removed: u16,
    pub time: TimeProperties,
}

/// 受け取った Sync。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncInfo {
    pub master: PortIdentity,
    pub sequence_id: u16,
    /// Sync を送る間隔の log2（秒）。「指定なし」なら `None`
    pub log_interval: Option<i8>,
    /// one-step なら送った時刻。two-step なら `None`（`Follow_Up` で知らせる）
    pub origin: Option<PhcTime>,
    /// `correctionField`（経路の機器が足した滞留時間など）
    pub correction: TimeDelta,
    pub received: EventStamp,
}

/// 受け取った `Follow_Up`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowUpInfo {
    pub master: PortIdentity,
    pub sequence_id: u16,
    pub precise_origin: PhcTime,
    pub correction: TimeDelta,
}

/// 受け取った `Delay_Resp`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayRespInfo {
    pub master: PortIdentity,
    pub sequence_id: u16,
    /// `Delay_Req` を送ったポート（自分宛てか確かめる）
    pub requesting: PortIdentity,
    pub receive: PhcTime,
    pub correction: TimeDelta,
    /// マスタが求める `Delay_Req` の最小間隔の log2（秒）。「指定なし」なら `None`
    pub log_min_interval: Option<i8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterConfig {
    /// ドメイン番号（表示用。違うドメインのメッセージは application 層が渡さない）
    pub domain: u8,
    /// 自分のポートの識別子（`Delay_Resp` が自分宛てか確かめる）
    pub own: PortIdentity,
    /// `Delay_Req` を送る間隔の下限（利用者の指定）。マスタの最小間隔の方が長ければそちらを使う
    pub request_interval: TimeDelta,
    /// `Delay_Req` を送ってから、`Delay_Resp`（と two-step の `Follow_Up`）を待つ上限。`Delay_Req` の間隔の
    /// 半分の方が短ければそちら（応答のない交換で次の Sync を逃さないように）
    pub response_timeout: TimeDelta,
    /// クロックフィルタが保持する標本の数
    pub filter_len: usize,
}

impl MasterConfig {
    #[must_use]
    pub fn new(domain: u8, own: PortIdentity, request_interval: TimeDelta) -> Self {
        Self {
            domain,
            own,
            request_interval: request_interval.max(TimeDelta::from_millis(10)),
            response_timeout: TimeDelta::from_secs(1),
            filter_len: 8,
        }
    }
}

/// 同期の状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MasterState {
    /// マスタを探している（候補になったマスタがいない）
    Listening,
    /// マスタを選び、最初の標本を待っている
    Uncalibrated,
    /// マスタの時刻を測れている
    Synchronized,
    /// 測れていたが、マスタを見失った・標本が届かなくなった。最後の基準点から進め続ける
    Holdover,
}

impl MasterState {
    /// マスタの時刻を表示してよい状態か。
    #[must_use]
    pub const fn shows_time(self) -> bool {
        matches!(self, Self::Synchronized | Self::Holdover)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MasterStats {
    /// 受け取った Announce（どのマスタのものも）
    pub announces: u64,
    /// 選んだマスタから受け取った Sync
    pub syncs: u64,
    /// 送った `Delay_Req`
    pub requests: u64,
    /// 標本にできた交換
    pub samples: u64,
    /// 失敗した交換（応答がない・送れない・標本にできない）
    pub failures: u64,
    /// 使わなかったメッセージ（ほかのマスタ・ほかのスレーブ宛ての `Delay_Resp`・番号が合わないもの）
    pub ignored: u64,
}

/// 選んだマスタの情報（表示用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasterStatus {
    pub master: PortIdentity,
    pub grandmaster: ClockIdentity,
    pub priority1: u8,
    pub priority2: u8,
    pub grandmaster_quality: ClockQuality,
    pub steps_removed: u16,
    pub time: TimeProperties,
    pub announce_interval: TimeDelta,
    /// Sync を送る間隔（直近の Sync の値）。まだ受け取っていない・「指定なし」なら `None`
    pub sync_interval: Option<TimeDelta>,
    /// two-step（送った時刻を `Follow_Up` で知らせる）か。Sync をまだ受け取っていなければ `None`
    pub two_step: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MasterEvent {
    /// マスタを選んだ（替えた）
    MasterSelected {
        master: PortIdentity,
        grandmaster: ClockIdentity,
    },
    /// マスタを見失った（Announce が届かなくなった）
    MasterLost {
        master: PortIdentity,
    },
    RequestSent {
        seq: u16,
    },
    SampleAccepted {
        seq: u16,
        sample: Sample,
        /// t2 と t3 に設定した方式の時刻を使えたか（偽ならユーザ空間の時刻）
        precise: bool,
    },
    ExchangeFailed {
        seq: u16,
        detail: String,
    },
    /// マスタの時刻とモノトニック時計の関係が段差で変わった（マスタの時刻の修正・UTC との差の変化・
    /// 時刻の違うマスタへの切り替え・PC のスリープなど）。それまでの標本を捨て、新しい時刻に合わせた
    ClockStepDetected {
        step: TimeDelta,
    },
    StateChanged {
        from: MasterState,
        to: MasterState,
    },
}

/// マスタの候補（Announce の送り手 1 つ）。
#[derive(Clone, Copy, Debug)]
struct Candidate {
    announce: AnnounceInfo,
    /// 1 つ前に Announce を受け取った時刻
    previous: Option<PcTime>,
    last: PcTime,
}

impl Candidate {
    fn interval(&self) -> TimeDelta {
        log_interval(
            self.announce
                .log_interval
                .unwrap_or(DEFAULT_LOG_ANNOUNCE_INTERVAL),
        )
    }

    /// 直近の 2 通が、Announce 4 回分の区間に収まっているか。
    fn qualified(&self) -> bool {
        self.previous.is_some_and(|previous| {
            self.last
                .since(previous)
                .is_ok_and(|gap| gap <= self.interval() * FOREIGN_MASTER_TIME_WINDOW)
        })
    }

    /// Announce が 3 回分の間隔届いていないか。
    fn expired(&self, now: PcTime) -> bool {
        now.since(self.last)
            .is_ok_and(|silence| silence > self.interval() * ANNOUNCE_RECEIPT_TIMEOUT)
    }

    /// 小さいほど良い（データセットの比較を簡略にしたもの。比べる順に並べた組）。
    const fn rank(&self) -> (u8, u8, u8, u16, u8, ClockIdentity, u16, PortIdentity) {
        let announce = &self.announce;
        (
            announce.priority1,
            announce.quality.class,
            announce.quality.accuracy,
            announce.quality.variance,
            announce.priority2,
            announce.grandmaster,
            announce.steps_removed,
            announce.master,
        )
    }
}

/// 交換の途中の Sync。
#[derive(Clone, Copy, Debug)]
struct SyncPart {
    sequence_id: u16,
    received: EventStamp,
    /// 補正を足した、Sync を送った時刻（two-step なら `Follow_Up` が届くまで `None`）
    origin: Option<PhcTime>,
    correction: TimeDelta,
}

/// 1 回の交換（Sync・`Delay_Req`・`Delay_Resp`）。
#[derive(Clone, Copy, Debug)]
struct Exchange {
    seq: u16,
    sync: SyncPart,
    /// `Delay_Req` を送った時刻（t3）
    sent: Option<EventStamp>,
    /// 補正を引いた、マスタが `Delay_Req` を受け取った時刻（t4）
    received_by_master: Option<PhcTime>,
    started: PcTime,
}

/// 時刻はすべて引数で受け取り（モノトニック時計）、起きたことを [`MasterEvent`] で返す。
#[derive(Clone, Debug)]
pub struct MasterSource {
    config: MasterConfig,
    state: MasterState,
    candidates: Vec<Candidate>,
    selected: Option<PortIdentity>,
    /// 選んだマスタの直近の Sync（交換に使っていないもの）
    latest_sync: Option<SyncPart>,
    /// Sync より先に届いた `Follow_Up`（同じ番号の Sync が来たら使う）
    early_follow_up: Option<FollowUpInfo>,
    sync_log_interval: Option<i8>,
    two_step: Option<bool>,
    exchange: Option<Exchange>,
    /// 直近の `Delay_Req` に使った Sync を受け取った時刻（間隔を測る起点）
    last_request_sync: Option<PcTime>,
    /// マスタが求める `Delay_Req` の最小間隔の log2（`Delay_Resp` で知る）
    master_min_log: Option<i8>,
    next_seq: u16,
    tracker: ClockTracker,
    last_sample: Option<Sample>,
    last_success: Option<PcTime>,
    /// 直近の交換が失敗した理由。標本を作れたら `None` に戻る
    last_failure: Option<String>,
    stats: MasterStats,
    other_domains: Vec<u8>,
}

impl MasterSource {
    #[must_use]
    pub fn new(config: MasterConfig) -> Self {
        Self {
            config,
            state: MasterState::Listening,
            candidates: Vec::new(),
            selected: None,
            latest_sync: None,
            early_follow_up: None,
            sync_log_interval: None,
            two_step: None,
            exchange: None,
            last_request_sync: None,
            master_min_log: None,
            next_seq: 0,
            tracker: ClockTracker::new(config.filter_len),
            last_sample: None,
            last_success: None,
            last_failure: None,
            stats: MasterStats::default(),
            other_domains: Vec::new(),
        }
    }

    // ---- 参照 ----

    #[must_use]
    pub const fn config(&self) -> &MasterConfig {
        &self.config
    }

    #[must_use]
    pub const fn state(&self) -> MasterState {
        self.state
    }

    #[must_use]
    pub const fn stats(&self) -> MasterStats {
        self.stats
    }

    /// 大きい時計の表示に使う基準点。表示してよい状態（[`MasterState::shows_time`]）のときだけ返す。
    #[must_use]
    pub const fn anchor(&self) -> Option<Anchor> {
        if self.state.shows_time() {
            self.tracker.anchor()
        } else {
            None
        }
    }

    #[must_use]
    pub const fn last_sample(&self) -> Option<Sample> {
        self.last_sample
    }

    /// クロックフィルタが選んだ標本。
    #[must_use]
    pub fn selected_sample(&self) -> Option<Sample> {
        self.tracker.selected()
    }

    /// マスタの時計がモノトニック時計より速く進む割合の推定値。まだ推定できていなければ `None`。
    #[must_use]
    pub const fn frequency(&self) -> Option<FrequencyOffset> {
        self.tracker.frequency()
    }

    /// 最後に標本を作れた時刻（その Sync を受け取った時刻）。
    #[must_use]
    pub const fn last_success(&self) -> Option<PcTime> {
        self.last_success
    }

    /// 直近の交換が失敗した理由（標本を作れたら `None` に戻る）。
    #[must_use]
    pub fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    /// 候補になっているマスタの数（選んだものを含む）。
    #[must_use]
    pub fn candidate_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.qualified())
            .count()
    }

    /// 選んだマスタの情報。
    #[must_use]
    pub fn status(&self) -> Option<MasterStatus> {
        let candidate = self.selected_candidate()?;
        let announce = candidate.announce;
        Some(MasterStatus {
            master: announce.master,
            grandmaster: announce.grandmaster,
            priority1: announce.priority1,
            priority2: announce.priority2,
            grandmaster_quality: announce.quality,
            steps_removed: announce.steps_removed,
            time: announce.time,
            announce_interval: candidate.interval(),
            sync_interval: self.sync_log_interval.map(log_interval),
            two_step: self.two_step,
        })
    }

    /// マスタが求める `Delay_Req` の最小間隔（`Delay_Resp` で知らされたもの）。
    #[must_use]
    pub fn master_min_request_interval(&self) -> Option<TimeDelta> {
        self.master_min_log.map(log_interval)
    }

    /// 実際に使う `Delay_Req` の間隔（利用者の指定とマスタの最小間隔の長い方）。
    #[must_use]
    pub fn request_interval(&self) -> TimeDelta {
        let master = log_interval(
            self.master_min_log
                .unwrap_or(DEFAULT_LOG_MIN_DELAY_REQ_INTERVAL),
        );
        self.config.request_interval.max(master)
    }

    /// 違うドメインのメッセージを受け取ったドメイン番号（小さい順。最大 8 個）。
    #[must_use]
    pub fn other_domains(&self) -> &[u8] {
        &self.other_domains
    }

    /// 交換の応答の期限。交換の途中でなければ `None`。
    #[must_use]
    pub fn response_deadline(&self) -> Option<PcTime> {
        let timeout = self.response_timeout();
        self.exchange
            .map(|exchange| exchange.started.saturating_add(timeout))
    }

    /// 応答を待つ上限（設定の値と、`Delay_Req` の間隔の半分の短い方）。
    fn response_timeout(&self) -> TimeDelta {
        self.config
            .response_timeout
            .min(self.request_interval() / 2)
    }

    // ---- コマンド ----

    /// 違うドメインのメッセージを受け取った（使わない。どのドメインに何かがいるかを知らせるために覚える）。
    pub fn note_other_domain(&mut self, domain: u8) {
        self.stats.ignored += 1;
        if let Err(at) = self.other_domains.binary_search(&domain)
            && self.other_domains.len() < MAX_OTHER_DOMAINS
        {
            self.other_domains.insert(at, domain);
        }
    }

    /// 使わないメッセージを受け取った（ほかの種類など）。
    pub const fn note_ignored(&mut self) {
        self.stats.ignored += 1;
    }

    /// 時間の経過: 届かなくなったマスタを外し、応答の来ない交換を失敗にし、標本が途絶えたら自走中にする。
    pub fn on_tick(&mut self, now: PcTime) -> Vec<MasterEvent> {
        let mut events = Vec::new();
        self.reselect(now, &mut events);
        if let Some(exchange) = self.exchange
            && let Some(deadline) = self.response_deadline()
            && now.is_at_or_after(deadline) == Ok(true)
        {
            let missing = if exchange.sent.is_none() {
                "Delay_Req を送れていません"
            } else if exchange.sync.origin.is_none() {
                "Follow_Up が届きません"
            } else {
                "Delay_Resp が届きません"
            };
            self.fail(exchange.seq, missing.to_owned(), &mut events);
        }
        if self.state == MasterState::Synchronized && self.is_stale(now) {
            self.transition(MasterState::Holdover, &mut events);
        }
        events
    }

    /// Announce を受け取った。
    pub fn announce_received(&mut self, now: PcTime, announce: AnnounceInfo) -> Vec<MasterEvent> {
        self.stats.announces += 1;
        if let Some(candidate) = self
            .candidates
            .iter_mut()
            .find(|candidate| candidate.announce.master == announce.master)
        {
            candidate.previous = Some(candidate.last);
            candidate.last = now;
            candidate.announce = announce;
        } else {
            self.candidates.push(Candidate {
                announce,
                previous: None,
                last: now,
            });
        }
        let mut events = Vec::new();
        self.reselect(now, &mut events);
        events
    }

    /// Sync を受け取った。選んだマスタのものだけを使う。
    pub fn sync_received(&mut self, sync: SyncInfo) {
        if self.selected != Some(sync.master) {
            self.stats.ignored += 1;
            return;
        }
        self.stats.syncs += 1;
        self.sync_log_interval = sync.log_interval;
        self.two_step = Some(sync.origin.is_none());
        let early = self
            .early_follow_up
            .take()
            .filter(|follow_up| follow_up.sequence_id == sync.sequence_id);
        let origin = match (sync.origin, early) {
            (Some(origin), _) => Some(shifted(origin, sync.correction)),
            (None, Some(follow_up)) => Some(shifted(
                follow_up.precise_origin,
                sync.correction + follow_up.correction,
            )),
            (None, None) => None,
        };
        self.latest_sync = Some(SyncPart {
            sequence_id: sync.sequence_id,
            received: sync.received,
            origin,
            correction: sync.correction,
        });
    }

    /// `Follow_Up` を受け取った。交換の途中の Sync（なければ直近の Sync）と番号が合えば、送った時刻にする。
    pub fn follow_up_received(&mut self, follow_up: FollowUpInfo) -> Vec<MasterEvent> {
        if self.selected != Some(follow_up.master) {
            self.stats.ignored += 1;
            return Vec::new();
        }
        let origin = |sync: &SyncPart| {
            shifted(
                follow_up.precise_origin,
                sync.correction + follow_up.correction,
            )
        };
        let mut events = Vec::new();
        if let Some(exchange) = self.exchange.as_mut()
            && exchange.sync.sequence_id == follow_up.sequence_id
            && exchange.sync.origin.is_none()
        {
            exchange.sync.origin = Some(origin(&exchange.sync));
            self.try_complete(&mut events);
        } else if let Some(sync) = self.latest_sync.as_mut()
            && sync.sequence_id == follow_up.sequence_id
            && sync.origin.is_none()
        {
            sync.origin = Some(origin(sync));
        } else {
            // Sync より先に届いた（受け取る順が入れ替わった）かもしれないので、取っておく
            if self.early_follow_up.replace(follow_up).is_some() {
                self.stats.ignored += 1;
            }
        }
        events
    }

    /// 直近の Sync で `Delay_Req` を送る時か。送るなら交換を始め、`Delay_Req` の番号を返す。
    /// Sync を受け取った直後に呼ぶこと（Sync と `Delay_Req` の間が空くと往復遅延が不正確になる）。
    pub fn request_due(&mut self, now: PcTime) -> Option<u16> {
        if self.exchange.is_some() || self.selected.is_none() {
            return None;
        }
        let sync = self.latest_sync?;
        let at = sync.received.user.mono;
        let interval = self.request_interval();
        let due = self.last_request_sync.is_none_or(|last| {
            at.since(last)
                .is_ok_and(|gap| gap >= interval - interval / 16)
        });
        if !due {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.latest_sync = None;
        self.last_request_sync = Some(at);
        self.exchange = Some(Exchange {
            seq,
            sync,
            sent: None,
            received_by_master: None,
            started: now,
        });
        Some(seq)
    }

    /// `Delay_Req` を送った（`sent` は送った時刻 t3）。
    pub fn request_sent(&mut self, seq: u16, sent: EventStamp) -> Vec<MasterEvent> {
        let mut events = Vec::new();
        if let Some(exchange) = self.exchange.as_mut()
            && exchange.seq == seq
        {
            exchange.sent = Some(sent);
            self.stats.requests += 1;
            events.push(MasterEvent::RequestSent { seq });
            self.try_complete(&mut events);
        }
        events
    }

    /// `Delay_Req` を送れなかった。
    pub fn request_failed(&mut self, seq: u16, detail: String) -> Vec<MasterEvent> {
        let mut events = Vec::new();
        if self.exchange.is_some_and(|exchange| exchange.seq == seq) {
            self.fail(seq, detail, &mut events);
        }
        events
    }

    /// `Delay_Resp` を受け取った。自分の `Delay_Req` への、選んだマスタの応答だけを使う。
    pub fn delay_resp_received(&mut self, response: DelayRespInfo) -> Vec<MasterEvent> {
        let mut events = Vec::new();
        let ours = response.requesting == self.config.own && self.selected == Some(response.master);
        match self.exchange.as_mut() {
            Some(exchange)
                if ours
                    && exchange.seq == response.sequence_id
                    && exchange.received_by_master.is_none() =>
            {
                exchange.received_by_master = Some(shifted(response.receive, -response.correction));
                self.master_min_log = response.log_min_interval.or(self.master_min_log);
                self.try_complete(&mut events);
            }
            Some(_) | None => self.stats.ignored += 1,
        }
        events
    }

    // ---- 内部 ----

    fn selected_candidate(&self) -> Option<&Candidate> {
        let selected = self.selected?;
        self.candidates
            .iter()
            .find(|candidate| candidate.announce.master == selected)
    }

    /// 届かなくなった候補を外し、最も良い候補を選び直す。
    fn reselect(&mut self, now: PcTime, events: &mut Vec<MasterEvent>) {
        self.candidates.retain(|candidate| !candidate.expired(now));
        let best = self
            .candidates
            .iter()
            .filter(|candidate| candidate.qualified())
            .min_by_key(|candidate| candidate.rank())
            .map(|candidate| candidate.announce);
        let best_master = best.map(|announce| announce.master);
        if best_master == self.selected {
            return;
        }
        if let Some(previous) = self.selected {
            events.push(MasterEvent::MasterLost { master: previous });
        }
        // 交換の途中のものは、前のマスタのもの
        self.selected = best_master;
        self.latest_sync = None;
        self.early_follow_up = None;
        self.exchange = None;
        self.last_request_sync = None;
        self.master_min_log = None;
        self.sync_log_interval = None;
        self.two_step = None;
        if let Some(announce) = best {
            events.push(MasterEvent::MasterSelected {
                master: announce.master,
                grandmaster: announce.grandmaster,
            });
            if self.state == MasterState::Listening {
                self.transition(MasterState::Uncalibrated, events);
            }
        } else {
            let to = if self.state.shows_time() {
                MasterState::Holdover
            } else {
                MasterState::Listening
            };
            self.transition(to, events);
        }
    }

    /// 4 つの時刻がそろったら標本にする。
    fn try_complete(&mut self, events: &mut Vec<MasterEvent>) {
        let Some(exchange) = self.exchange else {
            return;
        };
        let (Some(origin), Some(sent), Some(received_by_master)) = (
            exchange.sync.origin,
            exchange.sent,
            exchange.received_by_master,
        ) else {
            return;
        };
        self.exchange = None;
        let Some(time) = self
            .selected_candidate()
            .map(|candidate| candidate.announce.time)
        else {
            return;
        };
        let (sync_received, request_sent, precise) =
            match (exchange.sync.received.precise, sent.precise) {
                (Some(sync_received), Some(request_sent)) => (sync_received, request_sent, true),
                (Some(_) | None, _) => (exchange.sync.received.user, sent.user, false),
            };
        match Sample::from_ptp_exchange(
            origin.to_display_time(&time),
            sync_received,
            request_sent,
            received_by_master.to_display_time(&time),
        ) {
            Ok(sample) => {
                self.stats.samples += 1;
                self.last_failure = None;
                self.last_sample = Some(sample);
                self.last_success = Some(sample.received().mono);
                events.push(MasterEvent::SampleAccepted {
                    seq: exchange.seq,
                    sample,
                    precise,
                });
                if let Some(step) = self.tracker.track(sample) {
                    events.push(MasterEvent::ClockStepDetected { step });
                }
                self.transition(MasterState::Synchronized, events);
            }
            Err(error) => self.fail(exchange.seq, error.to_string(), events),
        }
    }

    fn fail(&mut self, seq: u16, detail: String, events: &mut Vec<MasterEvent>) {
        self.exchange = None;
        self.stats.failures += 1;
        self.last_failure = Some(detail.clone());
        events.push(MasterEvent::ExchangeFailed { seq, detail });
    }

    /// 標本が交換の 4 回分の間隔と応答の待ち時間を合わせたより長く届いていないか。
    /// 交換は Sync を受け取ったときにしか始めないので、交換の間隔は `Delay_Req` の間隔と Sync の間隔の長い方。
    fn is_stale(&self, now: PcTime) -> bool {
        let request = self.request_interval();
        let cadence = self
            .sync_log_interval
            .map_or(request, |log| request.max(log_interval(log)));
        let limit = cadence * STALE_INTERVALS + self.response_timeout();
        self.last_success
            .is_some_and(|last| now.since(last).is_ok_and(|silence| silence > limit))
    }

    fn transition(&mut self, to: MasterState, events: &mut Vec<MasterEvent>) {
        if self.state != to {
            events.push(MasterEvent::StateChanged {
                from: self.state,
                to,
            });
            self.state = to;
        }
    }
}

/// PTP の時刻に補正を足す。
fn shifted(time: PhcTime, delta: TimeDelta) -> PhcTime {
    PhcTime::from_nanos(time.nanos() + i128::from(delta.as_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp_clock::{LeapWarning, TimeSourceKind, Timescale};
    use crate::shared_kernel::NtpTimestamp;

    const NANOS_PER_SEC: i128 = 1_000_000_000;
    /// PC の壁時計の起点（2026-11-17 00:00:00 UTC の Unix 秒）
    const WALL_BASE_SECS: i128 = 1_794_873_600;
    /// マスタは PC より 5ms 進んでいる
    const MASTER_AHEAD_US: i64 = 5_000;
    /// 片道の遅延
    const ONE_WAY_US: i64 = 100;
    const OWN: PortIdentity = PortIdentity {
        clock: ClockIdentity([0x02, 0, 0, 0xFF, 0xFE, 0, 0, 1]),
        port: 1,
    };
    const MASTER: PortIdentity = PortIdentity {
        clock: ClockIdentity([0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56]),
        port: 1,
    };

    fn mono(micros: i64) -> PcTime {
        PcTime::monotonic(u64::try_from(micros).unwrap() * 1_000)
    }

    /// PC の時刻 `micros`（モノトニック時計と壁時計は同じだけ進む）。
    fn stamp(micros: i64) -> LocalStamp {
        LocalStamp {
            mono: mono(micros),
            wall: NtpTimestamp::from_unix_nanos(WALL_BASE_SECS * NANOS_PER_SEC)
                .wrapping_add(TimeDelta::from_micros(micros)),
        }
    }

    /// PC の時刻 `micros` の瞬間のマスタの時計（TAI = UTC + 37 秒）。
    fn master_time(micros: i64) -> PhcTime {
        PhcTime::from_nanos(
            (WALL_BASE_SECS + 37) * NANOS_PER_SEC + i128::from(micros + MASTER_AHEAD_US) * 1_000,
        )
    }

    fn time() -> TimeProperties {
        TimeProperties {
            utc_offset: 37,
            utc_offset_valid: true,
            timescale: Timescale::Ptp,
            leap: LeapWarning::None,
            time_traceable: true,
            frequency_traceable: true,
            time_source: TimeSourceKind::Gnss,
        }
    }

    fn announce(master: PortIdentity, priority1: u8) -> AnnounceInfo {
        AnnounceInfo {
            master,
            log_interval: Some(0),
            grandmaster: master.clock,
            priority1,
            quality: ClockQuality {
                class: 6,
                accuracy: 0x21,
                variance: 0x4E5D,
            },
            priority2: 128,
            steps_removed: 0,
            time: time(),
        }
    }

    fn source(request_interval_ms: i64) -> MasterSource {
        MasterSource::new(MasterConfig::new(
            0,
            OWN,
            TimeDelta::from_millis(request_interval_ms),
        ))
    }

    /// Announce を 2 通（0 秒と 1 秒）受け取り、マスタを選んだ状態。
    fn selected(request_interval_ms: i64) -> MasterSource {
        let mut source = source(request_interval_ms);
        assert_eq!(source.announce_received(mono(0), announce(MASTER, 128)), []);
        let events = source.announce_received(mono(1_000_000), announce(MASTER, 128));
        assert_eq!(
            events,
            [
                MasterEvent::MasterSelected {
                    master: MASTER,
                    grandmaster: MASTER.clock,
                },
                MasterEvent::StateChanged {
                    from: MasterState::Listening,
                    to: MasterState::Uncalibrated,
                },
            ]
        );
        source
    }

    fn precise(micros: i64) -> EventStamp {
        // カーネルの時刻はユーザ空間の時刻より 20µs 早い（受信）
        EventStamp {
            precise: Some(stamp(micros)),
            user: stamp(micros + 20),
        }
    }

    /// PC の時刻 `at` に受け取る Sync。
    fn sync(sequence_id: u16, at: i64, two_step: bool) -> SyncInfo {
        SyncInfo {
            master: MASTER,
            sequence_id,
            log_interval: Some(0),
            origin: (!two_step).then(|| master_time(at - ONE_WAY_US)),
            correction: TimeDelta::ZERO,
            received: precise(at),
        }
    }

    fn follow_up(sequence_id: u16, at: i64) -> FollowUpInfo {
        FollowUpInfo {
            master: MASTER,
            sequence_id,
            precise_origin: master_time(at - ONE_WAY_US),
            correction: TimeDelta::ZERO,
        }
    }

    /// PC の時刻 `sent` に送った `Delay_Req` への応答。
    fn delay_resp(sequence_id: u16, sent: i64) -> DelayRespInfo {
        DelayRespInfo {
            master: MASTER,
            sequence_id,
            requesting: OWN,
            receive: master_time(sent + ONE_WAY_US),
            correction: TimeDelta::ZERO,
            log_min_interval: Some(0),
        }
    }

    /// PC の時刻 `at` に Sync を受け取り、200µs 後に `Delay_Req` を送る交換を一通り行う。
    fn exchange(source: &mut MasterSource, sequence_id: u16, at: i64) -> Vec<MasterEvent> {
        source.sync_received(sync(sequence_id, at, true));
        let seq = source.request_due(mono(at)).expect("送る時のはず");
        let mut events = source.request_sent(seq, precise(at + 200));
        events.extend(source.follow_up_received(follow_up(sequence_id, at)));
        events.extend(source.delay_resp_received(delay_resp(seq, at + 200)));
        events
    }

    fn assert_close(actual: TimeDelta, expected: TimeDelta) {
        assert!(
            (actual - expected).abs() <= TimeDelta::from_nanos(10),
            "actual {actual:?}, expected {expected:?}"
        );
    }

    #[test]
    fn needs_two_announces_before_selecting() {
        let mut source = source(1_000);
        drop(source.announce_received(mono(0), announce(MASTER, 128)));
        assert_eq!(source.state(), MasterState::Listening);
        assert_eq!(source.candidate_count(), 0);
        // 選ぶ前の Sync は使わない
        source.sync_received(sync(1, 500_000, true));
        assert_eq!(source.request_due(mono(500_000)), None);
        assert_eq!(source.stats().ignored, 1);
        // 4 回分の間隔（4 秒）より離れた 2 通では候補にならない
        drop(source.announce_received(mono(5_000_000), announce(MASTER, 128)));
        assert_eq!(source.state(), MasterState::Listening);
    }

    #[test]
    fn a_two_step_exchange_measures_the_master() {
        let mut source = selected(1_000);
        let events = exchange(&mut source, 1, 2_000_000);
        let Some(MasterEvent::SampleAccepted {
            seq: 0,
            sample,
            precise: true,
        }) = events.get(1).cloned()
        else {
            panic!("{events:?}");
        };
        assert_eq!(events.first(), Some(&MasterEvent::RequestSent { seq: 0 }));
        assert_eq!(
            events.get(2..),
            Some(
                [MasterEvent::StateChanged {
                    from: MasterState::Uncalibrated,
                    to: MasterState::Synchronized,
                }]
                .as_slice()
            )
        );
        // TAI から 37 秒引いた UTC で、PC より 5ms 進み、往復 200µs
        assert_close(
            sample.offset().delta(),
            TimeDelta::from_micros(MASTER_AHEAD_US),
        );
        assert_close(
            sample.delay().delta(),
            TimeDelta::from_micros(2 * ONE_WAY_US),
        );
        assert_eq!(sample.received(), stamp(2_000_000), "カーネルの時刻を使う");
        assert!(source.anchor().is_some());
        assert_eq!(source.status().unwrap().two_step, Some(true));
        assert_eq!(source.stats().samples, 1);
        assert_eq!(source.last_success(), Some(mono(2_000_000)));
    }

    #[test]
    fn a_one_step_sync_needs_no_follow_up() {
        let mut source = selected(1_000);
        source.sync_received(sync(1, 2_000_000, false));
        let seq = source.request_due(mono(2_000_000)).unwrap();
        drop(source.request_sent(seq, precise(2_000_200)));
        let events = source.delay_resp_received(delay_resp(seq, 2_000_200));
        assert!(
            matches!(events.first(), Some(MasterEvent::SampleAccepted { .. })),
            "{events:?}"
        );
        assert_eq!(source.status().unwrap().two_step, Some(false));
    }

    /// 経路の機器が足した滞留時間: Sync と `Follow_Up` の補正は t1 に足し、`Delay_Resp` の補正は t4 から引く。
    #[test]
    fn corrections_are_applied() {
        let mut source = selected(1_000);
        let mut with_residence = sync(1, 2_000_000, true);
        // Sync は経路で 30µs 滞留した（その分、送った時刻が早く見える。補正は Sync 20µs + Follow_Up 10µs）
        with_residence.correction = TimeDelta::from_micros(20);
        source.sync_received(with_residence);
        let seq = source.request_due(mono(2_000_000)).unwrap();
        drop(source.request_sent(seq, precise(2_000_200)));
        let mut late = follow_up(1, 2_000_000);
        late.precise_origin = shifted(late.precise_origin, TimeDelta::from_micros(-30));
        late.correction = TimeDelta::from_micros(10);
        drop(source.follow_up_received(late));
        let mut response = delay_resp(seq, 2_000_200);
        // Delay_Req は経路で 40µs 滞留した（その分、マスタに遅く届いた）
        response.receive = shifted(response.receive, TimeDelta::from_micros(40));
        response.correction = TimeDelta::from_micros(40);
        let events = source.delay_resp_received(response);
        let Some(MasterEvent::SampleAccepted { sample, .. }) = events.first().cloned() else {
            panic!("{events:?}");
        };
        assert_close(
            sample.offset().delta(),
            TimeDelta::from_micros(MASTER_AHEAD_US),
        );
        assert_close(
            sample.delay().delta(),
            TimeDelta::from_micros(2 * ONE_WAY_US),
        );
    }

    /// `Follow_Up`（一般用のポート）が Sync（イベント用のポート）より先に届いても使う。
    #[test]
    fn a_follow_up_before_its_sync_is_kept() {
        let mut source = selected(1_000);
        assert_eq!(source.follow_up_received(follow_up(1, 2_000_000)), []);
        source.sync_received(sync(1, 2_000_000, true));
        let seq = source.request_due(mono(2_000_000)).unwrap();
        drop(source.request_sent(seq, precise(2_000_200)));
        let events = source.delay_resp_received(delay_resp(seq, 2_000_200));
        let Some(MasterEvent::SampleAccepted { sample, .. }) = events.first().cloned() else {
            panic!("{events:?}");
        };
        assert_close(
            sample.offset().delta(),
            TimeDelta::from_micros(MASTER_AHEAD_US),
        );
        assert_eq!(source.stats().ignored, 0);
        // 番号の違う Sync には使わない
        assert_eq!(source.follow_up_received(follow_up(7, 3_000_000)), []);
        source.sync_received(sync(8, 3_000_000, true));
        assert_eq!(source.latest_sync.and_then(|sync| sync.origin), None);
    }

    #[test]
    fn mixed_timestamp_sources_fall_back_to_user_time() {
        let mut source = selected(1_000);
        source.sync_received(sync(1, 2_000_000, true));
        let seq = source.request_due(mono(2_000_000)).unwrap();
        // 送信の時刻はカーネルから取れなかった
        drop(source.request_sent(
            seq,
            EventStamp {
                precise: None,
                user: stamp(2_000_200),
            },
        ));
        drop(source.follow_up_received(follow_up(1, 2_000_000)));
        let events = source.delay_resp_received(delay_resp(seq, 2_000_200));
        let Some(MasterEvent::SampleAccepted {
            sample, precise, ..
        }) = events.first().cloned()
        else {
            panic!("{events:?}");
        };
        assert!(!precise);
        assert_eq!(
            sample.received(),
            stamp(2_000_020),
            "ユーザ空間の時刻を使う"
        );
    }

    /// 最初は 1 秒（マスタの既定）。`Delay_Resp` で 250ms と知らされたら、利用者の 500ms を使う。
    #[test]
    fn requests_respect_the_masters_minimum_interval() {
        let mut source = selected(500);
        let mut sent = Vec::new();
        // Sync は 250ms ごと（2 秒から）
        for (sequence_id, at) in (0u16..12).zip((8i64..).map(|n| n * 250_000)) {
            source.sync_received(sync(sequence_id, at, false));
            if let Some(seq) = source.request_due(mono(at)) {
                sent.push(at);
                drop(source.request_sent(seq, precise(at + 200)));
                let mut response = delay_resp(seq, at + 200);
                response.log_min_interval = Some(-2);
                drop(source.delay_resp_received(response));
            }
        }
        // 2.0 秒に送り、その応答で最小間隔 250ms を知る。以降は利用者の指定の 500ms ごと
        assert_eq!(
            sent,
            [
                2_000_000, 2_500_000, 3_000_000, 3_500_000, 4_000_000, 4_500_000
            ]
        );
        assert_eq!(source.request_interval(), TimeDelta::from_millis(500));
        assert_eq!(
            source.master_min_request_interval(),
            Some(TimeDelta::from_millis(250))
        );

        // マスタの最小間隔（2 秒）の方が長ければ、そちらを使う
        let mut slow = selected(500);
        slow.sync_received(sync(1, 2_000_000, false));
        let seq = slow.request_due(mono(2_000_000)).unwrap();
        drop(slow.request_sent(seq, precise(2_000_200)));
        let mut response = delay_resp(seq, 2_000_200);
        response.log_min_interval = Some(1);
        drop(slow.delay_resp_received(response));
        assert_eq!(slow.request_interval(), TimeDelta::from_secs(2));
        slow.sync_received(sync(2, 3_000_000, true));
        assert_eq!(slow.request_due(mono(3_000_000)), None);
        slow.sync_received(sync(3, 4_000_000, true));
        assert!(slow.request_due(mono(4_000_000)).is_some());
    }

    #[test]
    fn responses_for_other_slaves_are_ignored() {
        let mut source = selected(1_000);
        source.sync_received(sync(1, 2_000_000, true));
        let seq = source.request_due(mono(2_000_000)).unwrap();
        drop(source.request_sent(seq, precise(2_000_200)));
        drop(source.follow_up_received(follow_up(1, 2_000_000)));
        let mut other = delay_resp(seq, 2_000_200);
        other.requesting.port = 2;
        assert_eq!(source.delay_resp_received(other), []);
        let mut wrong_seq = delay_resp(seq, 2_000_200);
        wrong_seq.sequence_id = seq.wrapping_add(1);
        assert_eq!(source.delay_resp_received(wrong_seq), []);
        assert_eq!(source.stats().ignored, 2);
        // 自分宛ての応答は使える
        assert_ne!(source.delay_resp_received(delay_resp(seq, 2_000_200)), []);
    }

    #[test]
    fn missing_responses_fail_the_exchange() {
        let mut source = selected(1_000);
        source.sync_received(sync(1, 2_000_000, true));
        let seq = source.request_due(mono(2_000_000)).unwrap();
        drop(source.request_sent(seq, precise(2_000_200)));
        // 応答を待つのは、1 秒と間隔の半分（500ms）の短い方
        assert_eq!(source.on_tick(mono(2_499_999)), []);
        assert_eq!(
            source.on_tick(mono(2_500_000)),
            [MasterEvent::ExchangeFailed {
                seq,
                detail: "Follow_Up が届きません".to_owned(),
            }]
        );
        // 次の Sync で次の交換を始められる
        source.sync_received(sync(2, 3_000_000, true));
        let next = source.request_due(mono(3_000_000)).unwrap();
        drop(source.request_sent(next, precise(3_000_200)));
        drop(source.follow_up_received(follow_up(2, 3_000_000)));
        drop(source.announce_received(mono(3_500_000), announce(MASTER, 128)));
        assert_eq!(
            source.on_tick(mono(4_000_000)),
            [MasterEvent::ExchangeFailed {
                seq: next,
                detail: "Delay_Resp が届きません".to_owned(),
            }]
        );
        assert_eq!(source.stats().failures, 2);
    }

    #[test]
    fn losing_the_master_keeps_the_time_in_holdover() {
        let mut source = selected(1_000);
        drop(exchange(&mut source, 1, 2_000_000));
        // Announce は 1 秒ごと。最後（1 秒）から 3 秒より長く届かなければ見失う
        assert!(
            !source
                .on_tick(mono(4_000_000))
                .iter()
                .any(|event| matches!(event, MasterEvent::MasterLost { .. }))
        );
        let events = source.on_tick(mono(4_000_001));
        assert_eq!(
            events,
            [
                MasterEvent::MasterLost { master: MASTER },
                MasterEvent::StateChanged {
                    from: MasterState::Synchronized,
                    to: MasterState::Holdover,
                },
            ]
        );
        assert!(source.anchor().is_some(), "自走中も時刻を出す");

        // 一度も測れないまま見失ったら、探す状態に戻る
        let mut never = selected(1_000);
        let lost = never.on_tick(mono(4_000_001));
        assert_eq!(
            lost.last(),
            Some(&MasterEvent::StateChanged {
                from: MasterState::Uncalibrated,
                to: MasterState::Listening,
            })
        );
    }

    #[test]
    fn a_silent_master_becomes_holdover() {
        let mut source = selected(1_000);
        drop(exchange(&mut source, 1, 2_000_000));
        // Announce は届き続けるが、標本が間隔 4 回分 + 応答の待ち時間 0.5 秒（4.5 秒）より長く届かない
        for at in [
            2_000_000i64,
            3_000_000,
            4_000_000,
            5_000_000,
            6_000_000,
            7_000_000,
        ] {
            drop(source.announce_received(mono(at), announce(MASTER, 128)));
        }
        assert_eq!(source.on_tick(mono(6_500_000)), []);
        assert_eq!(
            source.on_tick(mono(6_500_001)),
            [MasterEvent::StateChanged {
                from: MasterState::Synchronized,
                to: MasterState::Holdover,
            }]
        );
        // 次の標本で同期に戻る
        let events = exchange(&mut source, 2, 7_500_000);
        assert_eq!(
            events.last(),
            Some(&MasterEvent::StateChanged {
                from: MasterState::Holdover,
                to: MasterState::Synchronized,
            })
        );
    }

    /// マスタの最小間隔（62.5ms）も利用者の指定（100ms）も短く、Sync が 1 秒ごとのとき: 標本は Sync ごとにしか
    /// できないので、標本の途絶えは Sync の間隔で数える（`Delay_Req` の間隔で数えると、Sync の合間に自走中になる）。
    #[test]
    fn slow_syncs_do_not_flap_into_holdover() {
        let mut source = selected(100);
        for (sequence_id, at) in [(1u16, 2_000_000i64), (2, 3_000_000)] {
            source.sync_received(sync(sequence_id, at, false));
            let seq = source.request_due(mono(at)).expect("送る時のはず");
            drop(source.request_sent(seq, precise(at + 200)));
            let mut response = delay_resp(seq, at + 200);
            response.log_min_interval = Some(-4);
            let events = source.delay_resp_received(response);
            assert!(
                matches!(events.first(), Some(MasterEvent::SampleAccepted { .. })),
                "{events:?}"
            );
            // 次の Sync（1 秒後）の直前まで同期のまま
            assert_eq!(source.on_tick(mono(at + 990_000)), []);
            assert_eq!(source.state(), MasterState::Synchronized);
        }
        assert_eq!(source.request_interval(), TimeDelta::from_millis(100));
    }

    #[test]
    fn the_better_master_wins() {
        let mut source = selected(1_000);
        let better = PortIdentity {
            clock: ClockIdentity([0x00, 0x1B, 0x21, 0xFF, 0xFE, 0xAB, 0xCD, 0xEF]),
            port: 1,
        };
        drop(source.announce_received(mono(1_500_000), announce(better, 100)));
        assert_eq!(
            source.status().unwrap().master,
            MASTER,
            "1 通では候補にならない"
        );
        let events = source.announce_received(mono(2_500_000), announce(better, 100));
        assert_eq!(
            events,
            [
                MasterEvent::MasterLost { master: MASTER },
                MasterEvent::MasterSelected {
                    master: better,
                    grandmaster: better.clock,
                },
            ]
        );
        assert_eq!(source.candidate_count(), 2);
        // 前のマスタの Sync は使わない
        source.sync_received(sync(1, 2_600_000, true));
        assert_eq!(source.request_due(mono(2_600_000)), None);
    }

    #[test]
    fn other_domains_are_remembered() {
        let mut source = source(1_000);
        for domain in [24u8, 0, 24, 127] {
            source.note_other_domain(domain);
        }
        assert_eq!(source.other_domains(), [0, 24, 127]);
        assert_eq!(source.stats().ignored, 4);
    }

    #[test]
    fn intervals_follow_the_log2_value() {
        assert_eq!(log_interval(0), TimeDelta::from_secs(1));
        assert_eq!(log_interval(1), TimeDelta::from_secs(2));
        assert_eq!(log_interval(-3), TimeDelta::from_millis(125));
        assert_eq!(log_interval(127), TimeDelta::from_secs(64));
        assert_eq!(log_interval(-128), TimeDelta::from_nanos(7_812_500));
    }
}
