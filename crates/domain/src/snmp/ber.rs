//! SNMP メッセージと BER（X.690 の基本符号化規則）のバイト列の相互変換。
//!
//! - 書くとき（encode）は最短の形にする（長さは 128 未満なら短形式、整数は余分な先頭バイトを付けない）
//! - 読むとき（decode）は寛容: 最短でない長さ・整数も受け付け、知らない値の型は [`SnmpValue::Other`] にする。
//!   ただし不定長（`0x80`）と 5 バイト以上の長さ、複数バイトのタグは受け付けない
//! - 符号なしの型（Counter32 など）は、先頭に `0x00` が付いた 5 バイトの形も、付け忘れた 4 バイトの形も読む

use super::{ObjectId, Pdu, PduKind, SnmpMessage, SnmpValue, VarBind};

const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_NULL: u8 = 0x05;
const TAG_OBJECT_ID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_IP_ADDRESS: u8 = 0x40;
const TAG_COUNTER32: u8 = 0x41;
const TAG_GAUGE32: u8 = 0x42;
const TAG_TIME_TICKS: u8 = 0x43;
const TAG_OPAQUE: u8 = 0x44;
const TAG_COUNTER64: u8 = 0x46;
const TAG_NO_SUCH_OBJECT: u8 = 0x80;
const TAG_NO_SUCH_INSTANCE: u8 = 0x81;
const TAG_END_OF_MIB_VIEW: u8 = 0x82;

/// 長形式の長さに使えるバイト数の上限。
const MAX_LENGTH_BYTES: usize = 4;
/// OID の最初の 2 つの数は `40 × X + Y` で 1 つにまとめる。
const FIRST_ARCS_BASE: u64 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("SNMP メッセージが途中で切れています")]
    Truncated,
    #[error("不定長の形式には対応していません")]
    IndefiniteLength,
    #[error("長さの表現が長すぎます（{0} バイト）")]
    LengthTooLong(u8),
    #[error("複数バイトのタグには対応していません")]
    HighTagNumber,
    #[error("型が違います（0x{expected:02X} のはずが 0x{found:02X}）")]
    UnexpectedTag { expected: u8, found: u8 },
    #[error("整数の中身が空です")]
    EmptyInteger,
    #[error("整数が大きすぎます")]
    IntegerTooLarge,
    #[error("OID の形式が正しくありません")]
    InvalidObjectId,
}

/// メッセージをバイト列にする。
///
/// OID の最初の 2 つの数は `40 × X + Y` にまとめる。数が 2 つ未満なら、足りない分を 0 とみなす。
#[must_use]
pub fn encode(message: &SnmpMessage) -> Vec<u8> {
    let mut varbinds = Vec::new();
    for varbind in &message.pdu.varbinds {
        let mut item = Vec::new();
        push_tlv(&mut item, TAG_OBJECT_ID, &object_id_content(&varbind.name));
        push_value(&mut item, &varbind.value);
        push_tlv(&mut varbinds, TAG_SEQUENCE, &item);
    }
    let mut pdu = Vec::new();
    push_tlv(
        &mut pdu,
        TAG_INTEGER,
        &signed_content(message.pdu.request_id),
    );
    push_tlv(
        &mut pdu,
        TAG_INTEGER,
        &signed_content(message.pdu.error_status),
    );
    push_tlv(
        &mut pdu,
        TAG_INTEGER,
        &signed_content(message.pdu.error_index),
    );
    push_tlv(&mut pdu, TAG_SEQUENCE, &varbinds);

    let mut body = Vec::new();
    push_tlv(&mut body, TAG_INTEGER, &signed_content(message.version));
    push_tlv(&mut body, TAG_OCTET_STRING, &message.community);
    push_tlv(&mut body, message.pdu.kind.tag(), &pdu);

    let mut bytes = Vec::with_capacity(body.len() + 6);
    push_tlv(&mut bytes, TAG_SEQUENCE, &body);
    bytes
}

/// バイト列をメッセージとして読む。メッセージの後ろに余分なバイトがあっても無視する。
///
/// # Errors
///
/// 途中で切れているとき、型（タグ）が SNMP メッセージの形と合わないとき、長さ・整数・OID が読めないとき。
pub fn decode(bytes: &[u8]) -> Result<SnmpMessage, DecodeError> {
    let mut outer = Reader::new(bytes);
    let mut fields = Reader::new(outer.expect(TAG_SEQUENCE)?);
    let version = signed(fields.expect(TAG_INTEGER)?)?;
    let community = fields.expect(TAG_OCTET_STRING)?.to_vec();
    let (pdu_tag, pdu_content) = fields.read_tlv()?;

    let mut pdu = Reader::new(pdu_content);
    let request_id = signed(pdu.expect(TAG_INTEGER)?)?;
    let error_status = signed(pdu.expect(TAG_INTEGER)?)?;
    let error_index = signed(pdu.expect(TAG_INTEGER)?)?;
    let mut list = Reader::new(pdu.expect(TAG_SEQUENCE)?);
    let mut varbinds = Vec::new();
    while !list.is_empty() {
        let mut item = Reader::new(list.expect(TAG_SEQUENCE)?);
        let name = object_id(item.expect(TAG_OBJECT_ID)?)?;
        let (tag, content) = item.read_tlv()?;
        varbinds.push(VarBind {
            name,
            value: value(tag, content)?,
        });
    }
    Ok(SnmpMessage {
        version,
        community,
        pdu: Pdu {
            kind: PduKind::from_tag(pdu_tag),
            request_id,
            error_status,
            error_index,
            varbinds,
        },
    })
}

// ---- 書く ----

fn push_tlv(out: &mut Vec<u8>, tag: u8, content: &[u8]) {
    out.push(tag);
    push_length(out, content.len());
    out.extend_from_slice(content);
}

/// 128 未満なら 1 バイト（短形式）、それ以上は `0x80 | バイト数` に続けて長さを書く（長形式）。
fn push_length(out: &mut Vec<u8>, len: usize) {
    let bytes = u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes();
    match bytes.iter().position(|&byte| byte != 0) {
        Some(7) if bytes[7] < 0x80 => out.push(bytes[7]),
        None => out.push(0),
        Some(start) => {
            // 1〜8 バイトなので u8 に収まる
            out.push(0x80 | u8::try_from(bytes.len() - start).unwrap_or(0));
            out.extend_from_slice(&bytes[start..]);
        }
    }
}

#[expect(
    clippy::pattern_type_mismatch,
    reason = "値の中身（Vec など）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
)]
fn push_value(out: &mut Vec<u8>, value: &SnmpValue) {
    match value {
        SnmpValue::Integer(number) => push_tlv(out, TAG_INTEGER, &signed_content(*number)),
        SnmpValue::OctetString(content) => push_tlv(out, TAG_OCTET_STRING, content),
        SnmpValue::Null => push_tlv(out, TAG_NULL, &[]),
        SnmpValue::ObjectId(oid) => push_tlv(out, TAG_OBJECT_ID, &object_id_content(oid)),
        SnmpValue::IpAddress(address) => push_tlv(out, TAG_IP_ADDRESS, address),
        SnmpValue::Counter32(number) => {
            push_tlv(out, TAG_COUNTER32, &unsigned_content(u64::from(*number)));
        }
        SnmpValue::Gauge32(number) => {
            push_tlv(out, TAG_GAUGE32, &unsigned_content(u64::from(*number)));
        }
        SnmpValue::TimeTicks(number) => {
            push_tlv(out, TAG_TIME_TICKS, &unsigned_content(u64::from(*number)));
        }
        SnmpValue::Opaque(content) => push_tlv(out, TAG_OPAQUE, content),
        SnmpValue::Counter64(number) => push_tlv(out, TAG_COUNTER64, &unsigned_content(*number)),
        SnmpValue::NoSuchObject => push_tlv(out, TAG_NO_SUCH_OBJECT, &[]),
        SnmpValue::NoSuchInstance => push_tlv(out, TAG_NO_SUCH_INSTANCE, &[]),
        SnmpValue::EndOfMibView => push_tlv(out, TAG_END_OF_MIB_VIEW, &[]),
        SnmpValue::Other { tag, content } => push_tlv(out, *tag, content),
    }
}

/// 2 の補数の最短の形（先頭の 0x00 / 0xFF は、次のバイトの最上位ビットで符号がわかるなら省く）。
fn signed_content(value: i64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let mut start = 0;
    while start + 1 < bytes.len() {
        let next_negative = bytes[start + 1] & 0x80 != 0;
        let redundant = match bytes[start] {
            0x00 => !next_negative,
            0xFF => next_negative,
            _ => false,
        };
        if !redundant {
            break;
        }
        start += 1;
    }
    bytes[start..].to_vec()
}

/// 符号なしの最短の形。最上位ビットが立つなら（負と読まれないよう）先頭に 0x00 を付ける。
fn unsigned_content(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let start = bytes
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(bytes.len() - 1);
    let mut content = Vec::with_capacity(bytes.len() + 1);
    if bytes[start] & 0x80 != 0 {
        content.push(0);
    }
    content.extend_from_slice(&bytes[start..]);
    content
}

fn object_id_content(oid: &ObjectId) -> Vec<u8> {
    let arcs = oid.arcs();
    let first = u64::from(arcs.first().copied().unwrap_or(0)) * FIRST_ARCS_BASE
        + u64::from(arcs.get(1).copied().unwrap_or(0));
    let mut content = Vec::new();
    push_base128(&mut content, first);
    for &arc in arcs.iter().skip(2) {
        push_base128(&mut content, u64::from(arc));
    }
    content
}

/// 7 bit ずつ上位から書く。最後のバイト以外は最上位ビットを立てる。
fn push_base128(out: &mut Vec<u8>, value: u64) {
    let mut groups = Vec::new();
    let mut rest = value;
    loop {
        groups.push(rest.to_be_bytes()[7] & 0x7F);
        rest >>= 7u32;
        if rest == 0 {
            break;
        }
    }
    let last = groups.len() - 1;
    for (index, group) in groups.iter().rev().enumerate() {
        out.push(if index < last { group | 0x80 } else { *group });
    }
}

// ---- 読む ----

/// TLV（タグ・長さ・中身）を先頭から順に読む。
struct Reader<'bytes> {
    rest: &'bytes [u8],
}

impl<'bytes> Reader<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { rest: bytes }
    }

    const fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    fn read_tlv(&mut self) -> Result<(u8, &'bytes [u8]), DecodeError> {
        let (&tag, after_tag) = self.rest.split_first().ok_or(DecodeError::Truncated)?;
        if tag & 0x1F == 0x1F {
            return Err(DecodeError::HighTagNumber);
        }
        let (&first, after_first) = after_tag.split_first().ok_or(DecodeError::Truncated)?;
        let (len, after_length) = match first {
            0x00..=0x7F => (usize::from(first), after_first),
            0x80 => return Err(DecodeError::IndefiniteLength),
            _ => {
                let count = first & 0x7F;
                if usize::from(count) > MAX_LENGTH_BYTES {
                    return Err(DecodeError::LengthTooLong(count));
                }
                let (length_bytes, after) = after_first
                    .split_at_checked(usize::from(count))
                    .ok_or(DecodeError::Truncated)?;
                let len = length_bytes
                    .iter()
                    .fold(0usize, |acc, &byte| (acc << 8u32) | usize::from(byte));
                (len, after)
            }
        };
        let (content, remaining) = after_length
            .split_at_checked(len)
            .ok_or(DecodeError::Truncated)?;
        self.rest = remaining;
        Ok((tag, content))
    }

    fn expect(&mut self, expected: u8) -> Result<&'bytes [u8], DecodeError> {
        let (tag, content) = self.read_tlv()?;
        if tag == expected {
            Ok(content)
        } else {
            Err(DecodeError::UnexpectedTag {
                expected,
                found: tag,
            })
        }
    }
}

fn value(tag: u8, content: &[u8]) -> Result<SnmpValue, DecodeError> {
    Ok(match tag {
        TAG_INTEGER => SnmpValue::Integer(signed(content)?),
        TAG_OCTET_STRING => SnmpValue::OctetString(content.to_vec()),
        TAG_NULL => SnmpValue::Null,
        TAG_OBJECT_ID => SnmpValue::ObjectId(object_id(content)?),
        TAG_IP_ADDRESS => <[u8; 4]>::try_from(content).map_or_else(
            |_| SnmpValue::Other {
                tag,
                content: content.to_vec(),
            },
            SnmpValue::IpAddress,
        ),
        TAG_COUNTER32 => SnmpValue::Counter32(unsigned32(content)?),
        TAG_GAUGE32 => SnmpValue::Gauge32(unsigned32(content)?),
        TAG_TIME_TICKS => SnmpValue::TimeTicks(unsigned32(content)?),
        TAG_OPAQUE => SnmpValue::Opaque(content.to_vec()),
        TAG_COUNTER64 => SnmpValue::Counter64(unsigned(content)?),
        TAG_NO_SUCH_OBJECT => SnmpValue::NoSuchObject,
        TAG_NO_SUCH_INSTANCE => SnmpValue::NoSuchInstance,
        TAG_END_OF_MIB_VIEW => SnmpValue::EndOfMibView,
        _ => SnmpValue::Other {
            tag,
            content: content.to_vec(),
        },
    })
}

/// 2 の補数の整数（8 バイトまで）。
fn signed(content: &[u8]) -> Result<i64, DecodeError> {
    let &first = content.first().ok_or(DecodeError::EmptyInteger)?;
    if content.len() > 8 {
        return Err(DecodeError::IntegerTooLarge);
    }
    let initial: i64 = if first & 0x80 == 0 { 0 } else { -1 };
    Ok(content
        .iter()
        .fold(initial, |acc, &byte| (acc << 8u32) | i64::from(byte)))
}

/// 符号なしの整数。先頭の 0x00 を除いて 8 バイトまで。
fn unsigned(content: &[u8]) -> Result<u64, DecodeError> {
    if content.is_empty() {
        return Err(DecodeError::EmptyInteger);
    }
    let start = content
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(content.len());
    let digits = &content[start..];
    if digits.len() > 8 {
        return Err(DecodeError::IntegerTooLarge);
    }
    Ok(digits
        .iter()
        .fold(0u64, |acc, &byte| (acc << 8u32) | u64::from(byte)))
}

fn unsigned32(content: &[u8]) -> Result<u32, DecodeError> {
    u32::try_from(unsigned(content)?)
        .ok()
        .ok_or(DecodeError::IntegerTooLarge)
}

fn object_id(content: &[u8]) -> Result<ObjectId, DecodeError> {
    let mut numbers: Vec<u64> = Vec::new();
    let mut current = 0u64;
    let mut continued = false;
    for &byte in content {
        // 7 bit ずつ足していくので、上位 7 bit が埋まっていたら次で溢れる
        if current >> 57u32 != 0 {
            return Err(DecodeError::InvalidObjectId);
        }
        current = (current << 7u32) | u64::from(byte & 0x7F);
        continued = byte & 0x80 != 0;
        if !continued {
            numbers.push(current);
            current = 0;
        }
    }
    let Some((&first, rest)) = numbers.split_first() else {
        return Err(DecodeError::InvalidObjectId);
    };
    if continued {
        return Err(DecodeError::InvalidObjectId);
    }
    // 最初の数: X は 0〜2。X = 2 のときだけ Y が 40 以上になりうる
    let top = first.min(2 * FIRST_ARCS_BASE) / FIRST_ARCS_BASE;
    let second = first - top * FIRST_ARCS_BASE;
    let arcs: Option<Vec<u32>> = [top, second]
        .iter()
        .chain(rest)
        .map(|&number| u32::try_from(number).ok())
        .collect();
    arcs.map(|parsed| ObjectId::new(&parsed))
        .ok_or(DecodeError::InvalidObjectId)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys_descr() -> ObjectId {
        ObjectId::new(&[1, 3, 6, 1, 2, 1, 1, 1, 0])
    }

    #[test]
    fn encodes_get_request_exactly() {
        let request = SnmpMessage::get_request(b"public", 1, &[sys_descr()]);
        #[rustfmt::skip]
        let expected = [
            0x30, 0x26,                                     // SEQUENCE（38 バイト）
            0x02, 0x01, 0x01,                               // version = 1（v2c）
            0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c', // community
            0xA0, 0x19,                                     // GetRequest（25 バイト）
            0x02, 0x01, 0x01,                               // request-id = 1
            0x02, 0x01, 0x00,                               // error-status = 0
            0x02, 0x01, 0x00,                               // error-index = 0
            0x30, 0x0E,                                     // varbind の並び
            0x30, 0x0C,                                     // varbind
            0x06, 0x08, 0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00, // 1.3.6.1.2.1.1.1.0
            0x05, 0x00,                                     // NULL
        ];
        assert_eq!(encode(&request), expected);
        assert_eq!(decode(&expected).unwrap(), request);
    }

    #[test]
    fn decodes_hand_written_response() {
        #[rustfmt::skip]
        let bytes = [
            0x30, 0x81, 0x45,                               // 長形式の長さ（69 バイト）
            0x02, 0x01, 0x01,
            0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c',
            0xA2, 0x38,                                     // Response
            0x02, 0x04, 0x7F, 0xFF, 0xFF, 0xFF,             // request-id = 2147483647
            0x02, 0x01, 0x00,
            0x02, 0x01, 0x00,
            0x30, 0x2A,
            // 1.3.6.1.4.1.9070.1.2.5.7.8.1.3.0 = INTEGER -19
            0x30, 0x14, 0x06, 0x0F, 0x2B, 0x06, 0x01, 0x04, 0x01, 0xC6, 0x6E, 0x01, 0x02, 0x05, 0x07, 0x08, 0x01, 0x03, 0x00,
            0x02, 0x01, 0xED,
            // 2.999 = Gauge32 4294967295（先頭に 0x00 が付いた 5 バイト）
            0x30, 0x0B, 0x06, 0x02, 0x88, 0x37, 0x42, 0x05, 0x00, 0xFF, 0xFF, 0xFF, 0xFF,
            // 1.3 = noSuchInstance
            0x30, 0x05, 0x06, 0x01, 0x2B, 0x81, 0x00,
        ];
        let message = decode(&bytes).unwrap();
        assert_eq!(message.version, SnmpMessage::VERSION_2C);
        assert_eq!(message.community, b"public");
        assert_eq!(message.pdu.kind, PduKind::Response);
        assert_eq!(message.pdu.request_id, i64::from(i32::MAX));
        let values: Vec<(String, SnmpValue)> = message
            .pdu
            .varbinds
            .into_iter()
            .map(|varbind| (varbind.name.to_string(), varbind.value))
            .collect();
        assert_eq!(
            values,
            [
                (
                    "1.3.6.1.4.1.9070.1.2.5.7.8.1.3.0".to_owned(),
                    SnmpValue::Integer(-19)
                ),
                ("2.999".to_owned(), SnmpValue::Gauge32(u32::MAX)),
                ("1.3".to_owned(), SnmpValue::NoSuchInstance),
            ]
        );
    }

    #[test]
    fn round_trips_every_value_type() {
        let values = [
            SnmpValue::Integer(i64::MIN),
            SnmpValue::Integer(-129),
            SnmpValue::Integer(0),
            SnmpValue::Integer(i64::MAX),
            SnmpValue::OctetString(b"SyncServer S650\0".to_vec()),
            SnmpValue::OctetString(vec![0x41; 300]),
            SnmpValue::Null,
            SnmpValue::ObjectId(ObjectId::new(&[1, 3, 6, 1, 4, 1, 5597, 30])),
            SnmpValue::IpAddress([192, 168, 1, 123]),
            SnmpValue::Counter32(u32::MAX),
            SnmpValue::Gauge32(0),
            SnmpValue::TimeTicks(123_456),
            SnmpValue::Opaque(vec![0x9F, 0x78, 0x04]),
            SnmpValue::Counter64(u64::MAX),
            SnmpValue::NoSuchObject,
            SnmpValue::NoSuchInstance,
            SnmpValue::EndOfMibView,
            SnmpValue::Other {
                tag: 0x47,
                content: vec![1, 2, 3],
            },
        ];
        let message = SnmpMessage {
            version: SnmpMessage::VERSION_2C,
            community: b"secret".to_vec(),
            pdu: Pdu {
                kind: PduKind::Response,
                request_id: 42,
                error_status: Pdu::GEN_ERR,
                error_index: 3,
                varbinds: values
                    .iter()
                    .zip(1u32..)
                    .map(|(value, index)| VarBind {
                        name: ObjectId::new(&[1, 3, 6, 1, 4, 1, 99999, index]),
                        value: value.clone(),
                    })
                    .collect(),
            },
        };
        assert_eq!(decode(&encode(&message)).unwrap(), message);
    }

    #[test]
    fn integers_use_the_shortest_form() {
        for (number, expected) in [
            (0i64, &[0x00u8][..]),
            (127, &[0x7F]),
            (128, &[0x00, 0x80]),
            (-1, &[0xFF]),
            (-128, &[0x80]),
            (-129, &[0xFF, 0x7F]),
        ] {
            assert_eq!(signed_content(number), expected, "{number}");
            assert_eq!(signed(expected).unwrap(), number);
        }
        assert_eq!(unsigned_content(0), [0x00]);
        assert_eq!(
            unsigned_content(u64::from(u32::MAX)),
            [0x00, 0xFF, 0xFF, 0xFF, 0xFF]
        );
        // 0x00 を付け忘れた 4 バイトの形も符号なしとして読む
        assert_eq!(unsigned32(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap(), u32::MAX);
        assert_eq!(
            unsigned32(&[0x01, 0x00, 0x00, 0x00, 0x00]),
            Err(DecodeError::IntegerTooLarge)
        );
        assert_eq!(signed(&[0x01; 9]), Err(DecodeError::IntegerTooLarge));
        assert_eq!(signed(&[]), Err(DecodeError::EmptyInteger));
    }

    #[test]
    fn lengths_use_short_and_long_forms() {
        for (len, expected) in [
            (0usize, &[0x00u8][..]),
            (127, &[0x7F]),
            (128, &[0x81, 0x80]),
            (300, &[0x82, 0x01, 0x2C]),
        ] {
            let mut out = Vec::new();
            push_length(&mut out, len);
            assert_eq!(out, expected, "{len}");
        }
    }

    #[test]
    fn object_ids_combine_the_first_two_arcs() {
        for (arcs, expected) in [
            (&[1u32, 3][..], &[0x2Bu8][..]),
            (&[0, 0], &[0x00]),
            (&[2, 999], &[0x88, 0x37]),
            (
                &[1, 3, 6, 1, 4, 1, 9070],
                &[0x2B, 0x06, 0x01, 0x04, 0x01, 0xC6, 0x6E],
            ),
            (&[1, 3, u32::MAX], &[0x2B, 0x8F, 0xFF, 0xFF, 0xFF, 0x7F]),
        ] {
            let oid = ObjectId::new(arcs);
            assert_eq!(object_id_content(&oid), expected, "{oid}");
            assert_eq!(object_id(expected).unwrap(), oid);
        }
        for bad in [
            &[][..],
            &[0x2B, 0x86],                         // 最後のバイトで続きの印が立っている
            &[0x2B, 0x90, 0x80, 0x80, 0x80, 0x00], // 32 bit を超える
            &[0xFF; 10],
        ] {
            assert_eq!(object_id(bad), Err(DecodeError::InvalidObjectId), "{bad:?}");
        }
    }

    /// ネットワークから届く入力なので、途中で切れたものはどれも `Err` で返す（パニックしない）。
    #[test]
    fn every_truncation_is_an_error() {
        let message = SnmpMessage::get_request(
            b"public",
            0x1234_5678,
            &[
                sys_descr(),
                ObjectId::new(&[1, 3, 6, 1, 4, 1, 5597, 30, 0, 1, 2, 1, 6, 1]),
            ],
        );
        let bytes = encode(&message);
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "{len} バイトで切れたもの");
        }
        assert_eq!(decode(&bytes).unwrap(), message);
    }

    #[test]
    fn rejects_malformed_lengths_and_tags() {
        for (bytes, expected) in [
            (
                &[0x30u8, 0x80, 0x00, 0x00][..],
                DecodeError::IndefiniteLength,
            ),
            (
                &[0x30, 0x85, 0x01, 0x00, 0x00, 0x00, 0x00],
                DecodeError::LengthTooLong(5),
            ),
            (
                &[0x30, 0x84, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
                DecodeError::Truncated,
            ),
            (&[0x30, 0x05, 0x02, 0x01], DecodeError::Truncated),
            (&[0x3F, 0x01, 0x00], DecodeError::HighTagNumber),
            (
                &[0x04, 0x00],
                DecodeError::UnexpectedTag {
                    expected: TAG_SEQUENCE,
                    found: TAG_OCTET_STRING,
                },
            ),
        ] {
            assert_eq!(decode(bytes), Err(expected), "{bytes:02X?}");
        }
    }

    #[test]
    fn keeps_unknown_pdu_kinds() {
        let mut message = SnmpMessage::get_request(b"public", 7, &[sys_descr()]);
        message.pdu.kind = PduKind::Other(0xAF);
        let decoded = decode(&encode(&message)).unwrap();
        assert_eq!(decoded.pdu.kind, PduKind::Other(0xAF));
        assert_eq!(PduKind::from_tag(PduKind::Report.tag()), PduKind::Report);
    }
}
