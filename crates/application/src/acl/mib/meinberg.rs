//! Meinberg（`enterprises.5597`）の MIB。ファームウェアの世代ごとに MIB が違う。
//!
//! - [`os`] … MEINBERG-OS-MIB（`meinbergOS` の機器）
//! - [`ltng`] … MBG-SNMP-LTNG-MIB（LANTIME の現行ファームウェア）
//! - [`lt`] … MBG-SNMP-LT-MIB（旧 LANTIME の V5 系）

pub mod lt;
pub mod ltng;
pub mod os;

use domain::snmp::ObjectId;

/// `enterprises.5597`
pub const ENTERPRISE: [u32; 7] = [1, 3, 6, 1, 4, 1, 5597];

/// 受信機（参照時計）の表の行。CLK1・CLK2 の 2 台まで。
pub const ROWS: [u32; 2] = [1, 2];

/// `enterprises.5597` の下の OID。
#[must_use]
pub fn oid(suffix: &[u32]) -> ObjectId {
    ObjectId::new(&ENTERPRISE).with_suffix(suffix)
}
