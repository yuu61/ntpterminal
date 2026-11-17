//! 直近の測定値の推移: 2 つの系列の小さい棒グラフ。
//!
//! - NTP: 成功した測定（`SyncSnapshot::last_sample`）のオフセットと遅延を 1 回ずつ貯める。情報欄の値は
//!   クロックフィルタが選んだ標本なので、最新の棒と一致しないことがある。見出しに「測定値」と書いて区別する
//! - ptp4l の PHC: 読めた ptp4l の状態の、マスタとの差（offsetFromMaster）と経路の遅延を 1 回ずつ貯める。
//!   PTP の同期の良さはこちらに出る（PC との差は、PC の時計の合わせ方で決まる）
//! - 棒は [`big_digits`](super::big_digits) と同じく、背景色付きの空白と `▄` で描く。`▁▂▃` などの
//!   ブロック文字・点字・ratatui の `Sparkline`（中で `▁▂▃` を使う）は使わない。East Asian Ambiguous
//!   （端末の設定で幅が 1 にも 2 にもなる）で、X のない Linux コンソールのフォントにも入っていないため
//! - 縦の段は、出している範囲の最小〜最大で決める。最小の値も 1 段で描き、空白にしない
//! - 出すかどうかは画面の側で決める（出しても大きい時計の倍率が変わらないときだけ）

use alloc::collections::VecDeque;

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;

use domain::shared_kernel::TimeDelta;

use super::{SourceSnapshot, theme};
use crate::format::ptp::{format_nanos, format_nanos_signed};
use crate::format::{format_delay_ms, format_offset_ms};

/// 貯める測定値の数。棒は常にこの本数分の幅を取り、新しい値を右端に置く。
const BARS: u16 = 60;
/// 1 系列の高さ（行）。1 行を上下 2 段に分けるので、段は 1〜`2 × ROWS_PER_SERIES`。
const ROWS_PER_SERIES: u16 = 2;
const TOP_LEVEL: u16 = ROWS_PER_SERIES * 2;
/// 範囲の表示（"-12.345 ms"）に取る幅。値の桁数でブロックが左右に揺れないよう、固定の幅で中央に置く。
const RANGE_WIDTH: u16 = 10;
/// 系列名と棒の間の空白
const LABEL_GAP: u16 = 2;
/// 棒と範囲の表示の間の空白
const RANGE_GAP: u16 = 1;

/// 履歴の 2 つの系列の種類。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Series {
    /// NTP のオフセットと遅延
    #[default]
    Ntp,
    /// ptp4l のマスタとの差と経路の遅延
    Ptp4l,
    /// PTP のマスタと PC の差（マスタ − PC）と経路の遅延（片道）
    PtpMaster,
}

/// 成功した測定値の履歴（古い順、最大 [`BARS`] 個）。
#[derive(Clone, Debug, Default)]
pub(super) struct SampleHistory {
    series: Series,
    /// （1 つ目の系列, 2 つ目の系列）
    values: VecDeque<(TimeDelta, TimeDelta)>,
    /// 最後に取り込んだときの成功回数
    seen: u64,
}

impl SampleHistory {
    /// スナップショットを 1 つ取り込む。成功回数が増えていれば、その測定値を足す。
    ///
    /// スナップショットはイベントごとに届くので、1 回の成功で同じ内容のものが何度も来る。
    /// 二重に数えないよう、測定値ではなく成功回数で判定する。
    pub(super) fn observe(&mut self, snapshot: &SourceSnapshot) {
        let (series, count, value) = if let Some(ntp) = snapshot.ntp() {
            (
                Series::Ntp,
                ntp.stats.successes,
                ntp.last_sample
                    .map(|sample| (sample.offset().delta(), sample.delay().delta())),
            )
        } else if let Some(phc) = snapshot.phc() {
            (
                Series::Ptp4l,
                phc.stats.status_updates,
                phc.status
                    .as_ref()
                    .map(|status| (status.offset_from_master, status.mean_path_delay)),
            )
        } else if let Some(master) = snapshot.ptp() {
            (
                Series::PtpMaster,
                master.stats.samples,
                master
                    .last_sample
                    .map(|sample| (sample.offset().delta(), sample.delay().delta() / 2)),
            )
        } else {
            return;
        };
        if count <= self.seen {
            return;
        }
        self.series = series;
        self.seen = count;
        let Some(pair) = value else {
            return;
        };
        if self.values.len() >= usize::from(BARS) {
            let _: Option<(TimeDelta, TimeDelta)> = self.values.pop_front();
        }
        self.values.push_back(pair);
    }
}

/// 1 系列分のグラフ（画面に出す内容）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Trend {
    pub label: &'static str,
    /// 古い順の棒の高さ（半行単位。1〜`2 × ROWS_PER_SERIES`）
    pub levels: Vec<u16>,
    pub max: String,
    pub min: String,
}

/// 履歴から 2 つの系列のグラフを作る。まだ測定値がなければ空。
pub(super) fn trends(history: &SampleHistory) -> Vec<Trend> {
    if history.values.is_empty() {
        return Vec::new();
    }
    let firsts: Vec<TimeDelta> = history.values.iter().map(|&(first, _)| first).collect();
    let seconds: Vec<TimeDelta> = history.values.iter().map(|&(_, second)| second).collect();
    match history.series {
        Series::Ntp => vec![
            trend("オフセット", &firsts, format_offset_ms),
            trend("遅延", &seconds, format_delay_ms),
        ],
        Series::Ptp4l => vec![
            trend("マスタとの差", &firsts, format_nanos_signed),
            trend("経路遅延", &seconds, format_nanos),
        ],
        Series::PtpMaster => vec![
            trend("PC との差", &firsts, format_nanos_signed),
            trend("経路遅延", &seconds, format_nanos),
        ],
    }
}

fn trend(label: &'static str, values: &[TimeDelta], format: fn(TimeDelta) -> String) -> Trend {
    let min = values.iter().copied().min().unwrap_or_default();
    let max = values.iter().copied().max().unwrap_or_default();
    Trend {
        label,
        levels: levels(values, min, max),
        max: format(max),
        min: format(min),
    }
}

/// 最小を 1 段、最大を最高の段として比例で割り当てる（四捨五入）。全部同じ値なら 1 段。
fn levels(values: &[TimeDelta], min: TimeDelta, max: TimeDelta) -> Vec<u16> {
    let steps = i128::from(TOP_LEVEL - 1);
    let span = i128::from(max.as_nanos()) - i128::from(min.as_nanos());
    if span <= 0 {
        return vec![1; values.len()];
    }
    values
        .iter()
        .map(|value| {
            let above = i128::from(value.as_nanos()) - i128::from(min.as_nanos());
            let step = (above * steps + span / 2) / span;
            1 + u16::try_from(step).unwrap_or(0).min(TOP_LEVEL - 1)
        })
        .collect()
}

/// 見出し 1 行と、系列ごとの行の合計。グラフがなければ 0。
pub(super) fn height(trends: &[Trend]) -> u16 {
    if trends.is_empty() {
        return 0;
    }
    u16::try_from(trends.len())
        .unwrap_or(u16::MAX)
        .saturating_mul(ROWS_PER_SERIES)
        .saturating_add(1)
}

/// ブロック全体の幅（範囲の表示は [`RANGE_WIDTH`] として数える）。
pub(super) fn width(trends: &[Trend]) -> u16 {
    label_width(trends)
        .saturating_add(BARS)
        .saturating_add(RANGE_GAP)
        .saturating_add(RANGE_WIDTH)
}

fn label_width(trends: &[Trend]) -> u16 {
    let widest = trends
        .iter()
        .map(|trend| Span::raw(trend.label).width())
        .max()
        .unwrap_or(0);
    u16::try_from(widest)
        .unwrap_or(u16::MAX)
        .saturating_add(LABEL_GAP)
}

/// 見出しとグラフを、`y` の行から下に、ブロック全体を左右中央に置いて描く。
pub(super) fn render(trends: &[Trend], area: Rect, y: u16, buf: &mut Buffer) {
    let x = area.x + area.width.saturating_sub(width(trends)) / 2;
    let bars_x = x.saturating_add(label_width(trends));
    let range_x = bars_x.saturating_add(BARS).saturating_add(RANGE_GAP);
    let count = trends.first().map_or(0, |trend| trend.levels.len());
    put(buf, area, x, y, &format!("直近 {count} 回の測定値"));

    let mut top = y.saturating_add(1);
    for trend in trends {
        let bottom = top.saturating_add(ROWS_PER_SERIES - 1);
        put(buf, area, x, top, trend.label);
        put(buf, area, range_x, top, &trend.max);
        put(buf, area, range_x, bottom, &trend.min);
        draw_bars(&trend.levels, bars_x, top, area, buf);
        top = top.saturating_add(ROWS_PER_SERIES);
    }
}

/// ラベルの色で 1 行を描く。`area` の外なら描かない。
fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str) {
    if x >= area.right() || y < area.y || y >= area.bottom() {
        return;
    }
    let span = Span::styled(text, Style::new().fg(theme::LABEL));
    let _: (u16, u16) = buf.set_span(x, y, &span, area.right() - x);
}

/// 棒を右詰めで描く。各行は下から 2 段ずつ受け持ち、2 段とも埋まれば背景色付きの空白、
/// 下の 1 段だけなら `▄` にする。
fn draw_bars(levels: &[u16], x: u16, top: u16, area: Rect, buf: &mut Buffer) {
    let shown = levels.len().min(usize::from(BARS));
    let first_column = x.saturating_add(BARS - u16::try_from(shown).unwrap_or(BARS));
    let newest = levels.get(levels.len() - shown..).unwrap_or_default();
    for (&level, index) in newest.iter().zip(0u16..) {
        let column = first_column.saturating_add(index);
        for row in 0..ROWS_PER_SERIES {
            let below = (ROWS_PER_SERIES - 1 - row) * 2;
            let position = (column, top.saturating_add(row));
            if column >= area.right() || position.1 < area.y || position.1 >= area.bottom() {
                continue;
            }
            let Some(cell) = buf.cell_mut(position) else {
                continue;
            };
            let _: &mut Cell = match level.saturating_sub(below) {
                0 => cell,
                1 => cell.set_symbol("▄").set_fg(theme::TREND),
                _ => cell.set_symbol(" ").set_bg(theme::TREND),
            };
        }
    }
}

#[cfg(test)]
pub(super) mod testing {
    use application::snapshot::SyncSnapshot;
    use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};
    use domain::time_sync::{ExchangeStats, Sample, SyncState};

    use super::{SampleHistory, SourceSnapshot};

    /// オフセット `offset_us`、往復遅延 `delay_us`（どちらもマイクロ秒）の測定値。
    pub(in super::super) fn sample(offset_us: i64, delay_us: i64) -> Sample {
        // T2 = T3。offset = T3 − T1 − delay / 2 になるよう T1 の壁時計を決める
        let t3 = NtpTimestamp::from_parts(3_900_000_000, 0);
        let delay = TimeDelta::from_micros(delay_us);
        let t1_wall = t3.wrapping_add(TimeDelta::from_nanos(
            -offset_us * 1_000 - delay.as_nanos() / 2,
        ));
        let sent = LocalStamp {
            mono: PcTime::monotonic(1_000_000_000),
            wall: t1_wall,
        };
        let received = LocalStamp {
            mono: PcTime::monotonic(1_000_000_000 + delay.as_nanos().cast_unsigned()),
            wall: t1_wall.wrapping_add(delay),
        };
        Sample::new(sent, t3, t3, received).unwrap()
    }

    /// 成功が `successes` 回目で、最後の測定値が `sample(offset_us, delay_us)` のスナップショット。
    pub(in super::super) fn snapshot(
        successes: u64,
        offset_us: i64,
        delay_us: i64,
    ) -> SyncSnapshot {
        let sample = sample(offset_us, delay_us);
        SyncSnapshot {
            state: SyncState::Synchronized,
            stats: ExchangeStats {
                attempts: successes,
                successes,
                ..ExchangeStats::default()
            },
            poll_interval: TimeDelta::from_millis(500),
            anchor: None,
            last_sample: Some(sample),
            selected_sample: Some(sample),
            frequency: None,
            server_info: None,
            last_success: None,
            next_poll: None,
            awaiting_reply: false,
        }
    }

    /// `(offset_us, delay_us)` を古い順に 1 回ずつ成功させた履歴。
    pub(in super::super) fn history(values: &[(i64, i64)]) -> SampleHistory {
        let mut history = SampleHistory::default();
        for (&(offset_us, delay_us), successes) in values.iter().zip(1u64..) {
            history.observe(&SourceSnapshot::Ntp(Box::new(snapshot(
                successes, offset_us, delay_us,
            ))));
        }
        history
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::display_width;
    use application::snapshot::PhcSnapshot;
    use domain::ptp_clock::{
        ClockIdentity, ClockQuality, LeapWarning, PhcState, PhcStats, PtpStatus, TimeProperties,
        TimeSourceKind, Timescale,
    };
    use domain::shared_kernel::PcTime;

    /// ptp4l の状態を `updates` 回目に読んだ、マスタとの差が `offset_ns` のスナップショット。
    fn phc_snapshot(updates: u64, offset_ns: i64) -> SourceSnapshot {
        SourceSnapshot::Phc(Box::new(PhcSnapshot {
            state: PhcState::Synchronized,
            stats: PhcStats {
                attempts: updates,
                status_updates: updates,
                ..PhcStats::default()
            },
            poll_interval: TimeDelta::from_millis(500),
            anchor: None,
            last_sample: None,
            selected_sample: None,
            frequency: None,
            status: Some(PtpStatus {
                domain: 0,
                ports: Vec::new(),
                offset_from_master: TimeDelta::from_nanos(offset_ns),
                mean_path_delay: TimeDelta::from_nanos(1_234),
                steps_removed: 1,
                grandmaster: ClockIdentity::default(),
                grandmaster_quality: ClockQuality::default(),
                time: TimeProperties {
                    utc_offset: 37,
                    utc_offset_valid: true,
                    timescale: Timescale::Ptp,
                    leap: LeapWarning::None,
                    time_traceable: true,
                    frequency_traceable: true,
                    time_source: TimeSourceKind::Gnss,
                },
            }),
            status_error: None,
            last_success: None,
            next_poll: PcTime::monotonic(0),
        }))
    }

    /// ptp4l の PHC: 読めた状態ごとに、マスタとの差と経路の遅延を貯める（ナノ秒で出す）。
    #[test]
    fn ptp4l_series_use_the_master_offset() {
        let mut history = SampleHistory::default();
        for (updates, offset_ns) in [(1u64, -12i64), (1, -12), (2, 30), (3, -5)] {
            history.observe(&phc_snapshot(updates, offset_ns));
        }
        let built = trends(&history);
        assert_eq!(built[0].label, "マスタとの差");
        assert_eq!(
            (built[0].max.as_str(), built[0].min.as_str()),
            ("+30 ns", "-12 ns")
        );
        assert_eq!(built[0].levels.len(), 3, "同じ状態は 1 回だけ数える");
        assert_eq!(built[1].label, "経路遅延");
        assert_eq!(built[1].max, "1234 ns");
    }

    fn offsets_us(history: &SampleHistory) -> Vec<i64> {
        history
            .values
            .iter()
            // NTP の時刻の分解能（約 0.23ns）でずれるので、四捨五入してマイクロ秒にする
            .map(|&(offset, _)| (offset.as_nanos() + 500).div_euclid(1_000))
            .collect()
    }

    fn millis(values: &[i64]) -> Vec<TimeDelta> {
        values.iter().copied().map(TimeDelta::from_millis).collect()
    }

    #[test]
    fn adds_each_success_only_once() {
        let mut history = SampleHistory::default();
        // 1 回の成功で、イベントの数だけ同じスナップショットが届く
        let first = testing::snapshot(1, -978, 423);
        for _ in 0u8..3 {
            history.observe(&SourceSnapshot::Ntp(Box::new(first)));
        }
        // 失敗したときのスナップショット（成功回数は変わらない）
        let mut failed = first;
        failed.stats.attempts += 1;
        failed.stats.timeouts += 1;
        history.observe(&SourceSnapshot::Ntp(Box::new(failed)));
        history.observe(&SourceSnapshot::Ntp(Box::new(testing::snapshot(
            2, -950, 400,
        ))));
        assert_eq!(offsets_us(&history), [-978, -950]);
    }

    #[test]
    fn keeps_only_the_latest_samples() {
        let values: Vec<(i64, i64)> = (1..=61).map(|offset| (offset, 400)).collect();
        let history = testing::history(&values);
        let offsets = offsets_us(&history);
        assert_eq!(offsets.len(), usize::from(BARS));
        assert_eq!(offsets.first(), Some(&2));
        assert_eq!(offsets.last(), Some(&61));
    }

    #[test]
    fn levels_span_from_min_to_max() {
        let values = millis(&[-3, -1, -2, -3]);
        let (min, max) = (TimeDelta::from_millis(-3), TimeDelta::from_millis(-1));
        // -2 は (1 × 3) / 2 = 1.5 段 → 四捨五入で 2 段上
        assert_eq!(levels(&values, min, max), [1, 4, 3, 1]);
    }

    #[test]
    fn equal_values_are_drawn_at_the_lowest_level() {
        let values = millis(&[5, 5, 5]);
        let five = TimeDelta::from_millis(5);
        assert_eq!(levels(&values, five, five), [1, 1, 1]);
        assert_eq!(levels(&values[..1], five, five), [1]);
    }

    #[test]
    fn builds_offset_and_delay_with_their_range() {
        let history = testing::history(&[(-978, 423), (-950, 400), (-1_020, 452)]);
        let built = trends(&history);
        let offset = &built[0];
        assert_eq!(offset.label, "オフセット");
        assert_eq!(
            (offset.max.as_str(), offset.min.as_str()),
            ("-0.950 ms", "-1.020 ms")
        );
        assert_eq!(offset.levels, [3, 4, 1]);
        let delay = &built[1];
        assert_eq!(delay.label, "遅延");
        assert_eq!(
            (delay.max.as_str(), delay.min.as_str()),
            ("0.452 ms", "0.400 ms")
        );
        assert_eq!(delay.levels, [2, 1, 4]);
        assert_eq!(trends(&SampleHistory::default()), Vec::<Trend>::new());
    }

    /// 棒のセルを文字にする: 背景色付きの空白 → `#`、`▄` → `▄`、何もない → 空白。
    fn bar_cells(buf: &Buffer, x: u16, y: u16, count: u16) -> String {
        (x..x + count)
            .map(|column| {
                let cell = &buf[(column, y)];
                match cell.symbol() {
                    " " if cell.bg == theme::TREND => '#',
                    "▄" if cell.fg == theme::TREND => '▄',
                    _ => ' ',
                }
            })
            .collect()
    }

    #[test]
    fn draws_bars_right_aligned_with_half_cells() {
        let built = [Trend {
            label: "遅延",
            levels: vec![1, 2, 3, 4],
            max: "0.452 ms".into(),
            min: "0.400 ms".into(),
        }];
        let area = Rect::new(0, 0, width(&built), height(&built));
        let mut buf = Buffer::empty(area);
        render(&built, area, 0, &mut buf);

        let bars_x = label_width(&built);
        // 右端の 4 本だけが描かれ、その左は空いている
        assert_eq!(bar_cells(&buf, bars_x, 1, BARS - 4).trim(), "");
        assert_eq!(bar_cells(&buf, bars_x, 2, BARS - 4).trim(), "");
        // 上の行は 3 段目から、下の行は 1 段目から埋まる
        assert_eq!(bar_cells(&buf, bars_x + BARS - 4, 1, 4), "  ▄#");
        assert_eq!(bar_cells(&buf, bars_x + BARS - 4, 2, 4), "▄###");

        // 全角文字の後ろの詰め物のセルは飛ばす
        let text = |y: u16| -> String {
            let mut line = String::new();
            let mut skip = 0;
            for x in 0..area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let symbol = buf[(x, y)].symbol();
                line.push_str(symbol);
                skip = display_width(symbol).saturating_sub(1);
            }
            line
        };
        assert!(text(0).starts_with("直近 4 回の測定値"));
        assert!(text(1).starts_with("遅延"));
        assert!(text(1).trim_end().ends_with("0.452 ms"));
        assert!(text(2).trim_end().ends_with("0.400 ms"));
    }

    #[test]
    fn clipped_area_does_not_panic() {
        let built = trends(&testing::history(&[(-978, 423), (-950, 400)]));
        for (width, height) in [(0, 0), (1, 1), (10, 3), (40, 5)] {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            render(&built, area, 0, &mut buf);
        }
    }
}
