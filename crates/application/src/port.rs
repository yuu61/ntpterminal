//! ポート: application 層が外側に求める機能。実装は infrastructure（本番）やテスト用の偽物。

use core::time::Duration;

use domain::ptp_clock::{PhcEvent, PhcReading};
use domain::shared_kernel::{LocalStamp, NtpTimestamp};
use domain::time_sync::SyncEvent;

use crate::snapshot::{PhcSnapshot, SyncSnapshot};

pub trait ClockPort: Send + Sync {
    /// モノトニック時計と壁時計を続けて読む。
    fn now(&self) -> LocalStamp;
}

/// 問い合わせごとのノンス（Transmit Timestamp に入れる、予測しにくい値）。
pub trait NonceSource: Send {
    /// 0 以外の値を返す。
    fn next_nonce(&mut self) -> NtpTimestamp;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inbound {
    /// UDP のペイロード（NTP メッセージ）
    pub payload: Vec<u8>,
    /// 受信した時刻（できるだけ受信の瞬間に近い値。T4）
    pub received: LocalStamp,
}

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
    ///
    /// # Errors
    ///
    /// 送信に失敗したとき。
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError>;

    /// 最大 `timeout` まで受信を待つ。時間内に届かなければ `Ok(None)`。
    ///
    /// # Errors
    ///
    /// 受信に失敗したとき（時間切れはエラーにしない）。
    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError>;
}

/// 送信モードを実行時に選べるよう、`Box<dyn PacketPort>` もポートとして使えるようにする。
impl<P: PacketPort + ?Sized> PacketPort for Box<P> {
    fn send(&mut self, payload: &[u8]) -> Result<LocalStamp, TransportError> {
        (**self).send(payload)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Inbound>, TransportError> {
        (**self).recv(timeout)
    }
}

/// ドメインイベントの通知先（CLI 出力・TUI・ログなど）。
pub trait EventSink {
    fn publish(&mut self, event: &SyncEvent, snapshot: &SyncSnapshot);
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PhcReadError(pub String);

/// NIC の時計（PHC）の読み取り。
pub trait PhcPort: Send {
    /// PHC を読む。前後に読んだ PC の時刻（モノトニック時計に写したもの）と一緒に返す。
    ///
    /// # Errors
    ///
    /// PHC を読めないとき、または PC の時計が飛んでモノトニック時計に写せないとき。
    fn read(&mut self) -> Result<PhcReading, PhcReadError>;
}

impl<P: PhcPort + ?Sized> PhcPort for Box<P> {
    fn read(&mut self) -> Result<PhcReading, PhcReadError> {
        (**self).read()
    }
}

/// PHC の時刻源のイベントの通知先。
pub trait PhcEventSink {
    fn publish(&mut self, event: &PhcEvent, snapshot: &PhcSnapshot);
}
