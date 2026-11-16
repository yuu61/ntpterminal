//! MBG-SNMP-LT-MIB（`enterprises.5597.3`。旧 LANTIME の V5 系）。
//!
//! - スカラー値だけで、参照時計は 1 台
//! - 衛星の数（良好・可視）は GPS の参照時計の値なので、参照時計が GPS のとき（`TypeVal` が GPS167 か、
//!   型の名前が gps・gns・gln で始まる）だけ使う
//! - ホールドオーバーの値はない

use domain::receiver::{AntennaState, ReceiverState, ReceiverStatus, SatelliteCounts};
use domain::snmp::ObjectId;

use super::oid;
use crate::acl::mib::{Values, enum_name};

/// `mbgLtRefclock`（`5597.3.2`）
pub const REFCLOCK: [u32; 2] = [3, 2];
/// `mbgLtRefClockType`（型の名前）
pub const OBJECT_TYPE: u32 = 1;
/// `mbgLtRefClockTypeVal`（MIB の判定にも使う）
pub const OBJECT_TYPE_VAL: u32 = 2;
/// `mbgLtRefGpsStateVal`
pub const OBJECT_GPS_STATE_VAL: u32 = 6;
/// `mbgLtRefGpsSatellitesGood`
pub const OBJECT_SATELLITES_GOOD: u32 = 9;
/// `mbgLtRefGpsSatellitesInView`
pub const OBJECT_SATELLITES_IN_VIEW: u32 = 10;
/// `mbgLtRefGpsModeVal`
pub const OBJECT_GPS_MODE_VAL: u32 = 16;

/// `mbgLtRefClockTypeVal` のうち GPS のもの（mbgGPS167・mbgGPS167BGTTGP）
pub const GPS_TYPE_VALS: [i64; 2] = [1, 2];
/// GNSS の型の名前の始まり
const GNSS_NAME_PREFIXES: [&str; 3] = ["GPS", "GNS", "GLN"];
/// `mbgLtRefGpsStateVal` の値と名前
pub const STATES: [(i64, &str); 3] = [
    (0, "notavailable"),
    (1, "synchronized"),
    (2, "notsynchronized"),
];
/// `mbgLtRefGpsModeVal` の値と名前
pub const MODES: [(i64, &str); 7] = [
    (0, "notavailable"),
    (1, "normalOperation"),
    (2, "trackingSearching"),
    (3, "antennaFaulty"),
    (4, "warmBoot"),
    (5, "coldBoot"),
    (6, "antennaShortcircuit"),
];

/// 参照時計のスカラー値（`mbgLtRefclock.<object>.0`）。
#[must_use]
pub fn scalar(object: u32) -> ObjectId {
    oid(&REFCLOCK).with_suffix(&[object, 0])
}

pub(crate) fn probe_oid() -> ObjectId {
    scalar(OBJECT_TYPE_VAL)
}

pub(crate) fn status_oids() -> Vec<ObjectId> {
    [
        OBJECT_TYPE,
        OBJECT_TYPE_VAL,
        OBJECT_GPS_STATE_VAL,
        OBJECT_SATELLITES_GOOD,
        OBJECT_SATELLITES_IN_VIEW,
        OBJECT_GPS_MODE_VAL,
    ]
    .map(scalar)
    .to_vec()
}

pub(crate) fn interpret(values: &Values<'_>) -> ReceiverStatus {
    let name = values.text(&scalar(OBJECT_TYPE));
    let is_gps = values
        .integer(&scalar(OBJECT_TYPE_VAL))
        .is_some_and(|value| GPS_TYPE_VALS.contains(&value))
        || name.as_deref().is_some_and(|text| {
            let upper = text.to_ascii_uppercase();
            GNSS_NAME_PREFIXES
                .iter()
                .any(|prefix| upper.starts_with(prefix))
        });
    let state = values.integer(&scalar(OBJECT_GPS_STATE_VAL));
    let mode = values.integer(&scalar(OBJECT_GPS_MODE_VAL));
    ReceiverStatus {
        name,
        state: state.and_then(|value| classify(value, mode)),
        state_detail: match (state, mode) {
            (_, Some(detail)) if detail != 0 => Some(enum_name(&MODES, detail)),
            (Some(value), _) => Some(enum_name(&STATES, value)),
            (None, _) => None,
        },
        antenna: mode.and_then(antenna),
        holdover: None,
        satellites: if is_gps {
            SatelliteCounts {
                good: values.count(&scalar(OBJECT_SATELLITES_GOOD)),
                in_view: values.count(&scalar(OBJECT_SATELLITES_IN_VIEW)),
                ..SatelliteCounts::default()
            }
        } else {
            SatelliteCounts::default()
        },
        constellations: Vec::new(),
    }
}

const fn classify(state: i64, mode: Option<i64>) -> Option<ReceiverState> {
    match (state, mode) {
        (1, _) => Some(ReceiverState::Synchronized),
        (2, Some(2 | 4 | 5)) => Some(ReceiverState::Acquiring),
        (2, _) => Some(ReceiverState::NotSynchronized),
        _ => None,
    }
}

const fn antenna(mode: i64) -> Option<AntennaState> {
    match mode {
        1 | 2 => Some(AntennaState::Connected),
        3 => Some(AntennaState::Faulty),
        6 => Some(AntennaState::ShortCircuit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use domain::snmp::SnmpValue;

    use super::*;
    use crate::acl::mib::MibProfile;
    use crate::acl::mib::testing::{text, varbind};

    #[test]
    fn reads_gps_satellites() {
        let varbinds = [
            varbind(&scalar(OBJECT_TYPE), text("GPS170")),
            varbind(&scalar(OBJECT_TYPE_VAL), SnmpValue::Integer(0)),
            varbind(&scalar(OBJECT_GPS_STATE_VAL), SnmpValue::Integer(1)),
            varbind(&scalar(OBJECT_SATELLITES_GOOD), SnmpValue::Integer(7)),
            varbind(&scalar(OBJECT_SATELLITES_IN_VIEW), SnmpValue::Integer(10)),
            varbind(&scalar(OBJECT_GPS_MODE_VAL), SnmpValue::Integer(1)),
        ];
        let status = MibProfile::MeinbergLt.interpret(&varbinds).unwrap();
        assert_eq!(status.name.as_deref(), Some("GPS170"));
        assert_eq!(status.state, Some(ReceiverState::Synchronized));
        assert_eq!(status.state_detail.as_deref(), Some("normalOperation"));
        assert_eq!(status.antenna, Some(AntennaState::Connected));
        assert_eq!(status.satellites.good, Some(7));
        assert_eq!(status.satellites.in_view, Some(10));
    }

    #[test]
    fn long_wave_refclocks_have_no_satellites() {
        let varbinds = [
            varbind(&scalar(OBJECT_TYPE), text("PZF509")),
            varbind(&scalar(OBJECT_TYPE_VAL), SnmpValue::Integer(3)),
            varbind(&scalar(OBJECT_GPS_STATE_VAL), SnmpValue::Integer(2)),
            varbind(&scalar(OBJECT_SATELLITES_GOOD), SnmpValue::Integer(0)),
            varbind(&scalar(OBJECT_GPS_MODE_VAL), SnmpValue::Integer(3)),
        ];
        let status = MibProfile::MeinbergLt.interpret(&varbinds).unwrap();
        assert!(status.satellites.is_empty());
        assert_eq!(status.state, Some(ReceiverState::NotSynchronized));
        assert_eq!(status.antenna, Some(AntennaState::Faulty));
        assert_eq!(status.state_detail.as_deref(), Some("antennaFaulty"));
    }

    #[test]
    fn gps167_is_gps_without_a_type_name() {
        let varbinds = [
            varbind(&scalar(OBJECT_TYPE_VAL), SnmpValue::Integer(1)),
            varbind(&scalar(OBJECT_GPS_STATE_VAL), SnmpValue::Integer(2)),
            varbind(&scalar(OBJECT_GPS_MODE_VAL), SnmpValue::Integer(5)),
            varbind(&scalar(OBJECT_SATELLITES_GOOD), SnmpValue::Integer(0)),
            varbind(&scalar(OBJECT_SATELLITES_IN_VIEW), SnmpValue::Integer(4)),
        ];
        let status = MibProfile::MeinbergLt.interpret(&varbinds).unwrap();
        assert_eq!(status.state, Some(ReceiverState::Acquiring));
        assert_eq!(status.state_detail.as_deref(), Some("coldBoot"));
        assert_eq!(status.satellites.in_view, Some(4));
    }
}
