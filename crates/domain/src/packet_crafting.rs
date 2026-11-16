//! パケット生成コンテキスト（支援サブドメイン）。
//!
//! NTP メッセージを「何でも」表現できる。規格外の値・不正な値もそのまま作れるし、そのまま読める。
//! 正しさの検証はここでは行わない（それは時刻同期コンテキストの責務）。
//!
//! IPv4 / UDP ヘッダの組み立て・解析は raw-ip モード（M3）用。Ethernet は M4 で追加する。

mod codec;
mod ipv4_udp;
mod message;

pub use codec::{DecodeError, HEADER_LEN, decode, encode};
pub use ipv4_udp::{
    HeaderTemplate, IPV4_HEADER_LEN, IpDecodeError, Ipv4Spec, ReceivedUdp, UDP_HEADER_LEN,
    UdpChecksum, decode_ipv4_udp, encode_ipv4_udp, internet_checksum, udp_checksum,
};
pub use message::{NtpMessage, RequestTemplate};
