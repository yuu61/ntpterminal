//! PTP の時計のコンテキスト: ptp4l（linuxptp）が合わせる NIC の時計（PHC）を、時刻源として表示する。
//!
//! 集約ルートは [`PhcSource`]（PHC 1 つ）。PHC の読み取り（[`PhcReading`]）と ptp4l の状態（[`PtpStatus`]）を
//! 受け取り、表示する時刻の基準点と、同期の状態（[`PhcState`]）を決める。
//!
//! - ptp4l の状態は、管理メッセージ（`ptp_management` コンテキスト）で読んだ値を application 層の ACL が
//!   翻訳したもの。ここは管理メッセージを知らない
//! - PHC の時刻は PTP の時刻系（TAI）なので、ptp4l が知らせる UTC との差（currentUtcOffset）を引いて UTC にする。
//!   時刻系が ARB（任意）なら換算しない（[`Timescale`]）
//! - 基準点を作る処理（段差の確認・クロックフィルタ・周波数の推定）は NTP と同じ
//!   [`ClockTracker`](crate::time_sync::ClockTracker) を使う。PHC の読み取りは、前後に読んだ PC の時刻を
//!   T1・T4、PHC の時刻を T2 = T3 とする標本にする（delay は読み取りの前後の幅、offset は PHC − PC の壁時計）

mod phc_time;
mod source;
mod status;

pub use phc_time::{PhcReading, PhcTime};
pub use source::{PhcConfig, PhcEvent, PhcSource, PhcState, PhcStats};
pub use status::{
    ClockIdentity, ClockQuality, ClockRole, LeapWarning, PortState, PortStatus, PtpStatus,
    TimeProperties, TimeSourceKind, Timescale,
};
