//! Schema validation tests: real serialized payloads must pass the generated
//! JSON Schema (via the `jsonschema` crate), and the schema must stay complete
//! (every error code present, every capability-token field documented).

use chrono::Utc;
use jsonschema::Validator;
use serde_json::{json, Value};

use cu_core::actions::{ComputerAction, MouseButton, TextInputMethod, WaitPolicy};
use cu_core::coordinates::{CoordinateSpace, Point};
use cu_core::errors::ErrorCode;
use cu_core::protocol::{ActParams, CancelParams, ObserveResult, SessionResult, ShutdownParams};
use cu_core::sessions::SessionState;
use cu_protocol_gen::build_protocol_schema;

/// Compile the schema restricted to one `$defs` entry, so an instance is
/// validated against exactly that type.
fn compile_def(def_name: &str) -> Validator {
    let schema = build_protocol_schema();
    let defs = schema["$defs"].clone();
    Validator::new(&json!({ "$ref": format!("#/$defs/{def_name}"), "$defs": defs }))
        .unwrap_or_else(|e| panic!("schema for {def_name} does not compile: {e}"))
}

fn validate(def_name: &str, instance: &Value) -> Result<(), String> {
    let validator = compile_def(def_name);
    validator
        .validate(instance)
        .map_err(|e| format!("{def_name} rejected: {e}"))
}

#[test]
fn session_start_result_passes_the_schema_with_both_capability_tokens() {
    let session = SessionResult {
        session_id: "s1".into(),
        state: SessionState::Active,
        paused: false,
        user_takeover: false,
        lock_held: true,
        display_id: "1".into(),
        created_at: Utc::now(),
        last_action_at: None,
        current_frame_id: Some("frame_9".into()),
        trace_dir: Some("/tmp/cu-traces/s1".into()),
        started_by: "pi-extension".into(),
        // Issued exactly once, in the start response.
        control_token: Some("fake-control-token".into()),
        observation_token: Some("fake-observation-token".into()),
        owner_client_id: Some("client-1".into()),
        owner_client_name: Some("Pi".into()),
        owner_instance_id: Some("instance-1".into()),
        message: None,
    };
    let v = serde_json::to_value(session).unwrap();
    validate("SessionResult", &v).unwrap();

    // Both capability tokens are documented properties of the start response…
    let schema = build_protocol_schema();
    let props = schema["$defs"]["SessionResult"]["properties"]
        .as_object()
        .unwrap();
    assert!(props.contains_key("control_token"));
    assert!(props.contains_key("observation_token"));
    // …but optional on the wire (status/stop never carry them).
    let required = schema["$defs"]["SessionResult"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(required.contains(&"session_id"));
    assert!(!required.contains(&"control_token"));
    assert!(!required.contains(&"observation_token"));
}

#[test]
fn act_params_passes_the_schema_with_a_real_action_batch() {
    let params = ActParams {
        session_id: "s1".into(),
        frame_id: "frame_9".into(),
        actions: vec![
            ComputerAction::Click {
                x: 123.0,
                y: 456.0,
                button: MouseButton::Left,
                coordinate_space: CoordinateSpace::Normalized1000,
            },
            ComputerAction::Drag {
                from: Point::new(1.0, 2.0),
                to: Point::new(3.0, 4.0),
                coordinate_space: CoordinateSpace::ImagePixels,
                duration_ms: Some(250),
            },
            ComputerAction::TypeText {
                text: "hello".into(),
                method: TextInputMethod::Clipboard,
            },
        ],
        control_token: Some("fake-control-token".into()),
        wait_policy: Some(WaitPolicy::UntilStable),
        fixed_wait_ms: None,
        return_screenshot: Some(true),
        risk_level: Some("medium".into()),
        requires_confirmation: Some(false),
        policy_context: None,
    };
    let v = serde_json::to_value(params).unwrap();
    validate("ActParams", &v).unwrap();
    // The internally-tagged action union: every variant carries its `type`.
    assert_eq!(v["actions"][0]["type"], "click");
    assert_eq!(v["actions"][1]["type"], "drag");
    assert_eq!(v["actions"][2]["type"], "type");

    // A batch without a control token is still schema-valid (the daemon
    // rejects it at runtime with CONTROL_TOKEN_REQUIRED).
    let mut no_token = v.clone();
    no_token.as_object_mut().unwrap().remove("control_token");
    validate("ActParams", &no_token).unwrap();
}

#[test]
fn observe_result_passes_the_schema_including_the_rfc3339_timestamp() {
    let result = ObserveResult {
        session_id: "s1".into(),
        frame_id: "frame_9".into(),
        width: 1512,
        height: 982,
        display_id: "1".into(),
        scale_factor: 2.0,
        active_application: Some("Finder".into()),
        active_window: None,
        image_base64: Some("aGVsbG8=".into()),
        image_path: "/tmp/cu-frames/frame_9.png".into(),
        image_mime_type: "image/png".into(),
        captured_at: Utc::now(),
    };
    let v = serde_json::to_value(result).unwrap();
    validate("ObserveResult", &v).unwrap();
}

#[test]
fn cancel_params_documents_the_precise_request_key() {
    let v = serde_json::to_value(CancelParams {
        session_id: "s1".into(),
        control_token: Some("fake-control-token".into()),
        request_id: Some(json!(42)),
    })
    .unwrap();
    validate("CancelParams", &v).unwrap();
    // `request_id` is an optional wire field (omitted = cancel the whole
    // in-flight batch); when present it must be the exact JSON-RPC id.
    let schema = build_protocol_schema();
    let props = schema["$defs"]["CancelParams"]["properties"]
        .as_object()
        .unwrap();
    assert!(props.contains_key("request_id"));
}

#[test]
fn shutdown_params_documents_the_admin_token_field() {
    let v = serde_json::to_value(ShutdownParams {
        admin_token: Some("fake-admin-token".into()),
    })
    .unwrap();
    validate("ShutdownParams", &v).unwrap();
    // The field is optional on the wire so the daemon can answer
    // DAEMON_ADMIN_TOKEN_REQUIRED; it is documented in the schema.
    let schema = build_protocol_schema();
    assert!(schema["$defs"]["ShutdownParams"]["properties"]
        .as_object()
        .unwrap()
        .contains_key("admin_token"));
}

#[test]
fn every_error_code_is_in_the_schema_and_nothing_more() {
    let schema = build_protocol_schema();
    // schemars splits doc-commented variants into separate oneOf branches;
    // flatten every branch's enum into one set.
    let mut schema_codes: Vec<&str> = schema["$defs"]["ErrorCode"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|branch| branch["enum"].as_array().unwrap().iter())
        .map(|c| c.as_str().unwrap())
        .collect();
    schema_codes.sort_unstable();

    // The complete list from the Rust enum — kept explicit so that adding a
    // variant to `ErrorCode` without updating the protocol surfaces here.
    let expected = [
        ErrorCode::ParseError.as_str(),
        ErrorCode::InvalidRequest.as_str(),
        ErrorCode::InvalidParams.as_str(),
        ErrorCode::MethodNotFound.as_str(),
        ErrorCode::Internal.as_str(),
        ErrorCode::NotReady.as_str(),
        ErrorCode::Permission.as_str(),
        ErrorCode::StaleFrame.as_str(),
        ErrorCode::ControlLocked.as_str(),
        ErrorCode::Paused.as_str(),
        ErrorCode::UserTakeover.as_str(),
        ErrorCode::OutOfBounds.as_str(),
        ErrorCode::UnknownFrame.as_str(),
        ErrorCode::SessionNotFound.as_str(),
        ErrorCode::InvalidSessionState.as_str(),
        ErrorCode::ConfirmationRequired.as_str(),
        ErrorCode::Cancelled.as_str(),
        ErrorCode::TraceError.as_str(),
        ErrorCode::DriverError.as_str(),
        ErrorCode::Unsupported.as_str(),
        ErrorCode::UserTakeoverActive.as_str(),
        ErrorCode::ActionTimeout.as_str(),
        ErrorCode::CaptureFailed.as_str(),
        ErrorCode::ControlTokenRequired.as_str(),
        ErrorCode::InvalidControlToken.as_str(),
        ErrorCode::ObservationTokenRequired.as_str(),
        ErrorCode::InvalidObservationToken.as_str(),
        ErrorCode::SessionStopped.as_str(),
        ErrorCode::RequestTimeout.as_str(),
        ErrorCode::ProtocolVersionMismatch.as_str(),
        ErrorCode::DaemonAdminTokenRequired.as_str(),
        ErrorCode::InvalidDaemonAdminToken.as_str(),
    ];
    assert_eq!(
        expected.len(),
        schema_codes.len(),
        "schema enum size mismatch"
    );
    for code in expected {
        assert!(
            schema_codes.contains(&code),
            "missing {code} in $defs.ErrorCode.enum"
        );
    }
}

#[test]
fn invalid_instances_are_rejected() {
    // Missing required `session_id` is rejected.
    let bad_session = json!({ "state": "active", "paused": false });
    assert!(validate("SessionResult", &bad_session).is_err());

    // A session-id-only read is *schema-valid* but the daemon rejects it with
    // OBSERVATION_TOKEN_REQUIRED — the schema documents permission via the
    // token fields, not by making them required.
    let no_token_observe = json!({
        "session_id": "s1",
        "target": "screen",
        "display_id": "1"
    });
    validate("ObserveParams", &no_token_observe).unwrap();

    // Unknown error codes are rejected.
    assert!(validate("ErrorCode", &json!("NOT_A_REAL_CODE")).is_err());
    assert!(validate("ErrorCode", &json!("STALE_FRAME")).is_ok());
}

#[test]
fn protocol_meta_matches_the_version_constants() {
    let schema = build_protocol_schema();
    let meta = &schema["x-protocol-meta"];
    assert_eq!(
        meta,
        &json!({
            "protocol_version": cu_core::security::PROTOCOL_VERSION,
            "minimum_client_protocol_version": cu_core::security::MIN_CLIENT_PROTOCOL_VERSION,
            "maximum_client_protocol_version": cu_core::security::MAX_CLIENT_PROTOCOL_VERSION,
            "jsonrpc_version": "2.0",
        })
    );
}

#[test]
fn every_capability_token_field_marks_the_session_id_alone_as_insufficient() {
    // The observation/control token slots exist on every sensitive call.
    let schema = build_protocol_schema();
    for def in [
        "ObserveParams",
        "InspectParams",
        "TraceGetParams",
        "TraceExportParams",
        "TraceReplayParams",
    ] {
        let props = schema["$defs"][def]["properties"].as_object().unwrap();
        assert!(
            props.contains_key("observation_token"),
            "{def} missing observation_token"
        );
        assert!(
            props.contains_key("control_token"),
            "{def} missing control_token"
        );
    }
}
