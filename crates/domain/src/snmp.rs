//! SNMP コンテキスト（支援サブドメイン）。SNMP v2c のメッセージと BER（X.690）の相互変換。
//!
//! タイムサーバの GNSS 受信機の状態を、SNMP の GET で読むために使う。
//!
//! - 扱うのは v2c のメッセージ（`SEQUENCE { version, community, PDU }`）と、GET・GETNEXT・Response などの
//!   PDU だけ。v1 の Trap（形式が違う）と v3（USM・暗号）は扱わない
//! - パケット生成コンテキストと同じく、不正な値も作れるし、読めるものはそのまま読む。値の意味
//!   （どの OID が何を表すか）はここでは扱わない（メーカーの MIB の翻訳は application 層の ACL）
//! - 読むのはネットワークから届いたバイト列なので、どんな入力でもパニックせず `Err` を返す。
//!   長さは 4 バイトまでの長形式を受け付け、不定長は受け付けない

mod ber;
mod message;
mod object_id;

pub use ber::{DecodeError, decode, encode};
pub use message::{Pdu, PduKind, SnmpMessage, SnmpValue, VarBind};
pub use object_id::{ObjectId, ObjectIdParseError};
