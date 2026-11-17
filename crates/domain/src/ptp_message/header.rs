//! 共通ヘッダ（34 バイト）と、メッセージに共通の値の型。
//!
//! 配置（オフセットはメッセージの先頭から。すべてビッグエンディアン）:
//! - 0: 上位 4 bit が `transportSpecific`（`majorSdoId`）、下位 4 bit が `messageType`
//! - 1: 上位 4 bit が `minorVersionPTP`、下位 4 bit が `versionPTP`
//! - 2〜3: `messageLength`、4: `domainNumber`、5: `minorSdoId`、6〜7: `flagField`
//! - 8〜15: `correctionField`（ナノ秒 × 2^16）、16〜19: `messageTypeSpecific`
//! - 20〜29: `sourcePortIdentity`、30〜31: `sequenceId`、32: `controlField`、33: `logMessageInterval`

use core::fmt;

use crate::shared_kernel::TimeDelta;

/// ポートの識別子（`clockIdentity` 8 バイトとポート番号）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortIdentity {
    pub clock: [u8; 8],
    pub port: u16,
}

/// linuxptp と同じ書き方: "001b21.fffe.123456-1"
impl fmt::Display for PortIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.clock.iter().enumerate() {
            if index == 3 || index == 5 {
                f.write_str(".")?;
            }
            write!(f, "{byte:02x}")?;
        }
        write!(f, "-{}", self.port)
    }
}

/// `flagField`（2 バイトをビッグエンディアンの 16 bit として持つ。上位バイトが規格の 0 番目のオクテット）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(pub u16);

impl Flags {
    /// 送った時刻を後から `Follow_Up` で知らせる（two-step の Sync）
    pub const TWO_STEP: Self = Self(0x0200);
    /// ユニキャストで送った
    pub const UNICAST: Self = Self(0x0400);
    /// その日の最後の 1 分が 61 秒になる（Announce）
    pub const LEAP_61: Self = Self(0x0001);
    /// その日の最後の 1 分が 59 秒になる（Announce）
    pub const LEAP_59: Self = Self(0x0002);
    /// `currentUtcOffset` が確かな値（Announce）
    pub const UTC_OFFSET_VALID: Self = Self(0x0004);
    /// 時刻系が PTP（TAI）。偽なら任意（ARB）（Announce）
    pub const PTP_TIMESCALE: Self = Self(0x0008);
    pub const TIME_TRACEABLE: Self = Self(0x0010);
    pub const FREQUENCY_TRACEABLE: Self = Self(0x0020);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// `correctionField`（ナノ秒 × 2^16 の符号付きの値）。経路の機器が足した滞留時間など。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Correction(pub i64);

impl Correction {
    /// ナノ秒に直す（2^-16 ナノ秒の端数は最も近いナノ秒に丸める）。
    #[must_use]
    pub const fn to_delta(self) -> TimeDelta {
        // + 0.5 ns してから算術右シフト（切り下げ）
        TimeDelta::from_nanos(self.0.saturating_add(1 << 15u32) >> 16u32)
    }
}

/// PTP の時刻（`Timestamp`: 秒 48 bit とナノ秒 32 bit）。起点は 1970-01-01（時刻系は PTP なら TAI）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PtpTimestamp {
    /// 下位 48 bit だけを使う
    pub seconds: u64,
    pub nanos: u32,
}

impl PtpTimestamp {
    pub(super) const LEN: usize = 10;

    /// 起点からのナノ秒。
    #[must_use]
    pub const fn to_nanos(self) -> i128 {
        (self.seconds & SECONDS_MASK) as i128 * 1_000_000_000 + self.nanos as i128
    }

    pub(super) fn read(bytes: &[u8; Self::LEN]) -> Self {
        let mut seconds = [0u8; 8];
        seconds[2..].copy_from_slice(&bytes[..6]);
        Self {
            seconds: u64::from_be_bytes(seconds),
            nanos: u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
        }
    }

    pub(super) fn push(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.seconds & SECONDS_MASK).to_be_bytes()[2..]);
        out.extend_from_slice(&self.nanos.to_be_bytes());
    }
}

/// 48 bit の秒
const SECONDS_MASK: u64 = 0xFFFF_FFFF_FFFF;

/// 共通ヘッダのうち、メッセージの種類と長さ（本体から決まる）と `controlField` 以外。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// `transportSpecific`（`majorSdoId`）。既定のプロファイルは 0（gPTP は 1）
    pub transport_specific: u8,
    /// `versionPTP` のバイト（上位 4 bit が `minorVersionPTP`、下位 4 bit が `versionPTP`）
    pub version: u8,
    pub domain: u8,
    pub minor_sdo_id: u8,
    pub flags: Flags,
    pub correction: Correction,
    pub source: PortIdentity,
    pub sequence_id: u16,
    /// 送る間隔の log2（秒）。0x7F（127）は「指定なし」
    pub log_message_interval: i8,
}

impl Header {
    pub(super) const LEN: usize = 34;
    /// `versionPTP`: IEEE 1588-2008
    pub const VERSION_2: u8 = 2;
    /// `logMessageInterval` の「指定なし」
    pub const LOG_INTERVAL_UNSPECIFIED: i8 = 0x7F;
}

pub(super) fn push_port_identity(out: &mut Vec<u8>, identity: PortIdentity) {
    out.extend_from_slice(&identity.clock);
    out.extend_from_slice(&identity.port.to_be_bytes());
}

/// `at` から 10 バイトをポートの識別子として読む。呼び出し側が長さを確かめておくこと。
pub(super) fn read_port_identity(bytes: &[u8], at: usize) -> PortIdentity {
    let mut clock = [0u8; 8];
    clock.copy_from_slice(&bytes[at..at + 8]);
    PortIdentity {
        clock,
        port: read_u16(bytes, at + 8),
    }
}

/// `at` から 2 バイトをビッグエンディアンで読む。呼び出し側が長さを確かめておくこと。
pub(super) const fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correction_is_rounded_to_nanoseconds() {
        assert_eq!(Correction(0).to_delta(), TimeDelta::ZERO);
        assert_eq!(
            Correction(1000 << 16).to_delta(),
            TimeDelta::from_nanos(1000)
        );
        // 1.5 ns は 2 ns、-1.5 ns は -1 ns（+0.5 してから切り下げる）
        assert_eq!(Correction(0x1_8000).to_delta(), TimeDelta::from_nanos(2));
        assert_eq!(Correction(-0x1_8000).to_delta(), TimeDelta::from_nanos(-1));
        assert_eq!(
            Correction(-(250 << 16)).to_delta(),
            TimeDelta::from_nanos(-250)
        );
    }

    #[test]
    fn timestamps_use_48_bit_seconds() {
        let stamp = PtpTimestamp {
            seconds: 0x0001_2345_6789,
            nanos: 999_999_999,
        };
        let mut bytes = Vec::new();
        stamp.push(&mut bytes);
        assert_eq!(
            bytes,
            [0x00, 0x01, 0x23, 0x45, 0x67, 0x89, 0x3B, 0x9A, 0xC9, 0xFF]
        );
        assert_eq!(PtpTimestamp::read(bytes.first_chunk().unwrap()), stamp);
        assert_eq!(
            stamp.to_nanos(),
            0x0001_2345_6789i128 * 1_000_000_000 + 999_999_999
        );
    }

    #[test]
    fn port_identities_are_written_like_linuxptp() {
        let identity = PortIdentity {
            clock: [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56],
            port: 1,
        };
        assert_eq!(identity.to_string(), "001b21.fffe.123456-1");
    }

    #[test]
    fn flags_combine() {
        let flags = Flags::PTP_TIMESCALE.with(Flags::UTC_OFFSET_VALID);
        assert!(flags.contains(Flags::PTP_TIMESCALE));
        assert!(flags.contains(Flags::UTC_OFFSET_VALID));
        assert!(!flags.contains(Flags::TWO_STEP));
        assert_eq!(flags, Flags(0x000C));
    }
}
