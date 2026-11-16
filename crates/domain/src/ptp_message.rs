//! PTP のメッセージのコンテキスト（支援サブドメイン）。IEEE 1588 の時刻のメッセージとバイト列の相互変換。
//!
//! PTP のスレーブとしてマスタに問い合わせるために使う（UDP の 319 番・320 番で送受信するもの）。
//!
//! - 扱うのは Announce・Sync・`Follow_Up`・`Delay_Req`・`Delay_Resp`。ほかの種類（Pdelay・Signaling・管理）は
//!   種類と本体のバイト列だけを読む（[`Body::Other`]）
//! - 管理メッセージ（`ptp_management`）とは別に持つ（共通ヘッダの配置は同じ）。両方を知る必要があるのは
//!   application 層だけなので、コンテキストどうしは参照しない
//! - SNMP や管理メッセージのコンテキストと同じく、不正な値も作れるし、読めるものはそのまま読む。値の意味
//!   （どのマスタを選ぶか、TAI と UTC の換算など）はここでは扱わない（application 層の ACL が `ptp_clock`
//!   コンテキストへ翻訳する）
//! - 読むのはネットワークから届いたバイト列なので、どんな入力でもパニックせず `Err` を返す

mod header;
mod message;

pub use header::{Correction, Flags, Header, PortIdentity, PtpTimestamp};
pub use message::{Announce, Body, DecodeError, MessageType, PtpMessage, decode, encode};
