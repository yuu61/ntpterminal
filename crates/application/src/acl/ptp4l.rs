//! ptp4l の管理メッセージで読んだデータセット（`ptp_management` コンテキスト）を、PTP の時計のコンテキスト
//! （`ptp_clock`）の状態にする。
//!
//! - 時間（`TimeInterval`）は「ナノ秒 × 2^16」なので、2^16 で割ってナノ秒にする（0 の方向に切り捨てる。
//!   表示は 1 ns 単位なので、それより細かい値は使わない）
//! - ポートの状態は、表示している PHC を使うポートのものだけを、問い合わせた順（ポート番号の順）に並べる

use domain::ptp_clock::{
    ClockIdentity, ClockQuality, LeapWarning, PortState, PortStatus, PtpStatus, TimeProperties,
    TimeSourceKind, Timescale,
};
use domain::ptp_management::{
    ClockQuality as WireQuality, CurrentDataSet, ParentDataSet, PortDataSet, TimePropertiesDataSet,
};
use domain::shared_kernel::TimeDelta;

/// `TimeInterval` の 1 ns
const SCALED_NANOS_PER_NANO: i64 = 1 << 16;

/// 読んだデータセットから ptp4l の状態を作る。
#[must_use]
pub fn to_status(
    domain: u8,
    current: &CurrentDataSet,
    parent: &ParentDataSet,
    time: &TimePropertiesDataSet,
    ports: &[PortDataSet],
) -> PtpStatus {
    PtpStatus {
        domain,
        ports: ports
            .iter()
            .map(|port| PortStatus {
                number: port.port.port,
                state: PortState::from_bits(port.port_state),
            })
            .collect(),
        offset_from_master: time_interval(current.offset_from_master_scaled),
        mean_path_delay: time_interval(current.mean_path_delay_scaled),
        steps_removed: current.steps_removed,
        grandmaster: ClockIdentity(parent.grandmaster_identity),
        grandmaster_quality: to_quality(parent.grandmaster_quality),
        time: to_time_properties(time),
    }
}

#[must_use]
pub fn to_time_properties(time: &TimePropertiesDataSet) -> TimeProperties {
    let flag = |bit: u8| time.flags & bit != 0;
    TimeProperties {
        utc_offset: time.current_utc_offset,
        utc_offset_valid: flag(TimePropertiesDataSet::UTC_OFFSET_VALID),
        timescale: if flag(TimePropertiesDataSet::PTP_TIMESCALE) {
            Timescale::Ptp
        } else {
            Timescale::Arbitrary
        },
        // 両方立っているのは規格違反。挿入を優先する（削除のうるう秒は実際に起きたことがない）
        leap: if flag(TimePropertiesDataSet::LEAP_61) {
            LeapWarning::Insert
        } else if flag(TimePropertiesDataSet::LEAP_59) {
            LeapWarning::Delete
        } else {
            LeapWarning::None
        },
        time_traceable: flag(TimePropertiesDataSet::TIME_TRACEABLE),
        frequency_traceable: flag(TimePropertiesDataSet::FREQUENCY_TRACEABLE),
        time_source: TimeSourceKind::from_bits(time.time_source),
    }
}

/// `TimeInterval`（ナノ秒 × 2^16）をナノ秒にする。
#[must_use]
pub const fn time_interval(scaled: i64) -> TimeDelta {
    TimeDelta::from_nanos(scaled / SCALED_NANOS_PER_NANO)
}

const fn to_quality(quality: WireQuality) -> ClockQuality {
    ClockQuality {
        class: quality.class,
        accuracy: quality.accuracy,
        variance: quality.offset_scaled_log_variance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ptp_management::PortIdentity;

    const GM: [u8; 8] = [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56];

    #[test]
    fn translates_the_data_sets() {
        let current = CurrentDataSet {
            steps_removed: 1,
            offset_from_master_scaled: -12 * SCALED_NANOS_PER_NANO - 0x8000,
            mean_path_delay_scaled: 1_234 * SCALED_NANOS_PER_NANO + 0x8000,
        };
        let parent = ParentDataSet {
            grandmaster_identity: GM,
            grandmaster_quality: WireQuality {
                class: 6,
                accuracy: 0x21,
                offset_scaled_log_variance: 0x4E5D,
            },
            ..ParentDataSet::default()
        };
        let time = TimePropertiesDataSet {
            current_utc_offset: 37,
            flags: TimePropertiesDataSet::UTC_OFFSET_VALID
                | TimePropertiesDataSet::PTP_TIMESCALE
                | TimePropertiesDataSet::LEAP_61
                | TimePropertiesDataSet::TIME_TRACEABLE,
            time_source: 0x20,
        };
        let ports = [
            PortDataSet {
                port: PortIdentity { clock: GM, port: 1 },
                port_state: 9,
                ..PortDataSet::default()
            },
            PortDataSet {
                port: PortIdentity { clock: GM, port: 2 },
                port_state: 6,
                ..PortDataSet::default()
            },
        ];
        let status = to_status(24, &current, &parent, &time, &ports);
        assert_eq!(status.domain, 24);
        assert_eq!(
            status.ports,
            [
                PortStatus {
                    number: 1,
                    state: PortState::Slave,
                },
                PortStatus {
                    number: 2,
                    state: PortState::Master,
                },
            ]
        );
        // 1 ns 未満は 0 の方向に切り捨てる
        assert_eq!(status.offset_from_master, TimeDelta::from_nanos(-12));
        assert_eq!(status.mean_path_delay, TimeDelta::from_nanos(1_234));
        assert_eq!(status.grandmaster.to_string(), "001b21.fffe.123456");
        assert_eq!(status.grandmaster_quality.class, 6);
        assert_eq!(
            status.time,
            TimeProperties {
                utc_offset: 37,
                utc_offset_valid: true,
                timescale: Timescale::Ptp,
                leap: LeapWarning::Insert,
                time_traceable: true,
                frequency_traceable: false,
                time_source: TimeSourceKind::Gnss,
            }
        );
    }

    #[test]
    fn an_arbitrary_timescale_without_valid_offset() {
        let time = to_time_properties(&TimePropertiesDataSet {
            current_utc_offset: 37,
            flags: TimePropertiesDataSet::LEAP_59,
            time_source: 0xA0,
        });
        assert_eq!(time.timescale, Timescale::Arbitrary);
        assert!(!time.utc_offset_valid);
        assert_eq!(time.leap, LeapWarning::Delete);
        assert_eq!(time.time_source, TimeSourceKind::InternalOscillator);
    }
}
