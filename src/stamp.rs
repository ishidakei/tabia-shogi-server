//! Wall-clock stamps: the `Game_ID`'s date, a record's two timestamps, a row's
//! `started_at` and `ended_at`, and a backup file's name.
//!
//! A core module because two layers need one calendar and neither may name the
//! other: the session layer mints identifiers and record headers, and the
//! storage layer names a backup file after the moment it was taken.
//!
//! Not on the clock path. What is produced here is an identifier, two header
//! lines and a filename; every measured duration in this crate is a monotonic
//! [`Instant`], because a `SystemTime` on the time-control path would let an NTP
//! step charge a player for time they did not use.
//!
//! [`Instant`]: tokio::time::Instant
//!
//! Both forms are UTC, because the clock a log line carries is UTC and
//! reconstructing a disputed game means lining a record up against the log. No
//! timezone database is carried, and the record format has no offset field for a
//! reader to tell a local stamp from a UTC one by.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds in a day, as the divisor both forms below use.
const DAY: u64 = 86_400;

/// Today's date in UTC, as `YYYYMMDD`.
///
/// A `Game_ID`'s uniqueness comes from its two counters rather than the date, so
/// a date that rolls over mid-round changes nothing.
pub fn utc_date() -> String {
    let (year, month, day, _, _, _) = civil(SystemTime::now());

    format!("{year:04}{month:02}{day:02}")
}

/// `at` in UTC, as `yyyy/MM/dd HH:mm:ss` — a record's `$START_TIME` and
/// `'$END_TIME`.
///
/// Takes the moment rather than reading the clock, so that the two stamps of one
/// record are the two moments the game actually reached and not two readings
/// taken where they happened to be formatted.
pub fn stamp(at: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = civil(at);

    format!("{year:04}/{month:02}/{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// `at` in UTC, as RFC 3339 — `yyyy-MM-ddTHH:mm:ssZ`.
///
/// What a `games` row and its sidecar store, and not what a record header
/// stores: the record's format is fixed by the CSA convention [`stamp`] writes,
/// and a column that orders lexicographically is what the newest-first index is
/// for.
///
/// Takes the moment, on [`stamp`]'s terms.
pub fn rfc3339(at: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = civil(at);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `at` in UTC, as ISO 8601's basic form — `YYYYMMDDTHHMMSSZ`.
///
/// A filename form: the same moment [`rfc3339`] writes, with the separators
/// dropped because RFC 3339's include colons, which half the tools an operator
/// would reach for treat as something other than a character. What is left is
/// fixed width and zero-padded, so a string comparison over two of them is a
/// comparison of two moments — which is what lets a backup directory be pruned
/// by sorting its names.
///
/// Takes the moment, on [`stamp`]'s terms.
///
/// # Examples
///
/// ```
/// use std::time::{Duration, UNIX_EPOCH};
/// use tabia_shogi_server::stamp::{compact, rfc3339};
///
/// let at = UNIX_EPOCH + Duration::from_secs(1_787_142_896);
/// assert_eq!(compact(at), "20260819T123456Z");
/// assert_eq!(rfc3339(at), "2026-08-19T12:34:56Z");
/// ```
pub fn compact(at: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = civil(at);

    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// `at` broken into its UTC civil fields.
///
/// A clock before the epoch yields the epoch: the value is a header line or an
/// identifier, and a system whose clock reads 1969 has a problem this function
/// cannot report to anybody.
fn civil(at: SystemTime) -> (i64, i64, i64, u64, u64, u64) {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (year, month, day) = civil_from_days(i64::try_from(seconds / DAY).unwrap_or(0));
    let within = seconds % DAY;

    (
        year,
        month,
        day,
        within / 3_600,
        (within / 60) % 60,
        within % 60,
    )
}

/// The civil date `days` after 1970-01-01, in the proleptic Gregorian calendar.
///
/// Howard Hinnant's `civil_from_days`, the algorithm the C++ chrono proposal
/// standardized. Written out rather than taken from a crate, since the only
/// thing this server needs a calendar for is the `<date>` field of an identifier
/// and the two timestamps of a record.
///
/// Total for every value this can be handed: the algorithm has no failure mode,
/// and `days` comes from a clock reading rather than from input.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift the epoch to 0000-03-01, so that a leap day is the last day of a
    // year rather than a hole in the middle of one.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };

    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    /// The moment `seconds` after the epoch.
    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn civil_from_days_converts_the_unix_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_converts_a_leap_day() {
        // 2020-02-29, the case a naive month table gets wrong.
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
    }

    #[test]
    fn civil_from_days_converts_a_century_that_is_not_a_leap_year() {
        // 1900-03-01, before the epoch and after a February with no 29th.
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
    }

    #[test]
    fn civil_from_days_converts_a_date_after_the_epoch() {
        // 2026-08-14: an ordinary date well after the epoch.
        assert_eq!(civil_from_days(20_679), (2026, 8, 14));
    }

    #[test]
    fn a_stamp_writes_the_date_and_the_time_of_day() {
        // 2026-08-19 12:34:56 UTC.
        assert_eq!(stamp(at(20_684 * DAY + 45_296)), "2026/08/19 12:34:56");
    }

    #[test]
    fn a_stamp_pads_every_field_to_the_width_the_format_fixes() {
        // 2020-02-29 00:00:00 UTC, and one second later: single-digit fields on
        // both sides of the space, which is where a missing pad shows.
        assert_eq!(stamp(at(18_321 * DAY)), "2020/02/29 00:00:00");
        assert_eq!(stamp(at(18_321 * DAY + 3_661)), "2020/02/29 01:01:01");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn the_two_forms_agree_on_the_date_they_are_taken_at() {
        // One calendar, so a record written today and a `Game_ID` minted today
        // cannot name different days.
        let now = SystemTime::now();
        let (year, month, day, _, _, _) = civil(now);

        assert_eq!(
            utc_date(),
            format!("{year:04}{month:02}{day:02}"),
            "the date field of {}",
            stamp(now)
        );
    }

    #[test]
    fn a_clock_before_the_epoch_is_the_epoch_rather_than_a_panic() {
        assert_eq!(
            stamp(UNIX_EPOCH - Duration::from_secs(1)),
            "1970/01/01 00:00:00"
        );
    }

    #[test]
    fn an_rfc_3339_stamp_is_the_same_moment_the_record_writes() {
        // 2026-08-19 12:34:56 UTC, the moment `a_stamp_writes_...` uses: one
        // calendar, so the row and the record cannot name different seconds.
        let at = at(20_684 * DAY + 45_296);

        assert_eq!(rfc3339(at), "2026-08-19T12:34:56Z");
        assert_eq!(stamp(at), "2026/08/19 12:34:56");
    }

    #[test]
    fn a_compact_stamp_is_the_rfc_3339_one_with_the_separators_taken_out() {
        // The same moment as the two tests above, so all three forms are
        // readable against one another — one calendar, and the compact form is
        // not a second reading of the clock.
        let at = at(20_684 * DAY + 45_296);

        assert_eq!(compact(at), "20260819T123456Z");
        assert_eq!(rfc3339(at), "2026-08-19T12:34:56Z");
        assert!(
            !compact(at).contains(':'),
            "a colon reached a filename: {}",
            compact(at)
        );
    }

    #[test]
    fn a_compact_stamp_pads_every_field_to_the_width_a_filename_fixes() {
        // Fixed width is what makes the sort below a sort of moments, so a
        // single-digit field that lost its pad would be a retention sweep
        // deleting the wrong file.
        assert_eq!(compact(at(18_321 * DAY)), "20200229T000000Z");
        assert_eq!(compact(at(18_321 * DAY + 3_661)), "20200229T010101Z");
        assert_eq!(compact(at(18_321 * DAY)).len(), 16);
    }

    #[test]
    fn compact_stamps_sort_the_way_the_moments_do() {
        // What the backup directory's retention is: the five newest names are
        // the five newest backups only if this holds. The pair crosses a day,
        // so a form that put the time of day before the date would fail here.
        let earlier = compact(at(20_684 * DAY + 3_600));
        let later = compact(at(20_685 * DAY));

        assert!(earlier < later, "{earlier} sorted after {later}");
    }

    #[test]
    fn rfc_3339_stamps_sort_the_way_the_moments_do() {
        // What the `ended_at` index is for: a newest-first page is an ordering
        // over these strings, so the string order has to be the time order.
        let earlier = rfc3339(at(20_684 * DAY + 3_600));
        let later = rfc3339(at(20_684 * DAY + 45_296));

        assert!(earlier < later, "{earlier} sorted after {later}");
    }
}
