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
use crate::sessions::{SessionAction, SessionState};

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC 2.0 request as received by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response written by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ObserveParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

/// `computer.act` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActParams {
    pub session_id: String,
    pub frame_id: String,
    pub actions: Vec<ComputerAction>,
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

/// Result of one action inside a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResultReport {
    pub index: usize,
    pub status: String, // success | failed | cancelled
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `computer.act` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActResult {
    pub executed: bool,
    pub action_results: Vec<ActionResultReport>,
    pub screen_changed: bool,
    pub stable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ObserveResult>,
}

/// `computer.inspect` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectParams {
    pub session_id: String,
    pub frame_id: String,
    pub region: Region,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u32>,
}

/// Mapping info that lets a model safely translate an inspect-relative
/// coordinate back into global desktop coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectMapping {
    /// Where the crop sits in the original image (pixels).
    pub source_image_rect: Region,
    /// Global desktop point corresponding to the crop's top-left corner.
    pub global_origin: (f64, f64),
    /// The crop's top-left corner in the original frame's normalized_1000 space.
    pub normalized_1000_origin: (f64, f64),
}

/// `computer.inspect` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectResult {
    pub session_id: String,
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub image_base64: String,
    pub image_mime_type: String,
    pub mapping: InspectMapping,
}

/// `computer.session` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionParams {
    pub action: SessionAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
}

/// `computer.session` result (shape depends on `action`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `trace.list` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceSummary {
    pub session_id: String,
    pub path: String,
    pub entries: usize,
    pub bytes: u64,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_entry_at: Option<DateTime<Utc>>,
}

/// One entry inside a trace file (JSONL).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// `trace.export` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceExport {
    pub session_id: String,
    pub path: String,
    pub format: String,
    pub exported_at: DateTime<Utc>,
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
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: ActParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.session_id, "s1");
        assert_eq!(back.risk_level.as_deref(), Some("high"));
        assert_eq!(back.requires_confirmation, Some(true));
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
}
