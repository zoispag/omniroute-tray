use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

const LOG_NAME: &str = "omniroute.log";
const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub struct ServerLog {
    path: PathBuf,
}

impl ServerLog {
    pub fn new(log_dir: impl AsRef<Path>) -> Self {
        Self {
            path: log_dir.as_ref().join(LOG_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn ensure_exists(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !self.path.exists() {
            fs::write(
                &self.path,
                b"OmniRoute server log. Output appears here once the tray starts a managed server.\n",
            )?;
        }
        Ok(())
    }

    pub fn open_for_append(&self) -> std::io::Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.rotate_if_needed()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }

    fn rotate_if_needed(&self) -> std::io::Result<()> {
        if let Ok(meta) = fs::metadata(&self.path) {
            if meta.len() >= MAX_BYTES {
                let rotated = self.path.with_extension("log.1");
                let _ = fs::remove_file(&rotated);
                fs::rename(&self.path, &rotated)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn creates_log_file_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ServerLog::new(tmp.path());
        let mut f = log.open_for_append().unwrap();
        writeln!(f, "hello").unwrap();
        assert!(log.path().is_file());
        assert!(fs::read_to_string(log.path()).unwrap().contains("hello"));
    }

    #[test]
    fn ensure_exists_creates_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ServerLog::new(tmp.path().join("logs"));
        assert!(!log.path().exists());
        log.ensure_exists().unwrap();
        assert!(log.path().is_file());
        assert!(!fs::read_to_string(log.path()).unwrap().is_empty());
    }

    #[test]
    fn rotates_when_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let log = ServerLog::new(tmp.path());
        let big = vec![b'x'; (MAX_BYTES + 1) as usize];
        fs::write(log.path(), &big).unwrap();
        let _ = log.open_for_append().unwrap();
        assert!(log.path().with_extension("log.1").is_file());
        assert!(fs::metadata(log.path()).unwrap().len() < MAX_BYTES);
    }
}
