//! `stats` サブコマンドの出力: 1 回ごとの結果と、最後の集計。

use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

use std::sync::Arc;

use application::port::EventSink;
use application::snapshot::SyncSnapshot;
use application::timestamping::TimestampingStatus;
use domain::time_sync::SyncEvent;
use serde_json::json;

use super::render_rows;
use crate::format::{
    format_delay_ms, format_offset_ms, state_key, state_label, timestamping_breakdown,
};

/// 平均・標準偏差を逐次計算する（Welford 法）。
#[derive(Clone, Copy, Debug, Default)]
struct RunningStats {
    count: u64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
}

impl RunningStats {
    fn push(&mut self, value: f64) {
        self.count += 1;
        if self.count == 1 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (value - self.mean);
    }

    /// 標本標準偏差。
    fn stddev(&self) -> f64 {
        if self.count < 2 {
            0.0
        } else {
            (self.m2 / (self.count - 1) as f64).sqrt()
        }
    }
}

/// 同期イベントを 1 行ずつ出力し、最後に集計を出す。
pub struct StatsReporter<W: Write> {
    out: W,
    server: SocketAddr,
    json: bool,
    offset_ms: RunningStats,
    delay_ms: RunningStats,
    last: Option<SyncSnapshot>,
    summary_every: Option<u64>,
    timestamping: Option<Arc<TimestampingStatus>>,
}

impl<W: Write> StatsReporter<W> {
    pub fn new(out: W, server: SocketAddr, json: bool) -> Self {
        Self {
            out,
            server,
            json,
            offset_ms: RunningStats::default(),
            delay_ms: RunningStats::default(),
            last: None,
            summary_every: None,
            timestamping: None,
        }
    }

    /// 集計にタイムスタンプの内訳を含める。
    pub fn with_timestamping(mut self, status: Arc<TimestampingStatus>) -> Self {
        self.timestamping = Some(status);
        self
    }

    /// `every` 回の問い合わせごとに途中集計を出す（回数無制限で動かすとき用）。
    pub fn with_periodic_summary(mut self, every: u64) -> Self {
        self.summary_every = Some(every.max(1));
        self
    }

    /// 開始時の見出し（テキスト形式のときだけ）。
    pub fn write_header(&mut self, interval: Duration, transport: &str) {
        if !self.json {
            let _ = writeln!(
                self.out,
                "{} に {} ms 間隔で問い合わせます（送信方式: {transport}）",
                self.server,
                interval.as_millis()
            );
        }
    }

    pub fn write_summary(&mut self) {
        let text = if self.json {
            self.summary_json()
        } else {
            self.summary_text()
        };
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    fn summary_text(&self) -> String {
        let mut text = format!("--- {} の統計 ---\n", self.server);
        let Some(snapshot) = self.last else {
            text.push_str("問い合わせていません\n");
            return text;
        };
        let stats = snapshot.stats;
        let rate = if stats.attempts == 0 {
            0.0
        } else {
            stats.successes as f64 / stats.attempts as f64 * 100.0
        };
        let mut rows = vec![
            (
                "成功/試行",
                format!(
                    "{}/{} ({rate:.1}%)  タイムアウト {}  通信エラー {}  不正応答 {}  無関係 {}",
                    stats.successes,
                    stats.attempts,
                    stats.timeouts,
                    stats.failures,
                    stats.rejections,
                    stats.strays
                ),
            ),
            ("状態", state_label(snapshot.state).to_string()),
        ];
        if let Some(status) = &self.timestamping {
            rows.push(("タイムスタンプ", timestamping_breakdown(&status.counts())));
        }
        if self.offset_ms.count > 0 {
            rows.push(("offset [ms]", describe(&self.offset_ms, true)));
            rows.push(("delay [ms]", describe(&self.delay_ms, false)));
        } else {
            rows.push(("結果", "有効な応答がありませんでした".to_string()));
        }
        if let Some(selected) = snapshot.selected_sample {
            rows.push((
                "採用値",
                format!(
                    "offset {} / delay {}（直近で遅延が最小の標本）",
                    format_offset_ms(selected.offset().delta()),
                    format_delay_ms(selected.delay().delta())
                ),
            ));
        }
        text.push_str(&render_rows(&rows));
        text
    }

    fn summary_json(&self) -> String {
        let stats = self.last.map(|snapshot| snapshot.stats).unwrap_or_default();
        let selected = self.last.and_then(|snapshot| snapshot.selected_sample);
        json!({
            "event": "summary",
            "server": self.server.to_string(),
            "attempts": stats.attempts,
            "successes": stats.successes,
            "timeouts": stats.timeouts,
            "failures": stats.failures,
            "rejections": stats.rejections,
            "strays": stats.strays,
            "state": self.last.map(|snapshot| state_key(snapshot.state)),
            "offset_ms": stats_json(&self.offset_ms),
            "delay_ms": stats_json(&self.delay_ms),
            "timestamping": self.timestamping.as_ref().map(|status| {
                let counts = status.counts();
                json!({
                    "kernel_enabled": counts.kernel_enabled,
                    "kernel_samples": counts.kernel_samples,
                    "user_samples": counts.user_samples,
                    "tx_misses": counts.tx_misses,
                })
            }),
            "selected": selected.map(|sample| json!({
                "offset_ms": sample.offset().delta().as_millis_f64(),
                "delay_ms": sample.delay().delta().as_millis_f64(),
            })),
        })
        .to_string()
    }

    fn line_text(event: &SyncEvent, snapshot: &SyncSnapshot) -> Option<String> {
        let line = match event {
            SyncEvent::ExchangeStarted { .. } => return None,
            SyncEvent::SampleAccepted {
                seq,
                sample,
                server_info,
            } => format!(
                "#{seq:<5} offset {:>11}  delay {:>9}  stratum {}  ref {}  {}",
                format_offset_ms(sample.offset().delta()),
                format_delay_ms(sample.delay().delta()),
                server_info.stratum.value(),
                server_info.reference_id,
                state_label(snapshot.state)
            ),
            SyncEvent::SampleRejected {
                seq: Some(seq),
                reason,
            } => format!("#{seq:<5} 不採用: {reason}"),
            SyncEvent::SampleRejected { seq: None, reason } => format!("       無視: {reason}"),
            SyncEvent::ExchangeTimedOut { seq } => format!("#{seq:<5} タイムアウト（応答なし）"),
            SyncEvent::ExchangeFailed {
                seq: Some(seq),
                detail,
            } => format!("#{seq:<5} {detail}"),
            SyncEvent::ExchangeFailed { seq: None, detail } => format!("       {detail}"),
            SyncEvent::PollIntervalChanged { from, to } => format!(
                "       ポーリング間隔を変更: {} ms → {} ms",
                from.as_millis_f64(),
                to.as_millis_f64()
            ),
            SyncEvent::StateChanged { from, to } => {
                format!("       状態: {} → {}", state_label(*from), state_label(*to))
            }
        };
        Some(line)
    }

    fn line_json(event: &SyncEvent, snapshot: &SyncSnapshot) -> Option<String> {
        let value = match event {
            SyncEvent::ExchangeStarted { .. } => return None,
            SyncEvent::SampleAccepted {
                seq,
                sample,
                server_info,
            } => json!({
                "event": "sample",
                "seq": seq,
                "offset_ms": sample.offset().delta().as_millis_f64(),
                "delay_ms": sample.delay().delta().as_millis_f64(),
                "stratum": server_info.stratum.value(),
                "reference_id": server_info.reference_id.to_string(),
                "state": state_key(snapshot.state),
            }),
            SyncEvent::SampleRejected { seq, reason } => json!({
                "event": if reason.is_stray() { "stray" } else { "rejected" },
                "seq": seq,
                "reason": reason.to_string(),
            }),
            SyncEvent::ExchangeTimedOut { seq } => json!({ "event": "timeout", "seq": seq }),
            SyncEvent::ExchangeFailed { seq, detail } => {
                json!({ "event": "failed", "seq": seq, "detail": detail })
            }
            SyncEvent::PollIntervalChanged { from, to } => json!({
                "event": "poll_interval_changed",
                "from_ms": from.as_millis_f64(),
                "to_ms": to.as_millis_f64(),
            }),
            SyncEvent::StateChanged { from, to } => json!({
                "event": "state_changed",
                "from": state_key(*from),
                "to": state_key(*to),
            }),
        };
        Some(value.to_string())
    }
}

impl<W: Write> EventSink for StatsReporter<W> {
    fn publish(&mut self, event: &SyncEvent, snapshot: &SyncSnapshot) {
        self.last = Some(*snapshot);
        if let SyncEvent::SampleAccepted { sample, .. } = event {
            self.offset_ms.push(sample.offset().delta().as_millis_f64());
            self.delay_ms.push(sample.delay().delta().as_millis_f64());
        }
        let line = if self.json {
            Self::line_json(event, snapshot)
        } else {
            Self::line_text(event, snapshot)
        };
        if let Some(line) = line {
            let _ = writeln!(self.out, "{line}");
        }
        if let (Some(every), Some(seq)) = (self.summary_every, finished_seq(event))
            && seq.is_multiple_of(every)
        {
            self.write_summary();
        }
    }
}

/// 問い合わせが（成否にかかわらず）終わったイベントなら、その番号。
fn finished_seq(event: &SyncEvent) -> Option<u64> {
    match event {
        SyncEvent::SampleAccepted { seq, .. }
        | SyncEvent::ExchangeTimedOut { seq }
        | SyncEvent::SampleRejected { seq: Some(seq), .. }
        | SyncEvent::ExchangeFailed { seq: Some(seq), .. } => Some(*seq),
        _ => None,
    }
}

fn describe(stats: &RunningStats, signed: bool) -> String {
    let value = |v: f64| {
        if signed {
            format!("{v:+.3}")
        } else {
            format!("{v:.3}")
        }
    };
    format!(
        "min {}  avg {}  max {}  σ {:.3}",
        value(stats.min),
        value(stats.mean),
        value(stats.max),
        stats.stddev()
    )
}

fn stats_json(stats: &RunningStats) -> serde_json::Value {
    if stats.count == 0 {
        return serde_json::Value::Null;
    }
    json!({
        "count": stats.count,
        "min": stats.min,
        "avg": stats.mean,
        "max": stats.max,
        "stddev": stats.stddev(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_stats_match_direct_computation() {
        let mut stats = RunningStats::default();
        for value in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
            stats.push(value);
        }
        assert_eq!(stats.count, 8);
        assert!((stats.mean - 5.0).abs() < 1e-12);
        assert_eq!((stats.min, stats.max), (2.0, 9.0));
        // 標本標準偏差 = sqrt(32 / 7)
        assert!((stats.stddev() - (32.0_f64 / 7.0).sqrt()).abs() < 1e-12);
    }
}
