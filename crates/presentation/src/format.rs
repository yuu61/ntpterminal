//! 表示用の整形（日時・時間差・状態・文字幅）。

use domain::shared_kernel::{NtpTimestamp, TimeDelta};
use domain::time_sync::{LeapIndicator, SyncState};

const NANOS_PER_SEC: i128 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// 曜日（日曜始まり）。
pub const WEEKDAYS_JA: [&str; 7] = ["日", "月", "火", "水", "木", "金", "土"];

/// 表示用のタイムゾーン（UTC からの固定オフセット）。OS のタイムゾーン設定には依存しない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayZone {
    offset_secs: i32,
    label: String,
}

impl DisplayZone {
    pub fn new(offset_secs: i32, label: impl Into<String>) -> Self {
        Self {
            offset_secs,
            label: label.into(),
        }
    }

    pub fn jst() -> Self {
        Self::new(9 * 3600, "JST")
    }

    pub fn utc() -> Self {
        Self::new(0, "UTC")
    }

    pub fn offset_secs(&self) -> i32 {
        self.offset_secs
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// "+09:00" / "-05:30" / "+0900" / "Z" 形式のオフセットを秒にする。
    pub fn parse_offset(text: &str) -> Result<i32, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("z") || text.eq_ignore_ascii_case("utc") {
            return Ok(0);
        }
        let invalid = || format!("タイムゾーンは +09:00 の形式で指定してください: {text}");
        let (sign, rest) = if let Some(rest) = text.strip_prefix('+') {
            (1, rest)
        } else if let Some(rest) = text.strip_prefix('-') {
            (-1, rest)
        } else {
            return Err(invalid());
        };
        let digits: String = rest.chars().filter(|&c| c != ':').collect();
        if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(invalid());
        }
        let hours: i32 = digits[..2].parse().map_err(|_| invalid())?;
        let minutes: i32 = digits[2..].parse().map_err(|_| invalid())?;
        if hours > 14 || minutes > 59 {
            return Err(invalid());
        }
        Ok(sign * (hours * 3600 + minutes * 60))
    }
}

/// 暦の日時。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CivilTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanos: u32,
    /// 0 = 日曜
    pub weekday: u32,
}

/// NTP タイムスタンプを、UTC から `offset_secs` ずらした暦の日時にする。
pub fn to_civil(timestamp: NtpTimestamp, offset_secs: i32) -> CivilTime {
    let local = timestamp.to_unix_nanos() + i128::from(offset_secs) * NANOS_PER_SEC;
    let secs = local.div_euclid(NANOS_PER_SEC) as i64;
    let nanos = local.rem_euclid(NANOS_PER_SEC) as u32;
    let days = secs.div_euclid(SECS_PER_DAY);
    let second_of_day = secs.rem_euclid(SECS_PER_DAY) as u32;
    let (year, month, day) = civil_from_days(days);
    CivilTime {
        year,
        month,
        day,
        hour: second_of_day / 3600,
        minute: second_of_day / 60 % 60,
        second: second_of_day % 60,
        nanos,
        // 1970-01-01 は木曜日
        weekday: (days + 4).rem_euclid(7) as u32,
    }
}

/// 1970-01-01 からの日数を (年, 月, 日) にする（Howard Hinnant のアルゴリズム）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// "2026-11-15 (日) 17:25:45.146 JST"
pub fn format_date_time(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let c = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:04}-{:02}-{:02} ({}) {:02}:{:02}:{:02}.{:03} {}",
        c.year,
        c.month,
        c.day,
        WEEKDAYS_JA[c.weekday as usize],
        c.hour,
        c.minute,
        c.second,
        c.nanos / 1_000_000,
        zone.label()
    )
}

/// 時刻だけ: "17:25:45.146"
pub fn format_clock(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let c = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        c.hour,
        c.minute,
        c.second,
        c.nanos / 1_000_000
    )
}

/// 日付だけ: "2026-11-15 (日)"
pub fn format_date(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let c = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:04}-{:02}-{:02} ({})",
        c.year, c.month, c.day, WEEKDAYS_JA[c.weekday as usize]
    )
}

/// "2026-11-16T08:25:37.923456789Z"（UTC。JSON 用）
pub fn format_rfc3339_utc(timestamp: NtpTimestamp) -> String {
    let c = to_civil(timestamp, 0);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.nanos
    )
}

/// offset の表示: "+5.012 ms" / "-0.978 ms"（符号付き）
pub fn format_offset_ms(delta: TimeDelta) -> String {
    format!("{:+.3} ms", delta.as_millis_f64())
}

/// delay などの表示: "0.423 ms"
pub fn format_delay_ms(delta: TimeDelta) -> String {
    format!("{:.3} ms", delta.as_millis_f64())
}

pub fn state_label(state: SyncState) -> &'static str {
    match state {
        SyncState::Unsynchronized => "未同期",
        SyncState::Synchronized => "同期済み",
        SyncState::Holdover => "自走中（応答なし）",
        SyncState::Denied => "拒否された（KoD）",
    }
}

/// JSON 用の状態名。
pub fn state_key(state: SyncState) -> &'static str {
    match state {
        SyncState::Unsynchronized => "unsynchronized",
        SyncState::Synchronized => "synchronized",
        SyncState::Holdover => "holdover",
        SyncState::Denied => "denied",
    }
}

pub fn leap_label(leap: LeapIndicator) -> &'static str {
    match leap {
        LeapIndicator::NoWarning => "なし",
        LeapIndicator::InsertSecond => "うるう秒の挿入予告",
        LeapIndicator::DeleteSecond => "うるう秒の削除予告",
        LeapIndicator::Unsynchronized => "未同期（アラーム）",
    }
}

/// JSON 用のうるう秒予告の名前。
pub fn leap_key(leap: LeapIndicator) -> &'static str {
    match leap {
        LeapIndicator::NoWarning => "none",
        LeapIndicator::InsertSecond => "insert",
        LeapIndicator::DeleteSecond => "delete",
        LeapIndicator::Unsynchronized => "unsynchronized",
    }
}

/// 端末での表示幅（日本語などの全角文字は 2）。ラベルの桁揃えに使う。
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 表示幅が `width` になるまで右側に空白を足す。
pub fn pad_to_width(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

fn char_width(c: char) -> usize {
    let wide = matches!(
        u32::from(c),
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x20000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix_secs(secs: i64) -> NtpTimestamp {
        NtpTimestamp::from_unix_nanos(i128::from(secs) * NANOS_PER_SEC)
    }

    #[test]
    fn converts_known_dates() {
        let epoch = to_civil(unix_secs(0), 0);
        assert_eq!(
            (epoch.year, epoch.month, epoch.day, epoch.weekday),
            (1970, 1, 1, 4)
        );

        // NTP の era が一周する瞬間: 2036-02-07 06:28:16 UTC（木曜）
        let rollover = to_civil(unix_secs(2_085_978_496), 0);
        assert_eq!(
            (
                rollover.year,
                rollover.month,
                rollover.day,
                rollover.weekday
            ),
            (2036, 2, 7, 4)
        );
        assert_eq!(
            (rollover.hour, rollover.minute, rollover.second),
            (6, 28, 16)
        );

        // うるう年の 2 月 29 日
        let leap_day = to_civil(unix_secs(951_782_400), 0);
        assert_eq!((leap_day.year, leap_day.month, leap_day.day), (2000, 2, 29));
    }

    #[test]
    fn formats_date_time_in_jst() {
        // 2026-11-15 08:25:45.146 UTC = 17:25:45.146 JST（日曜）
        let secs = 20_772 * SECS_PER_DAY + 8 * 3600 + 25 * 60 + 45;
        let timestamp =
            NtpTimestamp::from_unix_nanos(i128::from(secs) * NANOS_PER_SEC + 146_000_000);
        assert_eq!(
            format_date_time(timestamp, &DisplayZone::jst()),
            "2026-11-15 (日) 17:25:45.146 JST"
        );
        assert_eq!(
            format_rfc3339_utc(timestamp),
            "2026-11-15T08:25:45.146000000Z"
        );
        assert_eq!(format_clock(timestamp, &DisplayZone::jst()), "17:25:45.146");
        assert_eq!(
            format_date(timestamp, &DisplayZone::jst()),
            "2026-11-15 (日)"
        );
    }

    #[test]
    fn jst_shifts_across_midnight() {
        // 2026-11-15 20:00 UTC = 2026-11-16 05:00 JST（月曜）
        let secs = 20_772 * SECS_PER_DAY + 20 * 3600;
        let c = to_civil(unix_secs(secs), DisplayZone::jst().offset_secs());
        assert_eq!((c.month, c.day, c.hour, c.weekday), (11, 16, 5, 1));
    }

    #[test]
    fn parses_timezone_offsets() {
        assert_eq!(DisplayZone::parse_offset("+09:00"), Ok(32_400));
        assert_eq!(DisplayZone::parse_offset("-0530"), Ok(-19_800));
        assert_eq!(DisplayZone::parse_offset("Z"), Ok(0));
        assert!(DisplayZone::parse_offset("09:00").is_err());
        assert!(DisplayZone::parse_offset("+25:00").is_err());
    }

    #[test]
    fn formats_milliseconds() {
        assert_eq!(format_offset_ms(TimeDelta::from_micros(-978)), "-0.978 ms");
        assert_eq!(format_offset_ms(TimeDelta::from_micros(5_012)), "+5.012 ms");
        assert_eq!(format_delay_ms(TimeDelta::from_micros(423)), "0.423 ms");
    }

    #[test]
    fn measures_east_asian_width() {
        assert_eq!(display_width("オフセット"), 10);
        assert_eq!(display_width("Stratum"), 7);
        assert_eq!(pad_to_width("遅延", 8), "遅延    ");
    }
}
