use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

const KEYCHAIN_SERVICE: &str = "dev.omniroute.tray";
const KEYCHAIN_ACCOUNT: &str = "omniroute-api-key";

#[derive(Debug, Error)]
pub enum ApiKeyError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no api key available")]
    Unavailable,
    #[error("keychain error: {0}")]
    Keychain(String),
}

pub fn read_from_db(db_path: &Path) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT key FROM api_keys WHERE is_active = 1 AND revoked_at IS NULL AND key IS NOT NULL ORDER BY last_used_at DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

pub fn read_from_env_file(env_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(env_path).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("OMNIROUTE_API_KEY=") {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn keychain_entry() -> Result<keyring::Entry, ApiKeyError> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| ApiKeyError::Keychain(e.to_string()))
}

pub fn read_from_keychain() -> Option<String> {
    keychain_entry().ok()?.get_password().ok()
}

pub fn store_in_keychain(key: &str) -> Result<(), ApiKeyError> {
    keychain_entry()?
        .set_password(key)
        .map_err(|e| ApiKeyError::Keychain(e.to_string()))
}

/// Reserved for the "adopt foreign server" case where no key exists in the
/// shared DB or .env and one must be minted via the CLI. The common path
/// resolves an existing key from storage.sqlite (see `read_from_db`).
#[allow(dead_code)]
pub struct KeyMinter {
    node_bin: PathBuf,
    omniroute_entry: PathBuf,
}

#[allow(dead_code)]
impl KeyMinter {
    pub fn new(node_bin: PathBuf, omniroute_entry: PathBuf) -> Self {
        Self {
            node_bin,
            omniroute_entry,
        }
    }

    fn cli(&self, args: &[&str]) -> Result<String, ApiKeyError> {
        let out = Command::new(&self.node_bin)
            .arg(&self.omniroute_entry)
            .args(args)
            .output()?;
        if !out.status.success() {
            return Err(ApiKeyError::Unavailable);
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn mint(&self) -> Result<String, ApiKeyError> {
        let raw = self.cli(&["keys", "regenerate", "default", "--output", "json"])?;
        extract_key(&raw).ok_or(ApiKeyError::Unavailable)
    }
}

fn extract_key(raw: &str) -> Option<String> {
    let start = raw.find(['{', '['])?;
    let value: serde_json::Value = serde_json::from_str(&raw[start..]).ok()?;
    for field in ["key", "apiKey", "token", "value"] {
        if let Some(k) = value.get(field).and_then(|v| v.as_str()) {
            return Some(k.to_string());
        }
    }
    None
}

pub fn resolve(env_path: &Path, db_path: &Path) -> Option<String> {
    if let Some(k) = read_from_keychain() {
        return Some(k);
    }
    if let Some(k) = read_from_env_file(env_path) {
        let _ = store_in_keychain(&k);
        return Some(k);
    }
    if let Some(k) = read_from_db(db_path) {
        let _ = store_in_keychain(&k);
        return Some(k);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_key_from_env_file() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        std::fs::write(&env, "FOO=bar\nOMNIROUTE_API_KEY=sk-test-123\nBAZ=qux\n").unwrap();
        assert_eq!(read_from_env_file(&env).as_deref(), Some("sk-test-123"));
    }

    #[test]
    fn strips_quotes_from_env_value() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        std::fs::write(&env, "OMNIROUTE_API_KEY=\"sk-quoted\"\n").unwrap();
        assert_eq!(read_from_env_file(&env).as_deref(), Some("sk-quoted"));
    }

    #[test]
    fn returns_none_when_key_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        std::fs::write(&env, "FOO=bar\n").unwrap();
        assert_eq!(read_from_env_file(&env), None);
    }

    #[test]
    fn reads_active_key_from_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("storage.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE api_keys (key TEXT, is_active INTEGER, revoked_at TEXT, last_used_at TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO api_keys VALUES ('sk-revoked', 1, '2020-01-01', '2020-01-01'), ('sk-active', 1, NULL, '2026-01-01')",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(read_from_db(&db).as_deref(), Some("sk-active"));
    }

    #[test]
    fn db_read_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_from_db(&tmp.path().join("nope.sqlite")), None);
    }

    #[test]
    fn extracts_key_from_json_variants() {
        assert_eq!(extract_key(r#"{"key":"abc"}"#).as_deref(), Some("abc"));
        assert_eq!(
            extract_key(r#"log line\n{"apiKey":"xyz"}"#).as_deref(),
            Some("xyz")
        );
        assert_eq!(extract_key(r#"{"nope":1}"#), None);
    }
}
