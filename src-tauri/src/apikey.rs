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
    // Prefer a key that carries a management scope: it is the only kind of
    // Bearer the server accepts on management routes once login is required.
    // `scopes` is a JSON array of strings (e.g. `["self:usage"]`). Older schemas
    // have no `scopes` column, so fall back to plain recency if that query fails.
    const SCOPED: &str = "SELECT key FROM api_keys \
         WHERE is_active = 1 AND revoked_at IS NULL AND key IS NOT NULL \
         ORDER BY (scopes LIKE '%\"manage\"%' OR scopes LIKE '%\"admin\"%') DESC, \
                  last_used_at DESC \
         LIMIT 1";
    const RECENT: &str = "SELECT key FROM api_keys \
         WHERE is_active = 1 AND revoked_at IS NULL AND key IS NOT NULL \
         ORDER BY last_used_at DESC LIMIT 1";
    let first = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, String>(0));
    first(SCOPED).or_else(|_| first(RECENT)).ok()
}

pub fn read_from_env_file(env_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(env_path).ok()?;
    crate::omniauth::env_value(&contents, "OMNIROUTE_API_KEY")
}

#[allow(dead_code)]
fn keychain_entry() -> Result<keyring::Entry, ApiKeyError> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| ApiKeyError::Keychain(e.to_string()))
}

#[allow(dead_code)]
pub fn read_from_keychain() -> Option<String> {
    keychain_entry().ok()?.get_password().ok()
}

#[allow(dead_code)]
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
    read_from_env_file(env_path).or_else(|| read_from_db(db_path))
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
    fn prefers_management_scoped_key_over_recent_inference_key() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("storage.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE api_keys (key TEXT, is_active INTEGER, revoked_at TEXT, last_used_at TEXT, scopes TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            r#"INSERT INTO api_keys VALUES
              ('sk-usage', 1, NULL, '2026-09-01', '["self:usage"]'),
              ('sk-manage', 1, NULL, '2025-01-01', '["manage"]'),
              ('sk-admin-revoked', 1, '2025-06-01', '2026-09-02', '["admin"]')"#,
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(read_from_db(&db).as_deref(), Some("sk-manage"));
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
