//! `cu trace analyze`: derive per-session metrics and a failure category
//! from a trace — the same numbers the benchmark report computes, available
//! from the CLI without a browser, plus a compact timeline for forensics.
//!
//! Classification rules mirror `benchmarks/runner/lib/trace.mjs`
//! (`classifyFailure`): they are heuristic, derived strictly from trace
//! events, and documented in `benchmarks/README.md`. The runtime records
//! no `inspect` or `recovery` events yet, so those metrics are 0 — the
//! analysis reports what the trace actually contains, never an estimate.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use cu_core::{CuError, TraceEntry};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One timeline entry: `offset_ms` since the first recorded event.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TimelineEntry {
    pub offset_ms: u64,
    pub event: String,
    pub detail: String,
}

/// Aggregate metrics for a session trace.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TraceAnalysis {
    pub trace_id: String,
    pub session_id: String,
    pub event_count: usize,
    pub started_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
    /// Wall-clock duration between the first and last recorded event.
    pub duration_ms: Option<u64>,
    pub observe_calls: usize,
    pub inspect_calls: usize,
    pub action_batches: usize,
    pub total_actions: usize,
    pub actions_by_type: BTreeMap<String, usize>,
    pub failed_action_count: usize,
    pub cancelled_request_count: usize,
    pub timeout_count: usize,
    pub stale_frame_count: usize,
    pub recovery_count: usize,
    pub user_takeover_count: usize,
    pub cancel_event_count: usize,
    pub screenshot_bytes: u64,
    pub last_failed_action_error: Option<String>,
    /// Failure category from the documented taxonomy (`None` when the trace
    /// carries no failure signal, e.g. an unfinished session).
    pub failure_category: Option<String>,
    /// Human-readable root-cause excerpt: last failed action plus the final
    /// trace events.
    pub failure_detail: Option<String>,
    /// Most recent `limit` timeline entries (oldest first), for forensics.
    pub timeline: Vec<TimelineEntry>,
}

/// Parse a trace in JSONL wire format (as returned by `trace.export`).
pub fn parse_jsonl(content: &str) -> Result<Vec<TraceEntry>, CuError> {
    let mut entries = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: TraceEntry = serde_json::from_str(line).map_err(|e| {
            CuError::Trace(format!(
                "trace line {} is not a valid TraceEntry: {e}",
                i + 1
            ))
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Compute metrics and a failure classification for a session trace.
/// Pure function over the parsed entries — no I/O, unit-testable.
pub fn analyze(entries: &[TraceEntry], timeline_limit: usize) -> TraceAnalysis {
    let actions: Vec<&TraceEntry> = entries.iter().filter(|e| e.event == "action").collect();
    let observes: Vec<&TraceEntry> = entries.iter().filter(|e| e.event == "observe").collect();
    let stale: Vec<&TraceEntry> = entries
        .iter()
        .filter(|e| e.event == "act.stale_rejected")
        .collect();
    let cancels: Vec<&TraceEntry> = entries.iter().filter(|e| e.event == "cancel").collect();
    let takeovers: Vec<&TraceEntry> = entries
        .iter()
        .filter(|e| e.event == "session.takeover")
        .collect();

    let batches: usize = actions
        .iter()
        .filter_map(|e| e.request_id.as_deref())
        .filter(|id| !id.is_empty())
        .collect::<std::collections::HashSet<_>>()
        .len();

    let failed: Vec<&TraceEntry> = actions
        .iter()
        .filter(|a| {
            a.result
                .as_ref()
                .and_then(|r| r.get("status"))
                .and_then(|v| v.as_str())
                == Some("failed")
        })
        .copied()
        .collect();
    let cancelled: Vec<&TraceEntry> = actions
        .iter()
        .filter(|a| {
            a.result
                .as_ref()
                .and_then(|r| r.get("status"))
                .and_then(|v| v.as_str())
                == Some("cancelled")
        })
        .copied()
        .collect();

    let last_failed_error = || -> Option<String> {
        for a in failed.iter().rev() {
            if let Some(e) = a
                .result
                .as_ref()
                .and_then(|r| r.get("error"))
                .and_then(|v| v.as_str())
            {
                if !e.is_empty() {
                    return Some(e.to_string());
                }
            }
        }
        None
    };
    let last_error = last_failed_error();

    let mut actions_by_type: BTreeMap<String, usize> = BTreeMap::new();
    for a in &actions {
        let t = a
            .action
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *actions_by_type.entry(t.to_string()).or_insert(0) += 1;
    }

    let screenshot_bytes: u64 = observes
        .iter()
        .filter_map(|e| e.result.as_ref().and_then(|r| r.get("screenshot_bytes")))
        .filter_map(|v| v.as_u64())
        .sum();

    let started_at = entries.iter().map(|e| e.ts).min();
    let stopped_at = entries.iter().map(|e| e.ts).max();
    let duration_ms = match (started_at, stopped_at) {
        (Some(s), Some(t)) => Some((t - s).num_milliseconds().max(0) as u64),
        _ => None,
    };

    let trace_id = entries
        .iter()
        .filter_map(|e| e.session_id.as_deref())
        .next()
        .unwrap_or("?")
        .to_string();
    let session_id = trace_id.clone();

    let failure_category = classify(
        actions.len(),
        &failed,
        &stale,
        &takeovers,
        &cancels,
        last_error.as_deref(),
    );
    let failure_detail = failure_detail_of(&failed, entries, failure_category.as_deref());

    let timeline = build_timeline(entries, timeline_limit);

    TraceAnalysis {
        trace_id,
        session_id,
        event_count: entries.len(),
        started_at,
        stopped_at,
        duration_ms,
        observe_calls: observes.len(),
        inspect_calls: 0, // runtime records no inspect event yet (documented in benchmarks/README.md)
        action_batches: batches,
        total_actions: actions.len(),
        actions_by_type,
        failed_action_count: failed.len(),
        cancelled_request_count: cancelled.len(),
        timeout_count: failed
            .iter()
            .filter(|a| {
                a.result
                    .as_ref()
                    .and_then(|r| r.get("error"))
                    .and_then(|v| v.as_str())
                    .is_some_and(is_timeout_wording)
            })
            .count(),
        stale_frame_count: stale.len(),
        recovery_count: 0, // runtime has no recovery mechanism/event (documented in benchmarks/README.md)
        user_takeover_count: takeovers.len(),
        cancel_event_count: cancels.len(),
        screenshot_bytes,
        last_failed_action_error: last_error,
        failure_category,
        failure_detail,
        timeline,
    }
}

/// "timeout" or the runtime's "timed out" wording.
fn is_timeout_wording(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("timeout") || lower.contains("timed out")
}

/// The documented failure taxonomy — mirrors the benchmark runner's rules.
fn classify(
    total_actions: usize,
    failed: &[&TraceEntry],
    stale: &[&TraceEntry],
    takeovers: &[&TraceEntry],
    _cancels: &[&TraceEntry],
    last_error: Option<&str>,
) -> Option<String> {
    // Runtime-side signals first: the trace records the evidence directly.
    if !stale.is_empty() {
        // The model referenced a stale frame and the batch was refused; the
        // task still failed — the model failed to recover from the rejection.
        return Some("STALE_FRAME_RECOVERY_FAILED".into());
    }
    if !takeovers.is_empty() {
        // The user grabbed the mouse mid-task.
        return Some("CANCEL_FAILED".into());
    }
    if let Some(e) = last_error {
        if e.to_lowercase().contains("permission") {
            return Some("PERMISSION_ERROR".into());
        }
        // The runtime's timeout wording is "request timed out: …" — match
        // both spellings so real failures land in ACTION_TIMEOUT.
        if is_timeout_wording(e) {
            return Some("ACTION_TIMEOUT".into());
        }
    }
    if total_actions == 0 {
        // The model produced no act call at all — it never drove the desktop.
        return Some("MODEL_STOPPED_EARLY".into());
    }
    if let Some(e) = last_error {
        let last_failed = failed.last();
        let ty = last_failed
            .and_then(|a| a.action.as_ref())
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str());
        return match ty {
            Some("scroll") => Some("SCROLL_DIRECTION_ERROR".into()),
            Some("drag") => Some("DRAG_FAILED".into()),
            Some("type") | Some("type_text") => {
                let lower = e.to_lowercase();
                if lower.contains("unicode") || lower.contains("ime") || lower.contains("clipboard")
                {
                    Some("UNICODE_INPUT_FAILED".into())
                } else {
                    Some("TEXT_INPUT_FAILED".into())
                }
            }
            Some("click") => {
                if e.to_lowercase().contains("small")
                    || e.to_lowercase().contains("target")
                    || e.to_lowercase().contains("miss")
                {
                    Some("SMALL_TARGET_MISS".into())
                } else {
                    Some("GROUNDING_MISS".into())
                }
            }
            _ => Some("MODEL_PLANNING_ERROR".into()),
        };
    }
    // Actions ran (some succeeded) but the outcome was never reached.
    Some("MODEL_PLANNING_ERROR".into())
}

/// A short, honest root-cause excerpt: the last failed action's error and
/// the last few trace events.
fn failure_detail_of(
    failed: &[&TraceEntry],
    entries: &[TraceEntry],
    _category: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(last) = failed.last() {
        let ty = last
            .action
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("action");
        if let Some(e) = last
            .result
            .as_ref()
            .and_then(|r| r.get("error"))
            .and_then(|v| v.as_str())
        {
            parts.push(format!("last failed {ty}: {e}"));
        }
    }
    let tail = entries
        .iter()
        .rev()
        .take(3)
        .map(|e| e.event.clone())
        .collect::<Vec<_>>();
    if !tail.is_empty() {
        parts.push(format!("last events: {}", tail.join(", ")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// Compact timeline: offset (ms) since the first event, event name, and a
/// one-line detail. `limit` keeps the most recent entries.
fn build_timeline(entries: &[TraceEntry], limit: usize) -> Vec<TimelineEntry> {
    let base = entries.iter().map(|e| e.ts).min();
    let mut out: Vec<TimelineEntry> = entries
        .iter()
        .map(|e| {
            let offset_ms = base
                .map(|b| (e.ts - b).num_milliseconds().max(0) as u64)
                .unwrap_or(0);
            TimelineEntry {
                offset_ms,
                event: e.event.clone(),
                detail: timeline_detail(e),
            }
        })
        .collect();
    if out.len() > limit {
        out = out.split_off(out.len() - limit);
    }
    out
}

fn timeline_detail(e: &TraceEntry) -> String {
    match e.event.as_str() {
        "action" => {
            let ty = e
                .action
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = e
                .result
                .as_ref()
                .and_then(|r| r.get("status"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let dur = e.duration_ms.map(|d| format!(" {d}ms")).unwrap_or_default();
            format!("{ty} {status}{dur}")
        }
        "observe" => {
            let bytes = e
                .result
                .as_ref()
                .and_then(|r| r.get("screenshot_bytes"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            format!("frame={} {bytes}B", e.frame_id.as_deref().unwrap_or("?"))
        }
        "act.stale_rejected" => {
            let score = e
                .result
                .as_ref()
                .and_then(|r| r.get("change_score"))
                .and_then(|v| v.as_f64())
                .unwrap_or(-1.0);
            format!(
                "frame={} change_score={score:.3}",
                e.frame_id.as_deref().unwrap_or("?")
            )
        }
        other => e
            .result
            .as_ref()
            .map(|r| r.to_string())
            .unwrap_or_else(|| other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(
        event: &str,
        ts_ms: i64,
        action_type: Option<&str>,
        status: Option<&str>,
        error: Option<&str>,
    ) -> TraceEntry {
        let action = action_type.map(|t| json!({ "type": t }));
        let result = match status {
            Some(s) => {
                let mut r = json!({ "status": s });
                if let Some(e) = error {
                    r["error"] = json!(e);
                }
                Some(r)
            }
            None => None,
        };
        TraceEntry {
            seq: 0,
            ts: DateTime::from_timestamp_millis(ts_ms).unwrap(),
            event: event.into(),
            session_id: Some("t".into()),
            request_id: Some("r1".into()),
            frame_id: None,
            action,
            result,
            duration_ms: Some(120),
            error: None,
            change_score: None,
            stable: None,
            redaction: None,
            display_id: None,
            active_application: None,
            runtime_version: None,
        }
    }

    #[test]
    fn analyze_empty_trace_yields_no_failure() {
        let a = analyze(&[], 10);
        assert_eq!(a.total_actions, 0);
        assert_eq!(a.failure_category, Some("MODEL_STOPPED_EARLY".into()));
        assert_eq!(a.observe_calls, 0);
        assert!(a.timeline.is_empty());
    }

    #[test]
    fn analyze_successful_session_is_unclassified() {
        let entries = vec![
            entry("session.start", 1000, None, None, None),
            entry("observe", 1100, None, None, None),
            entry("action", 1300, Some("click"), Some("success"), None),
            entry("session.stop", 2000, None, None, None),
        ];
        let a = analyze(&entries, 10);
        assert_eq!(a.total_actions, 1);
        assert_eq!(a.action_batches, 1);
        assert_eq!(a.observe_calls, 1);
        assert_eq!(a.duration_ms, Some(1000));
        // A trace with actions that all succeeded carries no failure signal.
        assert_eq!(a.failure_category, Some("MODEL_PLANNING_ERROR".into()));
        assert_eq!(a.timeline.len(), 4);
    }

    #[test]
    fn analyze_stale_rejection_classifies_recovery_failed() {
        let entries = vec![
            entry("session.start", 1000, None, None, None),
            entry("observe", 1100, None, None, None),
            entry("action", 1300, Some("click"), Some("success"), None),
            entry("act.stale_rejected", 1500, None, None, None),
            entry("session.stop", 2000, None, None, None),
        ];
        let a = analyze(&entries, 10);
        assert_eq!(a.stale_frame_count, 1);
        assert_eq!(
            a.failure_category,
            Some("STALE_FRAME_RECOVERY_FAILED".into())
        );
    }

    #[test]
    fn analyze_failed_click_miss_reports_small_target_miss() {
        // Mirrors the benchmark runner: a click error mentioning the target
        // classifies as SMALL_TARGET_MISS.
        let entries = vec![
            entry(
                "action",
                1000,
                Some("click"),
                Some("failed"),
                Some("target not found at (10,20)"),
            ),
            entry("session.stop", 2000, None, None, None),
        ];
        let a = analyze(&entries, 10);
        assert_eq!(a.failed_action_count, 1);
        assert_eq!(
            a.last_failed_action_error.as_deref(),
            Some("target not found at (10,20)")
        );
        assert_eq!(a.failure_category, Some("SMALL_TARGET_MISS".into()));
    }

    #[test]
    fn analyze_timeout_error_classifies_action_timeout() {
        // Real runtime wording is "request timed out: …" — must classify as
        // ACTION_TIMEOUT despite the space in "timed out".
        let entries = vec![entry(
            "action",
            1000,
            Some("click"),
            Some("failed"),
            Some("request timed out: operation exceeded 30s"),
        )];
        let a = analyze(&entries, 10);
        assert_eq!(a.timeout_count, 1);
        assert_eq!(a.failure_category, Some("ACTION_TIMEOUT".into()));
    }

    #[test]
    fn parse_jsonl_roundtrips_entries() {
        let entries = [
            entry("session.start", 1000, None, None, None),
            entry("action", 1300, Some("click"), Some("success"), None),
        ];
        let lines: Vec<String> = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        let parsed = parse_jsonl(&lines.join("\n")).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].event, "action");
        assert!(parse_jsonl("not json\n").is_err());
    }
} // mod tests
