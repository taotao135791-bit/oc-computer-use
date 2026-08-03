//! `cu-trace`: session trace recording, storage, and replay.
//!
//! Traces are newline-delimited JSON, one [`cu_core::TraceEntry`] per line,
//! written by [`recorder::TraceRecorder`] and managed by [`storage`]. Replay
//! ([`replay`]) reconstructs a step-by-step log of what the runtime did.

pub mod recorder;
pub mod replay;
pub mod storage;

pub use recorder::{TraceConfig, TraceRecorder};
pub use replay::{build_replay, replay_from_file, Replay, ReplayStep};
pub use storage::{export_trace, list_traces, prune_old_traces, read_trace};
