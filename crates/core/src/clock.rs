//! UTC timestamps in the protocol's RFC 3339 nanosecond form.
//!
//! The protocol tags every frame and every photo with a capture time; the device is the
//! only party that knows it, so the core formats its own clock instead of pulling in a
//! date-time dependency.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current UTC time as RFC 3339 with nanosecond precision.
///
/// The fraction is trimmed of trailing zeros and omitted entirely on a whole second,
/// exactly like Go's `RFC3339Nano`, so `2025-10-09T08:53:20Z` and
/// `2025-10-09T08:53:20.123456789Z` are both valid output.
pub fn now_rfc3339_nano() -> String {
    format_rfc3339_nano(SystemTime::now())
}

/// Formats `time` as RFC 3339 with nanosecond precision in UTC.
///
/// Times before the Unix epoch (a clock that is not set) format as the epoch itself.
fn format_rfc3339_nano(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = elapsed.as_secs();
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = rest / 3_600;
    let minute = rest % 3_600 / 60;
    let second = rest % 60;

    let mut fraction = format!("{:09}", elapsed.subsec_nanos());
    while fraction.ends_with('0') {
        fraction.pop();
    }

    if fraction.is_empty() {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction}Z")
    }
}

/// Converts days since `1970-01-01` into a civil `(year, month, day)` triple.
///
/// Howard Hinnant's `civil_from_days` algorithm, valid for the whole `u64` day range.
fn civil_from_days(days: u64) -> (i64, u32, u32) {
    let shifted = days as i64 + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::format_rfc3339_nano;

    #[test]
    fn formats_whole_seconds_without_a_fraction() {
        assert_eq!(
            format_rfc3339_nano(UNIX_EPOCH),
            "1970-01-01T00:00:00Z",
            "epoch must format without a fractional part"
        );
        assert_eq!(
            format_rfc3339_nano(UNIX_EPOCH + Duration::from_secs(86_400)),
            "1970-01-02T00:00:00Z"
        );
    }

    #[test]
    fn trims_trailing_zeros_of_the_nanosecond_fraction() {
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            format_rfc3339_nano(base + Duration::from_millis(123)),
            "2023-11-14T22:13:20.123Z"
        );
        // The fractions below are whole multiples of 100 ns on purpose: `SystemTime` carries
        // 100 ns granularity on Windows (FILETIME), so a value that is not a multiple of it
        // does not survive the round trip through one there — it comes back as 123_456_700,
        // and the trailing zeros this test is about are then the platform's, not the
        // formatter's. Multiples of 100 ns format identically on every host.
        assert_eq!(
            format_rfc3339_nano(base + Duration::new(0, 100_000_000)),
            "2023-11-14T22:13:20.1Z"
        );
        assert_eq!(
            format_rfc3339_nano(base + Duration::new(0, 123_456_000)),
            "2023-11-14T22:13:20.123456Z"
        );
        // A fraction whose last digit is not zero is not trimmed at all.
        assert_eq!(
            format_rfc3339_nano(base + Duration::new(0, 123_456_700)),
            "2023-11-14T22:13:20.1234567Z"
        );
    }
}
