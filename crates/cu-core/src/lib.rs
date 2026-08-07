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
pub mod pointer;
pub mod private_file;
pub mod protocol;
pub mod security;
pub mod sessions;

pub use actions::{
    ActionBatch, ComputerAction, MouseButton, RedactedText, TextInputMethod, WaitPolicy,
};
pub use coordinates::{CoordinateSpace, DisplayBounds, ImageGeometry, Point, Region};
pub use errors::{CuError, ErrorCode, PermissionIssue, PermissionKind, StaleFrameDetail};
pub use frames::{ScreenFrame, ScreenSnapshot, StaleFrameVerdict};
pub use pointer::{
    DragBackend, PhysicalFallbackState, PointerExecutionBackend, PointerMode, PointerPolicy,
    PointerStatusView, VirtualPointerState,
};
pub use protocol::{
    ActParams, ActResult, ActionResultReport, CancelParams, CancelResult, CapabilityTokenParams,
    ClientInfo, InspectMapping, InspectParams, InspectResult, ObserveParams, ObserveResult,
    RequestKey, RpcRequest, RpcResponse, RuntimeVersionResult, SessionParams, SessionResult,
    SessionSummary, ShutdownParams, StabilizationInfo, TraceAdminListParams, TraceEntry,
    TraceExportParams, TraceExportResult, TraceGetParams, TraceListParams, TraceReplayParams,
    TraceReport, TraceSummariesParams, TraceSummary,
};
pub use security::{
    generate_control_token, generate_daemon_admin_token, generate_observation_token, redact_json,
    ControlToken, DaemonAdminToken, ObservationToken, SecretToken, SecretTokenHash,
};
pub use sessions::{FocusPolicy, SessionAction, SessionState, SessionStatus, SessionTarget};
