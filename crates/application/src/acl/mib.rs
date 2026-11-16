//! MIB の対応表と翻訳: メーカーの MIB（SNMP の値）を、受信機コンテキストの [`ReceiverStatus`] にする。
//!
//! 対応している MIB と、それぞれが表す衛星の数の意味（メーカーごとに違うので混ぜない）:
//!
//! | MIB | 機器 | 衛星の数 |
//! |---|---|---|
//! | MEINBERG-OS-MIB（`5597.7`） | `meinbergOS` の機器 | 良好（`GoodSv`）・見えるはず（`Sv`） |
//! | MBG-SNMP-LTNG-MIB（`5597.30`） | LANTIME の現行ファームウェア | 良好（`StatusA`）・可視（`MaxStatusA`） |
//! | MBG-SNMP-LT-MIB（`5597.3`） | 旧 LANTIME（V5 系） | 良好・可視 |
//! | S650（`9070.1.2.5.7.8`） | Microchip `SyncServer` S650 | 追尾（`gnssSatlist` の先頭の数） |
//!
//! 決まりごと（理由はメーカーの MIB の記述と、実機の SNMP の記録から）:
//!
//! - どの MIB かは `sysObjectID` では決めない（LANTIME V6 は LTNG の値を返すのに `sysObjectID` が LT の番号、
//!   S650 は MIB の枝と違う番号を返す）。各 MIB の判定用の値に応答があるかで決め、Meinberg は新しい MIB から試す
//! - 値がない（`noSuchObject` など）・型が違う項目は `None` にする。0 や「正常」で埋めない
//! - 状態の分類は推測を最小限にし、メーカーの表記を `state_detail` に残す

pub mod meinberg;
pub mod microchip;

use domain::receiver::ReceiverStatus;
use domain::snmp::{ObjectId, SnmpValue, VarBind};

/// 対応している MIB。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MibProfile {
    MeinbergOs,
    MeinbergLtng,
    MeinbergLt,
    MicrochipS650,
}

impl MibProfile {
    /// 自動判定で試す順。Meinberg は新しい MIB から（新しい機器が古い MIB も返すことがあるため）。
    pub const DETECTION_ORDER: [Self; 4] = [
        Self::MeinbergOs,
        Self::MeinbergLtng,
        Self::MeinbergLt,
        Self::MicrochipS650,
    ];

    #[must_use]
    pub const fn vendor(self) -> &'static str {
        match self {
            Self::MeinbergOs | Self::MeinbergLtng | Self::MeinbergLt => "Meinberg",
            Self::MicrochipS650 => "Microchip",
        }
    }

    /// MIB のモジュール名。
    #[must_use]
    pub const fn mib_name(self) -> &'static str {
        match self {
            Self::MeinbergOs => "MEINBERG-OS-MIB",
            Self::MeinbergLtng => "MBG-SNMP-LTNG-MIB",
            Self::MeinbergLt => "MBG-SNMP-LT-MIB",
            Self::MicrochipS650 => "S650",
        }
    }

    /// この MIB があるかを確かめる OID（スカラー）。値が返ればこの MIB とみなす。
    #[must_use]
    pub fn probe_oid(self) -> ObjectId {
        match self {
            Self::MeinbergOs => meinberg::os::probe_oid(),
            Self::MeinbergLtng => meinberg::ltng::probe_oid(),
            Self::MeinbergLt => meinberg::lt::probe_oid(),
            Self::MicrochipS650 => microchip::s650::probe_oid(),
        }
    }

    /// 状態を読むために GET する OID。
    #[must_use]
    pub fn status_oids(self) -> Vec<ObjectId> {
        match self {
            Self::MeinbergOs => meinberg::os::status_oids(),
            Self::MeinbergLtng => meinberg::ltng::status_oids(),
            Self::MeinbergLt => meinberg::lt::status_oids(),
            Self::MicrochipS650 => microchip::s650::status_oids(),
        }
    }

    /// [`Self::status_oids`] の GET の応答から状態を組み立てる。
    ///
    /// # Errors
    ///
    /// 問い合わせた値が 1 つも返らなかったとき（機器がこの MIB に対応していない）。
    pub fn interpret(self, varbinds: &[VarBind]) -> Result<ReceiverStatus, MibError> {
        let values = Values(varbinds);
        if !self
            .status_oids()
            .iter()
            .any(|oid| values.get(oid).is_some())
        {
            return Err(MibError::NoValues(self));
        }
        Ok(match self {
            Self::MeinbergOs => meinberg::os::interpret(&values),
            Self::MeinbergLtng => meinberg::ltng::interpret(&values),
            Self::MeinbergLt => meinberg::lt::interpret(&values),
            Self::MicrochipS650 => microchip::s650::interpret(&values),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MibError {
    #[error("{} の値が返りません（この MIB に対応していない機器です）", .0.mib_name())]
    NoValues(MibProfile),
}

/// 判定用の OID（[`MibProfile::probe_oid`]）の GET の応答から、MIB を選ぶ。
/// [`MibProfile::DETECTION_ORDER`] の順で、値が返った最初のもの。
#[must_use]
pub fn detect(varbinds: &[VarBind]) -> Option<MibProfile> {
    let values = Values(varbinds);
    MibProfile::DETECTION_ORDER
        .into_iter()
        .find(|profile| values.get(&profile.probe_oid()).is_some())
}

/// 応答の varbind を OID で引く。`noSuchObject` などの「値がない」は、ないものとして扱う。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Values<'varbinds>(&'varbinds [VarBind]);

impl Values<'_> {
    fn get(&self, oid: &ObjectId) -> Option<&SnmpValue> {
        self.0
            .iter()
            .find(|varbind| varbind.name == *oid)
            .map(|varbind| &varbind.value)
            .filter(|value| !value.is_exception())
    }

    /// 整数の型（INTEGER・Gauge32 など）の値。
    fn integer(&self, oid: &ObjectId) -> Option<i64> {
        match *self.get(oid)? {
            SnmpValue::Integer(number) => Some(number),
            SnmpValue::Counter32(number)
            | SnmpValue::Gauge32(number)
            | SnmpValue::TimeTicks(number) => Some(i64::from(number)),
            SnmpValue::Counter64(number) => i64::try_from(number).ok(),
            SnmpValue::OctetString(_)
            | SnmpValue::Null
            | SnmpValue::ObjectId(_)
            | SnmpValue::IpAddress(_)
            | SnmpValue::Opaque(_)
            | SnmpValue::NoSuchObject
            | SnmpValue::NoSuchInstance
            | SnmpValue::EndOfMibView
            | SnmpValue::Other { .. } => None,
        }
    }

    /// 数（0 以上）。負の値は数として扱わない。
    fn count(&self, oid: &ObjectId) -> Option<u32> {
        u32::try_from(self.integer(oid)?).ok()
    }

    #[expect(
        clippy::pattern_type_mismatch,
        reason = "OCTET STRING の中身（Vec）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
    )]
    fn octets(&self, oid: &ObjectId) -> Option<&[u8]> {
        match self.get(oid)? {
            SnmpValue::OctetString(bytes) => Some(bytes),
            SnmpValue::Integer(_)
            | SnmpValue::Null
            | SnmpValue::ObjectId(_)
            | SnmpValue::IpAddress(_)
            | SnmpValue::Counter32(_)
            | SnmpValue::Gauge32(_)
            | SnmpValue::TimeTicks(_)
            | SnmpValue::Opaque(_)
            | SnmpValue::Counter64(_)
            | SnmpValue::NoSuchObject
            | SnmpValue::NoSuchInstance
            | SnmpValue::EndOfMibView
            | SnmpValue::Other { .. } => None,
        }
    }

    /// 文字列（`DisplayString`）。末尾の NUL と空白を取り除き、空なら `None`。
    fn text(&self, oid: &ObjectId) -> Option<String> {
        let bytes = self.octets(oid)?;
        let text = String::from_utf8_lossy(bytes);
        let trimmed = text
            .trim_end_matches(['\0', ' ', '\t', '\r', '\n'])
            .trim_start();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// BITS 型の `bit` 番目。BITS は先頭バイトの最上位ビットが 0 番目（bit 7 は 1 バイト目の 0x01）。
    /// 後ろのバイトが省かれていたら、そのビットは立っていないものとする。
    fn bit(&self, oid: &ObjectId, bit: usize) -> Option<bool> {
        let bytes = self.octets(oid)?;
        let mask = 0x80u8 >> (bit % 8);
        Some(bytes.get(bit / 8).is_some_and(|byte| byte & mask != 0))
    }
}

/// 列挙型の値の名前（表示用）。知らない値は数のまま。
fn enum_name(names: &[(i64, &str)], value: i64) -> String {
    names
        .iter()
        .find(|&&(number, _)| number == value)
        .map_or_else(|| value.to_string(), |&(_, name)| name.to_owned())
}

#[cfg(test)]
pub(crate) mod testing {
    //! 応答の varbind を手で組み立てる。

    use domain::snmp::{ObjectId, SnmpValue, VarBind};

    pub(crate) fn varbind(oid: &ObjectId, value: SnmpValue) -> VarBind {
        VarBind {
            name: oid.clone(),
            value,
        }
    }

    pub(crate) fn text(value: &str) -> SnmpValue {
        SnmpValue::OctetString(value.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{text, varbind};
    use super::*;

    fn probe_response(present: &[MibProfile]) -> Vec<VarBind> {
        MibProfile::DETECTION_ORDER
            .iter()
            .map(|profile| {
                let value = if present.contains(profile) {
                    SnmpValue::Integer(1)
                } else {
                    SnmpValue::NoSuchObject
                };
                varbind(&profile.probe_oid(), value)
            })
            .collect()
    }

    #[test]
    fn detects_the_newest_meinberg_mib_first() {
        assert_eq!(
            detect(&probe_response(&[
                MibProfile::MeinbergLt,
                MibProfile::MeinbergLtng
            ])),
            Some(MibProfile::MeinbergLtng)
        );
        assert_eq!(
            detect(&probe_response(&[MibProfile::MicrochipS650])),
            Some(MibProfile::MicrochipS650)
        );
        assert_eq!(detect(&probe_response(&[])), None);
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn interpret_fails_when_no_value_comes_back() {
        for profile in MibProfile::DETECTION_ORDER {
            let varbinds: Vec<VarBind> = profile
                .status_oids()
                .iter()
                .map(|oid| varbind(oid, SnmpValue::NoSuchObject))
                .collect();
            assert_eq!(
                profile.interpret(&varbinds),
                Err(MibError::NoValues(profile))
            );
        }
    }

    #[test]
    fn probe_and_status_oids_are_under_the_vendor_tree() {
        for profile in MibProfile::DETECTION_ORDER {
            let root = match profile {
                MibProfile::MeinbergOs | MibProfile::MeinbergLtng | MibProfile::MeinbergLt => {
                    ObjectId::new(&meinberg::ENTERPRISE)
                }
                MibProfile::MicrochipS650 => ObjectId::new(&microchip::ENTERPRISE),
            };
            assert!(profile.probe_oid().starts_with(&root), "{profile:?}");
            assert!(
                profile
                    .status_oids()
                    .iter()
                    .all(|oid| oid.starts_with(&root)),
                "{profile:?}"
            );
        }
    }

    #[test]
    fn values_ignore_exceptions_and_wrong_types() {
        let number = ObjectId::new(&[1, 1]);
        let gauge = ObjectId::new(&[1, 2]);
        let missing = ObjectId::new(&[1, 3]);
        let string = ObjectId::new(&[1, 4]);
        let negative = ObjectId::new(&[1, 5]);
        let varbinds = [
            varbind(&number, SnmpValue::Integer(7)),
            varbind(&gauge, SnmpValue::Gauge32(24)),
            varbind(&missing, SnmpValue::NoSuchInstance),
            varbind(&string, text("Timing I/O Card\0")),
            varbind(&negative, SnmpValue::Integer(-1)),
        ];
        let values = Values(&varbinds);
        assert_eq!(values.integer(&number), Some(7));
        assert_eq!(values.count(&gauge), Some(24));
        assert_eq!(values.integer(&missing), None);
        assert_eq!(values.integer(&string), None);
        assert_eq!(values.text(&number), None);
        assert_eq!(values.text(&string).as_deref(), Some("Timing I/O Card"));
        assert_eq!(values.count(&negative), None);
        assert_eq!(values.integer(&ObjectId::new(&[9])), None);
    }

    #[test]
    fn bits_count_from_the_top_of_the_first_byte() {
        let oid = ObjectId::new(&[1, 1]);
        let varbinds = [varbind(&oid, SnmpValue::OctetString(vec![0x01, 0x80]))];
        let values = Values(&varbinds);
        assert_eq!(values.bit(&oid, 0), Some(false));
        assert_eq!(values.bit(&oid, 7), Some(true));
        assert_eq!(values.bit(&oid, 8), Some(true));
        assert_eq!(values.bit(&oid, 9), Some(false));
        // 省かれた後ろのバイトは立っていない
        assert_eq!(values.bit(&oid, 20), Some(false));
        assert_eq!(values.bit(&ObjectId::new(&[2]), 0), None);
    }
}
