//! 画面の内容（[`ScreenModel`]）と描画。
//!
//! 内容の組み立て（[`build`]）と描画（`Widget for &ScreenModel`）を分け、どちらも
//! 端末なしでテストできるようにしている。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use application::snapshot::SyncSnapshot;
use domain::shared_kernel::{NtpTimestamp, PcTime, TimeDelta};
use domain::time_sync::SyncState;

use super::TuiConfig;
use super::big_digits::{self, BigText};
use super::theme;
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

/// 情報欄の 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoRow {
    pub label: &'static str,
    pub value: String,
    pub color: Color,
}

/// 画面に出す内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenModel {
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
    pub footer: Option<String>,
}

/// スナップショットと現在時刻から、画面に出す内容を組み立てる。
///
/// - `shown`: 大きい時計に出す時刻（[`domain::time_sync::DisplayClock`] の結果）
/// - `now`: モノトニック時計の現在値（「何秒前」「何秒後」の計算に使う）
pub fn build(
    config: &TuiConfig,
    snapshot: Option<&SyncSnapshot>,
    shown: Option<NtpTimestamp>,
    now: PcTime,
) -> ScreenModel {
    let state = snapshot.map_or(SyncState::Unsynchronized, |s| s.state);
    let (status, status_color) = match state {
        SyncState::Synchronized => (state_label(state).to_string(), theme::GOOD),
        SyncState::Unsynchronized => ("応答待ち（未同期）".to_string(), theme::LABEL),
        SyncState::Holdover => (state_label(state).to_string(), theme::WARN),
        SyncState::Denied => (state_label(state).to_string(), theme::BAD),
    };
    ScreenModel {
        title: config.title.clone(),
        server: config.server_label.clone(),
        clock: shown.map_or_else(
            || CLOCK_PLACEHOLDER.to_string(),
            |time| format_clock(time, &config.zone),
        ),
        date: shown.map(|time| format_date(time, &config.zone)),
        zone_label: config.zone.label().to_string(),
        status,
        status_color,
        rows: info_rows(config, snapshot, now),
        footer: config.footer.clone(),
    }
}

fn info_rows(config: &TuiConfig, snapshot: Option<&SyncSnapshot>, now: PcTime) -> Vec<InfoRow> {
    // offset と delay は、基準点と同じ「クロックフィルタが選んだ標本」の値を出す（生の値は揺れる）
    let selected = snapshot.and_then(|s| s.selected_sample);
    let last = snapshot.and_then(|s| s.last_sample);
    let info = snapshot.and_then(|s| s.server_info);

    let row = |label, value: Option<String>, color| InfoRow {
        label,
        value: value.unwrap_or_else(|| NO_VALUE.to_string()),
        color,
    };
    let offset = selected.map(|s| s.offset().delta());
    let delay = selected.map(|s| s.delay().delta());
    let next_poll = match snapshot {
        Some(s) if s.state == SyncState::Denied => Some("停止中".to_string()),
        Some(s) => s
            .next_poll
            .and_then(|at| at.since(now).ok())
            .map(|remaining| format!("{:.1} 秒後", remaining.max(TimeDelta::ZERO).as_secs_f64())),
        None => None,
    };

    vec![
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
        row(
            "NTP version",
            info.map(|i| i.version.to_string()),
            theme::TEXT,
        ),
        row(
            "サーバ時刻",
            last.map(|s| format_clock(s.server_transmit(), &config.zone)),
            theme::TEXT,
        ),
        row(
            "最終同期",
            snapshot
                .and_then(|s| s.last_success)
                .and_then(|at| now.since(at).ok())
                .map(|elapsed| format!("{:.1} 秒前", elapsed.max(TimeDelta::ZERO).as_secs_f64())),
            theme::TEXT,
        ),
        row("次回取得", next_poll, theme::TEXT),
        row(
            "成功/試行",
            snapshot.map(|s| format!("{}/{}", s.stats.successes, s.stats.attempts)),
            theme::TEXT,
        ),
        row("送信方式", Some(config.transport.clone()), theme::LABEL),
        {
            let counts = config.timestamping.counts();
            let color = if counts.last_was_kernel {
                theme::GOOD
            } else {
                theme::LABEL
            };
            row(
                "タイムスタンプ",
                Some(timestamping_label(&counts).to_string()),
                color,
            )
        },
    ]
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
    buf.set_line(x, y, line, area.right().saturating_sub(x));
}

impl Widget for &ScreenModel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // 縦の並び: 見出し / サーバ / 空行 / 時計 / 空行 / 日付 / 状態 / 情報欄 / (空行 / 最下部の一文)
        let rows = u16::try_from(self.rows.len()).unwrap_or(u16::MAX);
        let footer_rows = if self.footer.is_some() { 2 } else { 0 };
        let fixed = 6_u16.saturating_add(rows).saturating_add(footer_rows);

        // 時計は残りの高さと幅に収まる最大の倍率で描く。倍率 1 でも入らなければ普通の文字で出す
        let scale = big_digits::fit_scale(
            &self.clock,
            area.width.saturating_sub(2),
            area.height.saturating_sub(fixed),
        );
        let clock_height = scale.map_or(1, |s| big_digits::size(&self.clock, s).1);
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
            Some(scale) => {
                let (width, height) = big_digits::size(&self.clock, scale);
                let x = area.x + area.width.saturating_sub(width) / 2;
                let clock_area = Rect::new(x, y, width, height).intersection(area);
                BigText::new(&self.clock, scale, theme::ACCENT).render(clock_area, buf);
            }
            None => put_centered(buf, area, y, &Line::styled(self.clock.as_str(), accent)),
        }
        y = y.saturating_add(clock_height).saturating_add(1);

        if let Some(date) = &self.date {
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

        if let Some(footer) = &self.footer {
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
        buf.set_span(
            x,
            y,
            &Span::styled(row.label, Style::new().fg(theme::LABEL)),
            max_width,
        );
        if value_x < area.right() {
            buf.set_span(
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
    use domain::shared_kernel::{LocalStamp, NtpTimestamp};
    use domain::time_sync::{
        Anchor, ExchangeStats, LeapIndicator, ReferenceId, Sample, ServerInfo, Stratum,
    };

    /// 2026-11-15 17:25:37.923 JST
    fn server_time() -> NtpTimestamp {
        let unix_nanos =
            (20_772_i128 * 86_400 + 8 * 3600 + 25 * 60 + 37) * 1_000_000_000 + 923_000_000;
        NtpTimestamp::from_unix_nanos(unix_nanos)
    }

    fn config(footer: Option<&str>) -> TuiConfig {
        TuiConfig {
            title: "NTP CLOCK".into(),
            server_label: "192.168.1.123".into(),
            footer: footer.map(str::to_string),
            zone: DisplayZone::jst(),
            transport: "UDP ソケット".into(),
            timestamping: std::sync::Arc::new(application::timestamping::TimestampingStatus::new()),
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
            server_info: Some(server_info),
            last_success: Some(received.mono),
            next_poll: Some(PcTime::monotonic(17_000_000_000)),
            awaiting_reply: false,
        }
    }

    /// 7 秒後（= 最終同期 7 秒前、次回取得 9 秒後）。
    fn seven_seconds_later() -> PcTime {
        PcTime::monotonic(8_000_423_000)
    }

    /// 描画結果を文字にする。背景色付きの空白は `█`、全角文字の後ろの詰め物は飛ばす。
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
                    } else {
                        line.push_str(cell.symbol());
                        skip = display_width(cell.symbol()).saturating_sub(1);
                    }
                }
                line.trim_end().to_string()
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
        let model = build(&config(None), None, None, PcTime::monotonic(0));
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
            &config(Some("Sponsored by Example Inc.")),
            Some(&snapshot),
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
            "Sponsored by Example Inc.",
        ] {
            assert!(text.contains(expected), "{expected} がありません");
        }
        // 120 桁では倍率 2（100 桁 × 5 行）で描かれる
        assert_eq!(lines.iter().filter(|line| line.contains('█')).count(), 5);
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(&config(Some("footer")), Some(&snapshot), Some(shown), now);
        for (width, height) in [
            (0, 0),
            (1, 1),
            (20, 5),
            (40, 10),
            (49, 30),
            (80, 24),
            (300, 100),
        ] {
            render_text(&model, width, height);
        }
    }

    #[test]
    fn falls_back_to_plain_text_when_too_narrow() {
        let model = build(&config(None), None, None, PcTime::monotonic(0));
        let lines = render_text(&model, 40, 24);
        assert!(lines.iter().any(|line| line.contains(CLOCK_PLACEHOLDER)));
    }

    #[test]
    fn letter_spacing_keeps_word_gap() {
        assert_eq!(letter_spaced("NTP CLOCK"), "N T P   C L O C K");
    }
}
