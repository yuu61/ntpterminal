//! 標本から基準点を作る: 時刻の段差の確認（`continuity`）・クロックフィルタ・周波数の推定をまとめたもの。
//!
//! 時刻源の種類（NTP のサーバ・ptp4l が合わせる PHC）によらず同じ処理なので、集約から切り出している。
//! - 採用した標本は、それまでの基準点と同じ時間の流れに乗っているか確かめてから使う。
//!   食い違う標本は保留し、次の標本もそれと合えば時刻の段差とみなして、それまでの標本を捨てる。
//!   次の標本が元の流れに合えば、保留した標本は一時的な外れとして使わない

use super::continuity::{is_continuous, residual};
use super::{Anchor, ClockFilter, FrequencyEstimator, FrequencyOffset, Sample};
use crate::shared_kernel::TimeDelta;

#[derive(Clone, Debug)]
pub struct ClockTracker {
    filter: ClockFilter,
    frequency: FrequencyEstimator,
    /// それまでの基準点と食い違った標本。次の標本で、時刻の段差か一時的な外れかを確かめる
    suspect: Option<Sample>,
    anchor: Option<Anchor>,
}

impl ClockTracker {
    /// クロックフィルタに `filter_len` 件まで標本を持つ。
    #[must_use]
    pub fn new(filter_len: usize) -> Self {
        Self {
            filter: ClockFilter::new(filter_len),
            frequency: FrequencyEstimator::new(),
            suspect: None,
            anchor: None,
        }
    }

    /// 標本を取り込む。時刻の段差を確かめて新しい時刻に合わせたときは、
    /// それまでの基準点から推定した時刻に対する差を返す。
    pub fn track(&mut self, sample: Sample) -> Option<TimeDelta> {
        let frequency = self.frequency.estimate();
        let current = self.filter.best().copied();
        if current.is_none_or(|best| is_continuous(&best, &sample, frequency)) {
            self.suspect = None;
            self.adopt(sample);
            return None;
        }
        match self.suspect.take() {
            // 2 回続けて食い違い、その 2 つは互いに合っている: 時刻が段差で変わった
            Some(previous) if is_continuous(&previous, &sample, frequency) => {
                let step = current
                    .and_then(|best| residual(&best, &sample, frequency.unwrap_or_default()))
                    .unwrap_or_default();
                self.filter.clear();
                self.frequency.reset();
                self.adopt(previous);
                self.adopt(sample);
                Some(step)
            }
            // 1 回目の食い違い（または、保留していた標本とも合わない）: 保留して、まだ使わない
            Some(_) | None => {
                self.suspect = Some(sample);
                None
            }
        }
    }

    /// 大きい時計の表示に使う基準点（クロックフィルタが選んだ標本から作る）。
    #[must_use]
    pub const fn anchor(&self) -> Option<Anchor> {
        self.anchor
    }

    /// クロックフィルタが選んだ標本（直近の中で「往復遅延の半分 + 古さ × 15 ppm」が最小）。
    #[must_use]
    pub fn selected(&self) -> Option<Sample> {
        self.filter.best().copied()
    }

    /// 時刻源の時計がモノトニック時計より速く進む割合の推定値。まだ推定できていなければ `None`。
    #[must_use]
    pub const fn frequency(&self) -> Option<FrequencyOffset> {
        self.frequency.estimate()
    }

    /// クロックフィルタが持っている標本の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.filter.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filter.is_empty()
    }

    /// 標本をクロックフィルタと周波数の推定に入れ、基準点を作り直す。
    fn adopt(&mut self, sample: Sample) {
        self.filter.push(sample);
        self.frequency.observe(&sample);
        let frequency = self.frequency.estimate().unwrap_or_default();
        if let Some(Ok(anchor)) = self.filter.best().map(Anchor::from_sample) {
            self.anchor = Some(anchor.with_frequency(frequency));
        }
    }
}
