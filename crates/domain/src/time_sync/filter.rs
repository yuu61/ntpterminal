use alloc::collections::VecDeque;

use super::{FrequencyOffset, Sample};
use crate::shared_kernel::{PcTime, TimeDelta};

/// 直近 N 件の標本から、基準点に最も適したものを選ぶ（NTP の clock filter と同じ考え方）。
///
/// 選ぶ基準は「往復遅延の半分 + 古さ × 15 ppm」が最小であること（RFC 5905 の距離と同じ考え方）。
/// - 遅延が小さい標本ほど、経路の非対称や待ち行列の影響を受けておらず、サーバ時刻が正確に分かる
///   （誤差の上限が遅延の半分）
/// - ただし古い標本ほど、そこから時刻を進める間に PC とサーバの時計の進み方の差でずれる。
///   遅延だけで選ぶと、問い合わせ間隔が長いとき（16 秒 × 8 件 = 2 分前の標本など）にずれが大きくなる
#[derive(Clone, Debug)]
pub struct ClockFilter {
    capacity: usize,
    samples: VecDeque<Sample>,
}

impl ClockFilter {
    /// `capacity` 件まで保持する（最低 1 件）。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let size = capacity.max(1);
        Self {
            capacity: size,
            samples: VecDeque::with_capacity(size),
        }
    }

    pub fn push(&mut self, sample: Sample) {
        if self.samples.len() == self.capacity {
            let _: Option<Sample> = self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// 「往復遅延の半分 + 古さ × 15 ppm」が最小の標本。古さは最も新しい標本から数える。同じ値なら新しいほうを選ぶ。
    #[must_use]
    pub fn best(&self) -> Option<&Sample> {
        let newest = self.samples.back()?.received().mono;
        self.samples
            .iter()
            .rev()
            .min_by_key(|sample| distance(sample, newest))
    }

    /// すべての標本を捨てる（時刻が段差で変わったとき）。
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// 標本の選びにくさ。時刻基準が違って古さを測れない標本は、最後に選ぶ。
fn distance(sample: &Sample, newest: PcTime) -> TimeDelta {
    let age = newest
        .since(sample.received().mono)
        .map_or(TimeDelta::from_nanos(i64::MAX), |elapsed| {
            elapsed.max(TimeDelta::ZERO)
        });
    sample.delay().delta() / 2 + FrequencyOffset::WANDER.drift_over(age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::{LocalStamp, NtpTimestamp};

    fn sample_with_delay(start_ms: u64, delay_ms: u64) -> Sample {
        sample_with_delay_us(start_ms, delay_ms * 1_000)
    }

    fn sample_with_delay_us(start_ms: u64, delay_us: u64) -> Sample {
        let wall = NtpTimestamp::from_parts(3_900_000_000, 0);
        let sent = LocalStamp {
            mono: PcTime::monotonic(start_ms * 1_000_000),
            wall,
        };
        let received = LocalStamp {
            mono: PcTime::monotonic(start_ms * 1_000_000 + delay_us * 1_000),
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

    /// 遅延が少し小さいだけの古い標本より、新しい標本を選ぶ。
    #[test]
    fn prefers_newer_sample_when_older_one_is_only_slightly_faster() {
        // 16 秒間隔: 112 秒前の標本は 15 ppm × 112 秒 = 1.68 ms 分だけ不利になる
        let mut filter = ClockFilter::new(8);
        filter.push(sample_with_delay_us(0, 1_000));
        filter.push(sample_with_delay_us(112_000, 1_200));
        assert_eq!(filter.best().unwrap().sent().mono.nanos(), 112_000_000_000);

        // 500ms 間隔: 3.5 秒前なら 52.5 µs 分だけ不利。遅延の差（半分で 100 µs）の方が大きい
        let mut fast = ClockFilter::new(8);
        fast.push(sample_with_delay_us(0, 1_000));
        fast.push(sample_with_delay_us(3_500, 1_200));
        assert_eq!(fast.best().unwrap().sent().mono.nanos(), 0);
    }

    #[test]
    fn clear_drops_every_sample() {
        let mut filter = ClockFilter::new(8);
        filter.push(sample_with_delay(0, 1));
        filter.clear();
        assert!(filter.is_empty());
        assert!(filter.best().is_none());
    }
}
