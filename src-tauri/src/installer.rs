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
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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

    pub fn version(&self) -> Result<String, InstallError> {
        let out = Command::new(&self.node_bin).arg("--version").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    pub fn repair_runtime(&self, omniroute_entry: &Path) -> Result<(), InstallError> {
        let status = Command::new(&self.node_bin)
            .arg(omniroute_entry)
            .arg("runtime")
            .arg("repair")
            .status()?;
        if !status.success() {
            return Err(InstallError::NpmFailed("runtime repair failed".into()));
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
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        node.npm_install(&format!("omniroute@{version}"), &dir)?;
        prefix.mark_complete(version)?;
    }
    prefix.activate(version)?;
    Ok(())
}

pub fn ensure_installed(
    prefix: &Prefix,
    node: &NodeRuntime,
    pinned: &str,
) -> Result<String, InstallError> {
    prefix.discard_incomplete()?;
    if let Some(active) = prefix.active_version() {
        return Ok(active);
    }
    if let Ok(good) = prefix.last_good_version() {
        prefix.activate(&good)?;
        return Ok(good);
    }
    install_version(prefix, node, pinned)?;
    Ok(pinned.to_string())
}
