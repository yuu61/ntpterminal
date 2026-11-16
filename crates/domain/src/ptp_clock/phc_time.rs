use super::{TimeProperties, Timescale};
use crate::shared_kernel::{LocalStamp, NtpTimestamp};

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// PHC の時刻（PHC の時計の値。1970-01-01 起点のナノ秒）。
///
/// PTP の時刻系なら TAI（UTC より currentUtcOffset 秒進んでいる）。ARB なら起点も任意。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhcTime(i128);

impl PhcTime {
    #[must_use]
    pub const fn from_nanos(nanos: i128) -> Self {
        Self(nanos)
    }

    #[must_use]
    pub const fn nanos(self) -> i128 {
        self.0
    }

    /// 表示する時刻にする。PTP の時刻系なら UTC との差を引いて UTC に、ARB ならそのまま。
    ///
    /// UTC との差が確かでない（currentUtcOffsetValid が偽）ときも、知らされた値で換算する（確かでないことは
    /// 表示の側で知らせる）。
    #[must_use]
    pub const fn to_display_time(self, time: &TimeProperties) -> NtpTimestamp {
        match time.timescale {
            Timescale::Ptp => {
                NtpTimestamp::from_unix_nanos(self.0 - time.utc_offset as i128 * NANOS_PER_SEC)
            }
            Timescale::Arbitrary => NtpTimestamp::from_unix_nanos(self.0),
        }
    }
}

/// PHC を 1 回読んだ結果: PHC の時刻と、その前後に読んだ PC の時刻。
///
/// NIC とシステムの時計を同時に読めた（`PTP_SYS_OFFSET_PRECISE`）ときは `before` と `after` が同じになる。
/// どちらもモノトニック時計の時刻に写してあること（infrastructure が行う）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhcReading {
    pub phc: PhcTime,
    pub before: LocalStamp,
    pub after: LocalStamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp_clock::{LeapWarning, TimeSourceKind};

    fn properties(utc_offset: i16, timescale: Timescale) -> TimeProperties {
        TimeProperties {
            utc_offset,
            utc_offset_valid: true,
            timescale,
            leap: LeapWarning::None,
            time_traceable: true,
            frequency_traceable: true,
            time_source: TimeSourceKind::Gnss,
        }
    }

    #[test]
    fn ptp_timescale_is_converted_to_utc() {
        // 2026-11-17 00:00:37 TAI = 00:00:00 UTC
        let utc_secs = 1_794_873_600i128;
        let phc = PhcTime::from_nanos((utc_secs + 37) * NANOS_PER_SEC + 250);
        let shown = phc.to_display_time(&properties(37, Timescale::Ptp));
        assert_eq!(shown.to_unix_nanos(), utc_secs * NANOS_PER_SEC + 250);
    }

    #[test]
    fn arbitrary_timescale_is_shown_as_is() {
        let phc = PhcTime::from_nanos(123 * NANOS_PER_SEC);
        let shown = phc.to_display_time(&properties(37, Timescale::Arbitrary));
        assert_eq!(shown.to_unix_nanos(), 123 * NANOS_PER_SEC);
    }
}
