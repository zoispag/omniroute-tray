use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

const COMPLETE_MARKER: &str = ".install-complete";
const CURRENT_LINK: &str = "current";
const VERSIONS_DIR: &str = "versions";

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no installed omniroute version found")]
    NoVersionInstalled,
    #[error("version {0} is not installed or incomplete")]
    VersionUnavailable(String),
}

pub struct Prefix {
    root: PathBuf,
}

impl Prefix {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join(VERSIONS_DIR)
    }

    pub fn current_link(&self) -> PathBuf {
        self.root.join(CURRENT_LINK)
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions_dir().join(version)
    }

    pub fn omniroute_entry(&self, version: &str) -> PathBuf {
        self.version_dir(version)
            .join("node_modules")
            .join("omniroute")
            .join("bin")
            .join("omniroute.mjs")
    }

    pub fn omniroute_package_json(&self, version: &str) -> PathBuf {
        self.version_dir(version)
            .join("node_modules")
            .join("omniroute")
            .join("package.json")
    }

    pub fn ensure_layout(&self) -> Result<(), RuntimeError> {
        fs::create_dir_all(self.versions_dir())?;
        Ok(())
    }

    pub fn is_version_complete(&self, version: &str) -> bool {
        self.version_dir(version).join(COMPLETE_MARKER).is_file()
    }

    pub fn mark_complete(&self, version: &str) -> Result<(), RuntimeError> {
        let dir = self.version_dir(version);
        if !dir.is_dir() {
            return Err(RuntimeError::VersionUnavailable(version.to_string()));
        }
        fs::write(dir.join(COMPLETE_MARKER), version.as_bytes())?;
        Ok(())
    }

    pub fn activate(&self, version: &str) -> Result<(), RuntimeError> {
        if !self.is_version_complete(version) {
            return Err(RuntimeError::VersionUnavailable(version.to_string()));
        }
        let target = self.version_dir(version);
        atomic_symlink(&target, &self.current_link())?;
        Ok(())
    }

    pub fn active_version(&self) -> Option<String> {
        let link = self.current_link();
        let resolved = fs::read_link(&link).ok()?;
        let dir = if resolved.is_absolute() {
            resolved
        } else {
            link.parent()?.join(resolved)
        };
        let name = dir.file_name()?.to_str()?.to_string();
        if self.is_version_complete(&name) {
            Some(name)
        } else {
            None
        }
    }

    pub fn discard_incomplete(&self) -> Result<Vec<String>, RuntimeError> {
        let mut discarded = Vec::new();
        let versions = self.versions_dir();
        if !versions.is_dir() {
            return Ok(discarded);
        }
        for entry in fs::read_dir(&versions)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !self.is_version_complete(&name) {
                fs::remove_dir_all(entry.path())?;
                discarded.push(name);
            }
        }
        Ok(discarded)
    }

    pub fn last_good_version(&self) -> Result<String, RuntimeError> {
        let versions = self.versions_dir();
        let mut candidates: Vec<String> = Vec::new();
        if versions.is_dir() {
            for entry in fs::read_dir(&versions)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if self.is_version_complete(&name) {
                    candidates.push(name);
                }
            }
        }
        candidates.sort_by(|a, b| compare_versions(a, b));
        candidates.pop().ok_or(RuntimeError::NoVersionInstalled)
    }

    #[allow(dead_code)]
    pub fn seed_from(&self, version: &str, source: &Path) -> Result<(), RuntimeError> {
        let target = self.version_dir(version);
        if target.exists() {
            return Ok(());
        }
        self.ensure_layout()?;
        copy_dir_recursive(source, &target)?;
        self.mark_complete(version)?;
        Ok(())
    }
}

fn atomic_symlink(target: &Path, link: &Path) -> Result<(), RuntimeError> {
    let tmp = link.with_extension("tmp-link");
    let _ = fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &tmp)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, &tmp)?;
    fs::rename(&tmp, link)?;
    Ok(())
}

#[allow(dead_code)]
fn copy_dir_recursive(from: &Path, to: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(va), Ok(vb)) => va.cmp(&vb),
        _ => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_dummy_install(prefix: &Prefix, version: &str) {
        let dir = prefix.version_dir(version);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("omniroute.mjs"), b"stub").unwrap();
    }

    #[test]
    fn activates_complete_version_and_resolves_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.ensure_layout().unwrap();
        write_dummy_install(&prefix, "3.8.44");
        prefix.mark_complete("3.8.44").unwrap();
        prefix.activate("3.8.44").unwrap();
        assert_eq!(prefix.active_version().as_deref(), Some("3.8.44"));
    }

    #[test]
    fn refuses_to_activate_incomplete_version() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.ensure_layout().unwrap();
        write_dummy_install(&prefix, "3.9.0");
        let result = prefix.activate("3.9.0");
        assert!(matches!(result, Err(RuntimeError::VersionUnavailable(_))));
    }

    #[test]
    fn discards_incomplete_and_keeps_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.ensure_layout().unwrap();
        write_dummy_install(&prefix, "3.8.44");
        prefix.mark_complete("3.8.44").unwrap();
        write_dummy_install(&prefix, "3.9.0-partial");
        let discarded = prefix.discard_incomplete().unwrap();
        assert_eq!(discarded, vec!["3.9.0-partial".to_string()]);
        assert!(prefix.version_dir("3.8.44").is_dir());
        assert!(!prefix.version_dir("3.9.0-partial").exists());
    }

    #[test]
    fn last_good_returns_highest_semver() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.ensure_layout().unwrap();
        for v in ["3.8.44", "3.9.0", "3.8.100"] {
            write_dummy_install(&prefix, v);
            prefix.mark_complete(v).unwrap();
        }
        assert_eq!(prefix.last_good_version().unwrap(), "3.9.0");
    }

    #[test]
    fn seed_copies_source_and_marks_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("omniroute.mjs"), b"seed").unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.seed_from("3.8.44", source.path()).unwrap();
        assert!(prefix.is_version_complete("3.8.44"));
        assert!(prefix.version_dir("3.8.44").join("omniroute.mjs").is_file());
    }

    #[test]
    fn rollback_reactivates_last_good_after_discard() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = Prefix::new(tmp.path());
        prefix.ensure_layout().unwrap();
        write_dummy_install(&prefix, "3.8.44");
        prefix.mark_complete("3.8.44").unwrap();
        prefix.activate("3.8.44").unwrap();
        write_dummy_install(&prefix, "3.9.0");
        prefix.discard_incomplete().unwrap();
        let good = prefix.last_good_version().unwrap();
        prefix.activate(&good).unwrap();
        assert_eq!(prefix.active_version().as_deref(), Some("3.8.44"));
    }
}
