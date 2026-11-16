//! PC の時計（std の Instant / SystemTime）。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use application::port::ClockPort;
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime};

/// std の時計を使う [`ClockPort`] の実装。
///
/// モノトニック時計は、作成時の `Instant` を起点にしたナノ秒で表す。
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockPort for SystemClock {
    fn now(&self) -> LocalStamp {
        let elapsed = self.origin.elapsed();
        let wall = SystemTime::now();
        LocalStamp {
            mono: PcTime::monotonic(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)),
            wall: system_time_to_ntp(wall),
        }
    }
}

/// [`SystemTime`] を NTP タイムスタンプに変換する。
pub fn system_time_to_ntp(time: SystemTime) -> NtpTimestamp {
    let unix_nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
    };
    NtpTimestamp::from_unix_nanos(unix_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn converts_system_time_to_ntp() {
        assert_eq!(
            system_time_to_ntp(UNIX_EPOCH),
            NtpTimestamp::from_parts(2_208_988_800, 0)
        );
        assert_eq!(
            system_time_to_ntp(UNIX_EPOCH + Duration::from_millis(1_500)),
            NtpTimestamp::from_parts(2_208_988_801, 0x8000_0000)
        );
    }

    #[test]
    fn monotonic_part_never_goes_backwards() {
        let clock = SystemClock::new();
        let first = clock.now().mono;
        let second = clock.now().mono;
        assert!(second.is_at_or_after(first).unwrap());
    }
}
