//! 時刻同期コンテキスト（コアドメイン）。
//!
//! 集約ルートは [`TimeSource`]（サーバ 1 台につき 1 インスタンス）。
//! I/O を一切持たず、入力（送信・受信・時刻の経過）を受け取ってドメインイベントを返す。
//! 応答の検証ルール（mode・Originate の照合・KoD・LI など）はすべてここにある。
//! 表示する時刻は、クロックフィルタが選んだ標本（[`ClockFilter`]）から作る基準点（[`Anchor`]）に、
//! 時計の進み方の差（[`FrequencyEstimator`]）の補正をかけて求める。時刻の段差は `continuity` で検出する。

mod anchor;
mod continuity;
mod display_clock;
mod event;
mod filter;
mod frequency;
mod reply;
mod sample;
mod server_info;
mod source;
mod state;

pub use anchor::Anchor;
pub use display_clock::DisplayClock;
pub use event::{RejectReason, SyncEvent};
pub use filter::ClockFilter;
pub use frequency::{FrequencyEstimator, FrequencyOffset};
pub use reply::ServerReply;
pub use sample::{Offset, RoundTripDelay, Sample, SampleError};
pub use server_info::{KissCode, LeapIndicator, ReferenceId, ServerInfo, Stratum};
pub use source::{Deadlines, ExchangeStats, SourceConfig, TimeSource};
pub use state::SyncState;
