//! PTP のメッセージ（`ptp_message` コンテキスト）を、PTP の時計のコンテキスト（`ptp_clock`）の入力にする。
//!
//! - 時刻（`Timestamp`）は起点 1970-01-01 のナノ秒（[`PhcTime`]）にする。TAI か ARB かは Announce のフラグで決まり、
//!   換算は `ptp_clock` が行う
//! - `correctionField`（ナノ秒 × 2^16）は最も近いナノ秒にする
//! - 間隔（`logMessageInterval`）の 0x7F は「指定なし」なので `None` にする（ユニキャストの応答などで使われる）

use domain::ptp_clock::{
    AnnounceInfo, ClockIdentity, ClockQuality, DelayRespInfo, EventStamp, FollowUpInfo,
    LeapWarning, PhcTime, PortIdentity, SyncInfo, TimeProperties, TimeSourceKind, Timescale,
};
use domain::ptp_message::{Announce, Flags, Header, PortIdentity as WireIdentity, PtpTimestamp};

#[must_use]
pub const fn to_port_identity(identity: WireIdentity) -> PortIdentity {
    PortIdentity {
        clock: ClockIdentity(identity.clock),
        port: identity.port,
    }
}

#[must_use]
pub const fn to_wire_identity(identity: PortIdentity) -> WireIdentity {
    WireIdentity {
        clock: identity.clock.0,
        port: identity.port,
    }
}

/// `logMessageInterval`。0x7F（指定なし）は `None`。
#[must_use]
pub const fn log_interval(header: &Header) -> Option<i8> {
    if header.log_message_interval == Header::LOG_INTERVAL_UNSPECIFIED {
        None
    } else {
        Some(header.log_message_interval)
    }
}

#[must_use]
pub const fn to_time(stamp: PtpTimestamp) -> PhcTime {
    PhcTime::from_nanos(stamp.to_nanos())
}

/// Announce のフラグと本体から、時刻の性質を作る。
#[must_use]
pub const fn to_time_properties(flags: Flags, announce: &Announce) -> TimeProperties {
    TimeProperties {
        utc_offset: announce.current_utc_offset,
        utc_offset_valid: flags.contains(Flags::UTC_OFFSET_VALID),
        timescale: if flags.contains(Flags::PTP_TIMESCALE) {
            Timescale::Ptp
        } else {
            Timescale::Arbitrary
        },
        // 両方立っているのは規格違反。挿入を優先する（削除のうるう秒は実際に起きたことがない）
        leap: if flags.contains(Flags::LEAP_61) {
            LeapWarning::Insert
        } else if flags.contains(Flags::LEAP_59) {
            LeapWarning::Delete
        } else {
            LeapWarning::None
        },
        time_traceable: flags.contains(Flags::TIME_TRACEABLE),
        frequency_traceable: flags.contains(Flags::FREQUENCY_TRACEABLE),
        time_source: TimeSourceKind::from_bits(announce.time_source),
    }
}

#[must_use]
pub const fn to_announce(header: &Header, announce: &Announce) -> AnnounceInfo {
    AnnounceInfo {
        master: to_port_identity(header.source),
        log_interval: log_interval(header),
        grandmaster: ClockIdentity(announce.grandmaster_identity),
        priority1: announce.grandmaster_priority1,
        quality: ClockQuality {
            class: announce.grandmaster_class,
            accuracy: announce.grandmaster_accuracy,
            variance: announce.grandmaster_variance,
        },
        priority2: announce.grandmaster_priority2,
        steps_removed: announce.steps_removed,
        time: to_time_properties(header.flags, announce),
    }
}

/// Sync。two-step のフラグが立っていれば、本体の時刻は目安なので使わない（`Follow_Up` を待つ）。
#[must_use]
pub const fn to_sync(header: &Header, origin: PtpTimestamp, received: EventStamp) -> SyncInfo {
    SyncInfo {
        master: to_port_identity(header.source),
        sequence_id: header.sequence_id,
        log_interval: log_interval(header),
        origin: if header.flags.contains(Flags::TWO_STEP) {
            None
        } else {
            Some(to_time(origin))
        },
        correction: header.correction.to_delta(),
        received,
    }
}

#[must_use]
pub const fn to_follow_up(header: &Header, precise_origin: PtpTimestamp) -> FollowUpInfo {
    FollowUpInfo {
        master: to_port_identity(header.source),
        sequence_id: header.sequence_id,
        precise_origin: to_time(precise_origin),
        correction: header.correction.to_delta(),
    }
}

#[must_use]
pub const fn to_delay_resp(
    header: &Header,
    receive: PtpTimestamp,
    requesting: WireIdentity,
) -> DelayRespInfo {
    DelayRespInfo {
        master: to_port_identity(header.source),
        sequence_id: header.sequence_id,
        requesting: to_port_identity(requesting),
        receive: to_time(receive),
        correction: header.correction.to_delta(),
        log_min_interval: log_interval(header),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ptp_message::Correction;
    use domain::shared_kernel::{LocalStamp, NtpTimestamp, PcTime, TimeDelta};

    fn header(flags: Flags, log_message_interval: i8) -> Header {
        Header {
            transport_specific: 0,
            version: Header::VERSION_2,
            domain: 0,
            minor_sdo_id: 0,
            flags,
            correction: Correction(1500 << 16),
            source: WireIdentity {
                clock: [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56],
                port: 1,
            },
            sequence_id: 9,
            log_message_interval,
        }
    }

    #[test]
    fn a_two_step_sync_waits_for_the_follow_up() {
        let origin = PtpTimestamp {
            seconds: 100,
            nanos: 5,
        };
        let received = EventStamp {
            precise: None,
            user: LocalStamp {
                mono: PcTime::monotonic(0),
                wall: NtpTimestamp::from_bits(0),
            },
        };
        let two_step = to_sync(&header(Flags::TWO_STEP, 0), origin, received);
        assert_eq!(two_step.origin, None);
        assert_eq!(two_step.correction, TimeDelta::from_nanos(1500));
        let one_step = to_sync(&header(Flags(0), -3), origin, received);
        assert_eq!(one_step.origin, Some(PhcTime::from_nanos(100_000_000_005)));
        assert_eq!(one_step.log_interval, Some(-3));
        assert_eq!(one_step.master.to_string(), "001b21.fffe.123456-1");
    }

    #[test]
    fn unspecified_intervals_become_none() {
        let response = to_delay_resp(
            &header(Flags::UNICAST, 0x7F),
            PtpTimestamp::default(),
            WireIdentity::default(),
        );
        assert_eq!(response.log_min_interval, None);
        let multicast = to_delay_resp(
            &header(Flags(0), -1),
            PtpTimestamp::default(),
            WireIdentity::default(),
        );
        assert_eq!(multicast.log_min_interval, Some(-1));
    }

    #[test]
    fn announce_flags_become_time_properties() {
        let announce = Announce {
            current_utc_offset: 37,
            time_source: 0x20,
            ..Announce::default()
        };
        let flags = Flags::PTP_TIMESCALE
            .with(Flags::UTC_OFFSET_VALID)
            .with(Flags::LEAP_61)
            .with(Flags::TIME_TRACEABLE);
        let time = to_time_properties(flags, &announce);
        assert_eq!(time.utc_offset, 37);
        assert!(time.utc_offset_valid);
        assert_eq!(time.timescale, Timescale::Ptp);
        assert_eq!(time.leap, LeapWarning::Insert);
        assert!(time.time_traceable);
        assert!(!time.frequency_traceable);
        assert_eq!(time.time_source, TimeSourceKind::Gnss);
        let arbitrary = to_time_properties(Flags(0), &announce);
        assert_eq!(arbitrary.timescale, Timescale::Arbitrary);
        assert!(!arbitrary.utc_offset_valid);
    }
}
