//! 時刻の段差の検出: 2 つの標本が、同じ時間の流れ（サーバ時刻とモノトニック時計の関係）に乗っているかを確かめる。
//!
//! クロックフィルタは遅延と古さで標本を選ぶので、サーバの時刻が段差で変わっても、段差の前の標本を選び続けてしまう
//! （8 件が入れ替わるまで、表示が段差の分だけずれたままになる）。段差は、サーバの時刻の修正・閏秒のほか、
//! PC のスリープ（Linux のモノトニック時計はスリープ中に止まる）でも起きる。
//!
//! 1 つの標本から分かるサーバ時刻は、往復遅延の半分の幅でしか定まらない（経路の非対称の上限）。
//! 2 つの標本の幅を、間の時間に見込める時計のずれだけ広げても重ならなければ、時刻が段差で変わったとみなす。
//! - 時計のずれの見込みは、周波数の推定値があれば推定の残り（15 ppm）、なければ上限（500 ppm）
//! - さらに 1 ms の余裕を取る（表示は 1 ms 単位なので、それより小さい段差は追いかけなくてよい）
//!
//! 食い違った標本が 1 つだけなら一時的な外れとみなして使わない。判断は [`super::TimeSource`] が行う。

use super::{Anchor, FrequencyOffset, Sample};
use crate::shared_kernel::TimeDelta;

/// 段差とみなす前に取る余裕。
const MARGIN: TimeDelta = TimeDelta::from_millis(1);

/// `later` の T4 の瞬間のサーバ時刻を `earlier` から推定し、`later` 自身の値との差（`later` − 推定）を返す。
/// 比べられないとき（どちらかがモノトニック時計の時刻でない）は `None`。
pub(super) fn residual(
    earlier: &Sample,
    later: &Sample,
    frequency: FrequencyOffset,
) -> Option<TimeDelta> {
    let anchor = Anchor::from_sample(earlier).ok()?.with_frequency(frequency);
    let predicted = anchor.project(later.received().mono).ok()?;
    Some(later.server_time_at_receive().wrapping_since(predicted))
}

/// `earlier` と `later` が同じ時間の流れに乗っているか。`frequency` は周波数の推定値（まだなければ `None`）。
/// 比べられないときは乗っているとみなす（段差として扱う根拠がない）。
pub(super) fn is_continuous(
    earlier: &Sample,
    later: &Sample,
    frequency: Option<FrequencyOffset>,
) -> bool {
    residual(earlier, later, frequency.unwrap_or_default())
        .is_none_or(|error| error.abs() <= tolerance(earlier, later, frequency.is_some()))
}

/// 2 つの標本の差として許す大きさ: 遅延の半分ずつ + 間の時間の時計のずれの見込み + 余裕。
fn tolerance(earlier: &Sample, later: &Sample, frequency_known: bool) -> TimeDelta {
    let apart = later
        .received()
        .mono
        .since(earlier.received().mono)
        .map_or(TimeDelta::ZERO, TimeDelta::abs);
    let wander = if frequency_known {
        FrequencyOffset::WANDER
    } else {
        FrequencyOffset::MAX
    };
    earlier.delay().delta() / 2 + later.delay().delta() / 2 + wander.drift_over(apart) + MARGIN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::{LocalStamp, NtpTimestamp, PcTime};

    const BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);

    /// 時刻 `at_ms` に往復 `delay_us` で受け取った標本。サーバ時刻は PC の時刻 + `offset`。
    fn sample(at_ms: i64, delay_us: i64, offset: TimeDelta) -> Sample {
        let at = TimeDelta::from_millis(at_ms);
        let delay = TimeDelta::from_micros(delay_us);
        let mono = |time: TimeDelta| PcTime::monotonic(time.as_nanos().cast_unsigned());
        let server = BASE.wrapping_add(at + delay / 2 + offset);
        let sent = LocalStamp {
            mono: mono(at),
            wall: BASE,
        };
        let received = LocalStamp {
            mono: mono(at + delay),
            wall: BASE,
        };
        Sample::new(sent, server, server, received).unwrap()
    }

    #[test]
    fn same_timeline_is_continuous() {
        let first = sample(0, 200, TimeDelta::ZERO);
        let second = sample(500, 300, TimeDelta::ZERO);
        assert_eq!(
            residual(&first, &second, FrequencyOffset::ZERO),
            Some(TimeDelta::ZERO)
        );
        assert!(is_continuous(&first, &second, None));
    }

    #[test]
    fn detects_a_step_larger_than_the_tolerance() {
        let first = sample(0, 200, TimeDelta::ZERO);
        let stepped = sample(500, 200, TimeDelta::from_millis(-300));
        assert_eq!(
            residual(&first, &stepped, FrequencyOffset::ZERO),
            Some(TimeDelta::from_millis(-300))
        );
        assert!(!is_continuous(&first, &stepped, None));
    }

    #[test]
    fn allows_for_delay_and_clock_drift() {
        let first = sample(0, 2_000, TimeDelta::ZERO);
        // 遅延の半分ずつ（1ms + 1ms）+ 余裕 1ms = 3ms までは同じ流れ
        assert!(is_continuous(
            &first,
            &sample(500, 2_000, TimeDelta::from_micros(2_900)),
            Some(FrequencyOffset::ZERO)
        ));
        assert!(!is_continuous(
            &first,
            &sample(500, 2_000, TimeDelta::from_micros(3_100)),
            Some(FrequencyOffset::ZERO)
        ));
        // 周波数の推定がなければ 500 ppm を見込む: 60 秒離れていれば 30ms のずれまで許す
        let later = sample(60_000, 2_000, TimeDelta::from_millis(20));
        assert!(is_continuous(&first, &later, None));
        assert!(!is_continuous(&first, &later, Some(FrequencyOffset::ZERO)));
    }
}
