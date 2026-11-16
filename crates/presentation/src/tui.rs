//! TUI（M2）: 大きい時計と情報欄。
//!
//! 同期は別スレッドの `SyncService` が行う。ここではチャネルで届く [`SyncSnapshot`] を受け取り、
//! 毎フレーム描き直すだけで、集約には触らない。
//!
//! [`SyncSnapshot`]: application::snapshot::SyncSnapshot

mod app;
mod big_digits;
mod screen;
mod snapshot_sender;
mod theme;

pub use app::run;
pub use snapshot_sender::SnapshotSender;

use crate::format::DisplayZone;

/// 画面の設定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TuiConfig {
    /// 最上部の見出し（字間を空けて表示する）
    pub title: String,
    /// 見出しの下に出すサーバ名
    pub server_label: String,
    /// 最下部に出す一文（スポンサー表記など）。`None` なら出さない
    pub footer: Option<String>,
    pub zone: DisplayZone,
    /// 送信方式の表示名（"UDP ソケット" / "raw-ip（…）"）
    pub transport: String,
}
