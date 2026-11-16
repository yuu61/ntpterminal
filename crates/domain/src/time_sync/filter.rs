//! クロックフィルタ。

use std::collections::VecDeque;

use super::Sample;

/// 直近 N 件の標本から、往復遅延が最小のものを選ぶ（NTP の clock filter と同じ考え方）。
///
/// 遅延が小さい標本ほど、経路の非対称や待ち行列の影響を受けておらず offset が信頼できる。
#[derive(Clone, Debug)]
pub struct ClockFilter {
    capacity: usize,
    samples: VecDeque<Sample>,
}

impl ClockFilter {
    /// `capacity` 件まで保持する（最低 1 件）。
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            samples: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, sample: Sample) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// 往復遅延が最小の標本。同じ遅延なら新しいほうを選ぶ。
    pub fn best(&self) -> Option<&Sample> {
        self.samples
            .iter()
            .rev()
            .min_by_key(|sample| sample.delay())
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::{LocalStamp, NtpTimestamp, PcTime};

    fn sample_with_delay(start_ms: u64, delay_ms: u64) -> Sample {
        let wall = NtpTimestamp::from_parts(3_900_000_000, 0);
        let sent = LocalStamp {
            mono: PcTime::monotonic(start_ms * 1_000_000),
            wall,
        };
        let received = LocalStamp {
            mono: PcTime::monotonic((start_ms + delay_ms) * 1_000_000),
            wall,
        };
        Sample::new(sent, wall, wall, received).unwrap()
    }

    #[test]
    fn picks_minimum_delay_and_evicts_oldest() {
        let mut filter = ClockFilter::new(3);
        assert!(filter.best().is_none());

        filter.push(sample_with_delay(0, 1)); // 最小だが、あとで押し出される
        filter.push(sample_with_delay(100, 4));
        filter.push(sample_with_delay(200, 2));
        assert_eq!(filter.best().unwrap().sent().mono.nanos(), 0);

        filter.push(sample_with_delay(300, 3));
        assert_eq!(filter.len(), 3);
        assert_eq!(filter.best().unwrap().sent().mono.nanos(), 200_000_000);
    }

    #[test]
    fn prefers_newer_sample_on_tie() {
        let mut filter = ClockFilter::new(8);
        filter.push(sample_with_delay(0, 2));
        filter.push(sample_with_delay(100, 2));
        assert_eq!(filter.best().unwrap().sent().mono.nanos(), 100_000_000);
    }
}
