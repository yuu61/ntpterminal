//! ptp4l の状態（管理メッセージで読んだ値の解釈結果）。

use core::fmt;

use crate::shared_kernel::TimeDelta;

/// 時計の識別子（clockIdentity。EUI-64）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ClockIdentity(pub [u8; 8]);

/// linuxptp（`pmc`）と同じ書き方: "001b21.fffe.123456"
impl fmt::Display for ClockIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index == 3 || index == 5 {
                f.write_str(".")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// ポートの状態（IEEE 1588 の portState と、linuxptp 独自の `GRAND_MASTER`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PortState {
    Initializing,
    Faulty,
    Disabled,
    Listening,
    PreMaster,
    Master,
    Passive,
    Uncalibrated,
    Slave,
    /// linuxptp 独自（グランドマスタとして動いている）
    GrandMaster,
    Unknown(u8),
}

impl PortState {
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Initializing,
            2 => Self::Faulty,
            3 => Self::Disabled,
            4 => Self::Listening,
            5 => Self::PreMaster,
            6 => Self::Master,
            7 => Self::Passive,
            8 => Self::Uncalibrated,
            9 => Self::Slave,
            10 => Self::GrandMaster,
            other => Self::Unknown(other),
        }
    }
}

/// ptp4l の表示と同じ名前: "SLAVE"・"LISTENING" など。
impl fmt::Display for PortState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            Self::Initializing => "INITIALIZING",
            Self::Faulty => "FAULTY",
            Self::Disabled => "DISABLED",
            Self::Listening => "LISTENING",
            Self::PreMaster => "PRE_MASTER",
            Self::Master => "MASTER",
            Self::Passive => "PASSIVE",
            Self::Uncalibrated => "UNCALIBRATED",
            Self::Slave => "SLAVE",
            Self::GrandMaster => "GRAND_MASTER",
            Self::Unknown(bits) => return write!(f, "不明（{bits}）"),
        };
        f.write_str(name)
    }
}

/// PHC を使うポート 1 つの状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    pub number: u16,
    pub state: PortState,
}

/// ポートの状態をまとめた、時計としての役割。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockRole {
    /// どれかのポートがマスタに同期している（`SLAVE`）
    Slave,
    /// マスタを選び、合わせている途中（`UNCALIBRATED`）
    Uncalibrated,
    /// マスタとして時刻を配っている（`MASTER`・`GRAND_MASTER`）。この PHC がほかの機器の基準
    Master,
    /// マスタを探している・決めている途中（`LISTENING`・`INITIALIZING`・`PRE_MASTER`・`PASSIVE`）
    Listening,
    /// 使えない（`FAULTY`・`DISABLED`・不明な状態だけ）
    Faulty,
}

/// 時計の品質（グランドマスタの clockClass・clockAccuracy・offsetScaledLogVariance）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockQuality {
    pub class: u8,
    pub accuracy: u8,
    pub variance: u16,
}

/// 時刻系（ptpTimescale）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timescale {
    /// PTP の時刻系（TAI）。UTC との差（currentUtcOffset）を引くと UTC になる
    Ptp,
    /// 任意（ARB）。UTC との関係は分からないので換算しない
    Arbitrary,
}

/// うるう秒の予告（leap61・leap59）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeapWarning {
    None,
    /// その日の最後の 1 分が 61 秒になる
    Insert,
    /// その日の最後の 1 分が 59 秒になる
    Delete,
}

/// グランドマスタの時刻の出どころ（timeSource）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeSourceKind {
    AtomicClock,
    /// GNSS（IEEE 1588-2008 では GPS）
    Gnss,
    TerrestrialRadio,
    SerialTimeCode,
    Ptp,
    Ntp,
    HandSet,
    Other,
    InternalOscillator,
    Unknown(u8),
}

impl TimeSourceKind {
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            0x10 => Self::AtomicClock,
            0x20 => Self::Gnss,
            0x30 => Self::TerrestrialRadio,
            0x39 => Self::SerialTimeCode,
            0x40 => Self::Ptp,
            0x50 => Self::Ntp,
            0x60 => Self::HandSet,
            0x90 => Self::Other,
            0xA0 => Self::InternalOscillator,
            other => Self::Unknown(other),
        }
    }
}

/// 時刻の性質（`TIME_PROPERTIES_DATA_SET`）。グランドマスタが知らせる値を ptp4l が持っている。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeProperties {
    /// TAI − UTC（秒）
    pub utc_offset: i16,
    /// `utc_offset` が確かな値か（currentUtcOffsetValid）。グランドマスタが GNSS などから得ていなければ偽
    pub utc_offset_valid: bool,
    pub timescale: Timescale,
    pub leap: LeapWarning,
    pub time_traceable: bool,
    pub frequency_traceable: bool,
    pub time_source: TimeSourceKind,
}

/// ptp4l の状態（ある時点で読んだもの）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtpStatus {
    pub domain: u8,
    /// 表示している PHC を使うポートの状態（ポート番号の順）
    pub ports: Vec<PortStatus>,
    /// ptp4l が測ったマスタとの差（PHC − マスタ。ptp4l の offsetFromMaster）
    pub offset_from_master: TimeDelta,
    /// マスタまでの経路の遅延（片道。meanPathDelay）
    pub mean_path_delay: TimeDelta,
    pub steps_removed: u16,
    pub grandmaster: ClockIdentity,
    pub grandmaster_quality: ClockQuality,
    pub time: TimeProperties,
}

impl PtpStatus {
    /// ポートの状態をまとめる。どれかが SLAVE なら同期している（境界時計では、ほかのポートは MASTER）。
    #[must_use]
    pub fn role(&self) -> ClockRole {
        let any = |states: &[PortState]| self.ports.iter().any(|port| states.contains(&port.state));
        if any(&[PortState::Slave]) {
            ClockRole::Slave
        } else if any(&[PortState::Uncalibrated]) {
            ClockRole::Uncalibrated
        } else if any(&[PortState::Master, PortState::GrandMaster]) {
            ClockRole::Master
        } else if any(&[
            PortState::Listening,
            PortState::Initializing,
            PortState::PreMaster,
            PortState::Passive,
        ]) {
            ClockRole::Listening
        } else {
            ClockRole::Faulty
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(states: &[PortState]) -> PtpStatus {
        PtpStatus {
            domain: 0,
            ports: states
                .iter()
                .zip(1u16..)
                .map(|(&state, number)| PortStatus { number, state })
                .collect(),
            offset_from_master: TimeDelta::ZERO,
            mean_path_delay: TimeDelta::ZERO,
            steps_removed: 0,
            grandmaster: ClockIdentity::default(),
            grandmaster_quality: ClockQuality::default(),
            time: TimeProperties {
                utc_offset: 37,
                utc_offset_valid: true,
                timescale: Timescale::Ptp,
                leap: LeapWarning::None,
                time_traceable: true,
                frequency_traceable: true,
                time_source: TimeSourceKind::Gnss,
            },
        }
    }

    #[test]
    fn a_boundary_clock_with_a_slave_port_is_synchronized() {
        assert_eq!(
            status(&[PortState::Master, PortState::Slave]).role(),
            ClockRole::Slave
        );
        assert_eq!(
            status(&[PortState::Master, PortState::Uncalibrated]).role(),
            ClockRole::Uncalibrated
        );
        assert_eq!(status(&[PortState::GrandMaster]).role(), ClockRole::Master);
        assert_eq!(
            status(&[PortState::Faulty, PortState::Listening]).role(),
            ClockRole::Listening
        );
        assert_eq!(
            status(&[PortState::Faulty, PortState::Unknown(42)]).role(),
            ClockRole::Faulty
        );
        assert_eq!(status(&[]).role(), ClockRole::Faulty);
    }

    #[test]
    fn names_follow_linuxptp() {
        let identity = ClockIdentity([0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56]);
        assert_eq!(identity.to_string(), "001b21.fffe.123456");
        assert_eq!(PortState::from_bits(9).to_string(), "SLAVE");
        assert_eq!(PortState::from_bits(10), PortState::GrandMaster);
        assert_eq!(PortState::from_bits(0), PortState::Unknown(0));
        assert_eq!(TimeSourceKind::from_bits(0x20), TimeSourceKind::Gnss);
        assert_eq!(
            TimeSourceKind::from_bits(0xA0),
            TimeSourceKind::InternalOscillator
        );
    }
}
