//! サーバからの応答（未検証）。

use crate::shared_kernel::NtpTimestamp;

/// 時刻同期コンテキストが受け取る「サーバからの応答」。まだ検証していない生の値。
///
/// パケット生成コンテキストの `NtpMessage` からの変換は application 層の ACL が行う。
/// 検証は [`super::TimeSource::complete_exchange`] が行う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerReply {
    pub leap: u8,
    pub version: u8,
    pub mode: u8,
    pub stratum: u8,
    pub poll: i8,
    pub precision: i8,
    pub root_delay: u32,
    pub root_dispersion: u32,
    pub reference_id: [u8; 4],
    pub reference_time: NtpTimestamp,
    /// Originate Timestamp（こちらが送った Transmit Timestamp のコピー）
    pub origin: NtpTimestamp,
    /// Receive Timestamp（T2）
    pub receive: NtpTimestamp,
    /// Transmit Timestamp（T3）
    pub transmit: NtpTimestamp,
}
