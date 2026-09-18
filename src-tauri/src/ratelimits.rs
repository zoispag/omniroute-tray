use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::omniauth::Credentials;

/// Age at which a cached usage entry is worth a live refresh. OmniRoute's own
/// scheduler only resyncs every 70 minutes, so without this the bars would be
/// as stale as the dashboard's; with it, an account is at most about a minute
/// behind — while a poll every 5s no longer hits the upstream provider each time.
pub const REFRESH_AFTER: Duration = Duration::from_secs(60);
/// Minimum gap between two live probes of one account, whatever the outcome.
/// A failing upstream is retried once a minute, not every poll (#61).
const PROBE_GAP: Duration = Duration::from_secs(60);
/// OmniRoute answered "Usage not available for this connection" (HTTP 400): the
/// provider has no usage API. That does not change from one poll to the next.
const UNSUPPORTED_RECHECK: Duration = Duration::from_secs(60 * 60);
/// Live probes per fetch. Spreads a cold start over a few polls instead of firing
/// every account's upstream request in one burst.
const MAX_PROBES_PER_FETCH: usize = 3;
/// Budget for a live probe: OmniRoute may mint a token and call the provider.
const LIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for reads that only touch OmniRoute's own database.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Error)]
pub enum RateLimitError {
    #[error("network error: {0}")]
    Network(String),
    /// No API key resolved yet — normal for the seconds between a fresh install's
    /// first server start and its first minted key. Not an empty account list.
    #[error("waiting for OmniRoute credentials")]
    NoCredentials,
    #[error("OmniRoute rejected the tray's credentials (HTTP {0})")]
    Unauthorized(u16),
    #[error("parse error: {0}")]
    Parse(String),
    /// OmniRoute has no usage API for this provider (HTTP 400 on
    /// `/api/usage/<id>`). An answer, not a failure: the account has no usage.
    #[error("OmniRoute reports no usage for this connection")]
    Unsupported,
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
    /// OmniRoute's connection id — the only stable identity an account has. Two
    /// connections of one provider are both usually named "main", so anything
    /// that has to match an account across fetches matches on this.
    pub id: String,
    pub account: String,
    pub provider: String,
    pub windows: Vec<Window>,
    /// `isActive` on the OmniRoute connection. An inactive account reports no
    /// usage, but it still exists — Settings lists it so its hidden state stays
    /// togglable (#57).
    pub active: bool,
    /// The usage lookup for this account failed (network, auth, bad JSON). Kept
    /// apart from an empty `windows`, so a broken account is not mislabelled as
    /// an idle one.
    pub usage_unavailable: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Connection {
    id: String,
    provider: String,
    name: String,
    active: bool,
}

/// What the tray remembers about its own live probes, per connection id. Lives in
/// `AppState` and outlives a single `fetch`, so a failed upstream lookup is not
/// retried on the very next poll, its outcome keeps labelling the account until
/// a later probe succeeds, and the numbers a probe returned stay available to the
/// polls in between (an older OmniRoute without the cache endpoint has no other
/// source for them). Locked only to plan and to record probes, never across I/O.
#[derive(Debug, Default)]
pub struct ProbeLog {
    entries: HashMap<String, Probe>,
}

#[derive(Debug, Clone, PartialEq)]
struct Probe {
    /// When the last probe completed (or, for a never-completed entry, when it
    /// was first reserved).
    at: Instant,
    /// Outcome of the last COMPLETED probe. A reservation does not touch it, so
    /// the label an account carries survives an in-flight retry.
    outcome: Option<ProbeOutcome>,
    /// Set while a fetch is probing this account off the lock.
    reserved_at: Option<Instant>,
    /// Windows from the last probe that returned numbers, kept across failures
    /// and dropped once the provider is confirmed to have no usage API.
    windows: Option<Vec<Window>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ProbeOutcome {
    /// `/api/usage/<id>` returned fresh numbers.
    Fresh,
    /// OmniRoute could not reach the provider and served its own previous entry
    /// (`_stale: true`). Numbers exist, but they are not the latest.
    Stale,
    /// Network error, timeout, or a non-2xx answer other than the 400 below.
    Failed,
    /// HTTP 400 "Usage not available for this connection": no usage API.
    Unsupported,
}

impl ProbeLog {
    fn may_probe(&self, id: &str, now: Instant) -> bool {
        match self.entries.get(id) {
            None => true,
            Some(p) => {
                let gap = match p.outcome {
                    Some(ProbeOutcome::Unsupported) => UNSUPPORTED_RECHECK,
                    _ => PROBE_GAP,
                };
                // A reservation counts like an attempt: an overlapping fetch waits
                // for it, and an abandoned one expires after the same gap.
                let last = p.reserved_at.map_or(p.at, |r| r.max(p.at));
                now.duration_since(last) >= gap
            }
        }
    }

    /// Claim `ids` for a probe that is about to run without the lock, so an
    /// overlapping fetch does not probe the same accounts. The last completed
    /// outcome and its numbers are untouched — they keep labelling the account
    /// until the retry actually finishes.
    fn reserve(&mut self, ids: &[String], now: Instant) {
        for id in ids {
            let entry = self.entries.entry(id.clone()).or_insert(Probe {
                at: now,
                outcome: None,
                reserved_at: None,
                windows: None,
            });
            entry.reserved_at = Some(now);
        }
    }

    /// `windows` is `Some` only for a probe that returned numbers; a failure keeps
    /// the last ones, an unsupported answer drops them (they are obsolete).
    fn record(
        &mut self,
        id: &str,
        outcome: ProbeOutcome,
        windows: Option<Vec<Window>>,
        now: Instant,
    ) {
        let entry = self.entries.entry(id.to_string()).or_insert(Probe {
            at: now,
            outcome: None,
            reserved_at: None,
            windows: None,
        });
        entry.at = now;
        entry.outcome = Some(outcome);
        entry.reserved_at = None;
        match outcome {
            ProbeOutcome::Unsupported => entry.windows = None,
            _ => {
                if windows.is_some() {
                    entry.windows = windows;
                }
            }
        }
    }

    /// Drop an `Unsupported` verdict that OmniRoute's cache has since disproved
    /// (it wrote a usable entry after the probe). The account goes back to the
    /// normal probe gap and is no longer rendered as having no usage API.
    fn retract_unsupported(&mut self, id: &str) {
        if let Some(p) = self.entries.get_mut(id) {
            if p.outcome == Some(ProbeOutcome::Unsupported) {
                p.outcome = None;
            }
        }
    }

    /// The last completed probe did not produce fresh numbers.
    fn unavailable(&self, id: &str) -> bool {
        matches!(
            self.entries.get(id).and_then(|p| p.outcome),
            Some(ProbeOutcome::Failed | ProbeOutcome::Stale)
        )
    }

    fn unsupported(&self, id: &str) -> bool {
        matches!(
            self.entries.get(id).and_then(|p| p.outcome),
            Some(ProbeOutcome::Unsupported)
        )
    }

    /// When this account was last probed (completed or reserved); `None` if never.
    fn last_attempt(&self, id: &str) -> Option<Instant> {
        self.entries
            .get(id)
            .map(|p| p.reserved_at.map_or(p.at, |r| r.max(p.at)))
    }

    /// Numbers from the last probe that returned any.
    fn last_windows(&self, id: &str) -> Option<&[Window]> {
        self.entries.get(id).and_then(|p| p.windows.as_deref())
    }

    /// Drop memory of connections OmniRoute no longer reports.
    fn retain(&mut self, ids: &[String]) {
        self.entries.retain(|id, _| ids.contains(id));
    }
}

/// One account's cached usage as OmniRoute keeps it (`/api/usage/provider-limits`):
/// the windows we could read from its `quotas`, and how old the entry is.
#[derive(Debug)]
struct CachedUsage {
    windows: Vec<Window>,
    age_ms: Option<i64>,
}

pub fn fetch(
    base_url: &str,
    creds: &Credentials,
    probes: &Mutex<ProbeLog>,
) -> Result<Vec<AccountLimits>, RateLimitError> {
    let providers_raw = get(base_url, "/api/providers", creds, LOCAL_TIMEOUT)?;
    let connections = parse_connections(&providers_raw)?;

    // The dashboard's source: OmniRoute's own per-connection usage cache, one read
    // for every account and no upstream traffic. `/api/usage/<id>` is the other
    // path — a LIVE call to the provider on every request. Polling that every 5s
    // for every account is what made an account flap to "usage unavailable"
    // (timeouts, upstream rate limits) while the dashboard, reading the cache,
    // showed it fine (#61). A missing map (an older OmniRoute, or a failed read)
    // is not fatal: every account then goes through the throttled live path
    // below, and the ones it has no numbers for are flagged so the caller keeps
    // their last known bars.
    let cached = match get(base_url, "/api/usage/provider-limits", creds, LOCAL_TIMEOUT)
        .and_then(|raw| parse_provider_limits(&raw))
    {
        Ok(map) => Some(map),
        Err(e @ RateLimitError::Unauthorized(_)) => return Err(e),
        Err(e) => {
            log::debug!("provider-limits cache unavailable, probing live: {e}");
            None
        }
    };

    let now = Instant::now();
    let now_ms = unix_millis();

    let mut usage: HashMap<String, CachedUsage> = HashMap::new();
    for conn in &connections {
        if let Some(entry) = cached
            .as_ref()
            .and_then(|c| c.get(&conn.id))
            .filter(|e| !is_error_only_entry(e))
        {
            // An entry we cannot read is treated like no entry: the live probe
            // decides, and failing that the account is flagged, not blanked.
            if let Ok(windows) = windows_from_body(entry) {
                usage.insert(
                    conn.id.clone(),
                    CachedUsage {
                        windows,
                        age_ms: entry_age_ms(entry, now_ms),
                    },
                );
            }
        }
    }

    let candidates: Vec<(String, Option<i64>)> = connections
        .iter()
        .filter(|c| c.active)
        .map(|c| (c.id.clone(), usage.get(&c.id).and_then(|u| u.age_ms)))
        .collect();

    // Plan under the lock, probe without it: a probe may take seconds, and the
    // 5s poll must not queue behind the background refresh (or vice versa).
    let ids: Vec<String> = connections.iter().map(|c| c.id.clone()).collect();
    let due = {
        let mut log = probes.lock().unwrap();
        log.retain(&ids);
        // An `Unsupported` verdict is disproved by a usable entry OmniRoute wrote
        // AFTER the probe that produced it: the usage API works now. Retract it
        // before planning, or the account would sit out the hour-long recheck
        // gap (and be rendered as having no usage) despite live data in the cache.
        for (id, age) in &candidates {
            if log.unsupported(id)
                && entry_postdates_verdict(
                    *age,
                    log.last_attempt(id).map(|t| now.duration_since(t)),
                )
            {
                log.retract_unsupported(id);
            }
        }
        let due = plan_probes(&candidates, &log, now);
        log.reserve(&due, now);
        due
    };

    // Each probe carries its own completion time: they run one after the other,
    // and the gap before an account's next attempt counts from when ITS probe
    // ended, not from the start or end of the batch.
    let live: HashMap<String, (ProbeOutcome, Option<Vec<Window>>, Instant)> = due
        .into_iter()
        .map(|id| {
            let (outcome, windows) = match probe_live(base_url, &id, creds) {
                Ok(l) if l.stale => (ProbeOutcome::Stale, Some(l.windows)),
                Ok(l) => (ProbeOutcome::Fresh, Some(l.windows)),
                Err(RateLimitError::Unsupported) => (ProbeOutcome::Unsupported, None),
                Err(e) => {
                    log::debug!("live usage probe failed for {id}: {e}");
                    (ProbeOutcome::Failed, None)
                }
            };
            (id, (outcome, windows, Instant::now()))
        })
        .collect();

    let log = {
        let mut log = probes.lock().unwrap();
        for (id, (outcome, windows, done)) in &live {
            log.record(id, *outcome, windows.clone(), *done);
        }
        log
    };

    let mut result = Vec::new();
    for conn in connections {
        let mut windows = usage.get(&conn.id).map(|u| u.windows.clone());
        let mut fresh = usage
            .get(&conn.id)
            .and_then(|u| u.age_ms)
            .is_some_and(|age| age >= 0 && (age as u128) < REFRESH_AFTER.as_millis());

        match live.get(&conn.id) {
            Some((ProbeOutcome::Fresh, Some(w), _)) => {
                windows = Some(w.clone());
                fresh = true;
            }
            Some((ProbeOutcome::Stale, Some(w), _)) => {
                windows = Some(w.clone());
                fresh = false;
            }
            _ => {}
        }
        // No cache entry (older OmniRoute, or a failed read): the last numbers a
        // probe returned stand in, so an account does not blank between probes.
        if windows.is_none() {
            windows = log.last_windows(&conn.id).map(<[Window]>::to_vec);
        }
        // Confirmed without a usage API: whatever the cache still holds is
        // obsolete data to drop. (A verdict disproved by a newer entry was
        // retracted above, before planning.)
        let unsupported = log.unsupported(&conn.id);
        if unsupported {
            windows = Some(Vec::new());
        }

        // Flagged when the tray's own last look failed AND nothing fresher has
        // landed in OmniRoute's cache since (the dashboard or the scheduler may
        // have refreshed it) — a fresh entry is good data whoever fetched it —
        // or when no source has numbers for this account yet (no usable cache
        // entry, no probe so far), so the caller carries over its last known
        // windows instead of reading the gap as "no usage".
        let no_numbers = windows.is_none();
        let usage_unavailable =
            conn.active && !fresh && (log.unavailable(&conn.id) || (no_numbers && !unsupported));
        result.push(AccountLimits {
            id: conn.id,
            account: conn.name,
            provider: conn.provider,
            windows: if conn.active {
                windows.unwrap_or_default()
            } else {
                Vec::new()
            },
            active: conn.active,
            usage_unavailable,
        });
    }
    Ok(result)
}

/// Which active accounts get a live `/api/usage/<id>` this round: those with no
/// cache entry or one older than `REFRESH_AFTER`, that have not been probed
/// within `PROBE_GAP` (or, for unsupported ones, `UNSUPPORTED_RECHECK`), at most
/// `MAX_PROBES_PER_FETCH`. `age_ms` is `None` for an account OmniRoute holds no
/// entry for — those go first, never-probed ones ahead of the least recently
/// probed, so that with no cache at all (an older OmniRoute) every account gets
/// its turn instead of the same three winning each round. Stale entries follow,
/// oldest first.
fn plan_probes(
    candidates: &[(String, Option<i64>)],
    probes: &ProbeLog,
    now: Instant,
) -> Vec<String> {
    // Sort key, ascending: (0 = missing entry, 1 = stale entry; then how long
    // since the last probe for missing ones — never probed = longest — or how
    // stale the entry is for the rest, inverted so oldest sorts first; then id).
    let mut due: Vec<((u8, i64, i64), &str)> = candidates
        .iter()
        .filter(|(id, _)| probes.may_probe(id, now))
        .filter_map(|(id, age)| match age {
            None => {
                let since_probe = probes
                    .last_attempt(id)
                    .map_or(i64::MAX, |t| now.duration_since(t).as_millis() as i64);
                Some(((0, -since_probe, 0), id.as_str()))
            }
            Some(age) if *age < 0 || (*age as u128) >= REFRESH_AFTER.as_millis() => {
                Some(((1, 0, -*age), id.as_str()))
            }
            Some(_) => None,
        })
        .collect();
    due.sort();
    due.truncate(MAX_PROBES_PER_FETCH);
    due.into_iter().map(|(_, id)| id.to_string()).collect()
}

struct LiveUsage {
    windows: Vec<Window>,
    /// OmniRoute marks a body `_stale` when the provider did not answer and it
    /// fell back to its previous entry.
    stale: bool,
}

fn probe_live(base_url: &str, id: &str, creds: &Credentials) -> Result<LiveUsage, RateLimitError> {
    let raw = get(base_url, &format!("/api/usage/{id}"), creds, LIVE_TIMEOUT)?;
    parse_live(&raw)
}

/// Keep the last good windows for an account whose usage lookup just failed, so a
/// per-account error does not blank that provider's bars. Same intent as the
/// whole-list fallback in `get_rate_limits`: show the last known values rather
/// than nothing. The `usage_unavailable` flag stays set, so the UI can still say
/// the numbers are not fresh.
pub fn carry_over_windows(previous: &[AccountLimits], fresh: &mut [AccountLimits]) {
    for acc in fresh.iter_mut() {
        if !acc.usage_unavailable || !acc.windows.is_empty() {
            continue;
        }
        if let Some(prev) = previous.iter().find(|p| p.id == acc.id) {
            acc.windows.clone_from(&prev.windows);
        }
    }
}

fn get(
    base_url: &str,
    path: &str,
    creds: &Credentials,
    timeout: Duration,
) -> Result<String, RateLimitError> {
    let url = format!("{base_url}{path}");
    let req = creds.apply(ureq::get(&url).timeout(timeout));
    match req.call() {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| RateLimitError::Network(e.to_string())),
        // 401 = no usable credential; 403 = Bearer present but not a management
        // token (an inference-only key with login enabled). Both are auth, not network.
        Err(ureq::Error::Status(code @ (401 | 403), _)) => Err(RateLimitError::Unauthorized(code)),
        // `/api/usage/<id>` answers 400 "Usage not available for this connection"
        // for a provider without a usage API. Not a failure to retry — but only
        // that answer; any other 400 is a failure like the rest.
        Err(ureq::Error::Status(400, resp)) => {
            Err(error_for_400(&resp.into_string().unwrap_or_default()))
        }
        Err(e) => Err(RateLimitError::Network(e.to_string())),
    }
}

fn error_for_400(body: &str) -> RateLimitError {
    if body.contains("Usage not available") {
        RateLimitError::Unsupported
    } else {
        let short: String = body.chars().take(200).collect();
        RateLimitError::Network(format!("HTTP 400: {short}"))
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `/api/usage/provider-limits` → `caches: { <connectionId>: { quotas, fetchedAt,
/// message, ... } }`. Each entry has the same `quotas` shape as `/api/usage/<id>`.
fn parse_provider_limits(
    raw: &str,
) -> Result<HashMap<String, serde_json::Map<String, Value>>, RateLimitError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| RateLimitError::Parse(e.to_string()))?;
    let Some(caches) = value.get("caches").and_then(Value::as_object) else {
        return Err(RateLimitError::Parse(
            "provider-limits held no `caches` object".to_string(),
        ));
    };
    Ok(caches
        .iter()
        .filter_map(|(id, entry)| entry.as_object().map(|e| (id.clone(), e.clone())))
        .collect())
}

/// OmniRoute persists an error-only entry (`quotas: null` plus a `message`) when a
/// refresh failed and there was no earlier good entry to keep. That is a failed
/// lookup, not an account without usage: treat it as no entry, so it gets probed
/// and, failing that, flagged. A `quotas: null` with no message is a real answer.
fn is_error_only_entry(entry: &serde_json::Map<String, Value>) -> bool {
    let has_quotas = entry.get("quotas").is_some_and(Value::is_object);
    let has_message = entry
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|m| !m.trim().is_empty());
    !has_quotas && has_message
}

/// Whether a cache entry `age_ms` old was written after a verdict reached
/// `since_verdict` ago. `None` age (no usable entry, or no readable `fetchedAt`)
/// never postdates anything; `None` verdict means there is nothing to supersede.
fn entry_postdates_verdict(age_ms: Option<i64>, since_verdict: Option<Duration>) -> bool {
    match (age_ms, since_verdict) {
        (Some(age), Some(since)) => age >= 0 && (age as u128) < since.as_millis(),
        _ => false,
    }
}

/// Milliseconds since the entry's `fetchedAt`; `None` when it carries none we can read.
fn entry_age_ms(entry: &serde_json::Map<String, Value>, now_ms: i64) -> Option<i64> {
    let fetched = entry.get("fetchedAt").and_then(Value::as_str)?;
    Some(now_ms - chrono_parse_millis(fetched)?)
}

fn parse_connections(raw: &str) -> Result<Vec<Connection>, RateLimitError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| RateLimitError::Parse(e.to_string()))?;
    // OmniRoute has served this both as a flat array and wrapped in `connections`
    // (`health.rs` tolerates `providers` as well), and reading the wrong one here
    // empties the account list Settings depends on. Anything else is a broken
    // response, not "no accounts": erroring keeps the last-known list, since an
    // empty success would overwrite the cache and hide every account.
    let arr = match value
        .as_array()
        .or_else(|| value.get("connections").and_then(Value::as_array))
        .or_else(|| value.get("providers").and_then(Value::as_array))
    {
        Some(arr) => arr.clone(),
        None => {
            return Err(RateLimitError::Parse(
                "/api/providers held no connections array".to_string(),
            ))
        }
    };

    let mut connections = Vec::new();
    for c in &arr {
        let active = c
            .get("isActive")
            .or_else(|| c.get("enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
            active,
        });
    }
    // Same rule one level down: an empty answer is only believable when the server
    // actually sent an empty array. Entries we could not read at all mean a shape
    // we do not understand, and an empty success would wipe the cached accounts.
    if connections.is_empty() && !arr.is_empty() {
        return Err(RateLimitError::Parse(
            "no /api/providers entry carried an id and a provider".to_string(),
        ));
    }
    Ok(connections)
}

/// A live `/api/usage/<id>` answer: its windows, and whether OmniRoute marked the
/// body `_stale` (the provider did not answer and it served its previous entry).
fn parse_live(raw: &str) -> Result<LiveUsage, RateLimitError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| RateLimitError::Parse(e.to_string()))?;
    // A usage body is an object; `[]`, `null` or a string would otherwise slip
    // through `get("quotas")` as None and be read as "this account has no quotas".
    let Some(body) = value.as_object() else {
        return Err(RateLimitError::Parse(
            "usage response was not an object".to_string(),
        ));
    };
    // A 200 with `quotas: null` and a `message` is how OmniRoute reports a failed
    // provider lookup when it has no earlier entry to fall back on. Same rule as
    // for cached entries: that is a failed probe, not an account with no usage.
    if is_error_only_entry(body) {
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider lookup failed");
        return Err(RateLimitError::Network(format!("OmniRoute: {message}")));
    }
    Ok(LiveUsage {
        windows: windows_from_body(body)?,
        stale: body.get("_stale").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[cfg(test)]
fn parse_usage(raw: &str) -> Result<Vec<Window>, RateLimitError> {
    parse_live(raw).map(|live| live.windows)
}

/// The windows in a usage body — a live `/api/usage/<id>` answer or one entry of
/// the `/api/usage/provider-limits` cache; both carry the same `quotas` object.
fn windows_from_body(body: &serde_json::Map<String, Value>) -> Result<Vec<Window>, RateLimitError> {
    // Absent (or null) quotas is an answer: this account has none. Any other type
    // is a broken response, and must not be reported as "no usage" (#57).
    let quotas = match body.get("quotas") {
        Some(Value::Object(q)) => q,
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(_) => {
            return Err(RateLimitError::Parse(
                "`quotas` is not an object".to_string(),
            ))
        }
    };

    let mut windows = Vec::new();
    for (key, q) in quotas {
        if !is_time_window(key) {
            continue;
        }
        // A window we cannot read is not a window at 0% — that would paint a full
        // bar over the account's last known numbers.
        if !q.is_object() {
            return Err(RateLimitError::Parse(format!(
                "quota `{key}` is not an object"
            )));
        }
        let unlimited = q.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
        // An entry carrying no usable numbers is not a window at 0% either — that
        // paints a reassuring full bar. An unlimited window has nothing to count.
        let used_percent = match used_percent_of(q) {
            Some(pct) => pct,
            None if unlimited => 0.0,
            None => {
                return Err(RateLimitError::Parse(format!(
                    "quota `{key}` carried no usable numbers"
                )))
            }
        };
        windows.push(Window {
            label: pretty_label(key),
            short: short_label(key),
            used_percent,
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

/// `None` when the entry holds neither a usable `remainingPercentage` nor a
/// `used`/`total` pair — the caller decides whether that is a broken window or
/// simply one with nothing to count.
fn used_percent_of(q: &Value) -> Option<f64> {
    if let Some(rem) = q.get("remainingPercentage").and_then(Value::as_f64) {
        return Some(clamp(100.0 - rem));
    }
    if let (Some(used), Some(total)) = (
        q.get("used").and_then(Value::as_f64),
        q.get("total").and_then(Value::as_f64),
    ) {
        if total > 0.0 {
            return Some(clamp(used / total * 100.0));
        }
    }
    None
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
        let Some(used) = used_percent_of(q) else {
            continue;
        };
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

/// Unix milliseconds for an ISO-8601 timestamp. RFC 3339 input (what OmniRoute
/// writes, e.g. `2026-09-18T15:34:45.854Z`) keeps its fractional seconds and
/// offset — `entry_postdates_verdict` orders a cache entry against a probe that
/// may have completed in the same second, so whole seconds are not enough. The
/// hand parser below stays as a fallback for date-only or offset-less shapes.
fn chrono_parse_millis(iso: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso) {
        return Some(dt.timestamp_millis());
    }
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
        assert!(parse_usage(r#"{"quotas":null}"#).unwrap().is_empty());
    }

    #[test]
    fn malformed_quotas_is_an_error_not_an_idle_account() {
        assert!(parse_usage(r#"{"quotas":[]}"#).is_err());
        assert!(parse_usage(r#"{"quotas":"none"}"#).is_err());
    }

    #[test]
    fn usage_body_must_be_an_object() {
        for raw in ["[]", "null", r#""nope""#, "7"] {
            assert!(parse_usage(raw).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn window_without_usable_numbers_is_an_error_unless_it_is_unlimited() {
        assert!(parse_usage(r#"{"quotas":{"session (5h)":{}}}"#).is_err());
        assert!(
            parse_usage(r#"{"quotas":{"session (5h)":{"remainingPercentage":"73"}}}"#).is_err(),
            "a string percentage is not a number"
        );
        assert!(
            parse_usage(r#"{"quotas":{"session (5h)":{"used":5,"total":0}}}"#).is_err(),
            "a zero total cannot yield a percentage"
        );
        let unlimited = parse_usage(r#"{"quotas":{"weekly (7d)":{"unlimited":true}}}"#).unwrap();
        assert_eq!(unlimited.len(), 1);
        assert_eq!(unlimited[0].used_percent, 0.0);
    }

    #[test]
    fn unreadable_window_is_an_error_not_a_zero_percent_bar() {
        assert!(parse_usage(r#"{"quotas":{"session (5h)":null}}"#).is_err());
        assert!(parse_usage(r#"{"quotas":{"weekly (7d)":"full"}}"#).is_err());
        assert!(
            parse_usage(r#"{"quotas":{"gemini-2.5-flash":null}}"#).is_ok(),
            "per-model keys are filtered out before we look at them"
        );
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
    fn keeps_inactive_connections_and_flags_them() {
        let raw = r#"{"connections":[
          {"id":"a","provider":"claude","name":"me","isActive":true},
          {"id":"b","provider":"codex","name":"other","isActive":false}
        ]}"#;
        let conns = parse_connections(raw).unwrap();
        assert_eq!(conns.len(), 2, "an inactive account still exists (#57)");
        assert!(conns[0].active);
        assert!(!conns[1].active);
    }

    fn account(provider: &str, windows: usize, unavailable: bool) -> AccountLimits {
        AccountLimits {
            id: format!("{provider}-1"),
            account: "main".into(),
            provider: provider.into(),
            windows: (0..windows)
                .map(|_| Window {
                    label: "Session".into(),
                    short: "5h".into(),
                    used_percent: 42.0,
                    reset_at: None,
                    unlimited: false,
                })
                .collect(),
            active: true,
            usage_unavailable: unavailable,
        }
    }

    #[test]
    fn carries_last_known_windows_into_a_failed_lookup() {
        let previous = vec![account("claude", 2, false)];
        let mut fresh = vec![account("claude", 0, true)];
        carry_over_windows(&previous, &mut fresh);
        assert_eq!(fresh[0].windows.len(), 2);
        assert!(fresh[0].usage_unavailable, "still flagged as not fresh");
    }

    #[test]
    fn carry_over_matches_on_id_not_on_a_shared_account_name() {
        let mut previous = account("claude", 2, false);
        previous.id = "conn-a".into();
        let mut fresh = account("claude", 0, true);
        fresh.id = "conn-b".into();
        let mut fresh = vec![fresh];
        carry_over_windows(&[previous], &mut fresh);
        assert!(
            fresh[0].windows.is_empty(),
            "two connections of one provider are both called main; the id decides"
        );
    }

    #[test]
    fn carry_over_leaves_successful_and_unknown_accounts_alone() {
        let previous = vec![account("claude", 2, false)];
        let mut fresh = vec![account("claude", 0, false), account("codex", 0, true)];
        carry_over_windows(&previous, &mut fresh);
        assert!(
            fresh[0].windows.is_empty(),
            "a successful empty answer is the truth, not a gap to fill"
        );
        assert!(fresh[1].windows.is_empty(), "nothing cached for this one");
    }

    #[test]
    fn parses_connections_from_either_shape() {
        let wrapped =
            r#"{"connections":[{"id":"a","provider":"claude","name":"me","isActive":true}]}"#;
        let flat = r#"[{"id":"a","provider":"claude","name":"me","isActive":true}]"#;
        for raw in [wrapped, flat] {
            let conns = parse_connections(raw).unwrap();
            assert_eq!(conns.len(), 1, "shape: {raw}");
            assert_eq!(conns[0].provider, "claude");
        }
    }

    #[test]
    fn unsupported_provider_shape_is_an_error_not_an_empty_account_list() {
        // An empty success would replace the cached accounts and hide them all.
        assert!(parse_connections(r#"{"connections":{}}"#).is_err());
        assert!(parse_connections(r#"{"other":[]}"#).is_err());
        assert!(
            parse_connections(r#"{"connections":[]}"#)
                .unwrap()
                .is_empty(),
            "a genuinely empty list is still an answer"
        );
    }

    #[test]
    fn connection_without_id_or_provider_is_skipped() {
        let raw = r#"{"connections":[
          {"provider":"claude","name":"no id","isActive":true},
          {"id":"b","name":"no provider","isActive":true},
          {"id":"c","provider":"codex","name":"fine","isActive":true}
        ]}"#;
        let conns = parse_connections(raw).unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].id, "c");
    }

    #[test]
    fn entries_we_cannot_read_at_all_are_an_error_not_an_empty_list() {
        let unreadable = r#"{"connections":[{"name":"no id"},{"name":"no provider"}]}"#;
        assert!(parse_connections(unreadable).is_err());
    }

    // Shape of `/api/usage/provider-limits` as served by v3.8.51: one cache entry
    // per connection id, each with the same `quotas` object `/api/usage/<id>` returns.
    const PROVIDER_LIMITS: &str = r#"{
      "caches": {
        "claude-1": {
          "quotas": {
            "session (5h)": {"used":27,"total":100,"remaining":73,"resetAt":"2026-07-05T16:40:00Z","remainingPercentage":73,"unlimited":false},
            "weekly (7d)": {"used":10,"total":100,"remaining":90,"resetAt":"2026-07-08T07:00:00Z","remainingPercentage":90,"unlimited":false}
          },
          "plan": "default_raven", "message": null,
          "fetchedAt": "2026-09-18T15:34:45.854Z", "source": "manual"
        },
        "codex-1": {"quotas": null, "plan": null, "message": "rate limited", "fetchedAt": "2026-09-18T15:00:00.000Z"}
      },
      "intervalMinutes": 70,
      "lastAutoSyncAt": "2026-09-18T14:27:50.043Z"
    }"#;

    #[test]
    fn cache_entry_yields_the_same_windows_as_a_live_answer() {
        let caches = parse_provider_limits(PROVIDER_LIMITS).unwrap();
        let cached = windows_from_body(&caches["claude-1"]).unwrap();
        let live = parse_usage(USAGE).unwrap();
        assert_eq!(cached, live);
        assert!(
            windows_from_body(&caches["codex-1"]).unwrap().is_empty(),
            "an error-only entry is an account with no windows, not a parse error"
        );
    }

    #[test]
    fn provider_limits_without_caches_is_an_error() {
        assert!(parse_provider_limits(r#"{"error":"nope"}"#).is_err());
        assert!(parse_provider_limits(r#"[]"#).is_err());
        assert!(parse_provider_limits(r#"{"caches":{}}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn timestamps_keep_their_milliseconds() {
        let whole = chrono_parse_millis("2026-09-18T15:34:45Z").unwrap();
        assert_eq!(
            chrono_parse_millis("2026-09-18T15:34:45.854Z"),
            Some(whole + 854),
            "the fraction OmniRoute writes must survive, or same-second ordering breaks"
        );
        assert_eq!(
            chrono_parse_millis("2026-09-18T17:34:45.854+02:00"),
            Some(whole + 854),
            "offsets are honoured"
        );
        assert_eq!(whole % 1000, 0);
        assert!(
            chrono_parse_millis("2026-09-18").is_some(),
            "date-only input still parses through the fallback"
        );
    }

    #[test]
    fn entry_age_comes_from_fetched_at() {
        let caches = parse_provider_limits(PROVIDER_LIMITS).unwrap();
        let fetched = chrono_parse_millis("2026-09-18T15:34:45Z").unwrap();
        assert_eq!(
            entry_age_ms(&caches["claude-1"], fetched + 30_000),
            Some(30_000)
        );
        let bare = serde_json::Map::new();
        assert_eq!(entry_age_ms(&bare, fetched), None);
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn probes_go_to_missing_then_stalest_entries_capped_per_fetch() {
        let now = Instant::now();
        let log = ProbeLog::default();
        let refresh = REFRESH_AFTER.as_millis() as i64;
        let candidates = vec![
            ("fresh".to_string(), Some(refresh / 2)),
            ("stale".to_string(), Some(refresh + 1_000)),
            ("staler".to_string(), Some(refresh + 60_000)),
            ("never".to_string(), None),
            ("clock-skew".to_string(), Some(-5_000)),
        ];
        let due = plan_probes(&candidates, &log, now);
        assert_eq!(due.len(), MAX_PROBES_PER_FETCH);
        assert_eq!(due[0], "never", "no entry at all is the most overdue");
        assert_eq!(due[1], "staler");
        assert_eq!(due[2], "stale");
        assert!(!due.contains(&"fresh".to_string()));
    }

    #[test]
    fn with_no_cache_at_all_every_account_gets_its_turn() {
        // An older OmniRoute: no entry for anyone. Round one takes three; after
        // the gap, round two must start with the two that were skipped, not the
        // same three again (the closed-popover refresh is the only caller then).
        let t0 = Instant::now();
        let mut log = ProbeLog::default();
        let candidates: Vec<(String, Option<i64>)> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|id| (id.to_string(), None))
            .collect();
        let round1 = plan_probes(&candidates, &log, t0);
        assert_eq!(round1, vec!["a", "b", "c"]);
        for id in &round1 {
            log.record(id, ProbeOutcome::Failed, None, t0);
        }
        let round2 = plan_probes(&candidates, &log, t0 + PROBE_GAP * 5);
        assert_eq!(round2[..2], ["d", "e"], "never probed go first");
        assert_eq!(round2[2], "a", "then the least recently probed");
        for id in &round2 {
            log.record(id, ProbeOutcome::Failed, None, t0 + PROBE_GAP * 5);
        }
        let round3 = plan_probes(&candidates, &log, t0 + PROBE_GAP * 10);
        assert_eq!(
            round3,
            vec!["b", "c", "a"],
            "b and c are the oldest; a, d and e tie on time and the id decides"
        );
    }

    #[test]
    fn a_failed_probe_is_not_retried_until_the_gap_has_passed() {
        let t0 = Instant::now();
        let mut log = ProbeLog::default();
        let candidates = vec![("a".to_string(), None)];
        assert_eq!(plan_probes(&candidates, &log, t0), vec!["a".to_string()]);
        log.record("a", ProbeOutcome::Failed, None, t0);
        assert!(log.unavailable("a"));
        assert!(
            plan_probes(&candidates, &log, t0 + secs(5)).is_empty(),
            "the next 5s poll waits"
        );
        assert_eq!(
            plan_probes(&candidates, &log, t0 + PROBE_GAP),
            vec!["a".to_string()]
        );
        log.record("a", ProbeOutcome::Fresh, None, t0 + PROBE_GAP);
        assert!(!log.unavailable("a"), "a success clears the flag");
    }

    #[test]
    fn unsupported_connections_are_rechecked_rarely_and_not_flagged() {
        let t0 = Instant::now();
        let mut log = ProbeLog::default();
        log.record("api", ProbeOutcome::Unsupported, None, t0);
        assert!(log.unsupported("api"));
        assert!(
            !log.unavailable("api"),
            "no usage API is an answer, not an outage"
        );
        let candidates = vec![("api".to_string(), None)];
        assert!(plan_probes(&candidates, &log, t0 + PROBE_GAP * 10).is_empty());
        assert_eq!(
            plan_probes(&candidates, &log, t0 + UNSUPPORTED_RECHECK),
            vec!["api".to_string()]
        );
    }

    #[test]
    fn stale_marker_on_a_live_answer_is_read() {
        let raw = USAGE.trim_end().trim_end_matches('}').to_string()
            + r#","_stale":true,"_staleReason":"rate limited"}"#;
        let live = parse_live(&raw).unwrap();
        assert!(live.stale);
        assert_eq!(live.windows.len(), 2, "stale numbers are still numbers");
        assert!(!parse_live(USAGE).unwrap().stale);
    }

    #[test]
    fn an_error_only_live_body_is_a_failed_probe_not_empty_usage() {
        let raw = r#"{"quotas":null,"plan":null,"message":"Claude connected. Usage API requires admin permissions."}"#;
        assert!(
            matches!(parse_live(raw), Err(RateLimitError::Network(m)) if m.contains("admin permissions")),
            "a 200 carrying only an error message must not read as \"no usage\""
        );
        let no_quotas_no_message = r#"{"quotas":null,"plan":null,"message":null}"#;
        let live = parse_live(no_quotas_no_message).unwrap();
        assert!(
            live.windows.is_empty(),
            "no quotas and no message is a real empty answer"
        );
    }

    #[test]
    fn a_stale_server_answer_counts_as_not_fresh() {
        let mut log = ProbeLog::default();
        log.record("a", ProbeOutcome::Stale, None, Instant::now());
        assert!(log.unavailable("a"));
    }

    /// End to end against a running OmniRoute: the cached map answers every poll,
    /// live probes go only to stale/missing entries, and a second fetch right after
    /// the first probes nothing (so it only touches the local database).
    #[test]
    #[ignore = "live test: requires a running OmniRoute on OMNIROUTE_LIVE_PORT and a credential in OMNIROUTE_LIVE_CLI_TOKEN or OMNIROUTE_LIVE_API_KEY"]
    fn live_fetch_reads_the_cache_and_throttles_probes() {
        let port = std::env::var("OMNIROUTE_LIVE_PORT").expect("OMNIROUTE_LIVE_PORT");
        let base = format!("http://127.0.0.1:{port}");
        // Credentials come from the environment, never from `~/.omniroute/.env`
        // (a test must not read that file). The loopback token is
        // `printf omniroute-cli-auth-v1 | openssl dgst -sha256 -hmac <IOPlatformUUID, lower-cased>`.
        let creds = Credentials {
            api_key: std::env::var("OMNIROUTE_LIVE_API_KEY").ok(),
            cli_token: std::env::var("OMNIROUTE_LIVE_CLI_TOKEN").ok(),
        };
        assert!(
            !creds.is_empty(),
            "set OMNIROUTE_LIVE_CLI_TOKEN or OMNIROUTE_LIVE_API_KEY"
        );
        let probes = Mutex::new(ProbeLog::default());

        let t = Instant::now();
        let first = fetch(&base, &creds, &probes).expect("first fetch");
        let first_ms = t.elapsed().as_millis();
        assert!(!first.is_empty(), "the reference instance has accounts");
        for a in &first {
            // Provider + id prefix only: the account name is often an e-mail address.
            eprintln!(
                "{:<8} {:<14} active={} windows={} unavailable={}",
                &a.id[..a.id.len().min(8)],
                a.provider,
                a.active,
                a.windows.len(),
                a.usage_unavailable
            );
        }
        let recorded = probes.lock().unwrap().entries.len();
        eprintln!("first fetch: {first_ms}ms, probes recorded: {recorded}");
        assert!(recorded <= MAX_PROBES_PER_FETCH);

        let t = Instant::now();
        let second = fetch(&base, &creds, &probes).expect("second fetch");
        let second_ms = t.elapsed().as_millis();
        let recorded = probes.lock().unwrap().entries.len();
        eprintln!("second fetch: {second_ms}ms, probes recorded: {recorded}");
        assert_eq!(first.len(), second.len());
        assert!(
            second_ms < 2_000,
            "a follow-up poll must not wait on upstream providers ({second_ms}ms)"
        );
    }

    #[test]
    fn last_live_numbers_survive_a_failed_probe_and_go_with_an_unsupported_one() {
        let now = Instant::now();
        let mut log = ProbeLog::default();
        let windows = account("claude", 2, false).windows;
        log.record("a", ProbeOutcome::Fresh, Some(windows.clone()), now);
        assert_eq!(log.last_windows("a"), Some(windows.as_slice()));
        log.record("a", ProbeOutcome::Failed, None, now + PROBE_GAP);
        assert_eq!(
            log.last_windows("a"),
            Some(windows.as_slice()),
            "a failure keeps the last numbers for the polls in between"
        );
        assert!(log.unavailable("a"));
        log.record("a", ProbeOutcome::Unsupported, None, now + PROBE_GAP * 2);
        assert_eq!(
            log.last_windows("a"),
            None,
            "no usage API: old bars are obsolete"
        );
    }

    #[test]
    fn a_reserved_probe_blocks_an_overlapping_fetch_without_flagging_the_account() {
        let now = Instant::now();
        let mut log = ProbeLog::default();
        let windows = account("claude", 1, false).windows;
        log.record("a", ProbeOutcome::Fresh, Some(windows.clone()), now);
        log.reserve(&["a".to_string(), "b".to_string()], now + PROBE_GAP);
        assert!(!log.may_probe("a", now + PROBE_GAP + secs(5)));
        assert!(!log.may_probe("b", now + PROBE_GAP + secs(5)));
        assert!(!log.unavailable("a"), "in flight is not a failure");
        assert_eq!(
            log.last_windows("a"),
            Some(windows.as_slice()),
            "reserving keeps the numbers"
        );
        assert!(
            log.may_probe("a", now + PROBE_GAP * 2),
            "an abandoned reservation expires"
        );
    }

    #[test]
    fn an_error_only_cache_entry_is_not_an_answer() {
        let caches = parse_provider_limits(PROVIDER_LIMITS).unwrap();
        assert!(!is_error_only_entry(&caches["claude-1"]));
        assert!(
            is_error_only_entry(&caches["codex-1"]),
            "quotas null + message = a failed refresh OmniRoute persisted, not \"no usage\""
        );
        let no_quotas: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"quotas":null,"message":null,"fetchedAt":"2026-09-18T15:00:00Z"}"#,
        )
        .unwrap();
        assert!(
            !is_error_only_entry(&no_quotas),
            "no quotas and no message is a real answer"
        );
    }

    #[test]
    fn a_reservation_does_not_erase_the_last_outcome() {
        let now = Instant::now();
        let mut log = ProbeLog::default();
        log.record("f", ProbeOutcome::Failed, None, now);
        log.record("u", ProbeOutcome::Unsupported, None, now);
        log.reserve(&["f".to_string()], now + PROBE_GAP);
        assert!(
            log.unavailable("f"),
            "still flagged while the retry is in flight"
        );
        assert!(!log.may_probe("f", now + PROBE_GAP + secs(1)));
        log.record(
            "f",
            ProbeOutcome::Fresh,
            Some(Vec::new()),
            now + PROBE_GAP + secs(2),
        );
        assert!(!log.unavailable("f"), "the completed retry clears it");
        assert!(log.unsupported("u"), "untouched accounts keep theirs");
    }

    #[test]
    fn only_a_cache_entry_newer_than_the_unsupported_verdict_supersedes_it() {
        let since = Some(secs(600));
        assert!(
            entry_postdates_verdict(Some(30_000), since),
            "written 30s ago, verdict 10min ago: the usage API works now"
        );
        assert!(
            !entry_postdates_verdict(Some(3_600_000), since),
            "an hour-old entry is the obsolete data the verdict is about"
        );
        assert!(!entry_postdates_verdict(None, since), "no usable entry");
        assert!(
            !entry_postdates_verdict(Some(30_000), None),
            "no verdict: nothing to supersede (and nothing to clear)"
        );
        assert!(
            !entry_postdates_verdict(Some(-5_000), since),
            "clock skew is not freshness"
        );
    }

    #[test]
    fn a_disproved_unsupported_verdict_returns_the_account_to_the_normal_gap() {
        let t0 = Instant::now();
        let mut log = ProbeLog::default();
        log.record("u", ProbeOutcome::Unsupported, None, t0);
        let later = t0 + secs(600);
        // OmniRoute wrote a usable entry 30s ago, well after the verdict.
        let candidates = vec![("u".to_string(), Some(30_000))];
        assert!(
            entry_postdates_verdict(Some(30_000), Some(later.duration_since(t0))),
            "the entry disproves the verdict"
        );
        assert!(
            plan_probes(&candidates, &log, later).is_empty(),
            "still fresh: nothing to probe yet"
        );
        log.retract_unsupported("u");
        assert!(
            !log.unsupported("u"),
            "no longer rendered as having no usage API"
        );
        let stale = vec![("u".to_string(), Some(REFRESH_AFTER.as_millis() as i64 + 1))];
        assert_eq!(
            plan_probes(&stale, &log, later),
            vec!["u".to_string()],
            "once the entry ages past REFRESH_AFTER it is refreshed under the normal gap, not the hour"
        );
        // Retracting anything else is a no-op.
        log.record("f", ProbeOutcome::Failed, None, t0);
        log.retract_unsupported("f");
        assert!(log.unavailable("f"));
    }

    #[test]
    fn only_the_documented_400_means_unsupported() {
        assert!(matches!(
            error_for_400(r#"{"error":"Usage not available for this connection"}"#),
            RateLimitError::Unsupported
        ));
        assert!(matches!(
            error_for_400(r#"{"error":"connectionId is required"}"#),
            RateLimitError::Network(_)
        ));
        assert!(matches!(error_for_400(""), RateLimitError::Network(_)));
    }

    #[test]
    fn probe_log_forgets_connections_no_longer_reported() {
        let mut log = ProbeLog::default();
        let now = Instant::now();
        log.record("gone", ProbeOutcome::Failed, None, now);
        log.record("kept", ProbeOutcome::Failed, None, now);
        log.retain(&["kept".to_string()]);
        assert!(!log.unavailable("gone"));
        assert!(log.unavailable("kept"));
    }
}
