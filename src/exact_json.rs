//! Serde helpers for wire values that JSON cannot represent losslessly.

use std::path::{Path, PathBuf};
use std::{fmt, marker::PhantomData};

use serde::{Deserialize, Deserializer, Serializer, de};

struct DecimalIntegerVisitor<T>(PhantomData<T>);

impl<T> DecimalIntegerVisitor<T> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'de, T> de::Visitor<'de> for DecimalIntegerVisitor<T>
where
    T: TryFrom<u128> + std::str::FromStr,
    <T as TryFrom<u128>>::Error: fmt::Display,
    <T as std::str::FromStr>::Err: fmt::Display,
{
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a non-negative integer or decimal integer string")
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        T::try_from(u128::from(value)).map_err(E::custom)
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        T::try_from(value).map_err(E::custom)
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let value = u128::try_from(value).map_err(E::custom)?;
        T::try_from(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        value.parse().map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }
}

pub(crate) mod u128_decimal {
    use super::*;

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DecimalIntegerVisitor::<u128>::new())
    }
}

pub(crate) mod u64_decimal {
    use super::*;

    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DecimalIntegerVisitor::<u64>::new())
    }
}

/// OS paths are diagnostics, not identifiers. Serialize them lossily so a
/// valid non-Unicode Unix path cannot make an otherwise useful report fail.
pub(crate) mod optional_pathbuf_lossy {
    use super::*;

    pub fn serialize<S>(value: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(path) => serializer.serialize_some(path.to_string_lossy().as_ref()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer).map(|path| path.map(PathBuf::from))
    }
}

pub(crate) mod pathbuf_lossy {
    use super::*;

    pub fn serialize<S>(value: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.to_string_lossy().as_ref())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ExactIntegers {
        #[serde(with = "super::u128_decimal")]
        wide: u128,
        #[serde(with = "super::u64_decimal")]
        tokens: u64,
    }

    #[test]
    fn exact_integers_serialize_as_decimal_strings() {
        let value = ExactIntegers {
            wide: u128::MAX,
            tokens: 9_007_199_254_740_993,
        };

        assert_eq!(
            serde_json::to_value(&value).unwrap(),
            json!({
                "wide": u128::MAX.to_string(),
                "tokens": "9007199254740993"
            })
        );
        assert_eq!(
            serde_json::from_value::<ExactIntegers>(serde_json::to_value(&value).unwrap()).unwrap(),
            value
        );
    }

    #[test]
    fn exact_integers_accept_legacy_json_numbers() {
        assert_eq!(
            serde_json::from_value::<ExactIntegers>(json!({
                "wide": 123,
                "tokens": 456
            }))
            .unwrap(),
            ExactIntegers {
                wide: 123,
                tokens: 456
            }
        );
    }
}
