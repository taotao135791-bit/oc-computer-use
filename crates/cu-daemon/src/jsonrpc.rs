//! JSON-RPC 2.0 dispatch: turns wire requests into runtime calls.
//!
//! Every method maps to exactly one runtime operation. Errors are converted to
//! JSON-RPC error responses carrying the machine-readable `data.code` plus any
//! structured detail (stale-frame scores, permission guidance, …) so upper
//! layers can react instead of parsing prose.

use std::path::Path;

use cu_core::{
    ActParams, CuError, InspectParams, ObserveParams, RpcRequest, RpcResponse, SessionParams,
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

#[derive(serde::Deserialize)]
struct SessionOnlyParams {
    session_id: String,
}

/// Dispatch one request. Never panics; every path yields a response.
pub async fn dispatch(
    runtime: &std::sync::Arc<Runtime>,
    app_shutdown: &CancellationToken,
    req: RpcRequest,
) -> RpcResponse {
    let id = req.id.clone();
    let method = req.method.clone();
    let request_id = id.as_ref().map(|v| v.to_string());
    let params = req.params.clone().unwrap_or(serde_json::Value::Null);

    let result = match method.as_str() {
        // --- runtime introspection ---
        "runtime.health" => runtime.health().await,
        "runtime.version" => Ok(serde_json::json!({
            "name": cu_core::config::RUNTIME_NAME,
            "version": cu_core::config::RUNTIME_VERSION,
        })),
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
                .session(p.action, p.session_id.as_deref(), p.display_id, client)
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
            runtime.act(p, request_id).await.and_then(to_result)
        }

        // --- computer.cancel ---
        "computer.cancel" => {
            let p: SessionOnlyParams = match parse_params(&params) {
                Ok(p) => p,
                Err(e) => return error_response(id, e),
            };
            match runtime.cancel_in_flight(&p.session_id) {
                Ok(()) => Ok(serde_json::json!({ "cancelled": true, "session_id": p.session_id })),
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
    use cu_runtime::{Runtime, RuntimeConfig};
    use std::sync::Arc;

    // A stub runtime using a fake driver is heavy to construct in this crate;
    // instead, verify the pure helpers.

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
}
