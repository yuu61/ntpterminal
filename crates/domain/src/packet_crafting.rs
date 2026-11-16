//! パケット生成コンテキスト（支援サブドメイン）。
//!
//! NTP メッセージを「何でも」表現できる。規格外の値・不正な値もそのまま作れるし、そのまま読める。
//! 正しさの検証はここでは行わない（それは時刻同期コンテキストの責務）。
//!
//! IP / UDP / Ethernet ヘッダの指定は M3 以降でこのコンテキストに追加する。

mod codec;
mod message;

pub use codec::{DecodeError, HEADER_LEN, decode, encode};
pub use message::{NtpMessage, RequestTemplate};
