//! Ethernet フレーム（802.1Q の VLAN タグを含む）と ARP の組み立て・解析（link モード用）。
//!
//! 解析は任意の受信フレームに対して行うので、どんな入力でもパニックしない。

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

/// Ethernet ヘッダの長さ（VLAN タグなし）。
pub const ETHERNET_HEADER_LEN: usize = 14;
/// 802.1Q VLAN タグの長さ。
pub const VLAN_TAG_LEN: usize = 4;
/// ARP パケット（Ethernet / IPv4）の長さ。
pub const ARP_LEN: usize = 28;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_VLAN: u16 = 0x8100;

/// MAC アドレス。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const ZERO: Self = Self([0; 6]);
    pub const BROADCAST: Self = Self([0xFF; 6]);
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

/// MAC アドレスとして読めなかった。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("MAC アドレスは aa:bb:cc:dd:ee:ff の形式で指定してください")]
pub struct MacParseError;

impl FromStr for MacAddr {
    type Err = MacParseError;

    /// "aa:bb:cc:dd:ee:ff" または "aa-bb-cc-dd-ee-ff"。
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = text.split([':', '-']).collect();
        if parts.len() != 6 {
            return Err(MacParseError);
        }
        let mut bytes = [0u8; 6];
        for (byte, part) in bytes.iter_mut().zip(parts) {
            if part.len() != 2 {
                return Err(MacParseError);
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| MacParseError)?;
        }
        Ok(Self(bytes))
    }
}

/// 802.1Q の VLAN タグ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VlanTag {
    /// 優先度（PCP、3 bit）
    pub pcp: u8,
    /// DEI（1 bit）
    pub dei: bool,
    /// VLAN ID（12 bit）
    pub vid: u16,
}

impl VlanTag {
    fn to_tci(self) -> u16 {
        (u16::from(self.pcp & 0b111) << 13) | (u16::from(self.dei) << 12) | (self.vid & 0x0FFF)
    }

    fn from_tci(tci: u16) -> Self {
        Self {
            pcp: (tci >> 13) as u8,
            dei: tci & 0x1000 != 0,
            vid: tci & 0x0FFF,
        }
    }
}

/// Ethernet ヘッダの指定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EthernetSpec {
    /// `Some` なら 802.1Q の VLAN タグを付ける
    pub vlan: Option<VlanTag>,
}

/// Ethernet フレームを組み立てる。最小長（60 バイト）への詰め物はドライバに任せる。
pub fn encode_ethernet(
    spec: &EthernetSpec,
    destination: MacAddr,
    source: MacAddr,
    ethertype: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + VLAN_TAG_LEN + payload.len());
    frame.extend_from_slice(&destination.0);
    frame.extend_from_slice(&source.0);
    if let Some(vlan) = spec.vlan {
        frame.extend_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
        frame.extend_from_slice(&vlan.to_tci().to_be_bytes());
    }
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// 受信した Ethernet フレームの中身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedFrame<'a> {
    pub destination: MacAddr,
    pub source: MacAddr,
    /// VLAN タグ（NIC がタグを外して渡すこともあるので、付いていないこともある）
    pub vlan: Option<VlanTag>,
    pub ethertype: u16,
    /// ヘッダより後ろ（詰め物を含むことがある）
    pub payload: &'a [u8],
}

/// Ethernet として読めなかった。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameDecodeError {
    #[error("フレームが短すぎます")]
    TooShort,
}

/// Ethernet フレームを読む（VLAN タグは 1 段まで）。どんな入力でもパニックしない。
pub fn decode_ethernet(frame: &[u8]) -> Result<ReceivedFrame<'_>, FrameDecodeError> {
    let header = frame
        .get(..ETHERNET_HEADER_LEN)
        .ok_or(FrameDecodeError::TooShort)?;
    let destination = MacAddr(mac_at(header, 0));
    let source = MacAddr(mac_at(header, 6));
    let outer_type = be_u16(&header[12..14]);
    if outer_type == ETHERTYPE_VLAN {
        let tag = frame
            .get(ETHERNET_HEADER_LEN..ETHERNET_HEADER_LEN + VLAN_TAG_LEN)
            .ok_or(FrameDecodeError::TooShort)?;
        return Ok(ReceivedFrame {
            destination,
            source,
            vlan: Some(VlanTag::from_tci(be_u16(&tag[0..2]))),
            ethertype: be_u16(&tag[2..4]),
            payload: &frame[ETHERNET_HEADER_LEN + VLAN_TAG_LEN..],
        });
    }
    Ok(ReceivedFrame {
        destination,
        source,
        vlan: None,
        ethertype: outer_type,
        payload: &frame[ETHERNET_HEADER_LEN..],
    })
}

/// ARP の操作。
pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;

/// ARP パケット（Ethernet / IPv4 のもの）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub operation: u16,
    pub sender_mac: MacAddr,
    pub sender_ip: Ipv4Addr,
    pub target_mac: MacAddr,
    pub target_ip: Ipv4Addr,
}

impl ArpPacket {
    /// 「`target_ip` を持っているのは誰？」という問い合わせ。
    pub fn request(sender_mac: MacAddr, sender_ip: Ipv4Addr, target_ip: Ipv4Addr) -> Self {
        Self {
            operation: ARP_REQUEST,
            sender_mac,
            sender_ip,
            target_mac: MacAddr::ZERO,
            target_ip,
        }
    }
}

/// ARP パケットを組み立てる。
pub fn encode_arp(packet: &ArpPacket) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ARP_LEN);
    bytes.extend_from_slice(&1u16.to_be_bytes()); // ハードウェア種別: Ethernet
    bytes.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes()); // プロトコル種別: IPv4
    bytes.push(6); // ハードウェアアドレス長
    bytes.push(4); // プロトコルアドレス長
    bytes.extend_from_slice(&packet.operation.to_be_bytes());
    bytes.extend_from_slice(&packet.sender_mac.0);
    bytes.extend_from_slice(&packet.sender_ip.octets());
    bytes.extend_from_slice(&packet.target_mac.0);
    bytes.extend_from_slice(&packet.target_ip.octets());
    bytes
}

/// ARP として読めなかった。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArpDecodeError {
    #[error("ARP パケットが短すぎます")]
    TooShort,
    #[error("Ethernet / IPv4 の ARP ではありません")]
    Unsupported,
}

/// ARP パケットを読む（Ethernet / IPv4 のものだけ）。どんな入力でもパニックしない。
pub fn decode_arp(bytes: &[u8]) -> Result<ArpPacket, ArpDecodeError> {
    let arp = bytes.get(..ARP_LEN).ok_or(ArpDecodeError::TooShort)?;
    if be_u16(&arp[0..2]) != 1 || be_u16(&arp[2..4]) != ETHERTYPE_IPV4 || arp[4] != 6 || arp[5] != 4
    {
        return Err(ArpDecodeError::Unsupported);
    }
    Ok(ArpPacket {
        operation: be_u16(&arp[6..8]),
        sender_mac: MacAddr(mac_at(arp, 8)),
        sender_ip: Ipv4Addr::new(arp[14], arp[15], arp[16], arp[17]),
        target_mac: MacAddr(mac_at(arp, 18)),
        target_ip: Ipv4Addr::new(arp[24], arp[25], arp[26], arp[27]),
    })
}

fn mac_at(bytes: &[u8], offset: usize) -> [u8; 6] {
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&bytes[offset..offset + 6]);
    mac
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DST: MacAddr = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    const SRC: MacAddr = MacAddr([0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB]);

    #[test]
    fn parses_and_prints_mac_addresses() {
        assert_eq!("00:11:22:33:44:55".parse(), Ok(DST));
        assert_eq!("66-77-88-99-aa-bb".parse(), Ok(SRC));
        assert_eq!(SRC.to_string(), "66:77:88:99:aa:bb");
        assert!("00:11:22:33:44".parse::<MacAddr>().is_err());
        assert!("00:11:22:33:44:5".parse::<MacAddr>().is_err());
        assert!("00:11:22:33:44:zz".parse::<MacAddr>().is_err());
    }

    #[test]
    fn encodes_untagged_frame_exactly() {
        let frame = encode_ethernet(&EthernetSpec::default(), DST, SRC, ETHERTYPE_IPV4, b"ip");
        assert_eq!(
            frame,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0x08, 0x00,
                b'i', b'p'
            ]
        );
        let decoded = decode_ethernet(&frame).unwrap();
        assert_eq!((decoded.destination, decoded.source), (DST, SRC));
        assert_eq!(decoded.vlan, None);
        assert_eq!(decoded.ethertype, ETHERTYPE_IPV4);
        assert_eq!(decoded.payload, b"ip");
    }

    #[test]
    fn encodes_vlan_tag_bits() {
        let spec = EthernetSpec {
            vlan: Some(VlanTag {
                pcp: 6,
                dei: true,
                vid: 0x123,
            }),
        };
        let frame = encode_ethernet(&spec, DST, SRC, ETHERTYPE_IPV4, b"x");
        // TPID 0x8100、TCI = PCP 6 (110) / DEI 1 / VID 0x123 = 0xD123
        assert_eq!(&frame[12..18], &[0x81, 0x00, 0xD1, 0x23, 0x08, 0x00]);
        let decoded = decode_ethernet(&frame).unwrap();
        assert_eq!(decoded.vlan, spec.vlan);
        assert_eq!(decoded.ethertype, ETHERTYPE_IPV4);
        assert_eq!(decoded.payload, b"x");
    }

    #[test]
    fn encodes_arp_request_exactly() {
        let request =
            ArpPacket::request(SRC, Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(
            encode_arp(&request),
            [
                0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01, // Ethernet / IPv4 / 要求
                0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 10, 0, 0, 1, // 送信元
                0, 0, 0, 0, 0, 0, 10, 0, 0, 2, // 宛先（MAC は不明なので 0）
            ]
        );
        assert_eq!(decode_arp(&encode_arp(&request)), Ok(request));
    }

    #[test]
    fn decodes_padded_arp_reply() {
        let reply = ArpPacket {
            operation: ARP_REPLY,
            sender_mac: DST,
            sender_ip: Ipv4Addr::new(10, 0, 0, 2),
            target_mac: SRC,
            target_ip: Ipv4Addr::new(10, 0, 0, 1),
        };
        let mut bytes = encode_arp(&reply);
        bytes.extend_from_slice(&[0; 18]); // 最小フレーム長までの詰め物
        assert_eq!(decode_arp(&bytes), Ok(reply));
    }

    #[test]
    fn never_panics_on_truncated_input() {
        let spec = EthernetSpec {
            vlan: Some(VlanTag {
                pcp: 0,
                dei: false,
                vid: 1,
            }),
        };
        let frame = encode_ethernet(
            &spec,
            DST,
            SRC,
            ETHERTYPE_ARP,
            &encode_arp(&ArpPacket::request(
                SRC,
                Ipv4Addr::LOCALHOST,
                Ipv4Addr::LOCALHOST,
            )),
        );
        for len in 0..frame.len() {
            if let Ok(decoded) = decode_ethernet(&frame[..len]) {
                let _ = decode_arp(decoded.payload);
            }
        }
        assert!(decode_ethernet(&frame[..13]).is_err());
        assert!(decode_ethernet(&frame[..17]).is_err());
        assert!(decode_arp(&[0u8; 27]).is_err());
    }
}
