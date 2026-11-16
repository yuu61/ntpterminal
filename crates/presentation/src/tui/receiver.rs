//! 受信機の情報欄（`tui --snmp-community` のときだけ出す 2 行）。
//!
//! - 1 行目「GNSS 受信機」: MIB・受信機の名前・状態（アンテナ・ホールドオーバー）。直近の問い合わせが失敗したら、
//!   そのことと、最後に取れてからの経過時間を出す
//! - 2 行目「衛星」: 衛星の数を意味ごとに（良好・可視・見えるはず・追尾）。メーカーが数えていない意味の数は出さない。
//!   直近の問い合わせが失敗したら、代わりに「SNMP エラー」としてエラーの内容を出す（古い数を今の値のように出さない）
//! - 行を 2 つに抑えるのは、80×24 の端末でも大きい時計を縮めないため

use ratatui::style::Color;

use application::acl::mib::MibProfile;
use application::snapshot::ReceiverSnapshot;
use domain::receiver::{
    AntennaState, Constellation, ConstellationCount, ReceiverState, ReceiverStatus, SatelliteCounts,
};
use domain::shared_kernel::{PcTime, TimeDelta};

use super::screen::InfoRow;
use super::theme;

const LABEL_RECEIVER: &str = "GNSS 受信機";
const LABEL_SATELLITES: &str = "衛星";
const LABEL_ERROR: &str = "SNMP エラー";

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
                || "未取得".to_owned(),
                |elapsed| {
                    format!(
                        "最終取得 {:.1} 秒前",
                        elapsed.max(TimeDelta::ZERO).as_secs_f64()
                    )
                },
            );
        return [
            InfoRow {
                label: LABEL_RECEIVER,
                value: with_source(snap.profile, None, &format!("取得失敗（{since}）")),
                color: theme::BAD,
            },
            InfoRow {
                label: LABEL_ERROR,
                value: error.to_string(),
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
            value: "問い合わせ中".to_owned(),
            color: theme::LABEL,
        },
        InfoRow {
            label: LABEL_SATELLITES,
            value: "--".to_owned(),
            color: theme::TEXT,
        },
    ]
}

/// "Meinberg LTNG（gps180）: 同期（gpsSync）" のように、どこから読んだかを前に付ける。
fn with_source(profile: Option<MibProfile>, name: Option<&str>, text: &str) -> String {
    let source = match (profile.map(mib_label), name) {
        (Some(mib), Some(receiver)) => format!("{mib}（{receiver}）: "),
        (Some(mib), None) => format!("{mib}: "),
        (None, Some(receiver)) => format!("{receiver}: "),
        (None, None) => String::new(),
    };
    format!("{source}{text}")
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

/// "同期（gpsSync）・アンテナ正常・ホールドオーバー中"
fn state_text(status: &ReceiverStatus) -> String {
    let state = match status.state {
        Some(ReceiverState::Synchronized) => "同期",
        Some(ReceiverState::Acquiring) => "捕捉中",
        Some(ReceiverState::NotSynchronized) => "未同期",
        None => "状態不明",
    };
    let mut parts = vec![
        status
            .state_detail
            .as_deref()
            .map_or_else(|| state.to_owned(), |detail| format!("{state}（{detail}）")),
    ];
    if let Some(antenna) = status.antenna {
        parts.push(
            match antenna {
                AntennaState::Connected => "アンテナ正常",
                AntennaState::Disconnected => "アンテナ断線",
                AntennaState::ShortCircuit => "アンテナ短絡",
                AntennaState::Faulty => "アンテナ異常",
            }
            .to_owned(),
        );
    }
    if status.holdover == Some(true) {
        parts.push("ホールドオーバー中".to_owned());
    }
    parts.join("・")
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

/// "良好 5 / 可視 9"、"追尾 3（GPS 2・Galileo 1）"。どの数も取れなければ "取得不可"。
fn satellites_text(counts: &SatelliteCounts, constellations: &[ConstellationCount]) -> String {
    let parts: Vec<String> = [
        ("良好", counts.good),
        ("可視", counts.in_view),
        ("見えるはず", counts.expected),
        ("追尾", counts.tracking),
    ]
    .into_iter()
    .filter_map(|(label, count)| count.map(|number| format!("{label} {number}")))
    .collect();
    if parts.is_empty() {
        return "取得不可".to_owned();
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
        text = format!("{text}（{}）", breakdown.join("・"));
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
    use application::usecase::MonitorError;

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
                    "GNSS 受信機",
                    "Meinberg LTNG（gps180）: 同期（gpsSync）・アンテナ正常"
                ),
                ("衛星", "良好 5 / 可視 9"),
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
                    "GNSS 受信機",
                    "Microchip S650（SyncServer S650）: 同期（Tracking）・ホールドオーバー中"
                ),
                ("衛星", "追尾 3（GPS 2・Galileo 1）"),
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
                ("GNSS 受信機", "Meinberg LTNG: 未同期・アンテナ断線"),
                ("衛星", "取得不可"),
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
            "Meinberg LTNG: 取得失敗（最終取得 12.5 秒前）"
        );
        assert_eq!(rows[1].label, "SNMP エラー");
        assert_eq!(rows[1].value, MonitorError::Timeout.to_string());
        assert!(!rows.iter().any(|row| row.value.contains("良好")));

        snap.last_success = None;
        snap.profile = None;
        let never = super::rows(Some(&snap), PcTime::monotonic(0));
        assert_eq!(never[0].value, "取得失敗（未取得）");
    }

    #[test]
    fn waits_before_the_first_answer() {
        let rows = rows(None, PcTime::monotonic(0));
        assert_eq!(
            values(&rows),
            [("GNSS 受信機", "問い合わせ中"), ("衛星", "--")]
        );
    }
}
