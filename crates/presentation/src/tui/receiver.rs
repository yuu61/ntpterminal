//! 受信機の情報欄（`tui --snmp-community` のときだけ出す 2 行）。
//!
//! - 1 行目 "GNSS receiver": MIB・受信機の名前・状態（アンテナ・ホールドオーバー）。直近の問い合わせが失敗したら、
//!   そのことと、最後に取れてからの経過時間を出す
//! - 2 行目 "Satellites": 衛星の数を意味ごとに（良好・可視・見えるはず・追尾）。メーカーが数えていない意味の数は
//!   出さない。直近の問い合わせが失敗したら、代わりに "SNMP error" としてエラーの内容を出す（古い数を今の値のように
//!   出さない）
//! - 行を 2 つに抑えるのは、80×24 の端末でも大きい時計を縮めないため
//! - エラーの文言は、エラーの種類ごとにここで英語にする（[`MonitorError`] の `Display` は CLI 用の日本語）

use ratatui::style::Color;

use application::acl::mib::{MibError, MibProfile};
use application::port::TransportError;
use application::snapshot::ReceiverSnapshot;
use application::usecase::MonitorError;
use domain::receiver::{
    AntennaState, Constellation, ConstellationCount, ReceiverState, ReceiverStatus, SatelliteCounts,
};
use domain::shared_kernel::PcTime;

use super::screen::InfoRow;
use super::{text, theme};

const LABEL_RECEIVER: &str = "GNSS receiver";
const LABEL_SATELLITES: &str = "Satellites";
const LABEL_ERROR: &str = "SNMP error";

/// 受信機の 2 行。`snapshot` がなければ（最初の応答の前）問い合わせ中と出す。
pub(super) fn rows(snapshot: Option<&ReceiverSnapshot>, now: PcTime) -> [InfoRow; 2] {
    let Some(snap) = snapshot else {
        return waiting();
    };
    if let Some(error) = snap.last_error.as_ref() {
        let since = snap
            .last_success
            .and_then(|at| now.since(at).ok())
            .map_or_else(
                || "never succeeded".to_owned(),
                |elapsed| format!("last success {}", text::ago(elapsed)),
            );
        return [
            InfoRow {
                label: LABEL_RECEIVER,
                value: with_source(snap.profile, None, &format!("Query failed ({since})")),
                color: theme::BAD,
            },
            InfoRow {
                label: LABEL_ERROR,
                value: monitor_error(error),
                color: theme::BAD,
            },
        ];
    }
    let Some(status) = snap.status.as_ref() else {
        return waiting();
    };
    [
        InfoRow {
            label: LABEL_RECEIVER,
            value: with_source(snap.profile, status.name.as_deref(), &state_text(status)),
            color: state_color(status),
        },
        InfoRow {
            label: LABEL_SATELLITES,
            value: satellites_text(&status.satellites, &status.constellations),
            color: theme::TEXT,
        },
    ]
}

fn waiting() -> [InfoRow; 2] {
    [
        InfoRow {
            label: LABEL_RECEIVER,
            value: "Querying".to_owned(),
            color: theme::LABEL,
        },
        InfoRow {
            label: LABEL_SATELLITES,
            value: "--".to_owned(),
            color: theme::TEXT,
        },
    ]
}

/// "Meinberg LTNG (gps180): Synchronized (gpsSync)" のように、どこから読んだかを前に付ける。
fn with_source(profile: Option<MibProfile>, name: Option<&str>, state: &str) -> String {
    let source = match (profile.map(mib_label), name) {
        (Some(mib), Some(receiver)) => format!("{mib} ({receiver}): "),
        (Some(mib), None) => format!("{mib}: "),
        (None, Some(receiver)) => format!("{receiver}: "),
        (None, None) => String::new(),
    };
    format!("{source}{state}")
}

/// 受信機の監視のエラー。OS が返す説明（送受信のエラー）だけは、そのまま添える。
#[expect(
    clippy::pattern_type_mismatch,
    reason = "送受信のエラーの説明（String）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
)]
fn monitor_error(error: &MonitorError) -> String {
    match error {
        MonitorError::Transport(TransportError::Unreachable(detail)) => {
            format!("Unreachable: {detail}")
        }
        MonitorError::Transport(TransportError::Io(detail)) => format!("I/O error: {detail}"),
        MonitorError::Timeout => {
            "No SNMP response (check community, port and SNMP settings of the device)".to_owned()
        }
        MonitorError::ErrorStatus { status, index } => {
            format!("SNMP error response (error-status {status}, error-index {index})")
        }
        MonitorError::UnknownMib => "No supported MIB found (Meinberg, Microchip S650)".to_owned(),
        MonitorError::Mib(MibError::NoValues(profile)) => format!(
            "{} returned no values (device does not support this MIB)",
            mib_label(*profile)
        ),
        MonitorError::Stopped => "Stopped".to_owned(),
    }
}

/// 画面に出す MIB の短い名前。
#[must_use]
pub const fn mib_label(profile: MibProfile) -> &'static str {
    match profile {
        MibProfile::MeinbergOs => "Meinberg OS",
        MibProfile::MeinbergLtng => "Meinberg LTNG",
        MibProfile::MeinbergLt => "Meinberg LT",
        MibProfile::MicrochipS650 => "Microchip S650",
    }
}

/// "Synchronized (gpsSync), antenna OK, holdover"
fn state_text(status: &ReceiverStatus) -> String {
    let state = match status.state {
        Some(ReceiverState::Synchronized) => "Synchronized",
        Some(ReceiverState::Acquiring) => "Acquiring",
        Some(ReceiverState::NotSynchronized) => "Not synchronized",
        None => "Unknown state",
    };
    let mut parts = vec![
        status
            .state_detail
            .as_deref()
            .map_or_else(|| state.to_owned(), |detail| format!("{state} ({detail})")),
    ];
    if let Some(antenna) = status.antenna {
        parts.push(
            match antenna {
                AntennaState::Connected => "antenna OK",
                AntennaState::Disconnected => "antenna disconnected",
                AntennaState::ShortCircuit => "antenna short circuit",
                AntennaState::Faulty => "antenna fault",
            }
            .to_owned(),
        );
    }
    if status.holdover == Some(true) {
        parts.push("holdover".to_owned());
    }
    parts.join(", ")
}

/// 同期していない・アンテナの異常は異常、捕捉中・ホールドオーバーは注意。
fn state_color(status: &ReceiverStatus) -> Color {
    let antenna_problem = matches!(
        status.antenna,
        Some(AntennaState::Disconnected | AntennaState::ShortCircuit | AntennaState::Faulty)
    );
    match status.state {
        Some(ReceiverState::NotSynchronized) => theme::BAD,
        _ if antenna_problem => theme::BAD,
        Some(ReceiverState::Acquiring) => theme::WARN,
        _ if status.holdover == Some(true) => theme::WARN,
        Some(ReceiverState::Synchronized) => theme::GOOD,
        None => theme::TEXT,
    }
}

/// "good 5 / in view 9"、"tracking 3 (GPS 2, Galileo 1)"。どの数も取れなければ "Unavailable"。
fn satellites_text(counts: &SatelliteCounts, constellations: &[ConstellationCount]) -> String {
    let parts: Vec<String> = [
        ("good", counts.good),
        ("in view", counts.in_view),
        ("expected", counts.expected),
        ("tracking", counts.tracking),
    ]
    .into_iter()
    .filter_map(|(label, count)| count.map(|number| format!("{label} {number}")))
    .collect();
    if parts.is_empty() {
        return "Unavailable".to_owned();
    }
    let mut text = parts.join(" / ");
    if !constellations.is_empty() {
        let breakdown: Vec<String> = constellations
            .iter()
            .map(|entry| {
                format!(
                    "{} {}",
                    constellation_label(&entry.constellation),
                    entry.count
                )
            })
            .collect();
        text = format!("{text} ({})", breakdown.join(", "));
    }
    text
}

#[expect(
    clippy::pattern_type_mismatch,
    reason = "知らない衛星系の名前（String）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
)]
fn constellation_label(constellation: &Constellation) -> &str {
    match constellation {
        Constellation::Gps => "GPS",
        Constellation::Glonass => "GLONASS",
        Constellation::Galileo => "Galileo",
        Constellation::BeiDou => "BeiDou",
        Constellation::Qzss => "QZSS",
        Constellation::Sbas => "SBAS",
        Constellation::Navic => "NavIC",
        Constellation::Other(name) => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(status: ReceiverStatus) -> ReceiverSnapshot {
        ReceiverSnapshot {
            profile: Some(MibProfile::MeinbergLtng),
            status: Some(status),
            last_success: Some(PcTime::monotonic(1_000_000_000)),
            last_error: None,
            attempts: 1,
            successes: 1,
        }
    }

    fn lantime() -> ReceiverStatus {
        ReceiverStatus {
            name: Some("gps180".to_owned()),
            state: Some(ReceiverState::Synchronized),
            state_detail: Some("gpsSync".to_owned()),
            antenna: Some(AntennaState::Connected),
            holdover: None,
            satellites: SatelliteCounts {
                good: Some(5),
                in_view: Some(9),
                ..SatelliteCounts::default()
            },
            constellations: Vec::new(),
        }
    }

    fn values(rows: &[InfoRow; 2]) -> [(&str, &str); 2] {
        [
            (rows[0].label, rows[0].value.as_str()),
            (rows[1].label, rows[1].value.as_str()),
        ]
    }

    #[test]
    fn shows_the_source_state_and_counts() {
        let rows = rows(Some(&snapshot(lantime())), PcTime::monotonic(2_000_000_000));
        assert_eq!(
            values(&rows),
            [
                (
                    "GNSS receiver",
                    "Meinberg LTNG (gps180): Synchronized (gpsSync), antenna OK"
                ),
                ("Satellites", "good 5 / in view 9"),
            ]
        );
        assert_eq!(rows[0].color, theme::GOOD);
    }

    #[test]
    fn shows_constellations_and_holdover() {
        let status = ReceiverStatus {
            name: Some("SyncServer S650".to_owned()),
            state: Some(ReceiverState::Synchronized),
            state_detail: Some("Tracking".to_owned()),
            antenna: None,
            holdover: Some(true),
            satellites: SatelliteCounts {
                tracking: Some(3),
                ..SatelliteCounts::default()
            },
            constellations: vec![
                ConstellationCount {
                    constellation: Constellation::Gps,
                    count: 2,
                },
                ConstellationCount {
                    constellation: Constellation::Galileo,
                    count: 1,
                },
            ],
        };
        let mut snap = snapshot(status);
        snap.profile = Some(MibProfile::MicrochipS650);
        let rows = rows(Some(&snap), PcTime::monotonic(2_000_000_000));
        assert_eq!(
            values(&rows),
            [
                (
                    "GNSS receiver",
                    "Microchip S650 (SyncServer S650): Synchronized (Tracking), holdover"
                ),
                ("Satellites", "tracking 3 (GPS 2, Galileo 1)"),
            ]
        );
        assert_eq!(rows[0].color, theme::WARN);
    }

    #[test]
    fn missing_counts_are_not_zero() {
        let status = ReceiverStatus {
            state: Some(ReceiverState::NotSynchronized),
            antenna: Some(AntennaState::Disconnected),
            ..ReceiverStatus::default()
        };
        let rows = rows(Some(&snapshot(status)), PcTime::monotonic(0));
        assert_eq!(
            values(&rows),
            [
                (
                    "GNSS receiver",
                    "Meinberg LTNG: Not synchronized, antenna disconnected"
                ),
                ("Satellites", "Unavailable"),
            ]
        );
        assert_eq!(rows[0].color, theme::BAD);
    }

    /// 失敗したら古い衛星の数を出さず、エラーと最後に取れてからの時間を出す。
    #[test]
    fn a_failure_hides_the_old_counts() {
        let mut snap = snapshot(lantime());
        snap.last_error = Some(MonitorError::Timeout);
        let rows = rows(Some(&snap), PcTime::monotonic(13_500_000_000));
        assert_eq!(
            rows[0].value,
            "Meinberg LTNG: Query failed (last success 12.5 s ago)"
        );
        assert_eq!(rows[1].label, "SNMP error");
        assert_eq!(
            rows[1].value,
            "No SNMP response (check community, port and SNMP settings of the device)"
        );
        assert!(!rows.iter().any(|row| row.value.contains("good")));

        snap.last_success = None;
        snap.profile = None;
        let never = super::rows(Some(&snap), PcTime::monotonic(0));
        assert_eq!(never[0].value, "Query failed (never succeeded)");
    }

    #[test]
    fn waits_before_the_first_answer() {
        let rows = rows(None, PcTime::monotonic(0));
        assert_eq!(
            values(&rows),
            [("GNSS receiver", "Querying"), ("Satellites", "--")]
        );
    }
}
