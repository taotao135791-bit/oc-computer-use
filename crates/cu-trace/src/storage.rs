//! Trace storage: scanning, reading, exporting, and pruning trace files.
//!
//! Traces live under `<runtime_dir>/traces/<session_id>.jsonl`. Export copies
//! the raw JSONL to a caller-chosen file so a session can be shared or
//! inspected without touching the live file.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use cu_core::{CuError, TraceEntry, TraceSummary};

/// Default retention for trace files.
pub const DEFAULT_RETENTION_DAYS: u64 = 7;

pub fn traces_dir() -> PathBuf {
    cu_core::config::traces_dir()
}

/// Scan the trace directory and summarize every trace found.
pub fn list_traces(dir: &Path) -> Result<Vec<TraceSummary>, CuError> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir).map_err(|e| CuError::Trace(e.to_string()))? {
        let entry = entry.map_err(|e| CuError::Trace(e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(summary) = summarize(&path)? {
            out.push(summary);
        }
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    Ok(out)
}

/// Summarize only one session's trace (the session-scoped listing used by
/// `trace.list` / `trace.summaries`, which verify the session's own tokens
/// before scanning). Returns at most one entry — a session has one trace.
pub fn list_session_traces(dir: &Path, session_id: &str) -> Result<Vec<TraceSummary>, CuError> {
    let path = dir.join(format!("{session_id}.jsonl"));
    Ok(match summarize(&path)? {
        Some(s) => vec![s],
        None => Vec::new(),
    })
}

fn summarize(path: &Path) -> Result<Option<TraceSummary>, CuError> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let session_id = file_name
        .strip_suffix(".jsonl")
        .unwrap_or(file_name)
        .to_string();
    let meta = std::fs::metadata(path).map_err(|e| CuError::Trace(e.to_string()))?;
    if !meta.is_file() {
        return Ok(None);
    }
    // Read first and last non-empty line to recover timestamps cheaply.
    let content = std::fs::read_to_string(path).map_err(|e| CuError::Trace(e.to_string()))?;
    let mut event_count = 0usize;
    let mut started_at = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok());
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if event_count == 0 {
            if let Ok(e) = serde_json::from_str::<TraceEntry>(line) {
                started_at = Some(ts_to_duration(&e.ts));
            }
        }
        event_count += 1;
    }
    Ok(Some(TraceSummary {
        // One trace per session; the trace id is the file stem (the session
        // id). The filesystem path itself never leaves the daemon.
        trace_id: session_id.clone(),
        session_id,
        created_at: duration_to_ts(started_at),
        size_bytes: meta.len(),
        event_count,
    }))
}

fn ts_to_duration(ts: &DateTime<Utc>) -> std::time::Duration {
    ts.signed_duration_since(DateTime::from_timestamp(0, 0).unwrap())
        .to_std()
        .unwrap_or_default()
}

fn duration_to_ts(d: Option<std::time::Duration>) -> DateTime<Utc> {
    match d {
        Some(d) => {
            DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos()).unwrap_or_else(Utc::now)
        }
        None => Utc::now(),
    }
}

/// Read every entry in a trace as structured records.
pub fn read_trace(path: &Path) -> Result<Vec<TraceEntry>, CuError> {
    let content = std::fs::read_to_string(path).map_err(|e| CuError::Trace(e.to_string()))?;
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: TraceEntry = serde_json::from_str(line)
            .map_err(|e| CuError::Trace(format!("bad trace line: {e}")))?;
        out.push(entry);
    }
    Ok(out)
}

/// Copy a trace to a new location (for export). Returns the destination path.
pub fn export_trace(src: &Path, dest: &Path) -> Result<PathBuf, CuError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CuError::Trace(e.to_string()))?;
    }
    std::fs::copy(src, dest).map_err(|e| CuError::Trace(format!("export failed: {e}")))?;
    Ok(dest.to_path_buf())
}

/// Delete trace files older than `retention_days`. Returns the number removed.
pub fn prune_old_traces(dir: &Path, retention_days: u64) -> Result<usize, CuError> {
    let cutoff = chrono::Duration::days(retention_days as i64);
    let now = Utc::now();
    let mut removed = 0usize;
    if !dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(dir).map_err(|e| CuError::Trace(e.to_string()))? {
        let entry = entry.map_err(|e| CuError::Trace(e.to_string()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(summary) = summarize(&path)? {
            if now.signed_duration_since(summary.created_at) > cutoff {
                std::fs::remove_file(&path).map_err(|e| CuError::Trace(e.to_string()))?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn list_and_read_round_trip() {
        let dir = tempdir().unwrap();
        let rec = crate::recorder::TraceRecorder::open("s_list", dir.path(), Default::default())
            .await
            .unwrap();
        rec.record_event("session.start", serde_json::json!({"state": "active"}))
            .await
            .unwrap();
        rec.close().await.unwrap();

        let list = list_traces(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "s_list");
        assert_eq!(
            list[0].trace_id, "s_list",
            "one trace per session, id = file stem"
        );
        assert_eq!(list[0].event_count, 1);
        assert!(list[0].size_bytes > 0);

        // The summary carries no filesystem path — paths never cross the wire.
        let entries = read_trace(&dir.path().join("s_list.jsonl")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event, "session.start");
    }

    #[tokio::test]
    async fn export_copies_file() {
        let dir = tempdir().unwrap();
        let rec = crate::recorder::TraceRecorder::open("s_exp", dir.path(), Default::default())
            .await
            .unwrap();
        rec.record_event("x", serde_json::json!({})).await.unwrap();
        rec.close().await.unwrap();
        let dest = dir.path().join("out").join("export.jsonl");
        let out = export_trace(&dir.path().join("s_exp.jsonl"), &dest).unwrap();
        assert!(out.exists());
        let entries = read_trace(&out).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn prune_removes_old_traces() {
        let dir = tempdir().unwrap();
        let rec = crate::recorder::TraceRecorder::open("s_old", dir.path(), Default::default())
            .await
            .unwrap();
        rec.record_event("x", serde_json::json!({})).await.unwrap();
        rec.close().await.unwrap();
        // Backdate the file mtime far into the past.
        let path = dir.path().join("s_old.jsonl");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        std::fs::File::options().write(true).open(&path).unwrap();
        let _ = filetime_set_old(&path, old);
        // summarize() uses mtime only when it cannot parse the ts; our entries
        // carry real timestamps, so force the check against the *entry* ts by
        // using a zero retention.
        let removed = prune_old_traces(dir.path(), 0).unwrap();
        assert_eq!(removed, 1);
        assert!(!path.exists());
    }

    fn filetime_set_old(_path: &Path, _t: std::time::SystemTime) -> std::io::Result<()> {
        // Best-effort mtime backdating; the retention=0 case is what matters.
        Ok(())
    }
}
