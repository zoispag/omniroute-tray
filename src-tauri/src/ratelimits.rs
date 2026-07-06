use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RateLimitError {
    #[error("network error: {0}")]
    Network(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    pub label: String,
    pub short: String,
    pub used_percent: f64,
    pub reset_at: Option<String>,
    pub unlimited: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountLimits {
    pub account: String,
    pub provider: String,
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, PartialEq)]
struct Connection {
    id: String,
    provider: String,
    name: String,
}

pub fn fetch(base_url: &str, api_key: &str) -> Result<Vec<AccountLimits>, RateLimitError> {
    let providers_raw = get(base_url, "/api/providers", api_key)?;
    let connections = parse_connections(&providers_raw)?;

    let mut result = Vec::new();
    for conn in connections {
        let usage_raw = match get(base_url, &format!("/api/usage/{}", conn.id), api_key) {
            Ok(body) => body,
            Err(_) => continue,
        };
        if let Ok(windows) = parse_usage(&usage_raw) {
            if !windows.is_empty() {
                result.push(AccountLimits {
                    account: conn.name,
                    provider: conn.provider,
                    windows,
                });
            }
        }
    }
    Ok(result)
}

fn get(base_url: &str, path: &str, api_key: &str) -> Result<String, RateLimitError> {
    let url = format!("{base_url}{path}");
    match ureq::get(&url)
        .timeout(std::time::Duration::from_secs(4))
        .set("Authorization", &format!("Bearer {api_key}"))
        .call()
    {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| RateLimitError::Network(e.to_string())),
        Err(ureq::Error::Status(401, _)) => Err(RateLimitError::Unauthorized),
        Err(e) => Err(RateLimitError::Network(e.to_string())),
    }
}

fn parse_connections(raw: &str) -> Result<Vec<Connection>, RateLimitError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| RateLimitError::Parse(e.to_string()))?;
    let arr = value
        .get("connections")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut connections = Vec::new();
    for c in &arr {
        let active = c
            .get("isActive")
            .or_else(|| c.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !active {
            continue;
        }
        let (Some(id), Some(provider)) = (
            c.get("id").and_then(Value::as_str),
            c.get("provider").and_then(Value::as_str),
        ) else {
            continue;
        };
        let name = c
            .get("name")
            .or_else(|| c.get("email"))
            .and_then(Value::as_str)
            .unwrap_or(provider)
            .to_string();
        connections.push(Connection {
            id: id.to_string(),
            provider: provider.to_string(),
            name,
        });
    }
    Ok(connections)
}

pub fn parse_usage(raw: &str) -> Result<Vec<Window>, RateLimitError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| RateLimitError::Parse(e.to_string()))?;
    let quotas = match value.get("quotas").and_then(Value::as_object) {
        Some(q) => q,
        None => return Ok(Vec::new()),
    };

    let mut windows = Vec::new();
    for (key, q) in quotas {
        if !is_time_window(key) {
            continue;
        }
        let unlimited = q.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
        windows.push(Window {
            label: pretty_label(key),
            short: short_label(key),
            used_percent: used_percent_of(q),
            reset_at: q.get("resetAt").and_then(Value::as_str).map(str::to_string),
            unlimited,
        });
    }

    if windows.is_empty() {
        windows = aggregate_per_model(quotas);
    }

    windows.sort_by_key(|w| window_order(&w.label));
    Ok(windows)
}

fn used_percent_of(q: &Value) -> f64 {
    if let Some(rem) = q.get("remainingPercentage").and_then(Value::as_f64) {
        return clamp(100.0 - rem);
    }
    if let (Some(used), Some(total)) = (
        q.get("used").and_then(Value::as_f64),
        q.get("total").and_then(Value::as_f64),
    ) {
        if total > 0.0 {
            return clamp(used / total * 100.0);
        }
    }
    0.0
}

fn short_label(key: &str) -> String {
    let k = key.to_lowercase();
    if let Some(start) = k.find('(') {
        if let Some(end) = k[start..].find(')') {
            let inner = k[start + 1..start + end].trim();
            if !inner.is_empty() {
                return inner.to_string();
            }
        }
    }
    if k.contains("monthly") {
        return "mo".to_string();
    }
    if k.contains("weekly") {
        return "wk".to_string();
    }
    if k.contains("5h") {
        return "5h".to_string();
    }
    if k.starts_with("session") {
        return "sess".to_string();
    }
    k.chars().take(4).collect()
}

fn pretty_label(key: &str) -> String {
    let base = key.split(" (").next().unwrap_or(key);
    let mut chars = base.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => key.to_string(),
    }
}

fn aggregate_per_model(quotas: &serde_json::Map<String, Value>) -> Vec<Window> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, (f64, bool)> = BTreeMap::new();
    for q in quotas.values() {
        let Some(reset) = q.get("resetAt").and_then(Value::as_str) else {
            continue;
        };
        let used = used_percent_of(q);
        let unlimited = q.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
        let entry = groups.entry(reset.to_string()).or_insert((0.0, true));
        if used > entry.0 {
            entry.0 = used;
        }
        entry.1 = entry.1 && unlimited;
    }

    let mut windows: Vec<Window> = groups
        .into_iter()
        .map(|(reset, (used, unlimited))| Window {
            label: reset_bucket_label(&reset),
            short: reset_bucket_short(&reset),
            used_percent: used,
            reset_at: Some(reset),
            unlimited,
        })
        .collect();
    windows.sort_by(|a, b| a.reset_at.cmp(&b.reset_at));
    windows
}

fn reset_bucket_label(_reset: &str) -> String {
    "Quota".to_string()
}

fn reset_bucket_short(reset: &str) -> String {
    let mins = minutes_until(reset);
    match mins {
        Some(m) if m >= 20 * 1440 => "mo".to_string(),
        Some(m) if m >= 4 * 1440 => "wk".to_string(),
        Some(m) if m >= 20 * 60 => "1d".to_string(),
        Some(m) if m >= 3 * 60 => "5h".to_string(),
        Some(_) => "1h".to_string(),
        None => "win".to_string(),
    }
}

fn minutes_until(reset: &str) -> Option<i64> {
    let ts = chrono_parse_millis(reset)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((ts - now) / 60_000)
}

fn chrono_parse_millis(iso: &str) -> Option<i64> {
    let date = &iso.get(0..10)?;
    let time = iso.get(11..19).unwrap_or("00:00:00");
    let (y, m, d) = (
        date.get(0..4)?.parse::<i64>().ok()?,
        date.get(5..7)?.parse::<i64>().ok()?,
        date.get(8..10)?.parse::<i64>().ok()?,
    );
    let (hh, mm, ss) = (
        time.get(0..2)?.parse::<i64>().ok()?,
        time.get(3..5)?.parse::<i64>().ok()?,
        time.get(6..8)?.parse::<i64>().ok()?,
    );
    let days = days_from_civil(y, m, d);
    Some(((days * 86400) + hh * 3600 + mm * 60 + ss) * 1000)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn is_time_window(key: &str) -> bool {
    let k = key.to_lowercase();
    let time_like = k.starts_with("session")
        || k.starts_with("weekly")
        || k.starts_with("window_")
        || k == "monthly"
        || k.contains("(5h)")
        || k.contains("(7d)");
    let per_model = k.contains("gemini")
        || k.contains("gpt")
        || k.contains("claude-")
        || k.contains("sonnet")
        || k.contains("opus")
        || k.contains("haiku");
    time_like && !per_model
}

fn window_order(label: &str) -> u8 {
    let l = label.to_lowercase();
    if l.starts_with("session") {
        0
    } else if l == "weekly" {
        1
    } else {
        2
    }
}

fn clamp(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE: &str = r#"{
      "plan":"default_raven",
      "quotas":{
        "session (5h)":{"used":27,"total":100,"remaining":73,"resetAt":"2026-07-05T16:40:00Z","remainingPercentage":73,"unlimited":false},
        "weekly (7d)":{"used":10,"total":100,"remaining":90,"resetAt":"2026-07-08T07:00:00Z","remainingPercentage":90,"unlimited":false}
      }
    }"#;

    #[test]
    fn parses_real_usage_shape() {
        let w = parse_usage(USAGE).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "Session");
        assert_eq!(w[0].used_percent, 27.0);
        assert_eq!(w[0].reset_at.as_deref(), Some("2026-07-05T16:40:00Z"));
        assert_eq!(w[1].label, "Weekly");
        assert_eq!(w[1].used_percent, 10.0);
    }

    #[test]
    fn session_sorts_before_weekly() {
        let reversed = r#"{"quotas":{"weekly (7d)":{"remainingPercentage":90},"session (5h)":{"remainingPercentage":73}}}"#;
        let w = parse_usage(reversed).unwrap();
        assert_eq!(w[0].label, "Session");
        assert_eq!(w[1].label, "Weekly");
    }

    #[test]
    fn derives_used_from_used_total_when_no_percentage() {
        let raw = r#"{"quotas":{"session (5h)":{"used":40,"total":200}}}"#;
        let w = parse_usage(raw).unwrap();
        assert_eq!(w[0].used_percent, 20.0);
    }

    #[test]
    fn marks_unlimited() {
        let raw = r#"{"quotas":{"weekly (7d)":{"unlimited":true,"remainingPercentage":100}}}"#;
        let w = parse_usage(raw).unwrap();
        assert!(w[0].unlimited);
    }

    #[test]
    fn no_quotas_yields_empty() {
        assert!(parse_usage(r#"{"plan":"x"}"#).unwrap().is_empty());
    }

    #[test]
    fn aggregates_per_model_windows_by_reset_when_no_time_window() {
        let raw = r#"{"quotas":{
          "gemini-2.5-flash":{"used":0,"total":1000,"resetAt":"2026-07-13T08:50:26.000Z","remainingPercentage":100},
          "gemini-3.5-flash-high":{"used":800,"total":1000,"resetAt":"2026-07-13T08:50:26.000Z","remainingPercentage":20},
          "gemini-3.1-flash-lite":{"used":0,"total":1000,"resetAt":"2026-07-07T08:50:26.000Z","remainingPercentage":100}
        }}"#;
        let w = parse_usage(raw).unwrap();
        assert_eq!(
            w.len(),
            2,
            "two distinct reset windows -> two aggregated rows"
        );
        let by_reset: std::collections::HashMap<_, _> = w
            .iter()
            .map(|x| (x.reset_at.clone().unwrap(), x.used_percent))
            .collect();
        assert_eq!(
            by_reset["2026-07-13T08:50:26.000Z"], 80.0,
            "most-used model wins"
        );
        assert_eq!(by_reset["2026-07-07T08:50:26.000Z"], 0.0);
    }

    #[test]
    fn time_window_present_skips_aggregation() {
        let w = parse_usage(USAGE).unwrap();
        assert!(w
            .iter()
            .all(|x| x.label == "Session" || x.label == "Weekly"));
    }

    #[test]
    fn filters_out_per_model_windows() {
        let raw = r#"{"quotas":{
          "session (5h)":{"remainingPercentage":73},
          "gemini-2.5-flash":{"remainingPercentage":100},
          "claude-sonnet-4-6":{"remainingPercentage":100},
          "weekly (7d)":{"remainingPercentage":90}
        }}"#;
        let w = parse_usage(raw).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "Session");
        assert_eq!(w[1].label, "Weekly");
    }

    #[test]
    fn short_label_reflects_real_window() {
        assert_eq!(short_label("session (5h)"), "5h");
        assert_eq!(short_label("weekly (7d)"), "7d");
        assert_eq!(short_label("session"), "sess");
        assert_eq!(short_label("window_monthly"), "mo");
        assert_eq!(short_label("window_weekly"), "wk");
    }

    #[test]
    fn parses_short_from_usage() {
        let w = parse_usage(USAGE).unwrap();
        assert_eq!(w[0].short, "5h");
        assert_eq!(w[1].short, "7d");
    }

    #[test]
    fn only_enabled_connections_selected() {
        let raw = r#"{"connections":[
          {"id":"a","provider":"claude","name":"me","isActive":true},
          {"id":"b","provider":"codex","name":"other","isActive":false}
        ]}"#;
        let conns = parse_connections(raw).unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].id, "a");
    }
}
