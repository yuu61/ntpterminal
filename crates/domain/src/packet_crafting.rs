//! パケット生成コンテキスト（支援サブドメイン）。
//!
//! NTP メッセージを「何でも」表現できる。規格外の値・不正な値もそのまま作れるし、そのまま読める。
//! 正しさの検証はここでは行わない（それは時刻同期コンテキストの責務）。
//!
//! IPv4 / UDP ヘッダは raw-ip / link モード用、Ethernet・VLAN・ARP は link モード用。

mod codec;
mod ethernet;
mod ipv4_udp;
mod message;

pub use codec::{DecodeError, HEADER_LEN, decode, encode};
pub use ethernet::{
    ARP_LEN, ARP_REPLY, ARP_REQUEST, ArpDecodeError, ArpPacket, ETHERNET_HEADER_LEN, ETHERTYPE_ARP,
    ETHERTYPE_IPV4, ETHERTYPE_VLAN, EthernetSpec, FrameDecodeError, MacAddr, MacParseError,
    ReceivedFrame, VLAN_TAG_LEN, VlanTag, decode_arp, decode_ethernet, encode_arp, encode_ethernet,
};
pub use ipv4_udp::{
    HeaderTemplate, IPV4_HEADER_LEN, IpDecodeError, Ipv4Spec, ReceivedUdp, UDP_HEADER_LEN,
    UdpChecksum, decode_ipv4_udp, encode_ipv4_udp, internet_checksum, udp_checksum,
};
pub use message::{NtpMessage, RequestTemplate};
