use std::time::{Instant, SystemTime, UNIX_EPOCH};

use application::port::ClockPort;
use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};

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

pub fn system_time_to_ntp(time: SystemTime) -> NtpTimestamp {
    let unix_nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
    };
    NtpTimestamp::from_unix_nanos(unix_nanos)
}

/// カーネルが付けた時刻を受け入れる最大の遅れ。これを超えたら時計が飛んだとみなす。
const MAX_KERNEL_LAG_NANOS: i128 = 1_000_000_000;

/// カーネルが付けた時刻（CLOCK_REALTIME の Unix ナノ秒）を、その直後に読んだ PC の時刻 `observed` を使って
/// モノトニック時計の時刻に写す。
///
/// カーネルの時刻と `observed.wall`（`SystemTime`）はどちらも CLOCK_REALTIME なので、その差が
/// 「カーネルが時刻を付けてから読むまでの遅れ」になる。モノトニック時計をその分だけ戻す。
/// 遅れが負、または 1 秒を超えるとき（chrony などが CLOCK_REALTIME を動かした）は `None`。
pub fn rebase_kernel_time(kernel_unix_nanos: i128, observed: LocalStamp) -> Option<LocalStamp> {
    let lag = observed.wall.to_unix_nanos() - kernel_unix_nanos;
    if !(0..=MAX_KERNEL_LAG_NANOS).contains(&lag) {
        return None;
    }
    let lag = TimeDelta::from_nanos(i64::try_from(lag).ok()?);
    Some(LocalStamp {
        mono: observed.mono.saturating_add(-lag),
        wall: NtpTimestamp::from_unix_nanos(kernel_unix_nanos),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn observed_at(unix_nanos: i128, mono_nanos: u64) -> LocalStamp {
        LocalStamp {
            mono: PcTime::monotonic(mono_nanos),
            wall: NtpTimestamp::from_unix_nanos(unix_nanos),
        }
    }

    #[test]
    fn rebases_kernel_time_onto_monotonic_clock() {
        let now = 1_763_281_545_000_000_000;
        // カーネルが 30µs 前に時刻を付けた
        let stamp = rebase_kernel_time(now - 30_000, observed_at(now, 5_000_000)).unwrap();
        assert_eq!(stamp.mono, PcTime::monotonic(4_970_000));
        assert_eq!(stamp.wall.to_unix_nanos(), now - 30_000);
    }

    #[test]
    fn rejects_kernel_time_when_clock_jumped() {
        let now = 1_763_281_545_000_000_000;
        // カーネルの時刻が未来（CLOCK_REALTIME が戻された）
        assert_eq!(
            rebase_kernel_time(now + 1, observed_at(now, 5_000_000)),
            None
        );
        // 1 秒以上前（CLOCK_REALTIME が進められた）
        assert_eq!(
            rebase_kernel_time(now - 2_000_000_000, observed_at(now, 5_000_000_000)),
            None
        );
    }

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
