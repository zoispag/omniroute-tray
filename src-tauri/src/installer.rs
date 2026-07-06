use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::engine_gate::{check_engine, EngineVerdict};
use crate::registry;
use crate::runtime::{Prefix, RuntimeError};

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Registry(#[from] registry::RegistryError),
    #[error("bundled node requires an app update: omniroute {version} needs node {required}, bundle has {bundled}")]
    EngineIncompatible {
        version: String,
        required: String,
        bundled: String,
    },
    #[error("npm install failed: {0}")]
    NpmFailed(String),
    #[error("install verification failed: {0}")]
    Incomplete(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl InstallError {
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            InstallError::NpmFailed(_)
                | InstallError::Incomplete(_)
                | InstallError::Registry(_)
                | InstallError::Io(_)
        )
    }
}

pub struct NodeRuntime {
    node_bin: PathBuf,
    npm_cli: PathBuf,
}

impl NodeRuntime {
    pub fn new(node_root: impl AsRef<Path>) -> Self {
        let root = node_root.as_ref();
        Self {
            node_bin: root.join("bin").join("node"),
            npm_cli: root
                .join("lib")
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
        }
    }

    // npm lifecycle scripts (e.g. better-sqlite3's `prebuild-install || node-gyp
    // rebuild`) invoke bare `node`. Under launchd our bundled node bin is not on
    // PATH and there is no system node, so scripts fail with code 127. Prepend it.
    fn child_path(&self) -> std::ffi::OsString {
        let bin_dir = self
            .node_bin
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        match std::env::var_os("PATH") {
            Some(existing) => {
                let mut joined = bin_dir.into_os_string();
                joined.push(":");
                joined.push(existing);
                joined
            }
            None => bin_dir.into_os_string(),
        }
    }

    pub fn version(&self) -> Result<String, InstallError> {
        let out = Command::new(&self.node_bin).arg("--version").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn repair_runtime(&self, omniroute_entry: &Path) -> Result<(), InstallError> {
        let omniroute_root = omniroute_entry
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| InstallError::NpmFailed("cannot resolve omniroute root".into()))?;
        let dist = omniroute_root.join("dist");

        // `omniroute runtime repair` reports success but does NOT rebuild the
        // dist/node_modules/better-sqlite3 copy the server dlopen's, so the
        // native ABI mismatch persists. Rebuild it directly against our node.
        let target = if dist.join("node_modules").join("better-sqlite3").is_dir() {
            &dist
        } else {
            omniroute_root
        };

        let status = Command::new(&self.node_bin)
            .arg(&self.npm_cli)
            .arg("rebuild")
            .arg("better-sqlite3")
            .arg("--prefix")
            .arg(target)
            .env("PATH", self.child_path())
            .status()?;
        if !status.success() {
            return Err(InstallError::NpmFailed(
                "better-sqlite3 rebuild failed".into(),
            ));
        }
        Ok(())
    }

    pub fn npm_install(&self, spec: &str, cwd: &Path) -> Result<(), InstallError> {
        let status = Command::new(&self.node_bin)
            .arg(&self.npm_cli)
            .arg("install")
            .arg(spec)
            .arg("--no-audit")
            .arg("--no-fund")
            .arg("--foreground-scripts")
            .env("PATH", self.child_path())
            .current_dir(cwd)
            .status()?;
        if !status.success() {
            return Err(InstallError::NpmFailed(format!("exit {status}")));
        }
        Ok(())
    }
}

pub fn install_version(
    prefix: &Prefix,
    node: &NodeRuntime,
    version: &str,
) -> Result<(), InstallError> {
    let bundled_node = node.version()?;
    let engines = registry::engines_node(version)?;
    match check_engine(engines.as_deref(), &bundled_node) {
        EngineVerdict::Incompatible { required, bundled } => {
            return Err(InstallError::EngineIncompatible {
                version: version.to_string(),
                required,
                bundled,
            });
        }
        EngineVerdict::Compatible | EngineVerdict::Unknown => {}
    }

    prefix.ensure_layout()?;
    let dir = prefix.version_dir(version);
    if !prefix.is_version_complete(version) {
        if let Err(e) = download_and_verify(prefix, node, version, &dir) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    }
    prefix.activate(version)?;
    verify_install(prefix, version)?;
    Ok(())
}

fn download_and_verify(
    prefix: &Prefix,
    node: &NodeRuntime,
    version: &str,
    dir: &Path,
) -> Result<(), InstallError> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    node.npm_install(&format!("omniroute@{version}"), dir)?;
    verify_install(prefix, version)?;
    prefix.mark_complete(version)?;
    Ok(())
}

fn verify_install(prefix: &Prefix, version: &str) -> Result<(), InstallError> {
    let entry = prefix.omniroute_entry(version);
    if !entry.is_file() {
        return Err(InstallError::Incomplete(format!(
            "omniroute entry missing after install: {}",
            entry.display()
        )));
    }
    let pkg = prefix.omniroute_package_json(version);
    let raw = std::fs::read_to_string(&pkg)
        .map_err(|e| InstallError::Incomplete(format!("cannot read package.json: {e}")))?;
    let installed = raw
        .split_once("\"version\"")
        .and_then(|(_, rest)| rest.split('"').nth(1))
        .unwrap_or_default();
    if installed != version {
        return Err(InstallError::Incomplete(format!(
            "version mismatch: expected {version}, found {installed}"
        )));
    }
    Ok(())
}

pub fn ensure_installed(
    prefix: &Prefix,
    node: &NodeRuntime,
    pinned: &str,
) -> Result<String, InstallError> {
    prefix.discard_incomplete()?;

    if let Some(active) = prefix.active_version() {
        if verify_install(prefix, &active).is_ok() {
            return Ok(active);
        }
        prefix.deactivate()?;
        prefix.discard_version(&active)?;
    }

    if let Some(good) = verified_last_good(prefix)? {
        prefix.activate(&good)?;
        return Ok(good);
    }

    prefix.discard_version(pinned)?;
    install_version(prefix, node, pinned)?;
    Ok(pinned.to_string())
}

fn verified_last_good(prefix: &Prefix) -> Result<Option<String>, InstallError> {
    for version in prefix.complete_versions_desc()? {
        if verify_install(prefix, &version).is_ok() {
            return Ok(Some(version));
        }
        prefix.discard_version(&version)?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pkg(prefix: &Prefix, version: &str, pkg_version: &str) {
        let bin = prefix
            .omniroute_entry(version)
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(prefix.omniroute_entry(version), b"// entry").unwrap();
        std::fs::write(
            prefix.omniroute_package_json(version),
            format!("{{\n  \"name\": \"omniroute\",\n  \"version\": \"{pkg_version}\"\n}}"),
        )
        .unwrap();
    }

    #[test]
    fn verify_passes_when_entry_and_version_match() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        write_pkg(&prefix, "3.8.44", "3.8.44");
        assert!(verify_install(&prefix, "3.8.44").is_ok());
    }

    #[test]
    fn verify_fails_when_entry_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        let err = verify_install(&prefix, "3.8.44").unwrap_err();
        assert!(matches!(err, InstallError::Incomplete(_)));
    }

    #[test]
    fn verify_fails_on_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        write_pkg(&prefix, "3.8.44", "3.8.45");
        let err = verify_install(&prefix, "3.8.44").unwrap_err();
        assert!(matches!(err, InstallError::Incomplete(_)));
    }

    #[test]
    fn verified_last_good_prunes_broken_marked_version() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.ensure_layout().unwrap();
        std::fs::create_dir_all(prefix.version_dir("3.8.44")).unwrap();
        prefix.mark_complete("3.8.44").unwrap();
        assert!(verified_last_good(&prefix).unwrap().is_none());
        assert!(!prefix.version_dir("3.8.44").is_dir());
    }

    #[test]
    fn engine_incompatible_is_not_transient() {
        let e = InstallError::EngineIncompatible {
            version: "9.9.9".into(),
            required: ">=99".into(),
            bundled: "24.18.0".into(),
        };
        assert!(!e.is_transient());
        assert!(InstallError::Incomplete("x".into()).is_transient());
    }
}
