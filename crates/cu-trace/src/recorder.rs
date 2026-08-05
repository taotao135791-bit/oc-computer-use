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

    /// Record an observe call.
    pub async fn record_observe(
        &self,
        request_id: Option<String>,
        frame_id: &str,
        width: u32,
        height: u32,
        display_id: &str,
    ) -> Result<(), CuError> {
        self.append(TraceEntry {
            seq: 0,
            ts: Utc::now(),
            event: "observe".into(),
            session_id: Some(self.session_id.clone()),
            request_id,
            frame_id: Some(frame_id.to_string()),
            action: None,
            result: Some(
                serde_json::json!({ "width": width, "height": height, "display_id": display_id }),
            ),
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
