//! Credentials the tray presents to the local OmniRoute server.
//!
//! Every endpoint the popover reads except `/api/monitoring/health` is a
//! MANAGEMENT route in OmniRoute's authz pipeline (`src/server/authz/policies/
//! management.ts`). Once the user has set a dashboard password (`requireLogin`),
//! such a route accepts only a dashboard session, an API key carrying the
//! `manage`/`admin` scope, or — on loopback — the machine-derived CLI token that
//! OmniRoute's own `omniroute` CLI sends (`bin/cli/utils/cliToken.mjs`).
//!
//! The default key a fresh install mints is inference-only (`self:usage`), so a
//! Bearer-only request from the tray gets `403 Invalid management token` the
//! moment onboarding completes (#42). The tray is a loopback process just like
//! the CLI, so it sends the same token: `HMAC-SHA256(key = raw machine id,
//! message = salt)`, hex-encoded, in `x-omniroute-cli-token`. The Bearer key is
//! still sent as well — it is what identifies the tray when login is disabled and
//! it keeps working on servers that have the CLI token turned off.
//!
//! Derivation mirrors `node-machine-id` + `src/lib/machineToken.ts` exactly:
//! on macOS the raw id is `IOPlatformUUID` from `ioreg -rd1 -c
//! IOPlatformExpertDevice`, lower-cased; on Linux it is `/var/lib/dbus/machine-id`
//! or `/etc/machine-id`. `OMNIROUTE_CLI_SALT` / `OMNIROUTE_CLI_TOKEN` in
//! `~/.omniroute/.env` are honoured because the server reads them from there too.

use std::path::Path;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const CLI_TOKEN_HEADER: &str = "x-omniroute-cli-token";
const DEFAULT_SALT: &str = "omniroute-cli-auth-v1";

/// What gets attached to every request against the local server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Credentials {
    /// Active OmniRoute API key (Bearer). See `apikey::resolve`.
    pub api_key: Option<String>,
    /// Loopback CLI token, derived from the machine id. `None` when the machine
    /// id could not be read — the request then goes out with the key alone.
    pub cli_token: Option<String>,
}

impl Credentials {
    pub fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.cli_token.is_none()
    }

    /// Attach both credentials. Safe on any route: the server ignores the token
    /// on PUBLIC routes and strips the header before it reaches handlers.
    pub fn apply(&self, mut req: ureq::Request) -> ureq::Request {
        if let Some(key) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {key}"));
        }
        if let Some(token) = &self.cli_token {
            req = req.set(CLI_TOKEN_HEADER, token);
        }
        req
    }
}

/// Resolve the CLI token for this machine, honouring `.env` overrides the same
/// way `bin/cli/api.mjs` does (`OMNIROUTE_CLI_TOKEN` wins, then the salt).
pub fn resolve_cli_token(env_path: &Path) -> Option<String> {
    let env = std::fs::read_to_string(env_path).unwrap_or_default();
    if let Some(explicit) = env_value(&env, "OMNIROUTE_CLI_TOKEN") {
        return Some(explicit);
    }
    let salt = env_value(&env, "OMNIROUTE_CLI_SALT").unwrap_or_else(|| DEFAULT_SALT.to_string());
    let raw_id = raw_machine_id()?;
    Some(derive_machine_token(&raw_id, &salt))
}

/// `HMAC-SHA256(key = raw_id, msg = salt)` → lowercase hex. Same as
/// `deriveMachineToken` in `src/lib/machineToken.ts` and `deriveCliToken` in
/// `bin/cli/utils/cliToken.mjs`.
pub fn derive_machine_token(raw_id: &str, salt: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(raw_id.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(salt.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// The pre-#10148 token the server still accepts (`getLegacyCliTokenSync`):
/// `sha256(sha256hex(raw_id) + salt)[..32]`. Kept for parity and tests; the
/// server checks both, and the HMAC form is what the current CLI sends.
#[allow(dead_code)]
pub fn derive_legacy_token(raw_id: &str, salt: &str) -> String {
    let hashed_id = hex::encode(Sha256::digest(raw_id.as_bytes()));
    let digest = Sha256::digest(format!("{hashed_id}{salt}").as_bytes());
    hex::encode(digest)[..32].to_string()
}

/// Parse `KEY=value` out of a dotenv body, stripping surrounding quotes.
pub fn env_value(contents: &str, key: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(key) {
            let Some(rest) = rest.strip_prefix('=') else {
                continue;
            };
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn raw_machine_id() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ioreg_uuid(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
fn raw_machine_id() -> Option<String> {
    ["/var/lib/dbus/machine-id", "/etc/machine-id"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| normalize_id(&s))
        .filter(|s| !s.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn raw_machine_id() -> Option<String> {
    None
}

/// node-machine-id's darwin branch: take the text after `IOPlatformUUID` up to
/// the end of that line, drop `=`, quotes and whitespace, lower-case.
fn parse_ioreg_uuid(output: &str) -> Option<String> {
    let (_, rest) = output.split_once("IOPlatformUUID")?;
    let line = rest.lines().next()?;
    let id = normalize_id(&line.replace(['=', '"'], ""));
    (!id.is_empty()).then_some(id)
}

fn normalize_id(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "aabbccdd-1122-3344-5566-77889900aabb";

    // Vectors produced with openssl against the same inputs:
    //   printf omniroute-cli-auth-v1 | openssl dgst -sha256 -hmac $UUID
    #[test]
    fn hmac_token_matches_openssl_vector() {
        assert_eq!(
            derive_machine_token(UUID, DEFAULT_SALT),
            "c92d29052256391d134433118f0d199aa3b19dc2961ffa19b130769563049d5b"
        );
    }

    #[test]
    fn legacy_token_matches_openssl_vector() {
        assert_eq!(
            derive_legacy_token(UUID, DEFAULT_SALT),
            "609ca034a11d1ee2f4b15fb7bb9880cb"
        );
    }

    #[test]
    fn parses_ioreg_output_like_node_machine_id() {
        let out = r#"+-o J316sAP  <class IOPlatformExpertDevice, id 0x100000110, registered>
    {
      "IOPlatformSerialNumber" = "XXXXXXXXXX"
      "IOPlatformUUID" = "AABBCCDD-1122-3344-5566-77889900AABB"
      "compatible" = <"J316sAP","MacBookPro18,1","AppleARM">
    }
"#;
        assert_eq!(parse_ioreg_uuid(out).as_deref(), Some(UUID));
        assert_eq!(parse_ioreg_uuid("no uuid here"), None);
        assert_eq!(parse_ioreg_uuid("\"IOPlatformUUID\" = \"\"\n"), None);
    }

    #[test]
    fn env_value_strips_quotes_and_export() {
        let env = "FOO=1\nexport OMNIROUTE_CLI_SALT=\"custom-salt\"\nOMNIROUTE_CLI_TOKEN=\n";
        assert_eq!(
            env_value(env, "OMNIROUTE_CLI_SALT").as_deref(),
            Some("custom-salt")
        );
        assert_eq!(env_value(env, "OMNIROUTE_CLI_TOKEN"), None);
        assert_eq!(
            env_value(env, "OMNIROUTE_CLI"),
            None,
            "prefix must not match"
        );
    }

    #[test]
    fn explicit_env_token_wins_over_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let env = tmp.path().join(".env");
        std::fs::write(&env, "OMNIROUTE_CLI_TOKEN=deadbeef\n").unwrap();
        assert_eq!(resolve_cli_token(&env).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn custom_salt_changes_derived_token() {
        assert_ne!(
            derive_machine_token(UUID, DEFAULT_SALT),
            derive_machine_token(UUID, "rotated")
        );
    }

    #[test]
    fn apply_sets_both_headers() {
        let creds = Credentials {
            api_key: Some("sk-x".into()),
            cli_token: Some("tok".into()),
        };
        let req = creds.apply(ureq::get("http://127.0.0.1:1/x"));
        assert_eq!(req.header("Authorization"), Some("Bearer sk-x"));
        assert_eq!(req.header(CLI_TOKEN_HEADER), Some("tok"));
        assert!(Credentials::default().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_machine_id_is_a_uuid() {
        let id = raw_machine_id().expect("ioreg available on macOS");
        assert_eq!(id.len(), 36);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(id, id.to_lowercase());
    }
}
