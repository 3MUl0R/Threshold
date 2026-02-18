//! Template engine setup with embedded templates.

use std::sync::Arc;

use crate::helpers::time;

/// Build the minijinja template environment with all templates embedded.
///
/// In debug builds, templates can optionally be loaded from disk for hot-reload.
pub fn build_template_env() -> Arc<minijinja::Environment<'static>> {
    let mut env = minijinja::Environment::new();

    // Auto-escaping is enabled by default for HTML — prevents XSS
    // from untrusted content (audit logs, conversation text, etc.)

    // Register templates from embedded strings
    env.add_template_owned("base.html".to_string(), include_str!("../templates/base.html").to_string())
        .expect("base.html template");
    env.add_template_owned("index.html".to_string(), include_str!("../templates/index.html").to_string())
        .expect("index.html template");
    env.add_template_owned("error.html".to_string(), include_str!("../templates/error.html").to_string())
        .expect("error.html template");

    // Conversation templates
    env.add_template_owned("conversations/list.html".to_string(), include_str!("../templates/conversations/list.html").to_string())
        .expect("conversations/list.html template");
    env.add_template_owned("conversations/detail.html".to_string(), include_str!("../templates/conversations/detail.html").to_string())
        .expect("conversations/detail.html template");
    env.add_template_owned("conversations/audit_partial.html".to_string(), include_str!("../templates/conversations/audit_partial.html").to_string())
        .expect("conversations/audit_partial.html template");

    // Schedule templates
    env.add_template_owned("schedules/list.html".to_string(), include_str!("../templates/schedules/list.html").to_string())
        .expect("schedules/list.html template");

    // Audit templates
    env.add_template_owned("audit/browser.html".to_string(), include_str!("../templates/audit/browser.html").to_string())
        .expect("audit/browser.html template");
    env.add_template_owned("audit/tab_partial.html".to_string(), include_str!("../templates/audit/tab_partial.html").to_string())
        .expect("audit/tab_partial.html template");

    // Log templates
    env.add_template_owned("logs/viewer.html".to_string(), include_str!("../templates/logs/viewer.html").to_string())
        .expect("logs/viewer.html template");
    env.add_template_owned("logs/entries_partial.html".to_string(), include_str!("../templates/logs/entries_partial.html").to_string())
        .expect("logs/entries_partial.html template");

    // Config templates
    env.add_template_owned("config/editor.html".to_string(), include_str!("../templates/config/editor.html").to_string())
        .expect("config/editor.html template");
    env.add_template_owned("config/credentials.html".to_string(), include_str!("../templates/config/credentials.html").to_string())
        .expect("config/credentials.html template");

    // Register custom filters
    env.add_filter("relative_time", relative_time_filter);
    env.add_filter("duration_short", duration_short_filter);
    env.add_filter("truncate", truncate_filter);
    env.add_filter("tojson", tojson_filter);

    Arc::new(env)
}

/// Custom filter: convert ISO timestamp to "5 minutes ago" etc.
fn relative_time_filter(value: &str) -> String {
    match value.parse::<chrono::DateTime<chrono::Utc>>() {
        Ok(dt) => time::relative_time(&dt),
        Err(_) => value.to_string(),
    }
}

/// Custom filter: truncate a string to `length` chars, appending `end` if truncated.
/// Usage: {{ value|truncate(8, true, "") }} or {{ value|truncate(20) }}
fn truncate_filter(value: &str, length: Option<usize>, _killwords: Option<bool>, end: Option<&str>) -> String {
    let max = length.unwrap_or(255);
    let suffix = end.unwrap_or("...");
    if value.len() <= max {
        value.to_string()
    } else {
        let truncated: String = value.chars().take(max).collect();
        format!("{truncated}{suffix}")
    }
}

/// Custom filter: serialize a value to pretty-printed JSON.
/// Usage: {{ value|tojson(indent=2) }} or {{ value|tojson }}
fn tojson_filter(value: minijinja::Value, kwargs: minijinja::value::Kwargs) -> Result<String, minijinja::Error> {
    let _indent: Option<usize> = kwargs.get("indent")?;
    kwargs.assert_all_used()?;
    // Convert to serde_json::Value for pretty printing
    let json_val = serde_json::to_value(&value)
        .unwrap_or(serde_json::Value::Null);
    Ok(serde_json::to_string_pretty(&json_val)
        .unwrap_or_else(|_| value.to_string()))
}

/// Custom filter: convert ISO timestamp to short uptime duration.
fn duration_short_filter(value: &str) -> String {
    match value.parse::<chrono::DateTime<chrono::Utc>>() {
        Ok(dt) => time::format_duration_short(&dt),
        Err(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_env_builds_successfully() {
        let env = build_template_env();
        assert!(env.get_template("base.html").is_ok());
        assert!(env.get_template("index.html").is_ok());
        assert!(env.get_template("error.html").is_ok());
        assert!(env.get_template("conversations/list.html").is_ok());
        assert!(env.get_template("conversations/detail.html").is_ok());
        assert!(env.get_template("conversations/audit_partial.html").is_ok());
    }

    #[test]
    fn auto_escaping_prevents_xss() {
        let env = build_template_env();
        let tmpl = env.get_template("error.html").unwrap();
        let rendered = tmpl
            .render(minijinja::context! {
                status_code => 500,
                message => "<script>alert('xss')</script>",
            })
            .unwrap();
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(!rendered.contains("<script>alert"));
    }
}
