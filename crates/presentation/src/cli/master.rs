//! PTP のスレーブとしてマスタに問い合わせるときの `once`・`stats` の出力。
//!
//! - `once`: 最初の交換で測った結果（マスタの時刻・PC との差・経路の遅延）と、マスタ・グランドマスタの情報
//! - `stats`: 交換ごとの結果を 1 行ずつ出し、最後に集計（PC との差・経路の遅延）を出す

use alloc::sync::Arc;
use core::fmt;
use std::io::Write;

use application::port::MasterEventSink;
use application::snapshot::MasterSnapshot;
use application::timestamping::TimestampingStatus;
use domain::ptp_clock::{MasterEvent, MasterStatus, Timescale};
use domain::shared_kernel::TimeDelta;
use domain::time_sync::FrequencyOffset;
use serde_json::json;

use super::render_rows;
use super::stats::{RunningStats, describe, frequency_text, stats_json};
use crate::format::ptp::{
    clock_class_label, format_nanos, intervals_label, master_label, master_state_key,
    master_state_label, ptp_leap_key, ptp_leap_label, time_source_key, time_source_label,
    utc_offset_label,
};
use crate::format::{
    DisplayZone, format_date_time, format_offset_ms, format_rfc3339_utc, timestamping_breakdown,
};

/// 問い合わせ先の説明（`once`・`stats` の見出し）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasterTarget {
    pub domain: u8,
    /// 送受信の説明（"nic0 192.168.10.200・UDP 319/320・マルチキャスト"）
    pub transport: String,
}

impl MasterTarget {
    /// "PTP のマスタ（ドメイン 0）"
    #[must_use]
    pub fn label(&self) -> String {
        format!("PTP のマスタ（ドメイン {}）", self.domain)
    }
}

/// `once` のテキスト出力。`timestamping` は実際に取れたタイムスタンプの方式（"カーネル" など）。
#[must_use]
pub fn render_once_text(
    target: &MasterTarget,
    snapshot: &MasterSnapshot,
    timestamping: &str,
    zone: &DisplayZone,
) -> String {
    let status = snapshot.status;
    let text = |value: Option<String>| value.unwrap_or_else(|| "--".to_owned());
    let mut rows = vec![
        ("問い合わせ先", target.label()),
        ("送受信", target.transport.clone()),
        ("タイムスタンプ", timestamping.to_owned()),
        ("状態", master_state_label(snapshot)),
        (
            "マスタ",
            text(status.map(|master| master_label(&master, snapshot.master_address))),
        ),
        (
            "グランドマスタ",
            text(status.map(|master| master.grandmaster.to_string())),
        ),
        (
            "Clock class",
            text(status.map(|master| clock_class_label(master.grandmaster_quality.class))),
        ),
        (
            "時刻源",
            text(status.map(|master| time_source_label(master.time.time_source))),
        ),
        (
            "UTC との差",
            text(status.map(|master| utc_offset_label(&master.time).0)),
        ),
        (
            "うるう秒",
            text(status.map(|master| ptp_leap_label(master.time.leap).to_owned())),
        ),
        ("間隔", intervals_label(snapshot)),
    ];
    if let Some(sample) = snapshot.last_sample {
        rows.push((
            "PC との差",
            format!(
                "{}（マスタ − PC）",
                format_offset_ms(sample.offset().delta())
            ),
        ));
        rows.push(("経路遅延", format_nanos(sample.delay().delta() / 2)));
        rows.push((
            "マスタの時刻",
            format_date_time(sample.server_transmit(), zone),
        ));
    }
    render_rows(&rows)
}

/// `once` の JSON（1 行）。
#[must_use]
pub fn render_once_json(
    target: &MasterTarget,
    snapshot: &MasterSnapshot,
    timestamping: &str,
) -> String {
    json!({
        "ok": true,
        "domain": target.domain,
        "transport": target.transport,
        "timestamping": timestamping,
        "state": master_state_key(snapshot.state),
        "synchronized": snapshot.state.shows_time(),
        "master": snapshot.status.map(|master| status_json(&master, snapshot)),
        "pc_offset_ms": snapshot.last_sample.map(|sample| sample.offset().delta().as_millis_f64()),
        "mean_path_delay_ns": snapshot.last_sample.map(|sample| (sample.delay().delta() / 2).as_nanos()),
        "master_time_utc": snapshot.last_sample.map(|sample| format_rfc3339_utc(sample.server_transmit())),
    })
    .to_string()
}

/// `once` の失敗時の JSON（1 行）。
#[must_use]
pub fn render_once_error_json(target: &MasterTarget, error: &str) -> String {
    json!({
        "ok": false,
        "domain": target.domain,
        "transport": target.transport,
        "error": error,
    })
    .to_string()
}

fn status_json(status: &MasterStatus, snapshot: &MasterSnapshot) -> serde_json::Value {
    json!({
        "port_identity": status.master.to_string(),
        "address": snapshot.master_address.map(|address| address.ip().to_string()),
        "grandmaster": status.grandmaster.to_string(),
        "priority1": status.priority1,
        "priority2": status.priority2,
        "clock_class": status.grandmaster_quality.class,
        "steps_removed": status.steps_removed,
        "time_source": time_source_key(status.time.time_source),
        "utc_offset": status.time.utc_offset,
        "utc_offset_valid": status.time.utc_offset_valid,
        "ptp_timescale": status.time.timescale == Timescale::Ptp,
        "leap": ptp_leap_key(status.time.leap),
        "two_step": status.two_step,
        "sync_interval_s": status.sync_interval.map(TimeDelta::as_secs_f64),
        "delay_req_interval_s": snapshot.request_interval.as_secs_f64(),
    })
}

/// `stats` の出力: 交換ごとの結果を 1 行ずつ出し、最後に集計を出す。
pub struct MasterStatsReporter<W: Write> {
    out: W,
    target: MasterTarget,
    json: bool,
    pc_offset_ms: RunningStats,
    path_delay_ns: RunningStats,
    last: Option<MasterSnapshot>,
    summary_every: Option<u64>,
    timestamping: Option<Arc<TimestampingStatus>>,
}

impl<W: Write> fmt::Debug for MasterStatsReporter<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterStatsReporter")
            .field("target", &self.target)
            .field("json", &self.json)
            .field("summary_every", &self.summary_every)
            .finish_non_exhaustive()
    }
}

impl<W: Write> MasterStatsReporter<W> {
    pub fn new(out: W, target: MasterTarget, json: bool) -> Self {
        Self {
            out,
            target,
            json,
            pc_offset_ms: RunningStats::default(),
            path_delay_ns: RunningStats::default(),
            last: None,
            summary_every: None,
            timestamping: None,
        }
    }

    /// 集計に、タイムスタンプの取得状況（実際に使えた方式の内訳）を出す。
    #[must_use]
    pub fn with_timestamping(mut self, status: Arc<TimestampingStatus>) -> Self {
        self.timestamping = Some(status);
        self
    }

    /// `every` 回の標本ごとに途中集計を出す（回数無制限で動かすとき用）。
    #[must_use]
    pub fn with_periodic_summary(mut self, every: u64) -> Self {
        self.summary_every = Some(every.max(1));
        self
    }

    /// 開始時の見出し（テキスト形式のときだけ）。
    pub fn write_header(&mut self) {
        if !self.json {
            drop(writeln!(
                self.out,
                "{} に問い合わせます（{}）",
                self.target.label(),
                self.target.transport
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
        let mut text = format!("--- {} の統計 ---\n", self.target.label());
        let Some(snapshot) = self.last.as_ref() else {
            text.push_str("マスタからのメッセージを受け取っていません\n");
            return text;
        };
        let stats = snapshot.stats;
        let mut rows = vec![
            (
                "成功/試行",
                format!(
                    "{}/{}  失敗 {}  Sync {}  Announce {}",
                    stats.samples, stats.requests, stats.failures, stats.syncs, stats.announces
                ),
            ),
            ("状態", master_state_label(snapshot)),
        ];
        if let Some(status) = self.timestamping.as_ref() {
            rows.push(("タイムスタンプ", timestamping_breakdown(&status.counts())));
        }
        if self.pc_offset_ms.count() > 0 {
            rows.push(("PC との差 [ms]", describe(&self.pc_offset_ms, true)));
            // ユーザ空間の時刻なら数百 µs、カーネルの時刻なら数百 ns なので、どちらも読める µs で出す
            rows.push((
                "経路遅延 [µs]",
                describe(&self.path_delay_ns.scaled(1e-3), false),
            ));
            rows.push((
                "周波数のずれ",
                frequency_text(
                    snapshot.frequency,
                    "PC のモノトニック時計に対するマスタの時計の進み",
                ),
            ));
        } else {
            rows.push(("結果", "マスタの時刻を測れませんでした".to_owned()));
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
            "domain": self.target.domain,
            "transport": self.target.transport,
            "announces": stats.announces,
            "syncs": stats.syncs,
            "requests": stats.requests,
            "samples": stats.samples,
            "failures": stats.failures,
            "state": self.last.as_ref().map(|snapshot| master_state_key(snapshot.state)),
            "pc_offset_ms": stats_json(&self.pc_offset_ms),
            "mean_path_delay_ns": stats_json(&self.path_delay_ns),
            "timestamping": self.timestamping.as_ref().map(|status| {
                let counts = status.counts();
                json!({
                    "kernel_enabled": counts.kernel_enabled,
                    "kernel_samples": counts.kernel_samples,
                    "user_samples": counts.user_samples,
                    "tx_misses": counts.tx_misses,
                })
            }),
            "frequency_ppm": self
                .last
                .as_ref()
                .and_then(|snapshot| snapshot.frequency)
                .map(FrequencyOffset::as_ppm_f64),
        })
        .to_string()
    }

    /// テキストの 1 行。出さないイベントは `None`。
    fn line_text(snapshot: &MasterSnapshot, event: MasterEvent) -> Option<String> {
        let line = match event {
            MasterEvent::RequestSent { .. } => return None,
            MasterEvent::MasterSelected {
                master,
                grandmaster,
            } => format!("       マスタ: {master}（グランドマスタ {grandmaster}）"),
            MasterEvent::MasterLost { master } => {
                format!("       マスタを見失いました: {master}")
            }
            MasterEvent::SampleAccepted { seq, sample, .. } => format!(
                "#{seq:<5} PC との差 {:>11}  経路遅延 {:>9}",
                format_offset_ms(sample.offset().delta()),
                format_nanos(sample.delay().delta() / 2)
            ),
            MasterEvent::ExchangeFailed { seq, detail } => format!("#{seq:<5} {detail}"),
            MasterEvent::ClockStepDetected { step } => format!(
                "       時刻の段差を検出: {}（それまでの測定を捨てて合わせ直した）",
                format_offset_ms(step)
            ),
            MasterEvent::StateChanged { .. } => {
                format!("       状態: {}", master_state_label(snapshot))
            }
        };
        Some(line)
    }

    /// JSON の 1 行。出さないイベントは `None`。
    fn line_json(snapshot: &MasterSnapshot, event: MasterEvent) -> Option<String> {
        let value = match event {
            MasterEvent::RequestSent { .. } => return None,
            MasterEvent::MasterSelected {
                master,
                grandmaster,
            } => json!({
                "event": "master_selected",
                "master": master.to_string(),
                "grandmaster": grandmaster.to_string(),
            }),
            MasterEvent::MasterLost { master } => {
                json!({ "event": "master_lost", "master": master.to_string() })
            }
            MasterEvent::SampleAccepted {
                seq,
                sample,
                precise,
            } => json!({
                "event": "sample",
                "seq": seq,
                "pc_offset_ms": sample.offset().delta().as_millis_f64(),
                "mean_path_delay_ns": (sample.delay().delta() / 2).as_nanos(),
                "precise_timestamps": precise,
                "state": master_state_key(snapshot.state),
            }),
            MasterEvent::ExchangeFailed { seq, detail } => {
                json!({ "event": "exchange_failed", "seq": seq, "detail": detail })
            }
            MasterEvent::ClockStepDetected { step } => json!({
                "event": "step_detected",
                "step_ms": step.as_millis_f64(),
            }),
            MasterEvent::StateChanged { from, to } => json!({
                "event": "state_changed",
                "from": master_state_key(from),
                "to": master_state_key(to),
            }),
        };
        Some(value.to_string())
    }
}

impl<W: Write> MasterEventSink for MasterStatsReporter<W> {
    fn publish(&mut self, event: &MasterEvent, snapshot: &MasterSnapshot) {
        let sample_seq = if let MasterEvent::SampleAccepted { seq, sample, .. } = *event {
            self.pc_offset_ms
                .push(sample.offset().delta().as_millis_f64());
            self.path_delay_ns
                .push((sample.delay().delta() / 2).as_nanos() as f64);
            Some(seq)
        } else {
            None
        };
        self.last = Some(snapshot.clone());
        let rendered = if self.json {
            Self::line_json(snapshot, event.clone())
        } else {
            Self::line_text(snapshot, event.clone())
        };
        if let Some(line) = rendered {
            drop(writeln!(self.out, "{line}"));
        }
        if let Some(every) = self.summary_every
            && sample_seq.is_some()
            && snapshot.stats.samples.is_multiple_of(every)
        {
            self.write_summary();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::SocketAddr;
    use domain::ptp_clock::{
        ClockIdentity, ClockQuality, LeapWarning, MasterState, MasterStats, PortIdentity,
        TimeProperties, TimeSourceKind,
    };
    use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime};
    use domain::time_sync::Sample;

    /// 2026-11-17 00:00:00 UTC の Unix 秒
    const BASE_SECS: i128 = 1_794_873_600;

    fn stamp(micros: i64) -> LocalStamp {
        LocalStamp {
            mono: PcTime::monotonic(u64::try_from(micros).unwrap() * 1_000),
            wall: NtpTimestamp::from_unix_nanos(BASE_SECS * 1_000_000_000)
                .wrapping_add(TimeDelta::from_micros(micros)),
        }
    }

    /// マスタが 2.5ms 進み、往復 20µs（片道 10µs）の交換。
    fn sample() -> Sample {
        let master = |micros: i64| stamp(micros + 2_500).wall;
        Sample::from_ptp_exchange(
            master(1_000_000 - 10),
            stamp(1_000_000),
            stamp(1_000_100),
            master(1_000_110),
        )
        .unwrap()
    }

    fn snapshot() -> MasterSnapshot {
        MasterSnapshot {
            domain: 0,
            state: MasterState::Synchronized,
            stats: MasterStats {
                announces: 3,
                syncs: 4,
                requests: 2,
                samples: 2,
                failures: 0,
                ignored: 0,
            },
            status: Some(MasterStatus {
                master: PortIdentity {
                    clock: ClockIdentity([0x7A, 0x64, 0x36, 0xFF, 0xFE, 0x25, 0xF9, 0xDD]),
                    port: 1,
                },
                grandmaster: ClockIdentity([0x7A, 0x64, 0x36, 0xFF, 0xFE, 0x25, 0xF9, 0xDD]),
                priority1: 128,
                priority2: 128,
                grandmaster_quality: ClockQuality {
                    class: 248,
                    accuracy: 0xFE,
                    variance: 0xFFFF,
                },
                steps_removed: 0,
                time: TimeProperties {
                    utc_offset: 37,
                    utc_offset_valid: false,
                    timescale: Timescale::Arbitrary,
                    leap: LeapWarning::None,
                    time_traceable: false,
                    frequency_traceable: false,
                    time_source: TimeSourceKind::InternalOscillator,
                },
                announce_interval: TimeDelta::from_secs(1),
                sync_interval: Some(TimeDelta::from_millis(500)),
                two_step: Some(true),
            }),
            candidates: 1,
            configured_interval: TimeDelta::from_millis(500),
            request_interval: TimeDelta::from_millis(500),
            master_min_interval: Some(TimeDelta::from_millis(500)),
            anchor: None,
            last_sample: Some(sample()),
            selected_sample: Some(sample()),
            frequency: None,
            last_success: None,
            last_failure: None,
            other_domains: Vec::new(),
            master_address: Some(SocketAddr::from(([10, 99, 0, 1], 319))),
            receive_error: None,
        }
    }

    fn target() -> MasterTarget {
        MasterTarget {
            domain: 0,
            transport: "ptp-vs 10.99.0.2・UDP 319/320・マルチキャスト".to_owned(),
        }
    }

    #[test]
    fn once_shows_the_master_and_the_measurement() {
        let text = render_once_text(&target(), &snapshot(), "カーネル", &DisplayZone::jst());
        for expected in [
            "問い合わせ先    PTP のマスタ（ドメイン 0）",
            "タイムスタンプ  カーネル",
            "状態            同期済み",
            "マスタ          7a6436.fffe.25f9dd-1（10.99.0.1）",
            "Clock class     248（既定）",
            "UTC との差      換算なし（時刻系が ARB）",
            "間隔            Sync 0.5 秒（two-step）・Delay_Req 0.5 秒",
            "PC との差       +2.500 ms（マスタ − PC）",
            "経路遅延        10000 ns",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&render_once_json(&target(), &snapshot(), "カーネル")).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["synchronized"], true);
        assert_eq!(json["timestamping"], "カーネル");
        assert_eq!(json["mean_path_delay_ns"], 10_000i64);
        assert_eq!(json["master"]["address"], "10.99.0.1");
        assert_eq!(json["master"]["ptp_timescale"], false);
        let error: serde_json::Value =
            serde_json::from_str(&render_once_error_json(&target(), "見つかりません")).unwrap();
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"], "見つかりません");
    }

    #[test]
    fn stats_lines_and_summary() {
        let mut out = Vec::new();
        let mut reporter = MasterStatsReporter::new(&mut out, target(), false)
            .with_timestamping(Arc::new(TimestampingStatus::new()));
        let snap = snapshot();
        reporter.publish(&MasterEvent::RequestSent { seq: 0 }, &snap);
        reporter.publish(
            &MasterEvent::SampleAccepted {
                seq: 0,
                sample: sample(),
                precise: true,
            },
            &snap,
        );
        reporter.publish(
            &MasterEvent::ExchangeFailed {
                seq: 1,
                detail: "Delay_Resp が届きません".to_owned(),
            },
            &snap,
        );
        reporter.write_summary();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("#0     PC との差   +2.500 ms  経路遅延  10000 ns"),
            "{text}"
        );
        assert!(text.contains("#1     Delay_Resp が届きません"), "{text}");
        assert!(text.contains("成功/試行       2/2  失敗 0"), "{text}");
        // 集計の経路遅延は µs で出す（ユーザ空間の数百 µs も、カーネルの数百 ns も読めるように）
        assert!(
            text.contains("経路遅延 [µs]   min 10.000  avg 10.000  max 10.000"),
            "{text}"
        );
        assert!(text.contains("タイムスタンプ  "), "{text}");
        assert!(
            !text.contains("#0     （"),
            "送信のイベントは出さない: {text}"
        );
    }
}
