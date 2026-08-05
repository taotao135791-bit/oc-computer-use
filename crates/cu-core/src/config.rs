//! Tuning knobs for the runtime. Every threshold that can change how the
//! runtime behaves on a real desktop is centralized here and configurable
//! (defaults apply unless overridden via environment or daemon config).

/// Default retention of recently-observed frames in memory.
pub const DEFAULT_FRAME_CACHE_LIMIT: usize = 200;

/// How long a referenced frame is trusted at all, independent of visual
/// similarity. Wall-clock staleness is a backstop, never the primary signal.
pub const DEFAULT_MAX_FRAME_AGE_SECS: u64 = 120;

/// Threshold for the normalized thumbnail difference above which a referenced
/// frame is treated as stale (0..=1). Tuned for 64x64 grayscale thumbnails:
/// cursor motion and clocks score far below this; window/content swaps score
/// far above.
pub const DEFAULT_STALE_THRESHOLD: f64 = 0.12;

/// A change of active application is always treated as a stale frame, even if
/// the pixels happen to look similar.
pub const DEFAULT_APP_CHANGE_IS_STALE: bool = true;

/// Stabilizer defaults.
pub const DEFAULT_STABILIZER_INITIAL_DELAY_MS: u64 = 250;
pub const DEFAULT_STABILIZER_SAMPLE_INTERVAL_MS: u64 = 200;
pub const DEFAULT_STABILIZER_REQUIRED_STABLE_SAMPLES: u32 = 3;
pub const DEFAULT_STABILIZER_DIFFERENCE_THRESHOLD: f64 = 0.02;
pub const DEFAULT_STABILIZER_MAX_WAIT_MS: u64 = 8_000;

/// Capturing at higher than this width on every observe is wasteful for
/// model inference; observers can request more when they need it.
pub const DEFAULT_OBSERVE_MAX_WIDTH: u32 = 1440;
pub const DEFAULT_OBSERVE_FORMAT: &str = "jpeg";
pub const DEFAULT_OBSERVE_JPEG_QUALITY: u8 = 82;

/// Trace JSONL retention: files older than this are pruned on daemon start.
pub const DEFAULT_TRACE_RETENTION_DAYS: u64 = 7;

/// Default paths (overridable via `COMPUTER_USE_HOME`).
pub fn runtime_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("COMPUTER_USE_HOME") {
        return std::path::PathBuf::from(dir);
    }
    dirs::home_dir()
        .map(|h| h.join(".computer-use"))
        .unwrap_or_else(|| std::path::PathBuf::from(".computer-use"))
}

pub fn socket_path() -> std::path::PathBuf {
    runtime_dir().join("runtime.sock")
}

/// `~/.local/state/oc-computer-use` — where durable state files (the daemon
/// admin token, the CLI's session credential store) live, 0700. Kept separate
/// from `runtime_dir()` (the runtime's volatile working dir) so credentials
/// are never confused with frames/traces.
pub fn state_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".local").join("state").join("oc-computer-use"))
        .unwrap_or_else(|| std::path::PathBuf::from(".local/state/oc-computer-use"))
}

/// Path of the daemon admin token file (0600): the CLI's only way to shut the
/// daemon down gracefully. Written by the daemon at startup, removed on
/// graceful exit.
pub fn daemon_admin_path() -> std::path::PathBuf {
    state_dir().join("daemon-admin.json")
}

pub fn frames_dir() -> std::path::PathBuf {
    runtime_dir().join("frames")
}

pub fn traces_dir() -> std::path::PathBuf {
    runtime_dir().join("traces")
}

/// Path where the Swift bridge binary is expected to live.
pub fn bridge_path() -> std::path::PathBuf {
    runtime_dir().join("bin").join("cubridge")
}

/// Version of the runtime, reported by `runtime.version` and embedded in traces.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RUNTIME_NAME: &str = "computer-use";

/// Compose a monotonic-ish, collision-resistant frame id.
pub fn new_frame_id(counter: u64) -> String {
    format!("frame_{counter}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ids_are_predictable() {
        assert_eq!(new_frame_id(1), "frame_1");
        assert_eq!(new_frame_id(128), "frame_128");
    }

    #[test]
    fn runtime_dir_respects_env() {
        std::env::set_var("COMPUTER_USE_HOME", "/tmp/cu-test-home");
        assert_eq!(runtime_dir(), std::path::PathBuf::from("/tmp/cu-test-home"));
        std::env::remove_var("COMPUTER_USE_HOME");
    }
}
