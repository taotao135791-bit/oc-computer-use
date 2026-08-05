//! Trace access manifests: who may read a session's trace, persisted across
//! daemon restarts.
//!
//! A session's capability tokens are issued exactly once (in the `start`
//! response) and the daemon stores only their hashes. When the session is
//! live, those hashes authorize trace reads; after the daemon restarts, the
//! session is gone from memory but its trace files remain on disk. Without a
//! persisted record, nobody could prove they held the tokens — so every
//! started session gets a manifest next to its trace file
//! (`traces/<session_id>.manifest.json`) recording the **hashes** of the
//! tokens that may read it. Plaintext tokens never touch disk.
//!
//! Manifests are written and read through `cu_core::private_file`, the shared
//! private-file implementation: 0700 directory, 0600 atomic writes, and
//! validated reads (regular file, owner, mode, size — a symlink parked at a
//! manifest is refused, never followed).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use cu_core::private_file;
use cu_core::security::SecretTokenHash;

/// On-disk manifest format version. Reads refuse newer versions — never read
/// a format we may not understand.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// A manifest is a few token hashes plus timestamps — bytes, not megabytes.
/// Anything larger is not a manifest we wrote and is refused.
const MAX_MANIFEST_BYTES: u64 = private_file::DEFAULT_MAX_PRIVATE_FILE_BYTES;

/// One session's trace access manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceAccessManifest {
    /// On-disk format version (see [`MANIFEST_FORMAT_VERSION`]).
    pub format_version: u32,
    /// The session whose trace this manifest guards.
    pub session_id: String,
    /// Hex digests of the capability tokens that may read this trace (the
    /// control token and the observation token, both hashed — plaintext
    /// tokens are never persisted).
    pub access_token_hashes: Vec<String>,
    /// When the session started (UTC RFC 3339).
    pub created_at: DateTime<Utc>,
    /// When the session was stopped, if it has been.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
}

/// `traces/<session_id>.manifest.json`.
pub fn manifest_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.manifest.json"))
}

/// Write the manifest for a freshly started session. Called by the runtime
/// at `session_start`; a failure degrades gracefully (the live session's
/// tokens still authorize reads — only restart-persistence is lost) and is
/// surfaced as a warning by the caller.
pub fn write_manifest(
    dir: &Path,
    session_id: &str,
    control_token_hash: &SecretTokenHash,
    observation_token_hash: &SecretTokenHash,
) -> std::io::Result<()> {
    let manifest = TraceAccessManifest {
        format_version: MANIFEST_FORMAT_VERSION,
        session_id: session_id.to_string(),
        access_token_hashes: vec![control_token_hash.to_hex(), observation_token_hash.to_hex()],
        created_at: Utc::now(),
        stopped_at: None,
    };
    private_file::atomic_write_private_json(&manifest_path(dir, session_id), &manifest)
}

/// Record that the session was stopped. Reads the existing manifest (through
/// every read-side validation), stamps `stopped_at`, and atomically rewrites
/// it. A missing/invalid manifest is left untouched — the file is gone or
/// tampered with, and silently recreating it would paper over that.
pub fn mark_stopped(dir: &Path, session_id: &str) -> std::io::Result<()> {
    let path = manifest_path(dir, session_id);
    let mut manifest: TraceAccessManifest =
        private_file::read_private_json(&path, MAX_MANIFEST_BYTES)?;
    manifest.stopped_at = Some(Utc::now());
    private_file::atomic_write_private_json(&path, &manifest)
}

/// Remove a session's manifest (used when a session never actually starts,
/// e.g. the control lock is held — a never-live session must not leave an
/// access record behind). Unlinks the path itself; a link parked here is
/// removed, never followed.
pub fn remove_manifest(dir: &Path, session_id: &str) -> std::io::Result<()> {
    private_file::remove_private_file(&manifest_path(dir, session_id))
}

/// Check whether a presented observation/control token may read this
/// session's trace. Applies every read-side check to the manifest file
/// first; a symlinked, foreign-owned, oversized, or malformed manifest never
/// grants access. Returns:
///  - `Some(true)`  — a presented token matches a recorded hash
///  - `Some(false)` — the manifest exists but no presented token matches
///  - `None`        — no manifest (session never started, or tampered file)
pub fn check_access(
    dir: &Path,
    session_id: &str,
    observation_token: Option<&str>,
    control_token: Option<&str>,
) -> Option<bool> {
    let path = manifest_path(dir, session_id);
    let manifest: TraceAccessManifest =
        private_file::read_private_json(&path, MAX_MANIFEST_BYTES).ok()?;
    if manifest.format_version > MANIFEST_FORMAT_VERSION
        || manifest.session_id != session_id
        || manifest.access_token_hashes.is_empty()
    {
        return Some(false);
    }
    let presented = [control_token, observation_token]
        .into_iter()
        .flatten()
        .filter(|t| !t.is_empty());
    for token in presented {
        // Constant-time verify against every recorded hash; a malformed hash
        // entry simply never matches.
        if manifest
            .access_token_hashes
            .iter()
            .any(|h| SecretTokenHash::from_hex(h).is_some_and(|hash| hash.verify(token)))
        {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::security::{generate_control_token, generate_observation_token};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cu-trace-manifest-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_and_verify_round_trip() {
        let dir = temp_dir("roundtrip");
        let ctl = generate_control_token();
        let obs = generate_observation_token();
        let ctl_hash = SecretTokenHash::from_token(&ctl);
        let obs_hash = SecretTokenHash::from_token(&obs);
        write_manifest(&dir, "s_abc", &ctl_hash, &obs_hash).unwrap();

        // The file is a 0600 private file.
        let meta = fs::metadata(manifest_path(&dir, "s_abc")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        // Either token grants access; a stranger's token does not.
        assert_eq!(
            check_access(&dir, "s_abc", None, Some(ctl.as_str())),
            Some(true)
        );
        assert_eq!(
            check_access(&dir, "s_abc", Some(obs.as_str()), None),
            Some(true)
        );
        let stranger = generate_control_token();
        assert_eq!(
            check_access(&dir, "s_abc", Some(stranger.as_str()), None),
            Some(false)
        );
        // No token at all → denied (the manifest exists).
        assert_eq!(check_access(&dir, "s_abc", None, None), Some(false));
        // An unknown session → no manifest.
        assert_eq!(check_access(&dir, "s_nope", None, Some(ctl.as_str())), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_stopped_stamps_and_keeps_hashes() {
        let dir = temp_dir("stopped");
        let ctl = generate_control_token();
        let ctl_hash = SecretTokenHash::from_token(&ctl);
        write_manifest(
            &dir,
            "s_stop",
            &ctl_hash,
            &SecretTokenHash::from_token(&generate_observation_token()),
        )
        .unwrap();
        assert!(mark_stopped(&dir, "s_stop").is_ok());
        let manifest: TraceAccessManifest =
            private_file::read_private_json(&manifest_path(&dir, "s_stop"), MAX_MANIFEST_BYTES)
                .unwrap();
        assert!(manifest.stopped_at.is_some(), "stopped_at stamped");
        assert_eq!(manifest.access_token_hashes.len(), 2, "hashes preserved");
        // The stamped manifest still grants access.
        assert_eq!(
            check_access(&dir, "s_stop", None, Some(ctl.as_str())),
            Some(true)
        );
        // Marking an unknown session fails without creating anything.
        assert!(mark_stopped(&dir, "s_ghost").is_err());
        assert!(!manifest_path(&dir, "s_ghost").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_manifest_never_grants_access() {
        let dir = temp_dir("tamper");
        let ctl = generate_control_token();
        let ctl_hash = SecretTokenHash::from_token(&ctl);
        write_manifest(
            &dir,
            "s_x",
            &ctl_hash,
            &SecretTokenHash::from_token(&generate_observation_token()),
        )
        .unwrap();
        let path = manifest_path(&dir, "s_x");

        // A symlink parked at the manifest is refused (no follow).
        let link_dir = temp_dir("tamper-link");
        std::os::unix::fs::symlink(&path, manifest_path(&link_dir, "s_x")).unwrap();
        assert_eq!(
            check_access(&link_dir, "s_x", None, Some(ctl.as_str())),
            None,
            "a symlinked manifest must never authorize"
        );

        // World-readable manifest → refused.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            check_access(&dir, "s_x", None, Some(ctl.as_str())),
            None,
            "a wide-open manifest must never authorize"
        );

        // Garbage content → refused.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, "garbage {{{").unwrap();
        assert_eq!(
            check_access(&dir, "s_x", None, Some(ctl.as_str())),
            None,
            "a corrupt manifest must never authorize"
        );

        // A future format version → refused.
        write_manifest(
            &dir,
            "s_x",
            &ctl_hash,
            &SecretTokenHash::from_token(&generate_observation_token()),
        )
        .unwrap();
        let manifest: TraceAccessManifest =
            private_file::read_private_json(&path, MAX_MANIFEST_BYTES).unwrap();
        let mut future = manifest.clone();
        future.format_version = MANIFEST_FORMAT_VERSION + 1;
        fs::write(&path, serde_json::to_vec_pretty(&future).unwrap()).unwrap();
        assert_eq!(
            check_access(&dir, "s_x", None, Some(ctl.as_str())),
            Some(false),
            "a future-format manifest must never authorize"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&link_dir);
    }

    #[test]
    fn oversized_manifest_is_refused() {
        let dir = temp_dir("oversize");
        let ctl = generate_control_token();
        write_manifest(
            &dir,
            "s_big",
            &SecretTokenHash::from_token(&ctl),
            &SecretTokenHash::from_token(&generate_observation_token()),
        )
        .unwrap();
        let path = manifest_path(&dir, "s_big");
        fs::write(&path, vec![b'x'; (MAX_MANIFEST_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            check_access(&dir, "s_big", None, Some(ctl.as_str())),
            None,
            "an oversized manifest must never authorize"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
