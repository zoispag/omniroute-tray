//! Verified upstream contract (OmniRoute v3.8.45, on 127.0.0.1:20128):
//! - `GET /api/monitoring/health` (NO auth): `providerSummary.{activeCount,configuredCount}`, `circuitBreakers.{open,halfOpen}`, `providerHealth.<type>.state`.
//! - `GET /api/providers` (Bearer): flat array of per-*account* connections `{provider, isActive}`; aggregated here by provider type.
//! - `GET /api/telemetry/summary` (Bearer): top-level aggregate `p95`, `count`, `errorRate`.
//! - `GET /api/cache/stats` (Bearer): `hitRate` (0..1), `hits`, `misses`.
//!   (Docs claimed nested `semanticCache.hitRate`; the real shape is flat — confirmed by live probe.)

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderType {
    pub name: String,
    /// True when at least one account of this provider type is toggled on.
    pub active: bool,
    /// True when this provider type has an OPEN/HALF_OPEN circuit breaker.
    pub breaker_open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HealthStatus {
    pub active_providers: u32,
    pub configured_providers: u32,
    /// Circuit breakers currently OPEN or HALF_OPEN (i.e. tripped/recovering).
    pub breakers_open: u32,
    /// Per-type breakdown for the hover panel, sorted active-first then by name.
    pub providers: Vec<ProviderType>,
    pub p95_ms: f64,
    pub error_rate: f64,
    pub cache_hit_rate: f64,
    /// True when the cache has seen at least one lookup (hits + misses > 0).
    pub cache_active: bool,
    /// True when telemetry has at least one sampled request.
    pub latency_sampled: bool,
}

pub fn fetch(base_url: &str, api_key: Option<&str>) -> HealthStatus {
    let mut status = HealthStatus::default();

    // Health endpoint needs no auth; always attempt it.
    let mut breaker_states: BTreeMap<String, bool> = BTreeMap::new();
    if let Some(body) = get(&format!("{base_url}/api/monitoring/health"), None) {
        breaker_states = apply_health(&mut status, &body);
    }

    // Telemetry, cache, and the provider list require a Bearer key. Skip cleanly if unavailable.
    if let Some(key) = api_key {
        if let Some(body) = get(&format!("{base_url}/api/providers"), Some(key)) {
            apply_providers(&mut status, &body, &breaker_states);
        }
        if let Some(body) = get(&format!("{base_url}/api/telemetry/summary"), Some(key)) {
            apply_telemetry(&mut status, &body);
        }
        if let Some(body) = get(&format!("{base_url}/api/cache/stats"), Some(key)) {
            apply_cache(&mut status, &body);
        }
    }

    status
}

fn get(url: &str, api_key: Option<&str>) -> Option<Value> {
    let mut req = ureq::get(url).timeout(TIMEOUT);
    if let Some(key) = api_key {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let body = req.call().ok()?.into_string().ok()?;
    serde_json::from_str(&body).ok()
}

fn apply_health(status: &mut HealthStatus, v: &Value) -> BTreeMap<String, bool> {
    if let Some(summary) = v.get("providerSummary") {
        status.active_providers = u32_at(summary, "activeCount");
        status.configured_providers = u32_at(summary, "configuredCount");
    }
    if let Some(breakers) = v.get("circuitBreakers") {
        status.breakers_open = u32_at(breakers, "open") + u32_at(breakers, "halfOpen");
    }

    let mut states = BTreeMap::new();
    if let Some(map) = v.get("providerHealth").and_then(Value::as_object) {
        for (name, entry) in map {
            let open = matches!(
                entry.get("state").and_then(Value::as_str),
                Some("OPEN") | Some("HALF_OPEN")
            );
            states.insert(name.clone(), open);
        }
    }
    states
}

fn apply_providers(status: &mut HealthStatus, v: &Value, breaker_states: &BTreeMap<String, bool>) {
    let Some(arr) = provider_array(v) else {
        return;
    };

    // Aggregate per-account connections into distinct provider types; a type is
    // "active" if any of its accounts is toggled on (isActive=true).
    let mut types: BTreeMap<String, bool> = BTreeMap::new();
    for conn in arr {
        let Some(name) = conn.get("provider").and_then(Value::as_str) else {
            continue;
        };
        let active = conn
            .get("isActive")
            .or_else(|| conn.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let entry = types.entry(name.to_string()).or_insert(false);
        *entry = *entry || active;
    }

    if types.is_empty() {
        return;
    }

    let mut providers: Vec<ProviderType> = types
        .into_iter()
        .map(|(name, active)| {
            let breaker_open = breaker_states.get(&name).copied().unwrap_or(false);
            ProviderType {
                name,
                active,
                breaker_open,
            }
        })
        .collect();

    // Active first, then alphabetical — the sort order the hover panel relies on.
    providers.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.name.cmp(&b.name)));

    status.configured_providers = providers.len() as u32;
    status.active_providers = providers.iter().filter(|p| p.active).count() as u32;
    status.providers = providers;
}

fn provider_array(v: &Value) -> Option<&Vec<Value>> {
    v.as_array()
        .or_else(|| v.get("connections").and_then(Value::as_array))
        .or_else(|| v.get("providers").and_then(Value::as_array))
}

fn apply_telemetry(status: &mut HealthStatus, v: &Value) {
    let count = v.get("count").and_then(Value::as_u64).unwrap_or(0);
    status.latency_sampled = count > 0;
    if status.latency_sampled {
        status.p95_ms = v.get("p95").and_then(Value::as_f64).unwrap_or(0.0);
    }
    status.error_rate = v.get("errorRate").and_then(Value::as_f64).unwrap_or(0.0);
}

fn apply_cache(status: &mut HealthStatus, v: &Value) {
    let hits = v.get("hits").and_then(Value::as_f64).unwrap_or(0.0);
    let misses = v.get("misses").and_then(Value::as_f64).unwrap_or(0.0);
    status.cache_active = (hits + misses) > 0.0;
    if status.cache_active {
        status.cache_hit_rate = v.get("hitRate").and_then(Value::as_f64).unwrap_or(0.0);
    }
}

fn u32_at(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured verbatim from the running instance (v3.8.45), trimmed to relevant fields.
    const HEALTH: &str = r#"{
      "status": "healthy",
      "circuitBreakers": {"open": 0, "halfOpen": 0, "degraded": 0, "closed": 1, "total": 1},
      "providerHealth": {"claude": {"state": "CLOSED"}},
      "providerSummary": {"catalogCount": 255, "configuredCount": 5, "activeCount": 3, "monitoredCount": 1}
    }"#;

    const TELEMETRY: &str = r#"{
      "count": 1, "avg": 6176, "p50": 6176, "p95": 6176, "p99": 6176,
      "totalRequests": 1, "avgLatencyMs": 6176, "errorRate": 0
    }"#;

    const CACHE_EMPTY: &str =
        r#"{"size": 0, "maxSize": 50, "hits": 0, "misses": 0, "evictions": 0, "hitRate": 0}"#;

    const CACHE_WARM: &str =
        r#"{"size": 12, "maxSize": 50, "hits": 34, "misses": 66, "evictions": 0, "hitRate": 0.34}"#;

    // Synthetic fixture mirroring the /api/providers shape: multiple accounts per
    // provider type, so 6 rows collapse to 4 types of which 2 have an active account.
    // "codex" is active only because one of its two accounts is on.
    const PROVIDERS: &str = r#"[
      {"provider":"antigravity","isActive":true},
      {"provider":"codex","isActive":false},
      {"provider":"codex","isActive":true},
      {"provider":"kiro","isActive":false},
      {"provider":"claude","isActive":false},
      {"provider":"antigravity","isActive":true}
    ]"#;

    fn parse(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn health_extracts_provider_counts_and_breakers() {
        let mut s = HealthStatus::default();
        let _ = apply_health(&mut s, &parse(HEALTH));
        assert_eq!(s.active_providers, 3);
        assert_eq!(s.configured_providers, 5);
        assert_eq!(s.breakers_open, 0);
    }

    #[test]
    fn breakers_open_counts_open_and_half_open() {
        let mut s = HealthStatus::default();
        let _ = apply_health(
            &mut s,
            &parse(r#"{"circuitBreakers":{"open":2,"halfOpen":1,"closed":3}}"#),
        );
        assert_eq!(s.breakers_open, 3);
    }

    #[test]
    fn telemetry_marks_sampled_and_reads_p95() {
        let mut s = HealthStatus::default();
        apply_telemetry(&mut s, &parse(TELEMETRY));
        assert!(s.latency_sampled);
        assert_eq!(s.p95_ms, 6176.0);
        assert_eq!(s.error_rate, 0.0);
    }

    #[test]
    fn telemetry_zero_count_is_unsampled() {
        let mut s = HealthStatus::default();
        apply_telemetry(&mut s, &parse(r#"{"count":0,"p95":0,"errorRate":0}"#));
        assert!(!s.latency_sampled);
        assert_eq!(s.p95_ms, 0.0);
    }

    #[test]
    fn empty_cache_is_inactive() {
        let mut s = HealthStatus::default();
        apply_cache(&mut s, &parse(CACHE_EMPTY));
        assert!(!s.cache_active);
        assert_eq!(s.cache_hit_rate, 0.0);
    }

    #[test]
    fn warm_cache_reports_hit_rate() {
        let mut s = HealthStatus::default();
        apply_cache(&mut s, &parse(CACHE_WARM));
        assert!(s.cache_active);
        assert_eq!(s.cache_hit_rate, 0.34);
    }

    #[test]
    fn missing_fields_default_to_zero() {
        let mut s = HealthStatus::default();
        let _ = apply_health(&mut s, &parse("{}"));
        apply_telemetry(&mut s, &parse("{}"));
        apply_cache(&mut s, &parse("{}"));
        assert_eq!(s, HealthStatus::default());
    }

    #[test]
    fn providers_aggregate_accounts_into_types() {
        let mut s = HealthStatus::default();
        apply_providers(&mut s, &parse(PROVIDERS), &BTreeMap::new());
        assert_eq!(s.configured_providers, 4);
        assert_eq!(s.active_providers, 2);
        assert_eq!(s.providers.len(), 4);
    }

    #[test]
    fn providers_sorted_active_first_then_alpha() {
        let mut s = HealthStatus::default();
        apply_providers(&mut s, &parse(PROVIDERS), &BTreeMap::new());
        let names: Vec<&str> = s.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["antigravity", "codex", "claude", "kiro"]);
        assert!(s.providers[0].active);
        assert!(!s.providers[3].active);
    }

    #[test]
    fn provider_type_active_if_any_account_active() {
        let mut s = HealthStatus::default();
        apply_providers(&mut s, &parse(PROVIDERS), &BTreeMap::new());
        let by = |n: &str| s.providers.iter().find(|p| p.name == n).unwrap();
        assert!(by("codex").active);
        assert!(!by("kiro").active);
    }

    #[test]
    fn breaker_state_flags_matching_type() {
        let mut s = HealthStatus::default();
        let states = BTreeMap::from([("antigravity".to_string(), true)]);
        apply_providers(&mut s, &parse(PROVIDERS), &states);
        let by = |n: &str| s.providers.iter().find(|p| p.name == n).unwrap();
        assert!(by("antigravity").breaker_open);
        assert!(!by("codex").breaker_open);
    }

    #[test]
    fn health_returns_breaker_state_map() {
        let mut s = HealthStatus::default();
        let states = apply_health(
            &mut s,
            &parse(r#"{"providerHealth":{"claude":{"state":"OPEN"},"codex":{"state":"CLOSED"}}}"#),
        );
        assert_eq!(states.get("claude"), Some(&true));
        assert_eq!(states.get("codex"), Some(&false));
    }
}
