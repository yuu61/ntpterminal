//! ポート: application 層が外側に求める機能。実装は infrastructure（本番）やテスト用の偽物。

use std::time::Duration;

use domain::shared_kernel::{LocalStamp, NtpTimestamp};
use domain::time_sync::SyncEvent;

use crate::snapshot::SyncSnapshot;

/// PC の時計。
pub trait ClockPort: Send + Sync {
    /// モノトニック時計と壁時計を続けて読む。
    fn now(&self) -> LocalStamp;
}

/// 問い合わせごとのノンス（Transmit Timestamp に入れる、予測しにくい値）。
pub trait NonceSource: Send {
    /// 0 以外の値を返す。
    fn next_nonce(&mut self) -> NtpTimestamp;
}

/// 受信したパケット。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inbound {
    /// UDP のペイロード（NTP メッセージ）
    pub payload: Vec<u8>,
    /// 受信した時刻（できるだけ受信の瞬間に近い値。T4）
    pub received: LocalStamp,
}

/// 送受信のエラー。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// 宛先に到達できない（ICMP 到達不能を受け取ったなど）。1 回の問い合わせの失敗として扱う。
    #[error("宛先に到達できません: {0}")]
    Unreachable(String),
    #[error("通信エラー: {0}")]
    Io(String),
}

/// パケットの送受信。接続先（サーバ）は実装側が持つ。
pub trait PacketPort: Send {
    /// ペイロードを送る。送る直前の時刻（T1）を返す。
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError>;

    /// 最大 `timeout` まで受信を待つ。時間内に届かなければ `Ok(None)`。
    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError>;
}

/// ドメインイベントの通知先（CLI 出力・TUI・ログなど）。
pub trait EventSink {
    fn publish(&mut self, event: &SyncEvent, snapshot: &SyncSnapshot);
}
