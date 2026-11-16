//! SNMP v2c のメッセージ（RFC 3416 の PDU と、RFC 1901 のコミュニティ方式の包み）。

use super::ObjectId;

/// SNMP のメッセージ。`SEQUENCE { version, community, PDU }`。
///
/// 整数は生の値で持つ（範囲外の値も作れるし、読める）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnmpMessage {
    /// 0 = v1、1 = v2c
    pub version: i64,
    pub community: Vec<u8>,
    pub pdu: Pdu,
}

impl SnmpMessage {
    pub const VERSION_2C: i64 = 1;

    /// v2c の GET。値はすべて NULL にする。
    #[must_use]
    pub fn get_request(community: &[u8], request_id: i64, names: &[ObjectId]) -> Self {
        Self::request(PduKind::GetRequest, community, request_id, names)
    }

    /// v2c の GETNEXT。値はすべて NULL にする。
    #[must_use]
    pub fn get_next_request(community: &[u8], request_id: i64, names: &[ObjectId]) -> Self {
        Self::request(PduKind::GetNextRequest, community, request_id, names)
    }

    fn request(kind: PduKind, community: &[u8], request_id: i64, names: &[ObjectId]) -> Self {
        Self {
            version: Self::VERSION_2C,
            community: community.to_vec(),
            pdu: Pdu {
                kind,
                request_id,
                error_status: Pdu::NO_ERROR,
                error_index: 0,
                varbinds: names
                    .iter()
                    .map(|name| VarBind {
                        name: name.clone(),
                        value: SnmpValue::Null,
                    })
                    .collect(),
            },
        }
    }
}

/// PDU の種類（BER のタグ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PduKind {
    GetRequest,
    GetNextRequest,
    Response,
    SetRequest,
    GetBulkRequest,
    InformRequest,
    /// SNMPv2-Trap
    Trap,
    Report,
    /// 知らないタグ（v1 の Trap など）
    Other(u8),
}

impl PduKind {
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::GetRequest => 0xA0,
            Self::GetNextRequest => 0xA1,
            Self::Response => 0xA2,
            Self::SetRequest => 0xA3,
            Self::GetBulkRequest => 0xA5,
            Self::InformRequest => 0xA6,
            Self::Trap => 0xA7,
            Self::Report => 0xA8,
            Self::Other(tag) => tag,
        }
    }

    #[must_use]
    pub const fn from_tag(tag: u8) -> Self {
        match tag {
            0xA0 => Self::GetRequest,
            0xA1 => Self::GetNextRequest,
            0xA2 => Self::Response,
            0xA3 => Self::SetRequest,
            0xA5 => Self::GetBulkRequest,
            0xA6 => Self::InformRequest,
            0xA7 => Self::Trap,
            0xA8 => Self::Report,
            other => Self::Other(other),
        }
    }
}

/// PDU。GETBULK では `error_status` と `error_index` の位置に non-repeaters と max-repetitions が入る。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pdu {
    pub kind: PduKind,
    pub request_id: i64,
    pub error_status: i64,
    /// エラーの原因になった varbind の位置（1 から数える。0 = なし）
    pub error_index: i64,
    pub varbinds: Vec<VarBind>,
}

impl Pdu {
    pub const NO_ERROR: i64 = 0;
    pub const TOO_BIG: i64 = 1;
    /// v1 の「その名前はない」。v2c の GET では、ふつうは varbind ごとの `noSuchObject` などで返る
    pub const NO_SUCH_NAME: i64 = 2;
    pub const GEN_ERR: i64 = 5;
}

/// 名前（OID）と値の組。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VarBind {
    pub name: ObjectId,
    pub value: SnmpValue,
}

/// varbind の値。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnmpValue {
    /// INTEGER（Integer32 と列挙型）。読むときは 8 バイトまで受け付ける
    Integer(i64),
    /// OCTET STRING（`DisplayString` と BITS もこの型で届く）
    OctetString(Vec<u8>),
    Null,
    ObjectId(ObjectId),
    IpAddress([u8; 4]),
    Counter32(u32),
    /// Gauge32（`Unsigned32` も同じ型）
    Gauge32(u32),
    /// 1/100 秒
    TimeTicks(u32),
    Opaque(Vec<u8>),
    Counter64(u64),
    /// そのオブジェクトがない（v2c の GET の応答）
    NoSuchObject,
    /// オブジェクトはあるが、そのインデックスの行がない（v2c の GET の応答）
    NoSuchInstance,
    /// GETNEXT でこの先に何もない
    EndOfMibView,
    /// 知らない型。タグと中身をそのまま持つ
    Other {
        tag: u8,
        content: Vec<u8>,
    },
}

impl SnmpValue {
    /// `noSuchObject` / `noSuchInstance` / `endOfMibView`（値がない）か。
    #[must_use]
    pub const fn is_exception(&self) -> bool {
        matches!(
            self,
            Self::NoSuchObject | Self::NoSuchInstance | Self::EndOfMibView
        )
    }
}
