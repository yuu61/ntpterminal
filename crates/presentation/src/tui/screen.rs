//! 画面の内容（[`ScreenModel`]）と描画。
//!
//! 内容の組み立て（[`build`]）と描画（`Widget for &ScreenModel`）を分け、どちらも
//! 端末なしでテストできるようにしている。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use application::snapshot::{ReceiverSnapshot, SyncSnapshot};
use application::timestamping::TimestampSource;
use domain::shared_kernel::{NtpTimestamp, PcTime, TimeDelta};
use domain::time_sync::SyncState;

use super::TuiConfig;
use super::big_digits::{self, BigText};
use super::trend::{self, SampleHistory, Trend};
use super::{receiver, theme};
use crate::format::{
    format_clock, format_date, format_delay_ms, format_offset_ms, state_label, timestamping_label,
};

/// 同期前の時計の表示。
const CLOCK_PLACEHOLDER: &str = "--:--:--.---";
/// 値がまだないときの表示。
const NO_VALUE: &str = "--";
/// 状態の前に付ける印。
/// 注意: `●` は East Asian Ambiguous。端末の「曖昧幅」が半角（既定）であることを前提にしている。
const STATUS_MARK: &str = "●";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InfoRow {
    pub label: &'static str,
    pub value: String,
    pub color: Color,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenModel {
    pub title: String,
    pub server: String,
    /// 大きい時計の文字列（"17:25:45.146"）
    pub clock: String,
    /// "2026-11-15 (日)"。同期前は `None`
    pub date: Option<String>,
    pub zone_label: String,
    pub status: String,
    pub status_color: Color,
    pub rows: Vec<InfoRow>,
    /// 測定値の推移。まだ測定値がなければ空
    pub trends: Vec<Trend>,
    pub footer: Option<String>,
}

/// スナップショットと現在時刻から、画面に出す内容を組み立てる。
///
/// - `receiver`: 受信機の監視の最新のスナップショット（`config.receiver` のときだけ使う。まだ届いていなければ `None`）
/// - `history`: 推移のグラフに使う、これまでの測定値（`config.trend` のときだけ使う）
/// - `shown`: 大きい時計に出す時刻（[`domain::time_sync::DisplayClock`] の結果）
/// - `now`: モノトニック時計の現在値（「何秒前」「何秒後」の計算に使う）
pub(super) fn build(
    config: &TuiConfig,
    snapshot: Option<&SyncSnapshot>,
    receiver: Option<&ReceiverSnapshot>,
    history: &SampleHistory,
    shown: Option<NtpTimestamp>,
    now: PcTime,
) -> ScreenModel {
    let state = snapshot.map_or(SyncState::Unsynchronized, |snap| snap.state);
    let (status, status_color) = match state {
        SyncState::Synchronized => (state_label(state).to_owned(), theme::GOOD),
        SyncState::Unsynchronized => ("応答待ち（未同期）".to_owned(), theme::LABEL),
        SyncState::Holdover => (state_label(state).to_owned(), theme::WARN),
        SyncState::Denied => (state_label(state).to_owned(), theme::BAD),
    };
    ScreenModel {
        title: config.title.clone(),
        server: config.server_label.clone(),
        clock: shown.map_or_else(
            || CLOCK_PLACEHOLDER.to_owned(),
            |time| format_clock(time, &config.zone),
        ),
        date: shown.map(|time| format_date(time, &config.zone)),
        zone_label: config.zone.label().to_owned(),
        status,
        status_color,
        rows: info_rows(config, snapshot, receiver, now),
        trends: if config.trend {
            trend::trends(history)
        } else {
            Vec::new()
        },
        footer: config.footer.clone(),
    }
}

/// 受信機の行（`config.receiver` のとき）は、基準時計に関わる「Ref ID」の後ろに置く。
fn info_rows(
    config: &TuiConfig,
    snapshot: Option<&SyncSnapshot>,
    receiver: Option<&ReceiverSnapshot>,
    now: PcTime,
) -> Vec<InfoRow> {
    // offset と delay は、基準点と同じ「クロックフィルタが選んだ標本」の値を出す（生の値は揺れる）
    let selected = snapshot.and_then(|snap| snap.selected_sample);
    let last = snapshot.and_then(|snap| snap.last_sample);
    let info = snapshot.and_then(|snap| snap.server_info);

    let row = |label, value: Option<String>, color| InfoRow {
        label,
        value: value.unwrap_or_else(|| NO_VALUE.to_owned()),
        color,
    };
    let offset = selected.map(|sample| sample.offset().delta());
    let delay = selected.map(|sample| sample.delay().delta());
    let next_poll = match snapshot {
        Some(snap) if snap.state == SyncState::Denied => Some("停止中".to_owned()),
        Some(snap) => snap
            .next_poll
            .and_then(|at| at.since(now).ok())
            .map(|remaining| format!("{:.1} 秒後", remaining.max(TimeDelta::ZERO).as_secs_f64())),
        None => None,
    };

    let mut rows = vec![
        row(
            "オフセット",
            offset.map(format_offset_ms),
            offset.map_or(theme::TEXT, tone),
        ),
        row(
            "遅延",
            delay.map(format_delay_ms),
            delay.map_or(theme::TEXT, tone),
        ),
        row(
            "Stratum",
            info.map(|i| i.stratum.value().to_string()),
            if info.is_some_and(|i| i.stratum.is_primary()) {
                theme::GOOD
            } else {
                theme::TEXT
            },
        ),
        row(
            "Ref ID",
            info.map(|i| i.reference_id.to_string()),
            theme::TEXT,
        ),
    ];
    if config.receiver {
        rows.extend(receiver::rows(receiver, now));
    }
    rows.extend([
        row(
            "NTP version",
            info.map(|i| i.version.to_string()),
            theme::TEXT,
        ),
        row(
            "サーバ時刻",
            last.map(|sample| format_clock(sample.server_transmit(), &config.zone)),
            theme::TEXT,
        ),
        row(
            "最終同期",
            snapshot
                .and_then(|snap| snap.last_success)
                .and_then(|at| now.since(at).ok())
                .map(|elapsed| format!("{:.1} 秒前", elapsed.max(TimeDelta::ZERO).as_secs_f64())),
            theme::TEXT,
        ),
        row("次回取得", next_poll, theme::TEXT),
        row(
            "成功/試行",
            snapshot.map(|snap| format!("{}/{}", snap.stats.successes, snap.stats.attempts)),
            theme::TEXT,
        ),
        row("送信方式", Some(config.transport.clone()), theme::LABEL),
        {
            let counts = config.timestamping.counts();
            // カーネルかハードウェアの時刻が、設定どおりに取れているときだけ「良好」の色にする
            let color = if counts.last_source != TimestampSource::User
                && counts.last_source == counts.expected_source()
            {
                theme::GOOD
            } else {
                theme::LABEL
            };
            row(
                "タイムスタンプ",
                Some(timestamping_label(&counts).to_owned()),
                color,
            )
        },
    ]);
    rows
}

/// ずれの大きさで色を変える（1ms 未満: 良好、10ms 未満: 注意、それ以上: 異常）。
fn tone(delta: TimeDelta) -> Color {
    let abs = delta.abs();
    if abs < TimeDelta::from_millis(1) {
        theme::GOOD
    } else if abs < TimeDelta::from_millis(10) {
        theme::WARN
    } else {
        theme::BAD
    }
}

/// "NTP CLOCK" → "N T P   C L O C K"
fn letter_spaced(text: &str) -> String {
    text.chars().map(String::from).collect::<Vec<_>>().join(" ")
}

fn width_of(line: &Line<'_>) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

/// 1 行を左右中央に描く。`area` の外なら描かない。
fn put_centered(buf: &mut Buffer, area: Rect, y: u16, line: &Line<'_>) {
    if y < area.y || y >= area.bottom() {
        return;
    }
    let x = area.x + area.width.saturating_sub(width_of(line)) / 2;
    let _: (u16, u16) = buf.set_line(x, y, line, area.right().saturating_sub(x));
}

/// 座標の計算はすべて `u16` の飽和演算（`saturating_*`）で行う。端末を極端に縮めてもパニックしないように。
impl Widget for &ScreenModel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // 縦の並び: 見出し / サーバ / 空行 / 時計 / 空行 / 日付 / 状態 / 情報欄 /
        //           (空行 / 推移) / (空行 / 最下部の一文)
        let rows = u16::try_from(self.rows.len()).unwrap_or(u16::MAX);
        let footer_rows = if self.footer.is_some() { 2 } else { 0 };
        let base = 6u16.saturating_add(rows).saturating_add(footer_rows);

        // 時計は残りの高さと幅に収まる最大の倍率で描く。倍率 1 でも入らなければ普通の文字で出す
        let fit = |extra: u16| {
            big_digits::fit_scale(
                &self.clock,
                area.width.saturating_sub(2),
                area.height.saturating_sub(base.saturating_add(extra)),
            )
        };
        let scale = fit(0);
        // 推移は余った場所にだけ出す。出すと時計が縮む・普通の文字になる・横に入らないなら出さない
        let trend_rows = trend::height(&self.trends).saturating_add(1);
        let show_trends = !self.trends.is_empty()
            && scale.is_some()
            && fit(trend_rows) == scale
            && trend::width(&self.trends) <= area.width;
        let fixed = if show_trends {
            base.saturating_add(trend_rows)
        } else {
            base
        };
        let clock_height = scale.map_or(1, |factor| big_digits::size(&self.clock, factor).1);
        let total = fixed.saturating_add(clock_height);
        let mut y = area.y + area.height.saturating_sub(total) / 2;

        let accent = Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD);
        put_centered(
            buf,
            area,
            y,
            &Line::styled(letter_spaced(&self.title), accent),
        );
        y = y.saturating_add(1);
        put_centered(
            buf,
            area,
            y,
            &Line::styled(self.server.as_str(), Style::new().fg(theme::LABEL)),
        );
        y = y.saturating_add(2);

        match scale {
            Some(factor) => {
                let (width, height) = big_digits::size(&self.clock, factor);
                let x = area.x + area.width.saturating_sub(width) / 2;
                let clock_area = Rect::new(x, y, width, height).intersection(area);
                BigText::new(&self.clock, factor, theme::ACCENT).render(clock_area, buf);
            }
            None => put_centered(buf, area, y, &Line::styled(self.clock.as_str(), accent)),
        }
        y = y.saturating_add(clock_height).saturating_add(1);

        if let Some(date) = self.date.as_ref() {
            let line = Line::from(vec![
                Span::styled(date.as_str(), Style::new().fg(theme::TEXT)),
                Span::raw("  "),
                Span::styled(self.zone_label.as_str(), Style::new().fg(theme::LABEL)),
            ]);
            put_centered(buf, area, y, &line);
        }
        y = y.saturating_add(1);

        let status_style = Style::new().fg(self.status_color);
        let status = Line::from(vec![
            Span::styled(STATUS_MARK, status_style),
            Span::raw(" "),
            Span::styled(self.status.as_str(), status_style),
        ]);
        put_centered(buf, area, y, &status);
        y = y.saturating_add(1);

        y = render_info_rows(&self.rows, area, y, buf);
        if show_trends {
            trend::render(&self.trends, area, y.saturating_add(1), buf);
            y = y.saturating_add(trend_rows);
        }

        if let Some(footer) = self.footer.as_ref() {
            let line = Line::styled(footer.as_str(), Style::new().fg(theme::FOOTER));
            put_centered(buf, area, y.saturating_add(1), &line);
        }
    }
}

/// 情報欄: ラベルと値を 2 列にそろえ、ブロック全体を左右中央に置く。次の行の y を返す。
fn render_info_rows(rows: &[InfoRow], area: Rect, mut y: u16, buf: &mut Buffer) -> u16 {
    let label_width = rows
        .iter()
        .map(|row| Span::raw(row.label).width())
        .max()
        .unwrap_or(0)
        + 2;
    let value_width = rows
        .iter()
        .map(|row| Span::raw(row.value.as_str()).width())
        .max()
        .unwrap_or(0);
    let block_width = u16::try_from(label_width + value_width).unwrap_or(u16::MAX);
    let x = area.x + area.width.saturating_sub(block_width) / 2;
    let value_x = x.saturating_add(u16::try_from(label_width).unwrap_or(u16::MAX));
    for row in rows {
        if y >= area.bottom() {
            break;
        }
        let max_width = area.right().saturating_sub(x);
        let _: (u16, u16) = buf.set_span(
            x,
            y,
            &Span::styled(row.label, Style::new().fg(theme::LABEL)),
            max_width,
        );
        if value_x < area.right() {
            let _: (u16, u16) = buf.set_span(
                value_x,
                y,
                &Span::styled(row.value.as_str(), Style::new().fg(row.color)),
                area.right() - value_x,
            );
        }
        y = y.saturating_add(1);
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DisplayZone, display_width};
    use alloc::sync::Arc;
    use application::acl::mib::MibProfile;
    use application::timestamping::TimestampingStatus;
    use domain::receiver::{ReceiverState, ReceiverStatus, SatelliteCounts};
    use domain::shared_kernel::LocalStamp;
    use domain::time_sync::{
        Anchor, ExchangeStats, LeapIndicator, ReferenceId, Sample, ServerInfo, Stratum,
    };

    /// 2026-11-15 17:25:37.923 JST
    fn server_time() -> NtpTimestamp {
        let unix_nanos =
            (20_772i128 * 86_400 + 8 * 3600 + 25 * 60 + 37) * 1_000_000_000 + 923_000_000;
        NtpTimestamp::from_unix_nanos(unix_nanos)
    }

    fn config(footer: Option<&str>) -> TuiConfig {
        TuiConfig {
            title: "NTP CLOCK".into(),
            server_label: "192.168.1.123".into(),
            footer: footer.map(str::to_owned),
            zone: DisplayZone::jst(),
            trend: false,
            receiver: false,
            transport: "UDP ソケット".into(),
            timestamping: Arc::new(TimestampingStatus::new()),
        }
    }

    /// `--trend` を付けたときの設定。
    fn with_trend(config: TuiConfig) -> TuiConfig {
        TuiConfig {
            trend: true,
            ..config
        }
    }

    /// 画像と同じ状況: offset -0.978ms、delay 0.423ms、687/687 成功、7 秒前に同期、9 秒後に次回。
    fn synced_snapshot() -> SyncSnapshot {
        // T2 = T3（サーバ内の処理時間 0）、往復 0.423ms。
        // offset = ((T3 − T1) + (T3 − T4)) / 2 = −0.978ms になるよう、T1 の壁時計を T3 + 0.7665ms にする
        let t3 = server_time();
        let t1_wall = t3.wrapping_add(TimeDelta::from_nanos(766_500));
        let sent = LocalStamp {
            mono: PcTime::monotonic(1_000_000_000),
            wall: t1_wall,
        };
        let received = LocalStamp {
            mono: PcTime::monotonic(1_000_423_000),
            wall: t1_wall.wrapping_add(TimeDelta::from_micros(423)),
        };
        let sample = Sample::new(sent, t3, t3, received).unwrap();
        let server_info = ServerInfo {
            leap: LeapIndicator::NoWarning,
            version: 4,
            stratum: Stratum::new(1),
            poll: 4,
            precision: -20,
            root_delay: TimeDelta::ZERO,
            root_dispersion: TimeDelta::ZERO,
            reference_id: ReferenceId::interpret(Stratum::new(1), *b"GNSS"),
            reference_time: t3,
        };
        SyncSnapshot {
            state: SyncState::Synchronized,
            stats: ExchangeStats {
                attempts: 687,
                successes: 687,
                ..ExchangeStats::default()
            },
            poll_interval: TimeDelta::from_secs(16),
            anchor: Some(Anchor::from_sample(&sample).unwrap()),
            last_sample: Some(sample),
            selected_sample: Some(sample),
            frequency: None,
            server_info: Some(server_info),
            last_success: Some(received.mono),
            next_poll: Some(PcTime::monotonic(17_000_000_000)),
            awaiting_reply: false,
        }
    }

    /// 直近 3 回の測定値（オフセット -0.978 / -0.950 / -1.020 ms、遅延 0.423 / 0.400 / 0.452 ms）。
    fn recent_history() -> SampleHistory {
        trend::testing::history(&[(-978, 423), (-950, 400), (-1_020, 452)])
    }

    /// 7 秒後（= 最終同期 7 秒前、次回取得 9 秒後）。
    fn seven_seconds_later() -> PcTime {
        PcTime::monotonic(8_000_423_000)
    }

    /// 描画結果を文字にする。背景色付きの空白は `█`（時計）と `#`（推移）、全角文字の後ろの詰め物は飛ばす。
    fn render_text(model: &ScreenModel, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        model.render(area, &mut buf);
        (0..height)
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0;
                for x in 0..width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let cell = &buf[(x, y)];
                    if cell.symbol() == " " && cell.bg == theme::ACCENT {
                        line.push('█');
                    } else if cell.symbol() == " " && cell.bg == theme::TREND {
                        line.push('#');
                    } else {
                        line.push_str(cell.symbol());
                        skip = display_width(cell.symbol()).saturating_sub(1);
                    }
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn builds_values_like_the_reference_image() {
        let snapshot = synced_snapshot();
        let shown = snapshot
            .anchor
            .unwrap()
            .project(seven_seconds_later())
            .unwrap();
        let model = build(
            &config(None),
            Some(&snapshot),
            None,
            &SampleHistory::default(),
            Some(shown),
            seven_seconds_later(),
        );
        assert_eq!(model.status, "同期済み");
        let value = |label: &str| {
            model
                .rows
                .iter()
                .find(|row| row.label == label)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value("オフセット"), "-0.978 ms");
        assert_eq!(value("遅延"), "0.423 ms");
        assert_eq!(value("Stratum"), "1");
        assert_eq!(value("Ref ID"), "GNSS");
        assert_eq!(value("NTP version"), "4");
        assert_eq!(value("最終同期"), "7.0 秒前");
        assert_eq!(value("次回取得"), "9.0 秒後");
        assert_eq!(value("成功/試行"), "687/687");
        assert_eq!(value("送信方式"), "UDP ソケット");
        assert_eq!(value("タイムスタンプ"), "ユーザ空間");
        assert_eq!(model.date.as_deref(), Some("2026-11-15 (日)"));
    }

    #[test]
    fn shows_placeholders_before_first_reply() {
        let model = build(
            &config(None),
            None,
            None,
            &SampleHistory::default(),
            None,
            PcTime::monotonic(0),
        );
        assert_eq!(model.clock, CLOCK_PLACEHOLDER);
        assert_eq!(model.date, None);
        assert!(
            model
                .rows
                .iter()
                .filter(|row| !matches!(row.label, "送信方式" | "タイムスタンプ"))
                .all(|row| row.value == NO_VALUE)
        );
        let lines = render_text(&model, 80, 24);
        assert!(lines.iter().any(|line| line.contains("応答待ち（未同期）")));
    }

    #[test]
    fn renders_full_screen() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(
            &with_trend(config(Some("Sponsored by Example Inc."))),
            Some(&snapshot),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        let lines = render_text(&model, 120, 40);
        // 目視確認用: cargo test -p ntpterminal-presentation renders_full_screen -- --nocapture
        println!("{}", lines.join("\n"));

        let text = lines.join("\n");
        for expected in [
            "N T P   C L O C K",
            "192.168.1.123",
            "2026-11-15 (日)  JST",
            "● 同期済み",
            "オフセット",
            "-0.978 ms",
            "687/687",
            "直近 3 回の測定値",
            "-0.950 ms",
            "-1.020 ms",
            "0.452 ms",
            "0.400 ms",
            "Sponsored by Example Inc.",
        ] {
            assert!(text.contains(expected), "{expected} がありません");
        }
        // 120 桁では倍率 2（100 桁 × 5 行）で描かれる。推移を出しても時計は縮まない
        assert_eq!(lines.iter().filter(|line| line.contains('█')).count(), 5);
        // 推移の棒は 2 系列 × 2 行で、右端に新しい値が来る（段: オフセット 3,4,1 / 遅延 2,1,4）
        let bar_rows: Vec<&String> = lines.iter().filter(|line| line.contains('#')).collect();
        assert_eq!(bar_rows.len(), 4);
        for (row, expected) in bar_rows.iter().zip([
            " ▄#  -0.950 ms",
            " ##▄ -1.020 ms",
            "   # 0.452 ms",
            " #▄# 0.400 ms",
        ]) {
            assert!(
                row.ends_with(expected),
                "{row:?} が {expected:?} で終わっていない"
            );
        }
        // 最下部の一文は推移より下に出る
        let row_of = |needle: &str| lines.iter().position(|line| line.contains(needle)).unwrap();
        assert!(row_of("直近 3 回の測定値") < row_of("Sponsored by Example Inc."));
    }

    #[test]
    fn trends_are_hidden_by_default() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let config = config(Some("footer"));
        let model = build(
            &config,
            Some(&snapshot),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        assert_eq!(model.trends, []);
        let text = render_text(&model, 300, 60).join("\n");
        assert!(!text.contains("測定値"));
    }

    #[test]
    fn trends_never_shrink_the_clock() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let config = with_trend(config(Some("footer")));
        let with = build(
            &config,
            Some(&snapshot),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        let without = build(
            &config,
            Some(&snapshot),
            None,
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        // 300×30 は高さで倍率が決まる（推移を出すと倍率 4 → 2）。80×60 は推移が横に入らない。
        // 80×24 は時計が倍率 1 でぎりぎり。どれも推移を出さず、出さないときと同じ画面になる
        for (width, height) in [(300, 30), (80, 60), (80, 24)] {
            let lines = render_text(&with, width, height);
            assert_eq!(
                lines,
                render_text(&without, width, height),
                "{width}×{height}"
            );
            assert!(!lines.join("\n").contains("測定値"), "{width}×{height}");
        }
        // 余裕があれば出す
        let text = render_text(&with, 300, 60).join("\n");
        assert!(text.contains("直近 3 回の測定値"));
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(
            &with_trend(config(Some("footer"))),
            Some(&snapshot),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        for (width, height) in [
            (0, 0),
            (1, 1),
            (20, 5),
            (40, 10),
            (49, 30),
            (80, 24),
            (300, 100),
        ] {
            drop(render_text(&model, width, height));
        }
    }

    #[test]
    fn falls_back_to_plain_text_when_too_narrow() {
        let model = build(
            &config(None),
            None,
            None,
            &SampleHistory::default(),
            None,
            PcTime::monotonic(0),
        );
        let lines = render_text(&model, 40, 24);
        assert!(lines.iter().any(|line| line.contains(CLOCK_PLACEHOLDER)));
    }

    /// 受信機の行（2 行）は Ref ID の後ろに入り、80×24 で最下部の一文があっても大きい時計を縮めない。
    #[test]
    fn receiver_rows_fit_with_the_big_clock_on_80x24() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let receiver = ReceiverSnapshot {
            profile: Some(MibProfile::MeinbergLtng),
            status: Some(ReceiverStatus {
                name: Some("gps180".to_owned()),
                state: Some(ReceiverState::Synchronized),
                state_detail: Some("gpsSync".to_owned()),
                satellites: SatelliteCounts {
                    good: Some(5),
                    in_view: Some(9),
                    ..SatelliteCounts::default()
                },
                ..ReceiverStatus::default()
            }),
            last_success: Some(now),
            ..ReceiverSnapshot::default()
        };
        let with_receiver = TuiConfig {
            receiver: true,
            ..config(Some("footer"))
        };
        let model = build(
            &with_receiver,
            Some(&snapshot),
            Some(&receiver),
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        let labels: Vec<&str> = model.rows.iter().map(|row| row.label).collect();
        let ref_id = labels.iter().position(|&label| label == "Ref ID").unwrap();
        assert_eq!(labels[ref_id + 1..ref_id + 3], ["GNSS 受信機", "衛星"]);

        let lines = render_text(&model, 80, 24);
        // 目視確認用: cargo test -p ntpterminal-presentation receiver_rows_fit -- --nocapture
        println!("{}", lines.join("\n"));
        // 大きい時計（倍率 1）が描かれ、普通の文字の時計にはなっていない
        assert!(lines.iter().any(|line| line.contains('█')));
        assert!(!lines.iter().any(|line| line.contains(&model.clock)));
        let text = lines.join("\n");
        assert!(text.contains("Meinberg LTNG（gps180）: 同期（gpsSync）"));
        assert!(text.contains("良好 5 / 可視 9"));
        assert!(text.contains("footer"));

        // 既定（--snmp-community なし）では出さない
        let without = build(
            &config(None),
            Some(&snapshot),
            Some(&receiver),
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        assert!(!without.rows.iter().any(|row| row.label == "GNSS 受信機"));
    }

    #[test]
    fn letter_spacing_keeps_word_gap() {
        assert_eq!(letter_spaced("NTP CLOCK"), "N T P   C L O C K");
    }
}
