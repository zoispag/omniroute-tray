mod analytics;
mod apikey;
mod data;
mod doctor;
mod engine_gate;
mod github;
mod health;
mod installer;
mod lockfile;
mod logfile;
mod omniauth;
mod paths;
mod provider_icons;
mod ratelimits;
mod registry;
mod runtime;
mod spaces;
mod state;
mod supervisor;
mod traymenu;
mod updater;

use std::sync::Mutex;

use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_positioner::{Position, WindowExt};

use data::{CostResult, DataClient, QuotaRow};
use omniauth::Credentials;
use paths::AppPaths;
use state::ServerState;

const POPOVER_LABEL: &str = "popover";
const SERVER_URL: &str = "http://127.0.0.1:20128";

struct AppState {
    server: Mutex<ServerState>,
    data: Mutex<Option<DataClient>>,
    active_version: Mutex<Option<String>>,
    /// Bearer key + loopback CLI token presented on every management call.
    /// Resolved at bootstrap; re-resolved lazily while the key is still missing
    /// (a fresh install mints its first key only after the server starts).
    auth: Mutex<Credentials>,
    supervisor: Mutex<Option<supervisor::Supervisor>>,
    pin_open: std::sync::atomic::AtomicBool,
    /// Last successful quota fetch. Warmed by `schedule_quota_refresh` and used by
    /// `get_rate_limits` as a fallback when a live fetch fails, so the UI keeps the
    /// last-known values instead of blanking.
    rate_limit_cache: Mutex<Option<Vec<ratelimits::AccountLimits>>>,
    /// Provider marks fetched from the server's `/providers/<id>.svg`, keyed by
    /// provider id and valid for one served OmniRoute version. `None` records that
    /// the server has no mark for it (the popover then shows a lettered badge);
    /// lookups that never reached the server are not cached so they retry once it
    /// is up.
    provider_icons: Mutex<ProviderIconCache>,
}

/// Marks are assets of the OmniRoute build being served, so the cache is tied to
/// `active_version`: a restart or update onto another version drops every entry
/// (including cached misses) and the next paint refetches from the new server.
#[derive(Default)]
struct ProviderIconCache {
    version: Option<String>,
    marks: std::collections::HashMap<String, Option<String>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            server: Mutex::new(ServerState::Stopped),
            data: Mutex::new(None),
            active_version: Mutex::new(None),
            auth: Mutex::new(Credentials::default()),
            supervisor: Mutex::new(None),
            pin_open: std::sync::atomic::AtomicBool::new(false),
            rate_limit_cache: Mutex::new(None),
            provider_icons: Mutex::new(ProviderIconCache::default()),
        }
    }
}

fn set_state(app: &tauri::AppHandle, next: ServerState) {
    let app_state = app.state::<AppState>();
    *app_state.server.lock().unwrap() = next;
}

fn stop_managed_server(app: &tauri::AppHandle) {
    let taken = app.state::<AppState>().supervisor.lock().unwrap().take();
    if let Some(mut sup) = taken {
        let _ = sup.stop();
    }
}

/// Cycle the running server so it executes the version `current` now points at.
///
/// The supervisor is taken out of the mutex for the duration: this blocks for up
/// to ~45s and a Quit arriving meanwhile must not wait on the lock on the UI
/// thread. MUST run off the UI thread. Returns false when the old server refused
/// to release the port or the replacement never became healthy.
fn restart_managed_server(app: &tauri::AppHandle, version: &str) -> bool {
    let taken = app.state::<AppState>().supervisor.lock().unwrap().take();
    let Some(mut sup) = taken else {
        return false;
    };
    sup.set_expected_version(version);
    let restarted = sup.stop_and_wait()
        && sup.spawn().is_ok()
        && sup.wait_ready(std::time::Duration::from_secs(30));
    *app.state::<AppState>().supervisor.lock().unwrap() = Some(sup);
    restarted
}

/// Stop the running server and wait for the port to actually free, then bootstrap
/// a fresh one. MUST run off the UI thread — `stop_and_wait` blocks.
fn restart_flow(app: tauri::AppHandle) {
    set_state(&app, ServerState::Starting);
    let taken = app.state::<AppState>().supervisor.lock().unwrap().take();
    if let Some(mut sup) = taken {
        sup.stop_and_wait();
    }
    bootstrap(app);
}

fn install_with_retry(
    prefix: &runtime::Prefix,
    node: &installer::NodeRuntime,
) -> Result<String, installer::InstallError> {
    let backoffs = [2u64, 8, 20];
    let mut attempt = 0;
    loop {
        match installer::ensure_installed(prefix, node, paths::PINNED_OMNIROUTE) {
            Ok(v) => return Ok(v),
            Err(e) if e.is_transient() && attempt < backoffs.len() => {
                log::warn!("install attempt {} failed: {e}; retrying", attempt + 1);
                std::thread::sleep(std::time::Duration::from_secs(backoffs[attempt]));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn bootstrap(app: tauri::AppHandle) {
    use installer::NodeRuntime;
    use runtime::Prefix;
    use supervisor::Supervisor;

    set_state(&app, ServerState::Starting);

    let paths = match AppPaths::resolve(&app) {
        Ok(p) => p,
        Err(e) => {
            set_state(
                &app,
                ServerState::Error {
                    reason: format!("path resolution failed: {e}"),
                },
            );
            return;
        }
    };

    let node_root = paths
        .node_bin
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());
    let Some(node_root) = node_root else {
        set_state(
            &app,
            ServerState::Error {
                reason: "bundled node not found".into(),
            },
        );
        return;
    };

    let prefix = Prefix::new(&paths.prefix_root);
    let node = NodeRuntime::new(&node_root);

    let version = match install_with_retry(&prefix, &node) {
        Ok(v) => v,
        Err(e) => {
            set_state(
                &app,
                ServerState::Error {
                    reason: e.to_string(),
                },
            );
            return;
        }
    };

    let entry = paths.current_omniroute_entry();
    let _ = node.repair_runtime(&entry);
    {
        let app_state = app.state::<AppState>();
        *app_state.data.lock().unwrap() =
            Some(DataClient::new(paths.node_bin.clone(), entry.clone()));
        *app_state.active_version.lock().unwrap() = Some(version.clone());
        let creds = resolve_credentials(&paths, None);
        if creds.cli_token.is_none() {
            log::warn!("could not derive the OmniRoute CLI token; management calls will rely on the API key alone");
        }
        *app_state.auth.lock().unwrap() = creds;
    }

    let token = format!("omniroute-tray-{}", std::process::id());
    let log = logfile::ServerLog::new(&paths.log_dir);
    let mut supervisor = Supervisor::new(
        paths.node_bin.clone(),
        entry,
        paths.state_dir.clone(),
        token,
    )
    .with_log(log)
    // Lets reconcile() spot a server left over from before an update: it answers
    // on the port but still runs the version `current` pointed at when it started.
    .with_expected_version(&version);

    use supervisor::Reconciliation;
    match supervisor.reconcile() {
        Ok(decision) => {
            let ready = match decision {
                Reconciliation::SpawnFresh | Reconciliation::ReplaceStale => {
                    supervisor.wait_ready(std::time::Duration::from_secs(20))
                }
                Reconciliation::Adopt | Reconciliation::ReconcileForeign => true,
            };
            if ready {
                set_state(
                    &app,
                    ServerState::Running {
                        version: Some(version.clone()),
                    },
                );
                check_for_update(&app, &version);
            } else {
                set_state(
                    &app,
                    ServerState::Error {
                        reason: "server did not start within 20s (see View Logs)".into(),
                    },
                );
            }
        }
        Err(e) => set_state(
            &app,
            ServerState::Error {
                reason: e.to_string(),
            },
        ),
    }

    *app.state::<AppState>().supervisor.lock().unwrap() = Some(supervisor);
}

const HEALTH_FAILURES_BEFORE_ERROR: u32 = 3;

/// Spawned exactly once at app setup — NOT from `bootstrap()`, which re-runs on
/// every restart and would leak a loop each time, each holding the version string
/// captured at its own start and able to re-publish it long after an update.
fn monitor_health(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut consecutive_failures: u32 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let responding = supervisor::server_responding(20128);
            let app_state = app.state::<AppState>();
            let current = app_state.server.lock().unwrap().clone();
            let version = app_state.active_version.lock().unwrap().clone();
            // Misses only count while the server is meant to be up. Starting,
            // Updating and Restarting legitimately leave the port dark, and
            // carrying those misses forward would trip the debounce on the very
            // first busy probe after the server comes back.
            if responding || !current.is_running() {
                consecutive_failures = 0;
            } else {
                consecutive_failures += 1;
            }
            match (&current, responding) {
                // Debounced: a single missed probe is routinely just the server's
                // event loop being busy (e.g. the settings pane fetching per-account
                // usage); only sustained silence means it is actually down.
                (ServerState::Running { .. } | ServerState::UpdateAvailable { .. }, false)
                    if consecutive_failures >= HEALTH_FAILURES_BEFORE_ERROR =>
                {
                    set_state(
                        &app,
                        ServerState::Error {
                            reason: "OmniRoute server is not responding on :20128".into(),
                        },
                    );
                }
                (ServerState::Error { .. } | ServerState::Stopped, true) => {
                    set_state(
                        &app,
                        ServerState::Running {
                            version: version.clone(),
                        },
                    );
                    // bootstrap() only runs check_for_update when wait_ready succeeds
                    // within 20s. A slow cold start lands in Error, recovers here, and
                    // would otherwise never learn about a pending update.
                    if let Some(version) = &version {
                        check_for_update(&app, version);
                    }
                }
                _ => {}
            }
        }
    });
}

fn check_for_update(app: &tauri::AppHandle, current: &str) {
    if let Ok(latest) = registry::latest_version() {
        if updater::is_newer(&latest, current) {
            set_state(
                app,
                ServerState::UpdateAvailable {
                    current: current.to_string(),
                    latest,
                },
            );
        }
    }
}

const UPDATE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Periodically re-check the npm registry for a newer omniroute.
///
/// `check_for_update` otherwise only runs once at the end of `bootstrap()`, so
/// a release published while the app sits in `Running` was never noticed until
/// the next app restart (which is why an adopted instance — fresh launch, fresh
/// bootstrap — showed the update while a long-running one did not).
///
/// Spawned exactly once at app setup, NOT inside `bootstrap()`, which re-runs
/// on every "Restart Server" and would leak a duplicate loop each time.
fn schedule_update_checks(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(UPDATE_CHECK_INTERVAL);
        let app_state = app.state::<AppState>();
        // Only nudge steady states; never clobber Starting/Updating/Error.
        let can_check = matches!(
            *app_state.server.lock().unwrap(),
            ServerState::Running { .. } | ServerState::UpdateAvailable { .. }
        );
        if !can_check {
            continue;
        }
        let current = app_state.active_version.lock().unwrap().clone();
        if let Some(current) = current {
            check_for_update(&app, &current);
        }
    });
}

const QUOTA_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Periodically refresh Claude session/weekly quota in the background so quota
/// stays current even while the popover is closed (the 5s foreground loop only
/// runs when the UI is alive). Mirrors `schedule_update_checks`: spawned once at
/// setup, gated on the states where the server is live (`Running`/`UpdateAvailable`),
/// and pushes results to the frontend via an event.
fn schedule_quota_refresh(app: tauri::AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(QUOTA_REFRESH_INTERVAL);
        let app_state = app.state::<AppState>();
        let server_live = matches!(
            *app_state.server.lock().unwrap(),
            ServerState::Running { .. } | ServerState::UpdateAvailable { .. }
        );
        if !server_live {
            continue;
        }
        let creds = credentials_for_request(&app);
        if creds.is_empty() {
            continue;
        }
        let Ok(mut limits) = ratelimits::fetch(SERVER_URL, &creds) else {
            continue;
        };
        {
            let mut cache = app_state.rate_limit_cache.lock().unwrap();
            if let Some(previous) = cache.as_deref() {
                ratelimits::carry_over_windows(previous, &mut limits);
            }
            *cache = Some(limits.clone());
        }
        if let Some(window) = app.get_webview_window(POPOVER_LABEL) {
            let _ = window.emit("quota-refreshed", limits);
        }
    });
}

/// Everything the tray can present to the local server: the shared API key from
/// `.env`/`storage.sqlite` plus the machine-derived loopback CLI token (#42).
/// The token depends only on the machine, so a `known_token` is reused instead
/// of shelling out to `ioreg` again.
fn resolve_credentials(paths: &AppPaths, known_token: Option<String>) -> Credentials {
    let env_path = paths.omniroute_env_path();
    let db_path = paths.omniroute_db_path();
    Credentials {
        api_key: apikey::resolve(&env_path, &db_path),
        cli_token: known_token.or_else(|| omniauth::resolve_cli_token(&env_path)),
    }
}

/// Credentials for one data request. Blocking (touches disk when re-resolving),
/// so call it from `spawn_blocking`. While no API key is known yet, look again
/// each time: on a fresh install the server creates `storage.sqlite` and its
/// default key *after* bootstrap already resolved, and without this the popover
/// would show the usage skeleton until the next tray restart.
fn credentials_for_request(app: &tauri::AppHandle) -> Credentials {
    let state = app.state::<AppState>();
    let cached = state.auth.lock().unwrap().clone();
    if cached.api_key.is_some() {
        return cached;
    }
    let Ok(paths) = AppPaths::resolve(app) else {
        return cached;
    };
    let fresh = resolve_credentials(&paths, cached.cli_token.clone());
    if fresh.api_key.is_some() {
        log::info!("OmniRoute API key became available; using it from now on");
        *state.auth.lock().unwrap() = fresh.clone();
    }
    fresh
}

/// Build the popover from its config.
///
/// The window is declared `"create": false` and built here instead, because it
/// must not exist before `setup` has made this an accessory app: a window born
/// while the process is still a regular app is pinned to the space it was
/// created on for life (#58, see `spaces`). Also used to recover a window that
/// was destroyed out from under us — we survive that now (see
/// `RunEvent::ExitRequested`), and a tray whose popover never opens again would
/// be worse than the crash it replaced.
fn build_popover(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == POPOVER_LABEL)
        .cloned();
    let Some(config) = config else {
        log::error!("no `{POPOVER_LABEL}` window in the app config");
        return None;
    };
    // The whole UI hangs off this window, so a failure here must say why: it is
    // the one error that leaves the tray with nothing to show.
    match tauri::WebviewWindowBuilder::from_config(app, &config).and_then(|w| w.build()) {
        Ok(window) => {
            spaces::follow_active_space(&window);
            Some(window)
        }
        Err(err) => {
            log::error!("could not build the popover window: {err}");
            None
        }
    }
}

/// The popover, rebuilt if it has gone missing.
fn recreate_popover(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    log::warn!("popover window was gone; rebuilding it");
    build_popover(app)
}

fn toggle_popover(app: &tauri::AppHandle) {
    let window = app
        .get_webview_window(POPOVER_LABEL)
        .or_else(|| recreate_popover(app));
    let Some(window) = window else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        let _ = window.move_window_constrained(Position::TrayCenter);
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Show the tray menu ourselves, for the macOS versions where the status item
/// cannot own it (see `traymenu`). The popover is only borrowed as the menu's
/// owner window — it stays hidden, and the menu opens at the pointer.
fn show_tray_menu(app: &tauri::AppHandle, menu: &tauri::menu::Menu<tauri::Wry>) {
    let window = app
        .get_webview_window(POPOVER_LABEL)
        .or_else(|| recreate_popover(app));
    let Some(window) = window else {
        log::warn!("no window to anchor the tray menu to");
        return;
    };
    traymenu::present(window.as_ref().window(), menu);
}

#[tauri::command]
fn get_status(state: tauri::State<AppState>) -> ServerState {
    state.server.lock().unwrap().clone()
}

#[tauri::command]
fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[tauri::command]
fn get_port() -> u16 {
    20128
}

#[tauri::command]
fn restart_server(app: tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || restart_flow(handle));
}

#[tauri::command]
fn open_logs(app: tauri::AppHandle) -> Result<(), String> {
    let paths = AppPaths::resolve(&app).map_err(|e| e.to_string())?;
    let log = logfile::ServerLog::new(&paths.log_dir);
    let _ = log.ensure_exists();
    tauri_plugin_opener::open_path(log.path().display().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_quota(state: tauri::State<'_, AppState>) -> Result<Vec<QuotaRow>, String> {
    let client = state.data.lock().unwrap().clone();
    let Some(client) = client else {
        return Ok(Vec::new());
    };
    tauri::async_runtime::spawn_blocking(move || client.quota())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_cost(state: tauri::State<'_, AppState>, range: String) -> Result<CostResult, String> {
    let client = state.data.lock().unwrap().clone();
    let Some(client) = client else {
        return Ok(CostResult::unavailable());
    };
    tauri::async_runtime::spawn_blocking(move || client.cost_report(&range))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_rate_limits(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ratelimits::AccountLimits>, String> {
    let fetched = tauri::async_runtime::spawn_blocking(move || {
        let creds = credentials_for_request(&app);
        if creds.is_empty() {
            // An empty list would read as "this install has no accounts"; the
            // popover must show it as unavailable and keep any cached rows.
            return Err(ratelimits::RateLimitError::NoCredentials);
        }
        ratelimits::fetch(SERVER_URL, &creds)
    })
    .await
    .map_err(|e| e.to_string())?;
    match fetched {
        Ok(mut limits) => {
            let mut cache = state.rate_limit_cache.lock().unwrap();
            if let Some(previous) = cache.as_deref() {
                ratelimits::carry_over_windows(previous, &mut limits);
            }
            *cache = Some(limits.clone());
            Ok(limits)
        }
        Err(e) => match state.rate_limit_cache.lock().unwrap().clone() {
            Some(cached) => Ok(cached),
            None => Err(e.to_string()),
        },
    }
}

#[tauri::command]
async fn get_usage_trend(app: tauri::AppHandle) -> Result<Option<analytics::UsageTrend>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let creds = credentials_for_request(&app);
        if creds.is_empty() {
            return Ok(None);
        }
        analytics::fetch(SERVER_URL, &creds, "30d").map(Some)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_health(app: tauri::AppHandle) -> Result<health::HealthStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let creds = credentials_for_request(&app);
        health::fetch(SERVER_URL, &creds)
    })
    .await
    .map_err(|e| e.to_string())
}

/// The provider's brand mark as served by the local OmniRoute dashboard, or `None`
/// when it has none. Errors only when the server could not be reached at all, so
/// the popover can retry later instead of settling on the fallback badge.
#[tauri::command]
async fn get_provider_icon(
    provider: String,
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    let served = state.active_version.lock().unwrap().clone();
    let cached = {
        let mut cache = state.provider_icons.lock().unwrap();
        if cache.version != served {
            // Another OmniRoute version is being served (restart, update, adopted
            // daemon): its assets may differ, so forget everything learnt before.
            cache.marks.clear();
            cache.version = served.clone();
        }
        cache.marks.get(&provider).cloned()
    };
    if let Some(hit) = cached {
        return Ok(hit);
    }
    let id = provider.clone();
    let lookup =
        tauri::async_runtime::spawn_blocking(move || provider_icons::fetch(SERVER_URL, &id))
            .await
            .map_err(|e| e.to_string())?;
    // Re-read the LIVE version, not `cache.version`: the latter only moves when a
    // lookup runs, so it cannot tell us whether the server was replaced while this
    // fetch was in flight. Locked in the same order as above (active_version, then
    // provider_icons) so the two can never deadlock.
    let live = state.active_version.lock().unwrap().clone();
    let mut cache = state.provider_icons.lock().unwrap();
    // If the served version moved while we were fetching, this answer describes the
    // old server; hand it back for this paint but do not remember it.
    let current = live == served && cache.version == served;
    match lookup {
        provider_icons::Lookup::Found(svg) => {
            if current {
                cache.marks.insert(provider, Some(svg.clone()));
            }
            Ok(Some(svg))
        }
        provider_icons::Lookup::Missing => {
            if current {
                cache.marks.insert(provider, None);
            }
            Ok(None)
        }
        provider_icons::Lookup::Unreachable => Err("OmniRoute server unreachable".into()),
    }
}

#[tauri::command]
async fn get_tray_update() -> Result<github::TrayUpdate, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    tauri::async_runtime::spawn_blocking(move || github::latest_release(&current))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn run_doctor(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<doctor::DoctorReport, String> {
    let paths = AppPaths::resolve(&app).map_err(|e| e.to_string())?;
    let active = state.active_version.lock().unwrap().clone();
    Ok(doctor::diagnose(
        &paths.node_bin,
        &paths.prefix_root,
        &paths.current_omniroute_entry(),
        active.as_deref(),
    ))
}

#[tauri::command]
async fn apply_update(app: tauri::AppHandle, target: String) -> Result<String, String> {
    use installer::NodeRuntime;
    use runtime::Prefix;

    let paths = AppPaths::resolve(&app).map_err(|e| e.to_string())?;
    let node_root = paths
        .node_bin
        .parent()
        .and_then(|p| p.parent())
        .ok_or("bundled node not found")?
        .to_path_buf();

    // Set state before the heavy work so the next 5s poll shows "Updating…".
    set_state(
        &app,
        ServerState::Updating {
            target: target.clone(),
        },
    );

    // The staged install (npm + file I/O + atomic swap) must run off the main
    // thread or it freezes the whole popover until the update finishes.
    tauri::async_runtime::spawn_blocking(move || {
        let prefix = Prefix::new(&paths.prefix_root);
        let node = NodeRuntime::new(&node_root);

        match updater::apply_update(&prefix, &node, &target) {
            Ok(new_version) => {
                let _ = node.repair_runtime(&paths.current_omniroute_entry());
                *app.state::<AppState>().active_version.lock().unwrap() = Some(new_version.clone());
                // Swapping `current` does not touch the live daemon: it keeps
                // serving the code it loaded at launch until it is cycled (#34).
                if restart_managed_server(&app, &new_version) {
                    set_state(
                        &app,
                        ServerState::Running {
                            version: Some(new_version.clone()),
                        },
                    );
                    Ok(new_version)
                } else {
                    // The old server is still up and still serving old code. Report
                    // the version actually being served, not the one on disk, so the
                    // popover (and the next update check) stay truthful.
                    if let Some(served) =
                        supervisor::running_server(20128).and_then(|s| s.version)
                    {
                        *app.state::<AppState>().active_version.lock().unwrap() = Some(served);
                    }
                    let reason = format!(
                        "v{new_version} installed, but the running server could not be restarted — use Restart Server (see View Logs)"
                    );
                    set_state(
                        &app,
                        ServerState::Error {
                            reason: reason.clone(),
                        },
                    );
                    Err(reason)
                }
            }
            Err(e) => {
                // Never strand the UI in Updating: restore Running on failure.
                let restored = prefix.active_version();
                set_state(&app, ServerState::Running { version: restored });
                Err(e.to_string())
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_log_path(app: tauri::AppHandle) -> Result<String, String> {
    let paths = AppPaths::resolve(&app).map_err(|e| e.to_string())?;
    Ok(logfile::ServerLog::new(&paths.log_dir)
        .path()
        .display()
        .to_string())
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())?;
    } else {
        manager.disable().map_err(|e| e.to_string())?;
    }
    manager.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|_app, _argv, _cwd| {}))
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState::new())
        .setup(|app| {
            // Release builds used to log nowhere at all, which left silent exits
            // and supervisor decisions impossible to explain after the fact. Ship
            // a file log always; keep the noisy stdout target for dev only.
            // clear_targets() first: `target()` APPENDS to the plugin's defaults
            // (stdout + a LogDir file named after the app), so without it release
            // builds keep an unreachable stdout target and write two identical
            // log files.
            let mut logger = tauri_plugin_log::Builder::default()
                .clear_targets()
                .level(log::LevelFilter::Info)
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir {
                        file_name: Some("tray".into()),
                    },
                ));
            if cfg!(debug_assertions) {
                logger = logger.target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Stdout,
                ));
            }
            app.handle().plugin(logger.build())?;

            // Before the popover exists, and never after: the window inherits
            // the app's space binding from the policy in force when it is built
            // (#58, see `spaces`).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Cause is logged by `build_popover`; note the consequence, since a
            // tray with no popover has no UI at all until the next click retries.
            if build_popover(app.handle()).is_none() {
                log::error!("starting without a popover; the next tray click retries");
            }

            let tray_icon =
                tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon.png"))?;

            let dashboard = MenuItemBuilder::with_id("dashboard", "Open Dashboard").build(app)?;
            let restart = MenuItemBuilder::with_id("restart", "Restart Server").build(app)?;
            let doctor = MenuItemBuilder::with_id("doctor", "Run Doctor").build(app)?;
            let logs = MenuItemBuilder::with_id("logs", "View Logs").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&dashboard, &restart])
                .separator()
                .items(&[&doctor, &logs])
                .separator()
                .items(&[&quit])
                .build()?;

            // On macOS 27 a status item that owns an NSMenu never forwards clicks
            // to its view, so the menu has to stay off it and we present it
            // ourselves on right-click (see `traymenu`).
            let detached_menu = traymenu::detached();
            log::info!(
                "tray menu presented by {}",
                if detached_menu {
                    "the app"
                } else {
                    "the status item"
                }
            );

            let mut tray = TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true);
            if !detached_menu {
                tray = tray.menu(&menu).show_menu_on_left_click(false);
            }

            tray.on_tray_icon_event(move |tray, event| {
                tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                let TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } = event
                else {
                    return;
                };
                match (button, button_state) {
                    (MouseButton::Left, MouseButtonState::Up) => {
                        toggle_popover(tray.app_handle());
                    }
                    // Only ours to handle while the status item has no menu of
                    // its own; otherwise AppKit is already showing one.
                    (MouseButton::Right, MouseButtonState::Down) if detached_menu => {
                        show_tray_menu(tray.app_handle(), &menu);
                    }
                    _ => {}
                }
            })
            .on_menu_event(|app, event| match event.id().as_ref() {
                "quit" => {
                    // Cleanup runs in the RunEvent::ExitRequested handler, off the
                    // live popover, so Quit no longer blocks the UI here.
                    app.exit(0);
                }
                "dashboard" => {
                    let _ = tauri_plugin_opener::open_url("http://127.0.0.1:20128", None::<&str>);
                }
                "restart" => {
                    let handle = app.clone();
                    std::thread::spawn(move || restart_flow(handle));
                }
                "doctor" => {
                    if let Some(window) = app.get_webview_window(POPOVER_LABEL) {
                        app.state::<AppState>()
                            .pin_open
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = window.move_window_constrained(Position::TrayCenter);
                        let _ = window.show();
                        let _ = window.set_focus();
                        let _ = window.emit("run-doctor", ());
                    }
                }
                "logs" => {
                    if let Ok(paths) = AppPaths::resolve(app) {
                        let log = logfile::ServerLog::new(&paths.log_dir);
                        let _ = log.ensure_exists();
                        let _ = tauri_plugin_opener::open_path(
                            log.path().display().to_string(),
                            None::<&str>,
                        );
                    }
                }
                _ => {}
            })
            .build(app)?;

            let handle = app.handle().clone();
            std::thread::spawn(move || bootstrap(handle));
            monitor_health(app.handle().clone());
            schedule_update_checks(app.handle().clone());
            schedule_quota_refresh(app.handle().clone());

            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != POPOVER_LABEL {
                return;
            }
            match event {
                WindowEvent::Focused(false) => {
                    let app_state = window.app_handle().state::<AppState>();
                    if app_state
                        .pin_open
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    let _ = window.hide();
                }
                // The popover is the app's ONLY window, and tao exits the process
                // once the last window is destroyed. Closing it (⌘W while it has
                // focus is enough) therefore took the whole tray app down with no
                // crash report — just a silent exit(0). Hide instead; the window is
                // only ever destroyed on a real quit.
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_quota,
            get_cost,
            get_rate_limits,
            get_usage_trend,
            get_health,
            get_provider_icon,
            get_tray_update,
            get_app_version,
            get_port,
            restart_server,
            open_logs,
            set_autostart,
            get_autostart,
            open_url,
            run_doctor,
            get_log_path,
            apply_update
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                // `code` says who asked. `Some(_)` is `AppHandle::exit`, i.e. the
                // Quit menu item — the only exit this app performs. `None` means
                // tao is exiting because the last window was destroyed, which for
                // a menu-bar app is never what the user meant: refuse it and keep
                // the tray alive (the popover is rebuilt on the next click).
                //
                // Note this cannot block logout or shutdown: a system terminate
                // arrives as `RunEvent::Exit`, never as `ExitRequested`.
                if code.is_none() {
                    log::warn!("exit requested by window teardown, not by Quit; staying alive");
                    api.prevent_exit();
                    return;
                }
                // Single cleanup site: stop our spawned server and clear the lockfile.
                // Idempotent via the Mutex<Option<Supervisor>>::take() inside.
                stop_managed_server(app_handle);
            }
        });
}
