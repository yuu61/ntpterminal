//! ドメイン層。I/O・OS・時計の読み取りに一切依存しない純粋なモデル。
//!
//! - [`shared_kernel`] … 両コンテキストで共有する最小限の型（NTP 時刻・時間差・PC 側の時刻）
//! - [`packet_crafting`] … パケット生成コンテキスト（支援サブドメイン）。不正な値も表現できる
//! - [`time_sync`] … 時刻同期コンテキスト（コアドメイン）。検証済みの値だけを扱う
//! - [`snmp`] … SNMP コンテキスト（支援サブドメイン）。SNMP v2c のメッセージと BER の相互変換
//! - [`receiver`] … 受信機コンテキスト。タイムサーバの GNSS 受信機の状態（SNMP で読んだ値の解釈結果）
//!
//! コンテキスト間の独立は規約で守る: `time_sync` から `packet_crafting` を、`receiver` から `snmp` を参照しない。
//! 両者の翻訳は application 層の ACL（腐敗防止層）が担う。
#![forbid(unsafe_code)]

extern crate alloc;

pub mod packet_crafting;
pub mod receiver;
pub mod shared_kernel;
pub mod snmp;
pub mod time_sync;
