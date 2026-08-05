//! End-to-end integration tests for the daemon: spawn the real server
//! in-process on a temp socket (isolated COMPUTER_USE_HOME) and drive it over
//! the wire with a minimal JSON-RPC client.
//!
//! Protocol-level tests run everywhere (no screen access needed). Tests that
//! capture the screen or synthesize input are marked `#[ignore]` and run
//! explicitly (they need Screen Recording + Accessibility TCC grants):
//!
//!   cargo test -p cu-daemon --test integration -- --ignored --test-threads=1

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

/// Minimal newline-delimited JSON-RPC client for the tests.
async fn request(socket: &Path, method: &str, params: Value) -> Value {
    let stream = UnixStream::connect(socket)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to {socket:?}: {e}"));
    let mut stream = BufReader::new(stream);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&body).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_line(&mut response).await.unwrap();
    let v: Value = serde_json::from_str(response.trim()).unwrap();
    if let Some(err) = v.get("error") {
        return err.clone();
    }
    v["result"].clone()
}

struct TestDaemon {
    handle: JoinHandle<anyhow::Result<()>>,
    socket: PathBuf,
    admin_path: PathBuf,
    dir: tempfile::TempDir,
}

async fn spawn_daemon() -> TestDaemon {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("runtime.sock");
    let admin_path = dir.path().join("daemon-admin.json");
    let config = cu_daemon::DaemonConfig {
        socket_path: socket.clone(),
        admin_token_path: admin_path.clone(),
        request_timeout_secs: 60,
        shutdown_grace_secs: 5,
        runtime_config: cu_runtime::RuntimeConfig {
            traces_dir: dir.path().join("traces"),
            frames_dir: dir.path().join("frames"),
            ..cu_runtime::RuntimeConfig::default()
        },
    };
    let handle = tokio::spawn(cu_daemon::run(config));
    // Wait until the socket is listening (the admin token is persisted before
    // the socket binds, so its presence proves the daemon fully started).
    for _ in 0..200 {
        if socket.exists() && admin_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists(), "daemon did not create its socket");
    assert!(
        admin_path.exists(),
        "daemon did not persist its admin token"
    );
    TestDaemon {
        handle,
        socket,
        admin_path,
        dir,
    }
}

/// The admin token the daemon persisted, or a panic if it did not (a running
/// daemon always has one — the CLI's only way to stop it).
fn daemon_admin_token(admin_path: &Path) -> cu_core::security::DaemonAdminToken {
    cu_core::security::load_daemon_admin_token_from(admin_path)
        .expect("daemon must have persisted its admin token")
}

async fn shutdown_daemon(d: TestDaemon) {
    let token = daemon_admin_token(&d.admin_path);
    let _ = request(
        &d.socket,
        "runtime.shutdown",
        json!({ "admin_token": token.as_str() }),
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), d.handle).await;
    d.dir.close().unwrap();
}

// ---------------------------------------------------------------------------
// Protocol-level tests (no screen access)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daemon_serves_health_and_version() {
    let d = spawn_daemon().await;
    let health = request(&d.socket, "runtime.health", json!({})).await;
    assert_eq!(health["version"], json!("0.2.0-alpha.1"));
    assert!(health["ready"].is_boolean(), "ready is a bool");
    let version = request(&d.socket, "runtime.version", json!({})).await;
    assert_eq!(version["name"], json!(cu_core::config::RUNTIME_NAME));
    assert_eq!(
        version["runtime_version"],
        json!(cu_core::config::RUNTIME_VERSION),
        "the wire field is runtime_version per the protocol spec"
    );
    assert_eq!(version["protocol_version"], json!(3));
    assert_eq!(version["minimum_client_protocol_version"], json!(3));
    assert_eq!(version["maximum_client_protocol_version"], json!(3));
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn socket_and_home_are_current_user_only() {
    let d = spawn_daemon().await;
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(&d.socket).unwrap();
    let mode = meta.permissions().mode();
    // Socket itself: owner rwx, nothing for group/others.
    assert_eq!(mode & 0o777, 0o700, "socket mode {mode:o} must be 0700");
    let parent = std::fs::metadata(d.dir.path()).unwrap();
    assert_eq!(parent.permissions().mode() & 0o777, 0o700);
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn stale_socket_is_replaced_on_startup() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("runtime.sock");
    let admin_path = dir.path().join("daemon-admin.json");
    std::fs::write(&socket, b"garbage from a crashed daemon").unwrap();
    let config = cu_daemon::DaemonConfig {
        socket_path: socket.clone(),
        admin_token_path: admin_path.clone(),
        request_timeout_secs: 60,
        shutdown_grace_secs: 5,
        runtime_config: cu_runtime::RuntimeConfig {
            traces_dir: dir.path().join("traces"),
            frames_dir: dir.path().join("frames"),
            ..cu_runtime::RuntimeConfig::default()
        },
    };
    let handle = tokio::spawn(cu_daemon::run(config));
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        // A bind over a regular file fails; success proves the stale file was
        // removed and replaced.
        if let Ok(v) = tokio::time::timeout(
            Duration::from_millis(50),
            request(&socket, "runtime.version", json!({})),
        )
        .await
        {
            assert_eq!(
                v["runtime_version"],
                json!(cu_core::config::RUNTIME_VERSION)
            );
            break;
        }
    }
    let token = daemon_admin_token(&admin_path);
    let _ = request(
        &socket,
        "runtime.shutdown",
        json!({ "admin_token": token.as_str() }),
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
    dir.close().unwrap();
}

#[tokio::test]
async fn daemon_refuses_to_start_when_the_admin_token_cannot_be_persisted() {
    // §三: a daemon that cannot store its admin token must NOT start — running
    // without a stored token would leave it unstoppable (no silent fallback).
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("runtime.sock");
    let config = cu_daemon::DaemonConfig {
        socket_path: socket.clone(),
        // A path under a file (not a directory) can never be created.
        admin_token_path: dir.path().join("blocker").join("daemon-admin.json"),
        request_timeout_secs: 60,
        shutdown_grace_secs: 5,
        runtime_config: cu_runtime::RuntimeConfig {
            traces_dir: dir.path().join("traces"),
            frames_dir: dir.path().join("frames"),
            ..cu_runtime::RuntimeConfig::default()
        },
    };
    std::fs::write(dir.path().join("blocker"), b"in the way").unwrap();
    let err = tokio::time::timeout(Duration::from_secs(10), cu_daemon::run(config))
        .await
        .expect("run must return quickly")
        .expect_err("run must fail without a persistable admin token");
    let text = err.to_string();
    assert!(
        text.contains("admin token"),
        "the failure must mention the admin token, got: {text}"
    );
    assert!(
        !socket.exists(),
        "no socket may be bound when startup refuses"
    );
    dir.close().unwrap();
}

#[tokio::test]
async fn trace_list_is_empty_on_fresh_home() {
    let d = spawn_daemon().await;
    // trace.list is session-scoped since round 6: with no session_id at all
    // it is a malformed request — the daemon never leaks "which sessions
    // ever ran" to anonymous callers.
    let err = request(&d.socket, "trace.list", json!({})).await;
    assert_eq!(
        err["code"],
        json!(-32602),
        "trace.list without a session_id must be refused"
    );
    // And with an unknown session id the typed SESSION_NOT_FOUND — a
    // session id alone grants nothing.
    let err = request(&d.socket, "trace.list", json!({ "session_id": "s_nope" })).await;
    assert_eq!(
        err["code"],
        json!(-32009),
        "trace.list for an unknown session must be SESSION_NOT_FOUND"
    );
    // trace.admin_list without the admin token is refused too — anonymous
    // callers must not list which sessions ever ran.
    let err = request(&d.socket, "trace.admin_list", json!({})).await;
    assert_eq!(
        err["code"],
        json!(-32026),
        "trace.admin_list without the admin token must be refused"
    );
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn unknown_method_is_rejected() {
    let d = spawn_daemon().await;
    let err = request(&d.socket, "computer.explode", json!({})).await;
    assert_eq!(err["code"], json!(-32601), "METHOD_NOT_FOUND jsonrpc code");
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn session_status_without_session_is_session_not_found() {
    // The first-use contract: `status` with no active session is a *typed*
    // SESSION_NOT_FOUND (jsonrpc -32009), never INVALID_PARAMS, so adapters
    // can auto-start on the machine-readable code.
    let d = spawn_daemon().await;
    let err = request(&d.socket, "computer.session", json!({ "action": "status" })).await;
    assert_eq!(err["code"], json!(-32009), "SESSION_NOT_FOUND jsonrpc code");
    assert_eq!(err["data"]["code"], json!("SESSION_NOT_FOUND"));
    assert_eq!(
        err["data"]["message"],
        json!("No active computer-use session exists.")
    );
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn observe_without_session_is_rejected() {
    let d = spawn_daemon().await;
    let err = request(&d.socket, "computer.observe", json!({})).await;
    assert_eq!(err["code"], json!(-32602), "INVALID_PARAMS");
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn invalid_json_is_a_parse_error() {
    let d = spawn_daemon().await;
    let mut stream = BufReader::new(UnixStream::connect(&d.socket).await.unwrap());
    stream.write_all(b"this is not json\n").await.unwrap();
    let mut response = String::new();
    stream.read_line(&mut response).await.unwrap();
    let v: Value = serde_json::from_str(response.trim()).unwrap();
    assert_eq!(v["error"]["code"], json!(-32700), "PARSE_ERROR");
    shutdown_daemon(d).await;
}

#[tokio::test]
async fn shutdown_stops_the_accept_loop_and_removes_socket() {
    let d = spawn_daemon().await;
    let token = daemon_admin_token(&d.admin_path);
    let result = request(
        &d.socket,
        "runtime.shutdown",
        json!({ "admin_token": token.as_str() }),
    )
    .await;
    assert_eq!(result["status"], json!("shutting_down"));
    let completed = tokio::time::timeout(Duration::from_secs(10), d.handle)
        .await
        .expect("daemon should exit after shutdown");
    completed.unwrap().unwrap();
    assert!(
        !d.socket.exists(),
        "socket must be removed on graceful shutdown"
    );
    assert!(
        !d.admin_path.exists(),
        "the admin token file must be removed on graceful shutdown"
    );
    d.dir.close().unwrap();
}

/// §三: the shutdown credential matrix, end to end over the wire — tokenless
/// and wrong tokens (including a session's control token) are refused and the
/// daemon keeps serving; only the persisted admin token shuts it down.
#[tokio::test]
async fn shutdown_auth_matrix() {
    let d = spawn_daemon().await;

    // 1) Tokenless shutdown → DAEMON_ADMIN_TOKEN_REQUIRED; daemon stays up.
    let err = request(&d.socket, "runtime.shutdown", json!({})).await;
    assert_eq!(err["code"], json!(-32026), "tokenless shutdown: {err}");
    assert_eq!(err["data"]["code"], json!("DAEMON_ADMIN_TOKEN_REQUIRED"));
    let health = request(&d.socket, "runtime.health", json!({})).await;
    assert!(
        health["ready"].is_boolean(),
        "daemon still alive after refused shutdown"
    );

    // 2) A garbage token → INVALID_DAEMON_ADMIN_TOKEN; daemon stays up.
    let err = request(
        &d.socket,
        "runtime.shutdown",
        json!({ "admin_token": "definitely-not-it" }),
    )
    .await;
    assert_eq!(err["code"], json!(-32027), "wrong admin token: {err}");
    assert!(request(&d.socket, "runtime.health", json!({})).await["ready"].is_boolean());

    // 3) A session's control token can never shut the daemon down.
    let started = request(&d.socket, "computer.session", json!({ "action": "start" })).await;
    let control = started["control_token"].as_str().unwrap().to_string();
    let err = request(
        &d.socket,
        "runtime.shutdown",
        json!({ "admin_token": control }),
    )
    .await;
    assert_eq!(
        err["code"],
        json!(-32027),
        "a control token must never authorize shutdown: {err}"
    );
    assert!(request(&d.socket, "runtime.health", json!({})).await["ready"].is_boolean());

    // 4) The persisted admin token shuts it down cleanly: socket and token
    // file both removed, process exits.
    let token = daemon_admin_token(&d.admin_path);
    let result = request(
        &d.socket,
        "runtime.shutdown",
        json!({ "admin_token": token.as_str() }),
    )
    .await;
    assert_eq!(result["status"], json!("shutting_down"));
    let completed = tokio::time::timeout(Duration::from_secs(10), d.handle)
        .await
        .expect("daemon should exit after an authorized shutdown");
    completed.unwrap().unwrap();
    assert!(!d.socket.exists(), "socket removed");
    assert!(!d.admin_path.exists(), "admin token file removed");
    d.dir.close().unwrap();
}

// ---------------------------------------------------------------------------
// Cross-client ownership (§ 十九): a second client that knows a session id
// must be able to read it but never control or cancel it. Every request here
// is a fresh connection — a fresh client that knows the session id but not
// the control token. Protocol-level: no screen access needed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_clients_cannot_control_each_others_session() {
    let d = spawn_daemon().await;

    // Client A starts a session; both capability tokens are issued exactly
    // once (start only — never again).
    let started = request(&d.socket, "computer.session", json!({ "action": "start" })).await;
    let session_id = started["session_id"].as_str().unwrap().to_string();
    let token = started["control_token"]
        .as_str()
        .expect("start returns the control token")
        .to_string();
    let observation = started["observation_token"]
        .as_str()
        .expect("start returns the observation token")
        .to_string();
    assert_ne!(token, observation, "the two tokens are independent");

    // Client B (another connection) cannot read status with a session id
    // alone — status is a sensitive read → OBSERVATION_TOKEN_REQUIRED. The
    // public coarse-grained window is session.summary.
    let status_b = request(&d.socket, "computer.session", json!({ "action": "status" })).await;
    assert_eq!(
        status_b["code"],
        json!(-32024),
        "tokenless status must be OBSERVATION_TOKEN_REQUIRED: {status_b}"
    );
    let summary_b = request(&d.socket, "session.summary", json!({})).await;
    assert_eq!(summary_b["session_id"], json!(session_id));
    assert!(
        summary_b.get("control_token").is_none() && summary_b.get("observation_token").is_none(),
        "summary never carries capability tokens"
    );

    // Every mutating op from B without the token is refused — and, the
    // no-side-effects contract, the session survives each attempt untouched.
    for (method, params) in [
        (
            "computer.session",
            json!({ "action": "stop", "session_id": session_id }),
        ),
        (
            "computer.session",
            json!({ "action": "pause", "session_id": session_id }),
        ),
        (
            "computer.cancel",
            json!({ "session_id": session_id, "request_id": 7 }),
        ),
        (
            "computer.act",
            json!({
                "session_id": session_id,
                "frame_id": "f1",
                "actions": [
                    { "type": "move", "x": 100, "y": 100, "coordinate_space": "normalized_1000" }
                ],
            }),
        ),
    ] {
        let err = request(&d.socket, method, params).await;
        assert_eq!(
            err["code"],
            json!(-32019),
            "{method} without the token must be CONTROL_TOKEN_REQUIRED, got {err}"
        );
    }
    let still = request(&d.socket, "session.summary", json!({})).await;
    assert_eq!(
        still["state"],
        json!("active"),
        "B's failed attempts left no side effects"
    );

    // A wrong token is refused, not silently accepted.
    let wrong = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "stop",
            "session_id": session_id,
            "control_token": "not-the-token",
        }),
    )
    .await;
    assert_eq!(
        wrong["code"],
        json!(-32020),
        "wrong token must be INVALID_CONTROL_TOKEN"
    );
    let still2 = request(&d.socket, "session.summary", json!({})).await;
    assert_eq!(still2["state"], json!("active"));

    // B cannot start a second session while A's is active.
    let locked = request(&d.socket, "computer.session", json!({ "action": "start" })).await;
    assert_eq!(
        locked["code"],
        json!(-32004),
        "second start must be CONTROL_LOCKED, got {locked}"
    );

    // Status (with the observation token) carries the owner (non-secret) but
    // never repeats either capability token.
    let status = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "status",
            "session_id": session_id,
            "observation_token": observation,
        }),
    )
    .await;
    assert!(
        status["owner_client_id"].is_string(),
        "status should identify the owner: {status}"
    );
    assert!(
        !status.to_string().contains(&token),
        "status must never repeat the control token"
    );
    assert!(
        !status.to_string().contains(&observation),
        "status must never repeat the observation token"
    );

    // Only A, with the token, can stop.
    let stopped = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "stop",
            "session_id": session_id,
            "control_token": token,
        }),
    )
    .await;
    assert_eq!(
        stopped["state"],
        json!("stopped"),
        "token-carrying stop succeeds: {stopped}"
    );
    let gone = request(&d.socket, "computer.session", json!({ "action": "status" })).await;
    assert_eq!(
        gone["code"],
        json!(-32009),
        "after stop, status is SESSION_NOT_FOUND"
    );

    shutdown_daemon(d).await;
}

#[tokio::test]
async fn old_protocol_clients_are_refused_explicitly() {
    // § 二十一: a pre-ownership SDK (protocol v1) hitting this daemon gets a
    // typed PROTOCOL_VERSION_MISMATCH — never a confusing half-working
    // session. (Clients that don't advertise are served the version and must
    // check it themselves; their tokenless mutating calls fail regardless.)
    let d = spawn_daemon().await;

    let old_version = request(
        &d.socket,
        "runtime.version",
        json!({ "protocol_version": 1 }),
    )
    .await;
    assert_eq!(
        old_version["code"],
        json!(-32023),
        "old protocol_version: {old_version}"
    );
    // v2 (the pre-observation-capability protocol) is also refused — v3 is a
    // hard floor, not a suggestion.
    let v2 = request(
        &d.socket,
        "runtime.version",
        json!({ "protocol_version": 2 }),
    )
    .await;
    assert_eq!(v2["code"], json!(-32023), "v2 must also be refused: {v2}");

    // A v3 client is served the version plus the accepted bounds.
    let ok = request(
        &d.socket,
        "runtime.version",
        json!({ "protocol_version": 3 }),
    )
    .await;
    assert_eq!(
        ok["protocol_version"],
        json!(3),
        "current client is served: {ok}"
    );
    assert_eq!(ok["minimum_client_protocol_version"], json!(3));

    shutdown_daemon(d).await;
}

// ---------------------------------------------------------------------------
// Live tests (screen capture + input synthesis; require TCC grants).
// Run explicitly:  cargo test -p cu-daemon --test integration -- --ignored
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "captures the real screen; needs Screen Recording + Accessibility TCC"]
async fn live_session_observe_act_and_security_matrix() {
    let d = spawn_daemon().await;

    // Start a session — both capability tokens are issued exactly once.
    let s = request(&d.socket, "computer.session", json!({ "action": "start" })).await;
    assert_eq!(s["state"], json!("active"), "session should be active: {s}");
    let session_id = s["session_id"].as_str().unwrap().to_string();
    let control = s["control_token"].as_str().unwrap().to_string();
    let observation = s["observation_token"].as_str().unwrap().to_string();

    // Observe → a real frame.
    let frame = request(
        &d.socket,
        "computer.observe",
        json!({
            "session_id": session_id,
            "observation_token": observation,
            "include_image": true,
        }),
    )
    .await;
    assert!(frame["width"].as_u64().unwrap() > 0);
    assert!(!frame["image_base64"].as_str().unwrap().is_empty());
    let frame_id = frame["frame_id"].as_str().unwrap().to_string();

    // Act (move the pointer) → success.
    let moved = request(
        &d.socket,
        "computer.act",
        json!({
            "session_id": session_id,
            "frame_id": frame_id,
            "control_token": control,
            "actions": [{ "type": "move", "x": 500, "y": 400, "coordinate_space": "normalized_1000" }],
        }),
    )
    .await;
    assert_eq!(moved["action_results"][0]["status"], json!("success"));

    // Type (redacted in the trace).
    let typed = request(
        &d.socket,
        "computer.act",
        json!({
            "session_id": session_id,
            "frame_id": frame_id,
            "control_token": control,
            "actions": [{ "type": "type", "text": "secret-password" }],
        }),
    )
    .await;
    assert_eq!(typed["action_results"][0]["status"], json!("success"));

    // Trace records the type with redaction, not the text.
    let trace = request(
        &d.socket,
        "trace.get",
        json!({
            "session_id": session_id,
            "observation_token": observation,
        }),
    )
    .await;
    let body = serde_json::to_string(&trace).unwrap();
    assert!(!body.contains("secret-password"), "text must be redacted");
    assert!(body.contains("text_redacted"), "redaction marker present");

    // Pause → act rejected.
    let _ = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "pause",
            "session_id": session_id,
            "control_token": control,
        }),
    )
    .await;
    let paused_err = request(
        &d.socket,
        "computer.act",
        json!({
            "session_id": session_id,
            "frame_id": frame_id,
            "control_token": control,
            "actions": [{ "type": "move", "x": 100, "y": 100, "coordinate_space": "normalized_1000" }],
        }),
    )
    .await;
    assert_eq!(paused_err["code"], json!(-32005), "PAUSED");

    // Resume → act accepted again.
    let _ = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "resume",
            "session_id": session_id,
            "control_token": control,
        }),
    )
    .await;
    let resumed = request(
        &d.socket,
        "computer.act",
        json!({
            "session_id": session_id,
            "frame_id": frame_id,
            "control_token": control,
            "actions": [{ "type": "move", "x": 200, "y": 200, "coordinate_space": "normalized_1000" }],
        }),
    )
    .await;
    assert_eq!(resumed["action_results"][0]["status"], json!("success"));

    // User takeover → act rejected.
    let _ = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "takeover",
            "session_id": session_id,
            "control_token": control,
        }),
    )
    .await;
    let takeover_err = request(
        &d.socket,
        "computer.act",
        json!({
            "session_id": session_id,
            "frame_id": frame_id,
            "control_token": control,
            "actions": [{ "type": "move", "x": 300, "y": 300, "coordinate_space": "normalized_1000" }],
        }),
    )
    .await;
    assert_eq!(takeover_err["code"], json!(-32006), "USER_TAKEOVER");

    // Stop → acts rejected even after release.
    let _ = request(
        &d.socket,
        "computer.session",
        json!({
            "action": "stop",
            "session_id": session_id,
            "control_token": control,
        }),
    )
    .await;
    let stopped_err = request(
        &d.socket,
        "computer.act",
        json!({
            "session_id": session_id,
            "frame_id": frame_id,
            "control_token": control,
            "actions": [{ "type": "move", "x": 400, "y": 400, "coordinate_space": "normalized_1000" }],
        }),
    )
    .await;
    assert_eq!(stopped_err["code"], json!(-32010), "INVALID_SESSION_STATE");

    shutdown_daemon(d).await;
}
