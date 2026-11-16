//! 時刻同期コンテキスト（コアドメイン）。
//!
//! 集約ルートは [`TimeSource`]（サーバ 1 台につき 1 インスタンス）。
//! I/O を一切持たず、入力（送信・受信・時刻の経過）を受け取ってドメインイベントを返す。
//! 応答の検証ルール（mode・Originate の照合・KoD・LI など）はすべてここにある。

mod anchor;
mod display_clock;
mod event;
mod filter;
mod reply;
mod sample;
mod server_info;
mod source;
mod state;

pub use anchor::Anchor;
pub use display_clock::DisplayClock;
pub use event::{RejectReason, SyncEvent};
pub use filter::ClockFilter;
pub use reply::ServerReply;
pub use sample::{Offset, RoundTripDelay, Sample, SampleError};
pub use server_info::{KissCode, LeapIndicator, ReferenceId, ServerInfo, Stratum};
pub use source::{Deadlines, ExchangeStats, SourceConfig, TimeSource};
pub use state::SyncState;
