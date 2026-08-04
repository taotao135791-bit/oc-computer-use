//! Session credential store for the CLI.
//!
//! The daemon issues a session's control token **exactly once**, in the
//! `start` response, and stores only a hash of it. The CLI keeps its copy on
//! disk so later mutating commands (pause/resume/takeover/release/stop/act)
//! can authenticate: the token is the capability — knowing a session id alone
//! grants nothing, and the daemon refuses every mutating request without it.
//!
//! Files live in `~/.local/state/oc-computer-use/credentials/<session-id>.json`
//! with mode 0600 (directory 0700), and are deleted when the session is
//! stopped. Credentials are never printed, logged, traced, or committed.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One credential file's contents.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoredCredential {
    pub session_id: String,
    pub control_token: String,
    /// Identity of the CLI instance that started the session.
    pub client_instance_id: String,
    /// UTC RFC 3339 timestamp of the start.
    pub created_at: String,
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

/// Save the token issued by a `start` response. The directory is created
/// 0700 and the file 0600 — readable only by the current user.
pub fn save(
    session_id: &str,
    control_token: &str,
    client_instance_id: &str,
    created_at: &str,
) -> std::io::Result<()> {
    let dir = credentials_dir();
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;

    let cred = StoredCredential {
        session_id: session_id.to_string(),
        control_token: control_token.to_string(),
        client_instance_id: client_instance_id.to_string(),
        created_at: created_at.to_string(),
    };
    let path = credential_path(session_id);
    // Open with mode 0600 (the umask can only mask bits off, and we enforce
    // the final mode explicitly below anyway).
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    serde_json::to_writer_pretty(&mut f, &cred).map_err(std::io::Error::other)?;
    f.flush()?;
    f.sync_all()?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Load the credential for a session, if this CLI holds one.
pub fn load(session_id: &str) -> Option<StoredCredential> {
    let path = credential_path(session_id);
    if !path.exists() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let cred: StoredCredential = serde_json::from_str(&text).ok()?;
    if cred.session_id != session_id {
        return None;
    }
    Some(cred)
}

/// Delete the credential for a session (after a successful `stop`).
pub fn delete(session_id: &str) {
    let _ = fs::remove_file(credential_path(session_id));
}

/// Every credential this CLI holds, in arbitrary order.
pub fn all() -> Vec<StoredCredential> {
    let Ok(entries) = fs::read_dir(credentials_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let cred: StoredCredential = serde_json::from_str(&text).ok()?;
            Some(cred)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // Tests mutate the process-wide HOME env var (credentials_dir resolves
    // against it), so they must never run concurrently.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _guard = HOME_LOCK.lock().unwrap();
        let home = temp_home("roundtrip");
        let dir = credentials_dir();
        save("sess-1", "secret-token", "cu-42", "2026-08-04T00:00:00Z").unwrap();

        // Directory is 0700 and the file 0600.
        let dperm = fs::metadata(&dir).unwrap().permissions();
        assert_eq!(dperm.mode() & 0o777, 0o700);
        let fperm = fs::metadata(credential_path("sess-1"))
            .unwrap()
            .permissions();
        assert_eq!(fperm.mode() & 0o777, 0o600);

        let cred = load("sess-1").unwrap();
        assert_eq!(cred.session_id, "sess-1");
        assert_eq!(cred.control_token, "secret-token");
        assert_eq!(cred.client_instance_id, "cu-42");

        // Missing / mismatched ids resolve to None.
        assert!(load("sess-nope").is_none());

        delete("sess-1");
        assert!(load("sess-1").is_none());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn all_lists_held_credentials() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = temp_home("all");
        save("a", "tok-a", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        save("b", "tok-b", "cu-1", "2026-08-04T00:00:00Z").unwrap();
        let mut ids: Vec<String> = all().into_iter().map(|c| c.session_id).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn path_traversal_is_refused() {
        let _guard = HOME_LOCK.lock().unwrap();
        let home = temp_home("traversal");
        let evil = "../evil";
        // save() with a traversal id still writes (the id is daemon-generated;
        // the guard keeps load/delete from ever escaping the directory).
        save(evil, "tok", "cu-1", "2026-08-04T00:00:00Z").unwrap();
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
}
