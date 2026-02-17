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
//! **All times are UTC.**

use chrono::{DateTime, Utc};
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
}
