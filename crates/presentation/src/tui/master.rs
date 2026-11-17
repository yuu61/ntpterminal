//! PTP のスレーブとしてマスタに問い合わせるときの、状態の行と情報欄。
//!
//! - 状態の行: マスタを探している間・測っている間は時計を出さない（基準点は集約が出さない）ので、その理由を出す。
//!   測れない理由（`Delay_Resp` が届かないなど）があれば添える
//! - 情報欄は、PC との差（マスタ − PC）・経路の遅延（片道）・マスタ・グランドマスタ・clockClass・時刻の出どころ・
//!   UTC との差・うるう秒の予告・間隔（Sync と `Delay_Req`）・最終取得・成功/試行・送受信・タイムスタンプの 13 行。
//!   NTP の 11 行 + 受信機の 2 行と同じなので、80×24 でも大きい時計は縮まない
//! - PC との差は NTP と同じくミリ秒で出す（PC の時計の合い方を見るもの。ナノ秒の精度はない）。経路の遅延は
//!   PTP の表示に合わせてナノ秒で出す

use ratatui::style::Color;

use application::description::PtpTransportInfo;
use application::snapshot::MasterSnapshot;
use application::timestamping::{TimestampSource, TimestampingStatus};
use domain::ptp_clock::{LeapWarning, MasterState};
use domain::shared_kernel::PcTime;

use super::screen::{InfoRow, NO_VALUE, SUCCESS_LABEL, tone};
use super::{text, theme};
use crate::format::format_offset_ms;
use crate::format::ptp::format_nanos;

/// 状態の行の文言と色。
pub(super) fn status(snapshot: Option<&MasterSnapshot>) -> (String, Color) {
    let Some(snap) = snapshot else {
        return ("Looking for master".to_owned(), theme::LABEL);
    };
    let color = match snap.state {
        MasterState::Synchronized => theme::GOOD,
        MasterState::Listening if snap.receive_error.is_some() => theme::BAD,
        MasterState::Listening | MasterState::Uncalibrated => theme::LABEL,
        MasterState::Holdover => theme::WARN,
    };
    (text::master_state(snap), color)
}

/// 情報欄。`transport` は送受信の口、`timestamping` はタイムスタンプの取得状況。
pub(super) fn rows(
    snapshot: Option<&MasterSnapshot>,
    transport: &PtpTransportInfo,
    timestamping: &TimestampingStatus,
    now: PcTime,
) -> Vec<InfoRow> {
    let status = snapshot.and_then(|snap| snap.status);
    let row = |label, value: Option<String>, color| InfoRow {
        label,
        value: value.unwrap_or_else(|| NO_VALUE.to_owned()),
        color,
    };
    let selected = snapshot.and_then(|snap| snap.selected_sample);
    let pc_offset = selected.map(|sample| sample.offset().delta());
    let (utc, utc_trusted) = status.map_or((None, true), |master| {
        let (offset, trusted) = text::utc_offset(&master.time);
        (Some(offset), trusted)
    });
    let leap = status.map(|master| master.time.leap);
    let counts = timestamping.counts();
    vec![
        row(
            "PC offset",
            pc_offset.map(|offset| format!("{} (master - PC)", format_offset_ms(offset))),
            pc_offset.map_or(theme::TEXT, tone),
        ),
        row(
            "Path delay",
            selected.map(|sample| format_nanos(sample.delay().delta() / 2)),
            theme::TEXT,
        ),
        row(
            "Master",
            status
                .map(|master| text::master(&master, snapshot.and_then(|snap| snap.master_address))),
            theme::TEXT,
        ),
        row(
            "Grandmaster",
            status.map(|master| master.grandmaster.to_string()),
            theme::TEXT,
        ),
        row(
            "Clock class",
            status.map(|master| text::clock_class(master.grandmaster_quality.class)),
            theme::TEXT,
        ),
        row(
            "Time source",
            status.map(|master| text::time_source(master.time.time_source)),
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
        row("Intervals", snapshot.map(text::intervals), theme::TEXT),
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
            snapshot.map(|snap| format!("{}/{}", snap.stats.samples, snap.stats.requests)),
            theme::TEXT,
        ),
        row(
            "Transport",
            Some(text::ptp_transport(transport)),
            theme::LABEL,
        ),
        row(
            "Timestamps",
            Some(text::timestamping(&counts).to_owned()),
            // カーネルの時刻が、設定どおりに取れているときだけ「良好」の色にする
            if counts.last_source != TimestampSource::User
                && counts.last_source == counts.expected_source()
            {
                theme::GOOD
            } else {
                theme::LABEL
            },
        ),
    ]
}
