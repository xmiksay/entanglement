//! UTC-date formatting with no external date crate — split out of
//! `system_prompt` (which was the sole caller) purely to keep that file under
//! the 400-line cap; the computation itself has no coupling to prompt assembly.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current UTC date as `YYYY-MM-DD`. Uses the wall clock; a system clock before
/// the epoch degrades to `1970-01-01` rather than panicking.
pub(crate) fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a count of days since 1970-01-01 into `(year, month, day)`.
/// Howard Hinnant's `civil_from_days` — exact, no leap-second/timezone handling
/// (UTC calendar date), and no external date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 1704067200 / 86400
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
