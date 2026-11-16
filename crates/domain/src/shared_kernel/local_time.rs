//! PC 側で取った時刻。

use super::{NtpTimestamp, TimeDelta};

/// 時刻基準: PC 側の時刻を「どの時計で」取ったか。
///
/// 送信方式（UDP / raw / link）は実行時に選ぶので、時刻基準も実行時の値として持つ。
/// 異なる時刻基準の時刻どうしは比較も差分もできない（[`TimebaseMismatch`]）。
///
/// 実際には、カーネルや NIC の時刻は transport の中でモノトニック時計に写してから渡すので、
/// ドメインに届くのは `Monotonic` だけ。ほかの値は予約で、混ざったときの安全網として残している。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timebase {
    /// ユーザ空間で読んだプロセス内のモノトニック時計。
    Monotonic,
    /// カーネルが付けた CLOCK_REALTIME のタイムスタンプ（予約）。
    KernelRealtime,
    /// Windows のソフトウェアタイムスタンプ（QPC。予約）。
    WindowsQpc,
    /// NIC のハードウェア時計（PHC）。`index` は /dev/ptpN の番号など（予約）。
    NicPhc { index: u32 },
}

/// 異なる時刻基準の時刻を比べようとした。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("時刻基準が一致しません: {left:?} と {right:?}")]
pub struct TimebaseMismatch {
    pub left: Timebase,
    pub right: Timebase,
}

/// PC 側で取った時刻。どの時計で取ったか（[`Timebase`]）を必ず持つ。
///
/// 値は各時計の任意の起点からのナノ秒。同じ時刻基準どうしでしか比較・差分できない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PcTime {
    timebase: Timebase,
    nanos: u64,
}

impl PcTime {
    pub const fn new(timebase: Timebase, nanos: u64) -> Self {
        Self { timebase, nanos }
    }

    /// モノトニック時計の時刻を作る。
    pub const fn monotonic(nanos: u64) -> Self {
        Self::new(Timebase::Monotonic, nanos)
    }

    pub const fn timebase(self) -> Timebase {
        self.timebase
    }

    pub const fn nanos(self) -> u64 {
        self.nanos
    }

    /// `self − earlier` を求める。時刻基準が違えばエラー。
    pub fn since(self, earlier: Self) -> Result<TimeDelta, TimebaseMismatch> {
        self.ensure_same(earlier)?;
        Ok(TimeDelta::from_i128_nanos(
            i128::from(self.nanos) - i128::from(earlier.nanos),
        ))
    }

    /// `self + delta` を求める。0 未満にはならない。
    pub fn saturating_add(self, delta: TimeDelta) -> Self {
        let nanos = i128::from(self.nanos) + i128::from(delta.as_nanos());
        Self {
            timebase: self.timebase,
            nanos: u64::try_from(nanos.max(0)).unwrap_or(u64::MAX),
        }
    }

    /// `self` が `other` と同時かそれより後か。時刻基準が違えばエラー。
    pub fn is_at_or_after(self, other: Self) -> Result<bool, TimebaseMismatch> {
        self.ensure_same(other)?;
        Ok(self.nanos >= other.nanos)
    }

    fn ensure_same(self, other: Self) -> Result<(), TimebaseMismatch> {
        if self.timebase == other.timebase {
            Ok(())
        } else {
            Err(TimebaseMismatch {
                left: self.timebase,
                right: other.timebase,
            })
        }
    }
}

/// 同じ瞬間に（続けて）読んだ PC のモノトニック時計と壁時計の組。
///
/// delay と基準点は `mono` から、offset は `wall` から計算する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalStamp {
    pub mono: PcTime,
    pub wall: NtpTimestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differences_require_same_timebase() {
        let a = PcTime::monotonic(5_000);
        let b = PcTime::monotonic(2_000);
        assert_eq!(a.since(b), Ok(TimeDelta::from_nanos(3_000)));
        assert_eq!(b.since(a), Ok(TimeDelta::from_nanos(-3_000)));

        let kernel = PcTime::new(Timebase::KernelRealtime, 2_000);
        assert_eq!(
            a.since(kernel),
            Err(TimebaseMismatch {
                left: Timebase::Monotonic,
                right: Timebase::KernelRealtime,
            })
        );
        assert!(a.is_at_or_after(kernel).is_err());
    }

    #[test]
    fn saturating_add_never_goes_below_zero() {
        let t = PcTime::monotonic(1_000);
        assert_eq!(t.saturating_add(TimeDelta::from_nanos(-5_000)).nanos(), 0);
        assert_eq!(t.saturating_add(TimeDelta::from_nanos(500)).nanos(), 1_500);
    }
}
