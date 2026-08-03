//! Structured error types shared across every layer of the runtime.
//!
//! Every failure path in Computer Use returns a [`CuError`]. The type carries a
//! stable machine-readable `code` that flows unchanged over JSON-RPC, plus
//! optional structured detail (e.g. stale-frame scores, permission guidance) so
//! upper-layer agents can react instead of parsing prose.

use serde::{Deserialize, Serialize};

/// Well-known structured error codes. These are the stable strings that appear
/// in the `data.code` field of a JSON-RPC error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
        }
    }
}

/// Which macOS permission is missing. Used to generate actionable guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    ScreenRecording,
    Accessibility,
}

/// Extra structured detail attached to a [`CuError::Permission`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionIssue {
    pub kind: PermissionKind,
    pub granted: bool,
    pub guidance: String,
}

/// Detail attached to a [`CuError::StaleFrame`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleFrameDetail {
    pub referenced_frame_id: String,
    pub current_frame_id: String,
    pub change_score: f64,
    pub reason: String,
}

/// Detail attached to a [`CuError::OutOfBounds`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundsDetail {
    pub coordinate_space: String,
    pub x: f64,
    pub y: f64,
    pub image_width: u32,
    pub image_height: u32,
}

/// Detail attached to a [`CuError::ConfirmationRequired`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    #[error("control lock is held by {holder}")]
    ControlLocked { holder: String },
    #[error("session is paused")]
    Paused,
    #[error("user takeover is active")]
    UserTakeover,
    #[error("action is out of bounds: {0:?}")]
    OutOfBounds(BoundsDetail),
    #[error("unknown frame: {0}")]
    UnknownFrame(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("invalid session state: {0}")]
    InvalidSessionState(String),
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
            OutOfBounds(_) => ErrorCode::OutOfBounds,
            UnknownFrame(_) => ErrorCode::UnknownFrame,
            SessionNotFound(_) => ErrorCode::SessionNotFound,
            InvalidSessionState(_) => ErrorCode::InvalidSessionState,
            ConfirmationRequired(_) => ErrorCode::ConfirmationRequired,
            Cancelled => ErrorCode::Cancelled,
            Trace(_) => ErrorCode::TraceError,
            Driver(_) => ErrorCode::DriverError,
            Unsupported(_) => ErrorCode::Unsupported,
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
            CuError::ControlLocked { holder } => {
                map.insert("holder".into(), holder.clone().into());
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
