//! Time primitives shared by usage collection, history, and remote sync.
//!
//! Jiff supplies time arithmetic and calendar conversions. UTC serialization
//! retains the existing fractional precision and offset spelling because the
//! serialized timestamps also participate in persistent keys and digests.

use std::borrow::Cow;
use std::fmt::{self, Write};
use std::num::NonZeroU32;
use std::ops::{Add, AddAssign, Div, Neg, Sub, SubAssign};
use std::str::FromStr;
use std::time::{Duration as StdDuration, SystemTime};

use jiff::civil::{Date, DateTime};
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod utc_day;
pub use utc_day::UtcDay;

const SECONDS_PER_DAY: i64 = 86_400;
const NANOS_PER_SECOND: u32 = 1_000_000_000;
const DAYS_PER_GREGORIAN_CYCLE: i32 = 146_097;
const SECONDS_PER_GREGORIAN_CYCLE: i64 = SECONDS_PER_DAY * DAYS_PER_GREGORIAN_CYCLE as i64;
const EPOCH_DAY_2000: i32 = 10_957;
const JIFF_MIN_SECOND: i64 = -377_705_023_201;
const JIFF_MAX_SECOND: i64 = 253_402_207_200;

/// An absolute UTC timestamp with the project's stable wire spelling.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UtcTimestamp {
    epoch_day: i32,
    second_of_day: u32,
    // A nonzero encoding keeps both timestamp and optional timestamp compact.
    // Values beyond one second denote a legacy leap second and sort after 59.
    nanos_plus_one: NonZeroU32,
}

impl UtcTimestamp {
    // Preserve the range accepted by older rollout and App Server readers.
    pub const MIN: Self = Self::from_unix_parts(-8_334_601_228_800, 0);
    pub const MAX: Self = Self::from_unix_parts(8_210_266_876_799, 999_999_999);
    pub const UNIX_EPOCH: Self = Self::from_unix_parts(0, 0);

    pub fn now() -> Self {
        Self::try_from(SystemTime::now()).expect("system time outside supported UTC bounds")
    }

    pub fn from_jiff(instant: Timestamp) -> Self {
        let nanos = instant.subsec_nanosecond();
        let seconds = instant.as_second() - i64::from(nanos < 0);
        Self::from_unix_parts(seconds, nanos.rem_euclid(NANOS_PER_SECOND as i32) as u32)
    }

    // Callers first validate these parts. Every supported epoch day
    // fits in i32, and the encoded fraction fits comfortably in NonZeroU32.
    const fn from_unix_parts(seconds: i64, nanos: u32) -> Self {
        Self {
            epoch_day: seconds.div_euclid(SECONDS_PER_DAY) as i32,
            second_of_day: seconds.rem_euclid(SECONDS_PER_DAY) as u32,
            nanos_plus_one: NonZeroU32::new(nanos + 1).expect("timestamp fraction encodes nonzero"),
        }
    }

    const fn is_leap_second(self) -> bool {
        self.timestamp_subsec_nanos() >= NANOS_PER_SECOND
    }

    /// Project into Jiff for local calendar calculations. Legacy extended
    /// instants saturate at Jiff's calendar bounds. Use `as_jiff_checked`
    /// when it would be misleading to display an extended instant this way.
    pub const fn as_jiff(self) -> Timestamp {
        match self.as_jiff_checked() {
            Some(instant) => instant,
            None if self.timestamp() < JIFF_MIN_SECOND => Timestamp::MIN,
            None => Timestamp::MAX,
        }
    }

    /// Convert exactly within Jiff's calendar range. A legacy leap second
    /// projects into the preceding civil second, in the same minute.
    pub const fn as_jiff_checked(self) -> Option<Timestamp> {
        let seconds = self.timestamp();
        if seconds < JIFF_MIN_SECOND || seconds > JIFF_MAX_SECOND {
            return None;
        }
        let nanos = self.timestamp_subsec_nanos() % NANOS_PER_SECOND;
        Some(Timestamp::constant(seconds, nanos as i32))
    }

    // A Gregorian 400-year cycle repeats month lengths and weekdays. Let
    // Jiff derive the civil fields in a supported cycle and retain the year
    // displacement separately for extended-date display and date keys.
    fn calendar_parts(self) -> (DateTime, i32) {
        let (instant, year_displacement) = self.calendar_instant();
        (Offset::UTC.to_datetime(instant), year_displacement)
    }

    fn calendar_instant(self) -> (Timestamp, i32) {
        if let Some(instant) = self.as_jiff_checked() {
            return (instant, 0);
        }
        let cycles = (self.epoch_day - EPOCH_DAY_2000).div_euclid(DAYS_PER_GREGORIAN_CYCLE);
        let seconds = self.timestamp() - i64::from(cycles) * SECONDS_PER_GREGORIAN_CYCLE;
        let nanos = (self.timestamp_subsec_nanos() % NANOS_PER_SECOND) as i32;
        (Timestamp::constant(seconds, nanos), cycles * 400)
    }

    /// Construct from a floored Unix second and its nonnegative nanoseconds.
    /// Legacy leap seconds use nanoseconds in 1,000,000,000..2,000,000,000.
    pub fn from_timestamp(seconds: i64, nanos: u32) -> Option<Self> {
        let leap_second = nanos >= NANOS_PER_SECOND;
        if !(Self::MIN.timestamp()..=Self::MAX.timestamp()).contains(&seconds)
            || nanos >= 2 * NANOS_PER_SECOND
            || (leap_second && seconds.rem_euclid(60) != 59)
        {
            return None;
        }
        Some(Self::from_unix_parts(seconds, nanos))
    }

    pub fn from_timestamp_millis(millis: i64) -> Option<Self> {
        Self::from_timestamp(
            millis.div_euclid(1_000),
            (millis.rem_euclid(1_000) as u32) * 1_000_000,
        )
    }

    fn from_epoch_nanos(nanos: i128) -> Option<Self> {
        let nanos_per_second = i128::from(NANOS_PER_SECOND);
        let seconds = i64::try_from(nanos.div_euclid(nanos_per_second)).ok()?;
        Self::from_timestamp(seconds, nanos.rem_euclid(nanos_per_second) as u32)
    }

    pub fn from_utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let (year, seconds_to_restore) = calendar_year(year)?;
        let civil = DateTime::new(
            year,
            i8::try_from(month).ok()?,
            i8::try_from(day).ok()?,
            i8::try_from(hour).ok()?,
            i8::try_from(minute).ok()?,
            i8::try_from(second).ok()?,
            0,
        )
        .ok()?;
        let instant = Self::from_civil(civil)?;
        Self::from_timestamp(instant.timestamp() + seconds_to_restore, 0)
    }

    pub fn from_civil(value: DateTime) -> Option<Self> {
        Offset::UTC.to_timestamp(value).ok().map(Self::from_jiff)
    }

    /// Parse a full date and time with a required UTC or numerical offset.
    /// Accept space in place of `T`, as older rollouts did.
    pub fn parse_from_rfc3339(value: &str) -> Result<Self, TimestampParseError> {
        let bytes = value.as_bytes();
        let year_end = year_end(value).ok_or(TimestampParseError)?;
        let time_end = year_end + 15;
        let tail = bytes.get(time_end..).ok_or(TimestampParseError)?;
        let offset = &tail[fraction_digits(tail).map_or(0, |digits| digits + 1)..];
        if !matches!(bytes.get(year_end + 6), Some(b'T' | b't' | b' '))
            || bytes.get(year_end + 3) != Some(&b'-')
            || bytes.get(year_end + 9) != Some(&b':')
            || bytes.get(year_end + 12) != Some(&b':')
            || !rfc3339_offset(offset)
        {
            return Err(TimestampParseError);
        }
        Self::parse(value, Some(time_end))
    }

    fn parse(original: &str, mut time_end: Option<usize>) -> Result<Self, TimestampParseError> {
        let mut value = Cow::Borrowed(original);
        let mut seconds_to_restore = 0;
        if let Some(end) = year_end(original)
            && (end != 4 || original.starts_with("9999-"))
        {
            let year = original[..end]
                .parse::<i32>()
                .map_err(|_| TimestampParseError)?;
            let (year, restore) = calendar_year(year).ok_or(TimestampParseError)?;
            // Jiff's ISO reader requires signed years to have six digits.
            let normalized_year = if year < 0 {
                format!("{year:+07}")
            } else {
                format!("{year:04}")
            };
            seconds_to_restore = restore;
            time_end = time_end.map(|time_end| time_end - end + normalized_year.len());
            value = Cow::Owned(normalized_year + &original[end..]);
        }
        let leap_second = time_end.is_some_and(|end| &value.as_bytes()[end - 2..end] == b"60");
        if let Some(end) = time_end
            && let Some(digits) = fraction_digits(&value.as_bytes()[end..])
            && digits > 9
        {
            // Older readers retained at most nine fractional digits.
            value = Cow::Owned([&value[..end + 10], &value[end + digits + 1..]].concat());
        }
        let instant = value
            .parse::<Timestamp>()
            .map_err(|_| TimestampParseError)?;
        let timestamp = Self::from_jiff(instant);
        let nanos = timestamp.timestamp_subsec_nanos() + u32::from(leap_second) * NANOS_PER_SECOND;
        Self::from_timestamp(timestamp.timestamp() + seconds_to_restore, nanos)
            .ok_or(TimestampParseError)
    }

    /// Unix seconds rounded down, including for a pre-epoch instant.
    pub const fn timestamp(self) -> i64 {
        self.epoch_day as i64 * SECONDS_PER_DAY + self.second_of_day as i64
    }

    /// Unix milliseconds rounded down. Retains leap-second fractions in keys.
    pub fn timestamp_millis(self) -> i64 {
        self.timestamp() * 1_000 + i64::from(self.timestamp_subsec_nanos() / 1_000_000)
    }

    pub const fn timestamp_subsec_nanos(self) -> u32 {
        self.nanos_plus_one.get() - 1
    }

    pub fn timestamp_nanos_opt(self) -> Option<i64> {
        i64::try_from(
            i128::from(self.timestamp()) * i128::from(NANOS_PER_SECOND)
                + i128::from(self.timestamp_subsec_nanos()),
        )
        .ok()
    }

    pub fn checked_add_signed(self, elapsed: Duration) -> Option<Self> {
        if !self.is_leap_second() {
            if let Some(instant) = self.as_jiff_checked()
                && let Ok(instant) = instant.checked_add(elapsed.0)
            {
                return Some(Self::from_jiff(instant));
            }
            let nanos = i128::from(self.timestamp()) * i128::from(NANOS_PER_SECOND)
                + i128::from(self.timestamp_subsec_nanos());
            return Self::from_epoch_nanos(nanos + elapsed.0.as_nanos());
        }
        let last_second = i128::from(self.timestamp()) * i128::from(NANOS_PER_SECOND);
        let relative_nanos = i128::from(self.timestamp_subsec_nanos()) + elapsed.0.as_nanos();
        let second = i128::from(NANOS_PER_SECOND);
        if (second..2 * second).contains(&relative_nanos) {
            return Self::from_timestamp(self.timestamp(), relative_nanos as u32);
        }
        let after_leap = if relative_nanos >= 2 * second {
            second
        } else {
            0
        };
        Self::from_epoch_nanos(last_second + relative_nanos - after_leap)
    }

    pub fn checked_sub_signed(self, elapsed: Duration) -> Option<Self> {
        self.checked_add_signed(-elapsed)
    }

    pub fn checked_add_days(self, days: u64) -> Option<Self> {
        let duration = Duration::try_days(i64::try_from(days).ok()?)?;
        self.checked_shift_utc_day(duration)
    }

    pub fn checked_sub_days(self, days: u64) -> Option<Self> {
        let duration = Duration::try_days(i64::try_from(days).ok()?)?;
        self.checked_shift_utc_day(-duration)
    }

    fn checked_shift_utc_day(self, duration: Duration) -> Option<Self> {
        let seconds = self.timestamp().checked_add(duration.num_seconds())?;
        Self::from_timestamp(seconds, self.timestamp_subsec_nanos())
    }

    pub fn signed_duration_since(self, other: Self) -> Duration {
        let elapsed = match (self.as_jiff_checked(), other.as_jiff_checked()) {
            (Some(lhs), Some(rhs)) => lhs.duration_since(rhs),
            _ => {
                let seconds = self.timestamp() - other.timestamp();
                let lhs_nanos = (self.timestamp_subsec_nanos() % NANOS_PER_SECOND) as i32;
                let rhs_nanos = (other.timestamp_subsec_nanos() % NANOS_PER_SECOND) as i32;
                SignedDuration::new(seconds, lhs_nanos - rhs_nanos)
            }
        };
        if !self.is_leap_second() && !other.is_leap_second() {
            return Duration(elapsed);
        }
        // A represented leap second lengthens its own UTC day. As with the
        // legacy timestamp, other days have no implicit leap-second table.
        let add_leap = self.is_leap_second() && self.second_of_day >= other.second_of_day;
        let subtract_leap = other.is_leap_second() && other.second_of_day >= self.second_of_day;
        Duration(
            elapsed + SignedDuration::from_secs(i64::from(add_leap) - i64::from(subtract_leap)),
        )
    }

    pub fn date_naive(self) -> Date {
        self.naive_utc().date()
    }

    pub fn naive_utc(self) -> DateTime {
        Offset::UTC.to_datetime(self.as_jiff())
    }

    /// Return the full-range UTC date key for history and persistence.
    pub fn utc_date(self) -> UtcDay {
        UtcDay::from_timestamp(self)
    }

    /// Floor a timestamp to the beginning of its UTC day.
    pub fn utc_day_start(self) -> Self {
        Self::from_unix_parts(i64::from(self.epoch_day) * SECONDS_PER_DAY, 0)
    }

    pub fn format<'f>(self, pattern: &'f str) -> impl fmt::Display + 'f {
        TimestampFormat {
            value: self,
            pattern,
        }
    }

    /// UTC rendering used by existing fingerprints and string-valued records.
    pub fn to_rfc3339(self) -> String {
        self.render('T', "+00:00", None).to_string()
    }

    pub fn to_rfc3339_millis_z(self) -> String {
        self.render('T', "Z", Some(3)).to_string()
    }

    fn render(
        self,
        separator: char,
        suffix: &'static str,
        precision: Option<usize>,
    ) -> TimestampText {
        TimestampText {
            value: self,
            separator,
            suffix,
            precision,
        }
    }
}

fn fraction_digits(tail: &[u8]) -> Option<usize> {
    (tail.first() == Some(&b'.')).then(|| {
        tail[1..]
            .iter()
            .take_while(|digit| digit.is_ascii_digit())
            .count()
    })
}

fn year_end(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let start = usize::from(matches!(bytes.first(), Some(b'-' | b'+')));
    let digits = bytes[start..]
        .iter()
        .take_while(|digit| digit.is_ascii_digit())
        .count();
    let end = start + digits;
    (matches!(bytes.get(end), Some(b'-')) && (digits == 4 || (start == 1 && digits > 4)))
        .then_some(end)
}

fn calendar_year(year: i32) -> Option<(i16, i64)> {
    if (-9_998..=9_998).contains(&year) {
        return Some((year as i16, 0));
    }
    // An input offset can move a timestamp from a neighboring local year
    // back into the valid UTC range. Validate the final instant separately.
    if !(-262_144..=262_143).contains(&year) {
        return None;
    }
    let representative = 2_000 + year.rem_euclid(400);
    let cycles = (year - representative) / 400;
    Some((
        representative as i16,
        i64::from(cycles) * SECONDS_PER_GREGORIAN_CYCLE,
    ))
}

fn rfc3339_offset(offset: &[u8]) -> bool {
    match offset {
        [b'Z' | b'z'] => true,
        [
            b'+' | b'-',
            hour_tens,
            hour_ones,
            b':',
            minute_tens,
            minute_ones,
        ] => {
            let hours = [*hour_tens, *hour_ones];
            let minutes = [*minute_tens, *minute_ones];
            hours.is_ascii() && minutes.is_ascii() && hours <= *b"23" && minutes <= *b"59"
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimestampParseError;

impl fmt::Display for TimestampParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unsupported UTC timestamp")
    }
}

impl std::error::Error for TimestampParseError {}

impl FromStr for UtcTimestamp {
    type Err = TimestampParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let time_end = value
            .find(['T', 't', ' '])
            .map(|separator| separator + 9)
            .filter(|end| {
                let bytes = value.as_bytes();
                bytes.len() >= *end
                    && bytes.get(end - 6) == Some(&b':')
                    && bytes.get(end - 3) == Some(&b':')
            });
        Self::parse(value, time_end)
    }
}

impl TryFrom<SystemTime> for UtcTimestamp {
    type Error = jiff::Error;

    fn try_from(value: SystemTime) -> Result<Self, Self::Error> {
        match Timestamp::try_from(value) {
            Ok(timestamp) => Ok(Self::from_jiff(timestamp)),
            Err(error) => {
                let nanos = match value.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(elapsed) => elapsed.as_nanos() as i128,
                    Err(before_epoch) => -(before_epoch.duration().as_nanos() as i128),
                };
                Self::from_epoch_nanos(nanos).ok_or(error)
            }
        }
    }
}

impl Default for UtcTimestamp {
    fn default() -> Self {
        Self::UNIX_EPOCH
    }
}

struct TimestampText {
    value: UtcTimestamp,
    separator: char,
    suffix: &'static str,
    precision: Option<usize>,
}

fn format_year(output: &mut impl Write, year: i32) -> fmt::Result {
    if (0..=9_999).contains(&year) {
        write!(output, "{year:04}")
    } else {
        write!(output, "{year:+05}")
    }
}

impl fmt::Display for TimestampText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (civil, year_displacement) = self.value.calendar_parts();
        format_year(formatter, i32::from(civil.year()) + year_displacement)?;
        let second = civil.second() + i8::from(self.value.is_leap_second());
        write!(
            formatter,
            "-{:02}-{:02}{}{:02}:{:02}:{second:02}",
            civil.month(),
            civil.day(),
            self.separator,
            civil.hour(),
            civil.minute(),
        )?;
        let nanos = civil.subsec_nanosecond();
        let precision = self.precision.unwrap_or(match nanos {
            0 => 0,
            n if n % 1_000_000 == 0 => 3,
            n if n % 1_000 == 0 => 6,
            _ => 9,
        });
        match precision {
            0 => (),
            3 => write!(formatter, ".{:03}", nanos / 1_000_000)?,
            6 => write!(formatter, ".{:06}", nanos / 1_000)?,
            _ => write!(formatter, ".{nanos:09}")?,
        }
        formatter.write_str(self.suffix)
    }
}

struct TimestampFormat<'f> {
    value: UtcTimestamp,
    pattern: &'f str,
}

impl fmt::Display for TimestampFormat<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (instant, year_displacement) = self.value.calendar_instant();
        if year_displacement == 0 && !self.value.is_leap_second() {
            return instant.strftime(self.pattern).fmt(formatter);
        }
        let year = i32::from(Offset::UTC.to_datetime(instant).year()) + year_displacement;
        let mut expanded = String::with_capacity(self.pattern.len());
        let mut unparsed = self.pattern;
        while let Some((literal, suffix)) = unparsed.split_once('%') {
            expanded.push_str(literal);
            unparsed = match suffix.as_bytes().first() {
                Some(b'Y' | b'F') if year_displacement != 0 => {
                    format_year(&mut expanded, year)?;
                    if suffix.starts_with('F') {
                        expanded.push_str("-%m-%d");
                    }
                    &suffix[1..]
                }
                Some(b's') if year_displacement != 0 => {
                    write!(expanded, "{}", self.value.timestamp())?;
                    &suffix[1..]
                }
                Some(b'S') if self.value.is_leap_second() => {
                    expanded.push_str("60");
                    &suffix[1..]
                }
                Some(b'%') => {
                    expanded.push_str("%%");
                    &suffix[1..]
                }
                _ => {
                    expanded.push('%');
                    suffix
                }
            };
        }
        expanded.push_str(unparsed);
        instant.strftime(&expanded).fmt(formatter)
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.render(' ', " UTC", None).fmt(formatter)
    }
}

impl Serialize for UtcTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.render('T', "Z", None))
    }
}

impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = UtcTimestamp;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an RFC 3339 timestamp")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

impl Add<Duration> for UtcTimestamp {
    type Output = Self;

    fn add(self, duration: Duration) -> Self::Output {
        self.checked_add_signed(duration)
            .expect("timestamp addition out of range")
    }
}

impl AddAssign<Duration> for UtcTimestamp {
    fn add_assign(&mut self, duration: Duration) {
        *self = *self + duration;
    }
}

impl Sub<Duration> for UtcTimestamp {
    type Output = Self;

    fn sub(self, duration: Duration) -> Self::Output {
        self.checked_sub_signed(duration)
            .expect("timestamp subtraction out of range")
    }
}

impl SubAssign<Duration> for UtcTimestamp {
    fn sub_assign(&mut self, duration: Duration) {
        *self = *self - duration;
    }
}

impl Sub for UtcTimestamp {
    type Output = Duration;

    fn sub(self, other: Self) -> Self::Output {
        self.signed_duration_since(other)
    }
}

/// Elapsed time with fixed 24-hour days and the existing millisecond bounds.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Duration(SignedDuration);

impl Duration {
    const MAX_NANOS: i128 = i64::MAX as i128 * 1_000_000;
    pub const ZERO: Self = Self(SignedDuration::ZERO);

    pub const fn zero() -> Self {
        Self::ZERO
    }

    pub const fn seconds(seconds: i64) -> Self {
        Self::try_seconds(seconds).expect("duration seconds out of range")
    }

    pub const fn try_seconds(seconds: i64) -> Option<Self> {
        Self::bounded(SignedDuration::from_secs(seconds))
    }

    pub const fn milliseconds(millis: i64) -> Self {
        match Self::bounded(SignedDuration::from_millis(millis)) {
            Some(duration) => duration,
            None => panic!("duration milliseconds out of range"),
        }
    }

    pub const fn microseconds(micros: i64) -> Self {
        Self(SignedDuration::from_micros(micros))
    }

    pub const fn nanoseconds(nanos: i64) -> Self {
        Self(SignedDuration::from_nanos(nanos))
    }

    pub const fn minutes(minutes: i64) -> Self {
        Self::try_minutes(minutes).expect("duration minutes out of range")
    }

    pub const fn try_minutes(minutes: i64) -> Option<Self> {
        match SignedDuration::try_from_mins(minutes) {
            Some(value) => Self::bounded(value),
            None => None,
        }
    }

    pub const fn hours(hours: i64) -> Self {
        match SignedDuration::try_from_hours(hours) {
            Some(value) => Self::bounded(value).expect("duration hours out of range"),
            None => panic!("duration hours out of range"),
        }
    }

    pub const fn days(days: i64) -> Self {
        Self::try_days(days).expect("duration days out of range")
    }

    pub const fn try_days(days: i64) -> Option<Self> {
        match days.checked_mul(SECONDS_PER_DAY) {
            Some(seconds) => Self::try_seconds(seconds),
            None => None,
        }
    }

    pub const fn weeks(weeks: i64) -> Self {
        Self::days(weeks.checked_mul(7).expect("duration weeks out of range"))
    }

    pub const fn num_seconds(self) -> i64 {
        self.0.as_secs()
    }

    pub const fn num_minutes(self) -> i64 {
        self.0.as_mins()
    }

    pub const fn num_hours(self) -> i64 {
        self.0.as_hours()
    }

    pub const fn num_days(self) -> i64 {
        self.num_seconds() / SECONDS_PER_DAY
    }

    pub const fn num_milliseconds(self) -> i64 {
        self.0.as_millis() as i64
    }

    pub const fn as_jiff(self) -> SignedDuration {
        self.0
    }

    pub fn from_std(value: StdDuration) -> Result<Self, DurationRangeError> {
        SignedDuration::try_from(value)
            .ok()
            .and_then(Self::bounded)
            .ok_or(DurationRangeError)
    }

    pub fn to_std(self) -> Result<StdDuration, DurationRangeError> {
        StdDuration::try_from(self.0).map_err(|_| DurationRangeError)
    }

    const fn bounded(value: SignedDuration) -> Option<Self> {
        let nanos = value.as_nanos();
        if nanos < -Self::MAX_NANOS || nanos > Self::MAX_NANOS {
            None
        } else {
            Some(Self(value))
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DurationRangeError;

impl fmt::Display for DurationRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("duration out of range")
    }
}

impl std::error::Error for DurationRangeError {}

impl Add for Duration {
    type Output = Self;

    fn add(self, other: Self) -> Self::Output {
        Self::bounded(self.0 + other.0).expect("duration addition out of range")
    }
}

impl Sub for Duration {
    type Output = Self;

    fn sub(self, other: Self) -> Self::Output {
        Self::bounded(self.0 - other.0).expect("duration subtraction out of range")
    }
}

impl Div<i32> for Duration {
    type Output = Self;

    fn div(self, divisor: i32) -> Self::Output {
        Self(
            self.0
                .checked_div(divisor)
                .expect("invalid duration division"),
        )
    }
}

impl Neg for Duration {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self(-self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_old_json_and_fingerprint_timestamp_spelling() {
        for (input, utc) in [
            ("2026-11-18T09:11:03+08:00", "2026-11-18T01:11:03"),
            ("2026-11-18T01:11:03.1Z", "2026-11-18T01:11:03.100"),
            ("2026-11-18T01:11:03.0001Z", "2026-11-18T01:11:03.000100"),
            (
                "2026-11-18T01:11:03.12345678Z",
                "2026-11-18T01:11:03.123456780",
            ),
            (
                "2026-11-18T01:11:03.1234567894Z",
                "2026-11-18T01:11:03.123456789",
            ),
        ] {
            let timestamp = UtcTimestamp::parse_from_rfc3339(input).unwrap();
            let decoded: UtcTimestamp = serde_json::from_str(&format!("\"{input}\"")).unwrap();
            assert_eq!(decoded, timestamp);
            assert_eq!(
                serde_json::to_string(&timestamp).unwrap(),
                format!("\"{utc}Z\"")
            );
            assert_eq!(timestamp.to_rfc3339(), format!("{utc}+00:00"));
        }
        let timestamp = UtcTimestamp::UNIX_EPOCH + Duration::nanoseconds(123_456_789);
        assert_eq!(timestamp.to_rfc3339_millis_z(), "1970-01-01T00:00:00.123Z");
        assert_eq!(
            timestamp.format("%Y%m%d%H%M%S%3f").to_string(),
            "19700101000000123"
        );
    }

    #[test]
    fn negative_unix_timestamp_keys_floor_toward_the_previous_second() {
        let timestamp = UtcTimestamp::from_timestamp(-1, 999_999_999).unwrap();
        assert_eq!(timestamp.timestamp(), -1);
        assert_eq!(timestamp.timestamp_millis(), -1);
        assert_eq!(timestamp.timestamp_nanos_opt(), Some(-1));
        assert_eq!(timestamp.timestamp_subsec_nanos(), 999_999_999);
        assert_eq!(
            timestamp.to_rfc3339(),
            "1969-12-31T23:59:59.999999999+00:00"
        );
        // A Windows SystemTime tick is 100 ns. The timestamp assertions above
        // exercise the finer precision without going through the platform clock.
        let before_epoch = SystemTime::UNIX_EPOCH
            .checked_sub(StdDuration::from_nanos(100))
            .unwrap();
        assert_eq!(
            UtcTimestamp::try_from(before_epoch).unwrap(),
            UtcTimestamp::from_timestamp(-1, 999_999_900).unwrap(),
        );
        assert_eq!(
            UtcTimestamp::from_timestamp_millis(-1)
                .unwrap()
                .timestamp_millis(),
            -1
        );

        let timestamp = UtcTimestamp::from_timestamp(-9_223_372_037, 145_224_192).unwrap();
        assert_eq!(timestamp.timestamp_nanos_opt(), Some(i64::MIN));
    }

    #[test]
    fn legacy_leap_seconds_round_trip_without_reordering_their_minute() {
        let before = "2016-12-31T23:59:59.999Z".parse::<UtcTimestamp>().unwrap();
        let leap = "2017-01-01T00:59:60.125+01:00"
            .parse::<UtcTimestamp>()
            .unwrap();
        let after = "2017-01-01T00:00:00Z".parse::<UtcTimestamp>().unwrap();
        assert!(before < leap && leap < after);
        assert_eq!(leap.timestamp(), before.timestamp());
        assert_eq!(leap.timestamp_subsec_nanos(), 1_125_000_000);
        assert_eq!(
            serde_json::to_string(&leap).unwrap(),
            "\"2016-12-31T23:59:60.125Z\""
        );
        assert_eq!(leap + Duration::milliseconds(875), after);
        assert_eq!(
            (leap - Duration::milliseconds(126)).to_rfc3339(),
            before.to_rfc3339()
        );
        assert_eq!(leap - before, Duration::milliseconds(126));
        assert_eq!(
            leap.checked_add_days(1).unwrap().to_rfc3339(),
            "2017-01-01T23:59:60.125+00:00"
        );
    }

    #[test]
    fn malformed_inputs_and_calendar_overflow_remain_fallible() {
        for input in [
            "",
            "2026-11-18T01:11:",
            "2026-11-18T01:11:03",
            "2026-11-18T01:11:03+0800",
            "2026-11-18T01:11:03+24:00",
        ] {
            assert!(UtcTimestamp::parse_from_rfc3339(input).is_err(), "{input}");
        }
        assert!("2026-11-18T01:11:03+0800".parse::<UtcTimestamp>().is_ok());
        assert!(UtcTimestamp::from_timestamp(i64::MIN, 0).is_none());
        assert!(
            UtcTimestamp::MAX
                .checked_add_signed(Duration::nanoseconds(1))
                .is_none()
        );
        assert!(UtcTimestamp::MIN.checked_sub_days(1).is_none());
        assert_eq!(UtcTimestamp::MIN.utc_day_start(), UtcTimestamp::MIN);
        assert!(Duration::try_days(i64::MAX).is_none());
        assert!(Duration::try_minutes(i64::MAX).is_none());
        assert_eq!(
            Duration::milliseconds(i64::MAX).num_milliseconds(),
            i64::MAX
        );
        assert_eq!(Duration::milliseconds(-1_999).num_seconds(), -1);
        assert!(Duration::milliseconds(-1).to_std().is_err());
    }

    #[test]
    fn signed_years_and_legacy_calendar_bounds_round_trip() {
        let signed_year = UtcTimestamp::from_utc(-1, 1, 3, 0, 0, 0).unwrap();
        assert_eq!(
            serde_json::to_string(&signed_year).unwrap(),
            "\"-0001-01-03T00:00:00Z\""
        );
        for timestamp in [UtcTimestamp::MIN, signed_year, UtcTimestamp::MAX] {
            let json = serde_json::to_string(&timestamp).unwrap();
            assert_eq!(
                serde_json::from_str::<UtcTimestamp>(&json).unwrap(),
                timestamp
            );
        }
        assert_eq!(UtcTimestamp::default(), UtcTimestamp::UNIX_EPOCH);
        assert_eq!(Duration::nanoseconds(-3) / 2, Duration::nanoseconds(-1));
    }

    #[test]
    fn extended_instants_preserve_existing_seconds_and_wire_values() {
        let timestamp = UtcTimestamp::from_timestamp(1_783_834_200_123, 0).unwrap();
        assert_eq!(timestamp.timestamp(), 1_783_834_200_123);
        assert_eq!(timestamp.to_rfc3339(), "+58497-06-15T04:02:03+00:00");
        assert_eq!(timestamp.to_string(), "+58497-06-15 04:02:03 UTC");
        assert_eq!(
            timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            timestamp.to_string()
        );
        assert_eq!(
            timestamp.utc_day_start().to_rfc3339(),
            "+58497-06-15T00:00:00+00:00"
        );
        assert_eq!(timestamp.as_jiff_checked(), None);
        assert_eq!(
            UtcTimestamp::parse_from_rfc3339("+58497-06-15T12:02:03+08:00").unwrap(),
            timestamp
        );
        assert_eq!(
            UtcTimestamp::from_utc(58_497, 6, 15, 4, 2, 3),
            Some(timestamp)
        );
        for (bound, text) in [
            (UtcTimestamp::MIN, "-262143-01-01T00:00:00Z"),
            (UtcTimestamp::MAX, "+262142-12-31T23:59:59.999999999Z"),
        ] {
            assert_eq!(UtcTimestamp::parse_from_rfc3339(text).unwrap(), bound);
            assert_eq!(
                serde_json::to_string(&bound).unwrap(),
                format!("\"{text}\"")
            );
        }
        // Year 10000 exceeds Jiff's native range and fits the Windows clock.
        let system_time = SystemTime::UNIX_EPOCH
            .checked_add(StdDuration::from_secs(253_402_300_800))
            .unwrap();
        assert_eq!(
            UtcTimestamp::try_from(system_time).unwrap(),
            UtcTimestamp::from_utc(10_000, 1, 1, 0, 0, 0).unwrap(),
        );
        assert_eq!(
            (timestamp - Duration::nanoseconds(1)).timestamp_subsec_nanos(),
            999_999_999
        );
        assert_eq!(
            timestamp - (timestamp - Duration::nanoseconds(1)),
            Duration::nanoseconds(1)
        );
        assert_eq!(
            timestamp - UtcTimestamp::UNIX_EPOCH,
            Duration::seconds(1_783_834_200_123)
        );
        assert!(UtcTimestamp::from_timestamp(UtcTimestamp::MAX.timestamp() + 1, 0).is_none());
        assert!(UtcTimestamp::from_timestamp(UtcTimestamp::MIN.timestamp() - 1, 0).is_none());
    }

    #[test]
    fn timestamp_storage_and_optional_niche_match_previous_size() {
        assert_eq!(std::mem::size_of::<UtcTimestamp>(), 12);
        assert_eq!(std::mem::size_of::<Option<UtcTimestamp>>(), 12);
        assert_eq!(UtcTimestamp::MIN.as_jiff(), Timestamp::MIN);
        assert_eq!(UtcTimestamp::MAX.as_jiff(), Timestamp::MAX);
        assert_eq!(
            UtcTimestamp::from_jiff(Timestamp::MIN).as_jiff_checked(),
            Some(Timestamp::MIN)
        );
        assert_eq!(
            UtcTimestamp::from_jiff(Timestamp::MAX).as_jiff_checked(),
            Some(Timestamp::MAX)
        );
        let before = UtcTimestamp::from_timestamp(-1, 999_999_999).unwrap();
        assert!(UtcTimestamp::MIN < before);
        assert!(before < UtcTimestamp::UNIX_EPOCH);
        assert!(UtcTimestamp::UNIX_EPOCH < UtcTimestamp::MAX);
    }
}
