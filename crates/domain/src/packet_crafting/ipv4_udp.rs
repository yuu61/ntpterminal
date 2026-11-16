//! IPv4 / UDP ヘッダの組み立てと解析（raw-ip モード用）。
//!
//! 解析は任意の受信パケット（ほかの通信を含む）に対して行うので、どんな入力でもパニックしない。

use std::net::{Ipv4Addr, SocketAddrV4};

/// IPv4 ヘッダの長さ（オプションなし）。
pub const IPV4_HEADER_LEN: usize = 20;
/// UDP ヘッダの長さ。
pub const UDP_HEADER_LEN: usize = 8;

const IP_VERSION_4: u8 = 4;
const PROTOCOL_UDP: u8 = 17;
const FLAG_DONT_FRAGMENT: u16 = 0x4000;
const FLAG_MORE_FRAGMENTS: u16 = 0x2000;
const FRAGMENT_OFFSET_MASK: u16 = 0x1FFF;

/// IPv4 ヘッダの指定。
///
/// 注意（Linux の raw ソケット）: ヘッダのチェックサムと全長は常にカーネルが書き換える。
/// Identification を 0 にするとカーネルが値を入れる。これらをそのまま送れるのは link モードだけ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Spec {
    /// DSCP（6 bit）。例: 46 = EF（優先転送）
    pub dscp: u8,
    /// ECN（2 bit）
    pub ecn: u8,
    pub identification: u16,
    pub dont_fragment: bool,
    pub ttl: u8,
    /// ヘッダチェックサム。`None` なら正しく計算する。`Some` は試験用の固定値（link モードでのみ届く）
    pub header_checksum: Option<u16>,
}

impl Default for Ipv4Spec {
    fn default() -> Self {
        Self {
            dscp: 0,
            ecn: 0,
            identification: 0,
            dont_fragment: true,
            ttl: 64,
            header_checksum: None,
        }
    }
}

/// UDP チェックサムの入れ方。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UdpChecksum {
    /// 正しく計算する
    #[default]
    Compute,
    /// 0（計算しない。IPv4 では許されている）
    Zero,
    /// 指定した値をそのまま入れる（試験用。わざと不正にできる）
    Fixed(u16),
}

/// IPv4 / UDP ヘッダの雛形。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeaderTemplate {
    pub ipv4: Ipv4Spec,
    pub udp_checksum: UdpChecksum,
}

/// IPv4 + UDP のパケットを組み立てる。
///
/// `source` の IP は必ず実際の送信元を入れること（UDP チェックサムの計算に使うため）。
pub fn encode_ipv4_udp(
    template: &HeaderTemplate,
    source: SocketAddrV4,
    destination: SocketAddrV4,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = u16::try_from(UDP_HEADER_LEN + payload.len()).unwrap_or(u16::MAX);
    let total_len = udp_len.saturating_add(IPV4_HEADER_LEN as u16);
    let spec = &template.ipv4;

    let mut packet = Vec::with_capacity(usize::from(total_len));
    packet.push((IP_VERSION_4 << 4) | (IPV4_HEADER_LEN / 4) as u8);
    packet.push(((spec.dscp & 0x3F) << 2) | (spec.ecn & 0b11));
    packet.extend_from_slice(&total_len.to_be_bytes());
    packet.extend_from_slice(&spec.identification.to_be_bytes());
    let flags = if spec.dont_fragment {
        FLAG_DONT_FRAGMENT
    } else {
        0
    };
    packet.extend_from_slice(&flags.to_be_bytes());
    packet.push(spec.ttl);
    packet.push(PROTOCOL_UDP);
    packet.extend_from_slice(&[0, 0]); // ヘッダチェックサム（あとで入れる）
    packet.extend_from_slice(&source.ip().octets());
    packet.extend_from_slice(&destination.ip().octets());
    let header_checksum = spec
        .header_checksum
        .unwrap_or_else(|| internet_checksum(&packet));
    packet[10..12].copy_from_slice(&header_checksum.to_be_bytes());

    let udp_start = packet.len();
    packet.extend_from_slice(&source.port().to_be_bytes());
    packet.extend_from_slice(&destination.port().to_be_bytes());
    packet.extend_from_slice(&udp_len.to_be_bytes());
    packet.extend_from_slice(&[0, 0]); // UDP チェックサム（あとで入れる）
    packet.extend_from_slice(payload);
    let udp_checksum = match template.udp_checksum {
        UdpChecksum::Compute => {
            match udp_checksum(*source.ip(), *destination.ip(), &packet[udp_start..]) {
                // 計算結果が 0 のときは 0xFFFF を送る（0 は「チェックサムなし」の意味になるため）
                0 => 0xFFFF,
                sum => sum,
            }
        }
        UdpChecksum::Zero => 0,
        UdpChecksum::Fixed(value) => value,
    };
    packet[udp_start + 6..udp_start + 8].copy_from_slice(&udp_checksum.to_be_bytes());
    packet
}

/// 受信した IPv4 / UDP パケットの中身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedUdp<'a> {
    pub source: SocketAddrV4,
    pub destination: SocketAddrV4,
    pub dscp: u8,
    pub ecn: u8,
    pub identification: u16,
    pub dont_fragment: bool,
    pub ttl: u8,
    pub udp_checksum: u16,
    /// UDP ヘッダを含む UDP セグメント全体（チェックサムの検証用）
    pub udp_segment: &'a [u8],
    pub payload: &'a [u8],
}

/// IPv4 / UDP として読めなかった理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IpDecodeError {
    #[error("パケットが短すぎます")]
    TooShort,
    #[error("IPv4 ではありません（version {0}）")]
    NotIpv4(u8),
    #[error("IP ヘッダ長が不正です")]
    BadHeaderLength,
    #[error("UDP ではありません（protocol {0}）")]
    NotUdp(u8),
    #[error("断片化されたパケットです")]
    Fragmented,
    #[error("UDP の長さが不正です")]
    BadUdpLength,
}

/// IPv4 / UDP パケットを読む。どんな入力でもパニックしない。
pub fn decode_ipv4_udp(bytes: &[u8]) -> Result<ReceivedUdp<'_>, IpDecodeError> {
    let fixed = bytes
        .get(..IPV4_HEADER_LEN)
        .ok_or(IpDecodeError::TooShort)?;
    let version = fixed[0] >> 4;
    if version != IP_VERSION_4 {
        return Err(IpDecodeError::NotIpv4(version));
    }
    let header_len = usize::from(fixed[0] & 0x0F) * 4;
    let total_len = usize::from(be_u16(&fixed[2..4]));
    if header_len < IPV4_HEADER_LEN || total_len < header_len {
        return Err(IpDecodeError::BadHeaderLength);
    }
    let packet = bytes.get(..total_len).ok_or(IpDecodeError::TooShort)?;
    let flags = be_u16(&fixed[6..8]);
    if flags & FLAG_MORE_FRAGMENTS != 0 || flags & FRAGMENT_OFFSET_MASK != 0 {
        return Err(IpDecodeError::Fragmented);
    }
    if fixed[9] != PROTOCOL_UDP {
        return Err(IpDecodeError::NotUdp(fixed[9]));
    }
    let udp = packet.get(header_len..).ok_or(IpDecodeError::TooShort)?;
    let udp_header = udp.get(..UDP_HEADER_LEN).ok_or(IpDecodeError::TooShort)?;
    let udp_len = usize::from(be_u16(&udp_header[4..6]));
    if udp_len < UDP_HEADER_LEN {
        return Err(IpDecodeError::BadUdpLength);
    }
    let udp_segment = udp.get(..udp_len).ok_or(IpDecodeError::BadUdpLength)?;

    let source_ip = Ipv4Addr::new(fixed[12], fixed[13], fixed[14], fixed[15]);
    let destination_ip = Ipv4Addr::new(fixed[16], fixed[17], fixed[18], fixed[19]);
    Ok(ReceivedUdp {
        source: SocketAddrV4::new(source_ip, be_u16(&udp_header[0..2])),
        destination: SocketAddrV4::new(destination_ip, be_u16(&udp_header[2..4])),
        dscp: fixed[1] >> 2,
        ecn: fixed[1] & 0b11,
        identification: be_u16(&fixed[4..6]),
        dont_fragment: flags & FLAG_DONT_FRAGMENT != 0,
        ttl: fixed[8],
        udp_checksum: be_u16(&udp_header[6..8]),
        udp_segment,
        payload: &udp_segment[UDP_HEADER_LEN..],
    })
}

/// インターネットチェックサム（RFC 1071）。チェックサム欄を含めて計算すると、正しければ 0 になる。
pub fn internet_checksum(data: &[u8]) -> u16 {
    fold(sum_words(data, 0))
}

/// UDP チェックサム（疑似ヘッダ込み）。`segment` は UDP ヘッダ + ペイロード。
/// チェックサム欄を含めて計算すると、正しければ 0 になる。
pub fn udp_checksum(source: Ipv4Addr, destination: Ipv4Addr, segment: &[u8]) -> u16 {
    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&source.octets());
    pseudo[4..8].copy_from_slice(&destination.octets());
    pseudo[9] = PROTOCOL_UDP;
    let len = u16::try_from(segment.len()).unwrap_or(u16::MAX);
    pseudo[10..12].copy_from_slice(&len.to_be_bytes());
    fold(sum_words(segment, sum_words(&pseudo, 0)))
}

fn sum_words(data: &[u8], initial: u64) -> u64 {
    data.chunks(2).fold(initial, |sum, chunk| {
        let high = chunk[0];
        let low = chunk.get(1).copied().unwrap_or(0);
        sum + u64::from(u16::from_be_bytes([high, low]))
    })
}

fn fold(mut sum: u64) -> u16 {
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddrV4 {
        text.parse().unwrap()
    }

    #[test]
    fn header_checksum_matches_known_vector() {
        // よく知られた例: 4500 0073 0000 4000 4011 [b861] c0a8 0001 c0a8 00c7
        let header = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        assert_eq!(internet_checksum(&header), 0xb861);
    }

    #[test]
    fn encodes_headers_with_valid_checksums() {
        let template = HeaderTemplate {
            ipv4: Ipv4Spec {
                dscp: 46,
                ecn: 1,
                identification: 0x1234,
                dont_fragment: true,
                ttl: 7,
                header_checksum: None,
            },
            udp_checksum: UdpChecksum::Compute,
        };
        let payload = [0x23u8; 48];
        let packet = encode_ipv4_udp(
            &template,
            addr("192.168.1.10:40000"),
            addr("192.168.92.234:123"),
            &payload,
        );

        assert_eq!(packet.len(), 20 + 8 + 48);
        assert_eq!(packet[0], 0x45);
        assert_eq!(packet[1], (46 << 2) | 1);
        assert_eq!(&packet[2..4], &76u16.to_be_bytes());
        assert_eq!(&packet[4..6], &[0x12, 0x34]);
        assert_eq!(&packet[6..8], &[0x40, 0x00]);
        assert_eq!(packet[8], 7);
        assert_eq!(packet[9], 17);
        assert_eq!(&packet[12..16], &[192, 168, 1, 10]);
        assert_eq!(&packet[16..20], &[192, 168, 92, 234]);
        // チェックサム欄を含めて計算し直すと 0（正しい）
        assert_eq!(internet_checksum(&packet[..20]), 0);
        assert_eq!(
            udp_checksum(
                Ipv4Addr::new(192, 168, 1, 10),
                Ipv4Addr::new(192, 168, 92, 234),
                &packet[20..]
            ),
            0
        );

        let received = decode_ipv4_udp(&packet).unwrap();
        assert_eq!(received.source, addr("192.168.1.10:40000"));
        assert_eq!(received.destination, addr("192.168.92.234:123"));
        assert_eq!((received.dscp, received.ecn, received.ttl), (46, 1, 7));
        assert_eq!(received.identification, 0x1234);
        assert!(received.dont_fragment);
        assert_eq!(received.payload, &payload);
    }

    #[test]
    fn fixed_header_checksum_is_sent_as_is() {
        let template = HeaderTemplate {
            ipv4: Ipv4Spec {
                header_checksum: Some(0x1234),
                ..Ipv4Spec::default()
            },
            ..HeaderTemplate::default()
        };
        let packet = encode_ipv4_udp(&template, addr("10.0.0.1:1"), addr("10.0.0.2:123"), b"x");
        assert_eq!(&packet[10..12], &[0x12, 0x34]);
    }

    #[test]
    fn udp_checksum_modes() {
        let encode = |udp_checksum| {
            let template = HeaderTemplate {
                udp_checksum,
                ..HeaderTemplate::default()
            };
            let packet = encode_ipv4_udp(&template, addr("10.0.0.1:1"), addr("10.0.0.2:123"), b"x");
            decode_ipv4_udp(&packet).unwrap().udp_checksum
        };
        assert_eq!(encode(UdpChecksum::Zero), 0);
        assert_eq!(encode(UdpChecksum::Fixed(0xBEEF)), 0xBEEF);
        assert_ne!(encode(UdpChecksum::Compute), 0);
    }

    #[test]
    fn rejects_fragments_and_other_protocols() {
        let mut packet = encode_ipv4_udp(
            &HeaderTemplate::default(),
            addr("10.0.0.1:1"),
            addr("10.0.0.2:2"),
            b"abc",
        );
        packet[6] |= 0x20; // MF
        assert_eq!(decode_ipv4_udp(&packet), Err(IpDecodeError::Fragmented));
        packet[6] &= !0x20;
        packet[9] = 6; // TCP
        assert_eq!(decode_ipv4_udp(&packet), Err(IpDecodeError::NotUdp(6)));
    }

    #[test]
    fn honours_ip_options() {
        // IHL = 6（オプション 4 バイト付き）のパケットを手で作る
        let base = encode_ipv4_udp(
            &HeaderTemplate::default(),
            addr("10.0.0.1:1000"),
            addr("10.0.0.2:123"),
            b"hello",
        );
        let mut packet = Vec::new();
        packet.extend_from_slice(&base[..20]);
        packet.extend_from_slice(&[1, 1, 1, 0]); // NOP, NOP, NOP, EOL
        packet.extend_from_slice(&base[20..]);
        packet[0] = 0x46;
        let total = u16::try_from(packet.len()).unwrap();
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        let received = decode_ipv4_udp(&packet).unwrap();
        assert_eq!(received.payload, b"hello");
        assert_eq!(received.source.port(), 1000);
    }

    #[test]
    fn never_panics_on_truncated_or_garbage_input() {
        let packet = encode_ipv4_udp(
            &HeaderTemplate::default(),
            addr("10.0.0.1:1"),
            addr("10.0.0.2:2"),
            &[7u8; 48],
        );
        for len in 0..packet.len() {
            assert!(
                decode_ipv4_udp(&packet[..len]).is_err(),
                "長さ {len} で成功してしまった"
            );
        }
        // 長さ欄などを壊したパケット
        for index in 0..packet.len() {
            for value in [0x00, 0x0F, 0x45, 0x4F, 0xFF] {
                let mut broken = packet.clone();
                broken[index] = value;
                let _ = decode_ipv4_udp(&broken);
            }
        }
    }
}
