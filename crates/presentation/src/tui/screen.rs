//! 画面の内容（[`ScreenModel`]）と描画。
//!
//! 内容の組み立て（[`build`]）と描画（`Widget for &ScreenModel`）を分け、どちらも
//! 端末なしでテストできるようにしている。文言は英語（[`text`] の先頭）。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use application::description::NtpTransportInfo;
use application::snapshot::{ReceiverSnapshot, SyncSnapshot};
use application::timestamping::{TimestampSource, TimestampingStatus};
use domain::shared_kernel::{NtpTimestamp, PcTime, TimeDelta};
use domain::time_sync::SyncState;

use super::big_digits::{self, BigText};
use super::trend::{self, SampleHistory, Trend};
use super::{
    QrCorner, SourceSnapshot, TuiConfig, TuiSource, master, phc, qr, receiver, text, theme,
    user_text,
};
use crate::format::{DisplayZone, format_clock, format_delay_ms, format_offset_ms};

/// 同期前の時計の表示。
const CLOCK_PLACEHOLDER: &str = "--:--:--.---";
/// 値がまだないときの表示。
pub(super) const NO_VALUE: &str = "--";
/// 状態の前に付ける印。
/// 注意: `●` は East Asian Ambiguous。端末の「曖昧幅」が半角（既定）であることを前提にしている。
const STATUS_MARK: &str = "●";
/// 成功した回数と試みた回数（"687/687"）の行のラベル。時刻源によらず同じ。
pub(super) const SUCCESS_LABEL: &str = "Success/Tries";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InfoRow {
    pub label: &'static str,
    pub value: String,
    pub color: Color,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenModel {
    /// 見出しの行（字間を空け、タブを空白にしたもの。1 行以上）
    pub title: Vec<String>,
    pub server: String,
    /// 大きい時計の文字列（"17:25:45.146"）
    pub clock: String,
    /// "2026-11-15 (Sun)"。同期前は `None`
    pub date: Option<String>,
    pub zone_label: String,
    pub status: String,
    pub status_color: Color,
    pub rows: Vec<InfoRow>,
    /// 測定値の推移。まだ測定値がなければ空
    pub trends: Vec<Trend>,
    /// 最下部の一文の行（タブを空白にしたもの）。空なら出さない
    pub footer: Vec<String>,
    /// リポジトリの QR コードを置く隅。`None` なら出さない
    pub qr: Option<QrCorner>,
}

/// スナップショットと現在時刻から、画面に出す内容を組み立てる。
///
/// - `receiver`: 受信機の監視の最新のスナップショット（`config.receiver` のときだけ使う。まだ届いていなければ `None`）
/// - `history`: 推移のグラフに使う、これまでの測定値（`config.trend` のときだけ使う）
/// - `shown`: 大きい時計に出す時刻（[`domain::time_sync::DisplayClock`] の結果）
/// - `now`: モノトニック時計の現在値（「何秒前」「何秒後」の計算に使う）
#[expect(
    clippy::pattern_type_mismatch,
    reason = "設定の文字列などを借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
)]
pub(super) fn build(
    config: &TuiConfig,
    snapshot: Option<&SourceSnapshot>,
    receiver: Option<&ReceiverSnapshot>,
    history: &SampleHistory,
    shown: Option<NtpTimestamp>,
    now: PcTime,
) -> ScreenModel {
    let ((status, status_color), rows) = match &config.source {
        TuiSource::Ntp {
            receiver: with_receiver,
            transport,
            timestamping,
        } => {
            let ntp = snapshot.and_then(SourceSnapshot::ntp);
            (
                ntp_status(ntp),
                info_rows(
                    ntp,
                    *with_receiver,
                    receiver,
                    transport,
                    timestamping,
                    &config.zone,
                    now,
                ),
            )
        }
        TuiSource::Phc { phc: info, ports } => {
            let snap = snapshot.and_then(SourceSnapshot::phc);
            (phc::status(snap), phc::rows(snap, info, ports, now))
        }
        TuiSource::Ptp {
            transport,
            timestamping,
        } => {
            let snap = snapshot.and_then(SourceSnapshot::ptp);
            (
                master::status(snap),
                master::rows(snap, transport, timestamping, now),
            )
        }
    };
    ScreenModel {
        // 字間を空けてからタブを埋める（逆にすると、タブを埋めた空白の間にも字間が入る）
        title: user_text::lines(&config.title)
            .into_iter()
            .map(|line| user_text::expand_tabs(&letter_spaced(line)))
            .collect(),
        server: config.server_label.clone(),
        clock: shown.map_or_else(
            || CLOCK_PLACEHOLDER.to_owned(),
            |time| format_clock(time, &config.zone),
        ),
        date: shown.map(|time| text::date(time, &config.zone)),
        zone_label: config.zone.label().to_owned(),
        status,
        status_color,
        rows,
        trends: if config.trend {
            trend::trends(history)
        } else {
            Vec::new()
        },
        footer: config.footer.as_deref().map_or_else(Vec::new, |footer| {
            user_text::lines(footer)
                .into_iter()
                .map(user_text::expand_tabs)
                .collect()
        }),
        qr: config.qr,
    }
}

/// NTP の状態の行の文言と色。
fn ntp_status(snapshot: Option<&SyncSnapshot>) -> (String, Color) {
    let state = snapshot.map_or(SyncState::Unsynchronized, |snap| snap.state);
    let color = match state {
        SyncState::Synchronized => theme::GOOD,
        SyncState::Unsynchronized => theme::LABEL,
        SyncState::Holdover => theme::WARN,
        SyncState::Denied => theme::BAD,
    };
    (text::ntp_state(state).to_owned(), color)
}

/// NTP の情報欄。受信機の行（`show_receiver` のとき）は、基準時計に関わる「Ref ID」の後ろに置く。
fn info_rows(
    snapshot: Option<&SyncSnapshot>,
    show_receiver: bool,
    receiver: Option<&ReceiverSnapshot>,
    transport: &NtpTransportInfo,
    timestamping: &TimestampingStatus,
    zone: &DisplayZone,
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
        Some(snap) if snap.state == SyncState::Denied => Some("Stopped".to_owned()),
        Some(snap) => snap
            .next_poll
            .and_then(|at| at.since(now).ok())
            .map(text::after),
        None => None,
    };

    let mut rows = vec![
        row(
            "Offset",
            offset.map(format_offset_ms),
            offset.map_or(theme::TEXT, tone),
        ),
        row(
            "Delay",
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
    if show_receiver {
        rows.extend(receiver::rows(receiver, now));
    }
    rows.extend([
        row(
            "NTP version",
            info.map(|i| i.version.to_string()),
            theme::TEXT,
        ),
        row(
            "Server time",
            last.map(|sample| format_clock(sample.server_transmit(), zone)),
            theme::TEXT,
        ),
        row(
            "Last sync",
            snapshot
                .and_then(|snap| snap.last_success)
                .and_then(|at| now.since(at).ok())
                .map(text::ago),
            theme::TEXT,
        ),
        row("Next poll", next_poll, theme::TEXT),
        row(
            SUCCESS_LABEL,
            snapshot.map(|snap| format!("{}/{}", snap.stats.successes, snap.stats.attempts)),
            theme::TEXT,
        ),
        row(
            "Transport",
            Some(text::ntp_transport(transport)),
            theme::LABEL,
        ),
        {
            let counts = timestamping.counts();
            // カーネルかハードウェアの時刻が、設定どおりに取れているときだけ「良好」の色にする
            let color = if counts.last_source != TimestampSource::User
                && counts.last_source == counts.expected_source()
            {
                theme::GOOD
            } else {
                theme::LABEL
            };
            row(
                "Timestamps",
                Some(text::timestamping(&counts).to_owned()),
                color,
            )
        },
    ]);
    rows
}

/// ずれの大きさで色を変える（1ms 未満: 良好、10ms 未満: 注意、それ以上: 異常）。
pub(super) fn tone(delta: TimeDelta) -> Color {
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

/// 行の数（`u16` に収まらなければ飽和させる）。
fn count(len: usize) -> u16 {
    u16::try_from(len).unwrap_or(u16::MAX)
}

/// 行を `y` から下へ順に、それぞれ左右中央に描く。
fn put_lines(buf: &mut Buffer, area: Rect, mut y: u16, lines: &[String], style: Style) {
    for line in lines {
        put_centered(buf, area, y, &Line::styled(line.as_str(), style));
        y = y.saturating_add(1);
    }
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
        // 縦の並び: 見出し（行の数） / サーバ / 空行 / 時計 / 空行 / 日付 / 状態 / 情報欄 /
        //           (空行 / 推移) / (空行 / 最下部の一文（行の数）)
        // QR コードは並びに入れず、空いている隅にだけ重ねる
        let footer_rows = if self.footer.is_empty() {
            0
        } else {
            count(self.footer.len()).saturating_add(1)
        };
        let base = 5u16
            .saturating_add(count(self.title.len()))
            .saturating_add(count(self.rows.len()))
            .saturating_add(footer_rows);

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
        put_lines(buf, area, y, &self.title, accent);
        y = y.saturating_add(count(self.title.len()));
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

        put_lines(
            buf,
            area,
            y.saturating_add(1),
            &self.footer,
            Style::new().fg(theme::FOOTER),
        );

        // QR コードは最後に描き、隅がほかの表示と重ならないときだけ出す（`qr` の先頭）
        if let Some(corner) = self.qr
            && let Some(code) = qr::repository_qr()
        {
            qr::render(code, corner, area, buf);
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
    use crate::cli::master::MasterTarget;
    use crate::cli::phc::PhcTarget;
    use crate::format::display_width;
    use crate::tui::{master_server_label, phc_server_label};
    use alloc::sync::Arc;
    use application::acl::mib::{MibError, MibProfile};
    use application::description::{
        HardwareTimestampInfo, NtpTransportKind, PhcInfo, PhcReadMethod, PtpTransportInfo,
    };
    use application::port::TransportError;
    use application::snapshot::{MasterSnapshot, PhcSnapshot};
    use application::usecase::MonitorError;
    use core::net::{Ipv4Addr, SocketAddr};
    use domain::packet_crafting::MacAddr;
    use domain::ptp_clock::{
        ClockIdentity, ClockQuality, ExchangeFailure, LeapWarning, MasterState, MasterStats,
        MasterStatus, PhcState, PhcStats, PortIdentity, PortState, PortStatus, PtpStatus,
        TimeProperties, TimeSourceKind, Timescale,
    };
    use domain::receiver::{
        AntennaState, Constellation, ConstellationCount, ReceiverState, ReceiverStatus,
        SatelliteCounts,
    };
    use domain::shared_kernel::{LocalStamp, Timebase, TimebaseMismatch};
    use domain::time_sync::{
        Anchor, ExchangeStats, LeapIndicator, ReferenceId, Sample, SampleError, ServerInfo, Stratum,
    };
    use ratatui::buffer::Cell;
    use ratatui::layout::Position;

    /// 2026-11-15 17:25:37.923 JST
    fn server_time() -> NtpTimestamp {
        let unix_nanos =
            (20_772i128 * 86_400 + 8 * 3600 + 25 * 60 + 37) * 1_000_000_000 + 923_000_000;
        NtpTimestamp::from_unix_nanos(unix_nanos)
    }

    fn udp() -> NtpTransportInfo {
        NtpTransportInfo {
            kind: NtpTransportKind::Udp,
            hardware: None,
        }
    }

    fn config(footer: Option<&str>) -> TuiConfig {
        TuiConfig {
            title: "NTP CLOCK".into(),
            server_label: "192.168.1.123".into(),
            footer: footer.map(str::to_owned),
            zone: DisplayZone::jst(),
            trend: false,
            qr: None,
            source: TuiSource::Ntp {
                receiver: false,
                transport: udp(),
                timestamping: Arc::new(TimestampingStatus::new()),
            },
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
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            None,
            &SampleHistory::default(),
            Some(shown),
            seven_seconds_later(),
        );
        assert_eq!(model.status, "Synchronized");
        let value = |label: &str| {
            model
                .rows
                .iter()
                .find(|row| row.label == label)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(value("Offset"), "-0.978 ms");
        assert_eq!(value("Delay"), "0.423 ms");
        assert_eq!(value("Stratum"), "1");
        assert_eq!(value("Ref ID"), "GNSS");
        assert_eq!(value("NTP version"), "4");
        assert_eq!(value("Last sync"), "7.0 s ago");
        assert_eq!(value("Next poll"), "in 9.0 s");
        assert_eq!(value(SUCCESS_LABEL), "687/687");
        assert_eq!(value("Transport"), "UDP socket");
        assert_eq!(value("Timestamps"), "User space");
        assert_eq!(model.date.as_deref(), Some("2026-11-15 (Sun)"));
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
                .filter(|row| !matches!(row.label, "Transport" | "Timestamps"))
                .all(|row| row.value == NO_VALUE)
        );
        let lines = render_text(&model, 80, 24);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Waiting for reply (unsynchronized)"))
        );
    }

    #[test]
    fn renders_full_screen() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(
            &with_trend(config(Some("Sponsored by Example Inc."))),
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
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
            "2026-11-15 (Sun)  JST",
            "● Synchronized",
            "Offset",
            "-0.978 ms",
            "687/687",
            "Last 3 samples",
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
        // 推移の棒は 2 系列 × 2 行で、右端に新しい値が来る（段: Offset 3,4,1 / Delay 2,1,4）
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
        assert!(row_of("Last 3 samples") < row_of("Sponsored by Example Inc."));
    }

    const ALL_CORNERS: [QrCorner; 4] = [
        QrCorner::TopLeft,
        QrCorner::TopRight,
        QrCorner::BottomLeft,
        QrCorner::BottomRight,
    ];

    /// QR コードを `corner` に置く設定。
    fn with_qr(config: TuiConfig, corner: QrCorner) -> TuiConfig {
        TuiConfig {
            qr: Some(corner),
            ..config
        }
    }

    /// 同期済みの画面を `width`×`height` に描く。
    fn draw_synced(config: &TuiConfig, width: u16, height: u16) -> Buffer {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(
            config,
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        model.render(area, &mut buf);
        buf
    }

    /// 同期済みの画面を、QR コードを出す設定と出さない設定で描いて比べ、QR コードを描いた範囲を返す
    /// （描かなかったら `None`）。変わったセルが 1 つの長方形を埋めること、その長方形が QR コードを出さない画面では
    /// 空いていたこと（ほかの表示を消さず、時計の大きさも変えない）も確かめる。
    fn qr_area(config: &TuiConfig, corner: QrCorner, width: u16, height: u16) -> Option<Rect> {
        let plain = draw_synced(config, width, height);
        let with_code = draw_synced(&with_qr(config.clone(), corner), width, height);
        let changed: Vec<Position> = plain
            .area
            .positions()
            .filter(|&position| with_code[position] != plain[position])
            .collect();
        let left = changed.iter().map(|position| position.x).min()?;
        let top = changed.iter().map(|position| position.y).min()?;
        let right = changed.iter().map(|position| position.x).max()?;
        let bottom = changed.iter().map(|position| position.y).max()?;
        let rect = Rect::new(left, top, right - left + 1, bottom - top + 1);
        assert_eq!(rect.positions().count(), changed.len(), "{rect:?}");
        assert!(
            rect.positions()
                .all(|position| plain[position] == Cell::EMPTY),
            "{rect:?}"
        );
        Some(rect)
    }

    /// QR コードは空いている隅にぴったり置く（33 桁 × 17 行）。
    #[test]
    fn qr_code_goes_to_a_free_corner() {
        let footer = config(Some("Sponsored by Example Inc."));
        // 120×40 で推移を出さなければ、下の隅が空いている
        assert_eq!(
            qr_area(&footer, QrCorner::BottomRight, 120, 40),
            Some(Rect::new(87, 23, 33, 17))
        );
        assert_eq!(
            qr_area(&footer, QrCorner::BottomLeft, 120, 40),
            Some(Rect::new(0, 23, 33, 17))
        );
        // 縦に余裕のある端末なら、推移を出していても 4 隅のどこにでも置ける
        for (corner, x, y) in [
            (QrCorner::TopLeft, 0, 0),
            (QrCorner::TopRight, 167, 0),
            (QrCorner::BottomLeft, 0, 63),
            (QrCorner::BottomRight, 167, 63),
        ] {
            assert_eq!(
                qr_area(&with_trend(footer.clone()), corner, 200, 80),
                Some(Rect::new(x, y, 33, 17)),
                "{corner:?}"
            );
        }
    }

    /// 隅がほかの表示と重なるなら出さない（時計を縮めてまで出さない）。
    #[test]
    fn qr_code_is_hidden_when_the_corner_is_taken() {
        let footer = config(Some("Sponsored by Example Inc."));
        for corner in ALL_CORNERS {
            // 80×24 では時計と情報欄で埋まる
            assert_eq!(qr_area(&footer, corner, 80, 24), None, "{corner:?}");
            // 120×40 で推移を出すと、上の隅は時計と、下の隅は推移と重なる
            assert_eq!(
                qr_area(&with_trend(footer.clone()), corner, 120, 40),
                None,
                "{corner:?}"
            );
        }
    }

    /// 見出しと最下部の一文は、改行で複数行になり（末尾の改行は数えない）、行ごとに左右中央に置く。
    /// タブは空白で埋める（見出しは字間を空けてから）。
    #[test]
    fn multi_line_title_and_footer_are_centered_line_by_line() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let config = with_trend(TuiConfig {
            title: "NTP CLOCK\nBooth\tA-12".into(),
            ..config(Some("Sponsored by\nExample Inc.\tand friends\n"))
        });
        let model = build(
            &config,
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        // "B o o t h " の 10 桁の後のタブは 16 桁目まで埋め、その後に字間の空白が付く
        let booth = format!("B o o t h{}A - 1 2", " ".repeat(8));
        assert_eq!(model.title, ["N T P   C L O C K", booth.as_str()]);
        assert_eq!(
            model.footer,
            ["Sponsored by", "Example Inc.    and friends"]
        );

        let lines = render_text(&model, 120, 40);
        // 目視確認用: cargo test -p ntpterminal-presentation multi_line_title -- --nocapture
        println!("{}", lines.join("\n"));
        let row_of = |needle: &str| lines.iter().position(|line| line.contains(needle)).unwrap();
        let order = [
            "N T P   C L O C K",
            booth.as_str(),
            "192.168.1.123",
            "Last 3 samples",
            "Sponsored by",
            "Example Inc.    and friends",
        ];
        for (above, below) in order.iter().zip(order.iter().skip(1)) {
            assert!(row_of(above) < row_of(below), "{above} / {below}");
        }
        // 行ごとに左右中央（120 桁）
        for text in ["Sponsored by", "Example Inc.    and friends"] {
            let left = (120 - text.len()) / 2;
            assert_eq!(lines[row_of(text)], format!("{}{text}", " ".repeat(left)));
        }
        // 行が増えても、余裕があれば時計（倍率 2 の 5 行）は縮まない
        assert_eq!(lines.iter().filter(|line| line.contains('█')).count(), 5);
    }

    /// 行が増えて大きい時計が入らなくなったら、画面の外にはみ出さず、普通の文字の時計にする。
    #[test]
    fn extra_lines_shrink_the_clock_instead_of_overflowing() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        // 80×24 では、1 行の見出しと最下部の一文なら倍率 1（3 行）の時計を描いて 2 行余る。3 行増やすと入らない
        for (title, footer) in [
            ("NTP CLOCK", "footer 1\nfooter 2\nfooter 3\nfooter 4"),
            ("NTP CLOCK\nBooth A-12", "footer 1\nfooter 2\nfooter 3"),
        ] {
            let config = TuiConfig {
                title: title.into(),
                ..config(Some(footer))
            };
            let model = build(
                &config,
                Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
                None,
                &SampleHistory::default(),
                Some(shown),
                now,
            );
            let lines = render_text(&model, 80, 24);
            let text = lines.join("\n");
            assert!(!text.contains(['█', '▀', '▄']), "{text}");
            assert!(text.contains(&model.clock), "{text}");
            for line in title
                .lines()
                .map(letter_spaced)
                .chain(footer.lines().map(str::to_owned))
            {
                assert!(text.contains(&line), "{line} がありません\n{text}");
            }
        }
    }

    #[test]
    fn trends_are_hidden_by_default() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let config = config(Some("footer"));
        let model = build(
            &config,
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        assert_eq!(model.trends, []);
        let text = render_text(&model, 300, 60).join("\n");
        assert!(!text.contains("samples"));
    }

    #[test]
    fn trends_never_shrink_the_clock() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let config = with_trend(config(Some("footer")));
        let with = build(
            &config,
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            None,
            &recent_history(),
            Some(shown),
            now,
        );
        let without = build(
            &config,
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            None,
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        // 300×30 は高さで倍率が決まる（推移を出すと倍率 4 → 2）。78×60 は推移（79 桁）が横に入らない。
        // 80×24 は時計が倍率 1 でぎりぎり。どれも推移を出さず、出さないときと同じ画面になる
        for (width, height) in [(300, 30), (78, 60), (80, 24)] {
            let lines = render_text(&with, width, height);
            assert_eq!(
                lines,
                render_text(&without, width, height),
                "{width}×{height}"
            );
            assert!(!lines.join("\n").contains("samples"), "{width}×{height}");
        }
        // 余裕があれば出す（80 桁でも、高さがあれば入る）
        for (width, height) in [(300, 60), (80, 60)] {
            let text = render_text(&with, width, height).join("\n");
            assert!(text.contains("Last 3 samples"), "{width}×{height}");
        }
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let snapshot = synced_snapshot();
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let multi_line = TuiConfig {
            title: "NTP CLOCK\n\tBooth\t協賛".into(),
            ..config(Some("A\n\nB\tC\n".repeat(20).as_str()))
        };
        for config in [with_trend(config(Some("footer"))), with_trend(multi_line)] {
            let model = build(
                &config,
                Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
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
            source: TuiSource::Ntp {
                receiver: true,
                transport: udp(),
                timestamping: Arc::new(TimestampingStatus::new()),
            },
            ..config(Some("footer"))
        };
        let model = build(
            &with_receiver,
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            Some(&receiver),
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        let labels: Vec<&str> = model.rows.iter().map(|row| row.label).collect();
        let ref_id = labels.iter().position(|&label| label == "Ref ID").unwrap();
        assert_eq!(
            labels[ref_id + 1..ref_id + 3],
            ["GNSS receiver", "Satellites"]
        );

        let lines = render_text(&model, 80, 24);
        // 目視確認用: cargo test -p ntpterminal-presentation receiver_rows_fit -- --nocapture
        println!("{}", lines.join("\n"));
        // 大きい時計（倍率 1）が描かれ、普通の文字の時計にはなっていない
        assert!(lines.iter().any(|line| line.contains('█')));
        assert!(!lines.iter().any(|line| line.contains(&model.clock)));
        let text = lines.join("\n");
        assert!(text.contains("Meinberg LTNG (gps180): Synchronized (gpsSync)"));
        assert!(text.contains("good 5 / in view 9"));
        assert!(text.contains("footer"));

        // 既定（--snmp-community なし）では出さない
        let without = build(
            &config(None),
            Some(&SourceSnapshot::Ntp(Box::new(snapshot))),
            Some(&receiver),
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        assert!(!without.rows.iter().any(|row| row.label == "GNSS receiver"));
    }

    /// ptp4l の状態（ポート 1 つ）と、PHC の読み取りのスナップショット。
    fn phc_snapshot(state: PhcState, port_state: PortState) -> PhcSnapshot {
        let synced = synced_snapshot();
        let sample = synced.last_sample.unwrap();
        PhcSnapshot {
            state,
            stats: PhcStats {
                attempts: 120,
                readings: 120,
                status_updates: 120,
                ..PhcStats::default()
            },
            poll_interval: TimeDelta::from_millis(500),
            anchor: state.shows_time().then_some(synced.anchor).flatten(),
            last_sample: Some(sample),
            selected_sample: Some(sample),
            frequency: None,
            status: Some(PtpStatus {
                domain: 0,
                ports: vec![PortStatus {
                    number: 1,
                    state: port_state,
                }],
                offset_from_master: TimeDelta::from_nanos(-12),
                mean_path_delay: TimeDelta::from_nanos(1_234),
                steps_removed: 1,
                grandmaster: ClockIdentity([0xEC, 0x46, 0x70, 0xFF, 0xFE, 0, 0, 1]),
                grandmaster_quality: ClockQuality {
                    class: 6,
                    accuracy: 0x21,
                    variance: 0x4E5D,
                },
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
            last_success: synced.last_success,
            next_poll: seven_seconds_later(),
        }
    }

    fn phc_target() -> PhcTarget {
        PhcTarget {
            socket: "/var/run/ptp4l".to_owned(),
            domain: 0,
            phc: PhcInfo {
                path: "/dev/ptp0".to_owned(),
                virtual_clock: false,
                from_interface: false,
                method: PhcReadMethod::Precise,
            },
            ports: vec![(1, "eno1".to_owned())],
        }
    }

    fn phc_config() -> TuiConfig {
        let target = phc_target();
        TuiConfig {
            title: "PTP CLOCK".into(),
            server_label: phc_server_label(&target),
            source: TuiSource::Phc {
                phc: target.phc,
                ports: target.ports,
            },
            ..config(Some("footer"))
        }
    }

    #[test]
    fn phc_rows_fit_with_the_big_clock_on_80x24() {
        let snapshot = phc_snapshot(PhcState::Synchronized, PortState::Slave);
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(
            &phc_config(),
            Some(&SourceSnapshot::Phc(Box::new(snapshot))),
            None,
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        assert_eq!(model.server, "/var/run/ptp4l (domain 0)");
        assert_eq!(model.status, "Synchronized (SLAVE)");
        assert_eq!(model.status_color, theme::GOOD);
        let value = |label: &str| {
            model
                .rows
                .iter()
                .find(|row| row.label == label)
                .map(|row| row.value.clone())
                .unwrap()
        };
        assert_eq!(value("Master offset"), "-12 ns");
        assert_eq!(value("Path delay"), "1234 ns");
        assert_eq!(value("Grandmaster"), "ec4670.fffe.000001");
        assert_eq!(value("Clock class"), "6 (synced to reference)");
        assert_eq!(value("UTC offset"), "37 s");
        assert_eq!(value("PC offset"), "-0.978 ms (PHC - PC)");
        assert_eq!(value("Ports"), "eno1 SLAVE");
        assert_eq!(value("PHC"), "/dev/ptp0 (simultaneous read)");
        assert_eq!(value(SUCCESS_LABEL), "120/120");

        let lines = render_text(&model, 80, 24);
        // 目視確認用: cargo test -p ntpterminal-presentation phc_rows_fit -- --nocapture
        println!("{}", lines.join("\n"));
        assert!(lines.iter().any(|line| line.contains('█')));
        assert!(lines.join("\n").contains("footer"));
    }

    #[test]
    fn phc_status_tells_why_the_clock_is_hidden() {
        let listening = phc_snapshot(PhcState::Acquiring, PortState::Listening);
        assert_eq!(listening.anchor, None);
        let model = build(
            &phc_config(),
            Some(&SourceSnapshot::Phc(Box::new(listening))),
            None,
            &SampleHistory::default(),
            None,
            seven_seconds_later(),
        );
        assert_eq!(model.clock, CLOCK_PLACEHOLDER);
        assert_eq!(model.status, "Looking for master (LISTENING)");

        let mut lost = phc_snapshot(PhcState::Holdover, PortState::Slave);
        lost.status_error = Some("ptp4l から CURRENT_DATA_SET の応答がありません".to_owned());
        let holdover = build(
            &phc_config(),
            Some(&SourceSnapshot::Phc(Box::new(lost))),
            None,
            &SampleHistory::default(),
            None,
            seven_seconds_later(),
        );
        assert_eq!(holdover.status, "Holdover (no response from ptp4l)");
        assert_eq!(holdover.status_color, theme::WARN);

        let waiting = build(
            &phc_config(),
            None,
            None,
            &SampleHistory::default(),
            None,
            seven_seconds_later(),
        );
        assert_eq!(waiting.status, "Waiting for ptp4l");
        assert!(waiting.rows.iter().any(|row| row.value == NO_VALUE));
    }

    /// マスタ 1 台に問い合わせた結果（Sync 1 秒、`Delay_Req` はマスタの最小間隔の 1 秒）のスナップショット。
    fn master_snapshot(state: MasterState) -> MasterSnapshot {
        let synced = synced_snapshot();
        let sample = synced.last_sample.unwrap();
        MasterSnapshot {
            domain: 0,
            state,
            stats: MasterStats {
                announces: 60,
                syncs: 120,
                requests: 120,
                samples: 118,
                failures: 2,
                ignored: 0,
            },
            status: Some(MasterStatus {
                master: PortIdentity {
                    clock: ClockIdentity([0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56]),
                    port: 1,
                },
                grandmaster: ClockIdentity([0xEC, 0x46, 0x70, 0xFF, 0xFE, 0, 0, 1]),
                priority1: 128,
                priority2: 128,
                grandmaster_quality: ClockQuality {
                    class: 6,
                    accuracy: 0x21,
                    variance: 0x4E5D,
                },
                steps_removed: 1,
                time: TimeProperties {
                    utc_offset: 37,
                    utc_offset_valid: true,
                    timescale: Timescale::Ptp,
                    leap: LeapWarning::None,
                    time_traceable: true,
                    frequency_traceable: true,
                    time_source: TimeSourceKind::Gnss,
                },
                announce_interval: TimeDelta::from_secs(2),
                sync_interval: Some(TimeDelta::from_secs(1)),
                two_step: Some(true),
            }),
            candidates: 1,
            configured_interval: TimeDelta::from_millis(500),
            request_interval: TimeDelta::from_secs(1),
            master_min_interval: Some(TimeDelta::from_secs(1)),
            anchor: state.shows_time().then_some(synced.anchor).flatten(),
            last_sample: Some(sample),
            selected_sample: Some(sample),
            frequency: None,
            last_success: synced.last_success,
            last_failure: None,
            other_domains: Vec::new(),
            master_address: Some(SocketAddr::from(([192, 168, 10, 50], 319))),
            receive_error: None,
        }
    }

    fn master_config() -> TuiConfig {
        let target = MasterTarget {
            domain: 0,
            transport: PtpTransportInfo {
                interface: Some("nic0".to_owned()),
                local: Ipv4Addr::new(192, 168, 10, 200),
                event_port: 319,
                unicast_to: None,
            },
        };
        TuiConfig {
            title: "PTP CLOCK".into(),
            server_label: master_server_label(&target),
            source: TuiSource::Ptp {
                transport: target.transport,
                timestamping: Arc::new(TimestampingStatus::new()),
            },
            ..config(Some("footer"))
        }
    }

    #[test]
    fn master_rows_fit_with_the_big_clock_on_80x24() {
        let snapshot = master_snapshot(MasterState::Synchronized);
        let now = seven_seconds_later();
        let shown = snapshot.anchor.unwrap().project(now).unwrap();
        let model = build(
            &master_config(),
            Some(&SourceSnapshot::Ptp(Box::new(snapshot))),
            None,
            &SampleHistory::default(),
            Some(shown),
            now,
        );
        assert_eq!(model.server, "PTP master (domain 0)");
        assert_eq!(model.status, "Synchronized");
        assert_eq!(model.status_color, theme::GOOD);
        let value = |label: &str| {
            model
                .rows
                .iter()
                .find(|row| row.label == label)
                .map(|row| row.value.clone())
                .unwrap()
        };
        assert_eq!(value("PC offset"), "-0.978 ms (master - PC)");
        // 往復 0.423ms の片道
        assert_eq!(value("Path delay"), "0.211 ms");
        assert_eq!(value("Master"), "001b21.fffe.123456-1 (192.168.10.50)");
        assert_eq!(value("Clock class"), "6 (synced to reference)");
        assert_eq!(
            value("Intervals"),
            "Sync 1 s (two-step), Delay_Req 1 s (master minimum)"
        );
        assert_eq!(value(SUCCESS_LABEL), "118/120");
        assert_eq!(
            value("Transport"),
            "nic0 192.168.10.200, UDP 319/320, multicast"
        );
        assert_eq!(model.rows.len(), 13);

        let lines = render_text(&model, 80, 24);
        // 目視確認用: cargo test -p ntpterminal-presentation master_rows_fit -- --nocapture
        println!("{}", lines.join("\n"));
        assert!(lines.iter().any(|line| line.contains('█')));
        assert!(lines.join("\n").contains("footer"));
    }

    #[test]
    fn master_status_tells_why_the_clock_is_hidden() {
        let mut listening = master_snapshot(MasterState::Listening);
        listening.status = None;
        assert_eq!(listening.anchor, None);
        let model = build(
            &master_config(),
            Some(&SourceSnapshot::Ptp(Box::new(listening))),
            None,
            &SampleHistory::default(),
            None,
            seven_seconds_later(),
        );
        assert_eq!(model.clock, CLOCK_PLACEHOLDER);
        assert_eq!(model.status, "Looking for master (domain 0)");

        let mut silent = master_snapshot(MasterState::Uncalibrated);
        silent.last_failure = Some(ExchangeFailure::DelayRespMissing);
        let measuring = build(
            &master_config(),
            Some(&SourceSnapshot::Ptp(Box::new(silent))),
            None,
            &SampleHistory::default(),
            None,
            seven_seconds_later(),
        );
        assert_eq!(measuring.status, "Measuring master time (no Delay_Resp)");

        let mut lost = master_snapshot(MasterState::Holdover);
        lost.status = None;
        let holdover = build(
            &master_config(),
            Some(&SourceSnapshot::Ptp(Box::new(lost))),
            None,
            &SampleHistory::default(),
            None,
            seven_seconds_later(),
        );
        assert_eq!(holdover.status, "Holdover (master lost)");
        assert_eq!(holdover.status_color, theme::WARN);
    }

    /// 画面の文字が ASCII だけか（状態の印 `●`・大きい時計と推移の棒の半ブロック `▀` `▄`・`render_text` が
    /// 時計の背景を表す `█` を除く）。
    /// 描いた結果だけでなく、画面の内容（行の値など。狭い端末で切れて見えなくなるものも）も確かめる。
    fn assert_ascii(model: &ScreenModel) {
        let mut texts = vec![
            model.server.clone(),
            model.clock.clone(),
            model.zone_label.clone(),
            model.status.clone(),
        ];
        texts.extend(model.title.clone());
        texts.extend(model.date.clone());
        texts.extend(model.footer.clone());
        for row in &model.rows {
            texts.push(row.label.to_owned());
            texts.push(row.value.clone());
        }
        for trend in &model.trends {
            texts.push(trend.label.to_owned());
            texts.push(trend.max.clone());
            texts.push(trend.min.clone());
        }
        for text in &texts {
            assert!(text.is_ascii(), "{text:?}");
        }
        let screen = render_text(model, 200, 60).join("\n");
        assert!(
            screen
                .chars()
                .all(|ch| ch.is_ascii() || matches!(ch, '●' | '▀' | '▄' | '█')),
            "{screen}"
        );
    }

    /// 受信機の監視のスナップショットの、ありうる形（応答前・取れた・失敗の種類ごと）。
    fn receiver_variants(now: PcTime) -> Vec<Option<ReceiverSnapshot>> {
        let receiver_status = |antenna| ReceiverStatus {
            name: Some("gps180".to_owned()),
            state: Some(ReceiverState::Acquiring),
            state_detail: Some("gpsSync".to_owned()),
            antenna: Some(antenna),
            holdover: Some(true),
            satellites: SatelliteCounts {
                good: Some(5),
                in_view: Some(9),
                expected: Some(11),
                tracking: Some(3),
            },
            constellations: vec![
                ConstellationCount {
                    constellation: Constellation::Gps,
                    count: 2,
                },
                ConstellationCount {
                    constellation: Constellation::Other("IRNSS".to_owned()),
                    count: 1,
                },
            ],
        };
        let read = |status: ReceiverStatus| ReceiverSnapshot {
            profile: Some(MibProfile::MicrochipS650),
            status: Some(status),
            last_success: Some(now),
            ..ReceiverSnapshot::default()
        };
        let mut variants = vec![
            None,
            Some(ReceiverSnapshot::default()),
            Some(read(ReceiverStatus::default())),
        ];
        for antenna in [
            AntennaState::Connected,
            AntennaState::Disconnected,
            AntennaState::ShortCircuit,
            AntennaState::Faulty,
        ] {
            variants.push(Some(read(receiver_status(antenna))));
        }
        for error in [
            MonitorError::Transport(TransportError::Unreachable(
                "Connection refused (os error 111)".to_owned(),
            )),
            MonitorError::Transport(TransportError::Io("Broken pipe (os error 32)".to_owned())),
            MonitorError::Timeout,
            MonitorError::ErrorStatus {
                status: 2,
                index: 1,
            },
            MonitorError::UnknownMib,
            MonitorError::Mib(MibError::NoValues(MibProfile::MeinbergOs)),
            MonitorError::Stopped,
        ] {
            let mut failed = read(receiver_status(AntennaState::Connected));
            failed.last_error = Some(error);
            variants.push(Some(failed.clone()));
            failed.last_success = None;
            variants.push(Some(failed));
        }
        variants
    }

    /// NTP: 送信方式・同期の状態・受信機の状態（SNMP のエラーを含む）ごとに、英語（ASCII）だけで描く。
    #[test]
    fn ntp_screens_are_ascii() {
        let now = seven_seconds_later();
        let shown = synced_snapshot().anchor.unwrap().project(now).ok();
        let transports = [
            udp(),
            NtpTransportInfo {
                kind: NtpTransportKind::RawIp,
                hardware: Some(HardwareTimestampInfo {
                    device: "nic0".to_owned(),
                    enabled_temporarily: false,
                }),
            },
            NtpTransportInfo {
                kind: NtpTransportKind::Link {
                    interface: "eth0".to_owned(),
                    destination: MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
                },
                hardware: Some(HardwareTimestampInfo {
                    device: "eth0".to_owned(),
                    enabled_temporarily: true,
                }),
            },
        ];
        let mut snapshots = vec![None];
        for state in [
            SyncState::Unsynchronized,
            SyncState::Synchronized,
            SyncState::Holdover,
            SyncState::Denied,
        ] {
            let mut snapshot = synced_snapshot();
            snapshot.state = state;
            snapshots.push(Some(SourceSnapshot::Ntp(Box::new(snapshot))));
        }
        let receivers = receiver_variants(now);
        for transport in transports {
            let ntp = with_trend(TuiConfig {
                source: TuiSource::Ntp {
                    receiver: true,
                    transport,
                    timestamping: Arc::new(TimestampingStatus::new()),
                },
                ..config(Some("footer"))
            });
            for snapshot in &snapshots {
                for receiver in &receivers {
                    assert_ascii(&build(
                        &ntp,
                        snapshot.as_ref(),
                        receiver.as_ref(),
                        &recent_history(),
                        shown,
                        now,
                    ));
                }
            }
        }
    }

    /// ptp4l の PHC: PHC の読み方・状態・ポートの状態ごとに、英語（ASCII）だけで描く。
    /// ptp4l の問い合わせの失敗の説明（日本語）は画面に出さない。
    #[test]
    fn phc_screens_are_ascii() {
        let now = seven_seconds_later();
        let shown = synced_snapshot().anchor.unwrap().project(now).ok();
        for (virtual_clock, from_interface, method) in [
            (false, false, PhcReadMethod::Precise),
            (true, false, PhcReadMethod::Extended),
            (false, true, PhcReadMethod::Basic),
        ] {
            let mut target = phc_target();
            target.phc = PhcInfo {
                virtual_clock,
                from_interface,
                method,
                ..target.phc
            };
            let phc = with_trend(TuiConfig {
                server_label: phc_server_label(&target),
                source: TuiSource::Phc {
                    phc: target.phc,
                    ports: target.ports,
                },
                ..phc_config()
            });
            assert_ascii(&build(&phc, None, None, &recent_history(), None, now));
            for state in [
                PhcState::Starting,
                PhcState::Acquiring,
                PhcState::Synchronized,
                PhcState::Master,
                PhcState::Holdover,
                PhcState::Faulty,
            ] {
                for port_state in [
                    PortState::Slave,
                    PortState::Uncalibrated,
                    PortState::Unknown(9),
                ] {
                    for error in [
                        None,
                        Some("ptp4l から CURRENT_DATA_SET の応答がありません".to_owned()),
                    ] {
                        let mut snapshot = phc_snapshot(state, port_state);
                        snapshot.status_error = error;
                        if let Some(status) = snapshot.status.as_mut() {
                            // ポート番号 2 はインターフェース名が分からない
                            status.ports.push(PortStatus {
                                number: 2,
                                state: port_state,
                            });
                            status.time.timescale = Timescale::Arbitrary;
                            status.time.leap = LeapWarning::Insert;
                            status.time.time_source = TimeSourceKind::Unknown(0x42);
                        }
                        let source = SourceSnapshot::Phc(Box::new(snapshot));
                        let mut history = SampleHistory::default();
                        history.observe(&source);
                        assert_ascii(&build(&phc, Some(&source), None, &history, shown, now));
                    }
                }
            }
        }
    }

    /// PTP のマスタ: 送受信の口・状態・交換の失敗の理由・受信の失敗ごとに、英語（ASCII）だけで描く。
    /// 送受信の口のエラーの説明（日本語）は画面に出さない。
    #[test]
    fn master_screens_are_ascii() {
        let now = seven_seconds_later();
        let shown = synced_snapshot().anchor.unwrap().project(now).ok();
        let failures = [
            None,
            Some(ExchangeFailure::RequestNotSent),
            Some(ExchangeFailure::FollowUpMissing),
            Some(ExchangeFailure::DelayRespMissing),
            Some(ExchangeFailure::SendFailed(
                "通信エラー: Network is unreachable (os error 101)".to_owned(),
            )),
            Some(ExchangeFailure::InvalidSample(
                SampleError::TimebaseMismatch(TimebaseMismatch {
                    left: Timebase::Monotonic,
                    right: Timebase::KernelRealtime,
                }),
            )),
            Some(ExchangeFailure::InvalidSample(
                SampleError::ServerTimeReversed,
            )),
            Some(ExchangeFailure::InvalidSample(
                SampleError::LocalTimeReversed,
            )),
        ];
        for unicast_to in [None, Some(SocketAddr::from(([192, 168, 10, 50], 319)))] {
            let target = MasterTarget {
                domain: 3,
                transport: PtpTransportInfo {
                    interface: None,
                    local: Ipv4Addr::new(192, 168, 10, 200),
                    event_port: 319,
                    unicast_to,
                },
            };
            let master = with_trend(TuiConfig {
                server_label: master_server_label(&target),
                source: TuiSource::Ptp {
                    transport: target.transport,
                    timestamping: Arc::new(TimestampingStatus::new()),
                },
                ..master_config()
            });
            assert_ascii(&build(&master, None, None, &recent_history(), None, now));
            for state in [
                MasterState::Listening,
                MasterState::Uncalibrated,
                MasterState::Synchronized,
                MasterState::Holdover,
            ] {
                for failure in &failures {
                    for receive_error in [None, Some("受信のスレッドが止まりました".to_owned())]
                    {
                        for with_status in [true, false] {
                            let mut snapshot = master_snapshot(state);
                            snapshot.last_failure.clone_from(failure);
                            snapshot.receive_error.clone_from(&receive_error);
                            snapshot.master_min_interval = None;
                            if with_status {
                                if let Some(status) = snapshot.status.as_mut() {
                                    status.two_step = Some(false);
                                    status.time.utc_offset_valid = false;
                                    status.time.leap = LeapWarning::Delete;
                                    status.time.time_source = TimeSourceKind::HandSet;
                                    status.grandmaster_quality.class = 7;
                                }
                            } else {
                                snapshot.status = None;
                            }
                            let source = SourceSnapshot::Ptp(Box::new(snapshot));
                            let mut history = SampleHistory::default();
                            history.observe(&source);
                            assert_ascii(&build(
                                &master,
                                Some(&source),
                                None,
                                &history,
                                shown,
                                now,
                            ));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn letter_spacing_keeps_word_gap() {
        assert_eq!(letter_spaced("NTP CLOCK"), "N T P   C L O C K");
    }
}
