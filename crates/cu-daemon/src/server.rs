//! The daemon server: a newline-delimited JSON-RPC 2.0 endpoint over a Unix
//! domain socket owned by the current user.
//!
//! Security posture:
//! - The socket lives under `~/.computer-use/`, whose parent directory and the
//!   socket file itself are chmod 0700, so only the current user can connect.
//! - A stale socket left by a crashed daemon is removed before binding.
//! - Each request runs under a deadline; a timed-out request cancels the
//!   in-flight action batch (cooperatively) and recycles the Swift bridge so no
//!   stale response can desync later calls.
//! - `runtime.shutdown` cancels the accept loop for a graceful stop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cu_core::security::SecretTokenHash;
use cu_core::{ErrorCode, RpcRequest, RpcResponse};
use cu_driver::ComputerDriver;
use cu_driver_macos::MacosDriver;
use cu_runtime::{Runtime, RuntimeConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::jsonrpc::dispatch;

/// Configuration for the daemon process.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    /// Where the daemon persists its admin token (default
    /// `~/.local/state/oc-computer-use/daemon-admin.json`, 0600). The CLI
    /// reads this file to shut the daemon down; a daemon that cannot persist
    /// its token refuses to start (it would otherwise be unstoppable).
    pub admin_token_path: PathBuf,
    /// Per-request deadline in seconds. Generous by default; session
    /// pause/stop/cancel remain the responsive cancellation path.
    pub request_timeout_secs: u64,
    pub runtime_config: RuntimeConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let mut runtime_config = RuntimeConfig::default();
        // Explicit opt-in to record full typed text in traces. Off by default:
        // `type` actions are redacted to { text_redacted, character_count }
        // unless the operator sets this. See docs/permissions.md.
        if std::env::var("COMPUTER_USE_TRACE_DEV_MODE").as_deref() == Ok("1") {
            runtime_config.trace_dev_mode = true;
        }
        // Trace recording policy: required | best_effort (default) | disabled.
        runtime_config.trace_mode =
            cu_trace::TraceMode::from_env(std::env::var("COMPUTER_USE_TRACE_MODE").ok().as_deref());
        // Stale-frame policy: strict (default) | visual_match.
        runtime_config.stale.policy = cu_runtime::stale_frame::StaleFramePolicy::from_env(
            std::env::var("COMPUTER_USE_STALE_POLICY").ok().as_deref(),
        );
        Self {
            socket_path: cu_core::config::socket_path(),
            admin_token_path: cu_core::config::daemon_admin_path(),
            request_timeout_secs: 600,
            runtime_config,
        }
    }
}

/// Run the daemon until `runtime.shutdown` or Ctrl-C. Returns after cleanup.
pub async fn run(config: DaemonConfig) -> anyhow::Result<()> {
    let driver: Arc<dyn ComputerDriver> = Arc::new(MacosDriver::new());
    let runtime = Arc::new(Runtime::new(driver, config.runtime_config.clone()));

    // Prune trace files older than the retention window on startup.
    if let Ok(removed) = cu_trace::prune_old_traces(
        runtime.traces_dir(),
        cu_trace::storage::DEFAULT_RETENTION_DAYS,
    ) {
        if removed > 0 {
            tracing::info!(removed, "pruned old trace files");
        }
    }

    // Generate and persist the daemon admin token BEFORE the socket binds, so
    // the daemon never accepts shutdown requests it could not authorize (and
    // never runs without a way for the CLI to stop it). A persistence failure
    // refuses to start — a silent fallback would leave the daemon unstoppable.
    let admin_token = cu_core::security::generate_daemon_admin_token();
    let admin_token_path = &config.admin_token_path;
    if let Err(e) = cu_core::security::save_daemon_admin_token_to(&admin_token, admin_token_path) {
        anyhow::bail!(
            "cannot persist daemon admin token at {}: {e} — refusing to start",
            admin_token_path.display()
        );
    }
    tracing::info!(path = %admin_token_path.display(), "daemon admin token stored (0600)");
    let admin_hash = SecretTokenHash::from_token(&admin_token);

    let socket = &config.socket_path;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    if socket.exists() {
        tracing::warn!(path = %socket.display(), "removing stale socket from previous run");
        std::fs::remove_file(socket)?;
    }

    let listener =
        UnixListener::bind(socket).map_err(|e| anyhow::anyhow!("cannot bind {socket:?}: {e}"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o700))?;

    let app_shutdown = CancellationToken::new();
    let ctrl_c_shutdown = app_shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        ctrl_c_shutdown.cancel();
    });

    // Each connection gets a unique id. The id is part of the request key
    // `(connection_id, request_id)` that makes `computer.cancel` precise: two
    // clients may both send `request_id: 1`, and cancelling one never touches
    // the other. Ids start at 1 and wrap is impossible in practice (u64).
    let connection_counter = AtomicU64::new(1);

    tracing::info!(path = %socket.display(), version = %cu_core::config::RUNTIME_VERSION, "computer-use daemon listening");

    loop {
        tokio::select! {
            _ = app_shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let runtime = runtime.clone();
                        let shutdown = app_shutdown.clone();
                        let timeout = config.request_timeout_secs;
                        let admin_hash = admin_hash.clone();
                        let connection_id = connection_counter.fetch_add(1, Ordering::Relaxed);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, connection_id, runtime, shutdown, admin_hash, timeout).await {
                                tracing::debug!(error = %e, "connection handler ended");
                            }
                        });
                    }
                    Err(e) => tracing::error!(error = %e, "accept failed"),
                }
            }
        }
    }

    tracing::info!("shutting down daemon");
    let _ = runtime.shutdown().await;
    let _ = std::fs::remove_file(socket);
    // The admin token dies with the daemon: a stale file would make the next
    // `stop` misread state (and leak a live credential to nothing).
    cu_core::security::remove_daemon_admin_token_from(admin_token_path);
    Ok(())
}

/// Serve one connected client: read a JSON-RPC request per line, dispatch
/// each request concurrently, respond per line, until EOF.
///
/// Dispatch is concurrent (not serial) so a request is *not* blocked behind a
/// long-running predecessor on the same connection — in particular,
/// `computer.cancel` can reach the runtime while a `computer.act` batch is
/// still executing. Responses may arrive out of order; clients match them by
/// JSON-RPC id.
///
/// `connection_id` seeds every request key on this connection, so request
/// cancellation is scoped to the connection that issued it.
///
/// `admin_hash` is the digest of the daemon's admin token; `runtime.shutdown`
/// only honors a request presenting the matching token.
async fn handle_connection(
    stream: UnixStream,
    connection_id: u64,
    runtime: Arc<Runtime>,
    app_shutdown: CancellationToken,
    admin_hash: SecretTokenHash,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut inflight = Vec::new();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let request: RpcRequest = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                // A malformed line is answered immediately, inline.
                let resp = RpcResponse::err(None, -32700, format!("parse error: {e}"), None);
                let mut w = writer.lock().await;
                write_line(&mut *w, &resp).await?;
                continue;
            }
        };

        let runtime = runtime.clone();
        let shutdown = app_shutdown.clone();
        let admin_hash = admin_hash.clone();
        let writer = writer.clone();
        inflight.push(tokio::spawn(async move {
            let fut = dispatch(
                &runtime,
                &shutdown,
                connection_id,
                request.clone(),
                &admin_hash,
            );
            let timeout = tokio::time::Duration::from_secs(timeout_secs);
            let resp = match tokio::time::timeout(timeout, fut).await {
                Ok(resp) => resp,
                Err(_elapsed) => {
                    tracing::warn!(method = %request.method, "request timed out; recycling bridge");
                    // Recycle the Swift bridge in case the aborted request left a
                    // stale response in the pipe.
                    let _ = runtime.restart_bridge().await;
                    // A deadline hit is a timeout, not an explicit cancellation:
                    // ACTION_TIMEOUT tells callers the batch was still running.
                    RpcResponse::err(
                        request.id.clone(),
                        ErrorCode::ActionTimeout.jsonrpc_code(),
                        "request timed out".into(),
                        Some(serde_json::json!({
                            "code": "ACTION_TIMEOUT",
                            "message": "request timed out",
                            "method": request.method,
                        })),
                    )
                }
            };
            let mut w = writer.lock().await;
            let _ = write_line(&mut *w, &resp).await;
        }));
    }

    // Wait for every in-flight response before dropping the write half.
    for task in inflight {
        let _ = task.await;
    }
    Ok(())
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, resp: &RpcResponse) -> anyhow::Result<()> {
    let mut payload = serde_json::to_string(resp)?;
    payload.push('\n');
    w.write_all(payload.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}
