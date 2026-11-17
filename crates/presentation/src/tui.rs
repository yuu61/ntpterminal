//! TUI: 大きい時計・情報欄・測定値の推移・リポジトリの QR コード。
//!
//! 画面の文言は英語（`text` の先頭）。CLI の文言（日本語）とは別に持つ。
//!
//! 同期は別スレッドの `SyncService`（NTP）か `PhcService`（ptp4l が合わせる PHC）か `PtpSlaveService`（PTP の
//! マスタにスレーブとして問い合わせる）が行う。ここではチャネルで届く
//! [`SourceSnapshot`] を受け取り、毎フレーム描き直すだけで、集約には触らない（推移のグラフ用に、届いた測定値を
//! 表示側で貯める）。GNSS 受信機の状態も、別スレッドの `ReceiverMonitor` が [`ReceiverSnapshot`] をチャネルで送ってくる。
//!
//! [`ReceiverSnapshot`]: application::snapshot::ReceiverSnapshot

mod app;
mod big_digits;
mod master;
mod phc;
mod qr;
mod receiver;
mod screen;
mod snapshot_sender;
mod text;
mod theme;
mod trend;
mod user_text;

pub use app::run;
pub use qr::QrCorner;
pub use receiver::mib_label;
pub use snapshot_sender::{MasterSnapshotSender, PhcSnapshotSender, SnapshotSender};
pub use text::{master_server_label, phc_server_label};
pub use user_text::parse_user_text;

use alloc::sync::Arc;

use application::description::{NtpTransportInfo, PhcInfo, PtpTransportInfo};
use application::snapshot::{MasterSnapshot, PhcSnapshot, SyncSnapshot};
use application::timestamping::TimestampingStatus;
use domain::time_sync::Anchor;

use crate::format::DisplayZone;

/// TUI に届くスナップショット（時刻源の種類ごと）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSnapshot {
    Ntp(Box<SyncSnapshot>),
    Phc(Box<PhcSnapshot>),
    Ptp(Box<MasterSnapshot>),
}

/// 中身は参照のまま渡すので、照合は参照のまま行う（`ref` は `ref_patterns` で禁止）。
/// ほかの場所ではこれらの関数を使い、列挙を直接照合しない。
#[expect(
    clippy::pattern_type_mismatch,
    reason = "Box の中身を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
)]
impl SourceSnapshot {
    /// 大きい時計の基準点。
    #[must_use]
    pub fn anchor(&self) -> Option<Anchor> {
        match self {
            Self::Ntp(snapshot) => snapshot.anchor,
            Self::Phc(snapshot) => snapshot.anchor,
            Self::Ptp(snapshot) => snapshot.anchor,
        }
    }

    #[must_use]
    pub const fn ntp(&self) -> Option<&SyncSnapshot> {
        match self {
            Self::Ntp(snapshot) => Some(snapshot),
            Self::Phc(_) | Self::Ptp(_) => None,
        }
    }

    #[must_use]
    pub fn phc(&self) -> Option<&PhcSnapshot> {
        match self {
            Self::Phc(snapshot) => Some(snapshot),
            Self::Ntp(_) | Self::Ptp(_) => None,
        }
    }

    #[must_use]
    pub fn ptp(&self) -> Option<&MasterSnapshot> {
        match self {
            Self::Ptp(snapshot) => Some(snapshot),
            Self::Ntp(_) | Self::Phc(_) => None,
        }
    }
}

/// 表示する時刻源と、その種類だけの表示の設定。
#[derive(Clone, Debug)]
pub enum TuiSource {
    /// NTP サーバ
    Ntp {
        /// GNSS 受信機の行を出すか（SNMP で受信機を読むとき）
        receiver: bool,
        /// 送受信の口
        transport: NtpTransportInfo,
        /// タイムスタンプの取得状況（送受信のスレッドが更新する）
        timestamping: Arc<TimestampingStatus>,
    },
    /// ptp4l が合わせる PHC
    Phc {
        /// 開いた PHC
        phc: PhcInfo,
        /// PHC を使う ptp4l のポートの番号とインターフェース名
        ports: Vec<(u16, String)>,
    },
    /// PTP のスレーブとして問い合わせるマスタ
    Ptp {
        /// 送受信の口
        transport: PtpTransportInfo,
        /// タイムスタンプの取得状況（送受信のスレッドが更新する）
        timestamping: Arc<TimestampingStatus>,
    },
}

#[derive(Clone, Debug)]
pub struct TuiConfig {
    /// 最上部の見出し（字間を空けて表示する）。改行で複数行になり、タブは空白にして出す
    /// （利用者の指定は [`parse_user_text`] でエスケープを解いてから渡す）
    pub title: String,
    /// 見出しの下に出すサーバ名（ptp4l・PTP のマスタでは [`phc_server_label`]・[`master_server_label`]）
    pub server_label: String,
    /// 最下部に出す一文（スポンサー表記など）。`None` なら出さない。改行とタブは `title` と同じ
    pub footer: Option<String>,
    /// 測定値の推移のグラフを出すか（既定では出さない）
    pub trend: bool,
    /// リポジトリの URL の QR コードを置く隅。`None` なら出さない（既定）。ほかの表示と重なるときも出さない
    pub qr: Option<QrCorner>,
    pub zone: DisplayZone,
    pub source: TuiSource,
}
