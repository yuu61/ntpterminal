use core::fmt::{self, Write as _};
use core::net::Ipv4Addr;

use crate::shared_kernel::{NtpTimestamp, TimeDelta};

/// Leap Indicator（うるう秒の予告）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeapIndicator {
    NoWarning,
    /// その日の最後の 1 分が 61 秒になる
    InsertSecond,
    /// その日の最後の 1 分が 59 秒になる
    DeleteSecond,
    /// サーバ自身が同期していない（アラーム）
    Unsynchronized,
}

impl LeapIndicator {
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0 => Self::NoWarning,
            1 => Self::InsertSecond,
            2 => Self::DeleteSecond,
            _ => Self::Unsynchronized,
        }
    }
}

/// Stratum（基準時計からの段数）。1 = GNSS などの基準時計に直結したサーバ。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stratum(u8);

impl Stratum {
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_primary(self) -> bool {
        self.0 == 1
    }
}

/// Kiss-o'-Death のコード（stratum 0 の応答で Reference ID に入る 4 文字）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KissCode {
    /// DENY: アクセス拒否。問い合わせを止める
    Deny,
    /// RSTR: アクセス制限。問い合わせを止める
    Restrict,
    /// RATE: 問い合わせが多すぎる。間隔を延ばす
    RateExceeded,
    Other([u8; 4]),
}

impl KissCode {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 4]) -> Self {
        match &bytes {
            b"DENY" => Self::Deny,
            b"RSTR" => Self::Restrict,
            b"RATE" => Self::RateExceeded,
            _ => Self::Other(bytes),
        }
    }
}

impl fmt::Display for KissCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Deny => f.write_str("DENY"),
            Self::Restrict => f.write_str("RSTR"),
            Self::RateExceeded => f.write_str("RATE"),
            Self::Other(bytes) => write_ascii(f, bytes),
        }
    }
}

/// Reference ID の解釈。stratum によって意味が変わる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceId {
    /// stratum 1: 基準時計の種類（"GNSS", "GPS", "PPS" など。4 文字の ASCII）
    Source([u8; 4]),
    /// stratum 2 以上: 上位サーバの IPv4 アドレス（上位が IPv6 の場合はハッシュ値）
    Upstream(Ipv4Addr),
}

impl ReferenceId {
    #[must_use]
    pub fn interpret(stratum: Stratum, raw: [u8; 4]) -> Self {
        if stratum.value() <= 1 {
            Self::Source(raw)
        } else {
            Self::Upstream(Ipv4Addr::from(raw))
        }
    }
}

impl fmt::Display for ReferenceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Source(bytes) => write_ascii(f, bytes),
            Self::Upstream(addr) => write!(f, "{addr}"),
        }
    }
}

/// 4 文字の ASCII を表示する。末尾の NUL は除き、表示できない文字は '?' にする。
fn write_ascii(formatter: &mut fmt::Formatter<'_>, bytes: [u8; 4]) -> fmt::Result {
    for &byte in bytes.iter().take_while(|&&byte| byte != 0) {
        let shown = if byte.is_ascii_graphic() || byte == b' ' {
            char::from(byte)
        } else {
            '?'
        };
        formatter.write_char(shown)?;
    }
    Ok(())
}

/// 検証済みのサーバ情報（応答から offset / delay 以外を取り出したもの）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub leap: LeapIndicator,
    pub version: u8,
    pub stratum: Stratum,
    /// ポーリング間隔（2 の指数、秒）
    pub poll: i8,
    /// サーバの時計の精度（2 の指数、秒）
    pub precision: i8,
    pub root_delay: TimeDelta,
    pub root_dispersion: TimeDelta,
    pub reference_id: ReferenceId,
    pub reference_time: NtpTimestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_id_depends_on_stratum() {
        assert_eq!(
            ReferenceId::interpret(Stratum::new(1), *b"GNSS").to_string(),
            "GNSS"
        );
        assert_eq!(
            ReferenceId::interpret(Stratum::new(1), *b"GPS\0").to_string(),
            "GPS"
        );
        assert_eq!(
            ReferenceId::interpret(Stratum::new(2), [192, 168, 1, 123]).to_string(),
            "192.168.1.123"
        );
    }

    #[test]
    fn replaces_unprintable_bytes() {
        assert_eq!(
            ReferenceId::interpret(Stratum::new(1), [b'A', 0x01, b'B', 0]).to_string(),
            "A?B"
        );
    }

    #[test]
    fn parses_kiss_codes() {
        assert_eq!(KissCode::from_bytes(*b"RATE"), KissCode::RateExceeded);
        assert_eq!(KissCode::from_bytes(*b"DENY"), KissCode::Deny);
        assert_eq!(KissCode::from_bytes(*b"RSTR"), KissCode::Restrict);
        assert_eq!(KissCode::from_bytes(*b"INIT").to_string(), "INIT");
    }

    #[test]
    fn leap_indicator_uses_low_two_bits() {
        assert_eq!(LeapIndicator::from_bits(0), LeapIndicator::NoWarning);
        assert_eq!(LeapIndicator::from_bits(3), LeapIndicator::Unsynchronized);
        assert_eq!(LeapIndicator::from_bits(0b101), LeapIndicator::InsertSecond);
    }
}
