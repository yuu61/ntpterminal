//! MBG-SNMP-LTNG-MIB（`enterprises.5597.30`。LANTIME の現行ファームウェア）。
//!
//! - 参照時計の表（`mbgLtNgRefclockTable`）は 2 行。`Usage` が primary の行を使い、なければ状態の値がある
//!   最初の行を使う
//! - `StatusA` / `MaxStatusA` が衛星の数（良好・可視）を表すのは、参照時計が GNSS のときだけ（長波の PZF では
//!   相関値）。GNSS かどうかは `Type` で決め、型の名前で GNSS とわかるもの（gps・gns・gln で始まるもの）だけを
//!   GNSS とする。grc などは GNSS か確かめられていないので、衛星の数を出さない
//! - MIB の判定には、参照時計の表の 1 行目の `State` を使う（どのみち読むのはこの表なので。LANTIME V6 の実機の
//!   記録には、NTP の状態のスカラー値 `mbgLtNgNtpCurrentState` が出てこない）
//! - ホールドオーバーの値はない

use domain::receiver::{AntennaState, ReceiverState, ReceiverStatus, SatelliteCounts};
use domain::snmp::ObjectId;

use super::{ROWS, oid};
use crate::acl::mib::{Values, enum_name};

/// `mbgLtNgRefclockTableEntry`（`5597.30.0.1.2.1`）
pub const REFCLOCK_ENTRY: [u32; 5] = [30, 0, 1, 2, 1];
pub const COLUMN_TYPE: u32 = 2;
pub const COLUMN_USAGE: u32 = 3;
pub const COLUMN_STATE: u32 = 4;
pub const COLUMN_SUBSTATE: u32 = 5;
pub const COLUMN_STATUS_A: u32 = 6;
pub const COLUMN_MAX_STATUS_A: u32 = 7;

/// `mbgLtNgRefclockUsage` の primary
pub const USAGE_PRIMARY: i64 = 3;
/// `mbgLtNgRefclockState` の値と名前
pub const STATES: [(i64, &str); 3] = [
    (0, "notAvailable"),
    (1, "synchronized"),
    (2, "notSynchronized"),
];
/// `mbgLtNgRefclockSubstate` の値と名前
pub const SUBSTATES: [(i64, &str); 32] = [
    (-1, "mrsRefNone"),
    (0, "notAvailable"),
    (1, "gpsSync"),
    (2, "gpsTracking"),
    (3, "gpsAntennaDisconnected"),
    (4, "gpsWarmBoot"),
    (5, "gpsColdBoot"),
    (6, "gpsAntennaShortCircuit"),
    (50, "lwNeverSync"),
    (51, "lwNotSync"),
    (52, "lwSync"),
    (100, "tcrNotSync"),
    (101, "tcrSync"),
    (149, "mrsIntOscSync"),
    (150, "mrsGpsSync"),
    (151, "mrs10MhzSync"),
    (152, "mrsPpsInSync"),
    (153, "mrs10MhzPpsInSync"),
    (154, "mrsIrigSync"),
    (155, "mrsNtpSync"),
    (156, "mrsPtpIeee1588Sync"),
    (157, "mrsPtpOverE1Sync"),
    (158, "mrsFixedFreqInSync"),
    (159, "mrsPpsStringSync"),
    (160, "mrsVarFreqGpioSync"),
    (161, "mrsReserved"),
    (162, "mrsDcf77PzfSync"),
    (163, "mrsLongwaveSync"),
    (164, "mrsGlonassGpsSync"),
    (165, "mrsHavequickSync"),
    (166, "mrsExtOscSync"),
    (167, "mrsSyncE"),
];
/// GNSS の参照時計の型（`mbgLtNgRefclockType` の値と名前）。名前が gps・gns・gln で始まるもの
pub const GNSS_TYPES: [(i64, &str); 35] = [
    (1, "gps166"),
    (2, "gps167"),
    (3, "gps167SV"),
    (4, "gps167PC"),
    (5, "gps167PCI"),
    (6, "gps163"),
    (7, "gps168PCI"),
    (8, "gps161"),
    (9, "gps169PCI"),
    (11, "gps164"),
    (12, "gps170PCI"),
    (14, "gps170"),
    (19, "gps170PEX"),
    (20, "gps162"),
    (30, "gps180"),
    (31, "gln170"),
    (32, "gps180PEX"),
    (39, "gps180HS"),
    (40, "gps180AMC"),
    (50, "gln180Pex"),
    (78, "gps180csm"),
    (81, "gns180pex"),
    (84, "gps165"),
    (85, "gns181uc"),
    (91, "gps190"),
    (92, "gns181"),
    (97, "gns165"),
    (99, "gps16x"),
    (114, "gns190"),
    (115, "gps180msbc"),
    (116, "gns181msbc"),
    (117, "gns181ucmsbc"),
    (126, "gns190uc"),
    (129, "gns191"),
    (131, "gps182"),
];

/// 参照時計の表の列。
#[must_use]
pub fn column(column: u32, row: u32) -> ObjectId {
    oid(&REFCLOCK_ENTRY).with_suffix(&[column, row])
}

pub(crate) fn probe_oid() -> ObjectId {
    column(COLUMN_STATE, ROWS[0])
}

pub(crate) fn status_oids() -> Vec<ObjectId> {
    ROWS.iter()
        .flat_map(|&row| {
            [
                COLUMN_TYPE,
                COLUMN_USAGE,
                COLUMN_STATE,
                COLUMN_SUBSTATE,
                COLUMN_STATUS_A,
                COLUMN_MAX_STATUS_A,
            ]
            .map(|number| column(number, row))
        })
        .collect()
}

pub(crate) fn interpret(values: &Values<'_>) -> ReceiverStatus {
    let present = |row: &u32| values.integer(&column(COLUMN_STATE, *row)).is_some();
    let chosen = ROWS
        .into_iter()
        .filter(present)
        .find(|&row| values.integer(&column(COLUMN_USAGE, row)) == Some(USAGE_PRIMARY))
        .or_else(|| ROWS.into_iter().find(present));
    let Some(row) = chosen else {
        return ReceiverStatus::default();
    };
    let state = values.integer(&column(COLUMN_STATE, row));
    let substate = values.integer(&column(COLUMN_SUBSTATE, row));
    let gnss_type = values
        .integer(&column(COLUMN_TYPE, row))
        .and_then(|value| GNSS_TYPES.iter().find(|&&(number, _)| number == value));
    ReceiverStatus {
        name: gnss_type.map(|&(_, name)| name.to_owned()),
        state: state.and_then(|value| classify(value, substate)),
        state_detail: match (state, substate) {
            (_, Some(detail)) if detail != 0 => Some(enum_name(&SUBSTATES, detail)),
            (Some(value), _) => Some(enum_name(&STATES, value)),
            (None, _) => None,
        },
        antenna: substate.and_then(antenna),
        holdover: None,
        satellites: if gnss_type.is_some() {
            SatelliteCounts {
                good: values.count(&column(COLUMN_STATUS_A, row)),
                in_view: values.count(&column(COLUMN_MAX_STATUS_A, row)),
                ..SatelliteCounts::default()
            }
        } else {
            SatelliteCounts::default()
        },
        constellations: Vec::new(),
    }
}

const fn classify(state: i64, substate: Option<i64>) -> Option<ReceiverState> {
    match (state, substate) {
        (1, _) => Some(ReceiverState::Synchronized),
        (2, Some(2 | 4 | 5)) => Some(ReceiverState::Acquiring),
        (2, _) => Some(ReceiverState::NotSynchronized),
        _ => None,
    }
}

/// 同期・追尾できているならアンテナはつながっている。それ以外はアンテナの異常のときだけわかる。
const fn antenna(substate: i64) -> Option<AntennaState> {
    match substate {
        1 | 2 => Some(AntennaState::Connected),
        3 => Some(AntennaState::Disconnected),
        6 => Some(AntennaState::ShortCircuit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use domain::snmp::{SnmpValue, VarBind};

    use super::*;
    use crate::acl::mib::MibProfile;
    use crate::acl::mib::testing::varbind;

    fn row(row: u32, values: [i64; 6]) -> Vec<VarBind> {
        [
            COLUMN_TYPE,
            COLUMN_USAGE,
            COLUMN_STATE,
            COLUMN_SUBSTATE,
            COLUMN_STATUS_A,
            COLUMN_MAX_STATUS_A,
        ]
        .into_iter()
        .zip(values)
        .map(|(number, value)| {
            varbind(
                &column(number, row),
                SnmpValue::Gauge32(u32::try_from(value).unwrap()),
            )
        })
        .collect()
    }

    #[test]
    fn reads_good_and_in_view_satellites_of_a_gnss_refclock() {
        // gps180・primary・同期・gpsSync・良好 5・可視 9
        let status = MibProfile::MeinbergLtng
            .interpret(&row(1, [30, 3, 1, 1, 5, 9]))
            .unwrap();
        assert_eq!(status.name.as_deref(), Some("gps180"));
        assert_eq!(status.state, Some(ReceiverState::Synchronized));
        assert_eq!(status.state_detail.as_deref(), Some("gpsSync"));
        assert_eq!(status.antenna, Some(AntennaState::Connected));
        assert_eq!(status.holdover, None);
        assert_eq!(
            status.satellites,
            SatelliteCounts {
                good: Some(5),
                in_view: Some(9),
                expected: None,
                tracking: None,
            }
        );
    }

    #[test]
    fn status_a_of_a_long_wave_refclock_is_not_a_satellite_count() {
        // pzf511（長波）。StatusA は相関値なので衛星の数として出さない
        let status = MibProfile::MeinbergLtng
            .interpret(&row(1, [13, 3, 1, 52, 87, 100]))
            .unwrap();
        assert_eq!(status.name, None);
        assert_eq!(status.state_detail.as_deref(), Some("lwSync"));
        assert!(status.satellites.is_empty());
    }

    #[test]
    fn prefers_the_primary_refclock() {
        let mut varbinds = row(1, [30, 1, 2, 3, 0, 0]);
        varbinds.extend(row(2, [92, 3, 2, 2, 3, 12]));
        let status = MibProfile::MeinbergLtng.interpret(&varbinds).unwrap();
        assert_eq!(status.name.as_deref(), Some("gns181"));
        assert_eq!(status.state, Some(ReceiverState::Acquiring));
        assert_eq!(status.state_detail.as_deref(), Some("gpsTracking"));
        assert_eq!(status.satellites.good, Some(3));
        assert_eq!(status.satellites.in_view, Some(12));

        // primary がなければ最初の行
        let mut without_primary = row(1, [30, 1, 2, 6, 0, 0]);
        without_primary.extend(row(2, [92, 2, 1, 1, 8, 12]));
        let first = MibProfile::MeinbergLtng
            .interpret(&without_primary)
            .unwrap();
        assert_eq!(first.state, Some(ReceiverState::NotSynchronized));
        assert_eq!(first.antenna, Some(AntennaState::ShortCircuit));
    }

    #[test]
    fn state_detail_falls_back_to_the_state_name() {
        let status = MibProfile::MeinbergLtng
            .interpret(&row(1, [30, 3, 2, 0, 0, 0]))
            .unwrap();
        assert_eq!(status.state, Some(ReceiverState::NotSynchronized));
        assert_eq!(status.state_detail.as_deref(), Some("notSynchronized"));
        assert_eq!(status.antenna, None);
    }
}
