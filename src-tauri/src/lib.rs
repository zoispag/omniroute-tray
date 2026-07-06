mod analytics;
mod apikey;
mod data;
mod doctor;
mod engine_gate;
mod installer;
mod lockfile;
mod logfile;
mod paths;
mod ratelimits;
mod registry;
mod runtime;
mod state;
mod supervisor;
mod updater;

use std::sync::Mutex;

use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_positioner::{Position, WindowExt};

use data::{CostResult, DataClient, QuotaRow};
use paths::AppPaths;
use state::ServerState;

const POPOVER_LABEL: &str = "popover";

struct AppState {
    server: Mutex<ServerState>,
    data: Mutex<Option<DataClient>>,
    active_version: Mutex<Option<String>>,
    api_key: Mutex<Option<String>>,
    supervisor: Mutex<Option<supervisor::Supervisor>>,
    pin_open: std::sync::atomic::AtomicBool,
}

impl AppState {
    fn new() -> Self {
        Self {
            server: Mutex::new(ServerState::Stopped),
            data: Mutex::new(None),
            active_version: Mutex::new(None),
            api_key: Mutex::new(None),
            supervisor: Mutex::new(None),
            pin_open: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

fn set_state(app: &tauri::AppHandle, next: ServerState) {
    let app_state = app.state::<AppState>();
    *app_state.server.lock().unwrap() = next;
}

fn stop_managed_server(app: &tauri::AppHandle) {
    if let Some(mut sup) = app.state::<AppState>().supervisor.lock().unwrap().take() {
        let _ = sup.stop();
    }
}

fn bootstrap(app: tauri::AppHandle) {
    use installer::{ensure_installed, NodeRuntime};
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

    let version = match ensure_installed(&prefix, &node, paths::PINNED_OMNIROUTE) {
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
    let env_path = paths.omniroute_env_path();
    let db_path = paths.omniroute_db_path();
    {
        let app_state = app.state::<AppState>();
        *app_state.data.lock().unwrap() =
            Some(DataClient::new(paths.node_bin.clone(), entry.clone()));
        *app_state.active_version.lock().unwrap() = Some(version.clone());
        *app_state.api_key.lock().unwrap() = apikey::resolve(&env_path, &db_path);
    }

    let token = format!("omniroute-tray-{}", std::process::id());
    let log = logfile::ServerLog::new(&paths.log_dir);
    let mut supervisor = Supervisor::new(
        paths.node_bin.clone(),
        entry,
        paths.state_dir.clone(),
        token,
    )
    .with_log(log);

    use supervisor::Reconciliation;
    match supervisor.reconcile() {
        Ok(decision) => {
            let ready = match decision {
                Reconciliation::SpawnFresh => {
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

fn toggle_popover(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window(POPOVER_LABEL) else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        let _ = window.move_window(Position::TrayCenter);
        let _ = window.show();
        let _ = window.set_focus();
    }
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
async fn get_cost(state: tauri::State<'_, AppState>) -> Result<CostResult, String> {
    let client = state.data.lock().unwrap().clone();
    let Some(client) = client else {
        return Ok(CostResult::unavailable());
    };
    tauri::async_runtime::spawn_blocking(move || client.cost_by_model("30d"))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_rate_limits(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ratelimits::AccountLimits>, String> {
    let key = state.api_key.lock().unwrap().clone();
    let Some(key) = key else {
        return Ok(Vec::new());
    };
    tauri::async_runtime::spawn_blocking(move || ratelimits::fetch("http://127.0.0.1:20128", &key))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_usage_trend(
    state: tauri::State<'_, AppState>,
) -> Result<Option<analytics::UsageTrend>, String> {
    let key = state.api_key.lock().unwrap().clone();
    let Some(key) = key else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || {
        analytics::fetch("http://127.0.0.1:20128", &key, "30d")
    })
    .await
    .map_err(|e| e.to_string())?
    .map(Some)
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
fn apply_update(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
    target: String,
) -> Result<String, String> {
    use installer::NodeRuntime;
    use runtime::Prefix;

    let paths = AppPaths::resolve(&app).map_err(|e| e.to_string())?;
    let node_root = paths
        .node_bin
        .parent()
        .and_then(|p| p.parent())
        .ok_or("bundled node not found")?
        .to_path_buf();

    set_state(
        &app,
        ServerState::Updating {
            target: target.clone(),
        },
    );

    let prefix = Prefix::new(&paths.prefix_root);
    let node = NodeRuntime::new(&node_root);

    match updater::apply_update(&prefix, &node, &target) {
        Ok(new_version) => {
            *state.active_version.lock().unwrap() = Some(new_version.clone());
            set_state(
                &app,
                ServerState::Running {
                    version: Some(new_version.clone()),
                },
            );
            Ok(new_version)
        }
        Err(e) => {
            let restored = prefix.active_version();
            set_state(&app, ServerState::Running { version: restored });
            Err(e.to_string())
        }
    }
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
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            if let Some(window) = app.get_webview_window(POPOVER_LABEL) {
                let _ = window.hide();
            }

            let tray_icon =
                tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon.png"))?;

            let dashboard = MenuItemBuilder::with_id("dashboard", "Open Dashboard").build(app)?;
            let restart = MenuItemBuilder::with_id("restart", "Restart Server").build(app)?;
            let doctor = MenuItemBuilder::with_id("doctor", "Run Doctor").build(app)?;
            let logs = MenuItemBuilder::with_id("logs", "View Logs").build(app)?;
            let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
            let start_on_login = CheckMenuItemBuilder::with_id("start_on_login", "Start on Login")
                .checked(autostart_enabled)
                .build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&dashboard, &restart])
                .separator()
                .items(&[&doctor, &logs])
                .separator()
                .items(&[&start_on_login])
                .separator()
                .items(&[&quit])
                .build()?;

            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_popover(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "quit" => {
                        stop_managed_server(app);
                        app.exit(0);
                    }
                    "dashboard" => {
                        let _ =
                            tauri_plugin_opener::open_url("http://127.0.0.1:20128", None::<&str>);
                    }
                    "restart" => log::info!("restart requested"),
                    "doctor" => {
                        if let Some(window) = app.get_webview_window(POPOVER_LABEL) {
                            app.state::<AppState>()
                                .pin_open
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            let _ = window.move_window(Position::TrayCenter);
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
                    "start_on_login" => {
                        let manager = app.autolaunch();
                        if manager.is_enabled().unwrap_or(false) {
                            let _ = manager.disable();
                        } else {
                            let _ = manager.enable();
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            let handle = app.handle().clone();
            std::thread::spawn(move || bootstrap(handle));

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(false) = event {
                if window.label() == POPOVER_LABEL {
                    let app_state = window.app_handle().state::<AppState>();
                    if app_state
                        .pin_open
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_quota,
            get_cost,
            get_rate_limits,
            get_usage_trend,
            get_app_version,
            set_autostart,
            run_doctor,
            get_log_path,
            apply_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
