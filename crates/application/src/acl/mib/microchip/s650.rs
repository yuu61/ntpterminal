//! S650 の MIB（`enterprises.9070.1.2.5.7.8`。`SyncServer` S650）。
//!
//! - 値はすべて文字列（`DisplayString`）で、末尾に NUL が付くことがある
//! - 機種は `sysObjectID` ではなく `systemModel` で見る（実機は MIB の枝と違う `sysObjectID` を返す）
//! - 衛星の数は `gnssSatlist`（`N,X<衛星番号>,Y<通し番号>,C<衛星系>,…`）の先頭の N で、追尾している衛星の数。
//!   信号の強さと「計算に使っているか」はない（旧 `SyncServer` の `gpsSatlist` とは別物）
//! - `gnssSatlist` は宣言が 128 バイトまでで、衛星が多いと途中で切れる（MIB の例と実機の記録の両方で、
//!   並んでいる衛星が N より少ない）。数は N を正とし、衛星系の内訳は並びが N 個そろったときだけ出す。
//!   128 バイトを超える値も拒まない
//! - ホールドオーバーは `clockStatus`（Holdover・Bridging など）から。アンテナの状態はない

use domain::receiver::{
    Constellation, ConstellationCount, ReceiverState, ReceiverStatus, SatelliteCounts,
};
use domain::snmp::ObjectId;

use super::oid;
use crate::acl::mib::Values;

/// `s650`（`9070.1.2.5.7.8`）
pub const S650: [u32; 5] = [1, 2, 5, 7, 8];
/// `systemModel.0`（MIB の判定にも使う）
pub const SYSTEM_MODEL: [u32; 3] = [2, 3, 0];
/// `clockStatus.0`
pub const CLOCK_STATUS: [u32; 3] = [3, 1, 0];
/// `gnssReceiverStatus.0`
pub const GNSS_RECEIVER_STATUS: [u32; 3] = [4, 2, 0];
/// `gnssSatlist.0`
pub const GNSS_SATLIST: [u32; 3] = [4, 3, 0];

/// `s650` の下の OID。
#[must_use]
pub fn object(suffix: &[u32]) -> ObjectId {
    oid(&S650).with_suffix(suffix)
}

pub(crate) fn probe_oid() -> ObjectId {
    object(&SYSTEM_MODEL)
}

pub(crate) fn status_oids() -> Vec<ObjectId> {
    vec![
        object(&SYSTEM_MODEL),
        object(&CLOCK_STATUS),
        object(&GNSS_RECEIVER_STATUS),
        object(&GNSS_SATLIST),
    ]
}

pub(crate) fn interpret(values: &Values<'_>) -> ReceiverStatus {
    let receiver = values.text(&object(&GNSS_RECEIVER_STATUS));
    let satlist = values
        .text(&object(&GNSS_SATLIST))
        .as_deref()
        .map(SatList::parse);
    let count = satlist.as_ref().and_then(|list| list.count);
    ReceiverStatus {
        name: values.text(&object(&SYSTEM_MODEL)),
        state: receiver.as_deref().and_then(classify),
        state_detail: receiver,
        antenna: None,
        holdover: values
            .text(&object(&CLOCK_STATUS))
            .as_deref()
            .and_then(holdover),
        satellites: SatelliteCounts {
            tracking: count,
            ..SatelliteCounts::default()
        },
        constellations: satlist
            .filter(SatList::is_complete)
            .map(|list| ConstellationCount::tally(&list.systems))
            .unwrap_or_default(),
    }
}

/// `gnssReceiverStatus`: `Invalid` / `Tracking No UTC` / `Tracking` / `Not Available`
fn classify(text: &str) -> Option<ReceiverState> {
    match text.trim().to_ascii_lowercase().as_str() {
        "tracking" => Some(ReceiverState::Synchronized),
        "tracking no utc" => Some(ReceiverState::Acquiring),
        "invalid" => Some(ReceiverState::NotSynchronized),
        _ => None,
    }
}

/// `clockStatus`: `Warmup` / `Freerun` / `HandSet` / `FastLock` / `NormalLock` / `Bridging` / `Holdover` /
/// `Holdover Extended` / `Holdover Recovery`
fn holdover(text: &str) -> Option<bool> {
    let lower = text.trim().to_ascii_lowercase();
    match lower.as_str() {
        "warmup" | "freerun" | "handset" | "fastlock" | "normallock" => Some(false),
        "bridging" => Some(true),
        _ if lower.starts_with("holdover") => Some(true),
        _ => None,
    }
}

/// `gnssSatlist` を読んだもの。
#[derive(Clone, Debug, PartialEq, Eq)]
struct SatList {
    /// 先頭の N（追尾している衛星の数）
    count: Option<u32>,
    /// 並んでいた衛星の衛星系（3 つの組がそろったものだけ）
    systems: Vec<Constellation>,
}

impl SatList {
    fn parse(text: &str) -> Self {
        let mut tokens = text.split(',').map(str::trim);
        let count = tokens.next().and_then(|first| first.parse::<u32>().ok());
        let mut systems = Vec::new();
        while let (Some(vehicle), Some(index), Some(system)) =
            (tokens.next(), tokens.next(), tokens.next())
        {
            let (Some(_), Some(_), Some(name)) =
                (field(vehicle, 'X'), field(index, 'Y'), field(system, 'C'))
            else {
                break;
            };
            systems.push(Constellation::from_name(name));
        }
        Self { count, systems }
    }

    /// 並びが N 個そろっているか（途中で切れていないか）。
    fn is_complete(&self) -> bool {
        self.count
            .is_some_and(|count| usize::try_from(count).is_ok_and(|len| len == self.systems.len()))
    }
}

/// "X12" のように `prefix` で始まる項目の中身。空なら `None`。
fn field(token: &str, prefix: char) -> Option<&str> {
    token.strip_prefix(prefix).filter(|rest| !rest.is_empty())
}

#[cfg(test)]
mod tests {
    use domain::snmp::{SnmpValue, VarBind};

    use super::*;
    use crate::acl::mib::MibProfile;
    use crate::acl::mib::testing::{text, varbind};

    fn response(satlist: &str) -> Vec<VarBind> {
        vec![
            varbind(&object(&SYSTEM_MODEL), text("SyncServer S650\0")),
            varbind(&object(&CLOCK_STATUS), text("NormalLock")),
            varbind(&object(&GNSS_RECEIVER_STATUS), text("Tracking")),
            varbind(&object(&GNSS_SATLIST), text(satlist)),
        ]
    }

    #[test]
    fn reads_tracking_satellites_and_constellations() {
        let status = MibProfile::MicrochipS650
            .interpret(&response(
                "4,X2,Y1,CBeiDou,X5,Y2,CGPS,X9,Y3,CBeiDou,X13,Y4,CGLONASS",
            ))
            .unwrap();
        assert_eq!(status.name.as_deref(), Some("SyncServer S650"));
        assert_eq!(status.state, Some(ReceiverState::Synchronized));
        assert_eq!(status.state_detail.as_deref(), Some("Tracking"));
        assert_eq!(status.holdover, Some(false));
        assert_eq!(status.antenna, None);
        assert_eq!(
            status.satellites,
            SatelliteCounts {
                tracking: Some(4),
                ..SatelliteCounts::default()
            }
        );
        assert_eq!(
            status.constellations,
            [
                ConstellationCount {
                    constellation: Constellation::BeiDou,
                    count: 2
                },
                ConstellationCount {
                    constellation: Constellation::Gps,
                    count: 1
                },
                ConstellationCount {
                    constellation: Constellation::Glonass,
                    count: 1
                },
            ]
        );
    }

    /// MIB の説明の例と同じく、N が 11 なのに 9 個しか並んでいない（途中で切れた）。
    #[test]
    fn truncated_lists_keep_the_count_but_drop_the_breakdown() {
        let truncated = "11,X2,Y1,CBeiDou,X3,Y2,CBeiDou,X5,Y3,CBeiDou,X9,Y4,CBeiDou,X3,Y5,CGLONASS,\
                         X4,Y6,CGLONASS,X13,Y7,CGLONASS,X14,Y8,CGLONASS,X15,Y9,CGLONASS";
        assert!(truncated.len() > 128);
        let status = MibProfile::MicrochipS650
            .interpret(&response(truncated))
            .unwrap();
        assert_eq!(status.satellites.tracking, Some(11));
        assert_eq!(status.constellations, []);

        // 項目の途中で切れたもの（実機の記録と同じ形）
        let cut = MibProfile::MicrochipS650
            .interpret(&response("7,X1,Y1,CGPS,X3,"))
            .unwrap();
        assert_eq!(cut.satellites.tracking, Some(7));
        assert_eq!(cut.constellations, []);
    }

    #[test]
    fn parses_lists_with_a_trailing_comma_and_no_satellites() {
        assert_eq!(
            SatList::parse("2,X1,Y1,CGPS,X10,Y2,CGPS,"),
            SatList {
                count: Some(2),
                systems: vec![Constellation::Gps, Constellation::Gps],
            }
        );
        let empty = SatList::parse("0");
        assert!(empty.is_complete());
        assert_eq!(empty.systems, []);
        assert_eq!(SatList::parse("").count, None);
        assert!(!SatList::parse("x,X1,Y1,CGPS").is_complete());
    }

    #[test]
    fn reads_holdover_and_acquiring_states() {
        let mut varbinds = response("0");
        varbinds[1] = varbind(&object(&CLOCK_STATUS), text("Holdover Extended"));
        varbinds[2] = varbind(&object(&GNSS_RECEIVER_STATUS), text("Tracking No UTC"));
        let status = MibProfile::MicrochipS650.interpret(&varbinds).unwrap();
        assert_eq!(status.holdover, Some(true));
        assert_eq!(status.state, Some(ReceiverState::Acquiring));
        assert_eq!(status.satellites.tracking, Some(0));

        varbinds[1] = varbind(&object(&CLOCK_STATUS), text("Something New"));
        varbinds[2] = varbind(&object(&GNSS_RECEIVER_STATUS), text("Not Available"));
        let unknown = MibProfile::MicrochipS650.interpret(&varbinds).unwrap();
        assert_eq!(unknown.holdover, None);
        assert_eq!(unknown.state, None);
        assert_eq!(unknown.state_detail.as_deref(), Some("Not Available"));
    }

    #[test]
    fn missing_satlist_is_not_zero() {
        let mut varbinds = response("");
        varbinds[3] = varbind(&object(&GNSS_SATLIST), SnmpValue::NoSuchObject);
        let status = MibProfile::MicrochipS650.interpret(&varbinds).unwrap();
        assert_eq!(status.satellites.tracking, None);
    }
}
