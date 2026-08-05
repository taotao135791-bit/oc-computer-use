//! Capability tokens: observation, control, and daemon-admin credentials.
//!
//! Round 4 defines **three** independent capabilities:
//! - `control_token` — issued **once** at session creation; authorizes every
//!   mutating operation (act, cancel, pause, resume, takeover, release, stop)
//!   **and** doubles as an observation credential;
//! - `observation_token` — issued **once** at session creation; authorizes
//!   sensitive reads only (observe, inspect, status, trace);
//! - `daemon_admin_token` — a per-install credential that authorizes
//!   `runtime.shutdown`; only the daemon manager (CLI / LaunchAgent) holds it.
//!
//! Knowing a session id grants nothing. The daemon verifies hashes of the
//! tokens before any side effect, and never stores or logs plaintext.
//!
//! Hygiene rules enforced here:
//! - tokens are 32 random bytes from the OS CSPRNG, base64url-encoded;
//! - the runtime stores only a SHA-256 hash, never the plaintext;
//! - `Debug`/`Display` on any token-bearing type redact the value;
//! - hashes are compared without short-circuiting (constant-time-ish).

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

/// Wire protocol version. Round 4 (observation tokens, admin token, session
/// summary, protocol-version bounds) is a breaking protocol change; adapters
/// check this before talking to a daemon.
pub const PROTOCOL_VERSION: u32 = 3;

/// The lowest protocol version this daemon accepts (inclusive).
pub const MIN_CLIENT_PROTOCOL_VERSION: u32 = 3;
/// The highest protocol version this daemon accepts (inclusive).
pub const MAX_CLIENT_PROTOCOL_VERSION: u32 = 3;

/// Random bytes per token (256 bits).
const TOKEN_BYTES: usize = 32;

/// A plaintext control token. Never serialized, never logged; `Debug` prints
/// `[REDACTED]`.
#[derive(Clone, PartialEq, Eq)]
pub struct ControlToken(String);

impl ControlToken {
    /// The token as sent over the wire (base64url, no padding).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ControlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlToken([REDACTED])")
    }
}

impl std::fmt::Display for ControlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Displaying the token is as sensitive as Debug-ing it.
        f.write_str("[REDACTED]")
    }
}

/// Wipe the plaintext from memory when the token is dropped.
impl Drop for ControlToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// A plaintext observation token (read-only capability).
#[derive(Clone, PartialEq, Eq)]
pub struct ObservationToken(String);

impl ObservationToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ObservationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObservationToken([REDACTED])")
    }
}

impl std::fmt::Display for ObservationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Wipe the plaintext from memory when the token is dropped.
impl Drop for ObservationToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// A plaintext daemon-admin token (shutdown capability).
#[derive(Clone, PartialEq, Eq)]
pub struct DaemonAdminToken(String);

impl DaemonAdminToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for DaemonAdminToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DaemonAdminToken([REDACTED])")
    }
}

impl std::fmt::Display for DaemonAdminToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Wipe the plaintext from memory when the token is dropped.
impl Drop for DaemonAdminToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Generate a fresh token of `TOKEN_BYTES` random bytes (base64url, no pad).
fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a fresh control token from the operating system's CSPRNG.
/// 256 bits of entropy; tokens are never derived from time, uuids, or counters.
pub fn generate_control_token() -> ControlToken {
    ControlToken(generate_token())
}

/// Generate a fresh observation token, **independently** of any other token.
pub fn generate_observation_token() -> ObservationToken {
    ObservationToken(generate_token())
}

/// Generate a fresh daemon admin token.
pub fn generate_daemon_admin_token() -> DaemonAdminToken {
    DaemonAdminToken(generate_token())
}

/// A capability token as it crosses the wire (in request params and in the
/// one-time `start` response) and as adapters hold it.
///
/// The wire form is a plain string (`#[serde(transparent)]`), so the JSON
/// format is unchanged by the type — but every other surface is hardened:
/// - `Debug`/`Display` print `[REDACTED]` — deriving `Debug` on any struct
///   holding a `SecretToken` can no longer leak it;
/// - dropping the value zeroizes the underlying buffer;
/// - the runtime never stores plaintext — only `SecretTokenHash` (see below).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token as sent over the wire (base64url, no padding).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretToken([REDACTED])")
    }
}

impl std::fmt::Display for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::ops::Deref for SecretToken {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<String> for SecretToken {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SecretToken {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Empty default — used only as the `#[serde(default)]` of optional fields
/// (e.g. pre-v3 credential files without an observation token). An empty
/// token never verifies.
impl Default for SecretToken {
    fn default() -> Self {
        Self(String::new())
    }
}

/// Wipe the plaintext from memory when the token is dropped.
impl Drop for SecretToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Deep-copy `value` with every secret field replaced by `[REDACTED]` — the
/// one form of a JSON-RPC payload that may be logged or `Debug`-printed.
///
/// A field is a secret when its key contains `token` or `secret`
/// (`control_token`, `observation_token`, `admin_token`, `client_secret`, …).
/// String values in those fields are replaced; everything else is passed
/// through. This is the single redaction primitive: the `Debug` impls of the
/// JSON-RPC envelope and the CLI's output redaction both route through it.
pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let k_lower = k.to_ascii_lowercase();
                if (k_lower.contains("token") || k_lower.contains("secret")) && v.is_string() {
                    out.insert(k.clone(), Value::String("[REDACTED]".into()));
                } else {
                    out.insert(k.clone(), redact_json(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        other => other.clone(),
    }
}

/// A secret whose plaintext can be hashed without ever being printed.
pub trait SecretLike {
    fn as_secret_str(&self) -> &str;
}

impl SecretLike for ControlToken {
    fn as_secret_str(&self) -> &str {
        self.as_str()
    }
}
impl SecretLike for ObservationToken {
    fn as_secret_str(&self) -> &str {
        self.as_str()
    }
}
impl SecretLike for DaemonAdminToken {
    fn as_secret_str(&self) -> &str {
        self.as_str()
    }
}

/// SHA-256 hash of a secret token. This is the **only** form the runtime
/// stores; verifying a presented token hashes it and compares digests.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretTokenHash([u8; 32]);

impl SecretTokenHash {
    pub fn from_token(token: &impl SecretLike) -> Self {
        Self(Sha256::digest(token.as_secret_str().as_bytes()).into())
    }

    /// Constant-time comparison: the loop never exits early on a mismatch.
    pub fn verify(&self, presented: &str) -> bool {
        let digest = Sha256::digest(presented.as_bytes());
        let a = digest.as_slice();
        let b = &self.0;
        if a.len() != b.len() {
            return false;
        }
        let mut acc = 0u8;
        for i in 0..a.len() {
            acc |= a[i] ^ b[i];
        }
        acc == 0
    }

    /// Hex form for serialization (not currently persisted; kept for clarity).
    #[allow(dead_code)]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Debug for SecretTokenHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretTokenHash([REDACTED])")
    }
}

/// Why an admin token could not be loaded. Distinct variants so callers never
/// conflate "no daemon" with "token store broken" — a corrupt file must be
/// surfaced, never silently skipped (the daemon would then be unstoppable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminTokenFileError {
    /// No token file exists (the daemon is not running, or never started).
    Missing,
    /// The file exists but is unreadable or malformed.
    Corrupt(String),
}

impl std::fmt::Display for AdminTokenFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminTokenFileError::Missing => write!(f, "daemon admin token file not found"),
            AdminTokenFileError::Corrupt(why) => {
                write!(f, "daemon admin token file is corrupt ({why})")
            }
        }
    }
}

/// Write the daemon admin token to `~/.local/state/oc-computer-use/
/// daemon-admin.json` (state dir 0700, file 0600, fsync'd). The daemon calls
/// this at startup; a failure to persist **refuses to start** — running
/// without a stored admin token would make the daemon unstoppable.
pub fn save_daemon_admin_token(token: &DaemonAdminToken) -> std::io::Result<PathBuf> {
    save_daemon_admin_token_to(token, &crate::config::daemon_admin_path())
}

/// Same as [`save_daemon_admin_token`] but to an explicit path (the daemon
/// passes its configured `admin_token_path`; tests pass a temp file so the
/// real user state dir is never touched).
pub fn save_daemon_admin_token_to(
    token: &DaemonAdminToken,
    path: &Path,
) -> std::io::Result<PathBuf> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("admin token path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let payload = serde_json::json!({
        "admin_token": token.as_str(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer_pretty(&mut f, &payload).map_err(std::io::Error::other)?;
    f.flush()?;
    f.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path.to_path_buf())
}

/// Load the admin token the daemon persisted at startup. `Missing` when no
/// file exists; `Corrupt` when it exists but cannot be parsed — the CLI must
/// surface that loudly instead of guessing.
pub fn load_daemon_admin_token() -> Result<DaemonAdminToken, AdminTokenFileError> {
    load_daemon_admin_token_from(&crate::config::daemon_admin_path())
}

/// Same as [`load_daemon_admin_token`] but from an explicit path.
pub fn load_daemon_admin_token_from(path: &Path) -> Result<DaemonAdminToken, AdminTokenFileError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Err(AdminTokenFileError::Missing);
    };
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AdminTokenFileError::Corrupt(format!("not valid JSON: {e}")))?;
    match parsed.get("admin_token").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => Ok(DaemonAdminToken(t.to_string())),
        _ => Err(AdminTokenFileError::Corrupt(
            "missing admin_token field".into(),
        )),
    }
}

/// Remove the admin token file (graceful shutdown — the daemon is going away,
/// and a stale token would make the next stop fail confusingly).
pub fn remove_daemon_admin_token() {
    remove_daemon_admin_token_from(&crate::config::daemon_admin_path());
}

/// Same as [`remove_daemon_admin_token`] but for an explicit path.
pub fn remove_daemon_admin_token_from(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bits_of_randomness() {
        let a = generate_control_token();
        let b = generate_control_token();
        // 32 raw bytes → 43 base64url chars without padding.
        assert_eq!(a.as_str().len(), 43);
        assert_ne!(a, b, "two tokens must never collide");
    }

    #[test]
    fn every_token_kind_is_256_bits() {
        for t in [
            generate_control_token().as_str().to_string(),
            generate_observation_token().as_str().to_string(),
            generate_daemon_admin_token().as_str().to_string(),
        ] {
            assert_eq!(t.len(), 43, "token must be 256-bit base64url");
        }
    }

    #[test]
    fn observation_and_control_tokens_are_independent() {
        let control = generate_control_token();
        let observation = generate_observation_token();
        assert_ne!(
            control.as_str(),
            observation.as_str(),
            "the two session tokens must never collide"
        );
        // Neither is derivable from the other: hashing the observation token
        // must not verify against the control token's hash.
        let ctl_hash = SecretTokenHash::from_token(&control);
        assert!(!ctl_hash.verify(observation.as_str()));
    }

    #[test]
    fn hash_verifies_correct_token_and_rejects_others() {
        let token = generate_control_token();
        let hash = SecretTokenHash::from_token(&token);
        assert!(hash.verify(token.as_str()));
        assert!(!hash.verify("wrong-token"));
        assert!(!hash.verify(""));
        assert!(!hash.verify(&token.as_str()[..42]));
    }

    #[test]
    fn hash_works_for_observation_and_admin_tokens() {
        let obs = generate_observation_token();
        let admin = generate_daemon_admin_token();
        assert!(SecretTokenHash::from_token(&obs).verify(obs.as_str()));
        assert!(SecretTokenHash::from_token(&admin).verify(admin.as_str()));
    }

    #[test]
    fn hash_does_not_contain_the_token() {
        let token = generate_control_token();
        let hash = SecretTokenHash::from_token(&token);
        assert!(!hash.to_hex().contains(&token.as_str()[..8].to_string()));
    }

    #[test]
    fn debug_redacts_every_token_and_hash() {
        let control = generate_control_token();
        let obs = generate_observation_token();
        let admin = generate_daemon_admin_token();
        let hash = SecretTokenHash::from_token(&control);
        let t = format!("{control:?} {control} | {obs:?} {obs} | {admin:?} {admin}");
        let h = format!("{hash:?}");
        assert_eq!(
            t,
            "ControlToken([REDACTED]) [REDACTED] | ObservationToken([REDACTED]) [REDACTED] | DaemonAdminToken([REDACTED]) [REDACTED]"
        );
        assert_eq!(h, "SecretTokenHash([REDACTED])");
        assert!(!t.contains(control.as_str()));
        assert!(!t.contains(obs.as_str()));
        assert!(!t.contains(admin.as_str()));
        assert!(!h.contains(&hash.to_hex()));
    }

    #[test]
    fn protocol_version_is_three() {
        assert_eq!(PROTOCOL_VERSION, 3);
        assert_eq!(MIN_CLIENT_PROTOCOL_VERSION, 3);
        assert_eq!(MAX_CLIENT_PROTOCOL_VERSION, 3);
    }

    // The file store resolves against HOME, so tests that touch it must
    // redirect HOME to a temp dir and never run concurrently with each other
    // (cargo runs tests in threads; credentials.rs in cu-cli is a separate
    // process and cannot race a cu-core process).
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cu-security-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        dir
    }

    #[test]
    fn admin_token_save_load_remove_round_trip() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = temp_home("admin-roundtrip");
        let token = generate_daemon_admin_token();

        let path = save_daemon_admin_token(&token).unwrap();
        assert_eq!(path, crate::config::daemon_admin_path());

        // Directory 0700, file 0600 — the token must never be world-readable.
        let dperm = std::fs::metadata(crate::config::state_dir())
            .unwrap()
            .permissions();
        assert_eq!(dperm.mode() & 0o777, 0o700);
        let fperm = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(fperm.mode() & 0o777, 0o600);

        // The file stores the token (the daemon's only way to hand it to the
        // CLI) and never a hash.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(token.as_str()), "file must contain the token");
        assert!(text.contains("created_at"), "file records its creation");

        // Load returns the exact token; removal makes the store report Missing.
        let loaded = load_daemon_admin_token().unwrap();
        assert_eq!(loaded.as_str(), token.as_str());
        remove_daemon_admin_token();
        assert_eq!(
            load_daemon_admin_token().unwrap_err(),
            AdminTokenFileError::Missing
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn corrupt_admin_token_file_reports_corrupt_never_missing() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = temp_home("admin-corrupt");
        let path = crate::config::daemon_admin_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // Garbage that is not JSON at all.
        std::fs::write(&path, "not json {{{").unwrap();
        assert!(matches!(
            load_daemon_admin_token().unwrap_err(),
            AdminTokenFileError::Corrupt(_)
        ));

        // Valid JSON but no admin_token field — also corrupt, never Missing.
        std::fs::write(&path, r#"{"created_at": "2026-08-04T00:00:00Z"}"#).unwrap();
        assert!(matches!(
            load_daemon_admin_token().unwrap_err(),
            AdminTokenFileError::Corrupt(_)
        ));

        // Empty token — corrupt.
        std::fs::write(&path, r#"{"admin_token": ""}"#).unwrap();
        assert!(matches!(
            load_daemon_admin_token().unwrap_err(),
            AdminTokenFileError::Corrupt(_)
        ));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn admin_token_store_to_an_explicit_path_is_hermetic() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = temp_home("admin-explicit");
        let dir = std::env::temp_dir().join(format!("cu-admin-explicit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("state").join("daemon-admin.json");

        // An explicit path writes only where told — the default HOME-based
        // location must remain untouched (a real daemon run must never leave
        // a test's token in the user's real state dir).
        let token = generate_daemon_admin_token();
        save_daemon_admin_token_to(&token, &path).unwrap();
        assert!(path.exists());
        assert!(!crate::config::daemon_admin_path().exists());
        let loaded = load_daemon_admin_token_from(&path).unwrap();
        assert_eq!(loaded.as_str(), token.as_str());

        // Missing / corrupt semantics on the explicit path too.
        let missing = path.parent().unwrap().join("nope.json");
        assert_eq!(
            load_daemon_admin_token_from(&missing).unwrap_err(),
            AdminTokenFileError::Missing
        );
        std::fs::write(&path, "garbage").unwrap();
        assert!(matches!(
            load_daemon_admin_token_from(&path).unwrap_err(),
            AdminTokenFileError::Corrupt(_)
        ));
        remove_daemon_admin_token_from(&path);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn secret_token_serializes_as_plain_string_on_the_wire() {
        // The typed SecretToken must not change the JSON wire format: params
        // and the one-time start response carry tokens as plain strings.
        let token = SecretToken::new("wire-token-value");
        let json = serde_json::to_value(&token).unwrap();
        assert_eq!(json, serde_json::json!("wire-token-value"));
        let back: SecretToken = serde_json::from_value(json).unwrap();
        assert_eq!(back.as_str(), "wire-token-value");
        // Option<SecretToken> with skip_serializing_if = None behaves like a
        // plain optional string.
        let none: Option<SecretToken> = None;
        assert_eq!(serde_json::to_value(none).unwrap(), serde_json::Value::Null);
    }

    #[test]
    fn secret_token_debug_and_display_redact() {
        let token = SecretToken::new("super-secret-token-value");
        let d = format!("{token:?}");
        let p = format!("{token}");
        assert_eq!(d, "SecretToken([REDACTED])");
        assert_eq!(p, "[REDACTED]");
        assert!(!d.contains("super-secret"));
        assert!(!p.contains("super-secret"));
        // Deref exposes the plaintext only through the explicit as_str().
        assert_eq!(token.as_str(), "super-secret-token-value");
        let opt = Some(token);
        assert_eq!(opt.as_deref(), Some("super-secret-token-value"));
    }

    #[test]
    fn redact_json_hides_token_fields_at_any_depth() {
        let v = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "computer.act",
            "params": {
                "session_id": "s1",
                "control_token": "plaintext-control",
                "actions": [{ "type": "wait", "duration_ms": 1 }],
            },
            "nested": { "observation_token": "plaintext-obs", "keep": "visible" },
            "list": [{ "admin_token": "plaintext-admin" }],
        });
        let r = redact_json(&v);
        assert_eq!(r["params"]["control_token"], "[REDACTED]");
        assert_eq!(r["nested"]["observation_token"], "[REDACTED]");
        assert_eq!(r["list"][0]["admin_token"], "[REDACTED]");
        assert_eq!(
            r["params"]["session_id"], "s1",
            "non-secret fields pass through"
        );
        assert_eq!(r["nested"]["keep"], "visible");
        assert_eq!(r["method"], "computer.act");
        // The original value is untouched — redaction produces a copy.
        assert_eq!(v["params"]["control_token"], "plaintext-control");
    }

    #[test]
    fn redact_json_does_not_touch_non_string_values_or_other_keys() {
        let v = serde_json::json!({
            "control_token": 42,
            "token_count": "not-a-secret",
            "screen_token": null,
            "request_id": 7,
        });
        let r = redact_json(&v);
        // Non-string values under secret keys are passed through (a number is
        // never a token); string values under any *token/secret key are
        // redacted even when the key merely *contains* the word — the cost of
        // a false positive is a redacted log line, the cost of a false
        // negative is a leaked credential.
        assert_eq!(r["control_token"], 42, "non-string values are not redacted");
        assert_eq!(r["token_count"], "[REDACTED]");
        assert_eq!(r["screen_token"], serde_json::Value::Null);
        assert_eq!(r["request_id"], 7);
    }
}
