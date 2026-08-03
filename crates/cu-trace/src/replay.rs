//! Trace replay: reconstructs what happened during a session from its trace
//! JSONL. Version 1 is a **visual/log replay** — it replays the recorded
//! actions, timings, and results so a human or the inspector can step through
//! what the runtime did. It deliberately does **not** re-execute real mouse
//! events against the live desktop (that is a future, explicitly-gated feature).

use chrono::Utc;
use cu_core::{CuError, TraceEntry};

/// One step of a replay.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReplayStep {
    pub seq: u64,
    pub ts: String,
    pub event: String,
    /// The action as it was logged (may be redacted, so it is kept as raw JSON
    /// rather than re-deserialized into a `ComputerAction`).
    pub action: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub duration_ms: Option<u64>,
    pub frame_id: Option<String>,
}

/// A full replay of a session's trace.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Replay {
    pub session_id: String,
    pub steps: Vec<ReplayStep>,
    pub generated_at: String,
}

/// Build a replay from a list of trace entries (preserving order).
pub fn build_replay(session_id: &str, entries: &[TraceEntry]) -> Replay {
    let steps = entries
        .iter()
        .filter(|e| {
            e.event == "action"
                || e.event == "observe"
                || e.event == "session.start"
                || e.event == "session.stop"
        })
        .map(|e| ReplayStep {
            seq: e.seq,
            ts: e.ts.to_rfc3339(),
            event: e.event.clone(),
            action: e.action.clone(),
            result: e.result.clone(),
            duration_ms: e.duration_ms,
            frame_id: e.frame_id.clone(),
        })
        .collect();
    Replay {
        session_id: session_id.to_string(),
        steps,
        generated_at: Utc::now().to_rfc3339(),
    }
}

/// Load a trace file and build a replay from it.
pub fn replay_from_file(session_id: &str, path: &std::path::Path) -> Result<Replay, CuError> {
    let entries = crate::storage::read_trace(path)?;
    Ok(build_replay(session_id, &entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::{ComputerAction, TextInputMethod};
    use tempfile::tempdir;

    #[tokio::test]
    async fn replay_builds_from_recorded_actions() {
        let dir = tempdir().unwrap();
        let rec = crate::recorder::TraceRecorder::open("s_rp", dir.path(), Default::default())
            .await
            .unwrap();
        rec.record_event("session.start", serde_json::json!({"state": "active"}))
            .await
            .unwrap();
        let action = ComputerAction::TypeText {
            text: "hi".into(),
            method: TextInputMethod::Keyboard,
        };
        rec.record_action(
            Some("r1".into()),
            Some("f1".into()),
            &action,
            serde_json::json!({"status": "success"}),
            4,
            Some("1".into()),
            Some("TextEdit".into()),
        )
        .await
        .unwrap();
        rec.close().await.unwrap();

        let replay = replay_from_file("s_rp", &dir.path().join("s_rp.jsonl")).unwrap();
        assert_eq!(replay.session_id, "s_rp");
        assert_eq!(replay.steps.len(), 2);
        assert_eq!(replay.steps[0].event, "session.start");
        assert_eq!(replay.steps[1].event, "action");
        // Redaction: the action in the replay carries no plaintext.
        let logged = replay.steps[1].action.clone().unwrap();
        assert_eq!(logged["text_redacted"], true);
    }
}
