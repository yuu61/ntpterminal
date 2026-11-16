use super::Sample;
use crate::shared_kernel::{NtpTimestamp, PcTime, Timebase, TimebaseMismatch};

/// 基準点: 「PC のモノトニック時計がこの値の瞬間、サーバ時刻はこうだった」という組。
///
/// 大きい時計は、基準点からモノトニック時計の経過分だけサーバ時刻を進めて表示する。
/// PC の壁時計は使わないので、PC の時計がずれていても表示は正しい。
/// `mono` は必ず [`Timebase::Monotonic`]（ほかの時刻基準は採用前に変換しておく）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    server_time: NtpTimestamp,
    mono: PcTime,
}

impl Anchor {
    /// 標本の T4 を基準点にする。
    pub fn from_sample(sample: &Sample) -> Result<Self, TimebaseMismatch> {
        let mono = sample.received().mono;
        if mono.timebase() != Timebase::Monotonic {
            return Err(TimebaseMismatch {
                left: mono.timebase(),
                right: Timebase::Monotonic,
            });
        }
        Ok(Self {
            server_time: sample.server_time_at_receive(),
            mono,
        })
    }

    pub const fn server_time(&self) -> NtpTimestamp {
        self.server_time
    }

    pub const fn mono(&self) -> PcTime {
        self.mono
    }

    /// `now`（モノトニック時計）の瞬間のサーバ時刻を推定する。
    pub fn project(&self, now: PcTime) -> Result<NtpTimestamp, TimebaseMismatch> {
        Ok(self.server_time.wrapping_add(now.since(self.mono)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::{LocalStamp, TimeDelta};

    fn sample_received_at(mono: PcTime) -> Sample {
        let wall = NtpTimestamp::from_parts(3_900_000_000, 0);
        let sent = LocalStamp {
            mono: PcTime::new(mono.timebase(), 0),
            wall,
        };
        let received = LocalStamp { mono, wall };
        Sample::new(sent, wall, wall, received).unwrap()
    }

    #[test]
    fn projects_forward_by_monotonic_elapsed_time() {
        let sample = sample_received_at(PcTime::monotonic(1_000_000_000));
        let anchor = Anchor::from_sample(&sample).unwrap();
        let projected = anchor.project(PcTime::monotonic(1_500_000_000)).unwrap();
        assert_eq!(
            projected.wrapping_since(anchor.server_time()),
            TimeDelta::from_millis(500)
        );
    }

    #[test]
    fn requires_monotonic_timebase() {
        let sample = sample_received_at(PcTime::new(Timebase::KernelRealtime, 1_000));
        assert!(Anchor::from_sample(&sample).is_err());
    }
}
