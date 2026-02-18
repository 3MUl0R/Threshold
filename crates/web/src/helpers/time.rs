//! Relative time formatting for display in templates.

use chrono::{DateTime, Utc};

/// Format a datetime as a human-readable relative time string.
///
/// Examples: "just now", "5 minutes ago", "2 hours ago", "3 days ago", "2026-01-15"
pub fn relative_time(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(*dt);

    let seconds = duration.num_seconds();
    if seconds < 0 {
        return "just now".to_string();
    }

    if seconds < 60 {
        return "just now".to_string();
    }

    let minutes = duration.num_minutes();
    if minutes < 60 {
        return if minutes == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{minutes} minutes ago")
        };
    }

    let hours = duration.num_hours();
    if hours < 24 {
        return if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        };
    }

    let days = duration.num_days();
    if days < 30 {
        return if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        };
    }

    // Older than 30 days: show date
    dt.format("%Y-%m-%d").to_string()
}

/// Format a datetime as a human-readable duration string (for uptime).
///
/// Examples: "5m", "2h 30m", "3d 5h"
pub fn format_duration_short(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(*dt);
    let total_seconds = duration.num_seconds().max(0);

    let days = total_seconds / 86400;
    let hours = (total_seconds % 86400) / 3600;
    let minutes = (total_seconds % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn just_now_recent() {
        let dt = Utc::now() - Duration::seconds(30);
        assert_eq!(relative_time(&dt), "just now");
    }

    #[test]
    fn just_now_zero() {
        let dt = Utc::now();
        assert_eq!(relative_time(&dt), "just now");
    }

    #[test]
    fn minutes_ago_singular() {
        let dt = Utc::now() - Duration::minutes(1);
        assert_eq!(relative_time(&dt), "1 minute ago");
    }

    #[test]
    fn minutes_ago_plural() {
        let dt = Utc::now() - Duration::minutes(45);
        assert_eq!(relative_time(&dt), "45 minutes ago");
    }

    #[test]
    fn hours_ago_singular() {
        let dt = Utc::now() - Duration::hours(1);
        assert_eq!(relative_time(&dt), "1 hour ago");
    }

    #[test]
    fn hours_ago_plural() {
        let dt = Utc::now() - Duration::hours(5);
        assert_eq!(relative_time(&dt), "5 hours ago");
    }

    #[test]
    fn days_ago_singular() {
        let dt = Utc::now() - Duration::days(1);
        assert_eq!(relative_time(&dt), "1 day ago");
    }

    #[test]
    fn days_ago_plural() {
        let dt = Utc::now() - Duration::days(15);
        assert_eq!(relative_time(&dt), "15 days ago");
    }

    #[test]
    fn older_than_30_days_shows_date() {
        let dt = Utc::now() - Duration::days(60);
        let result = relative_time(&dt);
        // Should be in YYYY-MM-DD format
        assert!(result.len() == 10 && result.contains('-'));
    }

    #[test]
    fn future_datetime_shows_just_now() {
        let dt = Utc::now() + Duration::hours(1);
        assert_eq!(relative_time(&dt), "just now");
    }

    #[test]
    fn duration_short_minutes() {
        let dt = Utc::now() - Duration::minutes(5);
        assert_eq!(format_duration_short(&dt), "5m");
    }

    #[test]
    fn duration_short_hours() {
        let dt = Utc::now() - Duration::hours(2) - Duration::minutes(30);
        assert_eq!(format_duration_short(&dt), "2h 30m");
    }

    #[test]
    fn duration_short_days() {
        let dt = Utc::now() - Duration::days(3) - Duration::hours(5);
        assert_eq!(format_duration_short(&dt), "3d 5h");
    }
}
