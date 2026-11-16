//! NTP メッセージのモデル。

use crate::shared_kernel::NtpTimestamp;

/// NTP メッセージ（RFC 5905 のヘッダ 48 バイト + 後続データ）。
///
/// LI / VN / Mode は生の値で持つ。エンコード時に各ビット幅（2 / 3 / 3 bit）へ切り詰める。
/// 不正な組み合わせも作れる（試験用途のため）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NtpMessage {
    /// Leap Indicator（2 bit）
    pub leap: u8,
    /// Version Number（3 bit）
    pub version: u8,
    /// Mode（3 bit）
    pub mode: u8,
    pub stratum: u8,
    /// ポーリング間隔（2 の指数、秒）
    pub poll: i8,
    /// 時計の精度（2 の指数、秒）
    pub precision: i8,
    /// Root Delay（NTP short 形式 16.16）
    pub root_delay: u32,
    /// Root Dispersion（NTP short 形式 16.16）
    pub root_dispersion: u32,
    pub reference_id: [u8; 4],
    pub reference_timestamp: NtpTimestamp,
    pub origin_timestamp: NtpTimestamp,
    pub receive_timestamp: NtpTimestamp,
    pub transmit_timestamp: NtpTimestamp,
    /// ヘッダの後ろに続くバイト列（拡張フィールド・MAC など）。解釈せずそのまま保持する。
    pub trailing: Vec<u8>,
}

impl NtpMessage {
    /// Mode: クライアント
    pub const MODE_CLIENT: u8 = 3;
    /// Mode: サーバ
    pub const MODE_SERVER: u8 = 4;
}

/// クライアント要求の雛形。送信のたびに Transmit Timestamp（ノンス）だけを差し替えて使う。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestTemplate {
    pub leap: u8,
    pub version: u8,
    pub mode: u8,
    pub poll: i8,
    /// ヘッダの後ろに付けるバイト列（拡張フィールドなど）
    pub trailing: Vec<u8>,
}

impl Default for RequestTemplate {
    /// NTPv4 のクライアント要求。Transmit Timestamp 以外はすべて 0。
    fn default() -> Self {
        Self {
            leap: 0,
            version: 4,
            mode: NtpMessage::MODE_CLIENT,
            poll: 0,
            trailing: Vec::new(),
        }
    }
}

impl RequestTemplate {
    /// Transmit Timestamp に `transmit` を入れた要求を作る。
    pub fn build(&self, transmit: NtpTimestamp) -> NtpMessage {
        NtpMessage {
            leap: self.leap,
            version: self.version,
            mode: self.mode,
            poll: self.poll,
            transmit_timestamp: transmit,
            trailing: self.trailing.clone(),
            ..NtpMessage::default()
        }
    }
}
