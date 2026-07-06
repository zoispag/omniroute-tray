use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use thiserror::Error;

use crate::lockfile::{LockRecord, Lockfile};

const DEFAULT_PORT: u16 = 20128;
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

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
}

#[cfg(unix)]
#[allow(dead_code)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
#[allow(dead_code)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
#[allow(dead_code)]
fn kill_process_group(pid: u32) {
    let group = -(pid as i32);
    unsafe {
        libc_kill(group, SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(300));
    unsafe {
        libc_kill(group, SIGKILL);
    }
}

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
        }
    }

    pub fn with_log(mut self, log: crate::logfile::ServerLog) -> Self {
        self.log = Some(log);
        self
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
        self.lockfile.write(&LockRecord {
            pid,
            port: self.port,
            token: self.token.clone(),
        })?;
        Ok(pid)
    }

    pub fn reconcile(&mut self) -> Result<Reconciliation, SupervisorError> {
        let lock = self.lockfile.read()?;
        let alive = self.server_present();
        let decision = decide(lock.as_ref(), alive, alive, &self.token);
        match decision {
            Reconciliation::SpawnFresh => {
                self.lockfile.clear()?;
                self.spawn()?;
            }
            Reconciliation::Adopt => {}
            Reconciliation::ReconcileForeign => {}
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

    pub fn stop(&mut self) -> Result<(), SupervisorError> {
        if let Some(mut child) = self.child.take() {
            let mut stop_cmd = Command::new(&self.node_bin);
            stop_cmd
                .arg(&self.omniroute_entry)
                .arg("stop")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(node_dir) = self.node_bin.parent() {
                let existing = std::env::var("PATH").unwrap_or_default();
                stop_cmd.env("PATH", format!("{}:{existing}", node_dir.display()));
            }
            let _ = stop_cmd.status();
            let _ = child.wait();
        }
        self.lockfile.clear()?;
        Ok(())
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
    fn does_not_adopt_when_pid_dead_even_if_port_alive() {
        let rec = record("mine");
        assert_eq!(
            decide(Some(&rec), true, false, "mine"),
            Reconciliation::ReconcileForeign
        );
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
