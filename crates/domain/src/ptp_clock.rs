//! PTP の時計のコンテキスト: PTP の時計を時刻源として表示する。時刻源は 2 種類ある。
//!
//! - [`PhcSource`]: ptp4l（linuxptp）が合わせる NIC の時計（PHC）1 つ。PHC の読み取り（[`PhcReading`]）と
//!   ptp4l の状態（[`PtpStatus`]）を受け取り、表示する時刻の基準点と、同期の状態（[`PhcState`]）を決める
//! - [`MasterSource`]: PTP のスレーブとして問い合わせる、ネットワークの PTP マスタ。Announce・Sync・`Follow_Up`・
//!   `Delay_Resp` の内容を受け取り、マスタを選び、`Delay_Req` を送る時を決め、基準点と同期の状態（[`MasterState`]）を
//!   決める（手順は `master` の先頭）
//!
//! - ptp4l の状態は、管理メッセージ（`ptp_management` コンテキスト）で読んだ値を application 層の ACL が
//!   翻訳したもの。マスタのメッセージも `ptp_message` コンテキストで読んだ値の翻訳。ここはどちらも知らない
//! - PHC やマスタの時刻は PTP の時刻系（TAI）なので、ptp4l・マスタが知らせる UTC との差（currentUtcOffset）を
//!   引いて UTC にする。
//!   時刻系が ARB（任意）なら換算しない（[`Timescale`]）
//! - 基準点を作る処理（段差の確認・クロックフィルタ・周波数の推定）は NTP と同じ
//!   [`ClockTracker`](crate::time_sync::ClockTracker) を使う。PHC の読み取りは、前後に読んだ PC の時刻を
//!   T1・T4、PHC の時刻を T2 = T3 とする標本にする（delay は読み取りの前後の幅、offset は PHC − PC の壁時計）

mod master;
mod phc_time;
mod source;
mod status;

pub use master::{
    AnnounceInfo, DelayRespInfo, EventStamp, FollowUpInfo, MasterConfig, MasterEvent, MasterSource,
    MasterState, MasterStats, MasterStatus, PortIdentity, SyncInfo, log_interval,
};
pub use phc_time::{PhcReading, PhcTime};
pub use source::{PhcConfig, PhcEvent, PhcSource, PhcState, PhcStats};
pub use status::{
    ClockIdentity, ClockQuality, ClockRole, LeapWarning, PortState, PortStatus, PtpStatus,
    TimeProperties, TimeSourceKind, Timescale,
};
