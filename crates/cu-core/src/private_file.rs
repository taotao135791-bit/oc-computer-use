//! Shared, hardened private-file primitives — the **single** implementation
//! of secure credential/state storage, used by both the daemon admin
//! credential and the CLI's session credentials (never two near-copies).
//!
//! The guarantees, for every call site:
//!
//! # Write path (`atomic_write_private_json`)
//!
//! 1. the parent directory must exist, be owned by the current user, not be a
//!    symlink, be a real directory, and be mode `0700` or stricter
//!    (`ensure_private_directory` creates it 0700 and re-validates);
//! 2. a symlink parked at the **target** path is refused up front — the
//!    directory has been tampered with;
//! 3. content goes to a fresh **random** temporary file in the same
//!    directory (`create_new`, so an attacker-placed file is never reused or
//!    followed; `O_NOFOLLOW` refuses opening through a symlink; mode 0600
//!    from birth);
//! 4. the JSON is written, flushed, `fsync`ed, and only then atomically
//!    `rename`d over the target — readers never see a partial file;
//! 5. the parent directory is `fsync`ed so the rename survives a crash;
//! 6. on any failure the temporary file is removed, leaving the previous
//!    file (or nothing) intact — the final path is never truncated first.
//!
//! # Read path (`read_private_json` / `validate_private_regular_file`)
//!
//! Before a byte is read, the file must: not be a symlink, be a regular file
//! (a directory/FIFO/socket is refused), be owned by the current user, have
//! no group/other permission bits (0600 or stricter), and be within the
//! caller's size bound. Nothing is silently "fixed" — a file that fails a
//! check is refused, and the caller decides what to do.
//!
//! All checks use `symlink_metadata` (never follows links), so a link parked
//! anywhere in the path resolution is caught. The checks race a concurrent
//! attacker — but the remaining window can only deliver a file that still
//! passes JSON/size/version/id checks, and the directory is 0700.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use rand::RngCore;

/// A private state file we would have written ourselves is kilobytes at most.
/// Anything larger is not one of our files and is refused, bounding a hostile
/// read of an odd file the path resolution could still hit.
pub const DEFAULT_MAX_PRIVATE_FILE_BYTES: u64 = 64 * 1024;

/// What was validated about a file before it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateFileMetadata {
    pub len: u64,
    pub uid: u32,
    /// The raw mode (type bits included, e.g. `0o100600`).
    pub mode: u32,
}

// The uid private files must belong to. A test-only seam lets the
// owner-mismatch path be exercised without root (see the module tests);
// production always returns the real effective uid.
//
// Thread-local, not process-global: the seam's only caller sets it and
// exercises the checks on the same thread, and a process-wide override
// leaked into concurrently-running tests in other modules (they serialize
// on their own locks, not ours) — e.g. `security.rs` — making them refuse
// their own freshly-created directories as "foreign-owned".
#[cfg(test)]
thread_local! {
    static UID_OVERRIDE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

fn current_uid() -> u32 {
    #[cfg(test)]
    {
        if let Some(uid) = UID_OVERRIDE.with(|c| c.get()) {
            return uid;
        }
    }
    unsafe { libc::geteuid() }
}

/// Test-only: pin the uid the owner checks compare against. A real uid
/// mismatch cannot be created without root; the seam makes the check
/// testable. `None` restores the real effective uid.
#[cfg(test)]
fn set_test_uid(uid: u32) {
    UID_OVERRIDE.with(|c| c.set(Some(uid)));
}

/// Create `path` (and its parents) as a private directory and validate it:
/// a real directory, owned by the current user, mode `0700` or stricter, not
/// a symlink. Created directories are `chmod`'d to 0700 so a default umask
/// can never leave credentials world-readable.
pub fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    // create_dir_all applies the umask; force 0700 on the directory itself.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    validate_private_directory(path)
}

/// Validate an existing directory as a private directory (see
/// [`ensure_private_directory`]). Refuses symlinks, non-directories,
/// foreign-owned directories, and directories with any group/other bits.
pub fn validate_private_directory(path: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("cannot stat private directory {}: {e}", path.display()),
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} is a symlink — refusing to store credentials through it",
                path.display()
            ),
        ));
    }
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a directory", path.display()),
        ));
    }
    if meta.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {} — refusing foreign-owned credential store",
                path.display(),
                meta.uid()
            ),
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is mode {:o} — private directories must be 0700 or stricter",
                path.display(),
                meta.permissions().mode() & 0o777
            ),
        ));
    }
    Ok(())
}

/// Cryptographically random hex suffix for a temporary file name — a temp
/// file in a 0700 dir still must never be a predictable name (an attacker
/// might pre-create it; `create_new` would then fail loudly instead of
/// silently reusing it).
fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Atomically and privately write `value` as pretty JSON to `path`.
///
/// The parent directory is created/validated 0700; the content goes to a
/// fresh random temp file (`create_new` + `O_NOFOLLOW`, mode 0600), is
/// `fsync`ed, and only then `rename`d over the target. The parent directory
/// is `fsync`ed so the rename is durable. On any failure the temp file is
/// removed and the previous state is intact. A symlink parked at the target
/// is refused (never silently replaced).
pub fn atomic_write_private_json(
    path: &Path,
    value: &impl serde::Serialize,
) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "private file path has no parent directory",
        )
    })?;
    ensure_private_directory(dir)?;

    // A symlink parked at the target means the 0700 directory has been
    // tampered with. `rename` would atomically replace the *link* itself
    // (never follow it), but silently "fixing" tampering hides the problem —
    // refuse instead, and the caller surfaces the error.
    if fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "private file target is a symlink",
        ));
    }

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".into());
    let tmp = dir.join(format!(".{file_name}.{}.tmp", random_suffix()));

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
        // Never leave a partial file or a half-written temp behind.
        let _ = fs::remove_file(&tmp);
    }
    result?;
    // fsync the directory so the rename survives a crash.
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Validate a private regular file **before** it is read: not a symlink, a
/// regular file (directories/FIFOs/sockets are refused), owned by the current
/// user, no group/other permission bits (0600 or stricter), and no larger
/// than `max_size`. Returns the validated metadata.
pub fn validate_private_regular_file(
    path: &Path,
    max_size: u64,
) -> std::io::Result<PrivateFileMetadata> {
    // symlink_metadata never follows links: a link parked here is refused,
    // and the uid/mode/len checks run on the file itself.
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} is a symlink — refusing to read through it",
                path.display()
            ),
        ));
    }
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.uid() != current_uid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {} — refusing a foreign-owned private file",
                path.display(),
                meta.uid()
            ),
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is mode {:o} — private files must be 0600 or stricter",
                path.display(),
                meta.permissions().mode() & 0o777
            ),
        ));
    }
    if meta.len() > max_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} is {} bytes (limit {max_size}) — refusing to read an oversized file",
                path.display(),
                meta.len()
            ),
        ));
    }
    Ok(PrivateFileMetadata {
        len: meta.len(),
        uid: meta.uid(),
        mode: meta.permissions().mode(),
    })
}

/// Read and deserialize a private JSON file, applying every read-side check
/// of [`validate_private_regular_file`] first.
pub fn read_private_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    max_size: u64,
) -> std::io::Result<T> {
    validate_private_regular_file(path, max_size)?;
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid JSON: {e}"),
        )
    })
}

/// Remove a private file (credentials are deleted after a successful stop /
/// graceful shutdown). `remove_file` unlinks the path itself and never
/// follows a symlink, so a link parked here would be removed, not its target.
pub fn remove_private_file(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::Mutex;

    // Tests mutate the process-wide uid override / write real files, so they
    // must never run concurrently with each other.
    static LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Fixture {
        name: String,
        count: u32,
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cu-private-file-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The test's scratch dir itself is 0700 so the child private dir
        // validates on its own merits.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn normal_write_is_private_and_parsable() {
        let _g = guard();
        let root = temp_dir("normal");
        let dir = root.join("state");
        let path = dir.join("cred.json");

        atomic_write_private_json(
            &path,
            &Fixture {
                name: "n".into(),
                count: 7,
            },
        )
        .unwrap();

        // Directory 0700, file 0600.
        let dperm = fs::symlink_metadata(&dir).unwrap().permissions();
        assert_eq!(dperm.mode() & 0o777, 0o700, "directory must be 0700");
        let fperm = fs::symlink_metadata(&path).unwrap().permissions();
        assert_eq!(fperm.mode() & 0o777, 0o600, "file must be 0600");

        // Round-trip parses; no temp files survive.
        let back: Fixture = read_private_json(&path, DEFAULT_MAX_PRIVATE_FILE_BYTES).unwrap();
        assert_eq!(
            back,
            Fixture {
                name: "n".into(),
                count: 7
            }
        );
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["cred.json".to_string()], "no temp files left");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temp_names_are_random() {
        let _g = guard();
        let a = random_suffix();
        let b = random_suffix();
        assert_ne!(a, b, "temp suffixes must never collide");
        assert_eq!(a.len(), 16, "8 random bytes → 16 hex chars");
    }

    #[test]
    fn failed_write_leaves_previous_file_intact_and_no_temp() {
        let _g = guard();
        let root = temp_dir("atomic");
        let dir = root.join("state");
        let path = dir.join("cred.json");

        atomic_write_private_json(
            &path,
            &Fixture {
                name: "keep".into(),
                count: 1,
            },
        )
        .unwrap();

        // A Serialize impl that fails mid-write — the writer gets an error
        // after the temp file was created and partly written.
        struct Failing;
        impl Serialize for Failing {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected write failure"))
            }
        }
        let err = atomic_write_private_json(&path, &Failing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);

        // The previous content is intact and no temp file was left behind.
        let back: Fixture = read_private_json(&path, DEFAULT_MAX_PRIVATE_FILE_BYTES).unwrap();
        assert_eq!(back.name, "keep");
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["cred.json".to_string()]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn symlink_at_target_is_refused() {
        let _g = guard();
        let root = temp_dir("symtarget");
        let dir = root.join("state");
        let path = dir.join("cred.json");
        atomic_write_private_json(
            &path,
            &Fixture {
                name: "a".into(),
                count: 1,
            },
        )
        .unwrap();

        // Park a link over the target pointing elsewhere.
        let victim = root.join("victim.txt");
        fs::write(&victim, "precious").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let err = atomic_write_private_json(
            &path,
            &Fixture {
                name: "b".into(),
                count: 2,
            },
        )
        .expect_err("a symlink at the target must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // The victim is untouched and the link is not silently replaced.
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn symlinked_parent_is_refused() {
        let _g = guard();
        let root = temp_dir("symparent");
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.join("linked");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(
            validate_private_directory(&link).is_err(),
            "a symlinked parent must be refused"
        );
        // Writes through the symlinked parent are refused too.
        let err = atomic_write_private_json(
            &link.join("cred.json"),
            &Fixture {
                name: "x".into(),
                count: 0,
            },
        )
        .expect_err("write through a symlinked parent must be refused");
        assert!(
            err.to_string().contains("symlink"),
            "error should name the symlink: {err}"
        );
        assert!(
            !real.join("cred.json").exists(),
            "nothing written through the link"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overly_open_permissions_are_refused() {
        let _g = guard();
        let root = temp_dir("openperms");
        let dir = root.join("state");
        let path = dir.join("cred.json");
        atomic_write_private_json(
            &path,
            &Fixture {
                name: "a".into(),
                count: 1,
            },
        )
        .unwrap();

        // A file chmod'd to 0644 is refused (the credential has "leaked").
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            validate_private_regular_file(&path, DEFAULT_MAX_PRIVATE_FILE_BYTES).is_err(),
            "0644 file must be refused"
        );
        // A directory opened up to 0755 is refused for storing anything.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            validate_private_directory(&dir).is_err(),
            "0755 directory must be refused"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn foreign_owner_is_refused() {
        let _g = guard();
        let root = temp_dir("foreign");
        let dir = root.join("state");
        let path = dir.join("cred.json");
        atomic_write_private_json(
            &path,
            &Fixture {
                name: "a".into(),
                count: 1,
            },
        )
        .unwrap();

        // A real uid mismatch cannot be created without root; the test seam
        // pins the expected uid to an impossible value to exercise the check.
        set_test_uid(99999);
        let file_err = validate_private_regular_file(&path, DEFAULT_MAX_PRIVATE_FILE_BYTES)
            .expect_err("foreign-owned file must be refused");
        assert_eq!(file_err.kind(), std::io::ErrorKind::PermissionDenied);
        let dir_err =
            validate_private_directory(&dir).expect_err("foreign-owned dir must be refused");
        assert_eq!(dir_err.kind(), std::io::ErrorKind::PermissionDenied);
        set_test_uid(unsafe { libc::geteuid() });
        assert!(validate_private_regular_file(&path, DEFAULT_MAX_PRIVATE_FILE_BYTES).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn non_regular_files_are_refused() {
        let _g = guard();
        let root = temp_dir("filetype");
        let dir = root.join("state");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        // A directory where the file should be.
        let as_dir = dir.join("cred.json");
        fs::create_dir(&as_dir).unwrap();
        assert!(
            validate_private_regular_file(&as_dir, DEFAULT_MAX_PRIVATE_FILE_BYTES).is_err(),
            "a directory must be refused"
        );

        // A FIFO.
        let fifo = dir.join("fifo.json");
        unsafe {
            assert_eq!(
                libc::mkfifo(
                    std::ffi::CString::new(fifo.to_string_lossy().as_bytes())
                        .unwrap()
                        .as_ptr(),
                    0o600
                ),
                0,
                "mkfifo failed"
            );
        }
        assert!(
            validate_private_regular_file(&fifo, DEFAULT_MAX_PRIVATE_FILE_BYTES).is_err(),
            "a FIFO must be refused"
        );

        // A Unix socket.
        let sock = dir.join("sock.json");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        assert!(
            validate_private_regular_file(&sock, DEFAULT_MAX_PRIVATE_FILE_BYTES).is_err(),
            "a socket must be refused"
        );
        drop(listener);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_files_are_refused() {
        let _g = guard();
        let root = temp_dir("oversize");
        let dir = root.join("state");
        let path = dir.join("cred.json");
        atomic_write_private_json(
            &path,
            &Fixture {
                name: "a".into(),
                count: 1,
            },
        )
        .unwrap();

        fs::write(
            &path,
            vec![b'x'; (DEFAULT_MAX_PRIVATE_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            validate_private_regular_file(&path, DEFAULT_MAX_PRIVATE_FILE_BYTES).is_err(),
            "an oversized file must be refused"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_never_follows_a_symlink() {
        let _g = guard();
        let root = temp_dir("remove");
        let dir = root.join("state");
        let path = dir.join("cred.json");
        atomic_write_private_json(
            &path,
            &Fixture {
                name: "a".into(),
                count: 1,
            },
        )
        .unwrap();

        // A link parked at the path is unlinked itself, not its target.
        let victim = root.join("victim.txt");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("link.json")).unwrap();
        remove_private_file(&dir.join("link.json")).unwrap();
        assert!(
            fs::symlink_metadata(dir.join("link.json")).is_err(),
            "link removed"
        );
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "precious",
            "victim untouched"
        );

        remove_private_file(&path).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
