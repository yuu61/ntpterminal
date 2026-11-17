//! ptp4l が合わせる PHC を表示するときの、状態の行と情報欄。
//!
//! - 状態の行は、ptp4l のポートの状態（`SLAVE` など）を添える。ptp4l が同期を知らせるまでは時計を出さない
//!   （基準点は集約が出さない）ので、その間は「マスタを探しています」などと出す
//! - 情報欄は、マスタとの差・経路の遅延・グランドマスタ・clockClass・時刻の出どころ・UTC との差・うるう秒の予告・
//!   PC との差（PHC − PC。PC の時計の合い方の確認用）・ポート・PHC・最終取得・成功/試行の 12 行。
//!   NTP の 11 行 + 受信機の 2 行より少ないので、80×24 でも大きい時計は縮まない
//! - ptp4l の状態を読めなくなったら、最後に読めた値を出し続けるが、状態の行で「ptp4l の応答なし」と知らせる

use ratatui::style::Color;

use application::snapshot::PhcSnapshot;
use domain::ptp_clock::{LeapWarning, PhcState};
use domain::shared_kernel::{PcTime, TimeDelta};

use super::screen::{InfoRow, NO_VALUE, tone};
use super::theme;
use crate::format::format_offset_ms;
use crate::format::ptp::{
    clock_class_label, format_nanos, format_nanos_signed, phc_state_label, port_label,
    ptp_leap_label, time_source_label, utc_offset_label,
};

/// 状態の行の文言と色。
pub(super) fn status(snapshot: Option<&PhcSnapshot>) -> (String, Color) {
    let Some(snap) = snapshot else {
        return (
            phc_state_label(PhcState::Starting, None, None),
            theme::LABEL,
        );
    };
    let label = phc_state_label(
        snap.state,
        snap.status.as_ref(),
        snap.status_error.as_deref(),
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

/// 情報欄。`phc` は PHC の説明、`ports` は PHC を使うポートの番号とインターフェース名。
pub(super) fn rows(
    snapshot: Option<&PhcSnapshot>,
    phc: &str,
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
        let (text, trusted) = utc_offset_label(&ptp.time);
        (Some(text), trusted)
    });
    let leap = status.map(|ptp| ptp.time.leap);
    vec![
        row(
            "マスタとの差",
            master_offset.map(format_nanos_signed),
            master_offset.map_or(theme::TEXT, ptp_tone),
        ),
        row(
            "経路遅延",
            status.map(|ptp| format_nanos(ptp.mean_path_delay)),
            theme::TEXT,
        ),
        row(
            "グランドマスタ",
            status.map(|ptp| ptp.grandmaster.to_string()),
            theme::TEXT,
        ),
        row(
            "Clock class",
            status.map(|ptp| clock_class_label(ptp.grandmaster_quality.class)),
            theme::TEXT,
        ),
        row(
            "時刻源",
            status.map(|ptp| time_source_label(ptp.time.time_source)),
            theme::TEXT,
        ),
        row(
            "UTC との差",
            utc,
            if utc_trusted {
                theme::TEXT
            } else {
                theme::WARN
            },
        ),
        row(
            "うるう秒",
            leap.map(|warning| ptp_leap_label(warning).to_owned()),
            if leap.is_some_and(|warning| warning != LeapWarning::None) {
                theme::WARN
            } else {
                theme::TEXT
            },
        ),
        row(
            "PC との差",
            pc_offset.map(|offset| format!("{}（PHC − PC）", format_offset_ms(offset))),
            pc_offset.map_or(theme::TEXT, tone),
        ),
        row(
            "ポート",
            status.map(|ptp| port_label(ptp, ports)),
            theme::TEXT,
        ),
        row("PHC", Some(phc.to_owned()), theme::LABEL),
        row(
            "最終取得",
            snapshot
                .and_then(|snap| snap.last_success)
                .and_then(|at| now.since(at).ok())
                .map(|elapsed| format!("{:.1} 秒前", elapsed.max(TimeDelta::ZERO).as_secs_f64())),
            theme::TEXT,
        ),
        row(
            "成功/試行",
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
