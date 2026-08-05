//! `Date` formatting, to the precision the tables print it at.
//!
//! Three or four crates could supply this. None of them would reproduce
//! ECMAScript's expanded-year form (`+275760-09-13T00:00:00.000Z`), its exact
//! three-digit milliseconds, or its `TimeClip` range check without adaptation,
//! and the whole requirement is one format string over Howard Hinnant's
//! `civil_from_days`.

use super::{format_integer, js_number};

/// `TimeClip`'s bound: 100 000 000 days either side of the epoch.
const MAX_TIME_MS: f64 = 8.64e15;

/// `new Date(ms).toISOString()`, or `None` when the instant is out of range.
///
/// The caller mirrors the TypeScript's guard — it checks `getTime()` is finite
/// before formatting — so an out-of-range value degrades to a bare number
/// rather than throwing.
#[must_use]
pub fn iso8601_from_ms(ms: f64) -> Option<String> {
    // `new Date(v)` applies ToInteger, which truncates toward zero.
    if !ms.is_finite() {
        return None;
    }
    let ms = ms.trunc();
    if ms.abs() > MAX_TIME_MS {
        return None;
    }

    let total = ms as i64;
    // Euclidean division: the epoch is in the middle of the range, so negative
    // instants must floor, not truncate.
    let days = total.div_euclid(86_400_000);
    let rem = total.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);

    let hour = rem / 3_600_000;
    let minute = rem % 3_600_000 / 60_000;
    let second = rem % 60_000 / 1000;
    let milli = rem % 1000;

    // ECMA-262 §21.4.1.33: four digits inside 0..=9999, otherwise an expanded
    // year with a mandatory sign and six digits.
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
    };
    Some(format!(
        "{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z"
    ))
}

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
///
/// Hinnant's `civil_from_days`, which is exact over the whole `i64` range and
/// needs no tables.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = yoe as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `formatUnixSeconds` (`market-request.ts:540`).
///
/// The seconds are interpolated through `String(n)`, so they are *ungrouped*
/// here while the same value would carry commas through `formatInteger`
/// elsewhere. That asymmetry is in the original and is load-bearing for the
/// snapshot tests.
#[must_use]
pub fn unix_seconds_display(seconds: f64) -> String {
    match iso8601_from_ms(seconds * 1000.0) {
        Some(iso) => format!("{} ({iso})", js_number(seconds)),
        None => js_number(seconds),
    }
}

/// `formatMilliseconds` (`market-request.ts:545`).
///
/// `padStart(2, "0")` pads but never truncates, so an uptime past 100 hours
/// widens the clock field rather than wrapping it.
#[must_use]
pub fn milliseconds_display(milliseconds: f64) -> String {
    let total_seconds = (milliseconds / 1000.0).floor();
    let hours = (total_seconds / 3600.0).floor();
    let minutes = ((total_seconds % 3600.0) / 60.0).floor();
    let seconds = total_seconds % 60.0;
    format!(
        "{} ms (uptime {}:{}:{})",
        format_integer(milliseconds),
        pad_start_2(hours),
        pad_start_2(minutes),
        pad_start_2(seconds),
    )
}

fn pad_start_2(v: f64) -> String {
    let s = js_number(v);
    if s.len() >= 2 { s } else { format!("0{s}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expanded-year form and the `TimeClip` bound, stated directly rather
    /// than left to the sampled corpus.
    #[test]
    fn expanded_years_and_range() {
        assert_eq!(iso8601_from_ms(0.0).unwrap(), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_from_ms(8.64e15).unwrap(), "+275760-09-13T00:00:00.000Z");
        assert_eq!(iso8601_from_ms(-8.64e15).unwrap(), "-271821-04-20T00:00:00.000Z");
        assert!(iso8601_from_ms(8.64e15 + 1.0).is_none());
        assert!(iso8601_from_ms(f64::NAN).is_none());
    }

    /// `padStart(2, "0")` pads but never truncates, so a long uptime widens the
    /// clock rather than wrapping it. R21.
    #[test]
    fn uptime_clock_widens_past_two_digits() {
        assert_eq!(milliseconds_display(360_000_000.0), "360,000,000 ms (uptime 100:00:00)");
        assert_eq!(milliseconds_display(0.0), "0 ms (uptime 00:00:00)");
    }

    /// The seconds are interpolated ungrouped even though the same value would
    /// carry commas through `formatInteger` elsewhere. R21.
    #[test]
    fn unix_seconds_are_ungrouped() {
        assert_eq!(
            unix_seconds_display(1_700_000_000.0),
            "1700000000 (2023-11-14T22:13:20.000Z)"
        );
    }
}
