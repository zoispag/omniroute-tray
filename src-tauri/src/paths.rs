use std::path::PathBuf;

use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};

pub const PINNED_OMNIROUTE: &str = "3.8.44";

pub struct AppPaths {
    pub node_bin: PathBuf,
    pub prefix_root: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve(app: &AppHandle) -> tauri::Result<Self> {
        let node_bin = app
            .path()
            .resolve("node/bin/node", BaseDirectory::Resource)?;
        let data_dir = app.path().app_data_dir()?;
        Ok(Self {
            node_bin,
            prefix_root: data_dir.join("omniroute-prefix"),
            state_dir: data_dir.join("state"),
            log_dir: data_dir.join("logs"),
        })
    }

    #[allow(dead_code)]
    pub fn omniroute_entry(&self, version: &str) -> PathBuf {
        self.prefix_root
            .join("versions")
            .join(version)
            .join("node_modules")
            .join("omniroute")
            .join("bin")
            .join("omniroute.mjs")
    }

    pub fn current_omniroute_entry(&self) -> PathBuf {
        self.prefix_root
            .join("current")
            .join("node_modules")
            .join("omniroute")
            .join("bin")
            .join("omniroute.mjs")
    }

    pub fn omniroute_env_path(&self) -> PathBuf {
        dirs_home().join(".omniroute").join(".env")
    }

    pub fn omniroute_db_path(&self) -> PathBuf {
        dirs_home().join(".omniroute").join("storage.sqlite")
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
