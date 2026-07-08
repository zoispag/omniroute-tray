use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use chrono::{Duration, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse omniroute output: {0}")]
    Parse(String),
    #[error("omniroute command failed: {0}")]
    Command(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaRow {
    pub provider: String,
    #[serde(default)]
    pub limit: Option<f64>,
    #[serde(default)]
    pub used: Option<f64>,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default, rename = "resetAt")]
    pub reset_at: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

pub fn parse_quota(raw: &str) -> Result<Vec<QuotaRow>, DataError> {
    let json = extract_json(raw).ok_or_else(|| DataError::Parse("no json found".into()))?;
    let rows: Vec<QuotaRow> =
        serde_json::from_str(json).map_err(|e| DataError::Parse(e.to_string()))?;
    Ok(dedupe_quota(rows))
}

fn extract_json(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'[' || c == b'{' {
            let candidate = &raw[i..];
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Some(candidate);
            }
        }
        i += 1;
    }
    None
}

fn dedupe_quota(rows: Vec<QuotaRow>) -> Vec<QuotaRow> {
    let mut seen: BTreeMap<String, QuotaRow> = BTreeMap::new();
    for row in rows {
        seen.entry(row.provider.clone()).or_insert(row);
    }
    seen.into_values().collect()
}

#[derive(Clone)]
pub struct DataClient {
    node_bin: PathBuf,
    omniroute_entry: PathBuf,
}

impl DataClient {
    pub fn new(node_bin: PathBuf, omniroute_entry: PathBuf) -> Self {
        Self {
            node_bin,
            omniroute_entry,
        }
    }

    fn run_json(&self, args: &[&str]) -> Result<String, DataError> {
        let output = Command::new(&self.node_bin)
            .arg(&self.omniroute_entry)
            .args(args)
            .arg("--output")
            .arg("json")
            .output()?;
        if !output.status.success() {
            return Err(DataError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn quota(&self) -> Result<Vec<QuotaRow>, DataError> {
        let raw = self.run_json(&["usage", "quota"])?;
        parse_quota(&raw)
    }

    pub fn cost_by_model(&self, period: &str) -> CostResult {
        self.run_cost(&["cost", "--period", period, "--group-by", "model"])
    }

    pub fn cost_by_range(&self, since: &str, until: Option<&str>) -> CostResult {
        let mut args: Vec<&str> = vec!["cost", "--since", since];
        if let Some(until) = until {
            args.push("--until");
            args.push(until);
        }
        args.push("--group-by");
        args.push("model");
        self.run_cost(&args)
    }

    /// Resolves a UI-facing range identifier ("1d"/"7d"/"30d"/"today"/"yesterday")
    /// into the appropriate `omniroute cost` invocation. Unknown values fall back
    /// to the historical 30d default.
    pub fn cost_report(&self, range: &str) -> CostResult {
        match resolve_cost_query(range, Local::now().date_naive()) {
            CostQuery::Period(period) => self.cost_by_model(period),
            CostQuery::Range { since, until } => self.cost_by_range(&since, until.as_deref()),
        }
    }

    fn run_cost(&self, args: &[&str]) -> CostResult {
        match self.run_json(args) {
            Ok(raw) => match parse_cost(&raw) {
                Ok(rows) => CostResult::available(rows),
                Err(_) => CostResult::unavailable(),
            },
            Err(DataError::Command(msg)) if is_auth_error(&msg) => CostResult::needs_api_key(),
            Err(_) => CostResult::unavailable(),
        }
    }
}

/// Intermediate representation of how to query `omniroute cost` for a given
/// UI range selection, kept separate from `DataClient` so it can be unit
/// tested without a "today" that changes every day.
#[derive(Debug, Clone, PartialEq)]
enum CostQuery {
    Period(&'static str),
    Range {
        since: String,
        until: Option<String>,
    },
}

fn resolve_cost_query(range: &str, today: NaiveDate) -> CostQuery {
    match range {
        "1d" => CostQuery::Period("1d"),
        "7d" => CostQuery::Period("7d"),
        "90d" => CostQuery::Period("90d"),
        "ytd" => CostQuery::Period("ytd"),
        "all" => CostQuery::Period("all"),
        "today" => CostQuery::Range {
            since: format_date(today),
            until: None,
        },
        "yesterday" => {
            let yesterday = today - Duration::days(1);
            CostQuery::Range {
                since: format_date(yesterday),
                until: Some(format_date(today)),
            }
        }
        // "30d" and any unrecognized value fall back to the original default.
        _ => CostQuery::Period("30d"),
    }
}

fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CostStatus {
    Available,
    NeedsApiKey,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostResult {
    pub status: CostStatus,
    #[serde(default)]
    pub rows: Vec<CostRow>,
}

impl CostResult {
    fn available(rows: Vec<CostRow>) -> Self {
        Self {
            status: CostStatus::Available,
            rows,
        }
    }
    fn needs_api_key() -> Self {
        Self {
            status: CostStatus::NeedsApiKey,
            rows: Vec::new(),
        }
    }
    pub fn unavailable() -> Self {
        Self {
            status: CostStatus::Unavailable,
            rows: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostRow {
    #[serde(default, alias = "group", alias = "name")]
    pub model: String,
    #[serde(default, alias = "costUsd", alias = "totalCost", alias = "amount")]
    pub cost: Option<f64>,
    #[serde(default, alias = "requests")]
    pub requests: Option<f64>,
    #[serde(default, alias = "tokensIn")]
    pub tokens_in: Option<f64>,
    #[serde(default, alias = "tokensOut")]
    pub tokens_out: Option<f64>,
}

fn is_auth_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("authentication") || lower.contains("api_key") || lower.contains("api key")
}

pub fn parse_cost(raw: &str) -> Result<Vec<CostRow>, DataError> {
    let slice = extract_json(raw).ok_or_else(|| DataError::Parse("no json".into()))?;
    if let Ok(rows) = serde_json::from_str::<Vec<CostRow>>(slice) {
        return Ok(rows);
    }
    let wrapper: serde_json::Value =
        serde_json::from_str(slice).map_err(|e| DataError::Parse(e.to_string()))?;
    if let Some(arr) = wrapper.get("rows").and_then(|v| v.as_array()) {
        let rows: Vec<CostRow> = serde_json::from_value(serde_json::Value::Array(arr.clone()))
            .map_err(|e| DataError::Parse(e.to_string()))?;
        return Ok(rows);
    }
    Err(DataError::Parse("unrecognized cost shape".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"provider":"claude","limit":null,"used":null,"remaining":100,"resetAt":null,"state":"available"},
      {"provider":"codex","limit":null,"used":null,"remaining":100,"resetAt":null,"state":"available"},
      {"provider":"antigravity","limit":null,"used":null,"remaining":100,"resetAt":null,"state":"available"},
      {"provider":"codex","limit":null,"used":null,"remaining":100,"resetAt":null,"state":"available"},
      {"provider":"antigravity","limit":null,"used":null,"remaining":100,"resetAt":null,"state":"available"}
    ]"#;

    #[test]
    fn parses_and_dedupes_by_provider() {
        let rows = parse_quota(SAMPLE).unwrap();
        assert_eq!(rows.len(), 3);
        let providers: Vec<&str> = rows.iter().map(|r| r.provider.as_str()).collect();
        assert_eq!(providers, vec!["antigravity", "claude", "codex"]);
    }

    #[test]
    fn skips_leading_log_noise_before_json() {
        let noisy = format!("  Loaded env from /x/.env\n{SAMPLE}");
        let rows = parse_quota(&noisy).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn skips_ansi_escape_codes_containing_brackets() {
        let with_ansi = format!("  \x1b[2m📋 Loaded env from /x/.env\x1b[0m\n{SAMPLE}");
        let rows = parse_quota(&with_ansi).unwrap();
        assert_eq!(
            rows.len(),
            3,
            "must not match the '[' inside the ANSI escape"
        );
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let minimal = r#"[{"provider":"claude"}]"#;
        let rows = parse_quota(minimal).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remaining, None);
        assert_eq!(rows[0].state, None);
    }

    #[test]
    fn tolerates_unknown_extra_fields() {
        let extra = r#"[{"provider":"claude","remaining":50,"newFutureField":"x"}]"#;
        let rows = parse_quota(extra).unwrap();
        assert_eq!(rows[0].remaining, Some(50.0));
    }

    #[test]
    fn errors_on_non_json() {
        let result = parse_quota("Authentication required.");
        assert!(matches!(result, Err(DataError::Parse(_))));
    }

    #[test]
    fn parses_real_cost_shape() {
        let raw = r#"[{"group":"claude-opus-4-8","requests":1038,"tokensIn":314705975,"tokensOut":1160814,"costUsd":292.59,"costPct":73.66}]"#;
        let rows = parse_cost(raw).unwrap();
        assert_eq!(rows[0].model, "claude-opus-4-8");
        assert_eq!(rows[0].cost, Some(292.59));
        assert_eq!(rows[0].requests, Some(1038.0));
        assert_eq!(rows[0].tokens_in, Some(314705975.0));
        assert_eq!(rows[0].tokens_out, Some(1160814.0));
    }

    #[test]
    fn parses_cost_wrapped_rows_shape() {
        let raw = r#"{"rows":[{"group":"claude-opus-4-8","costUsd":10.0,"tokensIn":1000,"tokensOut":50}]}"#;
        let rows = parse_cost(raw).unwrap();
        assert_eq!(rows[0].model, "claude-opus-4-8");
        assert_eq!(rows[0].cost, Some(10.0));
    }

    #[test]
    fn detects_auth_errors() {
        assert!(is_auth_error(
            "Authentication required. Set OMNIROUTE_API_KEY"
        ));
        assert!(is_auth_error("missing api key"));
        assert!(!is_auth_error("connection refused"));
    }

    fn fixed_today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
    }

    #[test]
    fn resolves_period_ranges_passthrough() {
        let today = fixed_today();
        assert_eq!(resolve_cost_query("1d", today), CostQuery::Period("1d"));
        assert_eq!(resolve_cost_query("7d", today), CostQuery::Period("7d"));
        assert_eq!(resolve_cost_query("30d", today), CostQuery::Period("30d"));
    }

    #[test]
    fn resolves_today_to_since_only() {
        let today = fixed_today();
        assert_eq!(
            resolve_cost_query("today", today),
            CostQuery::Range {
                since: "2026-03-15".to_string(),
                until: None,
            }
        );
    }

    #[test]
    fn resolves_yesterday_to_since_until_pair() {
        let today = fixed_today();
        assert_eq!(
            resolve_cost_query("yesterday", today),
            CostQuery::Range {
                since: "2026-03-14".to_string(),
                until: Some("2026-03-15".to_string()),
            }
        );
    }

    #[test]
    fn resolves_yesterday_across_month_boundary() {
        let today = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        assert_eq!(
            resolve_cost_query("yesterday", today),
            CostQuery::Range {
                since: "2026-02-28".to_string(),
                until: Some("2026-03-01".to_string()),
            }
        );
    }

    #[test]
    fn unknown_range_falls_back_to_thirty_days() {
        let today = fixed_today();
        assert_eq!(resolve_cost_query("", today), CostQuery::Period("30d"));
        assert_eq!(resolve_cost_query("bogus", today), CostQuery::Period("30d"));
    }
}
