//! 管理メッセージの組み立てと解析。
//!
//! 配置（オフセットはメッセージの先頭から）:
//! - 0〜33: 共通ヘッダ（`messageType`・`versionPTP`・`messageLength`・`domainNumber`・`sourcePortIdentity`・
//!   `sequenceId` など）
//! - 34〜47: 管理メッセージの本体（`targetPortIdentity`・`startingBoundaryHops`・`boundaryHops`・`actionField`）
//! - 48〜: TLV（`tlvType`・`lengthField`・値）。`lengthField` は偶数でなければならない（ptp4l は奇数を捨てる）

use core::fmt;

/// `messageType`: 管理メッセージ
const MESSAGE_TYPE_MANAGEMENT: u8 = 0xD;
/// `versionPTP` の下位 4 bit（上位 4 bit は `minorVersionPTP`。ptp4l は下位だけを確かめる）
const MAJOR_VERSION: u8 = 2;
/// `controlField`: 管理メッセージ
const CONTROL_MANAGEMENT: u8 = 0x04;
/// `logMessageInterval`: 管理メッセージでは 0x7F
const LOG_MESSAGE_INTERVAL_NONE: u8 = 0x7F;
const HEADER_LEN: usize = 34;
/// 共通ヘッダ + 管理メッセージの本体
const BODY_END: usize = 48;
/// TLV の `tlvType` と `lengthField`
const TLV_HEADER_LEN: usize = 4;
const TLV_MANAGEMENT: u16 = 0x0001;
const TLV_MANAGEMENT_ERROR_STATUS: u16 = 0x0002;
/// `MANAGEMENT_ERROR_STATUS` の値のうち、`displayData` の前まで（`managementErrorId`・`managementId`・`reserved`）
const ERROR_STATUS_FIXED_LEN: usize = 8;

/// ポートの識別子（`clockIdentity` 8 バイトとポート番号）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PortIdentity {
    pub clock: [u8; 8],
    pub port: u16,
}

impl PortIdentity {
    /// どの時計のどのポートにも当てはまる宛先（`clockIdentity` がすべて 0xFF、ポート番号 0xFFFF）。
    pub const WILDCARD: Self = Self {
        clock: [0xFF; 8],
        port: 0xFFFF,
    };

    /// どの時計でもよく、ポート番号だけを指定する宛先。
    #[must_use]
    pub const fn port_of_any_clock(port: u16) -> Self {
        Self {
            clock: [0xFF; 8],
            port,
        }
    }
}

/// `actionField`（管理メッセージの動作）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Get,
    Set,
    Response,
    Command,
    Acknowledge,
    /// 規格にない値（下位 4 bit）
    Other(u8),
}

impl Action {
    #[must_use]
    pub const fn to_bits(self) -> u8 {
        match self {
            Self::Get => 0,
            Self::Set => 1,
            Self::Response => 2,
            Self::Command => 3,
            Self::Acknowledge => 4,
            Self::Other(bits) => bits & 0x0F,
        }
    }

    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0x0F {
            0 => Self::Get,
            1 => Self::Set,
            2 => Self::Response,
            3 => Self::Command,
            4 => Self::Acknowledge,
            other => Self::Other(other),
        }
    }
}

/// `managementId`（どのデータを読み書きするか）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ManagementId(pub u16);

impl ManagementId {
    pub const DEFAULT_DATA_SET: Self = Self(0x2000);
    pub const CURRENT_DATA_SET: Self = Self(0x2001);
    pub const PARENT_DATA_SET: Self = Self(0x2002);
    pub const TIME_PROPERTIES_DATA_SET: Self = Self(0x2003);
    pub const PORT_DATA_SET: Self = Self(0x2004);
    /// linuxptp 独自: ポートのインターフェース名とタイムスタンプの方式
    pub const PORT_PROPERTIES_NP: Self = Self(0xC004);
    /// linuxptp 独自: ポートが使う PHC の番号（linuxptp 4.0 から）
    pub const PORT_HWCLOCK_NP: Self = Self(0xC009);
}

impl fmt::Display for ManagementId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            Self::DEFAULT_DATA_SET => "DEFAULT_DATA_SET",
            Self::CURRENT_DATA_SET => "CURRENT_DATA_SET",
            Self::PARENT_DATA_SET => "PARENT_DATA_SET",
            Self::TIME_PROPERTIES_DATA_SET => "TIME_PROPERTIES_DATA_SET",
            Self::PORT_DATA_SET => "PORT_DATA_SET",
            Self::PORT_PROPERTIES_NP => "PORT_PROPERTIES_NP",
            Self::PORT_HWCLOCK_NP => "PORT_HWCLOCK_NP",
            Self(other) => return write!(f, "0x{other:04X}"),
        };
        f.write_str(name)
    }
}

/// `managementErrorId`（`MANAGEMENT_ERROR_STATUS` の理由）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagementErrorId {
    ResponseTooBig,
    /// その managementId を知らない
    NoSuchId,
    WrongLength,
    /// 値が正しくない（ptp4l は、存在しないポート番号への問い合わせにもこれを返す）
    WrongValue,
    NotSetable,
    /// 対応していない（ptp4l は、読み取り専用のソケットで `PORT_PROPERTIES_NP` などを問い合わせたときにも返す）
    NotSupported,
    GeneralError,
    Other(u16),
}

impl ManagementErrorId {
    #[must_use]
    pub const fn to_bits(self) -> u16 {
        match self {
            Self::ResponseTooBig => 0x0001,
            Self::NoSuchId => 0x0002,
            Self::WrongLength => 0x0003,
            Self::WrongValue => 0x0004,
            Self::NotSetable => 0x0005,
            Self::NotSupported => 0x0006,
            Self::GeneralError => 0xFFFE,
            Self::Other(bits) => bits,
        }
    }

    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        match bits {
            0x0001 => Self::ResponseTooBig,
            0x0002 => Self::NoSuchId,
            0x0003 => Self::WrongLength,
            0x0004 => Self::WrongValue,
            0x0005 => Self::NotSetable,
            0x0006 => Self::NotSupported,
            0xFFFE => Self::GeneralError,
            other => Self::Other(other),
        }
    }
}

impl fmt::Display for ManagementErrorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ResponseTooBig => f.write_str("RESPONSE_TOO_BIG"),
            Self::NoSuchId => f.write_str("NO_SUCH_ID"),
            Self::WrongLength => f.write_str("WRONG_LENGTH"),
            Self::WrongValue => f.write_str("WRONG_VALUE"),
            Self::NotSetable => f.write_str("NOT_SETABLE"),
            Self::NotSupported => f.write_str("NOT_SUPPORTED"),
            Self::GeneralError => f.write_str("GENERAL_ERROR"),
            Self::Other(bits) => write!(f, "0x{bits:04X}"),
        }
    }
}

/// 管理メッセージの TLV。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagementTlv {
    /// `MANAGEMENT`。GET の問い合わせではデータは空（ptp4l は空のデータを受け付ける）
    Management { id: ManagementId, data: Vec<u8> },
    /// `MANAGEMENT_ERROR_STATUS`（問い合わせへのエラー応答）。`display` は `displayData` の文字列
    ErrorStatus {
        error: ManagementErrorId,
        id: ManagementId,
        display: Vec<u8>,
    },
    /// ほかの種類の TLV（値はそのまま）
    Other { tlv_type: u16, value: Vec<u8> },
}

/// 管理メッセージ 1 つ（TLV は 1 つだけ）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagementMessage {
    /// `transportSpecific`（`majorSdoId`。先頭のバイトの上位 4 bit）。gPTP の設定の ptp4l は 1 でないと捨てる
    pub transport_specific: u8,
    /// `versionPTP` のバイト（上位 4 bit が `minorVersionPTP`、下位 4 bit が `versionPTP`）
    pub version: u8,
    /// ptp4l は自分のドメイン番号と違うメッセージを捨てる（管理用ソケットでも）
    pub domain: u8,
    pub source: PortIdentity,
    pub sequence_id: u16,
    pub target: PortIdentity,
    pub starting_boundary_hops: u8,
    /// 1 以上だと、ptp4l は問い合わせをネットワークのポートへも転送する
    pub boundary_hops: u8,
    pub action: Action,
    pub tlv: ManagementTlv,
}

impl ManagementMessage {
    /// GET の問い合わせ。`boundaryHops` は 0（ptp4l の外へ転送させない）、データは空にする。
    #[must_use]
    pub const fn get(
        domain: u8,
        source: PortIdentity,
        sequence_id: u16,
        target: PortIdentity,
        id: ManagementId,
    ) -> Self {
        Self {
            transport_specific: 0,
            version: MAJOR_VERSION,
            domain,
            source,
            sequence_id,
            target,
            starting_boundary_hops: 0,
            boundary_hops: 0,
            action: Action::Get,
            tlv: ManagementTlv::Management {
                id,
                data: Vec::new(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("管理メッセージとしては短すぎます（{0} バイト）")]
    TooShort(usize),
    #[error("管理メッセージではありません（messageType {0}）")]
    NotManagement(u8),
    #[error("対応していない PTP のバージョンです（{0}）")]
    UnsupportedVersion(u8),
    #[error("messageLength（{declared}）が受信した長さ（{actual} バイト）と合いません")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("TLV の長さが正しくありません")]
    BadTlvLength,
    #[error("データセットが短すぎます（{actual} バイト。{expected} バイト必要）")]
    DataTooShort { expected: usize, actual: usize },
}

/// メッセージをバイト列にする。TLV の値が奇数バイトなら、0 を 1 バイト足して偶数にする。
#[must_use]
pub fn encode(message: &ManagementMessage) -> Vec<u8> {
    // 値を組み立てるので、TLV は複製して取り出す（参照のままだと ref のパターンが要る）
    let (tlv_type, mut value) = match message.tlv.clone() {
        ManagementTlv::Management { id, data } => {
            let mut value = id.0.to_be_bytes().to_vec();
            value.extend_from_slice(&data);
            (TLV_MANAGEMENT, value)
        }
        ManagementTlv::ErrorStatus { error, id, display } => {
            let mut value = Vec::with_capacity(ERROR_STATUS_FIXED_LEN + 1 + display.len());
            value.extend_from_slice(&error.to_bits().to_be_bytes());
            value.extend_from_slice(&id.0.to_be_bytes());
            value.extend_from_slice(&[0; 4]);
            value.push(u8::try_from(display.len()).unwrap_or(u8::MAX));
            value.extend(display.iter().take(usize::from(u8::MAX)));
            (TLV_MANAGEMENT_ERROR_STATUS, value)
        }
        ManagementTlv::Other { tlv_type, value } => (tlv_type, value),
    };
    if value.len() % 2 != 0 {
        value.push(0);
    }
    let total = BODY_END + TLV_HEADER_LEN + value.len();
    let mut out = Vec::with_capacity(total);
    out.push((message.transport_specific << 4u8) | MESSAGE_TYPE_MANAGEMENT);
    out.push(message.version);
    out.extend_from_slice(&u16::try_from(total).unwrap_or(u16::MAX).to_be_bytes());
    out.push(message.domain);
    out.push(0); // minorSdoId（reserved）
    out.extend_from_slice(&[0; 2]); // flagField
    out.extend_from_slice(&[0; 8]); // correctionField
    out.extend_from_slice(&[0; 4]); // messageTypeSpecific
    push_port_identity(&mut out, message.source);
    out.extend_from_slice(&message.sequence_id.to_be_bytes());
    out.push(CONTROL_MANAGEMENT);
    out.push(LOG_MESSAGE_INTERVAL_NONE);
    push_port_identity(&mut out, message.target);
    out.push(message.starting_boundary_hops);
    out.push(message.boundary_hops);
    out.push(message.action.to_bits());
    out.push(0); // reserved
    out.extend_from_slice(&tlv_type.to_be_bytes());
    out.extend_from_slice(&u16::try_from(value.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&value);
    out
}

/// バイト列を管理メッセージとして読む。`messageLength` より後ろのバイトは無視する。
///
/// # Errors
///
/// 管理メッセージでない、`versionPTP` が 2 でない、長さが足りない・合わないとき。
pub fn decode(bytes: &[u8]) -> Result<ManagementMessage, DecodeError> {
    let Some(head) = bytes.first_chunk::<{ BODY_END + TLV_HEADER_LEN }>() else {
        return Err(DecodeError::TooShort(bytes.len()));
    };
    let message_type = head[0] & 0x0F;
    if message_type != MESSAGE_TYPE_MANAGEMENT {
        return Err(DecodeError::NotManagement(message_type));
    }
    if head[1] & 0x0F != MAJOR_VERSION {
        return Err(DecodeError::UnsupportedVersion(head[1]));
    }
    let declared = usize::from(read_u16(head, 2));
    if declared < head.len() || declared > bytes.len() {
        return Err(DecodeError::LengthMismatch {
            declared,
            actual: bytes.len(),
        });
    }
    let tlv_len = usize::from(read_u16(head, BODY_END + 2));
    let value = bytes[head.len()..declared]
        .get(..tlv_len)
        .ok_or(DecodeError::BadTlvLength)?;
    Ok(ManagementMessage {
        transport_specific: head[0] >> 4,
        version: head[1],
        domain: head[4],
        source: read_port_identity(head, 20),
        sequence_id: read_u16(head, 30),
        target: read_port_identity(head, HEADER_LEN),
        starting_boundary_hops: head[44],
        boundary_hops: head[45],
        action: Action::from_bits(head[46]),
        tlv: decode_tlv(read_u16(head, BODY_END), value)?,
    })
}

fn decode_tlv(tlv_type: u16, value: &[u8]) -> Result<ManagementTlv, DecodeError> {
    match tlv_type {
        TLV_MANAGEMENT => {
            let (id, data) = value
                .split_first_chunk::<2>()
                .ok_or(DecodeError::BadTlvLength)?;
            Ok(ManagementTlv::Management {
                id: ManagementId(u16::from_be_bytes(*id)),
                data: data.to_vec(),
            })
        }
        TLV_MANAGEMENT_ERROR_STATUS => {
            let (fixed, rest) = value
                .split_first_chunk::<ERROR_STATUS_FIXED_LEN>()
                .ok_or(DecodeError::BadTlvLength)?;
            // displayData は PTPText（長さ 1 バイト + 文字）。なければ空
            let display = rest
                .split_first()
                .and_then(|(len, text)| text.get(..usize::from(*len)))
                .unwrap_or_default();
            Ok(ManagementTlv::ErrorStatus {
                error: ManagementErrorId::from_bits(read_u16(fixed, 0)),
                id: ManagementId(read_u16(fixed, 2)),
                display: display.to_vec(),
            })
        }
        other => Ok(ManagementTlv::Other {
            tlv_type: other,
            value: value.to_vec(),
        }),
    }
}

pub(super) fn push_port_identity(out: &mut Vec<u8>, identity: PortIdentity) {
    out.extend_from_slice(&identity.clock);
    out.extend_from_slice(&identity.port.to_be_bytes());
}

/// `at` から 10 バイトをポートの識別子として読む。呼び出し側が長さを確かめておくこと。
pub(super) fn read_port_identity(bytes: &[u8], at: usize) -> PortIdentity {
    let mut clock = [0u8; 8];
    clock.copy_from_slice(&bytes[at..at + 8]);
    PortIdentity {
        clock,
        port: read_u16(bytes, at + 8),
    }
}

/// `at` から 2 バイトをビッグエンディアンで読む。呼び出し側が長さを確かめておくこと。
pub(super) const fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: PortIdentity = PortIdentity {
        clock: [0, 0, 0, 0, 0, 0, 0x12, 0x34],
        port: 0x5678,
    };

    /// `pmc -u -b 0 -d 24 'GET CURRENT_DATA_SET'` に相当する問い合わせ（-z: データは空）のバイト列。
    #[test]
    fn encodes_a_get_like_pmc() {
        let request = ManagementMessage::get(
            24,
            SOURCE,
            7,
            PortIdentity::WILDCARD,
            ManagementId::CURRENT_DATA_SET,
        );
        let bytes = encode(&request);
        #[rustfmt::skip]
        let expected: [u8; 54] = [
            0x0D, 0x02, 0x00, 54, 24, 0, 0, 0,          // messageType 0xD・version 2・長さ 54・ドメイン 24
            0, 0, 0, 0, 0, 0, 0, 0,                      // correctionField
            0, 0, 0, 0,                                  // messageTypeSpecific
            0, 0, 0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78,    // sourcePortIdentity
            0x00, 0x07, 0x04, 0x7F,                      // sequenceId・controlField・logMessageInterval
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // targetPortIdentity（すべて）
            0, 0, 0x00, 0,                               // boundaryHops 0 / 0・GET
            0x00, 0x01, 0x00, 0x02, 0x20, 0x01,          // MANAGEMENT TLV（長さ 2 = ID だけ）
        ];
        assert_eq!(bytes, expected);
        assert_eq!(decode(&bytes).unwrap(), request);
    }

    #[test]
    fn pads_odd_tlv_values_to_even_length() {
        let mut message =
            ManagementMessage::get(0, SOURCE, 1, PortIdentity::WILDCARD, ManagementId(0x2000));
        message.tlv = ManagementTlv::Management {
            id: ManagementId::PORT_PROPERTIES_NP,
            data: vec![1, 2, 3],
        };
        let bytes = encode(&message);
        assert_eq!(read_u16(&bytes, 50), 6, "ID 2 + データ 3 + 埋め 1");
        assert_eq!(bytes.len(), 58);
        let ManagementTlv::Management { data, .. } = decode(&bytes).unwrap().tlv else {
            panic!("MANAGEMENT TLV のはず");
        };
        assert_eq!(data, [1, 2, 3, 0]);
    }

    #[test]
    fn decodes_an_error_status_reply() {
        let mut reply = encode(&ManagementMessage::get(
            0,
            SOURCE,
            3,
            PortIdentity::port_of_any_clock(9),
            ManagementId::PORT_HWCLOCK_NP,
        ));
        reply[46] = 2; // RESPONSE
        reply.truncate(BODY_END);
        // MANAGEMENT_ERROR_STATUS: NO_SUCH_ID・PORT_HWCLOCK_NP・reserved・displayData "no"（長さ 2）+ 埋め
        reply.extend_from_slice(&[0x00, 0x02, 0x00, 12]);
        reply.extend_from_slice(&[0x00, 0x02, 0xC0, 0x09, 0, 0, 0, 0, 2, b'n', b'o', 0]);
        let total = u16::try_from(reply.len()).unwrap();
        reply[2..4].copy_from_slice(&total.to_be_bytes());
        let message = decode(&reply).unwrap();
        assert_eq!(message.action, Action::Response);
        assert_eq!(message.target, PortIdentity::port_of_any_clock(9));
        assert_eq!(
            message.tlv,
            ManagementTlv::ErrorStatus {
                error: ManagementErrorId::NoSuchId,
                id: ManagementId::PORT_HWCLOCK_NP,
                display: b"no".to_vec(),
            }
        );
        assert_eq!(encode(&message), reply);
    }

    #[test]
    fn rejects_other_messages_and_bad_lengths() {
        let good = encode(&ManagementMessage::get(
            0,
            SOURCE,
            1,
            PortIdentity::WILDCARD,
            ManagementId::DEFAULT_DATA_SET,
        ));
        let mut sync = good.clone();
        sync[0] = 0x00;
        assert_eq!(decode(&sync), Err(DecodeError::NotManagement(0)));
        let mut v1 = good.clone();
        v1[1] = 0x01;
        assert_eq!(decode(&v1), Err(DecodeError::UnsupportedVersion(1)));
        let mut longer = good.clone();
        longer[3] = 60;
        assert!(matches!(
            decode(&longer),
            Err(DecodeError::LengthMismatch { declared: 60, .. })
        ));
        let mut tlv_overrun = good.clone();
        tlv_overrun[51] = 4u8;
        assert_eq!(decode(&tlv_overrun), Err(DecodeError::BadTlvLength));
        // 後ろに余分なバイトがあっても、messageLength までを読む
        let mut padded = good.clone();
        padded.extend_from_slice(&[0xAA; 6]);
        assert_eq!(decode(&padded), decode(&good));
        // どの長さで切れても、パニックせずエラーになる
        for len in 0..good.len() {
            assert!(decode(&good[..len]).is_err(), "{len} バイト");
        }
    }

    #[test]
    fn version_minor_bits_are_ignored() {
        let mut bytes = encode(&ManagementMessage::get(
            0,
            SOURCE,
            1,
            PortIdentity::WILDCARD,
            ManagementId::DEFAULT_DATA_SET,
        ));
        bytes[1] = 0x12; // linuxptp 4 の送る PTP 2.1
        assert_eq!(decode(&bytes).unwrap().version, 0x12);
    }
}
