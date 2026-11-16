use core::net::SocketAddr;

use application::usecase::{OnceError, OnceOutcome};
use serde_json::json;

use super::render_rows;
use crate::format::{
    DisplayZone, format_date_time, format_delay_ms, format_offset_ms, format_rfc3339_utc, leap_key,
    leap_label,
};

#[must_use]
pub fn render_text(
    server: SocketAddr,
    transport: &str,
    timestamping: &str,
    outcome: &OnceOutcome,
    zone: &DisplayZone,
) -> String {
    let OnceOutcome {
        sample,
        server_info,
    } = *outcome;
    let rows = [
        ("サーバ", server.to_string()),
        (
            "オフセット",
            format!(
                "{}（サーバ − PC）",
                format_offset_ms(sample.offset().delta())
            ),
        ),
        ("遅延", format_delay_ms(sample.delay().delta())),
        ("Stratum", server_info.stratum.value().to_string()),
        ("Ref ID", server_info.reference_id.to_string()),
        ("NTP version", server_info.version.to_string()),
        ("Leap", leap_label(server_info.leap).to_owned()),
        ("Precision", format!("2^{} s", server_info.precision)),
        ("Root delay", format_delay_ms(server_info.root_delay)),
        (
            "Root dispersion",
            format_delay_ms(server_info.root_dispersion),
        ),
        (
            "サーバ時刻",
            format_date_time(sample.server_transmit(), zone),
        ),
        ("送信方式", transport.to_owned()),
        ("タイムスタンプ", timestamping.to_owned()),
    ];
    render_rows(&rows)
}

/// JSON（1 行）。
#[must_use]
pub fn render_json(
    server: SocketAddr,
    transport: &str,
    timestamping: &str,
    outcome: &OnceOutcome,
) -> String {
    let OnceOutcome {
        sample,
        server_info,
    } = *outcome;
    json!({
        "ok": true,
        "server": server.to_string(),
        "transport": transport,
        "timestamping": timestamping,
        "offset_ms": sample.offset().delta().as_millis_f64(),
        "delay_ms": sample.delay().delta().as_millis_f64(),
        "stratum": server_info.stratum.value(),
        "reference_id": server_info.reference_id.to_string(),
        "version": server_info.version,
        "leap": leap_key(server_info.leap),
        "precision_log2": server_info.precision,
        "root_delay_ms": server_info.root_delay.as_millis_f64(),
        "root_dispersion_ms": server_info.root_dispersion.as_millis_f64(),
        "server_time_utc": format_rfc3339_utc(sample.server_transmit()),
    })
    .to_string()
}

/// 失敗時の JSON（1 行）。
#[must_use]
pub fn render_error_json(server: SocketAddr, error: &OnceError) -> String {
    json!({
        "ok": false,
        "server": server.to_string(),
        "error": error.to_string(),
    })
    .to_string()
}
