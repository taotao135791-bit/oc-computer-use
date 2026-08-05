//! Structured error types shared across every layer of the runtime.
//!
//! Every failure path in Computer Use returns a [`CuError`]. The type carries a
//! stable machine-readable `code` that flows unchanged over JSON-RPC, plus
//! optional structured detail (e.g. stale-frame scores, permission guidance) so
//! upper-layer agents can react instead of parsing prose.

use serde::{Deserialize, Serialize};

/// Well-known structured error codes. These are the stable strings that appear
/// in the `data.code` field of a JSON-RPC error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    InvalidParams,
    MethodNotFound,
    Internal,
    NotReady,
    Permission,
    StaleFrame,
    ControlLocked,
    Paused,
    UserTakeover,
    OutOfBounds,
    UnknownFrame,
    SessionNotFound,
    InvalidSessionState,
    ConfirmationRequired,
    Cancelled,
    TraceError,
    DriverError,
    Unsupported,
    /// Attempted to `resume` a session that is in `UserTakeover`. The user
    /// must `release` first; resume can only recover a paused session.
    UserTakeoverActive,
    /// A request/action batch exceeded the daemon's request deadline. This is
    /// distinct from `Cancelled` (explicit cancellation): the batch was still
    /// running when the deadline hit.
    ActionTimeout,
    /// Capturing the screen failed (driver/capture failure, e.g. the screen
    /// recording session dropped).
    CaptureFailed,
    /// A mutating operation was attempted without a control token.
    ControlTokenRequired,
    /// A control token was presented but did not verify. The error is
    /// deliberately non-descriptive — it must not hint whether the token was
    /// malformed, wrong-length, or simply not this session's.
    InvalidControlToken,
    /// A sensitive read (observe / inspect / status / trace) was attempted
    /// without an observation or control token. The session id alone grants
    /// no observation permission.
    ObservationTokenRequired,
    /// A token was presented for a sensitive read but did not verify as either
    /// the session's observation or control token. Deliberately
    /// non-descriptive, like `InvalidControlToken`.
    InvalidObservationToken,
    /// A mutating operation targeted a session that is already stopped.
    SessionStopped,
    /// The SDK-side request deadline expired. Distinct from `ActionTimeout`
    /// (daemon-side): the client timed out waiting, and reports whether the
    /// runtime confirmed the cancellation.
    RequestTimeout,
    /// The client's protocol version is incompatible with the daemon's.
    ProtocolVersionMismatch,
    /// `runtime.shutdown` was attempted without the daemon admin token.
    DaemonAdminTokenRequired,
    /// An admin token was presented for `runtime.shutdown` but did not verify.
    /// Deliberately non-descriptive.
    InvalidDaemonAdminToken,
}

impl ErrorCode {
    /// JSON-RPC error code used in the top-level `error.code` field.
    pub fn jsonrpc_code(self) -> i64 {
        use ErrorCode::*;
        match self {
            ParseError => -32700,
            InvalidRequest => -32600,
            MethodNotFound => -32601,
            InvalidParams => -32602,
            // -32000..=-32099 are reserved for application errors.
            Internal => -32000,
            NotReady => -32001,
            Permission => -32002,
            StaleFrame => -32003,
            ControlLocked => -32004,
            Paused => -32005,
            UserTakeover => -32006,
            OutOfBounds => -32007,
            UnknownFrame => -32008,
            SessionNotFound => -32009,
            InvalidSessionState => -32010,
            ConfirmationRequired => -32011,
            Cancelled => -32012,
            TraceError => -32013,
            DriverError => -32014,
            Unsupported => -32015,
            UserTakeoverActive => -32016,
            ActionTimeout => -32017,
            CaptureFailed => -32018,
            ControlTokenRequired => -32019,
            InvalidControlToken => -32020,
            SessionStopped => -32021,
            RequestTimeout => -32022,
            ProtocolVersionMismatch => -32023,
            ObservationTokenRequired => -32024,
            InvalidObservationToken => -32025,
            DaemonAdminTokenRequired => -32026,
            InvalidDaemonAdminToken => -32027,
        }
    }

    pub fn as_str(self) -> &'static str {
        use ErrorCode::*;
        match self {
            ParseError => "PARSE_ERROR",
            InvalidRequest => "INVALID_REQUEST",
            MethodNotFound => "METHOD_NOT_FOUND",
            InvalidParams => "INVALID_PARAMS",
            Internal => "INTERNAL",
            NotReady => "NOT_READY",
            Permission => "PERMISSION",
            StaleFrame => "STALE_FRAME",
            ControlLocked => "CONTROL_LOCKED",
            Paused => "PAUSED",
            UserTakeover => "USER_TAKEOVER",
            OutOfBounds => "OUT_OF_BOUNDS",
            UnknownFrame => "UNKNOWN_FRAME",
            SessionNotFound => "SESSION_NOT_FOUND",
            InvalidSessionState => "INVALID_SESSION_STATE",
            ConfirmationRequired => "CONFIRMATION_REQUIRED",
            Cancelled => "CANCELLED",
            TraceError => "TRACE_ERROR",
            DriverError => "DRIVER_ERROR",
            Unsupported => "UNSUPPORTED",
            UserTakeoverActive => "USER_TAKEOVER_ACTIVE",
            ActionTimeout => "ACTION_TIMEOUT",
            CaptureFailed => "CAPTURE_FAILED",
            ControlTokenRequired => "CONTROL_TOKEN_REQUIRED",
            InvalidControlToken => "INVALID_CONTROL_TOKEN",
            ObservationTokenRequired => "OBSERVATION_TOKEN_REQUIRED",
            InvalidObservationToken => "INVALID_OBSERVATION_TOKEN",
            SessionStopped => "SESSION_STOPPED",
            RequestTimeout => "REQUEST_TIMEOUT",
            ProtocolVersionMismatch => "PROTOCOL_VERSION_MISMATCH",
            DaemonAdminTokenRequired => "DAEMON_ADMIN_TOKEN_REQUIRED",
            InvalidDaemonAdminToken => "INVALID_DAEMON_ADMIN_TOKEN",
        }
    }
}

/// Which macOS permission is missing. Used to generate actionable guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    ScreenRecording,
    Accessibility,
}

/// Extra structured detail attached to a [`CuError::Permission`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionIssue {
    pub kind: PermissionKind,
    pub granted: bool,
    pub guidance: String,
}

/// Detail attached to a [`CuError::StaleFrame`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StaleFrameDetail {
    pub referenced_frame_id: String,
    pub current_frame_id: String,
    pub change_score: f64,
    pub reason: String,
}

/// Detail attached to a [`CuError::OutOfBounds`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BoundsDetail {
    pub coordinate_space: String,
    pub x: f64,
    pub y: f64,
    pub image_width: u32,
    pub image_height: u32,
}

/// Detail attached to a [`CuError::ConfirmationRequired`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfirmationDetail {
    pub reason: String,
    pub risk_level: String,
    pub requires_confirmation: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_context: Option<String>,
}

/// The single error type used across the runtime, CLI, and daemon.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CuError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("runtime is not ready: {0}")]
    NotReady(String),
    #[error("macOS permission required")]
    Permission(PermissionIssue),
    #[error("stale frame: referenced {0:?} is out of date")]
    StaleFrame(StaleFrameDetail),
    /// Another client owns the active session. `holder` is the other session
    /// id; `owner` is the identity the holder's creator reported (non-secret
    /// metadata, useful for "Owner: OpenCode" style errors — never a token).
    #[error("control lock is held by {holder}")]
    ControlLocked {
        holder: String,
        owner: Option<crate::protocol::ClientInfo>,
    },
    #[error("session is paused")]
    Paused,
    #[error("user takeover is active")]
    UserTakeover,
    /// `resume` while the user holds control. The agent must `release` first.
    #[error("The user has taken control. Call release before resuming agent control.")]
    UserTakeoverActive,
    #[error("request timed out: {0}")]
    ActionTimeout(String),
    #[error("screen capture failed: {0}")]
    CaptureFailed(String),
    #[error("action is out of bounds: {0:?}")]
    OutOfBounds(BoundsDetail),
    #[error("unknown frame: {0}")]
    UnknownFrame(String),
    #[error("{0}")]
    SessionNotFound(String),
    /// A mutating operation required this session's control token, and none
    /// was supplied.
    #[error("This operation requires the session control token. Only the client that started the session has it; a session id alone grants no control.")]
    ControlTokenRequired,
    /// A control token was supplied but did not verify. The message must not
    /// reveal whether the token was malformed, wrong-length, or not this
    /// session's — that would leak verification details.
    #[error("Invalid control token for this session.")]
    InvalidControlToken,
    /// A sensitive read (observe / inspect / status / trace) required an
    /// observation or control token and none was supplied. A session id alone
    /// grants no observation permission.
    #[error("This operation requires the session observation token (or its control token). A session id alone grants no observation permission.")]
    ObservationTokenRequired,
    /// A token was presented for a sensitive read but did not verify as the
    /// session's observation or control token. Deliberately non-descriptive.
    #[error("Invalid observation token for this session.")]
    InvalidObservationToken,
    /// A mutating operation targeted a session that is already stopped.
    #[error("session is stopped")]
    SessionStopped,
    #[error("invalid session state: {0}")]
    InvalidSessionState(String),
    /// The SDK-side request deadline expired; `confirmed` records whether the
    /// runtime acknowledged the cancellation.
    #[error("request timed out: {0}")]
    RequestTimeout(String),
    /// Protocol version mismatch between client and daemon.
    #[error("protocol version mismatch: daemon expects v{expected}, client is {got:?}")]
    ProtocolVersionMismatch { expected: u32, got: Option<u32> },
    #[error("confirmation required")]
    ConfirmationRequired(ConfirmationDetail),
    #[error("operation cancelled")]
    Cancelled,
    #[error("trace error: {0}")]
    Trace(String),
    #[error("driver error: {0}")]
    Driver(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// `runtime.shutdown` requires the daemon admin token (a per-install
    /// credential only the daemon manager — CLI / LaunchAgent — holds).
    #[error("This operation requires the daemon admin token. Ordinary clients cannot shut the daemon down.")]
    DaemonAdminTokenRequired,
    /// An admin token was presented but did not verify. Deliberately
    /// non-descriptive.
    #[error("Invalid daemon admin token.")]
    InvalidDaemonAdminToken,
}

impl CuError {
    pub fn code(&self) -> ErrorCode {
        use CuError::*;
        match self {
            Parse(_) => ErrorCode::ParseError,
            InvalidRequest(_) => ErrorCode::InvalidRequest,
            MethodNotFound(_) => ErrorCode::MethodNotFound,
            InvalidParams(_) => ErrorCode::InvalidParams,
            Internal(_) => ErrorCode::Internal,
            NotReady(_) => ErrorCode::NotReady,
            Permission(_) => ErrorCode::Permission,
            StaleFrame(_) => ErrorCode::StaleFrame,
            ControlLocked { .. } => ErrorCode::ControlLocked,
            Paused => ErrorCode::Paused,
            UserTakeover => ErrorCode::UserTakeover,
            UserTakeoverActive => ErrorCode::UserTakeoverActive,
            ActionTimeout(_) => ErrorCode::ActionTimeout,
            CaptureFailed(_) => ErrorCode::CaptureFailed,
            ControlTokenRequired => ErrorCode::ControlTokenRequired,
            InvalidControlToken => ErrorCode::InvalidControlToken,
            ObservationTokenRequired => ErrorCode::ObservationTokenRequired,
            InvalidObservationToken => ErrorCode::InvalidObservationToken,
            SessionStopped => ErrorCode::SessionStopped,
            RequestTimeout(_) => ErrorCode::RequestTimeout,
            ProtocolVersionMismatch { .. } => ErrorCode::ProtocolVersionMismatch,
            OutOfBounds(_) => ErrorCode::OutOfBounds,
            UnknownFrame(_) => ErrorCode::UnknownFrame,
            SessionNotFound(_) => ErrorCode::SessionNotFound,
            InvalidSessionState(_) => ErrorCode::InvalidSessionState,
            ConfirmationRequired(_) => ErrorCode::ConfirmationRequired,
            Cancelled => ErrorCode::Cancelled,
            Trace(_) => ErrorCode::TraceError,
            Driver(_) => ErrorCode::DriverError,
            Unsupported(_) => ErrorCode::Unsupported,
            DaemonAdminTokenRequired => ErrorCode::DaemonAdminTokenRequired,
            InvalidDaemonAdminToken => ErrorCode::InvalidDaemonAdminToken,
        }
    }

    /// Serialize this error into the `data` object of a JSON-RPC error
    /// response, keeping the structured detail that upper layers depend on.
    pub fn to_error_data(&self) -> serde_json::Value {
        let code = self.code().as_str();
        let mut map = serde_json::Map::new();
        map.insert("code".into(), serde_json::Value::String(code.into()));
        map.insert(
            "message".into(),
            serde_json::Value::String(self.to_string()),
        );
        match self {
            CuError::Permission(issue) => {
                map.insert(
                    "permission".into(),
                    serde_json::to_value(issue).unwrap_or(serde_json::Value::Null),
                );
            }
            CuError::StaleFrame(detail) => {
                map.insert(
                    "referenced_frame_id".into(),
                    detail.referenced_frame_id.clone().into(),
                );
                map.insert(
                    "current_frame_id".into(),
                    detail.current_frame_id.clone().into(),
                );
                map.insert("change_score".into(), detail.change_score.into());
                map.insert("reason".into(), detail.reason.clone().into());
            }
            CuError::OutOfBounds(detail) => {
                map.insert(
                    "bounds".into(),
                    serde_json::to_value(detail).unwrap_or(serde_json::Value::Null),
                );
            }
            CuError::ConfirmationRequired(detail) => {
                map.insert(
                    "confirmation".into(),
                    serde_json::to_value(detail).unwrap_or(serde_json::Value::Null),
                );
            }
            CuError::ControlLocked { holder, owner } => {
                map.insert("holder".into(), holder.clone().into());
                if let Some(o) = owner {
                    map.insert(
                        "owner".into(),
                        serde_json::to_value(o).unwrap_or(serde_json::Value::Null),
                    );
                }
            }
            CuError::ProtocolVersionMismatch { expected, got } => {
                map.insert("expected".into(), (*expected).into());
                map.insert(
                    "got".into(),
                    got.map(serde_json::Value::from)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            _ => {}
        }
        serde_json::Value::Object(map)
    }

    /// Build a complete JSON-RPC error object for this error.
    pub fn to_jsonrpc_error(&self, id: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": self.code().jsonrpc_code(),
                "message": self.code().as_str(),
                "data": self.to_error_data(),
            }
        })
    }
}

impl From<serde_json::Error> for CuError {
    fn from(e: serde_json::Error) -> Self {
        CuError::Parse(e.to_string())
    }
}

/// Convenience constructors used by higher layers.
impl CuError {
    pub fn permission(kind: PermissionKind, granted: bool) -> Self {
        let guidance = match kind {
            PermissionKind::ScreenRecording => {
                "grant Screen Recording access (System Settings > Privacy & Security > Screen & System Audio Recording), then restart the daemon"
            }
            PermissionKind::Accessibility => {
                "grant Accessibility access (System Settings > Privacy & Security > Accessibility), then restart the daemon"
            }
        };
        CuError::Permission(PermissionIssue {
            kind,
            granted,
            guidance: guidance.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_str_round_trips() {
        assert_eq!(ErrorCode::StaleFrame.as_str(), "STALE_FRAME");
        assert_eq!(ErrorCode::Permission.as_str(), "PERMISSION");
    }

    #[test]
    fn stale_frame_error_serializes_detail() {
        let err = CuError::StaleFrame(StaleFrameDetail {
            referenced_frame_id: "frame_1".into(),
            current_frame_id: "frame_3".into(),
            change_score: 0.42,
            reason: "desktop changed".into(),
        });
        let data = err.to_error_data();
        assert_eq!(data["code"], "STALE_FRAME");
        assert_eq!(data["referenced_frame_id"], "frame_1");
        assert_eq!(data["current_frame_id"], "frame_3");
        assert_eq!(data["change_score"], 0.42);
    }

    #[test]
    fn permission_error_serializes_guidance() {
        let err = CuError::permission(PermissionKind::Accessibility, false);
        let data = err.to_error_data();
        assert_eq!(data["permission"]["kind"], "accessibility");
        assert_eq!(data["permission"]["granted"], false);
        assert!(data["permission"]["guidance"]
            .as_str()
            .unwrap()
            .contains("Accessibility"));
    }

    #[test]
    fn timeout_and_capture_failed_have_codes() {
        assert_eq!(ErrorCode::ActionTimeout.as_str(), "ACTION_TIMEOUT");
        assert_eq!(ErrorCode::ActionTimeout.jsonrpc_code(), -32017);
        assert_eq!(ErrorCode::CaptureFailed.as_str(), "CAPTURE_FAILED");
        assert_eq!(ErrorCode::CaptureFailed.jsonrpc_code(), -32018);
        let err = CuError::ActionTimeout("batch exceeded deadline".into());
        let data = err.to_error_data();
        assert_eq!(data["code"], "ACTION_TIMEOUT");
        assert_eq!(err.code(), ErrorCode::ActionTimeout);
    }

    #[test]
    fn jsonrpc_codes_are_in_reserved_range() {
        for c in [
            ErrorCode::Internal,
            ErrorCode::StaleFrame,
            ErrorCode::Permission,
            ErrorCode::Cancelled,
        ] {
            let code = c.jsonrpc_code();
            assert!(
                (-32099..=-32000).contains(&code),
                "code {code} outside range"
            );
        }
    }
}
