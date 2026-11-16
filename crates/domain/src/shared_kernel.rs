//! 共有カーネル: パケット生成と時刻同期のコンテキストで使う時刻まわりの型。
//!
//! このアプリには 3 種類の時刻がある。混ぜてはいけない。
//! 1. サーバ時刻 … [`NtpTimestamp`]（T2, T3）
//! 2. PC のモノトニック時計 … [`PcTime`]（[`Timebase::Monotonic`]）。delay と基準点に使う
//! 3. PC の壁時計 … [`LocalStamp::wall`]（`NtpTimestamp` 形式）。offset の計算にだけ使う

mod local_time;
mod ntp_timestamp;
mod time_delta;

pub use local_time::{LocalStamp, PcTime, Timebase, TimebaseMismatch};
pub use ntp_timestamp::NtpTimestamp;
pub use time_delta::TimeDelta;
