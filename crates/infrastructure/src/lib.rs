//! インフラ層: application 層のポートの実装。
//!
//! OS 固有のコードはこのクレートにだけ置く（`#[cfg(target_os = "...")]` で分ける）。
//! unsafe もここだけで許可する。

extern crate alloc;

pub mod clock;
pub mod nonce;
#[cfg(target_os = "linux")]
mod phc;
pub mod ptp4l;
pub mod resolve;
pub mod sim_server;
pub mod transport;
