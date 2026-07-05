use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const LOCK_NAME: &str = "supervisor.lock";

#[derive(Debug, Error)]
pub enum LockError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed lockfile: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

pub struct Lockfile {
    path: PathBuf,
}

impl Lockfile {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(LOCK_NAME),
        }
    }

    pub fn read(&self) -> Result<Option<LockRecord>, LockError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                let record = serde_json::from_str(&contents)
                    .map_err(|e| LockError::Malformed(e.to_string()))?;
                Ok(Some(record))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(LockError::Io(e)),
        }
    }

    pub fn write(&self, record: &LockRecord) -> Result<(), LockError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("lock.tmp");
        fs::write(
            &tmp,
            serde_json::to_vec(record).map_err(|e| LockError::Malformed(e.to_string()))?,
        )?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), LockError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(LockError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Lockfile::new(tmp.path());
        assert_eq!(lock.read().unwrap(), None);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Lockfile::new(tmp.path());
        let record = LockRecord {
            pid: 4321,
            port: 20128,
            token: "abc-123".into(),
        };
        lock.write(&record).unwrap();
        assert_eq!(lock.read().unwrap(), Some(record));
    }

    #[test]
    fn clear_removes_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = Lockfile::new(tmp.path());
        lock.write(&LockRecord {
            pid: 1,
            port: 2,
            token: "t".into(),
        })
        .unwrap();
        lock.clear().unwrap();
        assert_eq!(lock.read().unwrap(), None);
    }

    #[test]
    fn malformed_lock_reports_error() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(LOCK_NAME), b"not json").unwrap();
        let lock = Lockfile::new(tmp.path());
        assert!(matches!(lock.read(), Err(LockError::Malformed(_))));
    }
}
