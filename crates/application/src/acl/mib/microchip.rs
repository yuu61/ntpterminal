//! Microchip（旧 Symmetricom・Microsemi。`enterprises.9070`）の MIB。
//!
//! - [`s650`] … S650（SyncServer S650）

pub mod s650;

use domain::snmp::ObjectId;

/// `enterprises.9070`
pub const ENTERPRISE: [u32; 7] = [1, 3, 6, 1, 4, 1, 9070];

/// `enterprises.9070` の下の OID。
#[must_use]
pub fn oid(suffix: &[u32]) -> ObjectId {
    ObjectId::new(&ENTERPRISE).with_suffix(suffix)
}
