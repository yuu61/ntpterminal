//! 符号付きの時間差。

use std::ops::{Add, Div, Mul, Neg, Sub};
use std::time::Duration;

pub(crate) const NANOS_PER_SEC: i128 = 1_000_000_000;

/// 符号付きの時間差（ナノ秒単位）。offset のように負になりうる量を表す。
///
/// 演算は飽和演算（オーバーフローしてもパニックしない）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeDelta {
    nanos: i64,
}

impl TimeDelta {
    pub const ZERO: Self = Self { nanos: 0 };

    pub const fn from_nanos(nanos: i64) -> Self {
        Self { nanos }
    }

    pub const fn from_micros(micros: i64) -> Self {
        Self {
            nanos: micros.saturating_mul(1_000),
        }
    }

    pub const fn from_millis(millis: i64) -> Self {
        Self {
            nanos: millis.saturating_mul(1_000_000),
        }
    }

    pub const fn from_secs(secs: i64) -> Self {
        Self {
            nanos: secs.saturating_mul(1_000_000_000),
        }
    }

    /// 小数ミリ秒から作る（設定値の読み込み用）。範囲外は飽和し、NaN は 0 になる。
    pub fn from_millis_f64(millis: f64) -> Self {
        Self {
            nanos: (millis * 1_000_000.0).round() as i64,
        }
    }

    /// std の [`Duration`] から作る。範囲外は飽和する。
    pub fn from_std(duration: Duration) -> Self {
        Self {
            nanos: i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        }
    }

    /// 32.32 固定小数（符号付き・単位は秒）から変換する。最も近いナノ秒に丸める。
    pub(crate) fn from_ntp_fixed(fixed: i128) -> Self {
        Self::from_i128_nanos((fixed * NANOS_PER_SEC + (1 << 31)) >> 32)
    }

    /// 16.16 固定小数（NTP short 形式。root delay / root dispersion）から変換する。最も近いナノ秒に丸める。
    pub(crate) fn from_ntp_short(raw: u32) -> Self {
        Self::from_i128_nanos((i128::from(raw) * NANOS_PER_SEC + (1 << 15)) >> 16)
    }

    pub(crate) fn from_i128_nanos(nanos: i128) -> Self {
        let nanos = i64::try_from(nanos).unwrap_or(if nanos < 0 { i64::MIN } else { i64::MAX });
        Self { nanos }
    }

    pub const fn as_nanos(self) -> i64 {
        self.nanos
    }

    pub fn as_secs_f64(self) -> f64 {
        self.nanos as f64 / 1e9
    }

    pub fn as_millis_f64(self) -> f64 {
        self.nanos as f64 / 1e6
    }

    pub fn as_micros_f64(self) -> f64 {
        self.nanos as f64 / 1e3
    }

    pub const fn is_negative(self) -> bool {
        self.nanos < 0
    }

    pub const fn abs(self) -> Self {
        Self {
            nanos: self.nanos.saturating_abs(),
        }
    }

    /// std の [`Duration`] に変換する。負の値は 0 になる。
    pub fn to_std_saturating(self) -> Duration {
        Duration::from_nanos(u64::try_from(self.nanos).unwrap_or(0))
    }
}

impl Add for TimeDelta {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            nanos: self.nanos.saturating_add(rhs.nanos),
        }
    }
}

impl Sub for TimeDelta {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            nanos: self.nanos.saturating_sub(rhs.nanos),
        }
    }
}

impl Neg for TimeDelta {
    type Output = Self;
    fn neg(self) -> Self {
        Self {
            nanos: self.nanos.saturating_neg(),
        }
    }
}

impl Mul<i64> for TimeDelta {
    type Output = Self;
    fn mul(self, rhs: i64) -> Self {
        Self {
            nanos: self.nanos.saturating_mul(rhs),
        }
    }
}

impl Div<i64> for TimeDelta {
    type Output = Self;
    /// 整数除算（0 方向へ丸め）。`rhs` が 0 ならパニックする。
    fn div(self, rhs: i64) -> Self {
        Self {
            nanos: self.nanos / rhs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_ntp_fixed_point() {
        assert_eq!(
            TimeDelta::from_ntp_fixed(1 << 31),
            TimeDelta::from_millis(500)
        );
        assert_eq!(
            TimeDelta::from_ntp_fixed(-(1 << 32)),
            TimeDelta::from_secs(-1)
        );
        assert_eq!(
            TimeDelta::from_ntp_short(0x0001_8000),
            TimeDelta::from_millis(1_500)
        );
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        let max = TimeDelta::from_nanos(i64::MAX);
        assert_eq!(max + TimeDelta::from_nanos(1), max);
        assert_eq!(TimeDelta::from_secs(i64::MAX).as_nanos(), i64::MAX);
    }

    #[test]
    fn negative_values_become_zero_duration() {
        assert_eq!(
            TimeDelta::from_millis(-3).to_std_saturating(),
            Duration::ZERO
        );
        assert_eq!(
            TimeDelta::from_millis(3).to_std_saturating(),
            Duration::from_millis(3)
        );
    }

    #[test]
    fn parses_fractional_millis() {
        assert_eq!(
            TimeDelta::from_millis_f64(-0.978),
            TimeDelta::from_micros(-978)
        );
    }
}
