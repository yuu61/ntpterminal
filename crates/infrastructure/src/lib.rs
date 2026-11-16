//! インフラ層: application 層のポートの実装。
//!
//! OS 固有のコードはこのクレートにだけ置く（`#[cfg(target_os = "...")]` で分ける）。
//! unsafe が必要になる処理（M4 のカーネル / NIC タイムスタンプなど）もここだけで許可する。

pub mod clock;
pub mod nonce;
pub mod resolve;
pub mod sim_server;
pub mod transport;
