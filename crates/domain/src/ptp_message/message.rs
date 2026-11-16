//! メッセージの本体と、メッセージ全体の組み立て・解析。
//!
//! 本体の配置（オフセットはメッセージの先頭から）:
//! - Sync・`Delay_Req`・`Follow_Up`: 34〜43 に時刻（`originTimestamp`・`preciseOriginTimestamp`）。全体 44 バイト
//! - `Delay_Resp`: 34〜43 に `receiveTimestamp`、44〜53 に `requestingPortIdentity`。全体 54 バイト
//! - Announce: 34〜43 `originTimestamp`、44〜45 `currentUtcOffset`、46 予約、47 `grandmasterPriority1`、
//!   48〜51 `grandmasterClockQuality`、52 `grandmasterPriority2`、53〜60 `grandmasterIdentity`、
//!   61〜62 `stepsRemoved`、63 `timeSource`。全体 64 バイト
//!
//! 本体の後ろの TLV（Announce の `PATH_TRACE` など）は読まない（`messageLength` には含まれる）。

use super::header::{
    Correction, Flags, Header, PortIdentity, PtpTimestamp, push_port_identity, read_port_identity,
    read_u16,
};

/// `messageType`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageType {
    Sync,
    DelayReq,
    PdelayReq,
    PdelayResp,
    FollowUp,
    DelayResp,
    PdelayRespFollowUp,
    Announce,
    Signaling,
    Management,
    /// 規格にない値（下位 4 bit）
    Other(u8),
}

impl MessageType {
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0x0F {
            0x0 => Self::Sync,
            0x1 => Self::DelayReq,
            0x2 => Self::PdelayReq,
            0x3 => Self::PdelayResp,
            0x8 => Self::FollowUp,
            0x9 => Self::DelayResp,
            0xA => Self::PdelayRespFollowUp,
            0xB => Self::Announce,
            0xC => Self::Signaling,
            0xD => Self::Management,
            other => Self::Other(other),
        }
    }

    #[must_use]
    pub const fn to_bits(self) -> u8 {
        match self {
            Self::Sync => 0x0,
            Self::DelayReq => 0x1,
            Self::PdelayReq => 0x2,
            Self::PdelayResp => 0x3,
            Self::FollowUp => 0x8,
            Self::DelayResp => 0x9,
            Self::PdelayRespFollowUp => 0xA,
            Self::Announce => 0xB,
            Self::Signaling => 0xC,
            Self::Management => 0xD,
            Self::Other(bits) => bits & 0x0F,
        }
    }

    /// `controlField`（IEEE 1588-2008 の表 23。v2 の受け手は見ないが、v1 との互換のために送る）。
    const fn control(self) -> u8 {
        match self {
            Self::Sync => 0x00,
            Self::DelayReq => 0x01,
            Self::FollowUp => 0x02,
            Self::DelayResp => 0x03,
            Self::Management => 0x04,
            Self::PdelayReq
            | Self::PdelayResp
            | Self::PdelayRespFollowUp
            | Self::Announce
            | Self::Signaling
            | Self::Other(_) => 0x05,
        }
    }
}

/// Announce の本体（マスタとグランドマスタの情報）。時刻系などのフラグはヘッダの `flagField` にある。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Announce {
    pub origin: PtpTimestamp,
    /// TAI − UTC（秒）
    pub current_utc_offset: i16,
    pub grandmaster_priority1: u8,
    pub grandmaster_class: u8,
    pub grandmaster_accuracy: u8,
    pub grandmaster_variance: u16,
    pub grandmaster_priority2: u8,
    pub grandmaster_identity: [u8; 8],
    pub steps_removed: u16,
    pub time_source: u8,
}

impl Announce {
    const LEN: usize = 30;

    fn read(data: &[u8; Self::LEN]) -> Self {
        let mut identity = [0u8; 8];
        identity.copy_from_slice(&data[19..27]);
        Self {
            origin: read_timestamp(data, 0),
            current_utc_offset: read_u16(data, 10).cast_signed(),
            grandmaster_priority1: data[13],
            grandmaster_class: data[14],
            grandmaster_accuracy: data[15],
            grandmaster_variance: read_u16(data, 16),
            grandmaster_priority2: data[18],
            grandmaster_identity: identity,
            steps_removed: read_u16(data, 27),
            time_source: data[29],
        }
    }

    fn push(&self, out: &mut Vec<u8>) {
        self.origin.push(out);
        out.extend_from_slice(&self.current_utc_offset.to_be_bytes());
        out.push(0); // reserved
        out.push(self.grandmaster_priority1);
        out.push(self.grandmaster_class);
        out.push(self.grandmaster_accuracy);
        out.extend_from_slice(&self.grandmaster_variance.to_be_bytes());
        out.push(self.grandmaster_priority2);
        out.extend_from_slice(&self.grandmaster_identity);
        out.extend_from_slice(&self.steps_removed.to_be_bytes());
        out.push(self.time_source);
    }
}

/// メッセージの本体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// one-step なら送った時刻、two-step なら目安の時刻（正確な時刻は `Follow_Up`）
    Sync {
        origin: PtpTimestamp,
    },
    /// スレーブが送る。時刻はスレーブの送信の目安（マスタは使わない）
    DelayReq {
        origin: PtpTimestamp,
    },
    /// two-step の Sync を送った時刻
    FollowUp {
        precise_origin: PtpTimestamp,
    },
    /// マスタが `Delay_Req` を受け取った時刻と、その `Delay_Req` を送ったポート
    DelayResp {
        receive: PtpTimestamp,
        requesting: PortIdentity,
    },
    Announce(Announce),
    /// 解釈しない種類。`data` は共通ヘッダの後ろ（`messageLength` まで）
    Other {
        message_type: MessageType,
        data: Vec<u8>,
    },
}

impl Body {
    #[must_use]
    pub const fn message_type(&self) -> MessageType {
        match *self {
            Self::Sync { .. } => MessageType::Sync,
            Self::DelayReq { .. } => MessageType::DelayReq,
            Self::FollowUp { .. } => MessageType::FollowUp,
            Self::DelayResp { .. } => MessageType::DelayResp,
            Self::Announce(_) => MessageType::Announce,
            Self::Other { message_type, .. } => message_type,
        }
    }
}

/// メッセージ 1 つ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtpMessage {
    pub header: Header,
    pub body: Body,
}

impl PtpMessage {
    /// スレーブの `Delay_Req`。時刻は 0（マスタは使わない。ptp4l も送信の目安を入れるだけ）、補正は 0、
    /// 間隔は「指定なし」。`unicast` ならユニキャストのフラグを立てる（マスタはユニキャストで応える）。
    #[must_use]
    pub const fn delay_req(
        domain: u8,
        source: PortIdentity,
        sequence_id: u16,
        unicast: bool,
    ) -> Self {
        Self {
            header: Header {
                transport_specific: 0,
                version: Header::VERSION_2,
                domain,
                minor_sdo_id: 0,
                flags: if unicast { Flags::UNICAST } else { Flags(0) },
                correction: Correction(0),
                source,
                sequence_id,
                log_message_interval: Header::LOG_INTERVAL_UNSPECIFIED,
            },
            body: Body::DelayReq {
                origin: PtpTimestamp {
                    seconds: 0,
                    nanos: 0,
                },
            },
        }
    }

    #[must_use]
    pub const fn message_type(&self) -> MessageType {
        self.body.message_type()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("PTP のメッセージとしては短すぎます（{0} バイト）")]
    TooShort(usize),
    #[error("対応していない PTP のバージョンです（{0}）")]
    UnsupportedVersion(u8),
    #[error("messageLength（{declared}）が受信した長さ（{actual} バイト）と合いません")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("{message_type:?} の本体が短すぎます（{actual} バイト。{expected} バイト必要）")]
    BodyTooShort {
        message_type: MessageType,
        expected: usize,
        actual: usize,
    },
}

/// メッセージをバイト列にする（`messageLength` と `controlField` は本体の種類から決める）。
#[must_use]
pub fn encode(message: &PtpMessage) -> Vec<u8> {
    let message_type = message.message_type();
    let mut body = Vec::with_capacity(Announce::LEN);
    // Other のバイト列を借りずに取り出すため、本体は複製して照合する（参照のままだと ref のパターンが要る）
    match message.body.clone() {
        Body::Sync { origin } | Body::DelayReq { origin } => origin.push(&mut body),
        Body::FollowUp { precise_origin } => precise_origin.push(&mut body),
        Body::DelayResp {
            receive,
            requesting,
        } => {
            receive.push(&mut body);
            push_port_identity(&mut body, requesting);
        }
        Body::Announce(announce) => announce.push(&mut body),
        Body::Other { data, .. } => body.extend_from_slice(&data),
    }
    let header = &message.header;
    let total = Header::LEN + body.len();
    let mut out = Vec::with_capacity(total);
    out.push((header.transport_specific << 4u8) | message_type.to_bits());
    out.push(header.version);
    out.extend_from_slice(&u16::try_from(total).unwrap_or(u16::MAX).to_be_bytes());
    out.push(header.domain);
    out.push(header.minor_sdo_id);
    out.extend_from_slice(&header.flags.0.to_be_bytes());
    out.extend_from_slice(&header.correction.0.to_be_bytes());
    out.extend_from_slice(&[0; 4]); // messageTypeSpecific
    push_port_identity(&mut out, header.source);
    out.extend_from_slice(&header.sequence_id.to_be_bytes());
    out.push(message_type.control());
    out.push(header.log_message_interval.cast_unsigned());
    out.extend_from_slice(&body);
    out
}

/// バイト列を PTP のメッセージとして読む。`messageLength` より後ろのバイトは無視する。
///
/// # Errors
///
/// 短すぎる、`versionPTP` が 2 でない、`messageLength` が合わない、本体が種類に対して短いとき。
pub fn decode(bytes: &[u8]) -> Result<PtpMessage, DecodeError> {
    let Some(head) = bytes.first_chunk::<{ Header::LEN }>() else {
        return Err(DecodeError::TooShort(bytes.len()));
    };
    if head[1] & 0x0F != Header::VERSION_2 {
        return Err(DecodeError::UnsupportedVersion(head[1]));
    }
    let declared = usize::from(read_u16(head, 2));
    if declared < Header::LEN || declared > bytes.len() {
        return Err(DecodeError::LengthMismatch {
            declared,
            actual: bytes.len(),
        });
    }
    let message_type = MessageType::from_bits(head[0]);
    let mut correction = [0u8; 8];
    correction.copy_from_slice(&head[8..16]);
    let header = Header {
        transport_specific: head[0] >> 4u8,
        version: head[1],
        domain: head[4],
        minor_sdo_id: head[5],
        flags: Flags(read_u16(head, 6)),
        correction: Correction(i64::from_be_bytes(correction)),
        source: read_port_identity(head, 20),
        sequence_id: read_u16(head, 30),
        log_message_interval: head[33].cast_signed(),
    };
    let data = &bytes[Header::LEN..declared];
    let body = match message_type {
        MessageType::Sync => Body::Sync {
            origin: read_timestamp(fixed::<{ PtpTimestamp::LEN }>(message_type, data)?, 0),
        },
        MessageType::DelayReq => Body::DelayReq {
            origin: read_timestamp(fixed::<{ PtpTimestamp::LEN }>(message_type, data)?, 0),
        },
        MessageType::FollowUp => Body::FollowUp {
            precise_origin: read_timestamp(fixed::<{ PtpTimestamp::LEN }>(message_type, data)?, 0),
        },
        MessageType::DelayResp => {
            let fields = fixed::<{ PtpTimestamp::LEN + 10 }>(message_type, data)?;
            Body::DelayResp {
                receive: read_timestamp(fields, 0),
                requesting: read_port_identity(fields, PtpTimestamp::LEN),
            }
        }
        MessageType::Announce => Body::Announce(Announce::read(fixed::<{ Announce::LEN }>(
            message_type,
            data,
        )?)),
        MessageType::PdelayReq
        | MessageType::PdelayResp
        | MessageType::PdelayRespFollowUp
        | MessageType::Signaling
        | MessageType::Management
        | MessageType::Other(_) => Body::Other {
            message_type,
            data: data.to_vec(),
        },
    };
    Ok(PtpMessage { header, body })
}

/// 本体の先頭 `N` バイト。足りなければエラー。
fn fixed<const N: usize>(message_type: MessageType, data: &[u8]) -> Result<&[u8; N], DecodeError> {
    data.first_chunk::<N>().ok_or(DecodeError::BodyTooShort {
        message_type,
        expected: Header::LEN + N,
        actual: Header::LEN + data.len(),
    })
}

/// `at` から 10 バイトを時刻として読む。呼び出し側が長さを確かめておくこと。
fn read_timestamp(bytes: &[u8], at: usize) -> PtpTimestamp {
    let mut raw = [0u8; PtpTimestamp::LEN];
    raw.copy_from_slice(&bytes[at..at + PtpTimestamp::LEN]);
    PtpTimestamp::read(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str;

    const SLAVE: PortIdentity = PortIdentity {
        clock: [0x02, 0x11, 0x22, 0xFF, 0xFE, 0x33, 0x44, 0x55],
        port: 1,
    };
    const MASTER: PortIdentity = PortIdentity {
        clock: [0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56],
        port: 1,
    };

    fn header(sequence_id: u16, flags: Flags, log_message_interval: i8) -> Header {
        Header {
            transport_specific: 0,
            version: Header::VERSION_2,
            domain: 0,
            minor_sdo_id: 0,
            flags,
            correction: Correction(0),
            source: MASTER,
            sequence_id,
            log_message_interval,
        }
    }

    /// スレーブの `Delay_Req`: messageType 1・長さ 44・controlField 1・logMessageInterval 0x7F・補正 0。
    #[test]
    fn encodes_a_delay_req() {
        let bytes = encode(&PtpMessage::delay_req(24, SLAVE, 0x0102, false));
        #[rustfmt::skip]
        let expected: [u8; 44] = [
            0x01, 0x02, 0x00, 44, 24, 0, 0x00, 0x00,          // Delay_Req・version 2・長さ 44・ドメイン 24・フラグなし
            0, 0, 0, 0, 0, 0, 0, 0,                           // correctionField
            0, 0, 0, 0,                                       // messageTypeSpecific
            0x02, 0x11, 0x22, 0xFF, 0xFE, 0x33, 0x44, 0x55, 0x00, 0x01, // sourcePortIdentity
            0x01, 0x02, 0x01, 0x7F,                           // sequenceId・controlField・logMessageInterval
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,                     // originTimestamp
        ];
        assert_eq!(bytes, expected);
        let unicast = encode(&PtpMessage::delay_req(24, SLAVE, 0x0102, true));
        assert_eq!(&unicast[6..8], [0x04, 0x00], "ユニキャストのフラグ");
        assert_eq!(
            decode(&bytes).unwrap(),
            PtpMessage::delay_req(24, SLAVE, 0x0102, false)
        );
    }

    #[test]
    fn round_trips_every_known_message() {
        let stamp = PtpTimestamp {
            seconds: 1_794_873_637,
            nanos: 123_456_789,
        };
        let announce = Announce {
            origin: stamp,
            current_utc_offset: 37,
            grandmaster_priority1: 128,
            grandmaster_class: 6,
            grandmaster_accuracy: 0x21,
            grandmaster_variance: 0x4E5D,
            grandmaster_priority2: 128,
            grandmaster_identity: MASTER.clock,
            steps_removed: 0,
            time_source: 0x20,
        };
        let messages = [
            (Body::Sync { origin: stamp }, 44, 0x00),
            (
                Body::FollowUp {
                    precise_origin: stamp,
                },
                44,
                0x02,
            ),
            (
                Body::DelayResp {
                    receive: stamp,
                    requesting: SLAVE,
                },
                54,
                0x03,
            ),
            (Body::Announce(announce), 64, 0x05),
        ];
        for (body, len, control) in messages {
            let mut message = PtpMessage {
                header: header(7, Flags::TWO_STEP, 0),
                body,
            };
            message.header.correction = Correction(-(1500 << 16));
            let bytes = encode(&message);
            assert_eq!(bytes.len(), len, "{:?}", message.message_type());
            assert_eq!(bytes[32], control, "{:?}", message.message_type());
            assert_eq!(decode(&bytes).unwrap(), message);
        }
    }

    /// ptp4l の Announce（PTP 2.1・`PATH_TRACE` の TLV 付き）。TLV は読まずに本体だけを読む。
    #[test]
    fn decodes_an_announce_with_a_tlv() {
        #[rustfmt::skip]
        let bytes: [u8; 76] = [
            0x0B, 0x12, 0x00, 76, 0, 0, 0x00, 0x0C,             // Announce・2.1・長さ 76・ptpTimescale と UTC 有効
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56, 0x00, 0x01,
            0x00, 0x05, 0x05, 0x01,                             // sequenceId 5・logMessageInterval 1（2 秒）
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,                       // originTimestamp
            0x00, 37, 0, 128, 6, 0x21, 0x4E, 0x5D, 128,         // UTC との差 37・priority1・品質・priority2
            0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56,     // grandmasterIdentity
            0x00, 0x00, 0x20,                                   // stepsRemoved 0・GNSS
            0x00, 0x08, 0x00, 0x08,                             // PATH_TRACE（長さ 8）
            0x00, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56,
        ];
        let message = decode(&bytes).unwrap();
        assert_eq!(message.header.version, 0x12);
        assert_eq!(message.header.log_message_interval, 1);
        assert!(message.header.flags.contains(Flags::PTP_TIMESCALE));
        let Body::Announce(announce) = message.body else {
            panic!("Announce のはず");
        };
        assert_eq!(announce.current_utc_offset, 37);
        assert_eq!(announce.grandmaster_class, 6);
        assert_eq!(announce.grandmaster_identity, MASTER.clock);
        assert_eq!(announce.time_source, 0x20);
    }

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// linuxptp 4.0 の ptp4l（マスタ専用・ソフトウェアのタイムスタンプ・two-step）が送ったメッセージと、
    /// それに問い合わせた ntpterminal の `Delay_Req`（WSL の netns で tcpdump で記録したもの）。
    #[test]
    fn decodes_messages_captured_from_ptp4l() {
        let master = PortIdentity {
            clock: [0x7A, 0x64, 0x36, 0xFF, 0xFE, 0x25, 0xF9, 0xDD],
            port: 1,
        };
        let slave = PortIdentity {
            clock: [0x96, 0xE2, 0x22, 0x08, 0x15, 0x0E, 0x83, 0xFC],
            port: 1,
        };
        let sync = decode(&hex(
            "0012002c000002000000000000000000000000007a6436fffe25f9dd0001000000ff00000000000000000000",
        ))
        .unwrap();
        assert_eq!(sync.header.version, 0x12);
        assert_eq!(sync.header.source, master);
        assert!(sync.header.flags.contains(Flags::TWO_STEP));
        assert_eq!(sync.header.log_message_interval, -1);
        assert!(matches!(sync.body, Body::Sync { .. }));

        let follow_up = decode(&hex(
            "0812002c000000000000000000000000000000007a6436fffe25f9dd0001000000ff00006afb53c1281fa678",
        ))
        .unwrap();
        assert_eq!(
            follow_up.body,
            Body::FollowUp {
                precise_origin: PtpTimestamp {
                    seconds: 0x6AFB_53C1,
                    nanos: 0x281F_A678,
                },
            }
        );

        let response = decode(&hex(
            "09120036000000000000000000000000000000007a6436fffe25f9dd0001000000ff00006afb53c32829daca96e22208150e83fc0001",
        ))
        .unwrap();
        assert_eq!(
            response.header.log_message_interval, -1,
            "logMinDelayReqInterval"
        );
        let Body::DelayResp { requesting, .. } = response.body else {
            panic!("Delay_Resp のはず");
        };
        assert_eq!(requesting, slave);

        let announce = decode(&hex(
            "0b120040000000000000000000000000000000007a6436fffe25f9dd0001000000000000000000000000000000250080f8feffff807a6436fffe25f9dd0000a0",
        ))
        .unwrap();
        // ソフトウェアのタイムスタンプの ptp4l は、時刻系を ARB とし、UTC との差を確かでないとする
        assert!(!announce.header.flags.contains(Flags::PTP_TIMESCALE));
        assert!(!announce.header.flags.contains(Flags::UTC_OFFSET_VALID));
        let Body::Announce(body) = announce.body else {
            panic!("Announce のはず");
        };
        assert_eq!(body.current_utc_offset, 37);
        assert_eq!(body.grandmaster_class, 248);
        assert_eq!(body.grandmaster_identity, master.clock);
        assert_eq!(body.time_source, 0xA0);

        // ntpterminal が送った Delay_Req は、同じ組み立てでそのまま作れる
        let request = hex(
            "0102002c0000000000000000000000000000000096e22208150e83fc00010000017f00000000000000000000",
        );
        assert_eq!(encode(&PtpMessage::delay_req(0, slave, 0, false)), request);
    }

    #[test]
    fn other_types_keep_their_bytes() {
        let message = PtpMessage {
            header: header(1, Flags(0), 0),
            body: Body::Other {
                message_type: MessageType::PdelayReq,
                data: vec![0; 20],
            },
        };
        let bytes = encode(&message);
        assert_eq!(bytes[0], 0x02);
        assert_eq!(decode(&bytes).unwrap(), message);
    }

    #[test]
    fn rejects_bad_input_without_panicking() {
        let good = encode(&PtpMessage {
            header: header(1, Flags(0), 0),
            body: Body::DelayResp {
                receive: PtpTimestamp::default(),
                requesting: SLAVE,
            },
        });
        let mut v1 = good.clone();
        v1[1] = 0x01;
        assert_eq!(decode(&v1), Err(DecodeError::UnsupportedVersion(1)));
        let mut longer = good.clone();
        longer[3] = 60;
        assert!(matches!(
            decode(&longer),
            Err(DecodeError::LengthMismatch { declared: 60, .. })
        ));
        // 長さを本体の途中までにすると、本体が短い
        let mut short = good.clone();
        short[3] = 50;
        assert!(matches!(
            decode(&short),
            Err(DecodeError::BodyTooShort {
                message_type: MessageType::DelayResp,
                expected: 54,
                actual: 50,
            })
        ));
        // 後ろに余分なバイトがあっても、messageLength までを読む
        let mut padded = good.clone();
        padded.extend_from_slice(&[0xAA; 6]);
        assert_eq!(decode(&padded), decode(&good));
        for len in 0..good.len() {
            assert!(decode(&good[..len]).is_err(), "{len} バイト");
        }
    }
}
