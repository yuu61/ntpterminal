//! PTP 管理メッセージのコンテキスト（支援サブドメイン）。IEEE 1588 の管理メッセージ（15 章）とバイト列の相互変換。
//!
//! ptp4l（linuxptp）の状態を、その管理用ソケット（Unix ドメインソケット）で読むために使う。
//! `pmc -u` と同じメッセージを送る。
//!
//! - 扱うのは管理メッセージ（`messageType` 0xD）と、その中の TLV 1 つ（`MANAGEMENT`・`MANAGEMENT_ERROR_STATUS`）だけ。
//!   Sync などの PTP の時刻のメッセージは扱わない
//! - データ部（データセット）は、標準の `DEFAULT`・`CURRENT`・`PARENT`・`TIME_PROPERTIES`・`PORT` と、linuxptp 独自の
//!   `PORT_PROPERTIES_NP`・`PORT_HWCLOCK_NP` を読み書きする。配置は linuxptp の `ds.h`・`tlv.h` と同じ
//!   （すべてビッグエンディアン）
//! - SNMP コンテキストと同じく、不正な値も作れるし、読めるものはそのまま読む。値の意味（ポートの状態の名前、
//!   UTC との差の使い方など）はここでは扱わない（application 層の ACL が `ptp_clock` コンテキストへ翻訳する）
//! - 読むのはソケットから届いたバイト列なので、どんな入力でもパニックせず `Err` を返す

mod dataset;
mod message;

pub use dataset::{
    ClockQuality, CurrentDataSet, DefaultDataSet, ParentDataSet, PortDataSet, PortHwclockNp,
    PortPropertiesNp, TimePropertiesDataSet,
};
pub use message::{
    Action, DecodeError, ManagementErrorId, ManagementId, ManagementMessage, ManagementTlv,
    PortIdentity, decode, encode,
};
