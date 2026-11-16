use super::{FrequencyOffset, Sample};
use crate::shared_kernel::{NtpTimestamp, PcTime, Timebase, TimebaseMismatch};

/// 基準点: 「PC のモノトニック時計がこの値の瞬間、サーバ時刻はこうだった」という組。
///
/// 大きい時計は、基準点からモノトニック時計の経過分だけサーバ時刻を進めて表示する。
/// PC の壁時計は使わないので、PC の時計がずれていても表示は正しい。
/// 2 つの時計の進み方の差（[`FrequencyOffset`]）が分かっていれば、その分も補正する。
/// `mono` は必ず [`Timebase::Monotonic`]（ほかの時刻基準は採用前に変換しておく）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    server_time: NtpTimestamp,
    mono: PcTime,
    frequency: FrequencyOffset,
}

impl Anchor {
    /// 標本の T4 を基準点にする。
    ///
    /// # Errors
    ///
    /// 標本の T4 がモノトニック時計の時刻でないとき。
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
            frequency: FrequencyOffset::ZERO,
        })
    }

    /// サーバの時計がモノトニック時計より速く進む割合を設定する（[`super::FrequencyEstimator`] の推定値）。
    #[must_use]
    pub const fn with_frequency(self, frequency: FrequencyOffset) -> Self {
        Self { frequency, ..self }
    }

    #[must_use]
    pub const fn server_time(&self) -> NtpTimestamp {
        self.server_time
    }

    #[must_use]
    pub const fn mono(&self) -> PcTime {
        self.mono
    }

    #[must_use]
    pub const fn frequency(&self) -> FrequencyOffset {
        self.frequency
    }

    /// `now`（モノトニック時計）の瞬間のサーバ時刻を推定する（進み方の差も補正する）。
    ///
    /// # Errors
    ///
    /// `now` の時刻基準が基準点と違うとき。
    pub fn project(&self, now: PcTime) -> Result<NtpTimestamp, TimebaseMismatch> {
        let elapsed = now.since(self.mono)?;
        Ok(self
            .server_time
            .wrapping_add(elapsed + self.frequency.drift_over(elapsed)))
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
    fn corrects_for_the_frequency_offset() {
        let sample = sample_received_at(PcTime::monotonic(1_000_000_000));
        let anchor = Anchor::from_sample(&sample)
            .unwrap()
            .with_frequency(FrequencyOffset::from_ppb(35_000));
        // 60 秒でサーバの時計は 2.1ms 余分に進む。過去に向かっても同じ割合で戻す
        let later = anchor.project(PcTime::monotonic(61_000_000_000)).unwrap();
        assert_eq!(
            later.wrapping_since(anchor.server_time()),
            TimeDelta::from_secs(60) + TimeDelta::from_micros(2_100)
        );
        let earlier = anchor.project(PcTime::monotonic(0)).unwrap();
        assert_eq!(
            earlier.wrapping_since(anchor.server_time()),
            TimeDelta::from_secs(-1) - TimeDelta::from_nanos(35_000)
        );
    }

    #[test]
    fn requires_monotonic_timebase() {
        let sample = sample_received_at(PcTime::new(Timebase::KernelRealtime, 1_000));
        assert_eq!(
            Anchor::from_sample(&sample).unwrap_err(),
            TimebaseMismatch {
                left: Timebase::KernelRealtime,
                right: Timebase::Monotonic,
            }
        );
    }
}
