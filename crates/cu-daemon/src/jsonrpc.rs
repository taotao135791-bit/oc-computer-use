//! JSON-RPC 2.0 dispatch: turns wire requests into runtime calls.
//!
//! Every method maps to exactly one runtime operation. Errors are converted to
//! JSON-RPC error responses carrying the machine-readable `data.code` plus any
//! structured detail (stale-frame scores, permission guidance, …) so upper
//! layers can react instead of parsing prose.

use std::path::Path;

use cu_core::{
    ActParams, CancelParams, CancelResult, CuError, InspectParams, ObserveParams, RequestKey,
    RpcRequest, RpcResponse, SessionParams, TraceSummary,
};
use cu_runtime::Runtime;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// Convert any value to a JSON-RPC `result`, mapping serialization failures to
/// an internal error instead of swallowing them.
fn to_result<T: Serialize>(v: T) -> Result<serde_json::Value, CuError> {
    serde_json::to_value(v).map_err(|e| CuError::Internal(format!("cannot serialize result: {e}")))
}

/// Parse request params into the typed wire struct.
fn parse_params<T: DeserializeOwned>(params: &serde_json::Value) -> Result<T, CuError> {
    serde_json::from_value(params.clone())
        .map_err(|e| CuError::InvalidParams(format!("invalid params: {e}")))
}

/// A `session_id` used to address a trace file. Restricting the charset stops a
/// caller from reading arbitrary files via path traversal.
fn validate_session_id(id: &str) -> Result<(), CuError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CuError::InvalidParams(format!("invalid session_id `{id}`")));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct TraceGetParams {
    session_id: String,
}

#[derive(serde::Deserialize)]
struct TraceExportParams {
    session_id: String,
    dest: String,
}

#[derive(serde::Deserialize)]
struct TraceReplayParams {
    session_id: String,
}

/// Dispatch one request. Never panics; every path yields a response.
///
/// `connection_id` identifies the connection the request arrived on. Together
/// with the request's JSON-RPC id it forms the [`RequestKey`] that scopes
/// cancellation: `computer.cancel` may only cancel requests issued on the
/// *same* connection (and only with the session's control token).
pub async fn dispatch(
    runtime: &std::sync::Arc<Runtime>,
    app_shutdown: &CancellationToken,
    connection_id: u64,
    req: RpcRequest,
) -> RpcResponse {
    let id = req.id.clone();
    let method = req.method.clone();
    let request_id = id.as_ref().map(|v| v.to_string());
    let request_key = id.as_ref().map(|v| RequestKey {
        connection_id,
        request_id: v.clone(),
    });
    let params = req.params.clone().unwrap_or(serde_json::Value::Null);

    let result = match method.as_str() {
        // --- runtime introspection ---
        "runtime.health" => runtime.health().await,
        "runtime.version" => {
            // A client may advertise its protocol version. A mismatch is an
            // explicit PROTOCOL_VERSION_MISMATCH (never a confusing success),
            // so an old SDK talking to a new daemon fails loudly instead of
            // misbehaving. Clients that don't advertise still get the version
            // to check themselves; their tokenless mutating calls will fail
            // with CONTROL_TOKEN_REQUIRED regardless.
            if let serde_json::Value::Object(map) = &params {
                if let Some(serde_json::Value::Number(n)) = map.get("protocol_version") {
                    let got = n.as_u64().unwrap_or(u64::MAX) as u32;
                    if got != cu_core::security::PROTOCOL_VERSION {
                        return error_response(
                            id,
                            CuError::ProtocolVersionMismatch {
                                expected: cu_core::security::PROTOCOL_VERSION,
                                got: Some(got),
                            },
                        );
                    }
                }
            }
            Ok(serde_json::json!({
                "name": cu_core::config::RUNTIME_NAME,
                "version": cu_core::config::RUNTIME_VERSION,
                "protocol_version": cu_core::security::PROTOCOL_VERSION,
            }))
        }
        "runtime.permissions" => runtime.permissions().await.and_then(to_result),
        "runtime.displays" => runtime.displays().await.and_then(to_result),
        "runtime.desktop_layout" => runtime.desktop_layout().await.and_then(to_result),
        "runtime.pointer" => runtime.pointer_location().await.and_then(to_result),
        "runtime.active_application" => runtime.active_application().await.and_then(to_result),
        "runtime.shutdown" => {
            app_shutdown.cancel();
            Ok(serde_json::json!({ "status": "shutting_down" }))
        }

        // --- computer.session ---
        "computer.session" => {
            let p: SessionParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            // Identity of the caller. Recorded on `start` so the session has
            // an owner that (only) may stop it on exit. Callers that do not
            // identify themselves are anonymous "jsonrpc" clients.
            let client = cu_core::protocol::ClientInfo {
                client_id: p.client_id.clone().unwrap_or_else(|| "jsonrpc".into()),
                client_name: p
                    .client_name
                    .clone()
                    .unwrap_or_else(|| "JSON-RPC client".into()),
                client_instance_id: p
                    .client_instance_id
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
            };
            runtime
                .session(
                    p.action,
                    p.session_id.as_deref(),
                    p.display_id,
                    client,
                    p.control_token.as_deref(),
                )
                .await
                .and_then(to_result)
        }

        // --- computer.observe ---
        "computer.observe" => {
            let p: ObserveParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            runtime.observe(p, request_id).await.and_then(to_result)
        }

        // --- computer.inspect ---
        "computer.inspect" => {
            let p: InspectParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            runtime.inspect(p).await.and_then(to_result)
        }

        // --- computer.act ---
        "computer.act" => {
            let p: ActParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            // The request is registered under `(connection_id, request_id)` so
            // it can be cancelled precisely — only from this connection, and
            // only with the session's control token.
            runtime.act(p, request_key).await.and_then(to_result)
        }

        // --- computer.cancel ---
        "computer.cancel" => {
            let p: CancelParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            let cancelled = match &p.request_id {
                // Precise cancel: exactly one request on *this* connection.
                Some(rid) => runtime.cancel_request(
                    &RequestKey {
                        connection_id,
                        request_id: rid.clone(),
                    },
                    &p.session_id,
                    p.control_token.as_deref(),
                ),
                // Session-wide cancel (still token-verified).
                None => runtime
                    .cancel_in_flight(&p.session_id, p.control_token.as_deref())
                    .map(|()| true),
            };
            match cancelled {
                Ok(cancelled) => to_result(CancelResult {
                    cancelled,
                    session_id: p.session_id,
                }),
                Err(e) => Err(e),
            }
        }

        // --- trace management ---
        "trace.list" => cu_trace::list_traces(runtime.traces_dir())
            .and_then(|list| to_result(serde_json::json!({ "traces": list }))),
        "trace.get" => {
            let p: TraceGetParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            if let Err(e) = validate_session_id(&p.session_id) {
                return error_response(id, e);
            }
            let path = runtime.traces_dir().join(format!("{}.jsonl", p.session_id));
            cu_trace::read_trace(&path).and_then(to_result)
        }
        "trace.export" => {
            let p: TraceExportParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            if let Err(e) = validate_session_id(&p.session_id) {
                return error_response(id, e);
            }
            let src = runtime.traces_dir().join(format!("{}.jsonl", p.session_id));
            let dest = Path::new(&p.dest);
            cu_trace::export_trace(&src, dest)
                .map_err(|e| CuError::Trace(e.to_string()))
                .and_then(|exported| {
                    to_result(serde_json::json!({
                        "session_id": p.session_id,
                        "path": exported.to_string_lossy(),
                        "format": "jsonl",
                        "exported_at": chrono::Utc::now(),
                    }))
                })
        }
        "trace.replay" => {
            let p: TraceReplayParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            if let Err(e) = validate_session_id(&p.session_id) {
                return error_response(id, e);
            }
            let path = runtime.traces_dir().join(format!("{}.jsonl", p.session_id));
            cu_trace::replay_from_file(&p.session_id, &path).and_then(to_result)
        }
        "trace.summaries" => {
            let list = match cu_trace::list_traces(runtime.traces_dir()) {
                Ok(l) => l,
                Err(e) => return error_response(id, e),
            };
            to_result::<Vec<TraceSummary>>(list)
        }

        other => Err(CuError::MethodNotFound(other.to_string())),
    };

    match result {
        Ok(v) => RpcResponse::ok(id, v),
        Err(e) => error_response(id, e),
    }
}

fn error_response(id: Option<serde_json::Value>, e: CuError) -> RpcResponse {
    let code = e.code().jsonrpc_code();
    let message = e.code().as_str().to_string();
    RpcResponse::err(id, code, message, Some(e.to_error_data()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::{RpcRequest, SessionAction};
    use cu_driver::{
        ApplicationInfo, CaptureRequest, DesktopLayout, DisplayInfo, PermissionStatus, PointerInfo,
    };
    use cu_runtime::{Runtime, RuntimeConfig};
    use std::sync::Arc;

    /// A deterministic in-memory driver so the full dispatch path can be
    /// exercised without a real display. Waits actually sleep (so in-flight
    /// cancellation is observable); every execute is counted.
    #[derive(Default)]
    struct FakeDriver {
        pub executes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl cu_driver::ComputerDriver for FakeDriver {
        async fn list_displays(&self) -> Result<Vec<DisplayInfo>, CuError> {
            Ok(vec![DisplayInfo {
                id: "1".into(),
                name: "fake".into(),
                bounds: cu_core::DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 1280.0,
                    height: 800.0,
                },
                pixel_width: 2560,
                pixel_height: 1600,
                scale_factor: 2.0,
                is_main: true,
            }])
        }
        async fn desktop_layout(&self) -> Result<DesktopLayout, CuError> {
            Ok(DesktopLayout {
                displays: self.list_displays().await?,
                primary_id: "1".into(),
            })
        }
        async fn capture(
            &self,
            request: CaptureRequest,
        ) -> Result<cu_driver::CapturedFrame, CuError> {
            // Nothing ever re-parses the bytes in this test path.
            std::fs::write(&request.output_path, b"fake-png").unwrap();
            Ok(cu_driver::CapturedFrame {
                display_id: request.display_id,
                width: 4,
                height: 4,
                scale_factor: 1.0,
                bounds: cu_core::DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 4.0,
                    height: 4.0,
                },
                image_path: request.output_path,
                image_bytes: b"fake-png".to_vec(),
                format: request.format,
                active_application: None,
                captured_at: chrono::Utc::now(),
            })
        }
        async fn quick_snapshot(
            &self,
            display_id: &str,
        ) -> Result<cu_driver::QuickSnapshot, CuError> {
            Ok(cu_driver::QuickSnapshot {
                thumbnail: vec![0u8; 64],
                thumb_width: 8,
                thumb_height: 8,
                display_id: display_id.to_string(),
                active_application: None,
                captured_at: chrono::Utc::now(),
            })
        }
        async fn execute(
            &self,
            action: &cu_driver::ResolvedAction,
        ) -> Result<cu_driver::ActionResult, CuError> {
            self.executes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let cu_driver::ResolvedAction::Wait { duration_ms } = action {
                tokio::time::sleep(std::time::Duration::from_millis((*duration_ms).min(1000)))
                    .await;
            }
            Ok(cu_driver::ActionResult {
                success: true,
                duration_ms: 1,
                detail: None,
            })
        }
        async fn permission_status(&self) -> Result<PermissionStatus, CuError> {
            Ok(PermissionStatus {
                screen_recording: true,
                accessibility: true,
            })
        }
        async fn active_application(&self) -> Result<Option<ApplicationInfo>, CuError> {
            Ok(None)
        }
        async fn pointer_location(&self) -> Result<PointerInfo, CuError> {
            Ok(PointerInfo {
                location: cu_core::Point::new(0.0, 0.0),
                display_id: Some("1".into()),
            })
        }
        async fn shutdown(&self) -> Result<(), CuError> {
            Ok(())
        }
    }

    fn test_config() -> RuntimeConfig {
        let dir = std::env::temp_dir().join(format!("cu-daemon-tests-{}", std::process::id()));
        RuntimeConfig {
            traces_dir: dir.join("traces"),
            frames_dir: dir.join("frames"),
            ..RuntimeConfig::default()
        }
    }

    /// Dispatch one request as if it arrived on connection `conn`.
    async fn call(
        rt: &Arc<Runtime>,
        conn: u64,
        method: &str,
        id: u64,
        params: serde_json::Value,
    ) -> RpcResponse {
        dispatch(
            rt,
            &CancellationToken::new(),
            conn,
            RpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(id)),
                method: method.into(),
                params: Some(params),
            },
        )
        .await
    }

    fn error_code(resp: &RpcResponse) -> Option<(&str, i64)> {
        resp.error.as_ref().map(|e| {
            (
                e.data
                    .as_ref()
                    .and_then(|d| d.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or(e.message.as_str()),
                e.code,
            )
        })
    }

    /// Start a session on `conn`, returning its session_id and control token.
    async fn start_session(rt: &Arc<Runtime>, conn: u64) -> (String, String) {
        let resp = call(
            rt,
            conn,
            "computer.session",
            1,
            serde_json::json!({
                "action": "start",
                "client_id": "test-client",
                "client_name": "Test client",
                "client_instance_id": "test-instance",
            }),
        )
        .await;
        let result = resp.result.expect("session start must succeed");
        (
            result["session_id"].as_str().unwrap().to_string(),
            result["control_token"]
                .as_str()
                .expect("start response must issue the control token")
                .to_string(),
        )
    }

    /// Observe on `conn` and return the frame id.
    async fn observe_frame(rt: &Arc<Runtime>, conn: u64, session_id: &str) -> String {
        let resp = call(
            rt,
            conn,
            "computer.observe",
            2,
            serde_json::json!({
                "session_id": session_id,
                "include_image": false,
            }),
        )
        .await;
        resp.result
            .expect("observe must succeed")
            .get("frame_id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn session_id_validation() {
        assert!(validate_session_id("s_abc-123").is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id(&"a".repeat(65)).is_err());
        assert!(validate_session_id("../etc/passwd").is_err());
        assert!(validate_session_id("s1;rm -rf").is_err());
    }

    #[test]
    fn method_not_found_shapes_error() {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(1)),
            method: "no.such.method".into(),
            params: None,
        };
        // dispatch is async and needs a runtime; test the error mapping alone.
        let _ = req;
        let e = CuError::MethodNotFound("no.such.method".into());
        let resp = error_response(Some(serde_json::json!(1)), e);
        assert_eq!(resp.error.as_ref().unwrap().code, -32601);
        assert_eq!(resp.error.as_ref().unwrap().message, "METHOD_NOT_FOUND");
    }

    #[test]
    fn invalid_params_is_reported() {
        let e = CuError::InvalidParams("bad".into());
        let resp = error_response(None, e);
        assert_eq!(resp.error.as_ref().unwrap().code, -32602);
    }

    // Type-level smoke test that the dispatch match arms compile against the
    // real protocol types (no runtime construction needed).
    #[allow(dead_code)]
    fn _compile_only(runtime: &Arc<Runtime>) {
        let _ = &runtime;
        let _ = SessionAction::Start;
        let _ = TraceSummary {
            session_id: String::new(),
            path: String::new(),
            entries: 0,
            bytes: 0,
            started_at: chrono::Utc::now(),
            last_entry_at: None,
        };
    }

    // Ensure RuntimeConfig default is constructible from this crate.
    #[test]
    fn runtime_config_default_is_available() {
        let _ = RuntimeConfig::default();
    }

    async fn test_runtime() -> Arc<Runtime> {
        test_runtime_with_driver().await.0
    }

    async fn test_runtime_with_driver() -> (Arc<Runtime>, Arc<FakeDriver>) {
        let driver = Arc::new(FakeDriver::default());
        let rt = Arc::new(Runtime::new(
            driver.clone() as Arc<dyn cu_driver::ComputerDriver>,
            test_config(),
        ));
        (rt, driver)
    }

    #[tokio::test]
    async fn version_reports_protocol_and_rejects_mismatch() {
        let rt = test_runtime().await;
        let resp = call(&rt, 1, "runtime.version", 1, serde_json::Value::Null).await;
        let result = resp.result.expect("version must succeed");
        assert_eq!(result["protocol_version"], serde_json::json!(2));

        // An old client that advertises protocol 1 is told explicitly that it
        // cannot talk to this daemon.
        let resp = call(
            &rt,
            1,
            "runtime.version",
            1,
            serde_json::json!({ "protocol_version": 1 }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("PROTOCOL_VERSION_MISMATCH", -32023)),
            "old clients must get an explicit incompatibility error"
        );
    }

    #[tokio::test]
    async fn start_returns_token_once_and_status_never() {
        let rt = test_runtime().await;
        let (sid, token) = start_session(&rt, 1).await;
        assert_eq!(token.len(), 43, "token must be 256-bit base64url");

        // Read-only status must never leak the token.
        let resp = call(
            &rt,
            1,
            "computer.session",
            3,
            serde_json::json!({ "action": "status", "session_id": sid }),
        )
        .await;
        let result = resp.result.expect("status must succeed");
        assert!(
            result.get("control_token").is_none(),
            "status must never return the control token"
        );
    }

    /// §十六: client A cancelling request_id 1 must never cancel client B's
    /// request_id 1 — the connection id separates them, and the token gates
    /// who may cancel at all.
    #[tokio::test]
    async fn cross_connection_cancel_is_isolated() {
        let (rt, fake) = test_runtime_with_driver().await;
        let (sid, token) = start_session(&rt, 1).await;
        let frame = observe_frame(&rt, 1, &sid).await;

        // Client A (connection 1) starts a long act, request_id 5. The Move
        // proves the batch is executing (driver count), the 10s Wait keeps it
        // in flight until it is cancelled.
        let rt2 = rt.clone();
        let sid2 = sid.clone();
        let frame2 = frame.clone();
        let act_token = token.clone();
        let act_task = tokio::spawn(async move {
            call(
                &rt2,
                1,
                "computer.act",
                5,
                serde_json::json!({
                    "session_id": sid2,
                    "frame_id": frame2,
                    "control_token": act_token,
                    "actions": [
                        { "type": "move", "x": 100.0, "y": 100.0, "coordinate_space": "normalized_1000" },
                        { "type": "wait", "duration_ms": 10_000 }
                    ],
                }),
            )
            .await
        });
        // Wait until the batch is executing (driver execute count > 0).
        let start = std::time::Instant::now();
        while fake.executes.load(std::sync::atomic::Ordering::SeqCst) == 0
            && start.elapsed() < std::time::Duration::from_secs(3)
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the act batch must be executing before the cancel attempts"
        );
        assert!(!act_task.is_finished(), "the act must still be in flight");

        // Client B (connection 2) — same request id, different connection.
        // 1) Without a token: CONTROL_TOKEN_REQUIRED, nothing cancelled.
        let resp = call(
            &rt,
            2,
            "computer.cancel",
            5,
            serde_json::json!({ "session_id": sid, "request_id": 5 }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("CONTROL_TOKEN_REQUIRED", -32019)),
            "cancelling without a token must be refused"
        );
        assert!(
            !act_task.is_finished(),
            "act must survive a tokenless cancel"
        );

        // 2) With the token but on the wrong connection: request_id 5 is not
        // B's — nothing is cancelled (B has no registered request 5).
        let resp = call(
            &rt,
            2,
            "computer.cancel",
            5,
            serde_json::json!({
                "session_id": sid,
                "request_id": 5,
                "control_token": token,
            }),
        )
        .await;
        let result = resp.result.expect("cancel must succeed");
        assert_eq!(
            result["cancelled"], false,
            "client B must not cancel client A's request"
        );
        assert!(!act_task.is_finished(), "A's act must still be running");

        // 3) The owner on the right connection cancels precisely.
        let resp = call(
            &rt,
            1,
            "computer.cancel",
            5,
            serde_json::json!({
                "session_id": sid,
                "request_id": 5,
                "control_token": token,
            }),
        )
        .await;
        let result = resp.result.expect("cancel must succeed");
        assert_eq!(result["cancelled"], true);

        // The act aborts fast: the Move reports success, the interrupted Wait
        // reports `cancelled` — never "success", never an internal error.
        let start = std::time::Instant::now();
        let resp = tokio::time::timeout(std::time::Duration::from_secs(2), act_task)
            .await
            .expect("act must finish quickly after its cancel")
            .unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "cancel must stop the 10s wait fast"
        );
        let result = resp
            .result
            .expect("a cancelled act still reports its batch");
        assert_eq!(
            result["action_results"][0]["status"], "success",
            "the action that completed before the cancel stays success"
        );
        assert_eq!(
            result["action_results"][1]["status"], "cancelled",
            "the interrupted wait must be reported cancelled, got {}",
            result["action_results"][1]
        );
    }

    /// §十七/§三: mutating operations without a token must fail before any
    /// side effect — no session started, nothing paused, nothing executed.
    #[tokio::test]
    async fn mutating_ops_require_the_token_and_leave_no_side_effects() {
        let (rt, fake) = test_runtime_with_driver().await;
        let (sid, token) = start_session(&rt, 1).await;

        // pause without token → CONTROL_TOKEN_REQUIRED, and the session is
        // still Active afterwards (no side effect).
        let resp = call(
            &rt,
            1,
            "computer.session",
            4,
            serde_json::json!({ "action": "pause", "session_id": sid }),
        )
        .await;
        assert_eq!(error_code(&resp), Some(("CONTROL_TOKEN_REQUIRED", -32019)));
        let st = call(
            &rt,
            1,
            "computer.session",
            5,
            serde_json::json!({ "action": "status", "session_id": sid }),
        )
        .await;
        assert_eq!(st.result.unwrap()["state"], serde_json::json!("active"));

        // Wrong token → INVALID_CONTROL_TOKEN, session still Active.
        let resp = call(
            &rt,
            1,
            "computer.session",
            6,
            serde_json::json!({
                "action": "pause",
                "session_id": sid,
                "control_token": "wrong-token",
            }),
        )
        .await;
        assert_eq!(error_code(&resp), Some(("INVALID_CONTROL_TOKEN", -32020)));
        let st = call(
            &rt,
            1,
            "computer.session",
            7,
            serde_json::json!({ "action": "status", "session_id": sid }),
        )
        .await;
        assert_eq!(st.result.unwrap()["state"], serde_json::json!("active"));

        // act without a token → CONTROL_TOKEN_REQUIRED, and nothing reaches
        // the driver.
        let frame = observe_frame(&rt, 1, &sid).await;
        let resp = call(
            &rt,
            1,
            "computer.act",
            8,
            serde_json::json!({
                "session_id": sid,
                "frame_id": frame,
                "actions": [{ "type": "wait", "duration_ms": 1 }],
            }),
        )
        .await;
        assert_eq!(error_code(&resp), Some(("CONTROL_TOKEN_REQUIRED", -32019)));
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no action may reach the driver"
        );

        // Correct token: everything works. (Move, not Wait — waits are
        // executed inside the queue and never reach the driver.)
        let resp = call(
            &rt,
            1,
            "computer.act",
            9,
            serde_json::json!({
                "session_id": sid,
                "frame_id": frame,
                "control_token": token,
                "actions": [{ "type": "move", "x": 100.0, "y": 100.0, "coordinate_space": "normalized_1000" }],
            }),
        )
        .await;
        assert!(resp.result.is_some(), "token-verified act must succeed");
        assert_eq!(fake.executes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// stop without a token fails, and the session survives to be stopped
    /// properly with the token.
    #[tokio::test]
    async fn stop_requires_token_and_is_idempotent_with_it() {
        let rt = test_runtime().await;
        let (sid, token) = start_session(&rt, 1).await;

        let resp = call(
            &rt,
            1,
            "computer.session",
            10,
            serde_json::json!({ "action": "stop", "session_id": sid }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("CONTROL_TOKEN_REQUIRED", -32019)),
            "a session-id-only caller must not be able to stop the session"
        );

        let resp = call(
            &rt,
            1,
            "computer.session",
            11,
            serde_json::json!({
                "action": "stop",
                "session_id": sid,
                "control_token": token,
            }),
        )
        .await;
        let result = resp.result.expect("stop with token must succeed");
        assert_eq!(result["state"], serde_json::json!("stopped"));

        // Stop after stop: still Ok (idempotent), still token-verified.
        let resp = call(
            &rt,
            1,
            "computer.session",
            12,
            serde_json::json!({
                "action": "stop",
                "session_id": sid,
                "control_token": token,
            }),
        )
        .await;
        assert!(
            resp.result.is_some(),
            "double stop with token is idempotent"
        );
        assert_eq!(resp.result.unwrap()["state"], serde_json::json!("stopped"));
    }
}
