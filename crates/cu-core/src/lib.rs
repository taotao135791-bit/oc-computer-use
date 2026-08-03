//! `cu-core`: shared types, errors, coordinate math, and wire protocol for the
//! Computer Use runtime.
//!
//! This crate is deliberately dependency-light and platform-independent. It is
//! what every other crate (`cu-driver*`, `cu-runtime`, `cu-trace`, `cu-daemon`,
//! `cu-cli`) builds on, so it must never import macOS or Tokio specifics.

pub mod actions;
pub mod config;
pub mod coordinates;
pub mod errors;
pub mod frames;
pub mod protocol;
pub mod sessions;

pub use actions::{
    ActionBatch, ComputerAction, MouseButton, RedactedText, TextInputMethod, WaitPolicy,
};
pub use coordinates::{CoordinateSpace, DisplayBounds, ImageGeometry, Point, Region};
pub use errors::{CuError, ErrorCode, PermissionKind, PermissionIssue, StaleFrameDetail};
pub use frames::{ScreenFrame, ScreenSnapshot, StaleFrameVerdict};
pub use protocol::{
    ActParams, ActResult, ActionResultReport, InspectMapping, InspectParams, InspectResult,
    ObserveParams, ObserveResult, RpcRequest, RpcResponse, SessionParams, SessionResult,
    TraceEntry, TraceExport, TraceSummary,
};
pub use sessions::{SessionAction, SessionState, SessionStatus};
