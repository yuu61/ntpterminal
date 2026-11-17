//! アプリケーション層: ユースケースと、外側（infrastructure）に実装させるポート（trait）。
//!
//! I/O は直接行わず、すべてポート経由で行う。
#![forbid(unsafe_code)]

extern crate alloc;

pub mod acl;
pub mod description;
pub mod polling_policy;
pub mod port;
pub mod snapshot;
pub mod timestamping;
pub mod usecase;
