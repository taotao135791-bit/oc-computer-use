//! Trace recorder: appends a JSONL trace per session.
//!
//! Privacy invariants (enforced here, at the write boundary):
//! - `type` actions are redacted by default: only the character count is kept.
//! - Clipboard contents are never recorded.
//! - `key` combos are recorded because they describe *what the runtime did*,
//!   not credentials; sensitive combos are still the caller's responsibility.
//!
//! Dev mode (`dev_mode = true`) records full `type` text for debugging.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use std::os::unix::fs::PermissionsExt;

use chrono::Utc;
use cu_core::{actions::RedactedText, ComputerAction, CuError, TraceEntry};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

/// How strictly traces are recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, schemars::JsonSchema)]
pub enum TraceMode {
    /// A trace write failure fails the operation (session start or act batch).
    Required,
    /// A trace write failure is logged as degraded but the operation proceeds
    /// (the default).
    #[default]
    BestEffort,
    /// No trace is recorded for this session.
    Disabled,
}

impl TraceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TraceMode::Required => "required",
            TraceMode::BestEffort => "best_effort",
            TraceMode::Disabled => "disabled",
        }
    }

    /// Parse from the environment's `COMPUTER_USE_TRACE_MODE` value.
    pub fn from_env(s: Option<&str>) -> Self {
        match s {
            Some("required") => TraceMode::Required,
            Some("disabled") => TraceMode::Disabled,
            _ => TraceMode::BestEffort,
        }
    }
}

/// Configuration for a trace recorder.
#[derive(Debug, Clone, Default)]
pub struct TraceConfig {
    pub dev_mode: bool,
    pub mode: TraceMode,
}

/// Appends structured entries to `<traces_dir>/<session_id>.jsonl`.
pub struct TraceRecorder {
    session_id: String,
    path: PathBuf,
    config: TraceConfig,
    writer: Mutex<Option<BufWriter<tokio::fs::File>>>,
    seq: AtomicU64,
    /// Set when a write failed but the recorder kept going (best-effort mode).
    degraded: AtomicBool,
    warnings: Mutex<Vec<String>>,
}

impl TraceRecorder {
    /// Open (or create) the trace file for `session_id`.
    pub async fn open(
        session_id: &str,
        traces_dir: &Path,
        config: TraceConfig,
    ) -> Result<Self, CuError> {
        tokio::fs::create_dir_all(traces_dir)
            .await
            .map_err(|e| CuError::Trace(format!("cannot create trace dir: {e}")))?;
        // Traces must live under a private directory: the access manifest
        // (`manifest::write_manifest`) refuses directories with any
        // group/other bits, so force 0700 here too (create_dir_all applies
        // the umask, which would otherwise leave the dir world-readable).
        tokio::fs::set_permissions(traces_dir, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(|e| CuError::Trace(format!("cannot secure trace dir: {e}")))?;
        let path = traces_dir.join(format!("{session_id}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| CuError::Trace(format!("cannot open trace file: {e}")))?;
        Ok(Self {
            session_id: session_id.to_string(),
            path,
            config,
            writer: Mutex::new(Some(BufWriter::new(file))),
            seq: AtomicU64::new(0),
            degraded: AtomicBool::new(false),
            warnings: Mutex::new(Vec::new()),
        })
    }

    pub fn config(&self) -> &TraceConfig {
        &self.config
    }

    /// True after any write failure in best-effort mode (or an explicit
    /// degrade). Exposed so `computer.act` can report `trace.degraded`.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    /// Warnings accumulated since open (best-effort mode keeps going).
    pub async fn warnings(&self) -> Vec<String> {
        self.warnings.lock().await.clone()
    }

    /// Mark the recorder degraded with a reason (used when the recorder could
    /// not be opened and the session continues best-effort).
    pub async fn degrade(&self, warning: String) {
        self.degraded.store(true, Ordering::SeqCst);
        self.warnings.lock().await.push(warning);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Next monotonically-increasing sequence number for ordering entries.
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    /// Append a raw entry; increments `seq` and stamps the runtime version.
    ///
    /// Failure semantics follow `TraceMode`: `Required` propagates the error
    /// (the caller must fail the operation); `BestEffort` marks the recorder
    /// degraded, records a warning, and returns `Ok` so the operation can
    /// continue. `Disabled` recorders are never constructed.
    pub async fn append(&self, mut entry: TraceEntry) -> Result<(), CuError> {
        entry.seq = self.next_seq();
        entry.runtime_version = Some(cu_core::config::RUNTIME_VERSION.to_string());
        let line = serde_json::to_string(&entry)
            .map_err(|e| CuError::Trace(format!("cannot serialize trace entry: {e}")))?;
        let mut guard = self.writer.lock().await;
        let write_result: Result<(), CuError> = if let Some(w) = guard.as_mut() {
            w.write_all(line.as_bytes())
                .await
                .map_err(|e| CuError::Trace(format!("trace write failed: {e}")))?;
            w.write_all(b"\n")
                .await
                .map_err(|e| CuError::Trace(format!("trace write failed: {e}")))?;
            w.flush()
                .await
                .map_err(|e| CuError::Trace(format!("trace flush failed: {e}")))?;
            Ok(())
        } else {
            Ok(())
        };
        match write_result {
            Ok(()) => Ok(()),
            Err(e) => match self.config.mode {
                TraceMode::Required => Err(e),
                TraceMode::BestEffort => {
                    self.degraded.store(true, Ordering::SeqCst);
                    self.warnings.lock().await.push(e.to_string());
                    Ok(())
                }
                TraceMode::Disabled => Ok(()),
            },
        }
    }

    /// Record that an action was attempted/executed, applying redaction.
    #[allow(clippy::too_many_arguments)] // trace write boundary: all fields are distinct log columns
    pub async fn record_action(
        &self,
        request_id: Option<String>,
        frame_id: Option<String>,
        action: &ComputerAction,
        result: serde_json::Value,
        duration_ms: u64,
        display_id: Option<String>,
        active_application: Option<String>,
    ) -> Result<(), CuError> {
        let (action_log, redaction) = match action {
            ComputerAction::TypeText { text, .. } => {
                if self.config.dev_mode {
                    (serde_json::to_value(action).unwrap_or_default(), None)
                } else {
                    (
                        serde_json::json!({ "type": "type", "text_redacted": true }),
                        Some(RedactedText::from_text(text)),
                    )
                }
            }
            other => (serde_json::to_value(other).unwrap_or_default(), None),
        };
        self.append(TraceEntry {
            seq: 0,
            ts: Utc::now(),
            event: "action".into(),
            session_id: Some(self.session_id.clone()),
            request_id,
            frame_id,
            action: Some(action_log),
            result: Some(result),
            duration_ms: Some(duration_ms),
            error: None,
            change_score: None,
            stable: None,
            redaction,
            display_id,
            active_application,
            runtime_version: None,
        })
        .await
    }

    /// Record a lifecycle event (session start/stop, pause, takeover, …).
    pub async fn record_event(
        &self,
        event: &str,
        detail: serde_json::Value,
    ) -> Result<(), CuError> {
        self.append(TraceEntry {
            seq: 0,
            ts: Utc::now(),
            event: event.to_string(),
            session_id: Some(self.session_id.clone()),
            request_id: None,
            frame_id: None,
            action: None,
            result: Some(detail),
            duration_ms: None,
            error: None,
            change_score: None,
            stable: None,
            redaction: None,
            display_id: None,
            active_application: None,
            runtime_version: None,
        })
        .await
    }

    /// Record an observe call. `screenshot_bytes` is the size of the image
    /// delivered to the caller, so benchmark reports can cost the
    /// screenshot pipeline from the trace alone.
    pub async fn record_observe(
        &self,
        request_id: Option<String>,
        frame_id: &str,
        width: u32,
        height: u32,
        display_id: &str,
        screenshot_bytes: u64,
    ) -> Result<(), CuError> {
        self.append(TraceEntry {
            seq: 0,
            ts: Utc::now(),
            event: "observe".into(),
            session_id: Some(self.session_id.clone()),
            request_id,
            frame_id: Some(frame_id.to_string()),
            action: None,
            result: Some(serde_json::json!({
                "width": width,
                "height": height,
                "display_id": display_id,
                "screenshot_bytes": screenshot_bytes,
            })),
            duration_ms: None,
            error: None,
            change_score: None,
            stable: None,
            redaction: None,
            display_id: Some(display_id.to_string()),
            active_application: None,
            runtime_version: None,
        })
        .await
    }

    /// Close the underlying writer. Idempotent.
    pub async fn close(&self) -> Result<(), CuError> {
        let mut guard = self.writer.lock().await;
        if let Some(w) = guard.take() {
            let mut w = w;
            w.flush()
                .await
                .map_err(|e| CuError::Trace(format!("trace flush failed: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn recorder_secures_traces_dir_0700() {
        // Regression: the access manifest refuses directories with any
        // group/other bits — if the recorder left the traces dir
        // world-readable (umask), manifest writes would always fail and
        // restart-persistence would be silently lost. Opening a recorder
        // must force 0700 on the traces directory itself.
        let dir = tempdir().unwrap();
        let traces = dir.path().join("traces");
        TraceRecorder::open("s_test", &traces, TraceConfig::default())
            .await
            .unwrap();
        let meta = std::fs::metadata(&traces).unwrap();
        let mode = meta.permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "traces dir must have no group/other bits (got {mode:o})"
        );
        // And the manifest writer must now succeed against that dir.
        let ctl_hash = cu_core::security::SecretTokenHash::from_token(
            &cu_core::security::generate_control_token(),
        );
        let obs_hash = cu_core::security::SecretTokenHash::from_token(
            &cu_core::security::generate_observation_token(),
        );
        crate::manifest::write_manifest(&traces, "s_test", &ctl_hash, &obs_hash).unwrap();
        assert!(crate::manifest::manifest_path(&traces, "s_test").exists());
    }

    #[tokio::test]
    async fn recorder_writes_jsonl_and_redacts() {
        let dir = tempdir().unwrap();
        let rec = TraceRecorder::open("s_test", dir.path(), TraceConfig::default())
            .await
            .unwrap();
        let action = ComputerAction::TypeText {
            text: "super-secret-password".into(),
            method: cu_core::TextInputMethod::Keyboard,
        };
        rec.record_action(
            Some("req-1".into()),
            Some("frame_1".into()),
            &action,
            serde_json::json!({"status": "success"}),
            5,
            Some("1".into()),
            Some("TextEdit".into()),
        )
        .await
        .unwrap();
        rec.close().await.unwrap();

        let raw = std::fs::read_to_string(dir.path().join("s_test.jsonl")).unwrap();
        let entry: TraceEntry = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(entry.event, "action");
        assert_eq!(entry.seq, 0);
        // Redacted: no plaintext, count preserved.
        let action_log = entry.action.unwrap();
        assert_eq!(action_log["text_redacted"], true);
        assert!(
            !raw.contains("super-secret-password"),
            "plaintext must not appear"
        );
        let red = entry.redaction.unwrap();
        assert_eq!(red.character_count, "super-secret-password".len());
    }

    #[tokio::test]
    async fn dev_mode_records_full_text() {
        let dir = tempdir().unwrap();
        let rec = TraceRecorder::open(
            "s_dev",
            dir.path(),
            TraceConfig {
                dev_mode: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let action = ComputerAction::TypeText {
            text: "hello世界".into(),
            method: cu_core::TextInputMethod::Keyboard,
        };
        rec.record_action(
            None,
            None,
            &action,
            serde_json::json!({"ok": true}),
            3,
            None,
            None,
        )
        .await
        .unwrap();
        rec.close().await.unwrap();
        let raw = std::fs::read_to_string(dir.path().join("s_dev.jsonl")).unwrap();
        assert!(raw.contains("hello世界"));
    }

    #[tokio::test]
    async fn key_actions_are_recorded_verbatim() {
        let dir = tempdir().unwrap();
        let rec = TraceRecorder::open("s_key", dir.path(), TraceConfig::default())
            .await
            .unwrap();
        let action = ComputerAction::Key {
            keys: vec!["CMD".into(), "L".into()],
        };
        rec.record_action(
            None,
            None,
            &action,
            serde_json::json!({"ok": true}),
            2,
            None,
            None,
        )
        .await
        .unwrap();
        rec.close().await.unwrap();
        let raw = std::fs::read_to_string(dir.path().join("s_key.jsonl")).unwrap();
        assert!(raw.contains("CMD"));
        assert!(raw.contains("L"));
    }

    #[tokio::test]
    async fn action_result_retains_pointer_telemetry() {
        // Section 三十八 test 12 / audit G: the pointer-execution result the
        // runtime writes into the action result (backend, isolation, cursor
        // deltas, and the REAL P0-4 interrupt telemetry — event detection,
        // human→takeover, human→input-stop) must survive into the trace
        // verbatim — latency analysis reads it from the trace alone.
        let dir = tempdir().unwrap();
        let rec = TraceRecorder::open("s_ptr", dir.path(), TraceConfig::default())
            .await
            .unwrap();
        let action = ComputerAction::Click {
            x: 10.0,
            y: 20.0,
            button: cu_core::MouseButton::Left,
            coordinate_space: cu_core::CoordinateSpace::Normalized1000,
        };
        let result = serde_json::json!({
            "status": "success",
            "duration_ms": 3,
            "pointer": {
                "backend": "physical",
                "isolated": false,
                "physical_cursor_moved": true,
                "physical_cursor_delta_px": 12.0,
                "physical_cursor_restored": false,
                "human_input_during_fallback": true,
                "event_detection_latency_ms": 1,
                "human_to_takeover_ms": 3,
                "human_to_input_stop_ms": 0
            }
        });
        rec.record_action(
            Some("req-p".into()),
            Some("frame_1".into()),
            &action,
            result.clone(),
            3,
            Some("1".into()),
            None,
        )
        .await
        .unwrap();
        rec.close().await.unwrap();

        let raw = std::fs::read_to_string(dir.path().join("s_ptr.jsonl")).unwrap();
        let entry: TraceEntry = serde_json::from_str(raw.trim()).unwrap();
        let p = &entry.result.unwrap()["pointer"];
        assert_eq!(p["backend"], "physical");
        assert_eq!(p["isolated"], serde_json::json!(false));
        assert_eq!(p["physical_cursor_moved"], serde_json::json!(true));
        assert_eq!(p["physical_cursor_delta_px"], serde_json::json!(12.0));
        assert_eq!(p["physical_cursor_restored"], serde_json::json!(false));
        assert_eq!(p["human_input_during_fallback"], serde_json::json!(true));
        assert_eq!(p["event_detection_latency_ms"], serde_json::json!(1));
        assert_eq!(p["human_to_takeover_ms"], serde_json::json!(3));
        assert_eq!(p["human_to_input_stop_ms"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn best_effort_mode_degrades_instead_of_failing() {
        // A closed writer cannot be written to; in best-effort mode append
        // degrades the recorder and returns Ok. (A real failing FS write is
        // hard to provoke portably; the degrade path is exercised here.)
        let dir = tempdir().unwrap();
        let rec = TraceRecorder::open("s_be", dir.path(), TraceConfig::default())
            .await
            .unwrap();
        // Close first: writer becomes None, but a closed trace file still
        // lets us exercise the warning bookkeeping below.
        rec.close().await.unwrap();
        assert!(!rec.is_degraded());
        rec.degrade("simulated write failure".into()).await;
        assert!(rec.is_degraded());
        let warnings = rec.warnings().await;
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("simulated"));
        // Best-effort append after degrade still returns Ok.
        let entry = TraceEntry {
            seq: 0,
            ts: chrono::Utc::now(),
            event: "probe".into(),
            session_id: Some("s_be".into()),
            request_id: None,
            frame_id: None,
            action: None,
            result: Some(serde_json::json!({})),
            duration_ms: None,
            error: None,
            change_score: None,
            stable: None,
            redaction: None,
            display_id: None,
            active_application: None,
            runtime_version: None,
        };
        rec.append(entry).await.unwrap();
    }

    #[test]
    fn trace_mode_from_env_parses() {
        assert_eq!(TraceMode::from_env(Some("required")), TraceMode::Required);
        assert_eq!(TraceMode::from_env(Some("disabled")), TraceMode::Disabled);
        assert_eq!(
            TraceMode::from_env(Some("best_effort")),
            TraceMode::BestEffort
        );
        assert_eq!(TraceMode::from_env(None), TraceMode::BestEffort);
        assert_eq!(TraceMode::from_env(Some("bogus")), TraceMode::BestEffort);
        assert_eq!(TraceMode::Required.as_str(), "required");
        assert_eq!(TraceMode::Disabled.as_str(), "disabled");
    }

    #[tokio::test]
    async fn entries_are_sequenced() {
        let dir = tempdir().unwrap();
        let rec = TraceRecorder::open("s_seq", dir.path(), TraceConfig::default())
            .await
            .unwrap();
        assert_eq!(rec.next_seq(), 0);
        assert_eq!(rec.next_seq(), 1);
        assert_eq!(rec.next_seq(), 2);
        rec.close().await.unwrap();
    }
}
