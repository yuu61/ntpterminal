//! OID（オブジェクト識別子）。

use core::fmt;
use core::str::FromStr;

/// OID。`1.3.6.1.2.1.1.1.0` のような数の並び。
///
/// 並びの比較（`Ord`）は SNMP の辞書順と同じ（同じところまで等しければ、短い方が先）。
/// 数の個数や先頭の値は検証しない（BER にするときの扱いは [`super::encode`] を参照）。
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(Vec<u32>);

impl ObjectId {
    #[must_use]
    pub fn new(arcs: &[u32]) -> Self {
        Self(arcs.to_vec())
    }

    #[must_use]
    pub fn arcs(&self) -> &[u32] {
        &self.0
    }

    /// 後ろに数を足した OID（表の列にインデックスを付けるときなど）。
    #[must_use]
    pub fn with_suffix(&self, suffix: &[u32]) -> Self {
        let mut arcs = Vec::with_capacity(self.0.len() + suffix.len());
        arcs.extend_from_slice(&self.0);
        arcs.extend_from_slice(suffix);
        Self(arcs)
    }

    /// `prefix` の下（`prefix` 自身を含む）にあるか。
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, arc) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(".")?;
            }
            write!(f, "{arc}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("OID は 1.3.6.1.2.1.1.1.0 の形式で指定してください: {0}")]
pub struct ObjectIdParseError(String);

impl FromStr for ObjectId {
    type Err = ObjectIdParseError;

    /// "1.3.6.1.2.1.1.1.0"（先頭の "." はあってもよい）。
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        let digits = trimmed.strip_prefix('.').unwrap_or(trimmed);
        let arcs: Option<Vec<u32>> = digits
            .split('.')
            .map(|part| part.parse::<u32>().ok())
            .collect();
        match arcs {
            Some(parsed) if !digits.is_empty() => Ok(Self(parsed)),
            _ => Err(ObjectIdParseError(trimmed.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_and_parses_dotted_form() {
        let oid = ObjectId::new(&[1, 3, 6, 1, 2, 1, 1, 1, 0]);
        assert_eq!(oid.to_string(), "1.3.6.1.2.1.1.1.0");
        assert_eq!("1.3.6.1.2.1.1.1.0".parse::<ObjectId>().unwrap(), oid);
        assert_eq!(".1.3.6.1.2.1.1.1.0".parse::<ObjectId>().unwrap(), oid);
        for bad in ["", ".", "1..3", "1.3.x", "1.3.4294967296"] {
            assert!(bad.parse::<ObjectId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn orders_like_snmp() {
        let parent = ObjectId::new(&[1, 3, 6, 1]);
        let child = parent.with_suffix(&[2]);
        let sibling = ObjectId::new(&[1, 3, 6, 2]);
        assert!(parent < child);
        assert!(child < sibling);
        assert!(child.starts_with(&parent));
        assert!(!sibling.starts_with(&parent));
        assert!(parent.starts_with(&parent));
    }
}
