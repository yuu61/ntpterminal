//! ptp4l が合わせる PHC の `once`・`stats` の出力。
//!
//! - `once`: ptp4l の状態と PHC を 1 回読んだ結果。PHC の時刻は、ptp4l が同期を知らせていなければ
//!   「同期していない」と添える（合わせられていない PHC は任意の時刻で進んでいる）
//! - `stats`: 1 回ごとの結果と、最後の集計（マスタとの差・経路の遅延・PC との差）

use core::fmt;
use std::io::Write;

use application::port::PhcEventSink;
use application::snapshot::PhcSnapshot;
use domain::ptp_clock::{PhcEvent, PtpStatus, Timescale};
use domain::time_sync::FrequencyOffset;
use serde_json::json;

use super::render_rows;
use super::stats::{RunningStats, describe, frequency_text, stats_json};
use crate::format::ptp::{
    clock_class_label, format_nanos, format_nanos_signed, phc_state_key, phc_state_label,
    port_label, ptp_leap_key, ptp_leap_label, time_source_key, time_source_label, utc_offset_label,
};
use crate::format::{DisplayZone, format_date_time, format_offset_ms, format_rfc3339_utc};

/// 表示する PHC の説明（`once`・`stats` の見出し）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhcTarget {
    /// ptp4l の管理用ソケットの場所
    pub socket: String,
    pub domain: u8,
    /// PHC の説明（"/dev/ptp0（同時読み取り）"）
    pub phc: String,
    /// PHC を使うポートの番号とインターフェース名
    pub ports: Vec<(u16, String)>,
}

impl PhcTarget {
    /// "/var/run/ptp4l（ドメイン 0）"
    #[must_use]
    pub fn ptp4l_label(&self) -> String {
        format!("{}（ドメイン {}）", self.socket, self.domain)
    }
}

/// `once` のテキスト出力。
#[must_use]
pub fn render_once_text(target: &PhcTarget, snapshot: &PhcSnapshot, zone: &DisplayZone) -> String {
    let status = snapshot.status.as_ref();
    let text = |value: Option<String>| value.unwrap_or_else(|| "--".to_owned());
    let mut rows = vec![
        ("ptp4l", target.ptp4l_label()),
        ("PHC", target.phc.clone()),
        (
            "状態",
            phc_state_label(snapshot.state, status, snapshot.status_error.as_deref()),
        ),
        (
            "ポート",
            text(status.map(|ptp| port_label(ptp, &target.ports))),
        ),
        (
            "マスタとの差",
            text(status.map(|ptp| format_nanos_signed(ptp.offset_from_master))),
        ),
        (
            "経路遅延",
            text(status.map(|ptp| format_nanos(ptp.mean_path_delay))),
        ),
        (
            "グランドマスタ",
            text(status.map(|ptp| ptp.grandmaster.to_string())),
        ),
        (
            "Clock class",
            text(status.map(|ptp| clock_class_label(ptp.grandmaster_quality.class))),
        ),
        (
            "時刻源",
            text(status.map(|ptp| time_source_label(ptp.time.time_source))),
        ),
        (
            "UTC との差",
            text(status.map(|ptp| utc_offset_label(&ptp.time).0)),
        ),
        (
            "うるう秒",
            text(status.map(|ptp| ptp_leap_label(ptp.time.leap).to_owned())),
        ),
    ];
    if let Some(sample) = snapshot.last_sample {
        rows.push((
            "PC との差",
            format!("{}（PHC − PC）", format_offset_ms(sample.offset().delta())),
        ));
        let note = if snapshot.state.shows_time() {
            ""
        } else {
            "（ptp4l が同期していないので、正しい時刻とは限らない）"
        };
        rows.push((
            "PHC の時刻",
            format!("{}{note}", format_date_time(sample.server_transmit(), zone)),
        ));
    }
    render_rows(&rows)
}

/// `once` の JSON（1 行）。
#[must_use]
pub fn render_once_json(target: &PhcTarget, snapshot: &PhcSnapshot) -> String {
    json!({
        "ok": true,
        "ptp4l": target.socket,
        "domain": target.domain,
        "phc": target.phc,
        "state": phc_state_key(snapshot.state),
        "synchronized": snapshot.state.shows_time(),
        "ptp4l_status": snapshot.status.as_ref().map(|ptp| status_json(ptp, &target.ports)),
        "pc_offset_ms": snapshot.last_sample.map(|sample| sample.offset().delta().as_millis_f64()),
        "phc_time_utc": snapshot.last_sample.map(|sample| format_rfc3339_utc(sample.server_transmit())),
    })
    .to_string()
}

/// `once` の失敗時の JSON（1 行）。
#[must_use]
pub fn render_once_error_json(target: &PhcTarget, error: &str) -> String {
    json!({
        "ok": false,
        "ptp4l": target.socket,
        "phc": target.phc,
        "error": error,
    })
    .to_string()
}

fn status_json(status: &PtpStatus, ports: &[(u16, String)]) -> serde_json::Value {
    json!({
        "ports": status.ports.iter().map(|port| json!({
            "number": port.number,
            "interface": ports.iter().find(|pair| pair.0 == port.number).map(|pair| pair.1.clone()),
            "state": port.state.to_string(),
        })).collect::<Vec<_>>(),
        "offset_from_master_ns": status.offset_from_master.as_nanos(),
        "mean_path_delay_ns": status.mean_path_delay.as_nanos(),
        "steps_removed": status.steps_removed,
        "grandmaster": status.grandmaster.to_string(),
        "clock_class": status.grandmaster_quality.class,
        "time_source": time_source_key(status.time.time_source),
        "utc_offset": status.time.utc_offset,
        "utc_offset_valid": status.time.utc_offset_valid,
        "ptp_timescale": status.time.timescale == Timescale::Ptp,
        "leap": ptp_leap_key(status.time.leap),
    })
}

/// `stats` の出力: 1 回ごとの結果を 1 行ずつ出し、最後に集計を出す。
pub struct PhcStatsReporter<W: Write> {
    out: W,
    target: PhcTarget,
    json: bool,
    master_offset_ns: RunningStats,
    path_delay_ns: RunningStats,
    pc_offset_ms: RunningStats,
    last: Option<PhcSnapshot>,
    summary_every: Option<u64>,
}

impl<W: Write> fmt::Debug for PhcStatsReporter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhcStatsReporter")
            .field("target", &self.target)
            .field("json", &self.json)
            .field("summary_every", &self.summary_every)
            .finish_non_exhaustive()
    }
}

impl<W: Write> PhcStatsReporter<W> {
    pub fn new(out: W, target: PhcTarget, json: bool) -> Self {
        Self {
            out,
            target,
            json,
            master_offset_ns: RunningStats::default(),
            path_delay_ns: RunningStats::default(),
            pc_offset_ms: RunningStats::default(),
            last: None,
            summary_every: None,
        }
    }

    /// `every` 回の問い合わせごとに途中集計を出す（回数無制限で動かすとき用）。
    #[must_use]
    pub fn with_periodic_summary(mut self, every: u64) -> Self {
        self.summary_every = Some(every.max(1));
        self
    }

    /// 開始時の見出し（テキスト形式のときだけ）。
    pub fn write_header(&mut self, interval_ms: u128) {
        if !self.json {
            drop(writeln!(
                self.out,
                "ptp4l {} と PHC {} を {interval_ms} ms 間隔で読みます",
                self.target.ptp4l_label(),
                self.target.phc
            ));
        }
    }

    pub fn write_summary(&mut self) {
        let text = if self.json {
            self.summary_json()
        } else {
            self.summary_text()
        };
        drop(writeln!(self.out, "{text}"));
        drop(self.out.flush());
    }

    fn summary_text(&self) -> String {
        let mut text = format!("--- {} の統計 ---\n", self.target.phc);
        let Some(snapshot) = self.last.as_ref() else {
            text.push_str("読んでいません\n");
            return text;
        };
        let stats = snapshot.stats;
        let mut rows = vec![
            (
                "成功/試行",
                format!(
                    "{}/{}  PHC の読み取りの失敗 {}  ptp4l の応答なし・エラー {}",
                    stats.readings, stats.attempts, stats.read_failures, stats.status_failures
                ),
            ),
            (
                "状態",
                phc_state_label(
                    snapshot.state,
                    snapshot.status.as_ref(),
                    snapshot.status_error.as_deref(),
                ),
            ),
        ];
        if self.master_offset_ns.count() > 0 {
            rows.push(("マスタとの差 [ns]", describe(&self.master_offset_ns, true)));
            rows.push(("経路遅延 [ns]", describe(&self.path_delay_ns, false)));
        }
        if self.pc_offset_ms.count() > 0 {
            rows.push(("PC との差 [ms]", describe(&self.pc_offset_ms, true)));
            rows.push((
                "周波数のずれ",
                frequency_text(
                    snapshot.frequency,
                    "PC のモノトニック時計に対する PHC の進み",
                ),
            ));
        } else {
            rows.push(("結果", "PHC を読めませんでした".to_owned()));
        }
        text.push_str(&render_rows(&rows));
        text
    }

    fn summary_json(&self) -> String {
        let stats = self
            .last
            .as_ref()
            .map(|snapshot| snapshot.stats)
            .unwrap_or_default();
        json!({
            "event": "summary",
            "ptp4l": self.target.socket,
            "phc": self.target.phc,
            "attempts": stats.attempts,
            "readings": stats.readings,
            "read_failures": stats.read_failures,
            "status_updates": stats.status_updates,
            "status_failures": stats.status_failures,
            "state": self.last.as_ref().map(|snapshot| phc_state_key(snapshot.state)),
            "offset_from_master_ns": stats_json(&self.master_offset_ns),
            "mean_path_delay_ns": stats_json(&self.path_delay_ns),
            "pc_offset_ms": stats_json(&self.pc_offset_ms),
            "frequency_ppm": self
                .last
                .as_ref()
                .and_then(|snapshot| snapshot.frequency)
                .map(FrequencyOffset::as_ppm_f64),
        })
        .to_string()
    }

    #[expect(
        clippy::pattern_type_mismatch,
        reason = "失敗の detail（String）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
    )]
    fn line_text(&self, event: &PhcEvent, snapshot: &PhcSnapshot) -> Option<String> {
        let line = match event {
            PhcEvent::StatusReceived { .. } => return None,
            PhcEvent::ReadingAccepted { seq, sample } => {
                let ptp = snapshot.status.as_ref().map_or_else(String::new, |ptp| {
                    format!(
                        "  マスタとの差 {:>10}  経路遅延 {:>9}  {}",
                        format_nanos_signed(ptp.offset_from_master),
                        format_nanos(ptp.mean_path_delay),
                        port_label(ptp, &self.target.ports)
                    )
                });
                format!(
                    "#{seq:<5} PC との差 {:>11}{ptp}",
                    format_offset_ms(sample.offset().delta())
                )
            }
            PhcEvent::StatusFailed { seq, detail } => format!("#{seq:<5} ptp4l: {detail}"),
            PhcEvent::ReadingFailed { seq, detail } => format!("#{seq:<5} PHC: {detail}"),
            PhcEvent::ClockStepDetected { step } => format!(
                "       時刻の段差を検出: {}（それまでの測定を捨てて合わせ直した）",
                format_offset_ms(*step)
            ),
            // 変わる前の状態は残っていないので、ポートの状態を添えない
            PhcEvent::StateChanged { from, to } => format!(
                "       状態: {} → {}",
                phc_state_label(*from, None, None),
                phc_state_label(
                    *to,
                    snapshot.status.as_ref(),
                    snapshot.status_error.as_deref()
                )
            ),
        };
        Some(line)
    }

    #[expect(
        clippy::pattern_type_mismatch,
        reason = "失敗の detail（String）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
    )]
    fn line_json(&self, event: &PhcEvent, snapshot: &PhcSnapshot) -> Option<String> {
        let value = match event {
            PhcEvent::StatusReceived { .. } => return None,
            PhcEvent::ReadingAccepted { seq, sample } => json!({
                "event": "sample",
                "seq": seq,
                "pc_offset_ms": sample.offset().delta().as_millis_f64(),
                "read_width_ns": sample.delay().delta().as_nanos(),
                "state": phc_state_key(snapshot.state),
                "ptp4l_status": snapshot.status.as_ref().map(|ptp| status_json(ptp, &self.target.ports)),
            }),
            PhcEvent::StatusFailed { seq, detail } => {
                json!({ "event": "ptp4l_failed", "seq": seq, "detail": detail })
            }
            PhcEvent::ReadingFailed { seq, detail } => {
                json!({ "event": "phc_failed", "seq": seq, "detail": detail })
            }
            PhcEvent::ClockStepDetected { step } => json!({
                "event": "step_detected",
                "step_ms": step.as_millis_f64(),
            }),
            PhcEvent::StateChanged { from, to } => json!({
                "event": "state_changed",
                "from": phc_state_key(*from),
                "to": phc_state_key(*to),
            }),
        };
        Some(value.to_string())
    }
}

impl<W: Write> PhcEventSink for PhcStatsReporter<W> {
    fn publish(&mut self, event: &PhcEvent, snapshot: &PhcSnapshot) {
        if let PhcEvent::ReadingAccepted { sample, .. } = *event {
            self.pc_offset_ms
                .push(sample.offset().delta().as_millis_f64());
            if let Some(ptp) = snapshot.status.as_ref()
                && snapshot.status_error.is_none()
            {
                self.master_offset_ns
                    .push(ptp.offset_from_master.as_nanos() as f64);
                self.path_delay_ns
                    .push(ptp.mean_path_delay.as_nanos() as f64);
            }
        }
        self.last = Some(snapshot.clone());
        let rendered = if self.json {
            self.line_json(event, snapshot)
        } else {
            self.line_text(event, snapshot)
        };
        if let Some(line) = rendered {
            drop(writeln!(self.out, "{line}"));
        }
        if let (Some(every), Some(seq)) = (self.summary_every, finished_seq(event))
            && seq.is_multiple_of(every)
        {
            self.write_summary();
        }
    }
}

/// 1 回の問い合わせの最後のイベント（PHC の読み取りの成否）なら、その番号。
const fn finished_seq(event: &PhcEvent) -> Option<u64> {
    match *event {
        PhcEvent::ReadingAccepted { seq, .. } | PhcEvent::ReadingFailed { seq, .. } => Some(seq),
        PhcEvent::StatusReceived { .. }
        | PhcEvent::StatusFailed { .. }
        | PhcEvent::ClockStepDetected { .. }
        | PhcEvent::StateChanged { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ptp_clock::{
        ClockIdentity, ClockQuality, LeapWarning, PhcState, PhcStats, PortState, PortStatus,
        TimeProperties, TimeSourceKind,
    };
    use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};
    use domain::time_sync::Sample;

    fn target() -> PhcTarget {
        PhcTarget {
            socket: "/var/run/ptp4l".to_owned(),
            domain: 0,
            phc: "/dev/ptp0（同時読み取り）".to_owned(),
            ports: vec![(1, "eno1".to_owned())],
        }
    }

    /// 2026-11-17 00:00:00 UTC に読んだ、PC より 0.3 ms 進んだ PHC。
    fn snapshot(state: PhcState) -> PhcSnapshot {
        let phc = NtpTimestamp::from_unix_nanos(1_794_873_600 * 1_000_000_000);
        let pc = LocalStamp {
            mono: PcTime::monotonic(1_000),
            wall: phc.wrapping_add(TimeDelta::from_micros(-300)),
        };
        let sample = Sample::new(pc, phc, phc, pc).unwrap();
        PhcSnapshot {
            state,
            stats: PhcStats {
                attempts: 1,
                readings: 1,
                status_updates: 1,
                ..PhcStats::default()
            },
            poll_interval: TimeDelta::from_millis(500),
            anchor: None,
            last_sample: Some(sample),
            selected_sample: Some(sample),
            frequency: None,
            status: Some(PtpStatus {
                domain: 0,
                ports: vec![PortStatus {
                    number: 1,
                    state: PortState::Slave,
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
                    utc_offset_valid: false,
                    timescale: Timescale::Ptp,
                    leap: LeapWarning::Insert,
                    time_traceable: true,
                    frequency_traceable: true,
                    time_source: TimeSourceKind::Gnss,
                },
            }),
            status_error: None,
            last_success: Some(pc.mono),
            next_poll: PcTime::monotonic(500_001_000),
        }
    }

    #[test]
    fn once_shows_ptp4l_and_the_phc() {
        let text = render_once_text(
            &target(),
            &snapshot(PhcState::Synchronized),
            &DisplayZone::utc(),
        );
        for expected in [
            "ptp4l           /var/run/ptp4l（ドメイン 0）",
            "状態            同期済み（SLAVE）",
            "マスタとの差    -12 ns",
            "UTC との差      37 秒（未確定）",
            "うるう秒        うるう秒の挿入予告",
            "PC との差       +0.300 ms（PHC − PC）",
            "PHC の時刻      2026-11-17 (火) 00:00:00.000 UTC\n",
        ] {
            assert!(text.contains(expected), "{expected:?} がない:\n{text}");
        }
        let acquiring = render_once_text(
            &target(),
            &snapshot(PhcState::Acquiring),
            &DisplayZone::utc(),
        );
        assert!(acquiring.contains("正しい時刻とは限らない"), "{acquiring}");

        let json: serde_json::Value = serde_json::from_str(&render_once_json(
            &target(),
            &snapshot(PhcState::Synchronized),
        ))
        .unwrap();
        assert_eq!(json["state"], "synchronized");
        assert_eq!(json["ptp4l_status"]["offset_from_master_ns"], -12i64);
        assert_eq!(json["ptp4l_status"]["ports"][0]["interface"], "eno1");
        assert_eq!(json["ptp4l_status"]["utc_offset_valid"], false);
        assert_eq!(json["phc_time_utc"], "2026-11-17T00:00:00.000000000Z");
    }

    #[test]
    fn stats_prints_each_reading_and_a_summary() {
        let mut out = Vec::new();
        let mut reporter = PhcStatsReporter::new(&mut out, target(), false);
        let synced = snapshot(PhcState::Synchronized);
        let sample = synced.last_sample.unwrap();
        reporter.publish(&PhcEvent::StatusReceived { seq: 1 }, &synced);
        reporter.publish(&PhcEvent::ReadingAccepted { seq: 1, sample }, &synced);
        let mut lost = snapshot(PhcState::Holdover);
        lost.status_error = Some("応答なし".to_owned());
        reporter.publish(
            &PhcEvent::StatusFailed {
                seq: 2,
                detail: "応答なし".to_owned(),
            },
            &lost,
        );
        reporter.publish(
            &PhcEvent::StateChanged {
                from: PhcState::Synchronized,
                to: PhcState::Holdover,
            },
            &lost,
        );
        reporter.write_summary();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("#1     PC との差   +0.300 ms  マスタとの差     -12 ns"),
            "{text}"
        );
        assert!(text.contains("#2     ptp4l: 応答なし"), "{text}");
        assert!(
            text.contains("       状態: 同期済み → 自走中（ptp4l の応答なし）\n"),
            "{text}"
        );
        assert!(text.contains("マスタとの差 [ns]"), "{text}");
        assert!(text.contains("min -12.000"), "{text}");
    }
}
