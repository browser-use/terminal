//! Per-session script security: domain-scoped secrets + navigation allow/deny.
//!
//! The agent layer (which owns the SQLite metadata + the OS keychain) resolves
//! the effective policy for a session and pushes it here via
//! [`set_script_security`] before running a browser script. Two consumers read
//! it back, both keyed by the browser-script `session_id`:
//!
//! - `spawn_browser_script` bakes only the secret **metadata** (placeholder names
//!   + TOTP flags, never values) into the Python prelude. The script fetches a
//!   value on demand via the `secret` bridge request — so the OS keychain is read
//!   only when the agent is filling a field, and at most once per secret.
//! - `bridge_request_with_session` serves those lazy `secret` fetches and
//!   enforces the navigation policy on `Page.navigate`; the public script entry
//!   points redact any fetched value that flows back into model-visible output.
//!
//! This mirrors Browser Use Cloud's `sensitiveData` + `allowed_domains`, but
//! runs at the local browser-script layer so it works identically against a
//! local browser or a cloud `/browsers` session driven over CDP.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

use crate::BrowserScriptOutput;

/// Metadata about one configured secret — **no value**. Values are fetched
/// lazily from the OS keychain only when the running script actually asks for
/// them (see [`fetch_secret_for_session`]), so the keychain — and its access
/// prompt — is only touched when the agent is on the page and filling a field.
#[derive(Clone, Debug)]
pub struct ScriptSecret {
    pub domain: String,
    pub placeholder: String,
    pub is_totp: bool,
    pub allowed_domains: Vec<String>,
}

/// Effective security policy for a browser-script session (metadata + nav rules).
#[derive(Clone, Debug, Default)]
pub struct ScriptSecurity {
    pub secrets: Vec<ScriptSecret>,
    /// Navigation allow-list patterns (`example.com`, `*.example.com`). Empty
    /// means "no allow restriction".
    pub nav_allow: Vec<String>,
    /// Navigation deny-list patterns. Checked before the allow-list.
    pub nav_deny: Vec<String>,
    /// Whether an email inbox (for email-OTP 2FA) is available this session.
    pub email_available: bool,
}

impl ScriptSecurity {
    /// Metadata-only blob handed to the child over stdin: which placeholders
    /// exist per domain and whether each is a TOTP — **never the values**. The
    /// script fetches a value on demand via the `secret` bridge request. Shape:
    /// `{meta:{domain:{name:{totp:bool}}}, nav_allow:[...], nav_deny:[...]}`.
    pub(crate) fn stdin_blob(&self) -> String {
        let mut meta: Map<String, Value> = Map::new();
        for secret in &self.secrets {
            // Advertise the secret on its primary domain AND every allowed domain,
            // so the script's per-domain lookup finds it on SSO/OAuth hosts too.
            for domain in std::iter::once(&secret.domain).chain(secret.allowed_domains.iter()) {
                let entry = meta
                    .entry(domain.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(map) = entry {
                    map.insert(
                        secret.placeholder.clone(),
                        serde_json::json!({ "totp": secret.is_totp }),
                    );
                }
            }
        }
        let blob = serde_json::json!({
            "meta": Value::Object(meta),
            "nav_allow": self.nav_allow,
            "nav_deny": self.nav_deny,
            "email_available": self.email_available,
        });
        serde_json::to_string(&blob).unwrap_or_else(|_| "{}".to_string())
    }
}

fn current_unix_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|delta| delta.as_secs())
}

/// Reads a secret value (`domain`, `placeholder`) from the OS keychain. Returns
/// `None` if absent. Registered by the agent via [`set_secret_resolver`] and
/// invoked lazily by the bridge so the keychain is only read on demand.
pub type SecretResolver = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

fn resolver_slot() -> &'static Mutex<Option<SecretResolver>> {
    static SLOT: OnceLock<Mutex<Option<SecretResolver>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Register the on-demand keychain reader. Idempotent; set once by the agent.
pub fn set_secret_resolver(resolver: SecretResolver) {
    *resolver_slot().lock().expect("secret resolver poisoned") = Some(resolver);
}

/// Whether a resolver has been registered.
pub fn has_secret_resolver() -> bool {
    resolver_slot()
        .lock()
        .expect("secret resolver poisoned")
        .is_some()
}

/// Resolves an email-inbox op: `"address"` (the agent's inbox), `"inbox"` (list
/// recent messages as a JSON array string; `arg` is an optional limit), or
/// `"message"` (read one message's full body as a JSON object string; `arg` is
/// the message id). `Ok(None)` when unavailable; `Err(msg)` carries the real
/// failure (bad token, network, store lock) so it surfaces to the script instead
/// of a misleading generic message.
pub type EmailResolver =
    Arc<dyn Fn(&str, Option<&str>) -> Result<Option<String>, String> + Send + Sync>;

fn email_resolver_slot() -> &'static Mutex<Option<EmailResolver>> {
    static SLOT: OnceLock<Mutex<Option<EmailResolver>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Register the email-inbox resolver. Idempotent; set once by the agent.
pub fn set_email_resolver(resolver: EmailResolver) {
    *email_resolver_slot()
        .lock()
        .expect("email resolver poisoned") = Some(resolver);
}

/// Whether an email resolver has been registered.
pub fn has_email_resolver() -> bool {
    email_resolver_slot()
        .lock()
        .expect("email resolver poisoned")
        .is_some()
}

/// Run an email-inbox op. The agent reads inbox content directly (codes, links,
/// confirmations), so nothing here is redacted. `Err` carries the real failure
/// for the script to report.
pub(crate) fn email_for_session(op: &str, arg: Option<&str>) -> Result<Option<String>, String> {
    let resolver = match email_resolver_slot()
        .lock()
        .expect("email resolver poisoned")
        .clone()
    {
        Some(resolver) => resolver,
        None => return Ok(None),
    };
    resolver(op, arg)
}

/// Per-session cache of values already fetched this session, keyed by
/// `(domain, placeholder)`. Avoids re-reading the keychain (and re-prompting)
/// when a script asks for the same secret more than once, and drives redaction.
fn fetched_values() -> &'static Mutex<HashMap<String, HashMap<(String, String), String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, HashMap<(String, String), String>>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fetch a secret value for a running session, lazily and with caching. Validates
/// that `(domain, name)` is configured for the session before reading anything,
/// reads the keychain at most once per `(session, domain, name)`, and records the
/// value so it can be scrubbed from output.
pub(crate) fn fetch_secret_for_session(
    session_id: &str,
    domain: &str,
    name: &str,
) -> Result<String> {
    let security = script_security_for(session_id);
    let entry = security
        .secrets
        .iter()
        .find(|secret| {
            secret.placeholder == name
                && (secret.domain == domain || secret.allowed_domains.iter().any(|d| d == domain))
        })
        .ok_or_else(|| anyhow!("no secret named {name:?} is configured for {domain}"))?;

    let key = (entry.domain.clone(), name.to_string());
    if let Some(value) = fetched_values()
        .lock()
        .expect("fetched secret cache poisoned")
        .get(session_id)
        .and_then(|map| map.get(&key))
        .cloned()
    {
        return Ok(value);
    }

    let resolver = resolver_slot()
        .lock()
        .expect("secret resolver poisoned")
        .clone()
        .ok_or_else(|| anyhow!("secret resolver not configured"))?;
    let value = resolver(&entry.domain, name)
        .ok_or_else(|| anyhow!("secret {name:?} for {} not found in keychain", entry.domain))?;

    fetched_values()
        .lock()
        .expect("fetched secret cache poisoned")
        .entry(session_id.to_string())
        .or_default()
        .insert(key, value.clone());
    Ok(value)
}

/// `(value, label)` pairs to scrub from output: every value fetched this session
/// (label = placeholder), plus the live TOTP code(s) derived from any fetched
/// seed (codes rotate, so cover the adjacent windows).
fn redaction_needles_for(session_id: &str) -> Vec<(String, String)> {
    let security = script_security_for(session_id);
    let cache = fetched_values()
        .lock()
        .expect("fetched secret cache poisoned");
    let Some(session_cache) = cache.get(session_id) else {
        return Vec::new();
    };
    let mut needles: Vec<(String, String)> = Vec::new();
    for ((domain, name), value) in session_cache {
        needles.push((value.clone(), name.clone()));
        let is_totp = security
            .secrets
            .iter()
            .any(|s| &s.domain == domain && &s.placeholder == name && s.is_totp);
        if is_totp {
            if let Some(key) = browser_use_secrets::totp::base32_decode(value) {
                if key.len() >= 10 {
                    if let Some(now) = current_unix_secs() {
                        for step in [now.saturating_sub(30), now, now.saturating_add(30)] {
                            needles.push((
                                browser_use_secrets::totp::totp_at(&key, step, 30, 6),
                                name.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }
    needles
}

fn registry() -> &'static Mutex<HashMap<String, ScriptSecurity>> {
    static REG: OnceLock<Mutex<HashMap<String, ScriptSecurity>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Install (or replace) the security policy for a session. Called by the agent
/// layer before running a browser script.
pub fn set_script_security(session_id: &str, security: ScriptSecurity) {
    registry()
        .lock()
        .expect("script security registry poisoned")
        .insert(session_id.to_string(), security);
}

/// Whether a policy is already installed for this session. The agent uses this
/// to avoid re-resolving (and re-reading the OS keychain) on every script run,
/// which otherwise triggers a keychain prompt per run.
pub fn has_script_security(session_id: &str) -> bool {
    registry()
        .lock()
        .expect("script security registry poisoned")
        .contains_key(session_id)
}

/// Drop a session's policy + fetched-value cache (e.g. on disconnect, or to
/// force a re-resolve after the user changes their secrets).
pub fn clear_script_security(session_id: &str) {
    registry()
        .lock()
        .expect("script security registry poisoned")
        .remove(session_id);
    fetched_values()
        .lock()
        .expect("fetched secret cache poisoned")
        .remove(session_id);
}

pub(crate) fn script_security_for(session_id: &str) -> ScriptSecurity {
    registry()
        .lock()
        .expect("script security registry poisoned")
        .get(session_id)
        .cloned()
        .unwrap_or_default()
}

/// Extract the lowercased host from a URL, or `None` for hostless/internal URLs
/// (`about:blank`, `chrome://…`, `data:`) which are never gated.
pub(crate) fn nav_host(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, after_scheme) = url.split_once("://")?;
    // Only http(s) navigations are gated; internal schemes (chrome://,
    // devtools://, chrome-extension://, about:, data:) are never restricted.
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    // Authority ends at the first '/', '?' or '#'.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip userinfo.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, rest)| rest);
    // Strip the port, but handle a bracketed IPv6 literal (`[::1]:8080`) — its
    // host contains colons, so a naive split-on-first-colon mangles it.
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.split_once(':').map_or(host, |(host, _)| host)
    };
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

fn matches_any(host: &str, patterns: &[String]) -> bool {
    let normalized: Vec<String> = patterns
        .iter()
        .map(|pattern| {
            pattern
                .trim()
                .trim_start_matches("*.")
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|pattern| !pattern.is_empty())
        .collect();
    if normalized.is_empty() {
        return false;
    }
    // Reuse the cookie domain matcher: a bare `example.com` pattern matches the
    // apex and any subdomain.
    crate::cookie_domain_matches(host, &normalized)
}

/// Returns a human-readable reason if navigating to `url` is disallowed, or
/// `None` to allow. Deny is checked before allow; an empty allow+deny policy
/// never restricts (so existing browsing is unaffected until the user
/// configures a policy).
pub(crate) fn nav_denied_reason(url: &str, security: &ScriptSecurity) -> Option<String> {
    if security.nav_allow.is_empty() && security.nav_deny.is_empty() {
        return None;
    }
    let host = nav_host(url)?;
    if matches_any(&host, &security.nav_deny) {
        return Some(format!(
            "navigation to {host} is blocked by the user's /domains block-list. You can't change \
             this — tell the user they can unblock {host} in /domains if they want you to visit it."
        ));
    }
    if !security.nav_allow.is_empty() && !matches_any(&host, &security.nav_allow) {
        return Some(format!(
            "navigation to {host} is blocked: it isn't in the user's /domains allow-list. You can't \
             change this — tell the user they can allow {host} by running /domains if they want you \
             to visit it."
        ));
    }
    None
}

/// Replace every secret value in `text` with `<secret>LABEL</secret>` — the same
/// redaction token browser-use uses, so a leaked value reads back as the
/// placeholder the model already knows. Values shorter than 4 chars are skipped
/// to avoid pathological replacement of common substrings. Longer values are
/// replaced first so a value that contains another isn't half-scrubbed.
pub(crate) fn redact_secrets(text: &str, needles: &[(String, String)]) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let mut sorted: Vec<&(String, String)> = needles
        .iter()
        .filter(|(value, _)| value.len() >= 4)
        .collect();
    if sorted.is_empty() {
        return text.to_string();
    }
    sorted.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    let mut out = text.to_string();
    for (value, label) in sorted {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("<secret>{label}</secret>"));
        }
    }
    out
}

fn redact_value(value: &mut Value, needles: &[(String, String)]) {
    match value {
        Value::String(text) => *text = redact_secrets(text, needles),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_value(item, needles)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| redact_value(item, needles)),
        _ => {}
    }
}

/// Scrub secret values from every model-visible field of a script output.
pub(crate) fn redact_output(output: &mut BrowserScriptOutput, needles: &[(String, String)]) {
    if needles.is_empty() {
        return;
    }
    output.text = redact_secrets(&output.text, needles);
    if let Some(error) = output.error.as_mut() {
        *error = redact_secrets(error, needles);
    }
    for value in output
        .outputs
        .iter_mut()
        .chain(output.summary.iter_mut())
        .chain(output.browser_events.iter_mut())
        .chain(output.artifacts.iter_mut())
        .chain(output.images.iter_mut())
    {
        redact_value(value, needles);
    }
    redact_value(&mut output.data, needles);
    if let Some(diagnosis) = output.diagnosis.as_mut() {
        diagnosis.summary = redact_secrets(&diagnosis.summary, needles);
        diagnosis.what_happened = redact_secrets(&diagnosis.what_happened, needles);
        diagnosis.next_step = redact_secrets(&diagnosis.next_step, needles);
    }
}

/// Post-process a public script entry point's result, scrubbing any secret value
/// for `session_id` from model-visible output.
pub(crate) fn finish_with_redaction(
    session_id: &str,
    result: anyhow::Result<BrowserScriptOutput>,
) -> anyhow::Result<BrowserScriptOutput> {
    result.map(|mut output| {
        let needles = redaction_needles_for(session_id);
        redact_output(&mut output, &needles);
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn secret(domain: &str, placeholder: &str, is_totp: bool) -> ScriptSecret {
        ScriptSecret {
            domain: domain.to_string(),
            placeholder: placeholder.to_string(),
            is_totp,
            allowed_domains: Vec::new(),
        }
    }

    #[test]
    fn nav_host_parsing() {
        assert_eq!(
            nav_host("https://github.com/login").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            nav_host("https://user:pw@App.Example.com:8443/x?y#z").as_deref(),
            Some("app.example.com")
        );
        assert_eq!(nav_host("about:blank"), None);
        assert_eq!(nav_host("chrome://settings"), None);
        assert_eq!(nav_host("data:text/html,hi"), None);
    }

    #[test]
    fn empty_policy_allows_everything() {
        let security = ScriptSecurity::default();
        assert_eq!(nav_denied_reason("https://evil.test/x", &security), None);
    }

    #[test]
    fn deny_beats_allow() {
        let security = ScriptSecurity {
            secrets: vec![],
            nav_allow: vec!["example.com".to_string()],
            nav_deny: vec!["evil.example.com".to_string()],
            email_available: false,
        };
        assert!(nav_denied_reason("https://evil.example.com/x", &security).is_some());
        assert_eq!(
            nav_denied_reason("https://app.example.com/x", &security),
            None
        );
        assert!(nav_denied_reason("https://other.test/x", &security).is_some());
    }

    #[test]
    fn allow_matches_apex_and_subdomains_and_wildcards() {
        let security = ScriptSecurity {
            secrets: vec![],
            nav_allow: vec!["*.okta.com".to_string(), "github.com".to_string()],
            nav_deny: vec![],
            email_available: false,
        };
        assert_eq!(nav_denied_reason("https://github.com/x", &security), None);
        assert_eq!(
            nav_denied_reason("https://api.github.com/x", &security),
            None
        );
        assert_eq!(
            nav_denied_reason("https://acme.okta.com/x", &security),
            None
        );
        assert!(nav_denied_reason("https://evil.test/x", &security).is_some());
        // Internal pages are never gated.
        assert_eq!(nav_denied_reason("about:blank", &security), None);
    }

    #[test]
    fn redaction_replaces_values_with_labels() {
        let needles = vec![("hunter2pass".to_string(), "password".to_string())];
        assert_eq!(
            redact_secrets("logged in with hunter2pass ok", &needles),
            "logged in with <secret>password</secret> ok"
        );
        // Short values are not scrubbed.
        let short = vec![("ab".to_string(), "x".to_string())];
        assert_eq!(redact_secrets("ab cab", &short), "ab cab");
    }

    #[test]
    fn redaction_walks_output_fields() {
        let needles = vec![("s3cr3tvalue".to_string(), "password".to_string())];
        let mut output = BrowserScriptOutput {
            ok: true,
            text: "typed s3cr3tvalue".to_string(),
            error: Some("failed near s3cr3tvalue".to_string()),
            outputs: vec![json!({"echo": "s3cr3tvalue here"})],
            ..Default::default()
        };
        redact_output(&mut output, &needles);
        assert!(!output.text.contains("s3cr3tvalue"));
        assert!(!output.error.as_deref().unwrap().contains("s3cr3tvalue"));
        assert!(!output.outputs[0].to_string().contains("s3cr3tvalue"));
        assert!(output.text.contains("<secret>password</secret>"));
    }

    #[test]
    fn stdin_blob_bakes_metadata_not_values() {
        let security = ScriptSecurity {
            secrets: vec![
                secret("github.com", "password", false),
                secret("github.com", "otp", true),
            ],
            nav_allow: vec![],
            nav_deny: vec![],
            email_available: false,
        };
        let blob: Value = serde_json::from_str(&security.stdin_blob()).unwrap();
        // Only metadata (names + totp flag) is baked — never values.
        assert_eq!(blob["meta"]["github.com"]["password"]["totp"], json!(false));
        assert_eq!(blob["meta"]["github.com"]["otp"]["totp"], json!(true));
    }

    #[test]
    fn stdin_blob_advertises_secret_on_allowed_domains() {
        let security = ScriptSecurity {
            secrets: vec![ScriptSecret {
                domain: "github.com".to_string(),
                placeholder: "password".to_string(),
                is_totp: false,
                allowed_domains: vec!["sso.example.com".to_string()],
            }],
            ..Default::default()
        };
        let blob: Value = serde_json::from_str(&security.stdin_blob()).unwrap();
        // The secret is visible on its primary domain AND its allowed domain.
        assert_eq!(blob["meta"]["github.com"]["password"]["totp"], json!(false));
        assert_eq!(
            blob["meta"]["sso.example.com"]["password"]["totp"],
            json!(false)
        );
    }

    #[test]
    fn nav_host_handles_ipv6() {
        assert_eq!(nav_host("http://[::1]:8080/x").as_deref(), Some("::1"));
        assert_eq!(
            nav_host("https://[2001:db8::1]/path").as_deref(),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn redaction_covers_artifacts_and_images() {
        let needles = vec![("s3cr3tvalue".to_string(), "password".to_string())];
        let mut output = BrowserScriptOutput {
            ok: true,
            artifacts: vec![json!({ "label": "screenshot of s3cr3tvalue" })],
            images: vec![json!({ "alt": "s3cr3tvalue" })],
            ..Default::default()
        };
        redact_output(&mut output, &needles);
        assert!(!output.artifacts[0].to_string().contains("s3cr3tvalue"));
        assert!(!output.images[0].to_string().contains("s3cr3tvalue"));
    }

    #[test]
    fn lazy_fetch_caches_and_feeds_redaction() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let session = "test-session-lazy-totp";
        clear_script_security(session);
        set_script_security(
            session,
            ScriptSecurity {
                secrets: vec![secret("github.com", "otp", true)],
                nav_allow: vec![],
                nav_deny: vec![],
                email_available: false,
            },
        );
        let seed = "TESTTESTTESTTESTTESTTESTTESTTEST";
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        set_secret_resolver(Arc::new(move |_domain, _name| {
            calls2.fetch_add(1, Ordering::SeqCst);
            Some(seed.to_string())
        }));

        // First fetch reads via the resolver; the second is served from cache.
        assert_eq!(
            fetch_secret_for_session(session, "github.com", "otp").unwrap(),
            seed
        );
        assert_eq!(
            fetch_secret_for_session(session, "github.com", "otp").unwrap(),
            seed
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "keychain read at most once per session"
        );

        // An unconfigured secret is rejected (and never reaches the resolver).
        assert!(fetch_secret_for_session(session, "github.com", "nope").is_err());

        // Redaction now covers the fetched seed and its live code.
        let needles = redaction_needles_for(session);
        assert!(needles.iter().any(|(value, _)| value == seed));
        let key = browser_use_secrets::totp::base32_decode(seed).unwrap();
        let now = current_unix_secs().unwrap();
        let code = browser_use_secrets::totp::totp_at(&key, now, 30, 6);
        assert!(needles.iter().any(|(value, _)| value == &code));

        clear_script_security(session);
    }
}
