//! ptp4l が合わせる PHC を表示するときの、状態の行と情報欄。
//!
//! - 状態の行は、ptp4l のポートの状態（`SLAVE` など）を添える。ptp4l が同期を知らせるまでは時計を出さない
//!   （基準点は集約が出さない）ので、その間は "Looking for master" などと出す
//! - 情報欄は、マスタとの差・経路の遅延・グランドマスタ・clockClass・時刻の出どころ・UTC との差・うるう秒の予告・
//!   PC との差（PHC − PC。PC の時計の合い方の確認用）・ポート・PHC・最終取得・成功/試行の 12 行。
//!   NTP の 11 行 + 受信機の 2 行より少ないので、80×24 でも大きい時計は縮まない
//! - ptp4l の状態を読めなくなったら、最後に読めた値を出し続けるが、状態の行で ptp4l の応答がないと知らせる

use ratatui::style::Color;

use application::description::PhcInfo;
use application::snapshot::PhcSnapshot;
use domain::ptp_clock::{LeapWarning, PhcState};
use domain::shared_kernel::{PcTime, TimeDelta};

use super::screen::{InfoRow, NO_VALUE, SUCCESS_LABEL, tone};
use super::{text, theme};
use crate::format::format_offset_ms;
use crate::format::ptp::{format_nanos, format_nanos_signed};

/// 状態の行の文言と色。
pub(super) fn status(snapshot: Option<&PhcSnapshot>) -> (String, Color) {
    let Some(snap) = snapshot else {
        return (
            text::phc_state(PhcState::Starting, None, false),
            theme::LABEL,
        );
    };
    let label = text::phc_state(
        snap.state,
        snap.status.as_ref(),
        snap.status_error.is_some(),
    );
    let color = match snap.state {
        PhcState::Synchronized | PhcState::Master => theme::GOOD,
        PhcState::Starting if snap.status_error.is_some() => theme::BAD,
        PhcState::Starting | PhcState::Acquiring => theme::LABEL,
        PhcState::Holdover => theme::WARN,
        PhcState::Faulty => theme::BAD,
    };
    (label, color)
}

/// 情報欄。`phc` は開いた PHC、`ports` は PHC を使うポートの番号とインターフェース名。
pub(super) fn rows(
    snapshot: Option<&PhcSnapshot>,
    phc: &PhcInfo,
    ports: &[(u16, String)],
    now: PcTime,
) -> Vec<InfoRow> {
    let status = snapshot.and_then(|snap| snap.status.as_ref());
    let row = |label, value: Option<String>, color| InfoRow {
        label,
        value: value.unwrap_or_else(|| NO_VALUE.to_owned()),
        color,
    };
    let master_offset = status.map(|ptp| ptp.offset_from_master);
    let pc_offset = snapshot
        .and_then(|snap| snap.selected_sample)
        .map(|sample| sample.offset().delta());
    let (utc, utc_trusted) = status.map_or((None, true), |ptp| {
        let (offset, trusted) = text::utc_offset(&ptp.time);
        (Some(offset), trusted)
    });
    let leap = status.map(|ptp| ptp.time.leap);
    vec![
        row(
            "Master offset",
            master_offset.map(format_nanos_signed),
            master_offset.map_or(theme::TEXT, ptp_tone),
        ),
        row(
            "Path delay",
            status.map(|ptp| format_nanos(ptp.mean_path_delay)),
            theme::TEXT,
        ),
        row(
            "Grandmaster",
            status.map(|ptp| ptp.grandmaster.to_string()),
            theme::TEXT,
        ),
        row(
            "Clock class",
            status.map(|ptp| text::clock_class(ptp.grandmaster_quality.class)),
            theme::TEXT,
        ),
        row(
            "Time source",
            status.map(|ptp| text::time_source(ptp.time.time_source)),
            theme::TEXT,
        ),
        row(
            "UTC offset",
            utc,
            if utc_trusted {
                theme::TEXT
            } else {
                theme::WARN
            },
        ),
        row(
            "Leap second",
            leap.map(|warning| text::leap(warning).to_owned()),
            if leap.is_some_and(|warning| warning != LeapWarning::None) {
                theme::WARN
            } else {
                theme::TEXT
            },
        ),
        row(
            "PC offset",
            pc_offset.map(|offset| format!("{} (PHC - PC)", format_offset_ms(offset))),
            pc_offset.map_or(theme::TEXT, tone),
        ),
        row(
            "Ports",
            status.map(|ptp| text::ports(ptp, ports)),
            theme::TEXT,
        ),
        row("PHC", Some(text::phc(phc)), theme::LABEL),
        row(
            "Last update",
            snapshot
                .and_then(|snap| snap.last_success)
                .and_then(|at| now.since(at).ok())
                .map(text::ago),
            theme::TEXT,
        ),
        row(
            SUCCESS_LABEL,
            snapshot.map(|snap| format!("{}/{}", snap.stats.readings, snap.stats.attempts)),
            theme::TEXT,
        ),
    ]
}

/// ptp4l のマスタとの差の色（1 µs 未満: 良好、100 µs 未満: 注意、それ以上: 異常）。
fn ptp_tone(delta: TimeDelta) -> Color {
    let abs = delta.abs();
    if abs < TimeDelta::from_micros(1) {
        theme::GOOD
    } else if abs < TimeDelta::from_micros(100) {
        theme::WARN
    } else {
        theme::BAD
    }
}
