//! 表示用の整形（日時・時間差・状態・文字幅）。

use application::timestamping::{TimestampSource, TimestampingCounts};
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
    pub fn new<L>(offset_secs: i32, label: L) -> Self
    where
        L: Into<String>,
    {
        Self {
            offset_secs,
            label: label.into(),
        }
    }

    #[must_use]
    pub fn jst() -> Self {
        Self::new(9 * 3600, "JST")
    }

    #[must_use]
    pub fn utc() -> Self {
        Self::new(0, "UTC")
    }

    #[must_use]
    pub const fn offset_secs(&self) -> i32 {
        self.offset_secs
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// "+09:00" / "-05:30" / "+0900" / "Z" 形式のオフセットを秒にする。
    ///
    /// # Errors
    ///
    /// 形式が違うとき、または範囲外（時が 14、分が 59 を超える）のとき。利用者向けの文言を返す。
    pub fn parse_offset(text: &str) -> Result<i32, String> {
        let trimmed = text.trim();
        if trimmed.eq_ignore_ascii_case("z") || trimmed.eq_ignore_ascii_case("utc") {
            return Ok(0);
        }
        let invalid = || format!("タイムゾーンは +09:00 の形式で指定してください: {trimmed}");
        let (sign, rest) = if let Some(rest) = trimmed.strip_prefix('+') {
            (1i32, rest)
        } else if let Some(rest) = trimmed.strip_prefix('-') {
            (-1i32, rest)
        } else {
            return Err(invalid());
        };
        // ":" を除いた 4 桁の数字（hhmm）を 1 桁ずつ読む
        let digits: Option<Vec<i32>> = rest
            .chars()
            .filter(|&ch| ch != ':')
            .map(|ch| i32::try_from(ch.to_digit(10)?).ok())
            .collect();
        let Some(&[h1, h2, m1, m2]) = digits.as_deref() else {
            return Err(invalid());
        };
        let (hours, minutes) = (h1 * 10i32 + h2, m1 * 10i32 + m2);
        if hours > 14i32 || minutes > 59i32 {
            return Err(invalid());
        }
        Ok(sign * (hours * 3600 + minutes * 60))
    }
}

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
#[must_use]
pub fn to_civil(timestamp: NtpTimestamp, offset_secs: i32) -> CivilTime {
    let local = timestamp.to_unix_nanos() + i128::from(offset_secs) * NANOS_PER_SEC;
    // NTP の時刻（1968〜2104 年）なので、秒は i64 に、1 秒未満のナノ秒は u32 に必ず収まる
    let secs = i64::try_from(local.div_euclid(NANOS_PER_SEC)).unwrap_or(i64::MAX);
    let nanos = u32::try_from(local.rem_euclid(NANOS_PER_SEC)).unwrap_or(0);
    let days = secs.div_euclid(SECS_PER_DAY);
    let second_of_day = u32::try_from(secs.rem_euclid(SECS_PER_DAY)).unwrap_or(0);
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
        weekday: u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0),
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
    // 月は 1〜12、日は 1〜31 なので必ず u32 に収まる
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// "2026-11-15 (日) 17:25:45.146 JST"
#[must_use]
pub fn format_date_time(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let civil = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:04}-{:02}-{:02} ({}) {:02}:{:02}:{:02}.{:03} {}",
        civil.year,
        civil.month,
        civil.day,
        WEEKDAYS_JA[civil.weekday as usize],
        civil.hour,
        civil.minute,
        civil.second,
        civil.nanos / 1_000_000,
        zone.label()
    )
}

/// 時刻だけ: "17:25:45.146"
#[must_use]
pub fn format_clock(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let civil = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        civil.hour,
        civil.minute,
        civil.second,
        civil.nanos / 1_000_000
    )
}

/// 日付だけ: "2026-11-15 (日)"
#[must_use]
pub fn format_date(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let civil = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:04}-{:02}-{:02} ({})",
        civil.year, civil.month, civil.day, WEEKDAYS_JA[civil.weekday as usize]
    )
}

/// "2026-11-16T08:25:37.923456789Z"（UTC。JSON 用）
#[must_use]
pub fn format_rfc3339_utc(timestamp: NtpTimestamp) -> String {
    let civil = to_civil(timestamp, 0);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second, civil.nanos
    )
}

/// offset の表示: "+5.012 ms" / "-0.978 ms"（符号付き）
#[must_use]
pub fn format_offset_ms(delta: TimeDelta) -> String {
    format!("{:+.3} ms", delta.as_millis_f64())
}

/// delay などの表示: "0.423 ms"
#[must_use]
pub fn format_delay_ms(delta: TimeDelta) -> String {
    format!("{:.3} ms", delta.as_millis_f64())
}

#[must_use]
pub const fn state_label(state: SyncState) -> &'static str {
    match state {
        SyncState::Unsynchronized => "未同期",
        SyncState::Synchronized => "同期済み",
        SyncState::Holdover => "自走中（応答なし）",
        SyncState::Denied => "拒否された（KoD）",
    }
}

/// JSON 用の状態名。
#[must_use]
pub const fn state_key(state: SyncState) -> &'static str {
    match state {
        SyncState::Unsynchronized => "unsynchronized",
        SyncState::Synchronized => "synchronized",
        SyncState::Holdover => "holdover",
        SyncState::Denied => "denied",
    }
}

#[must_use]
pub const fn leap_label(leap: LeapIndicator) -> &'static str {
    match leap {
        LeapIndicator::NoWarning => "なし",
        LeapIndicator::InsertSecond => "うるう秒の挿入予告",
        LeapIndicator::DeleteSecond => "うるう秒の削除予告",
        LeapIndicator::Unsynchronized => "未同期（アラーム）",
    }
}

/// JSON 用のうるう秒予告の名前。
#[must_use]
pub const fn leap_key(leap: LeapIndicator) -> &'static str {
    match leap {
        LeapIndicator::NoWarning => "none",
        LeapIndicator::InsertSecond => "insert",
        LeapIndicator::DeleteSecond => "delete",
        LeapIndicator::Unsynchronized => "unsynchronized",
    }
}

/// タイムスタンプの出どころ（直近の受信）。設定した出どころの時刻が取れていないことを隠さない。
#[must_use]
pub const fn timestamping_label(counts: &TimestampingCounts) -> &'static str {
    if !counts.kernel_enabled {
        return "ユーザ空間";
    }
    if counts.samples() == 0 {
        return if counts.hardware_enabled {
            "ハードウェア（応答待ち）"
        } else {
            "カーネル（応答待ち）"
        };
    }
    match counts.last_source {
        TimestampSource::Hardware => "ハードウェア",
        TimestampSource::Kernel if counts.hardware_enabled => {
            "カーネル（ハードウェアの時刻が取れない）"
        }
        TimestampSource::Kernel => "カーネル",
        TimestampSource::User => "ユーザ空間（カーネルの時刻が取れない）",
    }
}

/// タイムスタンプの内訳: "カーネル 12 回 / ユーザ空間 0 回（送信時刻なし 0 回）"
/// （ハードウェアのときは先頭に "ハードウェア 12 回 / " が付く）
#[must_use]
pub fn timestamping_breakdown(counts: &TimestampingCounts) -> String {
    if counts.hardware_enabled {
        format!(
            "ハードウェア {} 回 / カーネル {} 回 / ユーザ空間 {} 回（送信時刻なし {} 回）",
            counts.hardware_samples, counts.kernel_samples, counts.user_samples, counts.tx_misses
        )
    } else if counts.kernel_enabled {
        format!(
            "カーネル {} 回 / ユーザ空間 {} 回（送信時刻なし {} 回）",
            counts.kernel_samples, counts.user_samples, counts.tx_misses
        )
    } else {
        "ユーザ空間".to_owned()
    }
}

/// 端末での表示幅（日本語などの全角文字は 2）。ラベルの桁揃えに使う。
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 表示幅が `width` になるまで右側に空白を足す。
#[must_use]
pub fn pad_to_width(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

fn char_width(ch: char) -> usize {
    let wide = matches!(
        u32::from(ch),
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
        let civil = to_civil(unix_secs(secs), DisplayZone::jst().offset_secs());
        assert_eq!(
            (civil.month, civil.day, civil.hour, civil.weekday),
            (11, 16, 5, 1)
        );
    }

    #[test]
    fn parses_timezone_offsets() {
        assert_eq!(DisplayZone::parse_offset("+09:00"), Ok(32_400i32));
        assert_eq!(DisplayZone::parse_offset("-0530"), Ok(-19_800i32));
        assert_eq!(DisplayZone::parse_offset("Z"), Ok(0i32));
        assert!(
            DisplayZone::parse_offset("09:00")
                .unwrap_err()
                .contains("+09:00 の形式")
        );
        assert!(
            DisplayZone::parse_offset("+25:00")
                .unwrap_err()
                .contains("+09:00 の形式")
        );
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

    #[test]
    fn timestamping_label_does_not_hide_fallbacks() {
        let user = TimestampingCounts::default();
        assert_eq!(timestamping_label(&user), "ユーザ空間");

        let kernel = TimestampingCounts {
            kernel_enabled: true,
            ..TimestampingCounts::default()
        };
        assert_eq!(timestamping_label(&kernel), "カーネル（応答待ち）");
        let kernel_ok = TimestampingCounts {
            kernel_samples: 1,
            last_source: TimestampSource::Kernel,
            ..kernel
        };
        assert_eq!(timestamping_label(&kernel_ok), "カーネル");

        let hardware = TimestampingCounts {
            hardware_enabled: true,
            ..kernel
        };
        assert_eq!(timestamping_label(&hardware), "ハードウェア（応答待ち）");
        let hardware_ok = TimestampingCounts {
            hardware_samples: 1,
            last_source: TimestampSource::Hardware,
            ..hardware
        };
        assert_eq!(timestamping_label(&hardware_ok), "ハードウェア");
        let hardware_fell_back = TimestampingCounts {
            kernel_samples: 1,
            last_source: TimestampSource::Kernel,
            ..hardware_ok
        };
        assert_eq!(
            timestamping_label(&hardware_fell_back),
            "カーネル（ハードウェアの時刻が取れない）"
        );
        let user_fallback = TimestampingCounts {
            user_samples: 1,
            last_source: TimestampSource::User,
            ..hardware_ok
        };
        assert_eq!(
            timestamping_label(&user_fallback),
            "ユーザ空間（カーネルの時刻が取れない）"
        );
    }

    #[test]
    fn timestamping_breakdown_lists_hardware_only_when_enabled() {
        let kernel = TimestampingCounts {
            kernel_enabled: true,
            kernel_samples: 3,
            user_samples: 1,
            tx_misses: 1,
            ..TimestampingCounts::default()
        };
        assert_eq!(
            timestamping_breakdown(&kernel),
            "カーネル 3 回 / ユーザ空間 1 回（送信時刻なし 1 回）"
        );
        let hardware = TimestampingCounts {
            hardware_enabled: true,
            hardware_samples: 18,
            kernel_samples: 2,
            user_samples: 0,
            tx_misses: 0,
            ..kernel
        };
        assert_eq!(
            timestamping_breakdown(&hardware),
            "ハードウェア 18 回 / カーネル 2 回 / ユーザ空間 0 回（送信時刻なし 0 回）"
        );
        assert_eq!(
            timestamping_breakdown(&TimestampingCounts::default()),
            "ユーザ空間"
        );
    }
}
