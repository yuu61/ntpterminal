//! 表示用の時計: 大きい時計に出す時刻を、逆戻りさせずに基準点へ追従させる。

use super::Anchor;
use crate::shared_kernel::{NtpTimestamp, PcTime, TimeDelta};

/// 1 フレームで補正してよい量の上限（経過時間に対する割合、%）。
/// 100% 未満なので、表示は経過時間の半分以上の速さで必ず前に進む（逆戻りしない）。
const MAX_SLEW_PERCENT: i64 = 50;

/// これより大きいずれは追従せず、一度に合わせる。
/// 逆戻りが起こりうる唯一の例外（サーバの時刻が大きく変わった場合）。
const STEP_THRESHOLD: TimeDelta = TimeDelta::from_secs(1);

/// 大きい時計に表示する時刻を決める。
///
/// 基準点（[`Anchor`]）はクロックフィルタの選択が変わるたびに切り替わり、そのまま表示すると
/// 数十 µs〜数 ms 前後に飛ぶ。ここでは表示を経過時間ぶん進めたうえで、基準点から求めた時刻との
/// ずれを少しずつ吸収する。
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayClock {
    last: Option<Shown>,
}

#[derive(Clone, Copy, Debug)]
struct Shown {
    mono: PcTime,
    time: NtpTimestamp,
}

impl DisplayClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// `now`（モノトニック時計）の瞬間に表示する時刻。基準点がまだなければ `None`。
    pub fn tick(&mut self, anchor: Option<&Anchor>, now: PcTime) -> Option<NtpTimestamp> {
        let target = anchor?.project(now).ok()?;
        let shown = match self.last {
            None => target,
            Some(last) => match now.since(last.mono) {
                Ok(elapsed) if !elapsed.is_negative() => {
                    let natural = last.time.wrapping_add(elapsed);
                    let error = target.wrapping_since(natural);
                    if error.abs() > STEP_THRESHOLD {
                        target
                    } else {
                        let limit = elapsed * MAX_SLEW_PERCENT / 100;
                        natural.wrapping_add(error.clamp(-limit, limit))
                    }
                }
                // 時刻が進んでいない（同じ瞬間に 2 回呼ばれた）: 前回と同じ値を出す
                Ok(_) => last.time,
                Err(_) => target,
            },
        };
        self.last = Some(Shown {
            mono: now,
            time: shown,
        });
        Some(shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::LocalStamp;
    use crate::time_sync::Sample;

    const SERVER_BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);
    const FRAME: TimeDelta = TimeDelta::from_millis(16);

    /// 固定小数との変換で生じる 1ns 未満の丸め誤差を許す比較。
    fn assert_close(actual: TimeDelta, expected: TimeDelta) {
        let diff = (actual - expected).abs();
        assert!(
            diff <= TimeDelta::from_nanos(2),
            "actual {actual:?}, expected {expected:?}"
        );
    }

    fn mono(nanos: i64) -> PcTime {
        PcTime::monotonic(nanos as u64)
    }

    /// モノトニック時計 `at_nanos` の瞬間にサーバ時刻が `SERVER_BASE + server_offset` だった、という基準点。
    fn anchor(at_nanos: i64, server_offset: TimeDelta) -> Anchor {
        let server_time = SERVER_BASE.wrapping_add(server_offset);
        let stamp = LocalStamp {
            mono: mono(at_nanos),
            wall: server_time,
        };
        let sample = Sample::new(stamp, server_time, server_time, stamp).unwrap();
        Anchor::from_sample(&sample).unwrap()
    }

    #[test]
    fn shows_nothing_before_first_anchor() {
        assert_eq!(DisplayClock::new().tick(None, mono(0)), None);
    }

    #[test]
    fn first_tick_shows_projection_exactly() {
        let a = anchor(0, TimeDelta::ZERO);
        let mut clock = DisplayClock::new();
        let now = mono(250_000_000);
        assert_eq!(clock.tick(Some(&a), now), Some(a.project(now).unwrap()));
    }

    /// 基準点が 5ms 過去側にずれても、表示は逆戻りせずに追いつく。
    #[test]
    fn never_runs_backwards_when_anchor_moves_back() {
        let old = anchor(0, TimeDelta::ZERO);
        let new = anchor(0, TimeDelta::from_millis(-5));
        let mut clock = DisplayClock::new();
        let mut now = TimeDelta::ZERO;
        let mut previous = clock.tick(Some(&old), mono(0)).unwrap();
        for _ in 0..20 {
            now = now + FRAME;
            let shown = clock.tick(Some(&new), mono(now.as_nanos())).unwrap();
            assert!(!shown.wrapping_since(previous).is_negative(), "逆戻りした");
            previous = shown;
        }
        // 16ms × 50% = 8ms ずつ吸収できるので、1 フレームで追いついている
        let target = new.project(mono(now.as_nanos())).unwrap();
        assert_close(previous.wrapping_since(target), TimeDelta::ZERO);
    }

    /// 補正量は経過時間の 50% までなので、大きめのずれは数フレームかけて吸収する。
    #[test]
    fn spreads_larger_corrections_over_several_frames() {
        let old = anchor(0, TimeDelta::ZERO);
        let new = anchor(0, TimeDelta::from_millis(-30));
        let mut clock = DisplayClock::new();
        let first = clock.tick(Some(&old), mono(0)).unwrap();
        let after_one = clock.tick(Some(&new), mono(FRAME.as_nanos())).unwrap();
        // 16ms 経過して 8ms だけ進んだ（逆戻りはしない）
        assert_close(after_one.wrapping_since(first), TimeDelta::from_millis(8));

        let mut shown = after_one;
        for frame in 2..=5 {
            shown = clock
                .tick(Some(&new), mono(FRAME.as_nanos() * frame))
                .unwrap();
        }
        let target = new.project(mono(FRAME.as_nanos() * 5)).unwrap();
        assert_close(shown.wrapping_since(target), TimeDelta::ZERO);
    }

    #[test]
    fn catches_up_forward_as_well() {
        let old = anchor(0, TimeDelta::ZERO);
        let new = anchor(0, TimeDelta::from_millis(4));
        let mut clock = DisplayClock::new();
        clock.tick(Some(&old), mono(0));
        let shown = clock.tick(Some(&new), mono(FRAME.as_nanos())).unwrap();
        let target = new.project(mono(FRAME.as_nanos())).unwrap();
        assert_close(shown.wrapping_since(target), TimeDelta::ZERO);
    }

    #[test]
    fn jumps_when_error_exceeds_threshold() {
        let old = anchor(0, TimeDelta::ZERO);
        let new = anchor(0, TimeDelta::from_secs(-3));
        let mut clock = DisplayClock::new();
        clock.tick(Some(&old), mono(0));
        let now = mono(FRAME.as_nanos());
        let shown = clock.tick(Some(&new), now).unwrap();
        assert_close(
            shown.wrapping_since(new.project(now).unwrap()),
            TimeDelta::ZERO,
        );
    }

    #[test]
    fn repeated_tick_at_same_instant_is_stable() {
        let a = anchor(0, TimeDelta::ZERO);
        let mut clock = DisplayClock::new();
        let first = clock.tick(Some(&a), mono(100));
        assert_eq!(clock.tick(Some(&a), mono(100)), first);
    }
}
