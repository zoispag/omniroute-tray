use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl Check {
    fn new(name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub healthy: bool,
    pub checks: Vec<Check>,
}

pub fn diagnose(
    node_bin: &Path,
    prefix_root: &Path,
    current_entry: &Path,
    active_version: Option<&str>,
) -> DoctorReport {
    let mut checks = Vec::new();

    checks.push(if node_bin.is_file() {
        Check::new(
            "Bundled Node",
            CheckStatus::Ok,
            node_bin.display().to_string(),
        )
    } else {
        Check::new("Bundled Node", CheckStatus::Fail, "node binary missing")
    });

    checks.push(if prefix_root.is_dir() {
        Check::new(
            "Prefix directory",
            CheckStatus::Ok,
            prefix_root.display().to_string(),
        )
    } else {
        Check::new("Prefix directory", CheckStatus::Fail, "prefix not created")
    });

    checks.push(match active_version {
        Some(v) => Check::new("Active omniroute", CheckStatus::Ok, format!("v{v}")),
        None => Check::new("Active omniroute", CheckStatus::Fail, "no active version"),
    });

    checks.push(if current_entry.is_file() {
        Check::new("omniroute entry", CheckStatus::Ok, "resolved")
    } else {
        Check::new(
            "omniroute entry",
            CheckStatus::Fail,
            "current entry missing",
        )
    });

    let healthy = checks.iter().all(|c| c.status != CheckStatus::Fail);
    DoctorReport { healthy, checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ok_when_everything_present() {
        let tmp = tempfile::tempdir().unwrap();
        let node = tmp.path().join("node");
        let entry = tmp.path().join("omniroute.mjs");
        std::fs::write(&node, b"x").unwrap();
        std::fs::write(&entry, b"x").unwrap();
        let report = diagnose(&node, tmp.path(), &entry, Some("3.8.44"));
        assert!(report.healthy);
        assert!(report.checks.iter().all(|c| c.status == CheckStatus::Ok));
    }

    #[test]
    fn unhealthy_when_entry_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let node = tmp.path().join("node");
        std::fs::write(&node, b"x").unwrap();
        let entry = tmp.path().join("missing.mjs");
        let report = diagnose(&node, tmp.path(), &entry, None);
        assert!(!report.healthy);
    }

    #[test]
    fn missing_node_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let report = diagnose(
            &tmp.path().join("nope"),
            tmp.path(),
            &tmp.path().join("nope.mjs"),
            None,
        );
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
        assert!(!report.healthy);
    }
}
