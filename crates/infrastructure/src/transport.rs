//! パケットの送受信（[`application::port::PacketPort`] の実装）。
//!
//! - `udp`: 標準ライブラリの UDP ソケット。権限不要・全 OS 対応（M1）
//! - `raw_ip`: raw ソケットで IP / UDP ヘッダから作る（M3 予定）
//! - `link`: Ethernet フレームから作る（M4 予定）

mod udp;

pub use udp::UdpTransport;
