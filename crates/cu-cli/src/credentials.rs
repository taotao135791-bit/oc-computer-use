//! Session credential store for the CLI.
//!
//! The daemon issues a session's capability tokens **exactly once**, in the
//! `start` response (a control token and an observation token), and stores
//! only hashes of them. The CLI keeps its copies on disk so later commands
//! can authenticate: the control token for mutating commands
//! (pause/resume/takeover/release/stop/act), the observation token for
//! sensitive reads (observe/inspect/status/trace). Knowing a session id alone
//! grants nothing — the daemon refuses every sensitive request without a
//! capability.
//!
//! Files live in `~/.local/state/oc-computer-use/credentials/<session-id>.json`
//! with mode 0600 (directory 0700), and are deleted when the session is
//! stopped. Credentials are never printed, logged, traced, or committed.
//!
//! # Write safety
//!
//! Saves are atomic: the JSON is written to a fresh temporary file in the
//! same directory (`create_new`, so an attacker-placed file is never reused
//! or followed; `O_NOFOLLOW` refuses opening through a symlink; mode 0600
//! from birth), `fsync`ed, `rename`d onto the target (which atomically
//! replaces any link *itself*, never what it points to), and then the parent
//! directory is `fsync`ed so the rename survives a crash. A symlink already
//! parked at the target is refused up front — the directory has been
//! tampered with. If any step fails, the temporary file is removed, leaving
//! the previous credential (or nothing) intact.
//!
//! # Read safety
//!
//! `load` refuses to even open a file that is a symlink, is not a regular
//! file, is owned by another user, is writable/readable beyond 0600, or is
//! larger than a credential we could ever have written. Parsed files must
//! carry a format version we understand and a `session_id` matching the
//! request, so a stray, truncated, or replayed file authenticates nothing.
//! (The checks race a concurrent attacker — but the remaining window can
//! only deliver a file that still passes JSON/size/version/id checks, and
//! the directory is 0700.)

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk credential format version. `load` refuses files claiming a newer
/// version — never read a format we don't understand (protocol compat).
const FORMAT_VERSION: u32 = 1;

/// A credential file is two tokens plus identity — kilobytes at most.
/// Anything larger is not a credential we wrote and is refused, bounding a
/// hostile read of an odd file the path resolution could still hit.
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;

/// One credential file's contents.
///
/// Token fields are typed `SecretToken`: serde-transparent, so the on-disk
/// JSON format is unchanged — but `Debug` on the struct (or anything holding
/// it) prints `[REDACTED]` and dropping a value zeroizes its buffer.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoredCredential {
    pub session_id: String,
    /// Full capability — the session's control token, issued once at start.
    pub control_token: cu_core::SecretToken,
    /// Read-only capability — the session's observation token, issued once at
    /// start (v3). Absent in pre-v3 files (`#[serde(default)]`), which can
    /// still control but cannot read.
    #[serde(default)]
    pub observation_token: cu_core::SecretToken,
    /// Identity of the CLI instance that started the session. The file is
    /// usable by any instance of the same user (credentials belong to the
    /// user, not to a process); the field exists so the file records its
    /// origin and is never empty.
    pub client_instance_id: String,
    /// UTC RFC 3339 timestamp of the start.
    pub created_at: String,
    /// On-disk format version. Absent in v1 files → 1 (compatible).
    #[serde(default = "default_format_version")]
    pub format_version: u32,
}

fn default_format_version() -> u32 {
    1
}

/// `~/.local/state/oc-computer-use/credentials`.
pub fn credentials_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| {
            h.join(".local")
                .join("state")
                .join("oc-computer-use")
                .join("credentials")
        })
        .unwrap_or_else(|| PathBuf::from(".local/state/oc-computer-use/credentials"))
}

/// File path for a session's credential. Session ids are daemon-generated
/// opaque strings; anything that could traverse directories is refused.
fn credential_path(session_id: &str) -> PathBuf {
    let safe = if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id == "."
        || session_id == ".."
    {
        "invalid"
    } else {
        session_id
    };
    credentials_dir().join(format!("{safe}.json"))
}

/// Save the tokens issued by a `start` response. The directory is created
/// 0700 and the file 0600 — readable only by the current user. See the
/// module docs for the atomic-write and symlink guarantees.
pub fn save(
    session_id: &str,
    control_token: &str,
    observation_token: &str,
    client_instance_id: &str,
    created_at: &str,
) -> std::io::Result<()> {
    let dir = credentials_dir();
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

    let cred = StoredCredential {
        session_id: session_id.to_string(),
        control_token: cu_core::SecretToken::new(control_token),
        observation_token: cu_core::SecretToken::new(observation_token),
        client_instance_id: client_instance_id.to_string(),
        created_at: created_at.to_string(),
        format_version: FORMAT_VERSION,
    };
    let path = credential_path(session_id);

    // A symlink parked at the target means the 0700 directory has been
    // tampered with. `rename` would atomically replace the *link* itself
    // (never follow it), but silently "fixing" tampering hides the problem —
    // refuse instead, and the caller surfaces the warning.
    if fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "credential target is a symlink",
        ));
    }

    atomic_write_private_json(&path, &cred)
}

/// Atomically and privately write `value` as pretty JSON to `path`.
///
/// The content goes to a fresh temporary file in the same directory
/// (`create_new` + `O_NOFOLLOW`, mode 0600), is `fsync`ed, and only then
/// `rename`d over the target — readers never see a partial credential. The
/// parent directory is `fsync`ed so the rename itself is durable. On any
/// failure the temporary file is removed and the previous state is intact.
fn atomic_write_private_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential path has no parent",
        )
    })?;
    let tmp = path.with_extension("json.tmp");
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        serde_json::to_writer_pretty(&mut f, value).map_err(std::io::Error::other)?;
        f.flush()?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // Never leave a partial credential or a half-written temp file behind.
        let _ = fs::remove_file(&tmp);
    }
    result?;
    // fsync the directory so the rename survives a crash.
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Load the credential for a session, if this CLI holds one.
///
/// The file is validated before it is read: not a symlink, a regular file,
/// owned by the current user, no wider than 0600, and small enough to be a
/// credential we wrote. Parsed content must carry a format version we
/// understand and a `session_id` matching the request.
pub fn load(session_id: &str) -> Option<StoredCredential> {
    read_validated(&credential_path(session_id), Some(session_id))
}

/// Read and validate one credential file. With `expected_session_id` the
/// session id must match the request; without it (the `all()` sweep) the
/// file must at least be internally consistent.
fn read_validated(path: &Path, expected_session_id: Option<&str>) -> Option<StoredCredential> {
    // All checks use symlink_metadata (never follows links) so a link parked
    // here is refused, and the uid/mode/len checks run on the file itself.
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None; // symlink, directory, fifo, …: not a credential
    }
    // Owned by the current user: a file planted by another user (or one made
    // world-visible) is not ours to trust.
    if meta.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    // No wider than 0600 — a credential that has been chmod'd open is a
    // credential that has already leaked; refuse to use it.
    if meta.permissions().mode() & 0o077 != 0 {
        return None;
    }
    if meta.len() > MAX_CREDENTIAL_BYTES {
        return None;
    }

    let text = fs::read_to_string(path).ok()?;
    let cred: StoredCredential = serde_json::from_str(&text).ok()?;
    if cred.format_version > FORMAT_VERSION {
        return None; // written by a newer CLI — never read what we may not understand
    }
    if cred.client_instance_id.is_empty() {
        return None;
    }
    if let Some(expected) = expected_session_id {
        if cred.session_id != expected {
            return None;
        }
    } else if cred.session_id.is_empty() {
        return None;
    }
    Some(cred)
}

/// Delete the credential for a session (after a successful `stop`).
///
/// `remove_file` unlinks the path itself and never follows a symlink, so a
/// link parked here would be removed, not its target.
pub fn delete(session_id: &str) {
    let _ = fs::remove_file(credential_path(session_id));
}

/// The read credential for a session, if this CLI holds one: its observation
/// token, or (pre-v3 files, and the read for a full-credential owner) its
/// control token — control includes observation, so either verifies server-side.
pub fn read_token(session_id: &str) -> Option<cu_core::SecretToken> {
    let cred = load(session_id)?;
    if !cred.observation_token.is_empty() {
        Some(cred.observation_token.clone())
    } else {
        Some(cred.control_token.clone())
    }
}

/// Every credential this CLI holds, in arbitrary order. Each file passes
/// the same read-side validation as `load`.
pub fn all() -> Vec<StoredCredential> {
    let Ok(entries) = fs::read_dir(credentials_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| read_validated(&e.path(), None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // Tests mutate the process-wide HOME env var (credentials_dir resolves
    // against it), so they must never run concurrently. Each test pins its own
    // temp HOME, so a panic in one test (poisoning the lock) is recoverable:
    // take the value instead of propagating the poison.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cu-credentials-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        dir
    }

    #[test]
    fn save_load_delete_round_trip() {
        let _guard = home_lock();
        let home = temp_home("roundtrip");
        let dir = credentials_dir();
        save(
            "sess-1",
            "secret-token",
            "obs-token",
            "cu-42",
            "2026-08-04T00:00:00Z",
        )
        .unwrap();

        // Directory is 0700 and the file 0600.
        let dperm = fs::metadata(&dir).unwrap().permissions();
        assert_eq!(dperm.mode() & 0o777, 0o700);
        let fperm = fs::metadata(credential_path("sess-1"))
            .unwrap()
            .permissions();
        assert_eq!(fperm.mode() & 0o777, 0o600);

        let cred = load("sess-1").unwrap();
        assert_eq!(cred.session_id, "sess-1");
        assert_eq!(cred.control_token.as_str(), "secret-token");
        assert_eq!(cred.observation_token.as_str(), "obs-token");
        assert_eq!(cred.client_instance_id, "cu-42");

        // read_token prefers the observation token; a pre-v3 file (no
        // observation_token field) falls back to the control token.
        assert_eq!(read_token("sess-1").as_deref(), Some("obs-token"));

        // Missing / mismatched ids resolve to None.
        assert!(load("sess-nope").is_none());

        delete("sess-1");
        assert!(load("sess-1").is_none());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn pre_v3_files_without_observation_token_still_load() {
        let _guard = home_lock();
        let home = temp_home("prev3");
        let dir = credentials_dir();
        fs::create_dir_all(&dir).unwrap();
        // A file written before v3 has no observation_token field. The
        // read-side checks demand 0600, so write the file private like the
        // original save() would have.
        let path = credential_path("old-sess");
        fs::write(
            &path,
            r#"{
  "session_id": "old-sess",
  "control_token": "old-control",
  "client_instance_id": "cu-1",
  "created_at": "2026-08-03T00:00:00Z"
}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let cred = load("old-sess").expect("pre-v3 file loads");
        assert_eq!(cred.observation_token.as_str(), "", "serde default");
        // Reads fall back to the control token (control includes observation).
        assert_eq!(read_token("old-sess").as_deref(), Some("old-control"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn all_lists_held_credentials() {
        let _guard = home_lock();
        let home = temp_home("all");
        save("a", "tok-a", "obs-a", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        save("b", "tok-b", "obs-b", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        let mut ids: Vec<String> = all().into_iter().map(|c| c.session_id).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn path_traversal_is_refused() {
        let _guard = home_lock();
        let home = temp_home("traversal");
        let evil = "../evil";
        // save() with a traversal id still writes (the id is daemon-generated;
        // the guard keeps load/delete from ever escaping the directory).
        save(evil, "tok", "obs", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        assert!(!credentials_dir()
            .parent()
            .unwrap()
            .join("evil.json")
            .exists());
        assert!(!credentials_dir().join("..").join("evil.json").exists());
        // The guarded id resolves to the "invalid" file, never outside.
        assert!(!credentials_dir()
            .parent()
            .unwrap()
            .join("evil.json")
            .exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn atomic_save_leaves_no_partial_files() {
        let _guard = home_lock();
        let home = temp_home("atomic");
        save("a", "tok-a", "obs-a", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        // The save is atomic: no temporary file survives, and the directory
        // contains exactly the one credential.
        let names: Vec<String> = fs::read_dir(credentials_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.json".to_string()]);
        // The written file records the current format version.
        let cred = load("a").unwrap();
        assert_eq!(cred.format_version, FORMAT_VERSION);
        assert_eq!(cred.control_token.as_str(), "tok-a");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn save_refuses_a_symlink_parked_at_the_target() {
        let _guard = home_lock();
        let home = temp_home("symsave");
        let dir = credentials_dir();
        fs::create_dir_all(&dir).unwrap();
        // A link parked at the credential path points at a file elsewhere.
        let victim = home.join("victim.txt");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, credential_path("targ")).unwrap();
        let err = save("targ", "tok", "obs", "cu-1", "2026-08-04T00:00:00Z")
            .expect_err("save refuses a symlink at the target");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // The victim is untouched, and the link is not silently replaced.
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert!(fs::symlink_metadata(credential_path("targ"))
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn load_refuses_symlinks_foreign_files_and_unexpected_content() {
        let _guard = home_lock();
        let home = temp_home("readcheck");
        fs::create_dir_all(credentials_dir()).unwrap();

        save("s", "tok", "obs", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        let genuine = credential_path("s");

        // A link parked at a credential path (even pointing at a genuine
        // file) is refused.
        let link = credential_path("linked");
        std::os::unix::fs::symlink(&genuine, &link).unwrap();
        assert!(load("linked").is_none(), "symlinked credential refused");

        // A credential whose file was chmod'd open is refused.
        fs::set_permissions(&genuine, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load("s").is_none(), "world-readable credential refused");

        // A future format version is refused (never read what we may not
        // understand); save() rewrites the file at 0600 first.
        save("s", "tok", "obs", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        let mut cred = load("s").unwrap();
        cred.format_version = FORMAT_VERSION + 1;
        fs::write(&genuine, serde_json::to_vec_pretty(&cred).unwrap()).unwrap();
        assert!(load("s").is_none(), "future format version refused");

        // An oversized file is refused (bounds a hostile read).
        fs::write(&genuine, vec![b'x'; (MAX_CREDENTIAL_BYTES + 1) as usize]).unwrap();
        assert!(load("s").is_none(), "oversized file refused");

        // A file whose session_id does not match its path is refused.
        let mismatched = serde_json::to_vec_pretty(&StoredCredential {
            session_id: "some-other-session".into(),
            control_token: cu_core::SecretToken::new("tok"),
            observation_token: cu_core::SecretToken::new("obs"),
            client_instance_id: "cu-1".into(),
            created_at: "2026-08-04T00:00:00Z".into(),
            format_version: FORMAT_VERSION,
        })
        .unwrap();
        fs::write(&genuine, mismatched).unwrap();
        assert!(load("s").is_none(), "mismatched session_id refused");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn debug_never_prints_the_tokens() {
        let _guard = home_lock();
        let home = temp_home("debug");
        save(
            "dbg",
            "plaintext-control",
            "plaintext-obs",
            "cu-1",
            "2026-08-04T00:00:00Z",
        )
        .unwrap();
        let cred = load("dbg").unwrap();
        let d = format!("{cred:?}");
        assert!(d.contains("[REDACTED]"));
        assert!(
            !d.contains("plaintext-control"),
            "Debug must not print the control token"
        );
        assert!(
            !d.contains("plaintext-obs"),
            "Debug must not print the observation token"
        );
        let _ = fs::remove_dir_all(&home);
    }
}
