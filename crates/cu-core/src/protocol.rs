//! Wire types for the JSON-RPC 2.0 protocol spoken between the daemon and the
//! SDK/CLI/MCP adapters. Keeping these in `cu-core` (rather than in the daemon
//! crate) guarantees the protocol is a single source of truth shared by every
//! adapter — no two adapters may define their own incompatible wire format.
//!
//! Transport: newline-delimited JSON over a local Unix domain socket. Requests
//! may be pipelined; responses carry the matching `id`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::actions::{ComputerAction, WaitPolicy};
use crate::coordinates::Region;
use crate::security::{redact_json, SecretToken};
use crate::sessions::{SessionAction, SessionState};

/// Identifies one in-flight JSON-RPC request across **all** connections.
///
/// Cancellation is scoped by this key: `computer.cancel` may only cancel the
/// request it names on the connection it arrived on — client A canceling
/// `request_id: 1` must never cancel client B's `request_id: 1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestKey {
    pub connection_id: u64,
    pub request_id: serde_json::Value,
}

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC 2.0 request as received by the daemon.
///
/// `Debug` is redacting by hand: `params` may carry capability tokens
/// (`control_token` / `observation_token` / `admin_token`), so the derived
/// form would print them. `{request:?}` in a log is always safe.
#[derive(Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl std::fmt::Debug for RpcRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcRequest")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("method", &self.method)
            .field("params", &self.params.as_ref().map(redact_json))
            .finish()
    }
}

/// A JSON-RPC 2.0 response written by the daemon.
///
/// `Debug` redacts `result`/`error.data` (the one-time `start` response
/// carries both capability tokens in `result`).
#[derive(Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl std::fmt::Debug for RpcResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcResponse")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("result", &self.result.as_ref().map(redact_json))
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Debug for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("data", &self.data.as_ref().map(redact_json))
            .finish()
    }
}

impl RpcResponse {
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(
        id: Option<serde_json::Value>,
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message,
                data,
            }),
        }
    }
}

/// `computer.observe` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct ObserveParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    /// The session's **observation token**. Required — observing captures the
    /// desktop; a session id alone grants no observation permission. A valid
    /// control token is accepted in its place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    /// The session's control token — accepted in place of the observation
    /// token (control includes observation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_cursor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpeg_quality: Option<u8>,
    /// When true, the response carries the image as base64. Off by default so a
    /// plain `observe` stays cheap; adapters that need pixels (MCP, vision
    /// harnesses) turn it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_image: Option<bool>,
}

/// `computer.observe` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObserveResult {
    pub session_id: String,
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub display_id: String,
    pub scale_factor: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_application: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_window: Option<String>,
    /// Base64-encoded image (only present when the caller requested it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_base64: Option<String>,
    /// Absolute path to the stored image file.
    pub image_path: String,
    pub image_mime_type: String,
    pub captured_at: DateTime<Utc>,
    /// P0-6: the coordinate space the returned image's pixels are expressed
    /// in (`"normalized_1000"` — the image is treated as a 1000x1000 canvas).
    /// Present on every observe so the caller never has to guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate_space: Option<String>,
    /// P0-6: when the session is scoped to a target window, the image is a
    /// CROP of that window and this is its global logical bounds (the caller
    /// maps window coords → screen via this). `None` = full-display observe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bounds: Option<crate::coordinates::DisplayBounds>,
    /// P0-6: the observed window's id, when the session is window-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<u32>,
}

/// `computer.act` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActParams {
    pub session_id: String,
    pub frame_id: String,
    pub actions: Vec<ComputerAction>,
    /// The session's control token. Required: without it the batch is rejected
    /// before any action is parsed, queued, or executed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_policy: Option<WaitPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_screenshot: Option<bool>,
    // Forward-looking safety hooks: an upper layer may tag a batch with
    // risk/confirmation policy. The runtime honours `requires_confirmation`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_confirmation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_context: Option<String>,
}

/// Pointer-execution detail retained in an action result (round 9 / P0-7,
/// P0-9). The backend that actually realized the action, whether it was
/// isolated, and how the real system cursor moved (if at all). This is used
/// by benchmark reports and human-interrupt latency analysis — never guessed
/// from the action type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PointerExecutionResult {
    /// The actuator that actually realized the action:
    /// `"virtual" | "direct_cg_event" | "accessibility" | "physical"`.
    pub backend: String,
    /// True when the action executed without touching the real system cursor.
    pub isolated: bool,
    /// True when the real system cursor was temporarily moved.
    pub physical_cursor_moved: bool,
    /// Distance the system cursor moved (logical px); 0 for isolated actions.
    pub physical_cursor_delta_px: f64,
    /// Whether the cursor was restored to its original position after a
    /// physical fallback transaction (absent when never borrowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_cursor_restored: Option<bool>,
    /// P0-2: whether a real human input occurred during the physical fallback
    /// transaction (in which case the cursor is NEVER yanked back). Absent
    /// when no physical fallback ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_input_during_fallback: Option<bool>,
    /// P0-4: human-interrupt telemetry, present when a real human input
    /// occurred during or near this action. `human_to_input_stop_ms` is THE
    /// Human Interrupt KPI — hardware event → LAST runtime synthetic input;
    /// 0 when the agent had already stopped. `event_detection_latency_ms` is
    /// hardware event → Event Tap callback; `human_to_takeover_ms` is hardware
    /// event → takeover applied. The inverse `synthetic → human` direction is
    /// intentionally NOT exposed here (it must never be labelled as interrupt
    /// latency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_detection_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_to_takeover_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_to_input_stop_ms: Option<u64>,
}

/// Result of one action inside a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActionResultReport {
    pub index: usize,
    pub status: String, // success | failed | cancelled
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Round 9 / P0-9: which pointer backend/actuator realized the action
    /// (retained in the result + trace for real verification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<PointerExecutionResult>,
}

/// Outcome of the post-batch stabilization wait (`WaitPolicy::UntilStable`).
/// `change_score` is the **last measured** thumbnail difference — on timeout
/// it carries the real score (never a fabricated 0), so the caller can tell a
/// screen that nearly settled from one that kept animating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StabilizationInfo {
    pub outcome: String, // "stable" | "timed_out"
    pub change_score: f64,
    pub samples: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// `computer.act` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActResult {
    pub executed: bool,
    pub action_results: Vec<ActionResultReport>,
    pub screen_changed: bool,
    pub stable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ObserveResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stabilization: Option<StabilizationInfo>,
    /// Trace-recording status for this batch. Present when the session has a
    /// recorder; `degraded`/`warnings` surface best-effort recording problems
    /// so callers know the trace may be incomplete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceReport>,
}

/// Recording status of the session's trace for one `computer.act` batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceReport {
    /// "required" | "best_effort" | "disabled" — the daemon's trace mode.
    pub mode: String,
    /// True when the trace could not be written in best-effort mode (or the
    /// recorder degraded); the operation itself still succeeded.
    pub degraded: bool,
    /// Human-readable warnings produced while recording this batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// `computer.inspect` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InspectParams {
    pub session_id: String,
    pub frame_id: String,
    pub region: Region,
    /// Observation (or control) token — required; a session id alone grants
    /// no observation permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u32>,
}

/// Mapping info that lets a model safely translate an inspect-relative
/// coordinate back into global desktop coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InspectMapping {
    /// Where the crop sits in the original image (pixels).
    pub source_image_rect: Region,
    /// Global desktop point corresponding to the crop's top-left corner.
    pub global_origin: (f64, f64),
    /// The crop's top-left corner in the original frame's normalized_1000 space.
    pub normalized_1000_origin: (f64, f64),
}

/// `computer.inspect` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InspectResult {
    pub session_id: String,
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub image_base64: String,
    pub image_mime_type: String,
    pub mapping: InspectMapping,
}

/// Identity of the client that started a session. Recorded on the session so
/// owners can be told apart: a client must not stop a session it did not
/// start. `client_instance_id` distinguishes multiple processes of the same
/// client (e.g. two Pi instances).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClientInfo {
    pub client_id: String,
    pub client_name: String,
    pub client_instance_id: String,
}

/// `computer.session` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionParams {
    pub action: SessionAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    /// The session's control token. Required for `pause`/`resume`/`takeover`/
    /// `release`/`stop`; `start` does not need one. `status` needs either this
    /// or the observation token (full status is a sensitive read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    /// The session's observation token — accepted for `status` in place of the
    /// control token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    /// Identity of the client performing the action; recorded on `start`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_instance_id: Option<String>,
    /// Optional app/window target for `action: start`. When set, the session
    /// is scoped to that target: observe defaults to it, act rejects
    /// coordinates outside it, and keyboard focus is validated against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<crate::sessions::SessionTarget>,
    /// Pointer isolation policy for `action: start`. Default
    /// `isolated_preferred` (never silently borrow the user's cursor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_policy: Option<crate::pointer::PointerPolicy>,
    /// Keyboard focus policy for `action: start`. Default `strict` (never
    /// steal foreground; Type/Key fail with INPUT_FOCUS_MISMATCH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_policy: Option<crate::sessions::FocusPolicy>,
}

/// `session.summary` — the **public** view of the active session. No token
/// needed: it exposes only coarse state and non-secret owner identity. Full
/// `status` (which includes `display_id`, `frame_id`, `trace_dir`) requires an
/// observation or control token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSummary {
    /// `null` when no session exists — every field is always present on the
    /// wire (explicit nulls, never omitted keys): consumers read
    /// `summary.session_id == null` without juggling absence.
    pub session_id: Option<String>,
    pub state: Option<SessionState>,
    /// True when the session is the control-lock holder.
    pub lock_held: bool,
    /// The non-secret identity of the creating client (name only — never a
    /// token, never an instance id or frame/trace paths).
    pub owner_client_id: Option<String>,
    pub owner_client_name: Option<String>,
    /// Human-readable hint for the common case: the active session is owned by
    /// another client.
    pub message: Option<String>,
}

/// `runtime.version` result — the protocol-version contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuntimeVersionResult {
    pub name: String,
    /// Wire name is `runtime_version` (the protocol spec's field name); the
    /// Rust field stays `version` to avoid `version.version`-style confusion.
    #[serde(rename = "runtime_version")]
    pub version: String,
    pub protocol_version: u32,
    /// Inclusive lower bound of the client protocol versions this daemon
    /// accepts. A client below this (or above `maximum_client_protocol_version`)
    /// gets `PROTOCOL_VERSION_MISMATCH`.
    pub minimum_client_protocol_version: u32,
    pub maximum_client_protocol_version: u32,
    /// Identity of the running daemon instance, generated at startup. A client
    /// holding an admin credential compares this against the instance id
    /// recorded in its credential file before using it to shut the daemon
    /// down — a credential from a different daemon install is stale.
    pub daemon_instance_id: String,
}

/// `computer.session` result (shape depends on `action`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionResult {
    pub session_id: String,
    pub state: SessionState,
    pub paused: bool,
    pub user_takeover: bool,
    pub lock_held: bool,
    pub display_id: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_action_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_dir: Option<String>,
    pub started_by: String,
    /// The session's control token. **Only present in the `start` response** —
    /// it is issued exactly once, on creation, and never repeated by `status`
    /// or any other read-only call. Keep it in memory (or the CLI's 0600
    /// credential file), never in logs or traces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    /// The session's observation token (read-only capability). **Only present
    /// in the `start` response**, like the control token. A holder of only
    /// this token can observe/inspect/read traces, but can never act, cancel,
    /// pause, or stop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    /// Who created this session (backward-compatible name of the starting
    /// client). The owner_* fields carry the structured identity; only the
    /// creating client may stop the session on exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `computer.cancel` request.
///
/// Cancellation is **precise**: with `request_id` set, only the request with
/// that JSON-RPC id (on the *same connection*) is cancelled — the runtime
/// keys the in-flight batch by `(connection_id, request_id)`, so cancelling
/// request A never touches request B, and client A can never cancel client B's
/// request even with an identical id. Without `request_id` the whole session's
/// in-flight batch is cancelled (still token-verified).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CancelParams {
    pub session_id: String,
    /// The session's control token — required; cancelling is a mutating op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    /// JSON-RPC id of the specific request to cancel (same connection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<serde_json::Value>,
}

/// `computer.cancel` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CancelResult {
    pub cancelled: bool,
    pub session_id: String,
}

/// `trace.get` / `trace.replay` request — trace contents are a sensitive read;
/// an observation or control token is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceGetParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
}

/// `trace.export` request (round 7) — a **pure read**. The daemon never
/// accepts a destination path: exporting a trace requires an observation or
/// control token and returns the content inline (plus its SHA-256), so an
/// observation-capable caller cannot write anywhere through the runtime.
/// Saving the content to a user-chosen location is the client's job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceExportParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
}

/// `trace.replay` request (token-verified like `trace.get`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceReplayParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
}

/// `runtime.shutdown` request — requires the daemon admin token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ShutdownParams {
    /// The daemon admin token (per-install credential held by the daemon
    /// manager — the CLI / LaunchAgent). Ordinary clients never hold it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token: Option<SecretToken>,
}

/// `trace.list` request (round 6): **session-scoped**. A session's capability
/// tokens authorize reads of *that session's* traces only — never a
/// cross-session listing. Verified against the live session, or against the
/// persisted trace access manifest after a daemon restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceListParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    /// Maximum entries to return (newest first). Absent = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `trace.summaries` request — same session-scoped shape and verification as
/// `trace.list`; the response is the raw summary array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceSummariesParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
    /// Maximum entries to return (newest first). Absent = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `trace.admin_list` request — the **daemon-manager** listing across all
/// sessions, authorized by the daemon admin token (never by a session
/// capability). Session tokens must not reveal which other sessions ever ran
/// on the machine; only the operator (CLI / LaunchAgent) may see that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceAdminListParams {
    /// The daemon admin token, same credential as `runtime.shutdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_token: Option<SecretToken>,
    /// Maximum entries to return (newest first). Absent = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `trace.list` / `trace.summaries` entry — metadata only. The absolute
/// filesystem path never crosses the wire (a path would leak the install
/// layout and invite path-based probing); contents are read via `trace.get`
/// and exported via `trace.export`, both token-gated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceSummary {
    /// Stable id of this trace. One trace per session — `trace_id` is the
    /// trace-file stem, which is currently the session id.
    pub trace_id: String,
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub size_bytes: u64,
    pub event_count: usize,
}

/// Shared token fields for the remaining **cross-session** sensitive reads:
/// `runtime.pointer`, `runtime.active_application`, `runtime.desktop_layout`.
/// (The trace reads became session-scoped in round 6.) These methods have no
/// `session_id`, so any valid observation or control token is accepted — the
/// token proves the caller is a trusted client of this daemon. No token →
/// `OBSERVATION_TOKEN_REQUIRED`; a token matching nothing →
/// `INVALID_OBSERVATION_TOKEN`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct CapabilityTokenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_token: Option<SecretToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<SecretToken>,
}

/// One entry inside a trace file (JSONL).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceEntry {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction: Option<crate::actions::RedactedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_application: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
}

/// `trace.export` result (round 7) — content, never a filesystem path.
/// The daemon performs no filesystem writes for an export; `file_name` is a
/// server-suggested name for a client-side save.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraceExportResult {
    /// Stable id of this trace (one per session — the trace-file stem).
    pub trace_id: String,
    pub session_id: String,
    /// Wire format of `content`, currently always `"jsonl"`.
    pub format: String,
    /// MIME type of `content`, e.g. `application/x-ndjson`.
    pub mime_type: String,
    /// Server-suggested file name (`s_<session_id>.jsonl`) — never a path.
    pub file_name: String,
    /// The trace content itself (redaction applied at record time).
    pub content: String,
    pub size_bytes: u64,
    /// Hex SHA-256 of `content`, so a saved copy can be verified locally.
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinates::{CoordinateSpace, Point};

    #[test]
    fn act_params_round_trip_with_forward_safety_hooks() {
        let p = ActParams {
            session_id: "s1".into(),
            frame_id: "frame_9".into(),
            actions: vec![crate::actions::ComputerAction::Move {
                x: 10.0,
                y: 20.0,
                coordinate_space: CoordinateSpace::Normalized1000,
                duration_ms: None,
            }],
            wait_policy: Some(WaitPolicy::UntilStable),
            fixed_wait_ms: Some(300),
            return_screenshot: Some(true),
            risk_level: Some("high".into()),
            requires_confirmation: Some(true),
            policy_context: Some("user-requested file deletion".into()),
            control_token: Some("secret-token".into()),
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: ActParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.session_id, "s1");
        assert_eq!(back.risk_level.as_deref(), Some("high"));
        assert_eq!(back.requires_confirmation, Some(true));
        assert_eq!(back.control_token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn rpc_request_parses() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"runtime.health"}"#;
        let req: RpcRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.method, "runtime.health");
    }

    #[test]
    fn region_serializes_with_coordinate_space() {
        let r = Region {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
            coordinate_space: CoordinateSpace::ImagePixels,
        };
        let v = serde_json::to_value(r).unwrap();
        assert_eq!(v["coordinate_space"], "image_pixels");
        let _ = Point::new(0.0, 0.0);
    }

    #[test]
    fn observe_params_defaults_are_none() {
        let v = serde_json::json!({});
        let p: ObserveParams = serde_json::from_value(v).unwrap();
        assert_eq!(p.session_id, None);
        assert_eq!(p.max_width, None);
        assert_eq!(p.include_image, None);
    }

    #[test]
    fn observe_include_image_flag_round_trips() {
        let v = serde_json::json!({ "include_image": true });
        let p: ObserveParams = serde_json::from_value(v).unwrap();
        assert_eq!(p.include_image, Some(true));
    }

    /// Round 5: derived `Debug` on any token-bearing struct must never print
    /// the plaintext — the `SecretToken` field type redacts structurally, so
    /// no struct holding one needs a hand-written `Debug`.
    #[test]
    fn derived_debug_on_token_bearing_structs_redacts() {
        let p = ActParams {
            session_id: "s1".into(),
            frame_id: "frame_9".into(),
            actions: Vec::new(),
            control_token: Some(SecretToken::new("plaintext-control-token")),
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
        };
        let d = format!("{p:?}");
        assert!(d.contains("[REDACTED]"));
        assert!(
            !d.contains("plaintext-control-token"),
            "Debug of ActParams must not contain the control token"
        );

        let obs = ObserveParams {
            observation_token: Some(SecretToken::new("plaintext-obs-token")),
            control_token: Some(SecretToken::new("plaintext-ctl-token")),
            ..Default::default()
        };
        let d = format!("{obs:?}");
        assert!(!d.contains("plaintext-obs-token"));
        assert!(!d.contains("plaintext-ctl-token"));

        let s = ShutdownParams {
            admin_token: Some(SecretToken::new("plaintext-admin-token")),
        };
        let d = format!("{s:?}");
        assert!(!d.contains("plaintext-admin-token"));
    }

    #[test]
    fn rpc_envelope_debug_redacts_token_fields_in_params_and_results() {
        // A start response carries both capability tokens exactly once; its
        // Debug form (a log line, a panic message) must never print them.
        let resp = RpcResponse::ok(
            Some(serde_json::json!(1)),
            serde_json::json!({
                "session_id": "s1",
                "control_token": "plaintext-control",
                "observation_token": "plaintext-obs",
            }),
        );
        let d = format!("{resp:?}");
        assert!(d.contains("[REDACTED]"));
        assert!(!d.contains("plaintext-control"));
        assert!(!d.contains("plaintext-obs"));

        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(2)),
            method: "computer.act".into(),
            params: Some(serde_json::json!({
                "session_id": "s1",
                "control_token": "plaintext-control",
            })),
        };
        let d = format!("{req:?}");
        assert!(!d.contains("plaintext-control"));
        assert!(d.contains("computer.act"), "method stays visible");

        let err = RpcResponse::err(
            Some(serde_json::json!(3)),
            -32024,
            "OBSERVATION_TOKEN_REQUIRED".into(),
            Some(serde_json::json!({ "admin_token": "plaintext-admin" })),
        );
        let d = format!("{err:?}");
        assert!(!d.contains("plaintext-admin"));
    }

    /// The wire format is unchanged by the typed token: params serialize as
    /// plain strings, exactly as before the type was introduced.
    #[test]
    fn token_fields_serialize_as_plain_strings() {
        let p = ObserveParams {
            session_id: Some("s1".into()),
            observation_token: Some(SecretToken::new("wire-obs")),
            ..Default::default()
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["observation_token"], "wire-obs");
        assert_eq!(v["session_id"], "s1");
        // skip_serializing_if still drops absent tokens.
        let none = ObserveParams::default();
        let v = serde_json::to_value(&none).unwrap();
        assert!(v.get("observation_token").is_none());
    }
}
