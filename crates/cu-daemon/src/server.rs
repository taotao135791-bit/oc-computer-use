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
//! - `runtime.shutdown` (admin token only) is a graceful stop: new requests
//!   are refused with `DAEMON_SHUTTING_DOWN`, in-flight action batches are
//!   cancelled, every connection is drained (it stops reading the moment
//!   shutdown is requested and only finishes responses already started), and
//!   anything still alive after the grace period is aborted. The socket and
//!   the admin token file are removed on the way out, so the daemon can be
//!   restarted cleanly on the same path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cu_core::security::SecretTokenHash;
use cu_core::{ErrorCode, RpcRequest, RpcResponse};
use cu_driver::ComputerDriver;
use cu_driver_macos::MacosDriver;
use cu_runtime::{HumanInputSink, Runtime, RuntimeConfig};
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
    /// How long shutdown waits for connections to drain their in-flight
    /// responses before aborting them (a graceful stop never hangs forever).
    pub shutdown_grace_secs: u64,
    pub runtime_config: RuntimeConfig,
    /// Start the continuous human-input Event Tap at daemon startup. True in
    /// production (Human Always Wins needs it); protocol-level tests set it
    /// false so they never touch a real macOS Event Tap / TCC prompt.
    pub enable_human_input: bool,
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
            shutdown_grace_secs: 10,
            runtime_config,
            enable_human_input: true,
        }
    }
}

/// Run the daemon until `runtime.shutdown` or Ctrl-C. Returns after cleanup.
pub async fn run(config: DaemonConfig) -> anyhow::Result<()> {
    let driver: Arc<dyn ComputerDriver> = Arc::new(MacosDriver::new());
    serve_with(driver, config).await
}

/// The daemon loop over a caller-provided driver (tests inject a fake).
pub(crate) async fn serve_with(
    driver: Arc<dyn ComputerDriver>,
    config: DaemonConfig,
) -> anyhow::Result<()> {
    let runtime = Arc::new(Runtime::new(driver.clone(), config.runtime_config.clone()));
    // P0-1: a real human input must cancel the active batch at event time (not
    // when the action loop next polls). The Event Tap thread invokes this hook
    // synchronously; it cancels the control-holder session's in-flight batches.
    runtime.install_human_takeover_hook(runtime.clone());

    // Round 8 / Phase 11: once the daemon is fully wired, start the continuous
    // human-input monitor (Event Tap). The tap feeds the runtime's
    // HumanInputMonitor, so Human Always Wins works *while* a batch executes.
    // Protocol-level tests run with `enable_human_input = false` so they never
    // touch a real macOS Event Tap.
    if config.enable_human_input {
        let monitor = runtime.human_input.clone();
        let started = driver.start_human_input_monitor(Box::new(move |latency_ms| {
            monitor.on_human_event(latency_ms);
        }));
        if started {
            tracing::info!(event_tap = true, "human-input monitor active");
        } else {
            tracing::warn!(
                event_tap = false,
                state = driver.human_input_monitor_state().unwrap_or_default().as_str(),
                "HUMAN_INPUT_MONITOR_UNAVAILABLE — Event Tap is not running; pointer-delta heuristic will be used as fallback"
            );
        }
    }

    // Prune trace files older than the retention window on startup.
    if let Ok(removed) = cu_trace::prune_old_traces(
        runtime.traces_dir(),
        cu_trace::storage::DEFAULT_RETENTION_DAYS,
    ) {
        if removed > 0 {
            tracing::info!(removed, "pruned old trace files");
        }
    }

    // Generate and persist the daemon admin credential BEFORE the socket
    // binds, so the daemon never accepts shutdown requests it could not
    // authorize (and never runs without a way for the CLI to stop it). A
    // persistence failure refuses to start — a silent fallback would leave
    // the daemon unstoppable. Any credential a previous run left behind is
    // validated first and cleaned if it fails the read-side checks (a
    // tampered store must not be papered over by the rename).
    let admin_token = cu_core::security::generate_daemon_admin_token();
    let daemon_instance_id = cu_core::security::generate_daemon_instance_id();
    let admin_token_path = &config.admin_token_path;
    match cu_core::security::validate_and_cleanup_admin_store(admin_token_path) {
        Ok(true) => tracing::warn!("cleaned an invalid admin credential from a previous run"),
        Ok(false) => {}
        Err(e) => anyhow::bail!(
            "cannot clean previous daemon admin credential at {}: {e} — refusing to start",
            admin_token_path.display()
        ),
    }
    if let Err(e) = cu_core::security::save_daemon_admin_token_to(
        &admin_token,
        &daemon_instance_id,
        admin_token_path,
    ) {
        anyhow::bail!(
            "cannot persist daemon admin token at {}: {e} — refusing to start",
            admin_token_path.display()
        );
    }
    tracing::info!(path = %admin_token_path.display(), instance = %daemon_instance_id, "daemon admin token stored (0600)");
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

    // Every connection task is tracked in a JoinSet so shutdown can drain
    // them: connections are told to stop reading (and finish their in-flight
    // responses), and anything still alive after the grace period is aborted.
    let mut connections = tokio::task::JoinSet::new();
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
                        let instance_id = daemon_instance_id.clone();
                        let connection_id = connection_counter.fetch_add(1, Ordering::Relaxed);
                        connections.spawn(async move {
                            if let Err(e) = handle_connection(stream, connection_id, runtime, shutdown, admin_hash, timeout, &instance_id).await {
                                tracing::debug!(error = %e, "connection handler ended");
                            }
                        });
                    }
                    Err(e) => tracing::error!(error = %e, "accept failed"),
                }
            }
        }
    }

    // Graceful shutdown: refuse new work, cancel in-flight actions, stop
    // sessions, release the driver — then wait for the connections to drain
    // their remaining responses (already in flight when the flag flipped).
    tracing::info!("shutting down daemon");
    let _ = runtime.shutdown().await;

    // Drain connections. Each one stopped reading the moment shutdown was
    // requested and is only finishing responses it already started; the grace
    // period bounds how long a stuck client can delay the exit.
    let grace = std::time::Duration::from_secs(config.shutdown_grace_secs);
    let drained = tokio::time::timeout(grace, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!("grace period expired; aborting remaining connections");
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

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
/// only honors a request presenting the matching token. `daemon_instance_id`
/// identifies this daemon run in `runtime.version`, so the CLI can prove an
/// admin credential belongs to the daemon it is talking to.
async fn handle_connection(
    stream: UnixStream,
    connection_id: u64,
    runtime: Arc<Runtime>,
    app_shutdown: CancellationToken,
    admin_hash: SecretTokenHash,
    timeout_secs: u64,
    daemon_instance_id: &str,
) -> anyhow::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut inflight = Vec::new();

    // On shutdown the daemon stops accepting new requests: the loop below
    // breaks, and the in-flight tasks finish (the runtime cancelled the
    // action batches; anything dispatched after the flag flipped fails fast
    // with DAEMON_SHUTTING_DOWN). The server's grace period is the bound on
    // a client that never reads its responses.
    loop {
        let line = tokio::select! {
            _ = app_shutdown.cancelled() => break,
            line = lines.next_line() => match line {
                Ok(Some(l)) => l,
                Ok(None) => break, // client closed
                Err(e) => return Err(e.into()),
            },
        };
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
        let instance_id = daemon_instance_id.to_string();
        let writer = writer.clone();
        inflight.push(tokio::spawn(async move {
            let fut = dispatch(
                &runtime,
                &shutdown,
                connection_id,
                request.clone(),
                &admin_hash,
                &instance_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{test_config, FakeDriver};
    use serde_json::{json, Value};
    use std::path::Path;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// One running server over a fake driver on a temp socket.
    struct TestServer {
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
        socket: PathBuf,
        admin_path: PathBuf,
        dir: tempfile::TempDir,
    }

    async fn spawn_test_server() -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("runtime.sock");
        let admin_path = dir.path().join("daemon-admin.json");
        let config = DaemonConfig {
            socket_path: socket.clone(),
            admin_token_path: admin_path.clone(),
            request_timeout_secs: 60,
            shutdown_grace_secs: 5,
            runtime_config: test_config(),
            enable_human_input: false,
        };
        let driver: Arc<dyn ComputerDriver> = Arc::new(FakeDriver::default());
        let handle = tokio::spawn(serve_with(driver, config));
        for _ in 0..200 {
            if socket.exists() && admin_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "server did not create its socket");
        assert!(
            admin_path.exists(),
            "server did not persist its admin token"
        );
        TestServer {
            handle,
            socket,
            admin_path,
            dir,
        }
    }

    /// A raw line-based client that keeps its connection open (unlike the
    /// one-shot `request` helper) so drain behavior is observable.
    struct WireClient {
        reader: BufReader<tokio::net::UnixStream>,
    }

    async fn connect(socket: &Path) -> WireClient {
        let stream = UnixStream::connect(socket)
            .await
            .unwrap_or_else(|e| panic!("cannot connect to {socket:?}: {e}"));
        WireClient {
            reader: BufReader::new(stream),
        }
    }

    impl WireClient {
        /// Send one request and read exactly one response line.
        ///
        /// `None` means the daemon closed the connection instead of answering
        /// (shutdown drain behavior): the write failed with broken pipe, the
        /// read hit EOF, or the line was not valid JSON.
        async fn call(
            &mut self,
            id: u64,
            method: &str,
            params: serde_json::Value,
        ) -> Option<Value> {
            let mut line = serde_json::to_string(
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            )
            .unwrap();
            line.push('\n');
            if self.reader.write_all(line.as_bytes()).await.is_err() {
                return None;
            }
            if self.reader.flush().await.is_err() {
                return None;
            }
            let mut resp = String::new();
            match self.reader.read_line(&mut resp).await {
                Ok(0) => None, // clean EOF: the daemon stopped reading
                Ok(_) => serde_json::from_str::<Value>(resp.trim()).ok(),
                Err(_) => None,
            }
        }
    }

    fn admin_token(path: &Path) -> cu_core::security::DaemonAdminToken {
        cu_core::security::load_daemon_admin_token_from(path)
            .expect("server must have persisted its admin token")
    }

    #[tokio::test]
    async fn shutdown_drains_connections_and_cleans_up_for_a_clean_restart() {
        let mut s = spawn_test_server().await;

        // Connection A stays open and watches the daemon go down.
        let mut a = connect(&s.socket).await;
        let ok = a
            .call(1, "runtime.health", json!({}))
            .await
            .expect("health must be answered");
        assert_eq!(ok["result"]["ready"], json!(true));

        // Connection B is established before shutdown too — it must observe
        // the drain: its request dispatched after the flag flips fails with
        // the typed DAEMON_SHUTTING_DOWN (or the connection just closes).
        let mut b = connect(&s.socket).await;

        // Connection A requests the graceful shutdown with the admin token.
        let token = admin_token(&s.admin_path);
        let resp = a
            .call(
                2,
                "runtime.shutdown",
                json!({ "admin_token": token.as_str() }),
            )
            .await
            .expect("shutdown must be answered");
        assert!(resp["result"].is_object(), "shutdown must succeed");

        // The server must exit on its own within the grace period.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), &mut s.handle)
            .await
            .expect("server must finish within the grace period")
            .expect("server run must return Ok");

        // New requests are refused: the daemon is gone (the connection
        // closes instead of answering).
        let refused = b.call(3, "runtime.health", json!({})).await;
        assert!(
            refused.is_none()
                || refused.as_ref().and_then(|v| v.get("error")).is_some()
                || refused.as_ref().and_then(|v| v.get("result")).is_none(),
            "a request after shutdown must not be served, got {refused:?}"
        );

        // The socket and the admin token file are gone — the daemon can
        // restart cleanly on the same paths.
        assert!(!s.socket.exists(), "socket must be removed on shutdown");
        assert!(!s.admin_path.exists(), "admin token file must be removed");

        // Restart on the same paths proves a clean, restartable daemon.
        let dir2 = tempfile::tempdir().unwrap();
        let socket2 = s.socket.clone();
        let admin2 = s.admin_path.clone();
        let config = DaemonConfig {
            socket_path: socket2.clone(),
            admin_token_path: admin2.clone(),
            request_timeout_secs: 60,
            shutdown_grace_secs: 5,
            runtime_config: test_config(),
            enable_human_input: false,
        };
        let driver2: Arc<dyn ComputerDriver> = Arc::new(FakeDriver::default());
        let h2 = tokio::spawn(serve_with(driver2, config));
        for _ in 0..200 {
            if socket2.exists() && admin2.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(socket2.exists(), "restarted server must bind its socket");
        let mut c = connect(&socket2).await;
        let ok = c
            .call(1, "runtime.health", json!({}))
            .await
            .expect("restarted health must be answered");
        assert_eq!(ok["result"]["ready"], json!(true));

        // Stop the restarted server too (fresh admin token).
        let token2 = admin_token(&admin2);
        let resp = c
            .call(
                2,
                "runtime.shutdown",
                json!({ "admin_token": token2.as_str() }),
            )
            .await
            .expect("restarted shutdown must be answered");
        assert!(resp["result"].is_object());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), h2)
            .await
            .expect("restarted server must also exit");
        let _ = dir2.close();
        s.dir.close().unwrap();
    }

    #[tokio::test]
    async fn requests_issued_after_shutdown_are_refused_with_daemon_shutting_down() {
        let mut s = spawn_test_server().await;
        let mut client = connect(&s.socket).await;

        // Fire the shutdown…
        let token = admin_token(&s.admin_path);
        let resp = client
            .call(
                1,
                "runtime.shutdown",
                json!({ "admin_token": token.as_str() }),
            )
            .await
            .expect("shutdown must be answered");
        assert!(resp["result"].is_object(), "shutdown must succeed");

        // …and a request that races the drain. Either the daemon already
        // stopped reading (connection closed) or dispatch refused it with the
        // typed code — never a real result.
        let late = client.call(2, "runtime.health", json!({})).await;
        let code = late
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|e| e.get("data"))
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        assert!(
            late.is_none()
                || code == "DAEMON_SHUTTING_DOWN"
                || late.as_ref().and_then(|v| v.get("result")).is_none(),
            "late request must be refused, got {late:?}"
        );

        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), &mut s.handle)
            .await
            .expect("server must exit")
            .expect("run must return Ok");
        s.dir.close().unwrap();
    }
}
