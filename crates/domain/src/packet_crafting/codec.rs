//! NTP メッセージとバイト列の相互変換。
//!
//! 解析（decode）は寛容: 長さ以外は一切検証しない。値の妥当性は時刻同期コンテキストが判断する。

use super::NtpMessage;
use crate::shared_kernel::NtpTimestamp;

/// NTP ヘッダの長さ（バイト）。
pub const HEADER_LEN: usize = 48;

/// バイト列を NTP メッセージとして読めなかった。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("NTP ヘッダより短いパケットです（{len} バイト）")]
    TooShort { len: usize },
}

/// NTP メッセージをバイト列にする。LI / VN / Mode は各ビット幅に切り詰める。
pub fn encode(message: &NtpMessage) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + message.trailing.len());
    buf.push(
        ((message.leap & 0b11) << 6) | ((message.version & 0b111) << 3) | (message.mode & 0b111),
    );
    buf.push(message.stratum);
    buf.extend_from_slice(&message.poll.to_be_bytes());
    buf.extend_from_slice(&message.precision.to_be_bytes());
    buf.extend_from_slice(&message.root_delay.to_be_bytes());
    buf.extend_from_slice(&message.root_dispersion.to_be_bytes());
    buf.extend_from_slice(&message.reference_id);
    for timestamp in [
        message.reference_timestamp,
        message.origin_timestamp,
        message.receive_timestamp,
        message.transmit_timestamp,
    ] {
        buf.extend_from_slice(&timestamp.to_bits().to_be_bytes());
    }
    buf.extend_from_slice(&message.trailing);
    buf
}

/// バイト列を NTP メッセージとして読む。48 バイト未満でなければ必ず成功する。
pub fn decode(bytes: &[u8]) -> Result<NtpMessage, DecodeError> {
    let Some(header) = bytes.get(..HEADER_LEN) else {
        return Err(DecodeError::TooShort { len: bytes.len() });
    };
    Ok(NtpMessage {
        leap: header[0] >> 6,
        version: (header[0] >> 3) & 0b111,
        mode: header[0] & 0b111,
        stratum: header[1],
        poll: i8::from_be_bytes([header[2]]),
        precision: i8::from_be_bytes([header[3]]),
        root_delay: be_u32(&header[4..8]),
        root_dispersion: be_u32(&header[8..12]),
        reference_id: [header[12], header[13], header[14], header[15]],
        reference_timestamp: timestamp(&header[16..24]),
        origin_timestamp: timestamp(&header[24..32]),
        receive_timestamp: timestamp(&header[32..40]),
        transmit_timestamp: timestamp(&header[40..48]),
        trailing: bytes[HEADER_LEN..].to_vec(),
    })
}

fn be_u32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |acc, &b| (acc << 8) | u32::from(b))
}

fn timestamp(bytes: &[u8]) -> NtpTimestamp {
    NtpTimestamp::from_bits(bytes.iter().fold(0, |acc, &b| (acc << 8) | u64::from(b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_crafting::RequestTemplate;

    #[test]
    fn encodes_default_client_request_exactly() {
        let request =
            RequestTemplate::default().build(NtpTimestamp::from_bits(0x0123_4567_89AB_CDEF));
        let mut expected = [0u8; HEADER_LEN];
        expected[0] = 0x23; // LI=0, VN=4, Mode=3
        expected[40..48].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]);
        assert_eq!(encode(&request), expected);
    }

    #[test]
    fn decodes_server_reply_bytes() {
        #[rustfmt::skip]
        let bytes: [u8; HEADER_LEN] = [
            0x24, 0x01, 0x04, 0xEC,                         // LI=0 VN=4 Mode=4, stratum 1, poll 4, precision -20
            0x00, 0x00, 0x00, 0x10,                         // root delay
            0x00, 0x00, 0x00, 0x20,                         // root dispersion
            b'G', b'N', b'S', b'S',                         // reference id
            0xEC, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // reference
            0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22, // origin
            0xEC, 0x00, 0x00, 0x01, 0x80, 0x00, 0x00, 0x00, // receive
            0xEC, 0x00, 0x00, 0x01, 0x80, 0x00, 0x10, 0x00, // transmit
        ];
        let message = decode(&bytes).unwrap();
        assert_eq!(message.leap, 0);
        assert_eq!(message.version, 4);
        assert_eq!(message.mode, NtpMessage::MODE_SERVER);
        assert_eq!(message.stratum, 1);
        assert_eq!(message.poll, 4);
        assert_eq!(message.precision, -20);
        assert_eq!(message.root_delay, 0x10);
        assert_eq!(message.root_dispersion, 0x20);
        assert_eq!(&message.reference_id, b"GNSS");
        assert_eq!(
            message.origin_timestamp,
            NtpTimestamp::from_parts(0x1111_1111, 0x2222_2222)
        );
        assert_eq!(
            message.receive_timestamp,
            NtpTimestamp::from_parts(0xEC00_0001, 0x8000_0000)
        );
        assert_eq!(
            message.transmit_timestamp,
            NtpTimestamp::from_parts(0xEC00_0001, 0x8000_1000)
        );
        assert!(message.trailing.is_empty());
    }

    #[test]
    fn round_trips_non_standard_values() {
        let message = NtpMessage {
            leap: 3,
            version: 7,
            mode: 7,
            stratum: 255,
            poll: -6,
            precision: 127,
            root_delay: u32::MAX,
            root_dispersion: 1,
            reference_id: [0, 0xFF, 0x7F, 0x80],
            reference_timestamp: NtpTimestamp::from_bits(1),
            origin_timestamp: NtpTimestamp::from_bits(2),
            receive_timestamp: NtpTimestamp::from_bits(3),
            transmit_timestamp: NtpTimestamp::from_bits(u64::MAX),
            trailing: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        assert_eq!(decode(&encode(&message)).unwrap(), message);
    }

    #[test]
    fn masks_out_of_range_header_bits() {
        let message = NtpMessage {
            leap: 0b111,
            version: 0b1111,
            mode: 0b1111,
            ..NtpMessage::default()
        };
        assert_eq!(encode(&message)[0], 0xFF);
    }

    #[test]
    fn rejects_short_packets_only() {
        assert_eq!(decode(&[0u8; 47]), Err(DecodeError::TooShort { len: 47 }));
        assert!(decode(&[0u8; 48]).is_ok());
    }
}
