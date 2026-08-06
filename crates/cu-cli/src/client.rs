//! JSON-RPC 2.0 client over the daemon's Unix socket.
//!
//! The CLI opens a fresh connection per request (the daemon serves each
//! connection independently), so there is no shared state and a hung request
//! never poisons later commands.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use cu_core::config::socket_path;
use cu_core::RpcRequest;

/// The daemon's Unix socket path, overridable for tests.
pub fn default_socket() -> PathBuf {
    socket_path()
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach daemon at {0} (is it running? try `cu daemon start`)")]
    Connect(PathBuf, #[source] std::io::Error),
    #[error("daemon closed the connection before answering")]
    ConnectionClosed,
    #[error("malformed response from daemon: {0}")]
    BadResponse(String),
    /// A client-side operation failed (e.g. writing an exported trace to a
    /// user-chosen path). Never used for daemon errors.
    #[error("{0}")]
    Message(String),
    /// The daemon answered with a JSON-RPC error.
    #[error("error [{code}] {message}")]
    Rpc {
        code: i64,
        message: String,
        /// Machine-readable `data.code` plus any structured detail.
        data: Option<Value>,
    },
}

impl ClientError {
    /// Exit code to use for this failure. RPC errors from the daemon are
    /// already typed (e.g. STALE_FRAME, PERMISSION_DENIED) so 1 is fine;
    /// connection failures are the common case and stay non-zero too.
    pub fn exit_code(&self) -> i32 {
        match self {
            ClientError::Rpc { .. } => 1,
            _ => 2,
        }
    }
}

/// Send one request to the daemon and await its reply.
pub async fn request(method: &str, params: Value) -> Result<Value, ClientError> {
    request_on(&default_socket(), method, params).await
}

/// `request` against an explicit socket path (used by tests).
pub async fn request_on(socket: &Path, method: &str, params: Value) -> Result<Value, ClientError> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| ClientError::Connect(socket.to_path_buf(), e))?;
    let (read_half, mut write_half) = stream.into_split();

    let req = RpcRequest {
        jsonrpc: cu_core::protocol::JSONRPC_VERSION.into(),
        id: Some(Value::from(1)),
        method: method.into(),
        params: Some(params),
    };
    let mut payload =
        serde_json::to_string(&req).map_err(|e| ClientError::BadResponse(e.to_string()))?;
    payload.push('\n');
    write_half
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| ClientError::Connect(socket.to_path_buf(), e))?;
    write_half
        .flush()
        .await
        .map_err(|e| ClientError::Connect(socket.to_path_buf(), e))?;

    let mut reader = tokio::io::BufReader::new(read_half);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| ClientError::Connect(socket.to_path_buf(), e))?;
    if n == 0 {
        return Err(ClientError::ConnectionClosed);
    }

    let resp: cu_core::RpcResponse = serde_json::from_str(&line)
        .map_err(|e| ClientError::BadResponse(format!("{e} in `{line}`")))?;

    match (resp.result, resp.error) {
        (Some(result), None) => Ok(result),
        (None, Some(err)) => Err(ClientError::Rpc {
            code: err.code,
            message: err.message,
            data: err.data,
        }),
        _ => Err(ClientError::BadResponse(
            "response had neither result nor error".into(),
        )),
    }
}

/// Short helper to build JSON from a serializable value.
pub fn to_json<T: Serialize>(v: &T) -> Result<Value, ClientError> {
    serde_json::to_value(v).map_err(|e| ClientError::BadResponse(e.to_string()))
}

/// A JSON-RPC params value that is "no params" — use `Value::Null`.
pub const NO_PARAMS: Value = Value::Null;
