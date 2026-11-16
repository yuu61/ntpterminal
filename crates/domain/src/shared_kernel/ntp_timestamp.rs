use super::time_delta::{NANOS_PER_SEC, TimeDelta};

/// 1900-01-01（NTP の起点）から 1970-01-01（Unix の起点）までの秒数。
const NTP_TO_UNIX_SECS: i128 = 2_208_988_800;
/// 秒フィールドが一周する長さ（2^32 秒 ≒ 136 年）。
const ERA_SECS: i128 = 1 << 32;
/// これより小さい秒は era 1（2036-02-07 以降）とみなす。
const ERA_PIVOT_SECS: u32 = 0x8000_0000;

/// NTP タイムスタンプ（32.32 固定小数、1900-01-01 00:00:00 UTC 起点）。
///
/// 2036-02-07 06:28:16 UTC に秒フィールドが一周する（era 0 → 1）。
/// - 差の計算は [`Self::wrapping_since`] を使う。両者が ±68 年以内なら一周をまたいでも正しい。
/// - 絶対時刻への変換（[`Self::to_unix_nanos`]）はピボット方式:
///   秒 < `0x8000_0000` なら era 1 とみなす。これで 1968〜2104 年を表せる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NtpTimestamp(u64);

impl NtpTimestamp {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn from_parts(seconds: u32, fraction: u32) -> Self {
        Self(((seconds as u64) << 32) | fraction as u64)
    }

    #[must_use]
    pub const fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }

    #[must_use]
    pub const fn fraction(self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub fn wrapping_since(self, earlier: Self) -> TimeDelta {
        let fixed = self.0.wrapping_sub(earlier.0).cast_signed();
        TimeDelta::from_ntp_fixed(i128::from(fixed))
    }

    #[must_use]
    pub fn wrapping_add(self, delta: TimeDelta) -> Self {
        // 最も近い固定小数の値に丸める（0 から遠い側へ丸める四捨五入）
        let scaled = i128::from(delta.as_nanos()) << 32i32;
        let fixed = (scaled + scaled.signum() * (NANOS_PER_SEC / 2)) / NANOS_PER_SEC;
        // 下位 64 ビットだけを足す（2^64 で折り返す）
        Self(
            self.0
                .wrapping_add((fixed.cast_unsigned() & 0xFFFF_FFFF_FFFF_FFFF) as u64),
        )
    }

    #[must_use]
    pub fn to_unix_nanos(self) -> i128 {
        let era = i128::from(self.seconds() < ERA_PIVOT_SECS);
        let secs = i128::from(self.seconds()) + era * ERA_SECS - NTP_TO_UNIX_SECS;
        // 最も近いナノ秒に丸める（分解能 ≒ 0.23ns なので、from_unix_nanos との往復で値が変わらない）
        let frac_nanos = (i128::from(self.fraction()) * NANOS_PER_SEC + (1 << 31i32)) >> 32i32;
        secs * NANOS_PER_SEC + frac_nanos
    }

    #[must_use]
    pub const fn from_unix_nanos(nanos: i128) -> Self {
        let unix_secs = nanos.div_euclid(NANOS_PER_SEC);
        let sub_nanos = nanos.rem_euclid(NANOS_PER_SEC);
        // 秒は 2^32 で折り返す（下位 32 ビット。rem_euclid(ERA_SECS) と同じ値）
        let seconds = ((unix_secs + NTP_TO_UNIX_SECS).cast_unsigned() & 0xFFFF_FFFF) as u32;
        // sub_nanos は 0 以上 1e9 未満なので結果は必ず 2^32 未満（マスクしても値は変わらない）
        let fraction =
            (((sub_nanos << 32i32) / NANOS_PER_SEC).cast_unsigned() & 0xFFFF_FFFF) as u32;
        Self::from_parts(seconds, fraction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2036-02-07 06:28:16 UTC（era 0 → 1 の境目）の Unix 秒。
    const ERA_BOUNDARY_UNIX_SECS: i128 = 2_085_978_496;
    const SEC: i128 = 1_000_000_000;

    #[test]
    fn splits_into_seconds_and_fraction() {
        let ts = NtpTimestamp::from_parts(0x0123_4567, 0x89AB_CDEF);
        assert_eq!(ts.to_bits(), 0x0123_4567_89AB_CDEF);
        assert_eq!(ts.seconds(), 0x0123_4567);
        assert_eq!(ts.fraction(), 0x89AB_CDEF);
    }

    #[test]
    fn unix_epoch_matches() {
        let epoch = NtpTimestamp::from_parts(2_208_988_800, 0);
        assert_eq!(epoch.to_unix_nanos(), 0);
        assert_eq!(NtpTimestamp::from_unix_nanos(0), epoch);
    }

    #[test]
    fn half_second_is_exact() {
        let ts = NtpTimestamp::from_unix_nanos(SEC / 2);
        assert_eq!(ts.fraction(), 0x8000_0000);
        assert_eq!(ts.to_unix_nanos(), SEC / 2);
    }

    #[test]
    fn round_trips_every_nanosecond_value_exactly() {
        for nanos in [0, 1, 146_000_000, 999_999_999, 123_456_789] {
            let unix = 1_763_281_545 * SEC + nanos;
            assert_eq!(NtpTimestamp::from_unix_nanos(unix).to_unix_nanos(), unix);
        }
    }

    #[test]
    fn handles_era_rollover_in_2036() {
        let before = NtpTimestamp::from_unix_nanos((ERA_BOUNDARY_UNIX_SECS - 1) * SEC);
        let after = NtpTimestamp::from_unix_nanos((ERA_BOUNDARY_UNIX_SECS + 1) * SEC);
        assert_eq!(before, NtpTimestamp::from_parts(u32::MAX, 0));
        assert_eq!(after, NtpTimestamp::from_parts(1, 0));

        // 絶対時刻への変換はピボットで era を判定する
        assert_eq!(before.to_unix_nanos(), (ERA_BOUNDARY_UNIX_SECS - 1) * SEC);
        assert_eq!(after.to_unix_nanos(), (ERA_BOUNDARY_UNIX_SECS + 1) * SEC);

        // 差と加算は一周をまたいでも正しい
        assert_eq!(after.wrapping_since(before), TimeDelta::from_secs(2));
        assert_eq!(before.wrapping_since(after), TimeDelta::from_secs(-2));
        assert_eq!(before.wrapping_add(TimeDelta::from_secs(2)), after);
    }

    #[test]
    fn wrapping_add_supports_negative_delta() {
        let ts = NtpTimestamp::from_parts(100, 0);
        assert_eq!(
            ts.wrapping_add(TimeDelta::from_millis(-500)),
            NtpTimestamp::from_parts(99, 0x8000_0000)
        );
    }
}
