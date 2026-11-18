use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{SECONDS_PER_DAY, TimestampParseError, UtcTimestamp, format_year};

/// The UTC calendar day used in history shards, cleanup cursors, and wire keys.
///
/// Uses an epoch-day index to retain distinct days outside Jiff's civil range.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UtcDay(i32);

impl UtcDay {
    pub(super) fn from_timestamp(timestamp: UtcTimestamp) -> Self {
        let days = timestamp.timestamp().div_euclid(SECONDS_PER_DAY);
        Self(i32::try_from(days).expect("every supported UTC day fits in i32"))
    }

    /// Return the first instant of this UTC day.
    pub fn start(self) -> UtcTimestamp {
        let seconds = i64::from(self.0) * SECONDS_PER_DAY;
        UtcTimestamp::from_timestamp(seconds, 0).expect("a supported UTC day begins at midnight")
    }

    /// Return the next UTC day if it is representable.
    pub fn checked_next(self) -> Option<Self> {
        self.start().checked_add_days(1).map(Self::from_timestamp)
    }

    pub fn days_since(self, other: Self) -> i64 {
        i64::from(self.0) - i64::from(other.0)
    }
}

impl fmt::Display for UtcDay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (civil, year_adjustment) = self.start().calendar_parts();
        format_year(formatter, i32::from(civil.year()) + year_adjustment)?;
        write!(formatter, "-{:02}-{:02}", civil.month(), civil.day())
    }
}

impl FromStr for UtcDay {
    type Err = TimestampParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !(10..=13).contains(&value.len()) {
            return Err(TimestampParseError);
        }
        let timestamp = format!("{value}T00:00:00Z").parse::<UtcTimestamp>()?;
        let day = Self::from_timestamp(timestamp);
        // Keep shard names canonical, including the sign on expanded years.
        (day.to_string() == value)
            .then_some(day)
            .ok_or(TimestampParseError)
    }
}

impl Serialize for UtcDay {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UtcDay {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = UtcDay;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a UTC day in YYYY-MM-DD format")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_keys_keep_canonical_signed_years_and_distinct_days() {
        let keys = [
            "-262143-01-01",
            "-0001-12-31",
            "1969-12-31",
            "+10000-02-29",
            "+262142-12-31",
        ];
        for key in keys {
            let day = key.parse::<UtcDay>().unwrap();
            assert_eq!(day.start().utc_date(), day);
            assert_eq!(day.to_string(), key);
            assert_eq!(serde_json::to_string(&day).unwrap(), format!("\"{key}\""));
            assert_eq!(
                serde_json::from_str::<UtcDay>(&format!("\"{key}\"")).unwrap(),
                day
            );
        }

        let pre_epoch = UtcTimestamp::from_timestamp(-1, 999_999_999)
            .unwrap()
            .utc_date();
        assert_eq!(pre_epoch.to_string(), "1969-12-31");
        let leap_day = "+10000-02-29".parse::<UtcDay>().unwrap();
        assert_eq!(leap_day.checked_next().unwrap().to_string(), "+10000-03-01");
        assert_eq!(leap_day.checked_next().unwrap().days_since(leap_day), 1);
        assert!(UtcTimestamp::MAX.utc_date().checked_next().is_none());
        assert!("2026-08-30T00:00:00Z".parse::<UtcDay>().is_err());
    }
}
