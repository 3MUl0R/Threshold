//! Cron expression parsing and next-run computation.
//!
//! Uses the `cron` crate which expects **7-field** expressions:
//!
//! ```text
//! sec  min  hour  day  month  weekday  year
//!  0    0    3     *     *       *       *     → 3:00 AM every day
//!  0   */30  *     *     *       *       *     → every 30 minutes
//! ```
//!
//! For convenience, 6-field expressions (without year) are accepted and
//! automatically normalized by appending `*` for the year field.
//!
//! Times are UTC by default. An optional IANA timezone (e.g., `America/Los_Angeles`)
//! can be provided for timezone-aware scheduling — the cron expression is evaluated
//! in local time and the resulting `next_run` is stored as UTC.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::str::FromStr;

/// Normalize a cron expression to 7 fields.
///
/// If 6 fields are given (missing year), appends `*`.
/// Expressions with any other field count are returned as-is and will
/// produce a parse error downstream.
pub fn normalize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 6 {
        format!("{} *", fields.join(" "))
    } else {
        fields.join(" ")
    }
}

/// Compute the next run time for a cron expression after the current moment.
///
/// Returns `None` if the expression is invalid or has no upcoming occurrence.
pub fn compute_next_run(cron_expr: &str) -> Option<DateTime<Utc>> {
    compute_next_run_after(cron_expr, Utc::now())
}

/// Compute the next run time for a cron expression after a given time.
///
/// Returns `None` if the expression is invalid or has no upcoming occurrence.
pub fn compute_next_run_after(cron_expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let normalized = normalize_cron(cron_expr);
    let schedule = cron::Schedule::from_str(&normalized).ok()?;
    schedule.after(&after).next()
}

/// Compute the next run time for a timezone-aware cron expression.
///
/// The cron expression is evaluated in the given IANA timezone (e.g.,
/// `America/Los_Angeles`), and the result is converted to UTC. This means
/// "noon daily" in Pacific time will correctly shift between 20:00 UTC (PST)
/// and 19:00 UTC (PDT) across daylight saving transitions.
///
/// Returns `None` if the expression is invalid or has no upcoming occurrence.
pub fn compute_next_run_tz(cron_expr: &str, tz: &Tz) -> Option<DateTime<Utc>> {
    compute_next_run_after_tz(cron_expr, Utc::now(), tz)
}

/// Compute the next run time for a timezone-aware cron expression after a given time.
pub fn compute_next_run_after_tz(
    cron_expr: &str,
    after: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    let normalized = normalize_cron(cron_expr);
    let schedule = cron::Schedule::from_str(&normalized).ok()?;
    let after_local = after.with_timezone(tz);
    let next_local = schedule.after(&after_local).next()?;
    Some(next_local.with_timezone(&Utc))
}

/// Parse and validate an IANA timezone string.
///
/// Returns the parsed `Tz` or an error message.
pub fn parse_timezone(tz_str: &str) -> Result<Tz, String> {
    tz_str
        .parse::<Tz>()
        .map_err(|_| format!("Unknown timezone: '{}'. Use an IANA timezone like 'America/Los_Angeles' or 'US/Pacific'.", tz_str))
}

/// Validate a cron expression without computing a next run time.
///
/// Returns `Ok(())` if the expression is valid, or an error message describing
/// the parse failure.
pub fn validate_cron(cron_expr: &str) -> Result<(), String> {
    let normalized = normalize_cron(cron_expr);
    cron::Schedule::from_str(&normalized)
        .map(|_| ())
        .map_err(|e| format!("Invalid cron expression: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike};

    #[test]
    fn normalize_6_field_appends_year() {
        let result = normalize_cron("0 0 3 * * *");
        assert_eq!(result, "0 0 3 * * * *");
    }

    #[test]
    fn normalize_7_field_unchanged() {
        let result = normalize_cron("0 0 3 * * * *");
        assert_eq!(result, "0 0 3 * * * *");
    }

    #[test]
    fn normalize_5_field_left_as_is() {
        // 5-field POSIX cron is not valid for the cron crate;
        // we don't auto-fix it (it will fail validation).
        let result = normalize_cron("0 3 * * *");
        assert_eq!(result, "0 3 * * *");
    }

    #[test]
    fn validate_valid_expression() {
        assert!(validate_cron("0 0 3 * * * *").is_ok());
    }

    #[test]
    fn validate_valid_6_field() {
        assert!(validate_cron("0 0 3 * * *").is_ok());
    }

    #[test]
    fn validate_invalid_expression() {
        let result = validate_cron("not a cron");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid cron expression"));
    }

    #[test]
    fn compute_next_run_returns_future_time() {
        let now = Utc::now();
        // Every minute
        let next = compute_next_run("0 * * * * * *");
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn compute_next_run_after_specific_time() {
        let after = Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap();
        // Every day at 3 AM
        let next = compute_next_run_after("0 0 3 * * * *", after);
        assert!(next.is_some());
        let next = next.unwrap();
        assert!(next > after);
        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn compute_next_run_6_field_works() {
        let next = compute_next_run("0 */30 * * * *");
        assert!(next.is_some());
    }

    #[test]
    fn compute_next_run_invalid_returns_none() {
        let next = compute_next_run("invalid");
        assert!(next.is_none());
    }

    #[test]
    fn compute_next_run_advances_across_day_boundary() {
        // 11:59 PM — next 3 AM should be next day
        let after = Utc.with_ymd_and_hms(2025, 6, 15, 23, 59, 0).unwrap();
        let next = compute_next_run_after("0 0 3 * * * *", after);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.day(), 16);
        assert_eq!(next.hour(), 3);
    }

    #[test]
    fn parse_timezone_valid() {
        let tz = parse_timezone("America/Los_Angeles");
        assert!(tz.is_ok());
        assert_eq!(tz.unwrap(), chrono_tz::America::Los_Angeles);
    }

    #[test]
    fn parse_timezone_us_pacific() {
        assert!(parse_timezone("US/Pacific").is_ok());
    }

    #[test]
    fn parse_timezone_invalid() {
        let result = parse_timezone("Mars/Olympus_Mons");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown timezone"));
    }

    #[test]
    fn compute_next_run_tz_noon_pacific_during_pst() {
        // Jan 15, 2026 at 10:00 UTC (2:00 AM PST) — noon PST = 20:00 UTC
        let after = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        let tz = chrono_tz::America::Los_Angeles;
        let next = compute_next_run_after_tz("0 0 12 * * * *", after, &tz);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.hour(), 20); // noon PST = 20:00 UTC (UTC-8)
        assert_eq!(next.day(), 15);
    }

    #[test]
    fn compute_next_run_tz_noon_pacific_during_pdt() {
        // Jul 15, 2026 at 10:00 UTC (3:00 AM PDT) — noon PDT = 19:00 UTC
        let after = Utc.with_ymd_and_hms(2026, 7, 15, 10, 0, 0).unwrap();
        let tz = chrono_tz::America::Los_Angeles;
        let next = compute_next_run_after_tz("0 0 12 * * * *", after, &tz);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.hour(), 19); // noon PDT = 19:00 UTC (UTC-7)
        assert_eq!(next.day(), 15);
    }

    #[test]
    fn compute_next_run_tz_crosses_dst_boundary() {
        // March 7, 2026 (before spring-forward on Mar 8, 2026)
        // At 6 AM UTC on Mar 7 — should get noon PST on Mar 7 (20:00 UTC)
        let after = Utc.with_ymd_and_hms(2026, 3, 7, 6, 0, 0).unwrap();
        let tz = chrono_tz::America::Los_Angeles;
        let next = compute_next_run_after_tz("0 0 12 * * * *", after, &tz);
        let next = next.unwrap();
        assert_eq!(next.day(), 7);
        assert_eq!(next.hour(), 20); // PST, UTC-8

        // After that, next firing on Mar 8 (PDT) should be 19:00 UTC
        let next2 = compute_next_run_after_tz("0 0 12 * * * *", next, &tz);
        let next2 = next2.unwrap();
        assert_eq!(next2.day(), 8);
        assert_eq!(next2.hour(), 19); // PDT, UTC-7
    }

    #[test]
    fn dst_spring_forward_gap_skips_nonexistent_time() {
        // 2026 spring forward: Mar 8 at 2:00 AM PST → 3:00 AM PDT
        // A task scheduled for 2:30 AM local time falls in the gap.
        // The cron library should advance to the next valid occurrence.
        let tz = chrono_tz::America::Los_Angeles;

        // Start just before the gap: Mar 8 at 1:00 AM PST = 09:00 UTC
        let before_gap = Utc.with_ymd_and_hms(2026, 3, 8, 9, 0, 0).unwrap();
        let next = compute_next_run_after_tz("0 30 2 * * * *", before_gap, &tz);
        let next = next.unwrap();

        // 2:30 AM doesn't exist on Mar 8 — should skip to Mar 9
        // Mar 9 2:30 AM PDT = 09:30 UTC (UTC-7)
        assert_eq!(next.month(), 3);
        assert_eq!(next.day(), 9);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn dst_fall_back_overlap_fires_first_occurrence() {
        // 2026 fall back: Nov 1 at 2:00 AM PDT → 1:00 AM PST
        // A task scheduled for 1:30 AM local time occurs twice.
        // The cron library should fire on the first occurrence (PDT).
        let tz = chrono_tz::America::Los_Angeles;

        // Start at midnight Nov 1 PDT = 07:00 UTC
        let before_overlap = Utc.with_ymd_and_hms(2026, 11, 1, 7, 0, 0).unwrap();
        let next = compute_next_run_after_tz("0 30 1 * * * *", before_overlap, &tz);
        let next = next.unwrap();

        // First 1:30 AM is during PDT: 1:30 AM PDT = 08:30 UTC (UTC-7)
        assert_eq!(next.month(), 11);
        assert_eq!(next.day(), 1);
        assert_eq!(next.hour(), 8);
        assert_eq!(next.minute(), 30);
    }
}
