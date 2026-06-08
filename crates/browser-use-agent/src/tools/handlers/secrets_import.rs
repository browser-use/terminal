//! Import logins from the 1Password CLI (`op`). Each login becomes up to three
//! secrets on its domain: `username`, `password`, and `otp`.

use anyhow::{anyhow, bail, Result};
use browser_use_secrets::SecretKind;
use browser_use_store::Store;
use serde_json::Value;

use super::secrets_admin::{list_secrets, normalize_domain, read_secret_value, set_secret_active};

/// A normalized login from 1Password.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportedLogin {
    pub domain: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Full `otpauth://...` URI, if the item carried a 2FA secret.
    pub otpauth: Option<String>,
}

/// Outcome of an import (a full re-sync that only writes what changed).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportStats {
    /// Logins that didn't exist locally before (all their secrets were new).
    pub new_logins: usize,
    /// Logins that existed but had at least one new/changed secret value.
    pub updated_logins: usize,
    /// Logins already stored identically — nothing written.
    pub unchanged_logins: usize,
    /// Individual secret values actually written (new or changed).
    pub secrets_written: usize,
    /// Entries skipped (no resolvable domain, or no credentials).
    pub skipped: usize,
}

impl ImportStats {
    /// Logins that were added or changed this run.
    pub fn changed_logins(&self) -> usize {
        self.new_logins + self.updated_logins
    }
}

/// Extract the base32 TOTP seed from an `otpauth://` URI's `secret=` parameter.
pub fn otpauth_seed(otpauth: &str) -> Option<String> {
    let query = otpauth.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("secret=") {
            let seed = value.trim().to_string();
            if browser_use_secrets::totp::validate_totp_seed(&seed).is_ok() {
                return Some(seed);
            }
        }
    }
    None
}

/// Sync logins into the store, writing only new/changed values.
pub fn import_logins(store: &Store, logins: &[ImportedLogin]) -> ImportStats {
    // Deduplicate against what's actually LISTED in saved secrets (the metadata
    // the TUI shows), NOT the encrypted value store. A user can delete an
    // imported login (which removes its metadata) and expect a re-import to bring
    // it back; gating on metadata makes that work and ignores any orphaned value
    // a prior delete may have left behind (set_secret_active overwrites it).
    let listed: std::collections::HashSet<(String, String)> = list_secrets(store)
        .unwrap_or_default()
        .into_iter()
        .map(|meta| (meta.domain, meta.placeholder))
        .collect();
    let mut stats = ImportStats::default();
    for login in logins {
        // Match the normalization set_secret applies, so the listed-set lookup
        // and value read use the same key the secret is stored under.
        let domain = normalize_domain(&login.domain);
        let mut desired: Vec<(&str, String, SecretKind)> = Vec::new();
        if let Some(username) = &login.username {
            desired.push(("username", username.clone(), SecretKind::Password));
        }
        if let Some(password) = &login.password {
            desired.push(("password", password.clone(), SecretKind::Password));
        }
        if let Some(seed) = login.otpauth.as_deref().and_then(otpauth_seed) {
            desired.push(("otp", seed, SecretKind::Totp));
        }
        if desired.is_empty() {
            stats.skipped += 1;
            continue;
        }

        let mut existed_any = false;
        let mut wrote_any = false;
        let mut failed_any = false;
        for (name, value, kind) in &desired {
            // "Exists" means it's currently listed — not just that an (orphaned)
            // value happens to sit in the encrypted store.
            let is_listed = listed.contains(&(domain.clone(), (*name).to_string()));
            if is_listed {
                existed_any = true;
            }
            // Only compare values for change-detection when it's actually listed;
            // an unlisted login is treated as new and (re)written.
            let current = if is_listed {
                read_secret_value(store, &domain, name)
            } else {
                None
            };
            if current.as_deref() != Some(value.as_str()) {
                // Only count new/changed once the write actually succeeds.
                if set_secret_active(store, &domain, name, *kind, Vec::new(), value).is_ok() {
                    wrote_any = true;
                    stats.secrets_written += 1;
                } else {
                    failed_any = true;
                }
            }
        }

        if wrote_any {
            if existed_any {
                stats.updated_logins += 1;
            } else {
                stats.new_logins += 1;
            }
        } else if failed_any {
            // Needed a write but it didn't persist — don't claim new/updated.
            stats.skipped += 1;
        } else {
            stats.unchanged_logins += 1;
        }
    }
    stats
}

/// Where to download the 1Password CLI when it isn't installed.
pub const OP_DOWNLOAD_URL: &str = "https://1password.com/downloads/command-line";

/// stdin = /dev/null so `op` can't prompt interactively (and corrupt the TUI)
/// when no account is signed in.
fn op_command(args: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new("op");
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd
}

/// Whether the 1Password CLI (`op`) is installed and runnable.
pub fn op_available() -> bool {
    op_command(&["--version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn run_op(args: &[&str]) -> Result<String> {
    let output = op_command(args)
        .output()
        .map_err(|err| anyhow!("running `op`: {err} (is the 1Password CLI installed?)"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        // Not signed in → actionable hint instead of op's interactive blurb.
        if lower.contains("no accounts")
            || lower.contains("not currently signed in")
            || lower.contains("account is not signed in")
            || lower.contains("please run 'op signin'")
        {
            bail!(
                "1Password CLI isn't signed in — run `op signin`, or set OP_SERVICE_ACCOUNT_TOKEN"
            );
        }
        bail!(
            "1Password CLI error: {}",
            stderr.trim().lines().next().unwrap_or("unknown error")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Map a single `op item get --format json` object to a normalized login.
fn login_from_op_item(item: &Value) -> Option<ImportedLogin> {
    let url = item
        .get("urls")
        .and_then(Value::as_array)
        .and_then(|urls| {
            urls.iter()
                .find_map(|u| u.get("href").and_then(Value::as_str))
        })
        .unwrap_or_default();
    let domain = normalize_domain(url);
    if domain.is_empty() {
        return None;
    }
    let mut username: Option<String> = None;
    let mut password: Option<String> = None;
    let mut otpauth: Option<String> = None;
    if let Some(fields) = item.get("fields").and_then(Value::as_array) {
        for field in fields {
            // Non-empty values only; not every login has both fields.
            let value = field
                .get("value")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty());
            match field.get("purpose").and_then(Value::as_str) {
                Some("USERNAME") if username.is_none() => {
                    username = value.map(str::to_string);
                }
                Some("PASSWORD") if password.is_none() => {
                    password = value.map(str::to_string);
                }
                _ => {}
            }
            // The TOTP seed surfaces as an `otpauth://` URI in some field's value.
            if otpauth.is_none() {
                if let Some(raw) = field.get("value").and_then(Value::as_str) {
                    if raw.starts_with("otpauth://") {
                        otpauth = Some(raw.to_string());
                    }
                }
            }
        }
    }
    // A username identical to the password isn't a real username.
    if username.is_some() && username == password {
        username = None;
    }
    // Keep OTP-only items: a 1Password entry can carry just a 2FA seed (no
    // username/password), and that seed should still import. Only skip entries
    // with nothing usable at all.
    if username.is_none() && password.is_none() && otpauth.is_none() {
        return None;
    }
    Some(ImportedLogin {
        domain,
        username,
        password,
        otpauth,
    })
}

/// Import Login items live from 1Password via the `op` CLI. Requires `op` to be
/// installed and signed in (interactive session or service account).
pub fn import_1password(store: &Store) -> Result<ImportStats> {
    if !op_available() {
        bail!("the 1Password CLI (`op`) is not installed — download it from {OP_DOWNLOAD_URL}, then run `op signin`");
    }
    let list = run_op(&["item", "list", "--categories", "Login", "--format", "json"])?;
    let items: Vec<Value> =
        serde_json::from_str(&list).map_err(|err| anyhow!("parse `op item list` output: {err}"))?;
    let mut logins = Vec::new();
    for item in &items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let detail = run_op(&["item", "get", id, "--format", "json"])?;
        let item: Value = serde_json::from_str(&detail)
            .map_err(|err| anyhow!("parse `op item get` output: {err}"))?;
        if let Some(login) = login_from_op_item(&item) {
            logins.push(login);
        }
    }
    if logins.is_empty() {
        bail!("no logins returned by 1Password (is `op` signed in? run `op signin`)");
    }
    Ok(import_logins(store, &logins))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::handlers::secrets_admin::{list_secrets, read_secret_value};

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::open(dir.path()).unwrap(), dir)
    }

    #[test]
    fn otpauth_seed_extraction() {
        let uri = "otpauth://totp/Apple:user@apple.com?secret=TESTTESTTESTTESTTESTTESTTESTTEST&issuer=Apple&period=30";
        assert_eq!(
            otpauth_seed(uri).as_deref(),
            Some("TESTTESTTESTTESTTESTTESTTESTTEST")
        );
        assert_eq!(otpauth_seed("otpauth://totp/x?secret=bad!"), None);
        assert_eq!(otpauth_seed("not a uri"), None);
    }

    #[test]
    fn login_from_op_item_maps_fields() {
        let item = serde_json::json!({
            "urls": [{"href": "https://github.com/login"}],
            "fields": [
                {"purpose": "USERNAME", "value": "me@example.com"},
                {"purpose": "PASSWORD", "value": "hunter2pass"},
                {"type": "OTP", "value": "otpauth://totp/GitHub?secret=TESTTESTTESTTESTTESTTESTTESTTEST"}
            ]
        });
        let login = login_from_op_item(&item).unwrap();
        assert_eq!(login.domain, "github.com");
        assert_eq!(login.username.as_deref(), Some("me@example.com"));
        assert_eq!(login.password.as_deref(), Some("hunter2pass"));
        assert!(login.otpauth.is_some());
    }

    #[test]
    fn password_only_login_has_no_username() {
        // Empty username field + a password → only a password, no username secret.
        let item = serde_json::json!({
            "urls": [{"href": "https://example.com/"}],
            "fields": [
                {"purpose": "USERNAME", "value": ""},
                {"purpose": "PASSWORD", "value": "s3cr3tvalue"}
            ]
        });
        let login = login_from_op_item(&item).unwrap();
        assert_eq!(login.username, None);
        assert_eq!(login.password.as_deref(), Some("s3cr3tvalue"));
    }

    #[test]
    fn otp_only_item_is_imported() {
        // A 1Password entry with only a 2FA seed (no username/password) must
        // still import its otp — not be skipped.
        let item = serde_json::json!({
            "urls": [{"href": "https://github.com/login"}],
            "fields": [
                {"type": "OTP", "value": "otpauth://totp/GitHub?secret=TESTTESTTESTTESTTESTTESTTESTTEST"}
            ]
        });
        let login = login_from_op_item(&item).expect("otp-only item should import");
        assert_eq!(login.domain, "github.com");
        assert_eq!(login.username, None);
        assert_eq!(login.password, None);
        assert!(login.otpauth.is_some());
    }

    #[test]
    fn username_equal_to_password_is_dropped() {
        let item = serde_json::json!({
            "urls": [{"href": "https://example.com/"}],
            "fields": [
                {"purpose": "USERNAME", "value": "samevalue"},
                {"purpose": "PASSWORD", "value": "samevalue"}
            ]
        });
        let login = login_from_op_item(&item).unwrap();
        assert_eq!(login.username, None);
        assert_eq!(login.password.as_deref(), Some("samevalue"));
    }

    #[test]
    fn item_with_neither_username_nor_password_is_skipped() {
        let item = serde_json::json!({
            "urls": [{"href": "https://example.com/"}],
            "fields": [
                {"purpose": "USERNAME", "value": ""},
                {"purpose": "NOTES", "value": "just a note"}
            ]
        });
        assert!(login_from_op_item(&item).is_none());
    }

    #[test]
    fn import_writes_encrypted_secrets() {
        let (store, _dir) = temp_store();
        let logins = vec![ImportedLogin {
            domain: "github.com".to_string(),
            username: Some("me@example.com".to_string()),
            password: Some("hunter2pass".to_string()),
            otpauth: Some(
                "otpauth://totp/GitHub?secret=TESTTESTTESTTESTTESTTESTTESTTEST".to_string(),
            ),
        }];
        // First sync: all new.
        let stats = import_logins(&store, &logins);
        assert_eq!(stats.new_logins, 1);
        assert_eq!(stats.updated_logins, 0);
        assert_eq!(stats.unchanged_logins, 0);
        assert_eq!(stats.secrets_written, 3); // username + password + otp

        let metas = list_secrets(&store).unwrap();
        assert_eq!(metas.len(), 3);
        assert_eq!(
            read_secret_value(&store, "github.com", "password").as_deref(),
            Some("hunter2pass")
        );
        assert!(read_secret_value(&store, "github.com", "otp").is_some());

        // Re-syncing the identical login writes nothing and reports unchanged.
        let again = import_logins(&store, &logins);
        assert_eq!(again.new_logins, 0);
        assert_eq!(again.updated_logins, 0);
        assert_eq!(again.unchanged_logins, 1);
        assert_eq!(again.secrets_written, 0);

        // Changing the password marks the login updated and writes only that one.
        let changed = vec![ImportedLogin {
            password: Some("newpass99".to_string()),
            ..logins[0].clone()
        }];
        let upd = import_logins(&store, &changed);
        assert_eq!(upd.new_logins, 0);
        assert_eq!(upd.updated_logins, 1);
        assert_eq!(upd.secrets_written, 1);
        assert_eq!(
            read_secret_value(&store, "github.com", "password").as_deref(),
            Some("newpass99")
        );
    }

    #[test]
    fn deleted_login_reimports_even_with_orphaned_value() {
        let (store, _dir) = temp_store();
        let logins = vec![ImportedLogin {
            domain: "github.com".to_string(),
            username: Some("me@example.com".to_string()),
            password: Some("hunter2pass".to_string()),
            otpauth: None,
        }];
        assert_eq!(import_logins(&store, &logins).new_logins, 1);

        // Simulate the user deleting the login from the saved-secrets list while a
        // value lingers in the encrypted store (the orphan case): drop only the
        // metadata the TUI lists from.
        for (key, _) in store.list_settings().unwrap() {
            if key.starts_with(super::super::secrets_admin::SECRETS_META_PREFIX) {
                store.delete_setting(&key).unwrap();
            }
        }
        assert!(list_secrets(&store).unwrap().is_empty());
        // Orphaned value still present — the old value-based dedup said "unchanged".
        assert!(read_secret_value(&store, "github.com", "password").is_some());

        // New behavior: not listed ⇒ treated as new and re-imported.
        let again = import_logins(&store, &logins);
        assert_eq!(again.new_logins, 1);
        assert_eq!(again.unchanged_logins, 0);
        assert!(again.secrets_written >= 1);
        assert_eq!(list_secrets(&store).unwrap().len(), 2); // username + password back
    }
}
