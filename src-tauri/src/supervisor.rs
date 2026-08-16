use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::lockfile::{LockRecord, Lockfile};

const DEFAULT_PORT: u16 = 20128;
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// How long each step of `stop_and_wait`'s escalation waits for the port to free.
const GRACEFUL_STOP_WAIT: Duration = Duration::from_secs(8);
const TERM_WAIT: Duration = Duration::from_secs(4);
const KILL_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Lock(#[from] crate::lockfile::LockError),
    #[error("omniroute entry not found at {0}")]
    EntryMissing(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciliation {
    Adopt,
    ReconcileForeign,
    SpawnFresh,
    /// A server of ours was already on the port but running a different version
    /// than `current` points at; it was stopped and replaced with a fresh spawn.
    ReplaceStale,
}

pub fn decide(
    lock: Option<&LockRecord>,
    port_alive: bool,
    pid_alive: bool,
    expected_token: &str,
) -> Reconciliation {
    match lock {
        Some(record) if port_alive && pid_alive && record.token == expected_token => {
            Reconciliation::Adopt
        }
        _ if port_alive => Reconciliation::ReconcileForeign,
        _ => Reconciliation::SpawnFresh,
    }
}

pub fn server_healthy(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/monitoring/health");
    match ureq::get(&url).timeout(Duration::from_secs(2)).call() {
        Ok(resp) => resp
            .into_string()
            .map(|b| b.contains("\"status\":\"healthy\""))
            .unwrap_or(false),
        Err(_) => false,
    }
}

// Liveness probe for the health monitor: any HTTP reply means the server is
// responding. `server_healthy` is unsuitable there — it demands a literal
// "healthy" status, so a degraded report (open circuit breaker) or a reply
// slower than its timeout while the event loop grinds through /api/usage
// calls would be misread as "server down".
pub fn server_responding(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/monitoring/health");
    match ureq::get(&url).timeout(Duration::from_secs(2)).call() {
        Ok(_) => true,
        Err(ureq::Error::Status(_, _)) => true,
        Err(_) => false,
    }
}

/// What the process currently answering on the port says about itself.
///
/// `/api/monitoring/health` needs no auth and reports both the omniroute version
/// actually loaded in memory and the daemon's pid. That version is the only
/// reliable one: swapping the `current` symlink does not touch a live process,
/// so a running server can serve old code while every path on disk says
/// otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunningServer {
    pub version: Option<String>,
    pub pid: Option<u32>,
}

pub fn running_server(port: u16) -> Option<RunningServer> {
    let url = format!("http://127.0.0.1:{port}/api/monitoring/health");
    let body = ureq::get(&url)
        .timeout(Duration::from_secs(2))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some(parse_running_server(&value))
}

fn parse_running_server(v: &serde_json::Value) -> RunningServer {
    let system = v.get("system");
    RunningServer {
        version: v
            .get("version")
            .and_then(|x| x.as_str())
            .or_else(|| {
                system
                    .and_then(|s| s.get("version"))
                    .and_then(|x| x.as_str())
            })
            .map(str::to_string),
        pid: system
            .and_then(|s| s.get("pid"))
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("pid").and_then(|x| x.as_u64()))
            .map(|p| p as u32),
    }
}

#[cfg(unix)]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg(format!("{field}="))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(unix)]
fn parent_of(pid: u32) -> Option<u32> {
    ps_field(pid, "ppid")?.parse().ok()
}

#[cfg(unix)]
fn process_group_of(pid: u32) -> Option<u32> {
    ps_field(pid, "pgid")?.parse().ok()
}

/// True when `pid` — or its parent — was launched out of `prefix_root`.
///
/// The daemon rewrites its own process title to `omniroute (vX.Y.Z)`, so its argv
/// carries no path at all; the launcher we spawned is the process that still
/// shows `…/omniroute-prefix/current/…/omniroute.mjs`, hence the parent hop. This
/// is the guard that keeps every kill path off a server we did not start (a user
/// running `omniroute serve` from their own global install).
#[cfg(unix)]
pub fn belongs_to_prefix(pid: u32, prefix_root: &Path) -> bool {
    let needle = prefix_root.to_string_lossy().to_string();
    let matches = |p: u32| ps_field(p, "args").is_some_and(|args| args.contains(&needle));
    matches(pid)
        || parent_of(pid).filter(|p| *p > 1).is_some_and(matches)
        || has_open_file_under(pid, &needle)
}

/// Last-resort ownership signal, for when the launcher is gone and the daemon has
/// been reparented to launchd: whatever its argv claims, the native modules it
/// keeps open still resolve to the version directory it was started from.
#[cfg(unix)]
fn has_open_file_under(pid: u32, needle: &str) -> bool {
    ["/usr/sbin/lsof", "/usr/bin/lsof"].iter().any(|lsof| {
        Command::new(lsof)
            .arg("-p")
            .arg(pid.to_string())
            .arg("-Fn")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(needle))
    })
}

#[cfg(not(unix))]
pub fn belongs_to_prefix(_pid: u32, _prefix_root: &Path) -> bool {
    false
}

#[cfg(not(unix))]
fn process_group_of(_pid: u32) -> Option<u32> {
    None
}

pub fn port_alive(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    match addr.to_socket_addrs() {
        Ok(mut addrs) => addrs
            .next()
            .map(|a| TcpStream::connect_timeout(&a, PROBE_TIMEOUT).is_ok())
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(unix)]
#[allow(dead_code)]
pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "getpgrp"]
    fn libc_getpgrp() -> i32;
}

#[cfg(unix)]
const SIGTERM: i32 = 15;

#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Never signal the group the tray itself lives in — that would take down the app.
#[cfg(unix)]
fn is_own_group(group: u32) -> bool {
    unsafe { libc_getpgrp() == group as i32 }
}

#[cfg(unix)]
fn signal_group(group: u32, sig: i32) {
    if group <= 1 || is_own_group(group) {
        return;
    }
    unsafe {
        libc_kill(-(group as i32), sig);
    }
}

#[cfg(unix)]
fn signal_group_term(pid: u32) {
    signal_group(pid, SIGTERM);
}

#[cfg(unix)]
fn signal_group_kill(pid: u32) {
    signal_group(pid, SIGKILL);
}

#[cfg(not(unix))]
fn signal_group_term(_pid: u32) {}

#[cfg(not(unix))]
fn signal_group_kill(_pid: u32) {}

#[cfg(not(unix))]
#[allow(dead_code)]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

pub struct Supervisor {
    node_bin: PathBuf,
    omniroute_entry: PathBuf,
    port: u16,
    lockfile: Lockfile,
    token: String,
    child: Option<Child>,
    log: Option<crate::logfile::ServerLog>,
    /// The version `current` points at, i.e. what a live server *should* be
    /// running. A live server reporting anything else is stale and gets replaced.
    expected_version: Option<String>,
    /// Whether the server on our port came out of our own prefix — either we
    /// spawned it, or `reconcile` traced it back to us (typically a daemon that
    /// outlived an earlier tray run). Resolved eagerly so the quit path, which
    /// runs on the UI thread and must not block, can decide without probing.
    /// A server that is genuinely someone else's is never stopped by us.
    manages_server: bool,
}

impl Supervisor {
    pub fn new(
        node_bin: PathBuf,
        omniroute_entry: PathBuf,
        state_dir: PathBuf,
        token: String,
    ) -> Self {
        Self {
            node_bin,
            omniroute_entry,
            port: DEFAULT_PORT,
            lockfile: Lockfile::new(state_dir),
            token,
            child: None,
            log: None,
            expected_version: None,
            manages_server: false,
        }
    }

    pub fn with_log(mut self, log: crate::logfile::ServerLog) -> Self {
        self.log = Some(log);
        self
    }

    pub fn with_expected_version(mut self, version: impl Into<String>) -> Self {
        self.expected_version = Some(version.into());
        self
    }

    pub fn set_expected_version(&mut self, version: impl Into<String>) {
        self.expected_version = Some(version.into());
    }

    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.port
    }

    #[allow(dead_code)]
    pub fn set_port(&mut self, port: u16) {
        self.port = port;
    }

    pub fn spawn(&mut self) -> Result<u32, SupervisorError> {
        if !self.omniroute_entry.exists() {
            return Err(SupervisorError::EntryMissing(self.omniroute_entry.clone()));
        }
        let mut command = Command::new(&self.node_bin);
        command
            .arg(&self.omniroute_entry)
            .arg("serve")
            .arg("--no-recovery")
            .arg("--no-tray")
            .arg("--no-open")
            .arg("--port")
            .arg(self.port.to_string());

        if let Some(node_dir) = self.node_bin.parent() {
            let existing = std::env::var("PATH").unwrap_or_default();
            let new_path = format!("{}:{existing}", node_dir.display());
            command.env("PATH", new_path);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        if let Some(log) = &self.log {
            if let Ok(out) = log.open_for_append() {
                if let Ok(err) = out.try_clone() {
                    command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
                }
            }
        }

        let child = command.spawn()?;
        let pid = child.id();
        self.child = Some(child);
        self.manages_server = true;
        self.lockfile.write(&LockRecord {
            pid,
            port: self.port,
            token: self.token.clone(),
        })?;
        Ok(pid)
    }

    /// `<prefix root>/current/node_modules/omniroute/bin/omniroute.mjs` → `<prefix root>`.
    fn prefix_root(&self) -> Option<&Path> {
        self.omniroute_entry.ancestors().nth(5)
    }

    /// Identify the live server in one probe: what it reports about itself, and
    /// whether it traces back to our prefix (i.e. we may stop it).
    fn inspect_running(&self) -> Option<(RunningServer, bool)> {
        let running = running_server(self.port)?;
        let ours = match (running.pid, self.prefix_root()) {
            (Some(pid), Some(prefix)) => belongs_to_prefix(pid, prefix),
            _ => false,
        };
        Some((running, ours))
    }

    /// True when a server of ours is serving a version other than the one
    /// `current` resolves to — the signature of a daemon that survived an update.
    fn is_stale(&self, running: &RunningServer, ours: bool) -> bool {
        match (
            ours,
            running.version.as_deref(),
            self.expected_version.as_deref(),
        ) {
            (true, Some(running), Some(expected)) => running != expected,
            _ => false,
        }
    }

    pub fn reconcile(&mut self) -> Result<Reconciliation, SupervisorError> {
        let lock = self.lockfile.read()?;
        let alive = self.server_present();

        if alive {
            // Resolve ownership up front: it decides both whether a stale server
            // may be replaced here and whether quitting is allowed to stop it.
            let (running, ours) = self.inspect_running().unwrap_or_default();
            self.manages_server = ours;

            // A symlink swap leaves the running daemon untouched, and the daemon
            // outlives the tray, so a relaunch can find a server still executing
            // the version from before an update. Cycle it instead of adopting it.
            if self.is_stale(&running, ours) {
                log::warn!(
                    "server on port {} runs {} but {} is active; restarting it",
                    self.port,
                    running.version.unwrap_or_default(),
                    self.expected_version.clone().unwrap_or_default(),
                );
                if self.stop_and_wait() {
                    self.lockfile.clear()?;
                    self.spawn()?;
                    return Ok(Reconciliation::ReplaceStale);
                }
                log::warn!(
                    "stale server on port {} did not release the port; leaving it in place",
                    self.port
                );
                return Ok(Reconciliation::ReconcileForeign);
            }
        }

        let decision = decide(lock.as_ref(), alive, alive, &self.token);
        match decision {
            Reconciliation::SpawnFresh => {
                self.lockfile.clear()?;
                self.spawn()?;
            }
            Reconciliation::Adopt | Reconciliation::ReconcileForeign => {}
            Reconciliation::ReplaceStale => unreachable!("handled above"),
        }
        Ok(decision)
    }

    #[allow(dead_code)]
    fn server_present(&self) -> bool {
        for attempt in 0..3 {
            if server_healthy(self.port) || port_alive(self.port) {
                return true;
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_millis(600));
            }
        }
        false
    }

    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if server_healthy(self.port) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        server_healthy(self.port)
    }

    /// `omniroute stop`, which terminates the detached server through omniroute's
    /// own pid-file — so it also reaches a daemon this process never spawned.
    /// All stdio is nulled so no inherited descriptor keeps a handle alive.
    fn stop_command(&self) -> Command {
        let mut cmd = Command::new(&self.node_bin);
        cmd.arg(&self.omniroute_entry)
            .arg("stop")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(node_dir) = self.node_bin.parent() {
            let existing = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{existing}", node_dir.display()));
        }
        cmd
    }

    pub fn stop(&mut self) -> Result<(), SupervisorError> {
        // This runs on the main/UI thread during quit, so it MUST NOT block.
        // Liveness is tracked over HTTP, not this launcher PID, and the OS reaps
        // our children on exit — so there is no reason to wait() here (a wait on
        // omniroute's double-forked, detached launcher can block forever).
        //
        // Fired even when we hold no Child handle, as long as `reconcile` traced
        // the live server back to our prefix: after a tray relaunch the daemon
        // from the previous run is still on the port with nothing owning it, and
        // leaving it behind is what stranded users on an old version (#34). The
        // pid-file makes `omniroute stop` reach that orphan too. A server that is
        // NOT ours (a user's own `omniroute serve`) is never touched.
        if self.manages_server {
            let _ = self.stop_command().spawn();
        }

        if let Some(child) = self.child.take() {
            // Best-effort backstop: SIGTERM the launcher's process group. No sleep,
            // no SIGKILL — both would block/pointlessly delay the main thread.
            signal_group_term(child.id());
            drop(child);
        }
        // ALWAYS clear the lockfile, synchronously, last. Idempotent.
        self.lockfile.clear()?;
        Ok(())
    }

    /// Stop whatever is serving on our port and block until the port is free.
    ///
    /// Unlike `stop()` — which runs on the UI thread at quit and must never block
    /// — this is for background threads that need the port genuinely released
    /// before spawning a replacement. It escalates: `omniroute stop`, then SIGTERM
    /// and finally SIGKILL of the daemon's process group (which also collects the
    /// launcher and helper processes such as esbuild). Signals are only ever sent
    /// to a process that traces back to our prefix.
    ///
    /// Returns true when the port ended up free.
    pub fn stop_and_wait(&mut self) -> bool {
        let inspected = self.inspect_running();
        let daemon_pid = inspected.as_ref().and_then(|(r, _)| r.pid);
        let child = self.child.take();

        // Same rule as `stop()`: only ever stop a server that is ours.
        let ours =
            child.is_some() || inspected.is_some_and(|(_, ours)| ours) || self.manages_server;
        if !ours {
            let free = !port_alive(self.port);
            if !free {
                log::warn!(
                    "server on port {} is not ours; leaving it running",
                    self.port
                );
            }
            let _ = self.lockfile.clear();
            return free;
        }

        let mut stop_child = self.stop_command().spawn().ok();
        if let Some(child) = &child {
            signal_group_term(child.id());
        }

        let freed = self.wait_port_free(GRACEFUL_STOP_WAIT) || self.force_stop(daemon_pid);

        if let Some(mut stopper) = stop_child.take() {
            let _ = stopper.kill();
            let _ = stopper.wait();
        }
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.lockfile.clear();
        freed
    }

    fn force_stop(&self, daemon_pid: Option<u32>) -> bool {
        let Some(pid) = daemon_pid else {
            return false;
        };
        let Some(prefix) = self.prefix_root() else {
            return false;
        };
        if !belongs_to_prefix(pid, prefix) {
            log::warn!(
                "server on port {} is not ours; not signalling it",
                self.port
            );
            return false;
        }
        let group = process_group_of(pid).unwrap_or(pid);
        signal_group_term(group);
        if self.wait_port_free(TERM_WAIT) {
            return true;
        }
        signal_group_kill(group);
        self.wait_port_free(KILL_WAIT)
    }

    fn wait_port_free(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !port_alive(self.port) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        !port_alive(self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(token: &str) -> LockRecord {
        LockRecord {
            pid: 1234,
            port: DEFAULT_PORT,
            token: token.to_string(),
        }
    }

    #[test]
    fn adopts_own_live_instance() {
        let rec = record("mine");
        assert_eq!(
            decide(Some(&rec), true, true, "mine"),
            Reconciliation::Adopt
        );
    }

    #[test]
    fn reconciles_foreign_when_port_alive_but_token_mismatch() {
        let rec = record("theirs");
        assert_eq!(
            decide(Some(&rec), true, true, "mine"),
            Reconciliation::ReconcileForeign
        );
    }

    #[test]
    fn reconciles_foreign_when_port_alive_but_no_lock() {
        assert_eq!(
            decide(None, true, false, "mine"),
            Reconciliation::ReconcileForeign
        );
    }

    #[test]
    fn spawns_fresh_when_nothing_alive() {
        assert_eq!(
            decide(None, false, false, "mine"),
            Reconciliation::SpawnFresh
        );
    }

    #[test]
    fn spawns_fresh_when_stale_lock_but_dead_port() {
        let rec = record("mine");
        assert_eq!(
            decide(Some(&rec), false, false, "mine"),
            Reconciliation::SpawnFresh
        );
    }

    #[test]
    fn stop_without_owned_child_only_clears_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = Supervisor::new(
            PathBuf::from("/nonexistent/node"),
            PathBuf::from("/nonexistent/omniroute.mjs"),
            dir.path().to_path_buf(),
            "mine".into(),
        );
        sup.lockfile
            .write(&LockRecord {
                pid: 999,
                port: DEFAULT_PORT,
                token: "mine".into(),
            })
            .unwrap();
        assert!(sup.child.is_none());
        sup.stop().unwrap();
        assert!(sup.lockfile.read().unwrap().is_none());
    }

    #[test]
    fn parses_version_and_daemon_pid_from_health() {
        // Shape verified against omniroute 3.8.49 /api/monitoring/health.
        let body = serde_json::json!({
            "status": "healthy",
            "system": { "version": "3.8.49", "nodeVersion": "v24.18.0", "pid": 3203 },
            "version": "3.8.49",
        });
        let running = parse_running_server(&body);
        assert_eq!(running.version.as_deref(), Some("3.8.49"));
        assert_eq!(running.pid, Some(3203));
    }

    #[test]
    fn parses_health_without_system_block() {
        let body = serde_json::json!({ "version": "3.8.44", "pid": 42 });
        let running = parse_running_server(&body);
        assert_eq!(running.version.as_deref(), Some("3.8.44"));
        assert_eq!(running.pid, Some(42));
    }

    #[test]
    fn missing_fields_parse_to_none_rather_than_guessing() {
        let running = parse_running_server(&serde_json::json!({ "status": "healthy" }));
        assert_eq!(running, RunningServer::default());
    }

    fn supervisor_at(prefix_root: &Path, state_dir: &Path) -> Supervisor {
        Supervisor::new(
            PathBuf::from("/nonexistent/node"),
            prefix_root
                .join("current")
                .join("node_modules")
                .join("omniroute")
                .join("bin")
                .join("omniroute.mjs"),
            state_dir.to_path_buf(),
            "mine".into(),
        )
    }

    #[test]
    fn prefix_root_is_derived_from_the_current_entry_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("omniroute-prefix");
        let sup = supervisor_at(&root, dir.path());
        assert_eq!(sup.prefix_root(), Some(root.as_path()));
    }

    #[test]
    fn staleness_needs_ownership_and_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = supervisor_at(&dir.path().join("omniroute-prefix"), dir.path());
        let running = RunningServer {
            version: Some("3.8.48".into()),
            pid: Some(1234),
        };

        // Without a known target there is nothing to compare against.
        assert!(!sup.is_stale(&running, true));

        sup.set_expected_version("3.8.49");
        assert!(sup.is_stale(&running, true));
        // A server that is not ours is never judged stale — we must not stop it.
        assert!(!sup.is_stale(&running, false));
        // Matching versions are current.
        sup.set_expected_version("3.8.48");
        assert!(!sup.is_stale(&running, true));
        // A server that does not report a version cannot be proven stale.
        assert!(!sup.is_stale(&RunningServer::default(), true));
    }

    #[test]
    fn a_freshly_built_supervisor_manages_nothing() {
        // Ownership is only ever granted by spawning or by reconcile tracing the
        // live server back to our prefix, so quit cannot stop a stranger's server.
        let dir = tempfile::tempdir().unwrap();
        let sup = supervisor_at(&dir.path().join("omniroute-prefix"), dir.path());
        assert!(!sup.manages_server);
    }

    #[test]
    fn force_stop_without_a_daemon_pid_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let sup = supervisor_at(&dir.path().join("omniroute-prefix"), dir.path());
        assert!(!sup.force_stop(None));
    }

    #[test]
    fn force_stop_refuses_a_process_outside_our_prefix() {
        // Our own pid is very much alive but does not resolve to the prefix, so
        // it must be left alone — this is the guard that keeps us off a user's
        // own `omniroute serve`.
        let dir = tempfile::tempdir().unwrap();
        let sup = supervisor_at(&dir.path().join("omniroute-prefix"), dir.path());
        assert!(!sup.force_stop(Some(std::process::id())));
    }

    #[cfg(unix)]
    #[test]
    fn own_process_group_is_never_signalled() {
        let group = unsafe { libc_getpgrp() } as u32;
        assert!(is_own_group(group));
        // Would kill the test runner if the guard regressed.
        signal_group_kill(group);
    }

    #[test]
    fn stop_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut sup = Supervisor::new(
            PathBuf::from("/nonexistent/node"),
            PathBuf::from("/nonexistent/omniroute.mjs"),
            dir.path().to_path_buf(),
            "mine".into(),
        );
        sup.stop().unwrap();
        sup.stop().unwrap();
        assert!(sup.lockfile.read().unwrap().is_none());
    }

    #[test]
    fn does_not_adopt_when_pid_dead_even_if_port_alive() {
        let rec = record("mine");
        assert_eq!(
            decide(Some(&rec), true, false, "mine"),
            Reconciliation::ReconcileForeign
        );
    }

    #[test]
    #[ignore = "live test: requires a running server on OMNIROUTE_LIVE_PORT installed under OMNIROUTE_LIVE_PREFIX"]
    fn live_running_server_is_identified_and_recognised_as_ours() {
        let port: u16 = std::env::var("OMNIROUTE_LIVE_PORT")
            .expect("OMNIROUTE_LIVE_PORT")
            .parse()
            .unwrap();
        let prefix = PathBuf::from(std::env::var("OMNIROUTE_LIVE_PREFIX").expect("prefix"));

        let running = running_server(port).expect("health endpoint must answer");
        let version = running.version.expect("health must report a version");
        let pid = running.pid.expect("health must report a pid");
        assert!(version.starts_with('3'), "unexpected version {version}");

        assert!(
            belongs_to_prefix(pid, &prefix),
            "daemon {pid} must resolve back to {}",
            prefix.display()
        );
        assert!(
            !belongs_to_prefix(std::process::id(), &prefix),
            "the test runner must not be mistaken for the server"
        );
        assert!(process_group_of(pid).is_some(), "pgid must resolve");
    }

    #[test]
    #[ignore = "live test: requires OMNIROUTE_LIVE_NODE, OMNIROUTE_LIVE_ENTRY, OMNIROUTE_LIVE_PORT"]
    fn live_spawn_probe_kill_cycle() {
        use std::thread::sleep;

        let node = std::env::var("OMNIROUTE_LIVE_NODE").expect("OMNIROUTE_LIVE_NODE");
        let entry = std::env::var("OMNIROUTE_LIVE_ENTRY").expect("OMNIROUTE_LIVE_ENTRY");
        let port: u16 = std::env::var("OMNIROUTE_LIVE_PORT")
            .expect("OMNIROUTE_LIVE_PORT")
            .parse()
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut sup = Supervisor::new(
            PathBuf::from(node),
            PathBuf::from(entry),
            dir.path().to_path_buf(),
            "live-token".into(),
        );
        sup.set_port(port);

        assert!(!port_alive(port), "port must be free before spawn");
        let pid = sup.spawn().unwrap();
        assert!(pid_alive(pid), "child pid must be alive after spawn");

        let mut up = false;
        for _ in 0..60 {
            if port_alive(port) {
                up = true;
                break;
            }
            sleep(Duration::from_millis(500));
        }
        assert!(up, "server must be listening within 30s");

        let rec = sup.lockfile.read().unwrap().expect("lockfile written");
        assert_eq!(rec.pid, pid);
        assert_eq!(rec.port, port);

        sup.stop().unwrap();
        sleep(Duration::from_secs(1));
        assert!(!pid_alive(pid), "child must be dead after stop");
        assert!(
            sup.lockfile.read().unwrap().is_none(),
            "lock cleared after stop"
        );
    }

    #[test]
    #[ignore = "live test: requires a foreign omniroute already listening on OMNIROUTE_FOREIGN_PORT"]
    fn live_foreign_instance_is_reconciled_not_duplicated() {
        let port: u16 = std::env::var("OMNIROUTE_FOREIGN_PORT")
            .expect("OMNIROUTE_FOREIGN_PORT")
            .parse()
            .unwrap();
        assert!(
            port_alive(port),
            "foreign server must be running on the port"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut sup = Supervisor::new(
            PathBuf::from("/nonexistent/node"),
            PathBuf::from("/nonexistent/omniroute.mjs"),
            dir.path().to_path_buf(),
            "mine".into(),
        );
        sup.set_port(port);

        let decision = sup.reconcile().unwrap();
        assert_eq!(
            decision,
            Reconciliation::ReconcileForeign,
            "must reconcile foreign instance, not spawn a duplicate"
        );
        assert!(
            sup.child.is_none(),
            "must not have spawned a child against a foreign instance"
        );
    }
}
