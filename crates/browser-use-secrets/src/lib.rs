//! Secret value storage for browser-use terminal.
//!
//! Domain-scoped credentials (passwords + TOTP seeds) mirror Browser Use
//! Cloud's `sensitiveData`: a `{ domain -> { placeholder -> value } }` shape
//! where the placeholder names are visible to the model and the values never
//! are. This crate owns only the **values**, kept in an AES-256-GCM encrypted
//! file ([`FileSecretStore`]) — no OS keychain, no access prompts, works headless
//! on every platform. The non-secret metadata (which domains/placeholders/kinds
//! exist) lives in the app's SQLite `app_settings` table, owned by
//! `browser-use-store`. The two are intentionally orthogonal so tests can use
//! [`InMemorySecretStore`] without touching disk.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

pub mod totp;

/// What a stored secret represents. Open for future `EmailCode` / `SmsCode`
/// retrieval strategies; the stored shape stays the same (a raw string keyed by
/// domain/placeholder) so adding a kind never migrates stored data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// A literal value typed into a field (password, username, API key, ...).
    Password,
    /// A base32 TOTP seed; the live 6-digit code is generated at fill time.
    Totp,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::Totp => "totp",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "password" | "secret" | "value" => Some(SecretKind::Password),
            "totp" | "otp" | "2fa" => Some(SecretKind::Totp),
            _ => None,
        }
    }
}

/// Non-secret description of a secret. Persisted (without the value) in
/// `app_settings`; the value lives in the [`SecretStore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMeta {
    /// Domain the secret belongs to (e.g. `github.com`). The model may only use
    /// the secret while the page is on a matching domain.
    pub domain: String,
    /// Placeholder name the model writes (e.g. `password`, `otp`).
    pub placeholder: String,
    /// Whether the value is a literal or a TOTP seed.
    pub kind: SecretKind,
    /// Extra domains on which this secret may also be used (SSO/OAuth redirect
    /// hosts). Empty means "only its own `domain`".
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

impl SecretMeta {
    /// The account string identifying this secret's value in the store.
    pub fn account(&self) -> String {
        account_for(&self.domain, &self.placeholder)
    }
}

/// Build the store account string for a `(domain, placeholder)` pair.
///
/// `/` is the field separator, so it (and the `\` escape char) are escaped in
/// each part. Without this, distinct pairs could collide — e.g. `("a/b", "c")`
/// and `("a", "b/c")` would both render `a/b/c`. Inputs that contain neither
/// character — normalized hostnames and validated placeholders — are emitted
/// unchanged, so account strings already persisted on disk stay valid.
pub fn account_for(domain: &str, placeholder: &str) -> String {
    fn escape(part: &str) -> String {
        part.replace('\\', "\\\\").replace('/', "\\/")
    }
    format!("{}/{}", escape(domain), escape(placeholder))
}

/// Errors from a [`SecretStore`].
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret storage error: {0}")]
    Storage(String),
}

pub type SecretResult<T> = std::result::Result<T, SecretError>;

/// Storage backend for secret **values** only.
pub trait SecretStore: Send + Sync {
    /// Store (or overwrite) the value for `meta`'s domain/placeholder.
    fn put(&self, meta: &SecretMeta, value: &str) -> SecretResult<()>;
    /// Fetch the value for a domain/placeholder; `None` if absent.
    fn get(&self, domain: &str, placeholder: &str) -> SecretResult<Option<String>>;
    /// Remove the value for a domain/placeholder; absent is not an error.
    fn delete(&self, domain: &str, placeholder: &str) -> SecretResult<()>;
}

/// In-memory backend for tests. Never touches disk.
#[derive(Default)]
pub struct InMemorySecretStore {
    values: Mutex<HashMap<String, String>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for InMemorySecretStore {
    fn put(&self, meta: &SecretMeta, value: &str) -> SecretResult<()> {
        self.values
            .lock()
            .expect("secret store mutex poisoned")
            .insert(meta.account(), value.to_string());
        Ok(())
    }

    fn get(&self, domain: &str, placeholder: &str) -> SecretResult<Option<String>> {
        Ok(self
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .get(&account_for(domain, placeholder))
            .cloned())
    }

    fn delete(&self, domain: &str, placeholder: &str) -> SecretResult<()> {
        self.values
            .lock()
            .expect("secret store mutex poisoned")
            .remove(&account_for(domain, placeholder));
        Ok(())
    }
}

/// Encrypted-file secret store: an AES-256-GCM blob at
/// `<state_dir>/secrets-encrypted.json` with a random key at
/// `<state_dir>/secrets.key` (created `0600`). This is the only secret-value
/// store — encrypted at rest, no OS prompts, works headless on every platform.
pub struct FileSecretStore {
    dir: std::path::PathBuf,
}

/// Serializes the load→modify→save sequence in `put`/`delete`. A fresh
/// `FileSecretStore` is created per call (so a per-instance lock wouldn't help),
/// and every instance points at the same on-disk file, so one process-wide lock
/// prevents concurrent writers from clobbering each other's updates. (`save_map`
/// writes via a temp file + atomic rename, so reads never need it.)
fn file_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl FileSecretStore {
    pub fn new(state_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            dir: state_dir.into(),
        }
    }

    fn key_path(&self) -> std::path::PathBuf {
        self.dir.join("secrets.key")
    }

    fn data_path(&self) -> std::path::PathBuf {
        self.dir.join("secrets-encrypted.json")
    }

    fn load_or_create_key(&self) -> SecretResult<[u8; 32]> {
        match std::fs::read(self.key_path()) {
            Ok(bytes) => {
                // A present-but-wrong-size key would decrypt nothing. Refuse to
                // regenerate — a fresh key would orphan every stored secret.
                if bytes.len() != 32 {
                    return Err(SecretError::Storage(format!(
                        "key file {} is corrupt ({} bytes, expected 32); not regenerating",
                        self.key_path().display(),
                        bytes.len()
                    )));
                }
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                Ok(key)
            }
            // First use: generate and persist a fresh key.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                use rand::RngCore;
                let mut key = [0u8; 32];
                rand::rng().fill_bytes(&mut key);
                std::fs::create_dir_all(&self.dir)
                    .map_err(|err| SecretError::Storage(format!("create state dir: {err}")))?;
                write_private(&self.key_path(), &key)?;
                Ok(key)
            }
            Err(err) => Err(SecretError::Storage(format!("read key file: {err}"))),
        }
    }

    fn load_map(&self) -> SecretResult<HashMap<String, String>> {
        match std::fs::read(self.data_path()) {
            // Corrupt JSON must NOT become an empty map — a later write would
            // then wipe every stored secret.
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| {
                SecretError::Storage(format!(
                    "secrets file {} is corrupt: {err}; refusing to overwrite",
                    self.data_path().display()
                ))
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(err) => Err(SecretError::Storage(format!("read secrets file: {err}"))),
        }
    }

    fn save_map(&self, map: &HashMap<String, String>) -> SecretResult<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|err| SecretError::Storage(format!("create state dir: {err}")))?;
        let bytes = serde_json::to_vec_pretty(map)
            .map_err(|err| SecretError::Storage(format!("serialize secrets: {err}")))?;
        write_private(&self.data_path(), &bytes)
    }

    fn cipher(&self) -> SecretResult<aes_gcm::Aes256Gcm> {
        use aes_gcm::{Aes256Gcm, KeyInit};
        let key = self.load_or_create_key()?;
        Ok(Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(&key)))
    }
}

/// Write a file readable only by the owner. On unix the temp file is *created*
/// with `0600` (no world-readable window), and a failure to do so aborts the
/// write rather than persisting a secret with broad permissions.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> SecretResult<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|err| SecretError::Storage(format!("open {}: {err}", tmp.display())))?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(&tmp)
            .map_err(|err| SecretError::Storage(format!("open {}: {err}", tmp.display())))?;
        file.write_all(bytes)
            .map_err(|err| SecretError::Storage(format!("write {}: {err}", tmp.display())))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|err| SecretError::Storage(format!("finalize {}: {err}", path.display())))
}

impl SecretStore for FileSecretStore {
    fn put(&self, meta: &SecretMeta, value: &str) -> SecretResult<()> {
        use aes_gcm::aead::Aead;
        use rand::RngCore;
        // Hold the lock across key-creation, encryption, and load→modify→save.
        // It must cover `cipher()` too: `load_or_create_key` is itself a racy
        // check-then-create, so concurrent first writers could otherwise mint
        // different keys and encrypt secrets under a key that's then overwritten.
        let _guard = file_write_lock().lock().unwrap_or_else(|e| e.into_inner());
        let cipher = self.cipher()?;
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(aes_gcm::Nonce::from_slice(&nonce), value.as_bytes())
            .map_err(|err| SecretError::Storage(format!("encrypt: {err}")))?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ciphertext);
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&blob);
        let mut map = self.load_map()?;
        map.insert(meta.account(), encoded);
        self.save_map(&map)
    }

    fn get(&self, domain: &str, placeholder: &str) -> SecretResult<Option<String>> {
        let map = self.load_map()?;
        let Some(encoded) = map.get(&account_for(domain, placeholder)) else {
            return Ok(None);
        };
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|err| SecretError::Storage(format!("decode: {err}")))?;
        if blob.len() < 12 {
            return Err(SecretError::Storage("corrupt secret blob".to_string()));
        }
        let (nonce, ciphertext) = blob.split_at(12);
        use aes_gcm::aead::Aead;
        let plaintext = self
            .cipher()?
            .decrypt(aes_gcm::Nonce::from_slice(nonce), ciphertext)
            .map_err(|err| SecretError::Storage(format!("decrypt: {err}")))?;
        Ok(Some(String::from_utf8(plaintext).map_err(|err| {
            SecretError::Storage(format!("utf8: {err}"))
        })?))
    }

    fn delete(&self, domain: &str, placeholder: &str) -> SecretResult<()> {
        // Same load→modify→save lock as `put` (see `file_write_lock`).
        let _guard = file_write_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut map = self.load_map()?;
        if map.remove(&account_for(domain, placeholder)).is_some() {
            self.save_map(&map)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(domain: &str, placeholder: &str, kind: SecretKind) -> SecretMeta {
        SecretMeta {
            domain: domain.to_string(),
            placeholder: placeholder.to_string(),
            kind,
            allowed_domains: Vec::new(),
        }
    }

    #[test]
    fn file_store_round_trip_and_encrypts_at_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path());
        assert_eq!(store.get("github.com", "password").unwrap(), None);

        store
            .put(
                &meta("github.com", "password", SecretKind::Password),
                "hunter2pass",
            )
            .unwrap();
        assert_eq!(
            store.get("github.com", "password").unwrap().as_deref(),
            Some("hunter2pass")
        );
        // The value is encrypted at rest — never plaintext on disk.
        let on_disk = std::fs::read_to_string(dir.path().join("secrets-encrypted.json")).unwrap();
        assert!(!on_disk.contains("hunter2pass"));

        // Overwrite + delete.
        store
            .put(
                &meta("github.com", "password", SecretKind::Password),
                "hunter3",
            )
            .unwrap();
        assert_eq!(
            store.get("github.com", "password").unwrap().as_deref(),
            Some("hunter3")
        );
        store.delete("github.com", "password").unwrap();
        assert_eq!(store.get("github.com", "password").unwrap(), None);
    }

    #[test]
    fn corrupt_secrets_file_errors_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path());
        store
            .put(
                &meta("github.com", "password", SecretKind::Password),
                "keepme",
            )
            .unwrap();

        // Corrupt the encrypted store.
        let data = dir.path().join("secrets-encrypted.json");
        std::fs::write(&data, b"not json {{{").unwrap();

        // Reads/writes error rather than silently returning empty + clobbering.
        assert!(store.get("github.com", "password").is_err());
        assert!(store
            .put(&meta("x.com", "password", SecretKind::Password), "new")
            .is_err());
        // The corrupt file is left intact (not wiped).
        assert_eq!(std::fs::read(&data).unwrap(), b"not json {{{");
    }

    #[test]
    fn corrupt_key_file_is_not_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path());
        store
            .put(
                &meta("github.com", "password", SecretKind::Password),
                "keepme",
            )
            .unwrap();
        // Truncate the key → must error, not silently mint a new key.
        std::fs::write(dir.path().join("secrets.key"), b"short").unwrap();
        assert!(store.get("github.com", "password").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn secret_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path());
        store
            .put(&meta("github.com", "password", SecretKind::Password), "pw")
            .unwrap();
        for name in ["secrets.key", "secrets-encrypted.json"] {
            let mode = std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name} perms");
        }
    }

    #[test]
    fn in_memory_round_trip() {
        let store = InMemorySecretStore::new();
        assert_eq!(store.get("github.com", "password").unwrap(), None);

        store
            .put(
                &meta("github.com", "password", SecretKind::Password),
                "hunter2",
            )
            .unwrap();
        assert_eq!(
            store.get("github.com", "password").unwrap().as_deref(),
            Some("hunter2")
        );

        // Overwrite.
        store
            .put(
                &meta("github.com", "password", SecretKind::Password),
                "hunter3",
            )
            .unwrap();
        assert_eq!(
            store.get("github.com", "password").unwrap().as_deref(),
            Some("hunter3")
        );

        store.delete("github.com", "password").unwrap();
        assert_eq!(store.get("github.com", "password").unwrap(), None);
        // Deleting a missing entry is fine.
        store.delete("github.com", "password").unwrap();
    }

    #[test]
    fn accounts_are_domain_scoped() {
        let store = InMemorySecretStore::new();
        store
            .put(&meta("a.com", "password", SecretKind::Password), "a-secret")
            .unwrap();
        store
            .put(&meta("b.com", "password", SecretKind::Password), "b-secret")
            .unwrap();
        assert_eq!(
            store.get("a.com", "password").unwrap().as_deref(),
            Some("a-secret")
        );
        assert_eq!(
            store.get("b.com", "password").unwrap().as_deref(),
            Some("b-secret")
        );
    }

    #[test]
    fn kind_parse_and_str_round_trip() {
        assert_eq!(SecretKind::parse("password"), Some(SecretKind::Password));
        assert_eq!(SecretKind::parse("TOTP"), Some(SecretKind::Totp));
        assert_eq!(SecretKind::parse("otp"), Some(SecretKind::Totp));
        assert_eq!(SecretKind::parse("nonsense"), None);
        assert_eq!(SecretKind::Password.as_str(), "password");
        assert_eq!(SecretKind::Totp.as_str(), "totp");
    }

    #[test]
    fn account_keys_cannot_collide_across_pairs() {
        // A slash in either part must not let two distinct pairs share a key.
        assert_ne!(account_for("a/b", "c"), account_for("a", "b/c"));
        assert_ne!(account_for("a", "b"), account_for("a\\", "b"));
        // Inputs without `/` or `\` are emitted unchanged, so keys already
        // persisted on disk stay valid (backward compatibility).
        assert_eq!(account_for("github.com", "password"), "github.com/password");
        assert_eq!(
            account_for("\u{1}agentmail", "token"),
            "\u{1}agentmail/token"
        );

        // And the store actually keeps such pairs separate end-to-end.
        let store = InMemorySecretStore::new();
        store
            .put(&meta("a/b", "c", SecretKind::Password), "first")
            .unwrap();
        store
            .put(&meta("a", "b/c", SecretKind::Password), "second")
            .unwrap();
        assert_eq!(store.get("a/b", "c").unwrap().as_deref(), Some("first"));
        assert_eq!(store.get("a", "b/c").unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn concurrent_writes_do_not_drop_secrets() {
        // Regression: load→modify→save must be serialized, or parallel writers
        // overwrite each other's entries. Each thread makes its own store
        // (mirrors the per-call `value_store(...)`), all against one file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = FileSecretStore::new(&path);
                    store
                        .put(
                            &meta("example.com", &format!("k{i}"), SecretKind::Password),
                            &format!("v{i}"),
                        )
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let store = FileSecretStore::new(&path);
        for i in 0..16 {
            assert_eq!(
                store
                    .get("example.com", &format!("k{i}"))
                    .unwrap()
                    .as_deref(),
                Some(format!("v{i}").as_str()),
                "secret k{i} was dropped by a concurrent write"
            );
        }
    }
}
