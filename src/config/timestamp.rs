//! RFC 3339 timestamps, for the one configured value that is a wall-clock time.
//!
//! `[matchmaking].first_round_at` names an absolute moment — "the first round
//! after startup runs at 09:00 Japan time" — and every other duration in the
//! configuration is a count of seconds from something. A count cannot express
//! it, so this module reads the one format an operator can write such a moment
//! in without ambiguity.
//!
//! **The offset is required**, which is what RFC 3339 asks for and what makes
//! the value mean the same thing on a server whose timezone nobody stated:
//!
//! > date-time = full-date "T" full-time
//! > full-time = partial-time time-offset
//! > time-offset = "Z" / time-numoffset
//!
//! **No date crate.** Every dependency here is carried for a stated reason, and
//! one timestamp key is not a reason: what is needed
//! is one parse of one grammar into a [`SystemTime`], and that is Howard
//! Hinnant's `days_from_civil` plus a fixed-width scan — the same argument
//! `session::server`'s `civil_from_days` records for the other direction.
//!
//! **Sub-second precision is read and discarded.** A fraction is accepted
//! because RFC 3339 has one, and truncated because what this value schedules is
//! a matchmaking round: the difference between 09:00:00.9 and 09:00:00 is not a
//! difference an engine developer can observe, and carrying it would put a
//! precision in the type that nothing downstream keeps.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parses an RFC 3339 `date-time`.
///
/// # Errors
///
/// [`TimestampError`] for anything that is not the grammar above: a missing
/// offset, a field of the wrong width, a field out of range, or a day that does
/// not exist in its month.
///
/// # Examples
///
/// ```
/// use std::time::{Duration, UNIX_EPOCH};
/// use tabia_shogi_server::config::timestamp::parse;
///
/// // The same moment, written in two offsets.
/// let utc = parse("2026-11-14T00:00:00Z").expect("a valid timestamp");
/// let jst = parse("2026-11-14T09:00:00+09:00").expect("a valid timestamp");
/// assert_eq!(utc, jst);
/// assert_eq!(utc, UNIX_EPOCH + Duration::from_secs(1_794_614_400));
///
/// // The offset is not optional.
/// assert!(parse("2026-11-14T00:00:00").is_err());
/// ```
pub fn parse(text: &str) -> Result<SystemTime, TimestampError> {
    let bytes = text.as_bytes();

    // Fixed widths up to the seconds, so a shape that is nearly right — a
    // one-digit hour, a missing separator — is refused as a shape rather than
    // parsed into a number that happens to fit. ASCII first, because every
    // index below is a byte index and a multi-byte character would otherwise
    // decide between a refusal and a panic.
    if !text.is_ascii() || bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(TimestampError::Shape);
    }
    if !matches!(bytes[10], b'T' | b't') || bytes[13] != b':' || bytes[16] != b':' {
        return Err(TimestampError::Shape);
    }

    let year = number(&text[0..4])?;
    let month = number(&text[5..7])?;
    let day = number(&text[8..10])?;
    let hour = number(&text[11..13])?;
    let minute = number(&text[14..16])?;
    let second = number(&text[17..19])?;

    let offset_seconds = offset(&text[19..])?;

    checked("month", month, 1, 12)?;
    checked("day", day, 1, days_in_month(year, month))?;
    checked("hour", hour, 0, 23)?;
    checked("minute", minute, 0, 59)?;
    // 60 is a leap second, which RFC 3339 admits. It lands on the next second,
    // which is what a scheduler wants from it.
    checked("second", second, 0, 60)?;

    let days = days_from_civil(i64::from(year), i64::from(month), i64::from(day));
    let seconds =
        days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)
            - offset_seconds;

    epoch(seconds).ok_or(TimestampError::Unrepresentable)
}

/// The `time-offset` at the end, in seconds east of UTC.
///
/// `Z` and `+00:00` are the same number, and both are written in practice.
fn offset(text: &str) -> Result<i64, TimestampError> {
    // The optional fraction, discarded once its digits are known to be digits:
    // `2026-11-14T00:00:00.5+09:00` differs from the same timestamp without the
    // fraction by half a second, and a round is not scheduled to a half second.
    let text = match text.strip_prefix('.') {
        None => text,
        Some(fraction) => {
            let digits = fraction
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(fraction.len());
            if digits == 0 {
                return Err(TimestampError::Shape);
            }
            &fraction[digits..]
        }
    };

    if matches!(text, "Z" | "z") {
        return Ok(0);
    }

    let bytes = text.as_bytes();
    if bytes.len() != 6 || bytes[3] != b':' {
        return Err(TimestampError::MissingOffset);
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return Err(TimestampError::MissingOffset),
    };

    let hours = number(&text[1..3])?;
    let minutes = number(&text[4..6])?;
    checked("offset hour", hours, 0, 23)?;
    checked("offset minute", minutes, 0, 59)?;

    Ok(sign * (i64::from(hours) * 3_600 + i64::from(minutes) * 60))
}

/// A fixed-width run of ASCII digits, as a number.
///
/// `str::parse` alone would accept `+1` and ` 1`, which the grammar does not.
fn number(text: &str) -> Result<u32, TimestampError> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TimestampError::Shape);
    }

    text.parse().map_err(|_| TimestampError::Shape)
}

/// One field's range, named so that the message says which one was wrong.
fn checked(field: &'static str, value: u32, low: u32, high: u32) -> Result<(), TimestampError> {
    if (low..=high).contains(&value) {
        Ok(())
    } else {
        Err(TimestampError::OutOfRange { field, value })
    }
}

/// Seconds since the Unix epoch as a [`SystemTime`], on either side of it.
fn epoch(seconds: i64) -> Option<SystemTime> {
    let magnitude = Duration::from_secs(seconds.unsigned_abs());

    if seconds < 0 {
        UNIX_EPOCH.checked_sub(magnitude)
    } else {
        UNIX_EPOCH.checked_add(magnitude)
    }
}

/// How many days that month has, the leap year included.
const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        // Out of range, and reported as such by the month check that runs
        // before the day check reads this.
        _ => 0,
    }
}

/// The number of days from 1970-01-01 to the given civil date.
///
/// Howard Hinnant's `days_from_civil`, the inverse of the `civil_from_days` in
/// [`session::server`], written out for the same reason: the whole of what this
/// server needs a calendar for is one date each way.
///
/// Total for every value the checks above let through, and for every value they
/// do not — the algorithm has no failure mode.
///
/// [`session::server`]: crate::session::server
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Shift the epoch to 0000-03-01, so that a leap day is the last day of a
    // year rather than a hole in the middle of one.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

/// Why a configured timestamp is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimestampError {
    /// Not the RFC 3339 shape at all.
    #[error("an RFC 3339 timestamp is written YYYY-MM-DDThh:mm:ss followed by Z or ±hh:mm")]
    Shape,

    /// The shape is right up to the offset, which is where it stops.
    #[error(
        "an RFC 3339 timestamp ends in an offset — Z, or ±hh:mm — without which \
         the moment it names depends on the server's timezone"
    )]
    MissingOffset,

    /// A field outside the range its position allows.
    #[error("{field} {value} is out of range")]
    OutOfRange {
        /// Which field, in the spelling the message uses.
        field: &'static str,
        /// What was written there.
        value: u32,
    },

    /// A year so far from the epoch that no [`SystemTime`] holds it.
    #[error("the timestamp is outside the range this platform's clock can represent")]
    Unrepresentable,
}

/// The moment `[matchmaking].first_round_at` names, and the text it was written
/// as.
///
/// Both, because both are read: the schedule needs the instant, and the startup
/// log line quotes the operator's own string back at them — an offset resolved
/// into some other spelling of the same moment is a line whose reader has to do
/// arithmetic to check their file against it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstRound {
    at: SystemTime,
    written: String,
}

impl FirstRound {
    /// Reads one, keeping the text.
    ///
    /// # Errors
    ///
    /// Whatever [`parse`] refuses.
    pub fn new(written: &str) -> Result<Self, TimestampError> {
        Ok(Self {
            at: parse(written)?,
            written: written.to_owned(),
        })
    }

    /// The moment itself.
    pub const fn at(&self) -> SystemTime {
        self.at
    }
}

impl fmt::Display for FirstRound {
    /// Exactly what the operator wrote.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> SystemTime {
        parse(text).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    fn refused(text: &str) -> TimestampError {
        match parse(text) {
            Err(error) => error,
            Ok(time) => panic!("{text} parsed to {time:?}"),
        }
    }

    fn seconds(text: &str) -> u64 {
        at(text)
            .duration_since(UNIX_EPOCH)
            .expect("the test timestamps are after the epoch")
            .as_secs()
    }

    #[test]
    fn the_epoch_itself_is_zero() {
        assert_eq!(seconds("1970-01-01T00:00:00Z"), 0);
    }

    #[test]
    fn an_offset_moves_the_moment_the_other_way() {
        // 09:00 in Japan is 00:00 UTC: an offset east of UTC is subtracted.
        assert_eq!(
            seconds("2026-11-14T09:00:00+09:00"),
            seconds("2026-11-14T00:00:00Z")
        );
        assert_eq!(
            seconds("2026-11-13T19:00:00-05:00"),
            seconds("2026-11-14T00:00:00Z")
        );
    }

    /// How far apart two timestamps are, in either direction from the epoch —
    /// 1900 is before it, and `seconds` above cannot express that.
    fn apart(later: &str, earlier: &str) -> Duration {
        at(later)
            .duration_since(at(earlier))
            .unwrap_or_else(|error| panic!("{later} is not after {earlier}: {error}"))
    }

    #[test]
    fn a_leap_day_and_a_century_that_is_not_one_both_count() {
        // 2020 is a leap year, so its February has a 29th; 1900 is divisible by
        // 100 and not by 400, so its February does not.
        assert_eq!(
            apart("2020-03-01T00:00:00Z", "2020-02-28T00:00:00Z"),
            Duration::from_secs(2 * 86_400),
        );
        assert_eq!(
            apart("1900-03-01T00:00:00Z", "1900-02-28T00:00:00Z"),
            Duration::from_secs(86_400),
        );
    }

    #[test]
    fn february_has_a_twenty_ninth_only_in_a_leap_year() {
        at("2020-02-29T00:00:00Z");
        assert_eq!(
            refused("2021-02-29T00:00:00Z"),
            TimestampError::OutOfRange {
                field: "day",
                value: 29,
            }
        );
    }

    #[test]
    fn a_lowercase_separator_and_a_lowercase_zulu_are_the_same_timestamp() {
        // RFC 3339 admits both spellings, and a client library that writes them
        // is not writing a different moment.
        assert_eq!(
            seconds("2026-11-14t00:00:00z"),
            seconds("2026-11-14T00:00:00Z")
        );
    }

    #[test]
    fn a_fraction_is_accepted_and_truncated() {
        assert_eq!(
            seconds("2026-11-14T00:00:00.750Z"),
            seconds("2026-11-14T00:00:00Z")
        );
        assert_eq!(
            seconds("2026-11-14T09:00:00.5+09:00"),
            seconds("2026-11-14T00:00:00Z")
        );
        assert_eq!(refused("2026-11-14T00:00:00.Z"), TimestampError::Shape);
    }

    #[test]
    fn a_timestamp_with_no_offset_says_so_rather_than_guessing_one() {
        assert_eq!(
            refused("2026-11-14T00:00:00"),
            TimestampError::MissingOffset
        );
        assert_eq!(
            refused("2026-11-14T00:00:00+0900"),
            TimestampError::MissingOffset
        );
    }

    #[test]
    fn a_field_out_of_range_is_named() {
        for (text, field, value) in [
            ("2026-13-01T00:00:00Z", "month", 13),
            ("2026-11-31T00:00:00Z", "day", 31),
            ("2026-11-14T24:00:00Z", "hour", 24),
            ("2026-11-14T00:60:00Z", "minute", 60),
            ("2026-11-14T00:00:61Z", "second", 61),
            ("2026-11-14T00:00:00+24:00", "offset hour", 24),
        ] {
            assert_eq!(
                refused(text),
                TimestampError::OutOfRange { field, value },
                "{text}",
            );
        }
    }

    #[test]
    fn a_leap_second_lands_on_the_next_second() {
        assert_eq!(
            seconds("2016-12-31T23:59:60Z"),
            seconds("2017-01-01T00:00:00Z")
        );
    }

    #[test]
    fn a_shape_that_is_nearly_right_is_still_refused() {
        for text in [
            "",
            "2026-11-14",
            "2026-11-14T00:00Z",
            "2026-1-14T00:00:00Z",
            "2026/11/14T00:00:00Z",
            "2026-11-14 00:00:00Z",
            "202X-11-14T00:00:00Z",
        ] {
            assert_eq!(refused(text), TimestampError::Shape, "{text}");
        }
    }

    #[test]
    fn a_date_before_the_epoch_is_a_time_before_the_epoch() {
        let before = at("1969-12-31T23:59:59Z");

        assert_eq!(
            UNIX_EPOCH
                .duration_since(before)
                .expect("it is before the epoch"),
            Duration::from_secs(1),
        );
    }

    #[test]
    fn a_first_round_keeps_the_text_the_operator_wrote() {
        let written = "2026-11-14T09:00:00+09:00";
        let first = FirstRound::new(written).expect("it is a valid timestamp");

        assert_eq!(first.to_string(), written);
        assert_eq!(first.at(), at("2026-11-14T00:00:00Z"));
    }
}
