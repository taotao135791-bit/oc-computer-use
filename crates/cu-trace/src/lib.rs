//! `cu-trace`: session trace recording, storage, and replay.
//!
//! Traces are newline-delimited JSON, one [`cu_core::TraceEntry`] per line,
//! written by [`recorder::TraceRecorder`] and managed by [`storage`]. Replay
//! ([`replay`]) reconstructs a step-by-step log of what the runtime did.

pub mod manifest;
pub mod recorder;
pub mod replay;
pub mod storage;

pub use manifest::{check_access, mark_stopped, write_manifest, TraceAccessManifest};
pub use recorder::{TraceConfig, TraceMode, TraceRecorder};
pub use replay::{build_replay, replay_from_file, Replay, ReplayStep};
pub use storage::{
    list_session_traces, list_traces, prune_old_traces, read_trace, read_trace_export,
    TraceExportBytes,
};
