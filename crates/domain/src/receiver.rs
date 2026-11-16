//! 受信機コンテキスト: タイムサーバの GNSS 受信機の状態。
//!
//! メーカーの MIB（SNMP）で読んだ値を、メーカーに依らない形にしたもの。SNMP の値からの翻訳は
//! application 層の ACL が担い、ここは SNMP を知らない。
//!
//! - 衛星の数は、メーカーごとに数えているものが違う（良好・可視・見えるはず・追尾）。意味ごとに別の欄に持ち、
//!   ほかの意味の数で埋めない（表示で偽らない）
//! - 取れなかった項目は `None` にする（0 や「正常」で埋めない）

/// 受信機の状態。どの項目も、取れなかったときは `None`（空）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiverStatus {
    /// 受信機の名前（型番やスロット名。"gps180"・"SyncServer S650" など、メーカーの表記のまま）
    pub name: Option<String>,
    pub state: Option<ReceiverState>,
    /// 受信機の状態のメーカーの表記（"gpsSync"・"Tracking No UTC" など）
    pub state_detail: Option<String>,
    pub antenna: Option<AntennaState>,
    /// サーバ全体の時計がホールドオーバー中か（受信機ではなく、内部の発振器で時刻を保っているか）
    pub holdover: Option<bool>,
    pub satellites: SatelliteCounts,
    /// 衛星系ごとの追尾数。衛星の一覧が全部そろったときだけ入れる（一部だけなら空にする）
    pub constellations: Vec<ConstellationCount>,
}

/// 受信機の状態の分類。メーカーの細かい状態は [`ReceiverStatus::state_detail`] に残す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverState {
    /// 衛星の時刻に同期している（時刻が有効）
    Synchronized,
    /// 起動中・衛星を探している・追尾しているが、まだ同期していない
    Acquiring,
    /// 同期していない（アンテナの異常などを含む）
    NotSynchronized,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AntennaState {
    Connected,
    Disconnected,
    ShortCircuit,
    /// 異常（断線か短絡かをメーカーが区別していない）
    Faulty,
}

/// 衛星の数。メーカーが数えているものだけを入れ、ほかの欄はそのまま `None` にする。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SatelliteCounts {
    /// 良好な（時刻の計算に使える）衛星
    pub good: Option<u32>,
    /// 見えている（受信している）衛星
    pub in_view: Option<u32>,
    /// 軌道からみて見えるはずの衛星（受信しているとは限らない）
    pub expected: Option<u32>,
    /// 追尾している衛星
    pub tracking: Option<u32>,
}

impl SatelliteCounts {
    /// どの数も取れていないか。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.good.is_none()
            && self.in_view.is_none()
            && self.expected.is_none()
            && self.tracking.is_none()
    }
}

/// 衛星系（コンステレーション）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Constellation {
    Gps,
    Glonass,
    Galileo,
    BeiDou,
    Qzss,
    Sbas,
    Navic,
    /// 知らない名前（メーカーの表記のまま）
    Other(String),
}

impl Constellation {
    /// "GPS"・"GLONASS"・"BeiDou" などの名前から（大文字・小文字は区別しない）。
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let trimmed = name.trim();
        match trimmed.to_ascii_uppercase().as_str() {
            "GPS" => Self::Gps,
            "GLONASS" => Self::Glonass,
            "GALILEO" => Self::Galileo,
            "BEIDOU" | "BDS" => Self::BeiDou,
            "QZSS" => Self::Qzss,
            "SBAS" => Self::Sbas,
            "NAVIC" | "IRNSS" => Self::Navic,
            _ => Self::Other(trimmed.to_owned()),
        }
    }
}

/// 衛星系ごとの数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstellationCount {
    pub constellation: Constellation,
    pub count: u32,
}

impl ConstellationCount {
    /// 衛星ごとの衛星系の並びを、衛星系ごとの数にまとめる（最初に出てきた順）。
    #[must_use]
    pub fn tally(systems: &[Constellation]) -> Vec<Self> {
        let mut counts: Vec<Self> = Vec::new();
        for system in systems {
            match counts
                .iter_mut()
                .find(|entry| entry.constellation == *system)
            {
                Some(entry) => entry.count += 1,
                None => counts.push(Self {
                    constellation: system.clone(),
                    count: 1,
                }),
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_constellation_names_case_insensitively() {
        assert_eq!(Constellation::from_name("GPS"), Constellation::Gps);
        assert_eq!(Constellation::from_name("BeiDou"), Constellation::BeiDou);
        assert_eq!(
            Constellation::from_name(" glonass "),
            Constellation::Glonass
        );
        assert_eq!(Constellation::from_name("Galileo"), Constellation::Galileo);
        assert_eq!(
            Constellation::from_name("XYZ"),
            Constellation::Other("XYZ".to_owned())
        );
    }

    #[test]
    fn tallies_in_order_of_first_appearance() {
        let systems = [
            Constellation::BeiDou,
            Constellation::Glonass,
            Constellation::BeiDou,
            Constellation::Glonass,
            Constellation::Glonass,
        ];
        assert_eq!(
            ConstellationCount::tally(&systems),
            [
                ConstellationCount {
                    constellation: Constellation::BeiDou,
                    count: 2
                },
                ConstellationCount {
                    constellation: Constellation::Glonass,
                    count: 3
                },
            ]
        );
        assert_eq!(ConstellationCount::tally(&[]), []);
    }

    #[test]
    fn counts_are_empty_until_something_is_known() {
        assert!(SatelliteCounts::default().is_empty());
        let counts = SatelliteCounts {
            tracking: Some(0),
            ..SatelliteCounts::default()
        };
        assert!(!counts.is_empty());
    }
}
