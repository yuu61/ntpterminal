//! MANAGEMENT TLV のデータ部（データセット）。配置は linuxptp の `ds.h`・`tlv.h`（どれも詰めて並べる）。
//!
//! - 時間（TimeInterval）は規格どおり「ナノ秒 × 2^16」の生の値で持つ（ナノ秒への換算は ACL）
//! - 読むときは、決まった長さより短ければエラーにし、後ろの余り（偶数にそろえる埋めなど）は無視する
//! - `to_bytes` は読むときと同じ配置で書く（テストで ptp4l の応答を作るのに使う）

use super::message::{push_port_identity, read_port_identity, read_u16};
use super::{DecodeError, PortIdentity};

/// 時計の品質（clockClass・clockAccuracy・offsetScaledLogVariance）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockQuality {
    pub class: u8,
    pub accuracy: u8,
    pub offset_scaled_log_variance: u16,
}

impl ClockQuality {
    const fn read(bytes: &[u8], at: usize) -> Self {
        Self {
            class: bytes[at],
            accuracy: bytes[at + 1],
            offset_scaled_log_variance: read_u16(bytes, at + 2),
        }
    }

    fn push(self, out: &mut Vec<u8>) {
        out.push(self.class);
        out.push(self.accuracy);
        out.extend_from_slice(&self.offset_scaled_log_variance.to_be_bytes());
    }
}

/// `DEFAULT_DATA_SET`（20 バイト）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DefaultDataSet {
    /// bit 0: twoStepFlag、bit 1: slaveOnly
    pub flags: u8,
    /// ネットワークのポートの数（ptp4l の管理用ソケットは数えない）。ポート番号は 1〜この数
    pub number_ports: u16,
    pub priority1: u8,
    pub quality: ClockQuality,
    pub priority2: u8,
    pub clock_identity: [u8; 8],
    pub domain: u8,
}

impl DefaultDataSet {
    const LEN: usize = 20;

    /// # Errors
    ///
    /// データが 20 バイトより短いとき。
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        let data = fixed::<{ Self::LEN }>(bytes)?;
        Ok(Self {
            flags: data[0],
            number_ports: read_u16(data, 2),
            priority1: data[4],
            quality: ClockQuality::read(data, 5),
            priority2: data[9],
            clock_identity: read_identity(data, 10),
            domain: data[18],
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![self.flags, 0];
        out.extend_from_slice(&self.number_ports.to_be_bytes());
        out.push(self.priority1);
        self.quality.push(&mut out);
        out.push(self.priority2);
        out.extend_from_slice(&self.clock_identity);
        out.push(self.domain);
        out.push(0);
        out
    }
}

/// `CURRENT_DATA_SET`（18 バイト）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CurrentDataSet {
    pub steps_removed: u16,
    /// マスタとの時刻の差（ナノ秒 × 2^16）
    pub offset_from_master_scaled: i64,
    /// マスタまでの経路の遅延（片道。ナノ秒 × 2^16）
    pub mean_path_delay_scaled: i64,
}

impl CurrentDataSet {
    const LEN: usize = 18;

    /// # Errors
    ///
    /// データが 18 バイトより短いとき。
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        let data = fixed::<{ Self::LEN }>(bytes)?;
        Ok(Self {
            steps_removed: read_u16(data, 0),
            offset_from_master_scaled: read_i64(data, 2),
            mean_path_delay_scaled: read_i64(data, 10),
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.steps_removed.to_be_bytes().to_vec();
        out.extend_from_slice(&self.offset_from_master_scaled.to_be_bytes());
        out.extend_from_slice(&self.mean_path_delay_scaled.to_be_bytes());
        out
    }
}

/// `PARENT_DATA_SET`（32 バイト）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParentDataSet {
    pub parent: PortIdentity,
    pub parent_stats: u8,
    pub observed_offset_scaled_log_variance: u16,
    pub observed_phase_change_rate: i32,
    pub grandmaster_priority1: u8,
    pub grandmaster_quality: ClockQuality,
    pub grandmaster_priority2: u8,
    pub grandmaster_identity: [u8; 8],
}

impl ParentDataSet {
    const LEN: usize = 32;

    /// # Errors
    ///
    /// データが 32 バイトより短いとき。
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        let data = fixed::<{ Self::LEN }>(bytes)?;
        Ok(Self {
            parent: read_port_identity(data, 0),
            parent_stats: data[10],
            observed_offset_scaled_log_variance: read_u16(data, 12),
            observed_phase_change_rate: i32::from_be_bytes([
                data[14], data[15], data[16], data[17],
            ]),
            grandmaster_priority1: data[18],
            grandmaster_quality: ClockQuality::read(data, 19),
            grandmaster_priority2: data[23],
            grandmaster_identity: read_identity(data, 24),
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        push_port_identity(&mut out, self.parent);
        out.push(self.parent_stats);
        out.push(0);
        out.extend_from_slice(&self.observed_offset_scaled_log_variance.to_be_bytes());
        out.extend_from_slice(&self.observed_phase_change_rate.to_be_bytes());
        out.push(self.grandmaster_priority1);
        self.grandmaster_quality.push(&mut out);
        out.push(self.grandmaster_priority2);
        out.extend_from_slice(&self.grandmaster_identity);
        out
    }
}

/// `TIME_PROPERTIES_DATA_SET`（4 バイト）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimePropertiesDataSet {
    pub current_utc_offset: i16,
    /// bit 0: leap61、bit 1: leap59、bit 2: currentUtcOffsetValid、bit 3: ptpTimescale、
    /// bit 4: timeTraceable、bit 5: frequencyTraceable
    pub flags: u8,
    pub time_source: u8,
}

impl TimePropertiesDataSet {
    pub const LEAP_61: u8 = 1 << 0;
    pub const LEAP_59: u8 = 1 << 1;
    pub const UTC_OFFSET_VALID: u8 = 1 << 2;
    pub const PTP_TIMESCALE: u8 = 1 << 3;
    pub const TIME_TRACEABLE: u8 = 1 << 4;
    pub const FREQUENCY_TRACEABLE: u8 = 1 << 5;
    const LEN: usize = 4;

    /// # Errors
    ///
    /// データが 4 バイトより短いとき。
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        let data = fixed::<{ Self::LEN }>(bytes)?;
        Ok(Self {
            current_utc_offset: i16::from_be_bytes([data[0], data[1]]),
            flags: data[2],
            time_source: data[3],
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.current_utc_offset.to_be_bytes().to_vec();
        out.push(self.flags);
        out.push(self.time_source);
        out
    }
}

/// `PORT_DATA_SET`（26 バイト）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortDataSet {
    pub port: PortIdentity,
    pub port_state: u8,
    pub log_min_delay_req_interval: i8,
    /// ナノ秒 × 2^16
    pub peer_mean_path_delay_scaled: i64,
    pub log_announce_interval: i8,
    pub announce_receipt_timeout: u8,
    pub log_sync_interval: i8,
    pub delay_mechanism: u8,
    pub log_min_pdelay_req_interval: i8,
    pub version: u8,
}

impl PortDataSet {
    const LEN: usize = 26;

    /// # Errors
    ///
    /// データが 26 バイトより短いとき。
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        let data = fixed::<{ Self::LEN }>(bytes)?;
        Ok(Self {
            port: read_port_identity(data, 0),
            port_state: data[10],
            log_min_delay_req_interval: i8::from_be_bytes([data[11]]),
            peer_mean_path_delay_scaled: read_i64(data, 12),
            log_announce_interval: i8::from_be_bytes([data[20]]),
            announce_receipt_timeout: data[21],
            log_sync_interval: i8::from_be_bytes([data[22]]),
            delay_mechanism: data[23],
            log_min_pdelay_req_interval: i8::from_be_bytes([data[24]]),
            version: data[25],
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        push_port_identity(&mut out, self.port);
        out.push(self.port_state);
        out.extend_from_slice(&self.log_min_delay_req_interval.to_be_bytes());
        out.extend_from_slice(&self.peer_mean_path_delay_scaled.to_be_bytes());
        out.extend_from_slice(&self.log_announce_interval.to_be_bytes());
        out.push(self.announce_receipt_timeout);
        out.extend_from_slice(&self.log_sync_interval.to_be_bytes());
        out.push(self.delay_mechanism);
        out.extend_from_slice(&self.log_min_pdelay_req_interval.to_be_bytes());
        out.push(self.version);
        out
    }
}

/// `PORT_PROPERTIES_NP`（linuxptp 独自。12 バイト + インターフェース名）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortPropertiesNp {
    pub port: PortIdentity,
    pub port_state: u8,
    /// linuxptp の `enum timestamp_type`（0: ソフトウェア、1: ハードウェア、2: 旧来のハードウェア、
    /// 3: one-step、4: P2P の one-step）
    pub timestamping: u8,
    /// インターフェース名（PTPText。長さ 1 バイト + 文字）
    pub interface: Vec<u8>,
}

impl PortPropertiesNp {
    const FIXED_LEN: usize = 13;

    /// # Errors
    ///
    /// データが固定部分（13 バイト）より短いとき、またはインターフェース名が途中で切れているとき。
    pub fn parse(data: &[u8]) -> Result<Self, DecodeError> {
        let (head, rest) =
            data.split_first_chunk::<{ Self::FIXED_LEN }>()
                .ok_or(DecodeError::DataTooShort {
                    expected: Self::FIXED_LEN,
                    actual: data.len(),
                })?;
        let name_len = usize::from(head[12]);
        let interface = rest.get(..name_len).ok_or(DecodeError::DataTooShort {
            expected: Self::FIXED_LEN + name_len,
            actual: data.len(),
        })?;
        Ok(Self {
            port: read_port_identity(head, 0),
            port_state: head[10],
            timestamping: head[11],
            interface: interface.to_vec(),
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::FIXED_LEN + self.interface.len());
        push_port_identity(&mut out, self.port);
        out.push(self.port_state);
        out.push(self.timestamping);
        let name = &self.interface[..self.interface.len().min(usize::from(u8::MAX))];
        out.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(name);
        out
    }
}

/// `PORT_HWCLOCK_NP`（linuxptp 4.0 から。16 バイト）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortHwclockNp {
    pub port: PortIdentity,
    /// ポートが使う PHC の番号（/dev/ptpN の N。ptp4l が PHC を合わせないとき（ソフトウェア・従来の方式の
    /// タイムスタンプ、`free_running 1`）は負）
    pub phc_index: i32,
    /// bit 0: 仮想 PHC（vclock）
    pub flags: u8,
}

impl PortHwclockNp {
    pub const VCLOCK: u8 = 1 << 0;
    const LEN: usize = 16;

    /// # Errors
    ///
    /// データが 16 バイトより短いとき。
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        let data = fixed::<{ Self::LEN }>(bytes)?;
        Ok(Self {
            port: read_port_identity(data, 0),
            phc_index: i32::from_be_bytes([data[10], data[11], data[12], data[13]]),
            flags: data[14],
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        push_port_identity(&mut out, self.port);
        out.extend_from_slice(&self.phc_index.to_be_bytes());
        out.push(self.flags);
        out.push(0);
        out
    }
}

/// 先頭の `N` バイト。足りなければエラー。
fn fixed<const N: usize>(data: &[u8]) -> Result<&[u8; N], DecodeError> {
    data.first_chunk::<N>().ok_or(DecodeError::DataTooShort {
        expected: N,
        actual: data.len(),
    })
}

fn read_identity(data: &[u8], at: usize) -> [u8; 8] {
    let mut identity = [0u8; 8];
    identity.copy_from_slice(&data[at..at + 8]);
    identity
}

fn read_i64(data: &[u8], at: usize) -> i64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&data[at..at + 8]);
    i64::from_be_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GM: [u8; 8] = [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56];

    /// linuxptp の `struct currentDS` の並び（`stepsRemoved`・`offsetFromMaster`・`meanPathDelay`）。
    #[test]
    fn reads_current_data_set() {
        #[rustfmt::skip]
        let data = [
            0x00, 0x01,                                      // stepsRemoved 1
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xF4, 0x00, 0x00,  // offsetFromMaster -12 ns
            0x00, 0x00, 0x00, 0x00, 0x04, 0xD2, 0x80, 0x00,  // meanPathDelay 1234.5 ns
        ];
        let current = CurrentDataSet::parse(&data).unwrap();
        assert_eq!(current.steps_removed, 1);
        assert_eq!(current.offset_from_master_scaled, -12i64 << 16i32);
        assert_eq!(current.mean_path_delay_scaled, (1234i64 << 16i32) + 0x8000);
        assert_eq!(current.to_bytes(), data);
    }

    /// linuxptp の `struct parentDS` の並び。
    #[test]
    fn reads_parent_data_set() {
        let mut data = vec![0xAA; 8];
        data.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x7F, 0xFF, 0xFF, 0xFF]);
        data.extend_from_slice(&[128, 6, 0x21, 0x4E, 0x5D, 128]);
        data.extend_from_slice(&GM);
        let parent = ParentDataSet::parse(&data).unwrap();
        assert_eq!(parent.parent.port, 1);
        assert_eq!(parent.grandmaster_priority1, 128);
        assert_eq!(
            parent.grandmaster_quality,
            ClockQuality {
                class: 6,
                accuracy: 0x21,
                offset_scaled_log_variance: 0x4E5D,
            }
        );
        assert_eq!(parent.grandmaster_identity, GM);
        assert_eq!(parent.to_bytes(), data);
    }

    #[test]
    fn reads_time_properties_and_port_data_set() {
        let time = TimePropertiesDataSet::parse(&[0x00, 37, 0b0001_1100, 0x20]).unwrap();
        assert_eq!(time.current_utc_offset, 37);
        assert_ne!(time.flags & TimePropertiesDataSet::PTP_TIMESCALE, 0);
        assert_ne!(time.flags & TimePropertiesDataSet::UTC_OFFSET_VALID, 0);
        assert_eq!(time.flags & TimePropertiesDataSet::LEAP_61, 0);
        assert_eq!(time.time_source, 0x20);

        let mut data = GM.to_vec();
        data.extend_from_slice(&[0x00, 0x01, 9, 0]);
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(&[1, 3, 0, 1, 0, 2]);
        let port = PortDataSet::parse(&data).unwrap();
        assert_eq!(port.port.port, 1);
        assert_eq!(port.port_state, 9);
        assert_eq!(port.announce_receipt_timeout, 3);
        assert_eq!(port.delay_mechanism, 1);
        assert_eq!(port.version, 2);
        assert_eq!(port.to_bytes(), data);
    }

    #[test]
    fn reads_default_data_set() {
        let mut data = vec![0x01, 0x00, 0x00, 0x02, 128, 248, 0xFE, 0xFF, 0xFF, 128];
        data.extend_from_slice(&GM);
        data.extend_from_slice(&[24, 0]);
        let default = DefaultDataSet::parse(&data).unwrap();
        assert_eq!(default.number_ports, 2);
        assert_eq!(default.quality.class, 248);
        assert_eq!(default.clock_identity, GM);
        assert_eq!(default.domain, 24);
        assert_eq!(default.to_bytes(), data);
    }

    /// linuxptp の `struct port_properties_np`: `portIdentity`・`port_state`・`timestamping`・`PTPText`。
    /// ptp4l は全体を偶数バイトにそろえるので、後ろに 0 が 1 バイト付くことがある。
    #[test]
    fn reads_port_properties_with_padding() {
        let mut data = GM.to_vec();
        data.extend_from_slice(&[0x00, 0x01, 9, 1, 4]);
        data.extend_from_slice(b"eno1");
        data.push(0);
        let properties = PortPropertiesNp::parse(&data).unwrap();
        assert_eq!(properties.port_state, 9);
        assert_eq!(properties.timestamping, 1);
        assert_eq!(properties.interface, b"eno1");
        assert_eq!(properties.to_bytes(), data[..data.len() - 1]);

        // 名前が途中で切れている
        assert_eq!(
            PortPropertiesNp::parse(&data[..15]),
            Err(DecodeError::DataTooShort {
                expected: 17,
                actual: 15,
            })
        );
    }

    /// linuxptp の `struct port_hwclock_np`: `portIdentity`・`phc_index`（`Integer32`）・`flags`・`reserved`。
    #[test]
    fn reads_port_hwclock() {
        let mut data = GM.to_vec();
        data.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00]);
        let hwclock = PortHwclockNp::parse(&data).unwrap();
        assert_eq!(hwclock.phc_index, 2i32);
        assert_eq!(hwclock.flags & PortHwclockNp::VCLOCK, PortHwclockNp::VCLOCK);
        assert_eq!(hwclock.to_bytes(), data);
        let software = PortHwclockNp {
            phc_index: -1i32,
            ..hwclock
        };
        assert_eq!(
            PortHwclockNp::parse(&software.to_bytes())
                .unwrap()
                .phc_index,
            -1i32
        );
    }

    #[test]
    fn short_data_is_an_error_for_every_data_set() {
        let too_short = |result: Result<(), DecodeError>, expected: usize| {
            assert_eq!(
                result,
                Err(DecodeError::DataTooShort {
                    expected,
                    actual: expected - 1,
                })
            );
        };
        too_short(DefaultDataSet::parse(&[0; 19]).map(drop), 20);
        too_short(CurrentDataSet::parse(&[0; 17]).map(drop), 18);
        too_short(ParentDataSet::parse(&[0; 31]).map(drop), 32);
        too_short(TimePropertiesDataSet::parse(&[0; 3]).map(drop), 4);
        too_short(PortDataSet::parse(&[0; 25]).map(drop), 26);
        too_short(PortPropertiesNp::parse(&[0; 12]).map(drop), 13);
        too_short(PortHwclockNp::parse(&[0; 15]).map(drop), 16);
    }
}
