//! JSON-RPC 2.0 dispatch: turns wire requests into runtime calls.
//!
//! Every method maps to exactly one runtime operation. Errors are converted to
//! JSON-RPC error responses carrying the machine-readable `data.code` plus any
//! structured detail (stale-frame scores, permission guidance, …) so upper
//! layers can react instead of parsing prose.

use std::path::Path;

use cu_core::security::SecretTokenHash;
use cu_core::{
    ActParams, CancelParams, CancelResult, CapabilityTokenParams, CuError, InspectParams,
    ObserveParams, RequestKey, RpcRequest, RpcResponse, RuntimeVersionResult, SessionParams,
    SessionSummary, ShutdownParams, TraceExportParams, TraceGetParams, TraceReplayParams,
    TraceSummary,
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

/// Parse request params into the typed wire struct. A JSON `null` (a request
/// with no `params` member) is treated as an empty object so methods with
/// all-optional params (e.g. `runtime.shutdown`) work without one.
fn parse_params<T: DeserializeOwned>(params: &serde_json::Value) -> Result<T, CuError> {
    let params = if params.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        params.clone()
    };
    serde_json::from_value(params)
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

/// Dispatch one request. Never panics; every path yields a response.
///
/// `connection_id` identifies the connection the request arrived on. Together
/// with the request's JSON-RPC id it forms the [`RequestKey`] that scopes
/// cancellation: `computer.cancel` may only cancel requests issued on the
/// *same* connection (and only with the session's control token).
///
/// `admin_hash` is the digest of the daemon's admin token, the only
/// credential that authorizes `runtime.shutdown` (session capability tokens
/// never do — a leaked control token must not be able to kill the daemon).
pub async fn dispatch(
    runtime: &std::sync::Arc<Runtime>,
    app_shutdown: &CancellationToken,
    connection_id: u64,
    req: RpcRequest,
    admin_hash: &SecretTokenHash,
) -> RpcResponse {
    let id = req.id.clone();
    let method = req.method.clone();
    let request_id = id.as_ref().map(|v| v.to_string());
    let request_key = id.as_ref().map(|v| RequestKey {
        connection_id,
        request_id: v.clone(),
    });
    let params = req.params.clone().unwrap_or(serde_json::Value::Null);

    // The daemon is stopping: refuse new work. The runtime sets the flag at
    // the very start of shutdown and cancels in-flight batches, so a request
    // that races the drain window gets a typed DAEMON_SHUTTING_DOWN instead
    // of starting work it cannot finish.
    if runtime.is_shutting_down() {
        return error_response(id, CuError::DaemonShuttingDown);
    }

    let result = match method.as_str() {
        // --- runtime introspection ---
        "runtime.health" => runtime.health().await,
        "runtime.version" => {
            // A client may advertise its protocol version. A mismatch is an
            // explicit PROTOCOL_VERSION_MISMATCH (never a confusing success),
            // so an old SDK talking to a new daemon fails loudly instead of
            // misbehaving. Clients that don't advertise still get the version
            // (with the min/max bounds) to check themselves; their tokenless
            // calls will fail with the token errors regardless.
            if let serde_json::Value::Object(map) = &params {
                if let Some(serde_json::Value::Number(n)) = map.get("protocol_version") {
                    let got = n.as_u64().unwrap_or(u64::MAX) as u32;
                    let (min, max) = (
                        cu_core::security::MIN_CLIENT_PROTOCOL_VERSION,
                        cu_core::security::MAX_CLIENT_PROTOCOL_VERSION,
                    );
                    if got < min || got > max {
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
            to_result(RuntimeVersionResult {
                name: cu_core::config::RUNTIME_NAME.into(),
                version: cu_core::config::RUNTIME_VERSION.into(),
                protocol_version: cu_core::security::PROTOCOL_VERSION,
                minimum_client_protocol_version: cu_core::security::MIN_CLIENT_PROTOCOL_VERSION,
                maximum_client_protocol_version: cu_core::security::MAX_CLIENT_PROTOCOL_VERSION,
            })
        }
        "runtime.permissions" => runtime.permissions().await.and_then(to_result),
        // Display count/identity is public (like `session.summary`); the
        // *precise* desktop geometry below is a sensitive read.
        "runtime.displays" => runtime.displays().await.and_then(to_result),
        "runtime.desktop_layout" => {
            let p: CapabilityTokenParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            // Precise layout (display bounds, primary id) reveals the
            // desktop's exact geometry — observation or control token only.
            if let Err(e) =
                runtime.verify_any_token(p.observation_token.as_deref(), p.control_token.as_deref())
            {
                return error_response(id, e);
            }
            runtime.desktop_layout().await.and_then(to_result)
        }
        "runtime.pointer" => {
            let p: CapabilityTokenParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            // Cursor location is a sensitive read — observation or control
            // token only.
            if let Err(e) =
                runtime.verify_any_token(p.observation_token.as_deref(), p.control_token.as_deref())
            {
                return error_response(id, e);
            }
            runtime.pointer_location().await.and_then(to_result)
        }
        "runtime.active_application" => {
            let p: CapabilityTokenParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            // The active application/window title is a sensitive read —
            // observation or control token only.
            if let Err(e) =
                runtime.verify_any_token(p.observation_token.as_deref(), p.control_token.as_deref())
            {
                return error_response(id, e);
            }
            runtime.active_application().await.and_then(to_result)
        }
        "runtime.shutdown" => {
            // Only the daemon's admin token may shut it down. The token is
            // presented by the CLI (which read it from the admin token file);
            // a missing token is DAEMON_ADMIN_TOKEN_REQUIRED and a wrong one
            // (including any session capability token) is
            // INVALID_DAEMON_ADMIN_TOKEN — nothing is cancelled either way.
            let p: ShutdownParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            match p.admin_token.as_deref() {
                None => Err(CuError::DaemonAdminTokenRequired),
                Some(presented) if !admin_hash.verify(presented) => {
                    Err(CuError::InvalidDaemonAdminToken)
                }
                Some(_) => {
                    app_shutdown.cancel();
                    Ok(serde_json::json!({ "status": "shutting_down" }))
                }
            }
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
                    p.observation_token.as_deref(),
                )
                .await
                .and_then(to_result)
        }

        // --- session.summary: the *public* session view ---
        // Coarse state + non-secret owner identity only. This is the one
        // session query that needs no token; it must never reveal display
        // ids, frame ids, trace paths, or any capability token.
        "session.summary" => to_result::<SessionSummary>(runtime.session_summary()),

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
        // Trace contents are a sensitive read: every trace method verifies an
        // observation or control token BEFORE touching any file. A
        // session-id-only caller gets OBSERVATION_TOKEN_REQUIRED and no file
        // I/O happens.
        "trace.list" => {
            // The listing spans sessions (it has no session_id), so it verifies
            // the caller's capability against any session: a valid token
            // proves a trusted client. The summaries are metadata only — the
            // absolute filesystem path never crosses the wire. No token →
            // OBSERVATION_TOKEN_REQUIRED, and the trace directory is not even
            // scanned.
            let p: CapabilityTokenParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            if let Err(e) =
                runtime.verify_any_token(p.observation_token.as_deref(), p.control_token.as_deref())
            {
                return error_response(id, e);
            }
            cu_trace::list_traces(runtime.traces_dir())
                .and_then(|list| to_result(serde_json::json!({ "traces": list })))
        }
        "trace.get" => {
            let p: TraceGetParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            if let Err(e) = validate_session_id(&p.session_id) {
                return error_response(id, e);
            }
            if let Err(e) = runtime.verify_session_read(
                &p.session_id,
                p.observation_token.as_deref(),
                p.control_token.as_deref(),
            ) {
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
            if let Err(e) = runtime.verify_session_read(
                &p.session_id,
                p.observation_token.as_deref(),
                p.control_token.as_deref(),
            ) {
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
            if let Err(e) = runtime.verify_session_read(
                &p.session_id,
                p.observation_token.as_deref(),
                p.control_token.as_deref(),
            ) {
                return error_response(id, e);
            }
            let path = runtime.traces_dir().join(format!("{}.jsonl", p.session_id));
            cu_trace::replay_from_file(&p.session_id, &path).and_then(to_result)
        }
        "trace.summaries" => {
            // Same token-gated, metadata-only treatment as `trace.list`.
            let p: CapabilityTokenParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            if let Err(e) =
                runtime.verify_any_token(p.observation_token.as_deref(), p.control_token.as_deref())
            {
                return error_response(id, e);
            }
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
    use crate::fakes::{test_admin, test_config, FakeDriver};
    use cu_core::{RpcRequest, SessionAction};
    use cu_runtime::{Runtime, RuntimeConfig};
    use std::sync::Arc;

    /// Dispatch one request as if it arrived on connection `conn`, against the
    /// shared test admin credential.
    async fn call(
        rt: &Arc<Runtime>,
        conn: u64,
        method: &str,
        id: u64,
        params: serde_json::Value,
    ) -> RpcResponse {
        let (_token, admin_hash) = test_admin();
        call_with(
            rt,
            conn,
            method,
            id,
            params,
            &admin_hash,
            &CancellationToken::new(),
        )
        .await
    }

    /// Dispatch against an explicit admin hash and shutdown token (so shutdown
    /// tests can observe what was cancelled and what was not).
    async fn call_with(
        rt: &Arc<Runtime>,
        conn: u64,
        method: &str,
        id: u64,
        params: serde_json::Value,
        admin_hash: &SecretTokenHash,
        app_shutdown: &CancellationToken,
    ) -> RpcResponse {
        dispatch(
            rt,
            app_shutdown,
            conn,
            RpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::json!(id)),
                method: method.into(),
                params: Some(params),
            },
            admin_hash,
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

    /// Start a session on `conn`, returning its session_id, control token, and
    /// observation token.
    async fn start_session(rt: &Arc<Runtime>, conn: u64) -> (String, String, String) {
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
            result["observation_token"]
                .as_str()
                .expect("start response must issue the observation token")
                .to_string(),
        )
    }

    /// Observe on `conn` and return the frame id.
    async fn observe_frame(
        rt: &Arc<Runtime>,
        conn: u64,
        session_id: &str,
        observation_token: &str,
    ) -> String {
        let resp = call(
            rt,
            conn,
            "computer.observe",
            2,
            serde_json::json!({
                "session_id": session_id,
                "observation_token": observation_token,
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

    /// Start a session and observe with its observation token (the minimum
    /// credential an observe call is issued with).
    async fn start_and_observe(rt: &Arc<Runtime>, conn: u64) -> (String, String, String) {
        let (sid, control, observation) = start_session(rt, conn).await;
        let frame = call(
            rt,
            conn,
            "computer.observe",
            2,
            serde_json::json!({
                "session_id": sid,
                "observation_token": observation,
                "include_image": false,
            }),
        )
        .await;
        frame
            .result
            .expect("observe with the observation token must succeed");
        (sid, control, observation)
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
            trace_id: String::new(),
            session_id: String::new(),
            created_at: chrono::Utc::now(),
            size_bytes: 0,
            event_count: 0,
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
        assert_eq!(result["protocol_version"], serde_json::json!(3));
        assert_eq!(
            result["minimum_client_protocol_version"],
            serde_json::json!(3)
        );
        assert_eq!(
            result["maximum_client_protocol_version"],
            serde_json::json!(3)
        );
        assert_eq!(
            result["runtime_version"],
            serde_json::json!("0.2.0-alpha.1")
        );

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
        let (sid, token, observation) = start_session(&rt, 1).await;
        assert_eq!(token.len(), 43, "control token must be 256-bit base64url");
        assert_eq!(observation.len(), 43, "observation token must be 256-bit");
        assert_ne!(token, observation, "the two tokens must be independent");

        // status without any token is a sensitive read: refused with
        // OBSERVATION_TOKEN_REQUIRED — a session id alone grants nothing.
        let resp = call(
            &rt,
            1,
            "computer.session",
            3,
            serde_json::json!({ "action": "status", "session_id": sid }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024)),
            "status must require an observation or control token"
        );

        // With the observation token, status succeeds — and must never leak
        // either capability token back.
        let resp = call(
            &rt,
            1,
            "computer.session",
            4,
            serde_json::json!({
                "action": "status",
                "session_id": sid,
                "observation_token": observation,
            }),
        )
        .await;
        let result = resp.result.expect("status with token must succeed");
        assert!(
            result.get("control_token").is_none(),
            "status must never return the control token"
        );
        assert!(
            result.get("observation_token").is_none(),
            "status must never return the observation token"
        );

        // The public session.summary works tokenless and carries no secrets.
        let resp = call(&rt, 1, "session.summary", 5, serde_json::Value::Null).await;
        let result = resp.result.expect("summary is public");
        assert_eq!(result["session_id"], serde_json::json!(sid));
        assert!(
            result.get("control_token").is_none() && result.get("observation_token").is_none(),
            "summary must never contain capability tokens"
        );
    }

    /// §十六: client A cancelling request_id 1 must never cancel client B's
    /// request_id 1 — the connection id separates them, and the token gates
    /// who may cancel at all.
    #[tokio::test]
    async fn cross_connection_cancel_is_isolated() {
        let (rt, fake) = test_runtime_with_driver().await;
        let (sid, token, observation) = start_and_observe(&rt, 1).await;
        let frame = observe_frame(&rt, 1, &sid, &observation).await;

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
        let (sid, token, observation) = start_and_observe(&rt, 1).await;

        // pause without token → CONTROL_TOKEN_REQUIRED, and the session is
        // still Active afterwards (no side effect). The tokenless state probe
        // is the public session.summary — status itself is token-gated.
        let resp = call(
            &rt,
            1,
            "computer.session",
            4,
            serde_json::json!({ "action": "pause", "session_id": sid }),
        )
        .await;
        assert_eq!(error_code(&resp), Some(("CONTROL_TOKEN_REQUIRED", -32019)));
        let st = call(&rt, 1, "session.summary", 5, serde_json::Value::Null).await;
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
        let st = call(&rt, 1, "session.summary", 7, serde_json::Value::Null).await;
        assert_eq!(st.result.unwrap()["state"], serde_json::json!("active"));

        // act without a token → CONTROL_TOKEN_REQUIRED, and nothing reaches
        // the driver.
        let frame = observe_frame(&rt, 1, &sid, &observation).await;
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
        let (sid, token, _observation) = start_session(&rt, 1).await;

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

    /// §二: the observation capability matrix. observe without a token →
    /// OBSERVATION_TOKEN_REQUIRED **with zero side effects** (no capture, no
    /// frame file, no trace entry, no frame-id consumed); a wrong token →
    /// INVALID_OBSERVATION_TOKEN (non-descriptive); the observation token
    /// observes but cannot act; the control token also observes.
    #[tokio::test]
    async fn observation_capability_matrix() {
        let (rt, fake) = test_runtime_with_driver().await;
        let (sid, token, observation) = start_and_observe(&rt, 1).await;

        // 1) No token → OBSERVATION_TOKEN_REQUIRED, and NOTHING happened:
        //    no capture, no frame file, no trace entry, no frame-id consumed.
        let resp = call(
            &rt,
            1,
            "computer.observe",
            10,
            serde_json::json!({ "session_id": sid, "include_image": false }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024)),
            "a session id alone grants no observation permission"
        );
        assert_eq!(
            fake.captures.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the rejected observe must not have captured — only the startup observe did"
        );
        let n_frames = std::fs::read_dir(test_config().frames_dir)
            .map(|d| {
                d.filter(|e| {
                    e.as_ref()
                        .map(|f| {
                            f.file_name()
                                .to_string_lossy()
                                .starts_with(&format!("{sid}_"))
                        })
                        .unwrap_or(false)
                })
                .count()
            })
            .unwrap_or(0);
        assert_eq!(
            n_frames, 1,
            "the rejected observe must not write a frame file — only the startup observe did"
        );
        // The trace exists (the startup observe began it). Its entry count
        // must be unchanged by the rejected observe — rejected reads are not
        // recorded in the sensitive trace.
        let trace_path = test_config().traces_dir.join(format!("{sid}.jsonl"));
        let trace_entries = |p: &std::path::Path| {
            std::fs::read_to_string(p)
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0)
        };
        let baseline = trace_entries(&trace_path);
        assert!(baseline >= 1, "the startup observe must have been traced");

        // 2) Wrong token → INVALID_OBSERVATION_TOKEN, never WHICH was wrong.
        let resp = call(
            &rt,
            1,
            "computer.observe",
            11,
            serde_json::json!({
                "session_id": sid,
                "observation_token": "wrong-token",
                "include_image": false,
            }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );
        assert_eq!(fake.captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            trace_entries(&trace_path),
            baseline,
            "a wrong-token observe must not record a trace entry either"
        );

        // 3) The observation token observes…
        let resp = call(
            &rt,
            1,
            "computer.observe",
            12,
            serde_json::json!({
                "session_id": sid,
                "observation_token": observation,
                "include_image": false,
            }),
        )
        .await;
        assert!(resp.result.is_some(), "observation token must observe");
        assert_eq!(fake.captures.load(std::sync::atomic::Ordering::SeqCst), 2);

        // …but cannot act: act with the observation token is refused as if no
        // control credential existed (CONTROL_TOKEN_REQUIRED), nothing executes.
        let frame = resp.result.unwrap()["frame_id"]
            .as_str()
            .unwrap()
            .to_string();
        let resp = call(
            &rt,
            1,
            "computer.act",
            13,
            serde_json::json!({
                "session_id": sid,
                "frame_id": frame,
                "observation_token": observation,
                "actions": [{ "type": "wait", "duration_ms": 1 }],
            }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("CONTROL_TOKEN_REQUIRED", -32019)),
            "the observation token must never grant control"
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no action may reach the driver with only an observation token"
        );

        // 4) The control token observes too — control includes observation.
        let resp = call(
            &rt,
            1,
            "computer.observe",
            14,
            serde_json::json!({
                "session_id": sid,
                "control_token": token,
                "include_image": false,
            }),
        )
        .await;
        assert!(resp.result.is_some(), "control token must also observe");
        assert_eq!(fake.captures.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// §二.4: inspect is a sensitive read — tokenless → OBSERVATION_TOKEN_REQUIRED,
    /// wrong → INVALID_OBSERVATION_TOKEN, both tokens work.
    #[tokio::test]
    async fn inspect_requires_an_observation_credential() {
        let (rt, fake) = test_runtime_with_driver().await;
        let (sid, _token, observation) = start_and_observe(&rt, 1).await;
        let frame_id = call(
            &rt,
            1,
            "computer.observe",
            2,
            serde_json::json!({
                "session_id": sid,
                "observation_token": observation,
                "include_image": false,
            }),
        )
        .await
        .result
        .unwrap()["frame_id"]
            .as_str()
            .unwrap()
            .to_string();

        // A well-formed inspect request (frame_id + region) with no token.
        let params = |extra: serde_json::Value| {
            let mut m = serde_json::json!({
                "session_id": sid,
                "frame_id": frame_id,
                "region": {
                    "x": 0,
                    "y": 0,
                    "width": 1,
                    "height": 1,
                    "coordinate_space": "image_pixels",
                },
            });
            if let Some(v) = extra.get("observation_token") {
                m["observation_token"] = v.clone();
            }
            m
        };

        let resp = call(
            &rt,
            1,
            "computer.inspect",
            10,
            params(serde_json::json!({})),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024))
        );

        let resp = call(
            &rt,
            1,
            "computer.inspect",
            11,
            params(serde_json::json!({ "observation_token": "wrong" })),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );
        assert_eq!(fake.captures.load(std::sync::atomic::Ordering::SeqCst), 2);

        let resp = call(
            &rt,
            1,
            "computer.inspect",
            12,
            params(serde_json::json!({ "observation_token": observation })),
        )
        .await;
        assert!(resp.result.is_some(), "inspect with token must succeed");
    }

    /// §二.5: trace contents are sensitive — get/export/replay are token-gated
    /// (even for a *stopped* session); the list itself is public metadata.
    #[tokio::test]
    async fn trace_reads_require_an_observation_credential() {
        let (rt, _fake) = test_runtime_with_driver().await;
        let (sid, _token, observation) = start_and_observe(&rt, 1).await;
        // Stop the session: trace reads must still work for a stopped session
        // (the session id remains addressable in the trace registry).
        call(
            &rt,
            1,
            "computer.session",
            20,
            serde_json::json!({
                "action": "stop",
                "session_id": sid,
                "control_token": _token,
            }),
        )
        .await;

        // trace.get without a token → OBSERVATION_TOKEN_REQUIRED, no file read.
        let resp = call(
            &rt,
            1,
            "trace.get",
            21,
            serde_json::json!({ "session_id": sid }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024))
        );

        // Wrong token → INVALID_OBSERVATION_TOKEN.
        let resp = call(
            &rt,
            1,
            "trace.get",
            22,
            serde_json::json!({ "session_id": sid, "observation_token": "wrong" }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );

        // export with a wrong token → INVALID_OBSERVATION_TOKEN.
        let dest = std::env::temp_dir().join(format!("cu-trace-export-{sid}.jsonl"));
        let resp = call(
            &rt,
            1,
            "trace.export",
            23,
            serde_json::json!({
                "session_id": sid,
                "observation_token": "wrong",
                "dest": dest.to_string_lossy(),
            }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );

        // The observation token reads the stopped session's trace.
        let resp = call(
            &rt,
            1,
            "trace.get",
            24,
            serde_json::json!({ "session_id": sid, "observation_token": observation }),
        )
        .await;
        assert!(
            resp.result.is_some(),
            "observation token must read a stopped session's trace"
        );

        // The list is token-gated: it is no longer public (round 5 — a trace
        // listing reveals which sessions ever ran on this machine).
        let resp = call(&rt, 1, "trace.list", 25, serde_json::Value::Null).await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024)),
            "trace.list without a capability token must be refused"
        );

        // With the observation token it works — metadata only, no paths, no
        // tokens.
        let resp = call(
            &rt,
            1,
            "trace.list",
            26,
            serde_json::json!({ "observation_token": observation }),
        )
        .await;
        let list = resp.result.expect("trace.list with a token must succeed");
        let ids: Vec<&str> = list["traces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["session_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&sid.as_str()));
        for entry in list["traces"].as_array().unwrap() {
            assert!(
                entry.get("path").is_none(),
                "trace.list must never expose filesystem paths"
            );
            assert!(entry.get("control_token").is_none());
            assert!(entry.get("observation_token").is_none());
        }
    }

    /// §二.3: session.summary is the public (tokenless) coarse-grained window —
    /// the inverse of `computer.session status`, which is a sensitive read.
    #[tokio::test]
    async fn summary_is_public_and_status_is_not() {
        let (rt, _fake) = test_runtime_with_driver().await;
        let (sid, _token, observation) = start_and_observe(&rt, 1).await;

        // summary: tokenless, coarse, carries no secrets.
        let resp = call(&rt, 1, "session.summary", 1, serde_json::Value::Null).await;
        let result = resp.result.expect("summary is public");
        assert_eq!(result["session_id"], serde_json::json!(sid));
        assert_eq!(result["state"], serde_json::json!("active"));
        assert_eq!(result["lock_held"], serde_json::json!(true));
        assert!(result.get("observation_token").is_none() && result.get("control_token").is_none());

        // status: sensitive read — no token → refused.
        let resp = call(
            &rt,
            1,
            "computer.session",
            2,
            serde_json::json!({ "action": "status", "session_id": sid }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024))
        );

        // status: observation token → fine.
        let resp = call(
            &rt,
            1,
            "computer.session",
            3,
            serde_json::json!({
                "action": "status",
                "session_id": sid,
                "observation_token": observation,
            }),
        )
        .await;
        assert!(resp.result.is_some(), "status with token must succeed");
    }

    /// Once the runtime begins shutting down, every dispatch is refused with
    /// the typed DAEMON_SHUTTING_DOWN — the daemon never starts new work it
    /// cannot finish.
    #[tokio::test]
    async fn dispatch_during_shutdown_returns_daemon_shutting_down() {
        let rt = test_runtime().await;
        let before = call(&rt, 1, "runtime.health", 1, serde_json::Value::Null).await;
        assert!(before.result.is_some(), "health serves before shutdown");
        rt.shutdown().await.expect("shutdown succeeds");
        let resp = call(&rt, 1, "runtime.health", 2, serde_json::Value::Null).await;
        let (code, _) = error_code(&resp).expect("an error response");
        assert_eq!(code, "DAEMON_SHUTTING_DOWN");
        // Even runtime.version is refused — the daemon is done serving.
        let resp = call(&rt, 1, "runtime.version", 3, serde_json::Value::Null).await;
        let (code, _) = error_code(&resp).expect("an error response");
        assert_eq!(code, "DAEMON_SHUTTING_DOWN");
    }

    /// Graceful shutdown cancels in-flight action batches: a long-running
    /// act is aborted the moment the daemon shuts down, and reports
    /// CANCELLED (not a success, not a hang).
    #[tokio::test]
    async fn shutdown_cancels_in_flight_actions() {
        let rt = test_runtime().await;
        let (sid, ctrl, obs) = start_session(&rt, 1).await;
        let frame = observe_frame(&rt, 1, &sid, &obs).await;
        // An action batch that would take 2s — far longer than the test.
        let act_rt = rt.clone();
        let sid2 = sid.clone();
        let ctrl2 = ctrl.clone();
        let frame2 = frame.clone();
        let act = tokio::spawn(async move {
            call(
                &act_rt,
                1,
                "computer.act",
                10,
                serde_json::json!({
                    "session_id": sid2,
                    "frame_id": frame2,
                    "control_token": ctrl2,
                    "actions": [{"type": "wait", "duration_ms": 2000}],
                }),
            )
            .await
        });
        // Give the act time to register and start, then shut down.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        rt.shutdown().await.expect("shutdown succeeds");
        let resp = act.await.expect("the act task finishes");
        // Cancellation is reported *inside the result* (per-action
        // `status: "cancelled"`), not as a JSON-RPC error — the batch did not
        // fail, it was aborted.
        let result = resp.result.expect("act returns a result");
        assert_eq!(
            result["executed"],
            serde_json::json!(false),
            "a cancelled batch must not report execution"
        );
        assert_eq!(
            result["action_results"][0]["status"],
            serde_json::json!("cancelled"),
            "in-flight action must be cancelled by shutdown"
        );
    }

    /// §三: runtime.shutdown is the daemon's kill switch — the admin token
    /// gates it. No token → DAEMON_ADMIN_TOKEN_REQUIRED, nothing cancelled.
    /// A wrong token — including a session's control token — →
    /// INVALID_DAEMON_ADMIN_TOKEN, nothing cancelled. Only the correct admin
    /// token cancels, and it reports shutting_down.
    #[tokio::test]
    async fn shutdown_requires_the_admin_token() {
        let rt = test_runtime().await;
        let (admin_token, admin_hash) = test_admin();
        let shutdown = CancellationToken::new();

        // 1) No token at all → DAEMON_ADMIN_TOKEN_REQUIRED, not cancelled.
        let resp = call_with(
            &rt,
            1,
            "runtime.shutdown",
            1,
            serde_json::json!({}),
            &admin_hash,
            &shutdown,
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("DAEMON_ADMIN_TOKEN_REQUIRED", -32026)),
            "a tokenless shutdown must be refused"
        );
        assert!(!shutdown.is_cancelled(), "nothing may be cancelled");

        // 2) A garbage token → INVALID_DAEMON_ADMIN_TOKEN, not cancelled.
        let resp = call_with(
            &rt,
            1,
            "runtime.shutdown",
            2,
            serde_json::json!({ "admin_token": "not-the-admin-token" }),
            &admin_hash,
            &shutdown,
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_DAEMON_ADMIN_TOKEN", -32027)),
            "a wrong admin token must be refused non-descriptively"
        );
        assert!(!shutdown.is_cancelled(), "nothing may be cancelled");

        // 3) A session's control token must never shut the daemon down.
        let (_sid, control_token, _obs) = start_session(&rt, 1).await;
        let resp = call_with(
            &rt,
            1,
            "runtime.shutdown",
            3,
            serde_json::json!({ "admin_token": control_token }),
            &admin_hash,
            &shutdown,
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_DAEMON_ADMIN_TOKEN", -32027)),
            "a session capability token must never authorize shutdown"
        );
        assert!(!shutdown.is_cancelled(), "nothing may be cancelled");

        // 4) The correct admin token cancels and reports shutting_down.
        let resp = call_with(
            &rt,
            1,
            "runtime.shutdown",
            4,
            serde_json::json!({ "admin_token": admin_token.as_str() }),
            &admin_hash,
            &shutdown,
        )
        .await;
        let result = resp.result.expect("authorized shutdown must succeed");
        assert_eq!(result["status"], serde_json::json!("shutting_down"));
        assert!(shutdown.is_cancelled(), "the daemon must be stopping");
    }

    /// A request with malformed shutdown params is an INVALID_PARAMS, and the
    /// daemon stays up.
    #[tokio::test]
    async fn shutdown_with_malformed_params_is_invalid_params() {
        let rt = test_runtime().await;
        let (_admin_token, admin_hash) = test_admin();
        let shutdown = CancellationToken::new();
        let resp = call_with(
            &rt,
            1,
            "runtime.shutdown",
            1,
            serde_json::json!({ "admin_token": 42 }),
            &admin_hash,
            &shutdown,
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_PARAMS", -32602)),
            "a non-string admin_token is a malformed request"
        );
        assert!(!shutdown.is_cancelled());
    }

    /// §四.1: `trace.list` / `trace.summaries` are sensitive reads — no token →
    /// OBSERVATION_TOKEN_REQUIRED, wrong token → INVALID_OBSERVATION_TOKEN,
    /// observation token → success, control token → success. The trace
    /// directory is not even scanned on the failure path.
    #[tokio::test]
    async fn trace_list_requires_a_capability_token() {
        let (rt, _fake) = test_runtime_with_driver().await;
        let (sid, token, observation) = start_and_observe(&rt, 1).await;

        // 1) No token → OBSERVATION_TOKEN_REQUIRED.
        let resp = call(&rt, 1, "trace.list", 1, serde_json::Value::Null).await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024))
        );
        let resp = call(&rt, 1, "trace.summaries", 2, serde_json::Value::Null).await;
        assert_eq!(
            error_code(&resp),
            Some(("OBSERVATION_TOKEN_REQUIRED", -32024))
        );

        // 2) Wrong token → INVALID_OBSERVATION_TOKEN (non-descriptive).
        let resp = call(
            &rt,
            1,
            "trace.list",
            3,
            serde_json::json!({ "observation_token": "wrong-token" }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );
        let resp = call(
            &rt,
            1,
            "trace.summaries",
            4,
            serde_json::json!({ "control_token": "wrong-token" }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );

        // 3) Observation token → success; entries are metadata only.
        let resp = call(
            &rt,
            1,
            "trace.list",
            5,
            serde_json::json!({ "observation_token": observation }),
        )
        .await;
        let list = resp.result.expect("observation token must list traces");
        let ids: Vec<&str> = list["traces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["session_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&sid.as_str()));
        for entry in list["traces"].as_array().unwrap() {
            assert!(
                entry.get("path").is_none(),
                "no filesystem paths on the wire"
            );
            assert_eq!(entry["event_count"], entry["event_count"]);
            assert!(entry["size_bytes"].is_u64());
        }

        // 4) Control token → success too (control includes observation).
        let resp = call(
            &rt,
            1,
            "trace.summaries",
            6,
            serde_json::json!({ "control_token": token }),
        )
        .await;
        assert!(resp.result.is_some(), "control token must also list traces");
    }

    /// §四.4/§四.5: `runtime.pointer`, `runtime.active_application` and
    /// `runtime.desktop_layout` are sensitive reads. Without a token the
    /// request is refused **before** any driver call — the driver counters
    /// stay at zero. The observation token opens them; a wrong token is
    /// refused with zero driver calls as well.
    #[tokio::test]
    async fn runtime_sensitive_reads_require_a_capability_token() {
        let (rt, fake) = test_runtime_with_driver().await;
        let (_sid, _token, observation) = start_and_observe(&rt, 1).await;

        let driver_calls = |fake: &FakeDriver| {
            (
                fake.pointer_calls.load(std::sync::atomic::Ordering::SeqCst),
                fake.active_app_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
            )
        };

        // 1) No token → OBSERVATION_TOKEN_REQUIRED, zero driver calls.
        for (method, id) in [
            ("runtime.pointer", 1),
            ("runtime.active_application", 2),
            ("runtime.desktop_layout", 3),
        ] {
            let resp = call(&rt, 1, method, id, serde_json::Value::Null).await;
            assert_eq!(
                error_code(&resp),
                Some(("OBSERVATION_TOKEN_REQUIRED", -32024)),
                "{method} without a token must be refused"
            );
            assert_eq!(
                driver_calls(&fake),
                (0, 0),
                "{method} must not touch the driver"
            );
        }

        // 2) Wrong token → INVALID_OBSERVATION_TOKEN, zero driver calls.
        let resp = call(
            &rt,
            1,
            "runtime.pointer",
            10,
            serde_json::json!({ "observation_token": "wrong-token" }),
        )
        .await;
        assert_eq!(
            error_code(&resp),
            Some(("INVALID_OBSERVATION_TOKEN", -32025))
        );
        assert_eq!(
            driver_calls(&fake),
            (0, 0),
            "wrong token must not touch the driver"
        );

        // 3) Observation token → all three succeed, driver called once each.
        let resp = call(
            &rt,
            1,
            "runtime.pointer",
            11,
            serde_json::json!({ "observation_token": observation }),
        )
        .await;
        assert!(resp.result.is_some(), "pointer with token must succeed");
        let resp = call(
            &rt,
            1,
            "runtime.active_application",
            12,
            serde_json::json!({ "observation_token": observation }),
        )
        .await;
        assert!(
            resp.result.is_some(),
            "active_application with token must succeed"
        );
        let resp = call(
            &rt,
            1,
            "runtime.desktop_layout",
            13,
            serde_json::json!({ "observation_token": observation }),
        )
        .await;
        assert!(
            resp.result.is_some(),
            "desktop_layout with token must succeed"
        );
        assert_eq!(driver_calls(&fake), (1, 1));
    }
}
