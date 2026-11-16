//! PC のモノトニック時計に対する、サーバの時計の進み方の差（周波数のずれ）の推定。
//!
//! 基準点（[`super::Anchor`]）からの時刻はモノトニック時計の経過時間で進めるので、2 つの時計の進み方が違うと、
//! 基準点から離れるほど表示がずれる。Windows のモノトニック時計（QPC）は時刻同期で合わせられず、
//! 数十 ppm 違うことがある（35 ppm なら 1 分で 2.1 ms、自走を 1 時間続けると 126 ms）。
//! ここでは測定結果から進み方の差を推定し、基準点からの時刻をその分だけ補正する。
//!
//! - 標本は 16 秒ごとの区間に分け、区間ごとに往復遅延が最小のものを 1 つだけ残す（待ち行列の影響を受けた標本を除く）
//! - 残した点（最大 32 個）に最小二乗法で直線を当てはめ、傾きを推定値とする。当てはめから大きく外れた点
//!   （残差が中央値の 3 倍を超え、かつ 10 µs を超えるもの）は除いて当てはめ直す
//! - 最初と最後の点が 60 秒以上離れるまでは推定しない。短い区間では、遅延のばらつき（数十 µs）が傾きの誤差として
//!   大きく出る（4 秒の区間で 30 µs なら 7.5 ppm）
//! - 500 ppm を超える推定は使わない（NTP と同じ上限。超えるなら時計の不具合か、見逃した時刻の段差）
//! - 時刻の段差を検出したとき（[`FrequencyEstimator::reset`]）は点を捨てるが、推定値は残す。進み方の差は
//!   PC とサーバの発振器の性質で、時刻が段差で変わっても変わらないため

use alloc::collections::VecDeque;

use super::Sample;
use crate::shared_kernel::{NtpTimestamp, PcTime, TimeDelta, Timebase};

/// 1 秒 = 10 億 ppb。
const PPB_PER_UNIT: i128 = 1_000_000_000;
/// 代表点を 1 つ選ぶ区間の長さ。
const BUCKET: TimeDelta = TimeDelta::from_secs(16);
/// 残す代表点の数（16 秒 × 32 ≒ 8.5 分。問い合わせ間隔が 16 秒より長いときは、そのぶん長い期間になる）。
const MAX_POINTS: usize = 32;
/// 推定に必要な代表点の数。
const MIN_POINTS: usize = 3;
/// 推定に必要な、最初と最後の代表点の間隔。
const MIN_BASELINE: TimeDelta = TimeDelta::from_secs(60);
/// 外れ値とみなす残差（残差の中央値の何倍を超えるか）。
const OUTLIER_FACTOR: f64 = 3.0;
/// これ以下の残差は外れ値として扱わない（ばらつきがごく小さいときに、正常な点まで除かないため）。
const OUTLIER_FLOOR_NANOS: f64 = 10_000.0;

/// サーバの時計がモノトニック時計より速く進む割合（ppb = 10 億分の 1）。正ならサーバの方が速い。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrequencyOffset {
    ppb: i64,
}

impl FrequencyOffset {
    pub const ZERO: Self = Self { ppb: 0 };
    /// これを超える推定値は使わない（500 ppm。NTP の上限と同じ）。
    pub const MAX: Self = Self { ppb: 500_000 };
    /// 推定した後も残りうる進み方の差の見込み（15 ppm。RFC 5905 の PHI と同じ）。
    pub const WANDER: Self = Self { ppb: 15_000 };

    #[must_use]
    pub const fn from_ppb(ppb: i64) -> Self {
        Self { ppb }
    }

    #[must_use]
    pub const fn ppb(self) -> i64 {
        self.ppb
    }

    #[must_use]
    pub fn as_ppm_f64(self) -> f64 {
        self.ppb as f64 / 1_000.0
    }

    /// モノトニック時計で `elapsed` 経つ間に、サーバの時計がそれより余分に進む量（最も近いナノ秒に丸める）。
    #[must_use]
    pub fn drift_over(self, elapsed: TimeDelta) -> TimeDelta {
        let scaled = i128::from(elapsed.as_nanos()) * i128::from(self.ppb);
        TimeDelta::from_i128_nanos((scaled + scaled.signum() * (PPB_PER_UNIT / 2)) / PPB_PER_UNIT)
    }
}

/// 代表点: T4 のモノトニック時計と、その瞬間のサーバ時刻の推定値。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    mono: PcTime,
    server: NtpTimestamp,
    delay: TimeDelta,
}

/// 採用した標本から、サーバの時計とモノトニック時計の進み方の差を推定する。
#[derive(Clone, Debug, Default)]
pub struct FrequencyEstimator {
    /// 区間ごとの代表点（古い順）。最後の点は、いま標本を集めている区間のもの
    points: VecDeque<Point>,
    /// いま標本を集めている区間の始まり
    bucket_start: Option<PcTime>,
    estimate: Option<FrequencyOffset>,
}

impl FrequencyEstimator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 推定値。まだ推定できていなければ `None`。
    #[must_use]
    pub const fn estimate(&self) -> Option<FrequencyOffset> {
        self.estimate
    }

    /// 採用した標本を 1 つ取り込み、推定し直す。
    pub fn observe(&mut self, sample: &Sample) {
        let mono = sample.received().mono;
        if mono.timebase() != Timebase::Monotonic {
            return;
        }
        let point = Point {
            mono,
            server: sample.server_time_at_receive(),
            delay: sample.delay().delta(),
        };
        let in_open_bucket = self.bucket_start.is_some_and(|start| {
            mono.since(start)
                .is_ok_and(|elapsed| !elapsed.is_negative() && elapsed < BUCKET)
        });
        if in_open_bucket {
            if let Some(representative) = self.points.back_mut()
                && point.delay < representative.delay
            {
                *representative = point;
            }
        } else {
            if self.points.len() == MAX_POINTS {
                let _: Option<Point> = self.points.pop_front();
            }
            self.points.push_back(point);
            self.bucket_start = Some(mono);
        }
        if let Some(fitted) = estimate_from(&self.points) {
            self.estimate = Some(fitted);
        }
    }

    /// 時刻が段差で変わったとき: 代表点を捨てる（推定値は残す）。
    pub fn reset(&mut self) {
        self.points.clear();
        self.bucket_start = None;
    }
}

/// 当てはめる座標: x = 最初の点からのモノトニック時計の経過、y = 同じ間のサーバ時刻の経過 − x（ずれの累積）。
/// どちらもナノ秒。
#[derive(Clone, Copy, Debug)]
struct Coord {
    x: f64,
    y: f64,
}

/// y = intercept + slope × x
#[derive(Clone, Copy, Debug)]
struct Line {
    slope: f64,
    intercept: f64,
}

impl Line {
    fn residual(self, coord: Coord) -> f64 {
        (coord.y - self.slope.mul_add(coord.x, self.intercept)).abs()
    }
}

/// 代表点から推定する。点が足りない・基線が短い・上限を超えるときは `None`。
fn estimate_from(points: &VecDeque<Point>) -> Option<FrequencyOffset> {
    let first = points.front()?;
    let coords: Vec<Coord> = points
        .iter()
        .map(|point| {
            let elapsed = point.mono.since(first.mono).ok()?.as_nanos();
            let server_elapsed = point.server.wrapping_since(first.server).as_nanos();
            Some(Coord {
                x: elapsed as f64,
                y: (i128::from(server_elapsed) - i128::from(elapsed)) as f64,
            })
        })
        .collect::<Option<_>>()?;
    let first_fit = fit(&coords)?;

    // 当てはめから大きく外れた点を除いて、当てはめ直す
    let mut residuals: Vec<f64> = coords
        .iter()
        .map(|&coord| first_fit.residual(coord))
        .collect();
    residuals.sort_by(f64::total_cmp);
    let median = residuals[residuals.len() / 2];
    let limit = (median * OUTLIER_FACTOR).max(OUTLIER_FLOOR_NANOS);
    let inliers: Vec<Coord> = coords
        .iter()
        .copied()
        .filter(|&coord| first_fit.residual(coord) <= limit)
        .collect();
    let line = if inliers.len() == coords.len() {
        first_fit
    } else {
        fit(&inliers)?
    };
    to_frequency(line.slope)
}

/// 最小二乗法で直線を当てはめる。点が足りない・基線が短いときは `None`。
fn fit(coords: &[Coord]) -> Option<Line> {
    if coords.len() < MIN_POINTS {
        return None;
    }
    let (min_x, max_x) = coords
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), coord| {
            (low.min(coord.x), high.max(coord.x))
        });
    if max_x - min_x < MIN_BASELINE.as_nanos() as f64 {
        return None;
    }
    let count = coords.len() as f64;
    let mean_x = coords.iter().map(|coord| coord.x).sum::<f64>() / count;
    let mean_y = coords.iter().map(|coord| coord.y).sum::<f64>() / count;
    let (sxx, sxy) = coords.iter().fold((0.0f64, 0.0f64), |(sxx, sxy), coord| {
        let dx = coord.x - mean_x;
        (dx.mul_add(dx, sxx), dx.mul_add(coord.y - mean_y, sxy))
    });
    let slope = sxy / sxx;
    Some(Line {
        slope,
        intercept: slope.mul_add(-mean_x, mean_y),
    })
}

/// 傾き（ナノ秒あたりのナノ秒）を ppb にする。上限を超える（NaN を含む）ときは `None`。
#[expect(
    clippy::cast_possible_truncation,
    reason = "直前に ±500 ppm（±500000 ppb）以内であることを確かめているので、i64 に必ず収まる"
)]
fn to_frequency(slope: f64) -> Option<FrequencyOffset> {
    let ppb = (slope * PPB_PER_UNIT as f64).round();
    (ppb.abs() <= FrequencyOffset::MAX.ppb as f64).then(|| FrequencyOffset::from_ppb(ppb as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::LocalStamp;

    const BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);

    /// モノトニック時計 `at` の瞬間に送って、往復 `delay` で受け取った標本。サーバの時計は `ppb` だけ速く進み、
    /// サーバ時刻の推定値に `error` の誤差が乗る。
    fn sample(at: TimeDelta, delay: TimeDelta, ppb: i64, error: TimeDelta) -> Sample {
        let received_at = at + delay;
        let mono = |time: TimeDelta| PcTime::monotonic(time.as_nanos().cast_unsigned());
        let server_at = |time: TimeDelta| {
            BASE.wrapping_add(time + FrequencyOffset::from_ppb(ppb).drift_over(time) + error)
        };
        // サーバ内の処理時間 0、片道は delay / 2
        let server = server_at(at + delay / 2);
        let sent = LocalStamp {
            mono: mono(at),
            wall: BASE,
        };
        let received = LocalStamp {
            mono: mono(received_at),
            wall: BASE,
        };
        Sample::new(sent, server, server, received).unwrap()
    }

    fn feed(estimator: &mut FrequencyEstimator, ppb: i64, seconds: i64, step_ms: i64) {
        let mut at = 0i64;
        while at <= seconds * 1_000 {
            // 遅延は 100〜400 µs で揺らす（区間ごとの最小を代表点にする）
            let delay = TimeDelta::from_micros(100 + (at / step_ms).rem_euclid(4) * 100);
            estimator.observe(&sample(
                TimeDelta::from_millis(at),
                delay,
                ppb,
                TimeDelta::ZERO,
            ));
            at += step_ms;
        }
    }

    #[test]
    fn converts_frequency_to_drift() {
        let offset = FrequencyOffset::from_ppb(35_000);
        assert_eq!(
            offset.drift_over(TimeDelta::from_secs(60)),
            TimeDelta::from_micros(2_100)
        );
        assert_eq!(
            FrequencyOffset::from_ppb(-35_000).drift_over(TimeDelta::from_secs(60)),
            TimeDelta::from_micros(-2_100)
        );
        assert!((offset.as_ppm_f64() - 35.0).abs() < 1e-9f64);
    }

    #[test]
    fn estimates_after_a_long_enough_baseline() {
        let mut estimator = FrequencyEstimator::new();
        feed(&mut estimator, 35_000, 40, 500);
        assert_eq!(estimator.estimate(), None, "60 秒たつまでは推定しない");

        let mut longer = FrequencyEstimator::new();
        feed(&mut longer, 35_000, 120, 500);
        let ppb = longer.estimate().unwrap().ppb();
        assert!((ppb - 35_000).abs() <= 100, "推定値 {ppb} ppb");
    }

    #[test]
    fn estimates_negative_frequency_with_sparse_polls() {
        // 16 秒間隔の問い合わせ（区間ごとに 1 点）
        let mut estimator = FrequencyEstimator::new();
        feed(&mut estimator, -20_000, 160, 16_000);
        let ppb = estimator.estimate().unwrap().ppb();
        assert!((ppb + 20_000).abs() <= 100, "推定値 {ppb} ppb");
    }

    #[test]
    fn ignores_an_outlying_point() {
        let mut estimator = FrequencyEstimator::new();
        for index in 0..10i64 {
            // 5 番目の区間だけ、サーバ時刻が 3 ms ずれた標本しかない
            let error = if index == 5 {
                TimeDelta::from_millis(3)
            } else {
                TimeDelta::ZERO
            };
            estimator.observe(&sample(
                TimeDelta::from_secs(index * 16),
                TimeDelta::from_micros(200),
                10_000,
                error,
            ));
        }
        let ppb = estimator.estimate().unwrap().ppb();
        assert!((ppb - 10_000).abs() <= 10, "推定値 {ppb} ppb");
    }

    #[test]
    fn keeps_the_minimum_delay_sample_of_each_bucket() {
        let mut estimator = FrequencyEstimator::new();
        for index in 0..6i64 {
            let at = TimeDelta::from_secs(index * 16);
            // 区間の最初に遅延が大きく時刻のずれた標本、次に遅延が小さく正しい標本
            estimator.observe(&sample(
                at,
                TimeDelta::from_millis(5),
                0,
                TimeDelta::from_micros(index * 300),
            ));
            estimator.observe(&sample(
                at + TimeDelta::from_secs(1),
                TimeDelta::from_micros(100),
                0,
                TimeDelta::ZERO,
            ));
        }
        assert_eq!(estimator.estimate(), Some(FrequencyOffset::ZERO));
    }

    #[test]
    fn refuses_implausible_frequency() {
        let mut estimator = FrequencyEstimator::new();
        feed(&mut estimator, 800_000, 120, 1_000);
        assert_eq!(estimator.estimate(), None);
    }

    #[test]
    fn reset_drops_points_but_keeps_the_estimate() {
        let mut estimator = FrequencyEstimator::new();
        feed(&mut estimator, 35_000, 120, 500);
        let before = estimator.estimate();
        assert!(before.is_some());
        estimator.reset();
        assert_eq!(estimator.estimate(), before);
        // 段差の後は新しい点だけで推定する（前の点と混ぜない）
        estimator.observe(&sample(
            TimeDelta::from_secs(200),
            TimeDelta::from_micros(100),
            35_000,
            TimeDelta::from_millis(-300),
        ));
        assert_eq!(estimator.estimate(), before);
    }
}
