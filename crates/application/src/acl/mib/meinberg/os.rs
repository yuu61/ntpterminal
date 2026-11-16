//! MEINBERG-OS-MIB（`enterprises.5597.7`。`meinbergOS` の機器）。
//!
//! - 受信機の表（`mbgOsReceiverTable`）は CLK1・CLK2 の 2 行。`IsGnss` が yes の行を使い、なければ状態の
//!   値がある最初の行を使う（衛星の数は出さない）
//! - 衛星の数: `GoodSv`（見えていて使える衛星）と `Sv`（見えるはずの衛星。受信しているとは限らない）。
//!   どちらも `IsGnss` が yes のときだけ有効
//! - ホールドオーバー: `mbgOsSysrefGlbStateMasterRefBits` の bit 7（`inHoldoverAfterSync`）

use domain::receiver::{AntennaState, ReceiverState, ReceiverStatus, SatelliteCounts};
use domain::snmp::ObjectId;

use super::{ROWS, oid};
use crate::acl::mib::{Values, enum_name};

/// `mbgOsReceiverTableEntry`（`5597.7.2.3.1.1`）
pub const RECEIVER_ENTRY: [u32; 5] = [7, 2, 3, 1, 1];
pub const COLUMN_SLOT: u32 = 2;
pub const COLUMN_STATE: u32 = 3;
pub const COLUMN_IS_GNSS: u32 = 4;
pub const COLUMN_GOOD_SV: u32 = 6;
pub const COLUMN_SV: u32 = 7;
/// `mbgOsSysrefGlbStateMasterRefBits.0`
pub const MASTER_REF_BITS: [u32; 7] = [7, 2, 4, 2, 1, 2, 0];
/// `mbgOsSysrefGlbStateSysSync.0`（MIB の判定に使う）
pub const SYS_SYNC: [u32; 7] = [7, 2, 4, 2, 1, 5, 0];
/// `MasterRefBits` の `inHoldoverAfterSync`
pub const BIT_IN_HOLDOVER_AFTER_SYNC: usize = 7;

/// `YesNo` の yes
pub const YES: i64 = 1;
/// `mbgOsReceiverState` の値と名前
pub const STATES: [(i64, &str); 7] = [
    (0, "noData"),
    (1, "waitingForData"),
    (2, "antennaShortCircuit"),
    (3, "antennaDisconnected"),
    (4, "coldBoot"),
    (5, "warmBoot"),
    (6, "synchronized"),
];

/// 受信機の表の列。
#[must_use]
pub fn column(column: u32, row: u32) -> ObjectId {
    oid(&RECEIVER_ENTRY).with_suffix(&[column, row])
}

pub(crate) fn probe_oid() -> ObjectId {
    oid(&SYS_SYNC)
}

pub(crate) fn status_oids() -> Vec<ObjectId> {
    let mut oids: Vec<ObjectId> = ROWS
        .iter()
        .flat_map(|&row| {
            [
                COLUMN_SLOT,
                COLUMN_STATE,
                COLUMN_IS_GNSS,
                COLUMN_GOOD_SV,
                COLUMN_SV,
            ]
            .map(|number| column(number, row))
        })
        .collect();
    oids.push(oid(&MASTER_REF_BITS));
    oids
}

pub(crate) fn interpret(values: &Values<'_>) -> ReceiverStatus {
    let holdover = values.bit(&oid(&MASTER_REF_BITS), BIT_IN_HOLDOVER_AFTER_SYNC);
    let is_gnss = |row: u32| values.integer(&column(COLUMN_IS_GNSS, row)) == Some(YES);
    let chosen = ROWS.into_iter().find(|&row| is_gnss(row)).or_else(|| {
        ROWS.into_iter()
            .find(|&row| values.integer(&column(COLUMN_STATE, row)).is_some())
    });
    let Some(row) = chosen else {
        return ReceiverStatus {
            holdover,
            ..ReceiverStatus::default()
        };
    };
    let state = values.integer(&column(COLUMN_STATE, row));
    ReceiverStatus {
        name: values.text(&column(COLUMN_SLOT, row)),
        state: state.and_then(classify),
        state_detail: state.map(|value| enum_name(&STATES, value)),
        antenna: state.and_then(antenna),
        holdover,
        satellites: if is_gnss(row) {
            SatelliteCounts {
                good: values.count(&column(COLUMN_GOOD_SV, row)),
                expected: values.count(&column(COLUMN_SV, row)),
                ..SatelliteCounts::default()
            }
        } else {
            SatelliteCounts::default()
        },
        constellations: Vec::new(),
    }
}

const fn classify(state: i64) -> Option<ReceiverState> {
    match state {
        6 => Some(ReceiverState::Synchronized),
        1 | 4 | 5 => Some(ReceiverState::Acquiring),
        2 | 3 => Some(ReceiverState::NotSynchronized),
        _ => None,
    }
}

/// 同期できているならアンテナはつながっている。それ以外はアンテナの異常のときだけわかる。
const fn antenna(state: i64) -> Option<AntennaState> {
    match state {
        6 => Some(AntennaState::Connected),
        2 => Some(AntennaState::ShortCircuit),
        3 => Some(AntennaState::Disconnected),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use domain::snmp::{SnmpValue, VarBind};

    use super::*;
    use crate::acl::mib::MibProfile;
    use crate::acl::mib::testing::{text, varbind};

    /// CLK1 は GNSS 受信機で同期済み（良好 24・見えるはず 33）、CLK2 は空き。
    fn synchronized_gnss() -> Vec<VarBind> {
        vec![
            varbind(&column(COLUMN_SLOT, 1), text("CLK1")),
            varbind(&column(COLUMN_STATE, 1), SnmpValue::Integer(6)),
            varbind(&column(COLUMN_IS_GNSS, 1), SnmpValue::Integer(YES)),
            varbind(&column(COLUMN_GOOD_SV, 1), SnmpValue::Integer(24)),
            varbind(&column(COLUMN_SV, 1), SnmpValue::Integer(33)),
            varbind(&column(COLUMN_STATE, 2), SnmpValue::NoSuchInstance),
            varbind(
                &oid(&MASTER_REF_BITS),
                SnmpValue::OctetString(vec![0x00, 0x30]),
            ),
        ]
    }

    #[test]
    fn reads_good_and_expected_satellites() {
        let status = MibProfile::MeinbergOs
            .interpret(&synchronized_gnss())
            .unwrap();
        assert_eq!(status.name.as_deref(), Some("CLK1"));
        assert_eq!(status.state, Some(ReceiverState::Synchronized));
        assert_eq!(status.state_detail.as_deref(), Some("synchronized"));
        assert_eq!(status.antenna, Some(AntennaState::Connected));
        assert_eq!(status.holdover, Some(false));
        assert_eq!(
            status.satellites,
            SatelliteCounts {
                good: Some(24),
                expected: Some(33),
                in_view: None,
                tracking: None,
            }
        );
    }

    #[test]
    fn holdover_is_bit_7_of_the_first_byte() {
        let bits = oid(&MASTER_REF_BITS);
        let varbinds: Vec<VarBind> = synchronized_gnss()
            .into_iter()
            .map(|item| {
                if item.name == bits {
                    varbind(&bits, SnmpValue::OctetString(vec![0x01, 0x00]))
                } else {
                    item
                }
            })
            .collect();
        let status = MibProfile::MeinbergOs.interpret(&varbinds).unwrap();
        assert_eq!(status.holdover, Some(true));
    }

    #[test]
    fn uses_the_gnss_row_and_ignores_counts_of_other_receivers() {
        // CLK1 は長波（GNSS ではない）、CLK2 が GNSS でアンテナ断線
        let varbinds = [
            varbind(&column(COLUMN_STATE, 1), SnmpValue::Integer(6)),
            varbind(&column(COLUMN_IS_GNSS, 1), SnmpValue::Integer(0)),
            varbind(&column(COLUMN_GOOD_SV, 1), SnmpValue::Integer(0)),
            varbind(&column(COLUMN_STATE, 2), SnmpValue::Integer(3)),
            varbind(&column(COLUMN_IS_GNSS, 2), SnmpValue::Integer(YES)),
            varbind(&column(COLUMN_GOOD_SV, 2), SnmpValue::Integer(0)),
            varbind(&column(COLUMN_SV, 2), SnmpValue::Integer(31)),
        ];
        let status = MibProfile::MeinbergOs.interpret(&varbinds).unwrap();
        assert_eq!(status.state, Some(ReceiverState::NotSynchronized));
        assert_eq!(status.antenna, Some(AntennaState::Disconnected));
        assert_eq!(status.satellites.good, Some(0));
        assert_eq!(status.satellites.expected, Some(31));
        assert_eq!(status.holdover, None);

        // GNSS の行がなければ、状態だけを出して衛星の数は出さない
        let without_gnss = MibProfile::MeinbergOs.interpret(&varbinds[..3]).unwrap();
        assert_eq!(without_gnss.state, Some(ReceiverState::Synchronized));
        assert!(without_gnss.satellites.is_empty());
    }

    #[test]
    fn unknown_states_keep_the_number() {
        let varbinds = [varbind(&column(COLUMN_STATE, 1), SnmpValue::Integer(99))];
        let status = MibProfile::MeinbergOs.interpret(&varbinds).unwrap();
        assert_eq!(status.state, None);
        assert_eq!(status.state_detail.as_deref(), Some("99"));
    }
}
