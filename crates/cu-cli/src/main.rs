//! `cu` — the computer-use runtime command line.
//!
//! Talks JSON-RPC 2.0 to the daemon over `~/.computer-use/runtime.sock`
//! (`cu daemon run` serves in-process; `cu daemon start` launches it detached).
//! Every subcommand exits non-zero on failure and supports `--json` for
//! machine-readable output.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use cu_cli::client::{request, ClientError};
use cu_cli::credentials;
use cu_core::{
    ComputerAction, CoordinateSpace, MouseButton, Point, Region, TextInputMethod, WaitPolicy,
};

#[derive(Parser)]
#[command(
    name = "cu",
    version = cu_core::config::RUNTIME_VERSION,
    about = "computer-use runtime control (daemon, sessions, observe/act, traces)",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the background daemon.
    Daemon(DaemonArgs),
    /// Check the runtime end-to-end (socket, permissions, displays).
    Doctor,
    /// Show screen-recording / accessibility permission status + guidance.
    Permissions,
    /// List attached displays.
    Displays,
    /// Print the global pointer position.
    Pointer,
    /// Print the frontmost application.
    ActiveApp,
    /// Session lifecycle: start / status / pause / resume / takeover / release / stop.
    Session(SessionArgs),
    /// Capture the screen. Returns a frame_id you can pass to inspect/act.
    Observe(ObserveArgs),
    /// Crop a region out of a stored frame as an image.
    Inspect(InspectArgs),
    /// Click at coordinates.
    Click(PointerActionArgs),
    /// Double-click at coordinates.
    DoubleClick(PointerActionArgs),
    /// Move the pointer (no button press).
    Move(PointerActionArgs),
    /// Type text (redacted in traces unless dev mode is on).
    Type(TypeArgs),
    /// Press keys, e.g. `cu key cmd l` or `cu key cmd,l`.
    Key(KeyArgs),
    /// Scroll the wheel.
    Scroll(ScrollArgs),
    /// Drag from one point to another.
    Drag(DragArgs),
    /// Wait (no-op action) for a duration.
    Wait(WaitArgs),
    /// Trace inspection: list / get / export / replay.
    Trace(TraceArgs),
}

// ---------------------------------------------------------------------------
// Subcommand argument structs
// ---------------------------------------------------------------------------

#[derive(Args)]
struct DaemonArgs {
    #[command(subcommand)]
    action: DaemonAction,
    /// Print the daemon admin token for debugging (with a warning). Never
    /// passed by default — the token can shut the daemon down.
    #[arg(long, global = true)]
    show_secret: bool,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Launch the daemon detached and wait until it is healthy.
    Start,
    /// Run the daemon in the foreground (used by `daemon start`).
    Run,
    /// Ask the daemon to shut down gracefully (reads the admin token file).
    Stop,
    /// Restart the daemon (stop if running, then start).
    Restart,
    /// Print whether the daemon is running and its version.
    Status,
}

#[derive(Args)]
struct SessionArgs {
    /// start | status | summary | pause | resume | takeover | release | stop
    action: String,
    /// Target session; defaults to the active one for status.
    #[arg(long)]
    session_id: Option<String>,
    /// Display to bind a new session to.
    #[arg(long)]
    display_id: Option<String>,
    /// App bundle id for a session target (e.g. com.google.Chrome).
    #[arg(long)]
    bundle_id: Option<String>,
    /// App pid for a session target.
    #[arg(long)]
    pid: Option<i32>,
    /// Window id (CGWindowNumber) for a session target.
    #[arg(long)]
    window_id: Option<u32>,
    /// Pointer isolation policy.
    #[arg(long, value_parser = ["isolated_only", "isolated_preferred", "physical_allowed"])]
    pointer_policy: Option<String>,
    /// Keyboard focus policy.
    #[arg(long, value_parser = ["strict", "activate_target"])]
    focus_policy: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ObserveArgs {
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    display_id: Option<String>,
    /// Include the base64 screenshot in the response.
    #[arg(long)]
    include_image: bool,
    /// Decode the returned screenshot to this file path.
    #[arg(long)]
    image_out: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct InspectArgs {
    #[arg(long)]
    session_id: Option<String>,
    /// Stored frame to crop (defaults to the session's current frame).
    #[arg(long)]
    frame_id: Option<String>,
    /// Region as x,y,width,height.
    #[arg(long, default_value = "0,0,0,0")]
    region: String,
    /// Output scale factor.
    #[arg(long)]
    scale: Option<u32>,
    /// Write the cropped image to this file.
    #[arg(long)]
    image_out: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PointerActionArgs {
    /// X coordinate.
    x: f64,
    /// Y coordinate.
    y: f64,
    #[arg(long, value_enum, default_value = "left")]
    button: ButtonArg,
    #[arg(long, default_value = "normalized_1000")]
    coordinate_space: CoordinateArg,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    frame_id: Option<String>,
    #[arg(long, value_enum, default_value = "none")]
    wait_policy: WaitPolicyArg,
    #[arg(long)]
    fixed_wait_ms: Option<u64>,
    #[arg(long)]
    screenshot: bool,
    #[arg(long)]
    json: bool,
}

#[derive(clap::ValueEnum, Clone)]
enum ButtonArg {
    Left,
    Right,
    Middle,
}

#[derive(clap::ValueEnum, Clone)]
enum CoordinateArg {
    #[value(name = "normalized_1000")]
    Normalized1000,
    #[value(name = "image_pixels")]
    ImagePixels,
}

impl CoordinateArg {
    fn space(&self) -> CoordinateSpace {
        match self {
            CoordinateArg::Normalized1000 => CoordinateSpace::Normalized1000,
            CoordinateArg::ImagePixels => CoordinateSpace::ImagePixels,
        }
    }
}

#[derive(clap::ValueEnum, Clone)]
enum WaitPolicyArg {
    None,
    Fixed,
    UntilStable,
}

impl WaitPolicyArg {
    fn policy(&self) -> WaitPolicy {
        match self {
            WaitPolicyArg::None => WaitPolicy::None,
            WaitPolicyArg::Fixed => WaitPolicy::Fixed,
            WaitPolicyArg::UntilStable => WaitPolicy::UntilStable,
        }
    }
}

#[derive(Args)]
struct TypeArgs {
    text: String,
    #[arg(long, value_enum, default_value = "keyboard")]
    method: MethodArg,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    frame_id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(clap::ValueEnum, Clone)]
enum MethodArg {
    Keyboard,
    Clipboard,
}

impl MethodArg {
    fn method(&self) -> TextInputMethod {
        match self {
            MethodArg::Keyboard => TextInputMethod::Keyboard,
            MethodArg::Clipboard => TextInputMethod::Clipboard,
        }
    }
}

#[derive(Args)]
struct KeyArgs {
    /// One or more key names, e.g. `cmd l` or `cmd,l`.
    keys: Vec<String>,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    frame_id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ScrollArgs {
    #[arg(long)]
    delta_x: f64,
    #[arg(long)]
    delta_y: f64,
    #[arg(long)]
    x: Option<f64>,
    #[arg(long)]
    y: Option<f64>,
    #[arg(long, default_value = "normalized_1000")]
    coordinate_space: CoordinateArg,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    frame_id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DragArgs {
    from_x: f64,
    from_y: f64,
    to_x: f64,
    to_y: f64,
    #[arg(long, default_value = "normalized_1000")]
    coordinate_space: CoordinateArg,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    frame_id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct WaitArgs {
    ms: u64,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    frame_id: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TraceArgs {
    #[command(subcommand)]
    action: TraceAction,
}

#[derive(Subcommand)]
enum TraceAction {
    /// List trace files across all sessions (daemon manager only, admin token).
    List,
    /// Print a trace's JSONL entries.
    Get { session_id: String },
    /// Export a trace's content (round 7: a pure read — the daemon never
    /// writes a path). Without --output the content goes to stdout; with
    /// --output this CLI writes the file and refuses to overwrite an
    /// existing one unless --force is given.
    Export {
        session_id: String,
        /// Write the exported content to this file (stdout when absent).
        #[arg(long)]
        output: Option<PathBuf>,
        /// Overwrite --output even though it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Re-run the actions recorded in a trace on the live desktop.
    Replay { session_id: String },
    /// Derive per-session metrics and a failure category from a trace
    /// (timeline, aggregates, classification — the same numbers the
    /// benchmark report computes). Reads via trace.export (a pure read).
    Analyze {
        session_id: String,
        /// Print the full analysis as JSON.
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Signal-driven cleanup: a one-shot command that auto-started its session
    // stops it before exit, so it never leaves a control-locked session
    // behind (observed in the wild: a `cu observe` left the daemon's control
    // lock held, and every later `session start` failed with CONTROL_LOCKED —
    // the MCP server already had this behavior; the CLI matched it).
    // `cu daemon run` is excluded: it owns no session, and it must keep its
    // own SIGTERM handling (a competing watcher would race its shutdown).
    let cli = Cli::parse();
    let is_daemon_cmd = matches!(&cli.command, Command::Daemon(_));
    if !is_daemon_cmd {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("cannot install SIGTERM handler");
        tokio::spawn(async move {
            let sigterm = tokio::select! {
                _ = tokio::signal::ctrl_c() => false,
                _ = terminate.recv() => true,
            };
            stop_auto_started().await;
            // Conventional "terminated by signal" statuses.
            std::process::exit(if sigterm { 143 } else { 130 });
        });
    }
    match run(cli).await {
        Ok(()) => {}
        Err(e) => {
            match &e {
                ClientError::Rpc {
                    data,
                    message,
                    code,
                } => {
                    // Render the machine-readable data.code if present, with
                    // the human-readable data.message when the daemon
                    // supplied one (the JSON-RPC message itself is the code
                    // name, e.g. SESSION_NOT_FOUND).
                    if let Some(Value::String(code_str)) = data.as_ref().and_then(|d| d.get("code"))
                    {
                        let human = data
                            .as_ref()
                            .and_then(|d| d.get("message"))
                            .and_then(Value::as_str)
                            .filter(|m| *m != code_str);
                        match human {
                            Some(m) => eprintln!("cu: {code_str} — {m}"),
                            None => eprintln!("cu: {code_str} — {message}"),
                        }
                    } else if data.is_some() {
                        eprintln!("cu: [{code}] {message}: {data:?}");
                    } else {
                        eprintln!("cu: [{code}] {message}");
                    }
                }
                other => eprintln!("cu: {other}"),
            }
            stop_auto_started().await;
            std::process::exit(e.exit_code());
        }
    }
    stop_auto_started().await;
}

async fn run(cli: Cli) -> Result<(), ClientError> {
    match cli.command {
        Command::Daemon(args) => run_daemon(args).await,
        Command::Doctor => run_doctor().await,
        Command::Permissions => print_json(request("runtime.permissions", Value::Null).await?),
        Command::Displays => print_json(request("runtime.displays", Value::Null).await?),
        // Cursor location and the active application are sensitive reads
        // (observation or control token required): present any capability
        // credential this CLI holds.
        Command::Pointer => {
            let params = token_params().await;
            print_json(request("runtime.pointer", params).await?)
        }
        Command::ActiveApp => {
            let params = token_params().await;
            print_json(request("runtime.active_application", params).await?)
        }
        Command::Session(args) => run_session(args).await,
        Command::Observe(args) => run_observe(args).await,
        Command::Inspect(args) => run_inspect(args).await,
        Command::Click(args) => run_pointer(args, false).await,
        Command::DoubleClick(args) => run_pointer(args, true).await,
        Command::Move(args) => run_move(args).await,
        Command::Type(args) => run_type(args).await,
        Command::Key(args) => run_key(args).await,
        Command::Scroll(args) => run_scroll(args).await,
        Command::Drag(args) => run_drag(args).await,
        Command::Wait(args) => run_wait(args).await,
        Command::Trace(args) => run_trace(args).await,
    }
}

// ---------------------------------------------------------------------------
// output helpers
// ---------------------------------------------------------------------------

/// Print `value` pretty-printed (or raw compact when `--json`).
fn emit(value: &Value, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(value).unwrap());
    } else {
        println!("{value}");
    }
}

/// Deep-redact every capability token field so a secret can never reach
/// stdout, even in `--json` output. The daemon only returns the session
/// tokens in `start` responses (exactly once); those must never be printed
/// verbatim. `admin_token` rides only *requests*, never responses, but
/// redacting it too is cheap defense if a response ever echoes params.
/// Delegates to the shared [`cu_core::redact_json`] — one redaction rule for
/// the runtime, the logs, and the CLI.
fn redact_token(value: &Value) -> Value {
    cu_core::redact_json(value)
}

/// `--json` flag forms: many commands carry it, but simple list-like commands
/// always print raw JSON (the `print_json` path).
fn print_json(value: Value) -> Result<(), ClientError> {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
    Ok(())
}

/// Build the token fields for the **cross-session** sensitive reads
/// (`runtime.pointer`, `runtime.active_application`, `runtime.desktop_layout`):
/// any capability credential this CLI holds. No credentials → the params stay
/// empty and the daemon refuses with OBSERVATION_TOKEN_REQUIRED.
///
/// The credential of the session this CLI resolves (active, or auto-started
/// like `cu observe`) is used: tokens live only while the daemon session is
/// in memory, so after a daemon restart an arbitrary stored credential (e.g.
/// from a stopped session) is rejected with INVALID_OBSERVATION_TOKEN.
///
/// Note: the trace reads are **not** cross-session since round 6 — they take
/// an explicit `session_id` plus that session's token (`trace.get` reads the
/// session credential; the cross-session `trace.admin_list` uses the daemon
/// admin token).
async fn token_params() -> Value {
    let token = match resolve_session(&None).await {
        Ok(sid) => {
            if let Some(cred) = credentials::load(&sid) {
                if !cred.observation_token.is_empty() {
                    Some(cred.observation_token)
                } else if !cred.control_token.is_empty() {
                    Some(cred.control_token)
                } else {
                    None
                }
            } else {
                None
            }
        }
        Err(_) => credentials::all()
            .into_iter()
            .map(|cred| {
                if !cred.observation_token.is_empty() {
                    cred.observation_token
                } else {
                    cred.control_token
                }
            })
            .next(),
    };
    match token {
        Some(t) => json!({ "observation_token": t }),
        None => json!({}),
    }
}

fn human_action_result(value: &Value) {
    // ActResult shape: { executed, action_results, screen_changed, stable, next_frame_id }
    if let Some(results) = value.get("action_results").and_then(|v| v.as_array()) {
        for r in results {
            let idx = r.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let status = r.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let dur = r.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  [{idx}] {status} ({dur}ms)");
            if let Some(err) = r.get("error") {
                println!("        error: {err}");
            }
        }
    }
    if let Some(changed) = value.get("screen_changed") {
        println!("  screen_changed: {changed}");
    }
    if let Some(stable) = value.get("stable") {
        println!("  stable: {stable}");
    }
    if let Some(next) = value.get("next_frame_id").and_then(|v| v.as_str()) {
        println!("  next_frame_id: {next}");
    }
}

// ---------------------------------------------------------------------------
// shared plumbing
// ---------------------------------------------------------------------------

/// Whether a daemon RPC error carries the given `data.code`.
fn rpc_code_is(data: &Option<Value>, code: &str) -> bool {
    matches!(
        data.as_ref().and_then(|d| d.get("code")).and_then(Value::as_str),
        Some(c) if c == code
    )
}

/// The session this process auto-started via `resolve_session` (implicit
/// first use of `cu observe` / `cu pointer` / …), if any. Stopped on exit so
/// a one-shot CLI never leaves a control-locked session behind. Sessions
/// started explicitly via `cu session start` are **not** recorded here —
/// that verb is the persistent-session mechanism and outlives the process.
static AUTO_STARTED_SESSION: Mutex<Option<String>> = Mutex::new(None);

fn remember_auto_started(session_id: &str) {
    *AUTO_STARTED_SESSION.lock().unwrap() = Some(session_id.to_string());
}

/// Best-effort stop of the session this process auto-started (with its
/// control token from the credential store — the daemon refuses a stop
/// without it). Never fatal and never prints anything: the process is about
/// to exit, and a stuck daemon must not turn a clean `cu observe` into an
/// error.
async fn stop_auto_started() {
    let sid = AUTO_STARTED_SESSION.lock().unwrap().take();
    let Some(sid) = sid else { return };
    let Some(cred) = credentials::load(&sid) else {
        return;
    };
    let _ = request(
        "computer.session",
        json!({
            "action": "stop",
            "session_id": sid,
            "control_token": cred.control_token,
        }),
    )
    .await;
    credentials::delete(&sid);
}

/// Resolve a session: the one named by `session_id`, or the currently active
/// one when the caller left it unspecified. First use auto-creates: when the
/// daemon has no active session, one is started with this CLI's identity, so
/// `cu observe` / `cu click` work straight after `cu daemon start`. The
/// capability tokens issued by that start are saved to the credential store.
///
/// Discovery is the public `session.summary` probe (v3): `status` is a
/// sensitive read and this CLI may not hold the active session's credential,
/// while `session.summary` answers "is there a session" tokenlessly.
async fn resolve_session(session_id: &Option<String>) -> Result<String, ClientError> {
    if let Some(id) = session_id {
        return Ok(id.clone());
    }
    let resp = request("session.summary", Value::Null).await?;
    let active = resp
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let started = match active {
        Some(id) => return Ok(id.to_string()),
        None => request("computer.session", session_start_params(Value::Null)).await?,
    };
    save_started_credential(&started);
    let sid = started
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ClientError::Rpc {
            code: -32602,
            message: "daemon did not return a session_id".into(),
            data: None,
        })?;
    remember_auto_started(&sid);
    Ok(sid)
}

/// Persist the tokens from a `start` response (issued exactly once) to the
/// 0600 credential store. Failures are non-fatal — the daemon simply refuses
/// later sensitive commands until the session is restarted.
fn save_started_credential(started: &Value) {
    if let (Some(sid), Some(token)) = (
        started.get("session_id").and_then(Value::as_str),
        started.get("control_token").and_then(Value::as_str),
    ) {
        let observation = started
            .get("observation_token")
            .and_then(Value::as_str)
            .unwrap_or("");
        let created_at = started
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Err(e) = credentials::save(
            sid,
            token,
            observation,
            &format!("cu-{}", std::process::id()),
            created_at,
        ) {
            eprintln!("cu: warning: cannot store session credential: {e}");
        }
    }
}

/// Drop credential files whose sessions no longer exist (stopped, or the
/// daemon restarted — the tokens died with the session). Probes with the
/// public `session.summary` (tokenless): `status` is a sensitive read in v3.
async fn prune_stale_credentials() {
    for cred in credentials::all() {
        match request("session.summary", Value::Null).await {
            Ok(v) => {
                let gone = match v.get("session_id").and_then(Value::as_str) {
                    Some(id) if id == cred.session_id => matches!(
                        v.get("state").and_then(Value::as_str),
                        Some("stopped" | "stopping" | "failed")
                    ),
                    Some(_) => false,
                    None => true,
                };
                if gone {
                    credentials::delete(&cred.session_id);
                }
            }
            Err(ClientError::Rpc { data, .. }) if rpc_code_is(&data, "SESSION_NOT_FOUND") => {
                credentials::delete(&cred.session_id);
            }
            _ => {}
        }
    }
}

/// Params for `computer.session start`: identity is always attached so the
/// session's owner is this CLI instance.
fn session_start_params(display_id: Value) -> Value {
    let mut params = json!({"action": "start"});
    if let Some(m) = params.as_object_mut() {
        m.insert("client_id".into(), json!("cu-cli"));
        m.insert("client_name".into(), json!("cu"));
        m.insert(
            "client_instance_id".into(),
            json!(format!("cu-{}", std::process::id())),
        );
    }
    if !display_id.is_null() {
        params["display_id"] = display_id;
    }
    params
}

/// Frame id for an action. When the caller did not pin a specific stored
/// frame, capture a fresh one right before acting — a one-shot CLI action
/// should always reference what the screen looks like *now* (the runtime's
/// stale-frame check would reject an old frame against changed pixels).
async fn resolve_frame(session_id: &str, frame_id: &Option<String>) -> Result<String, ClientError> {
    if let Some(f) = frame_id {
        return Ok(f.clone());
    }
    // Observe is a sensitive read: it needs the session's observation
    // credential, which this CLI holds only for sessions it started.
    let mut params = json!({"session_id": session_id, "include_image": false});
    if let Some(t) = credentials::read_token(session_id) {
        params["observation_token"] = json!(t);
    }
    let obs = request("computer.observe", params).await?;
    obs.get("frame_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ClientError::Rpc {
            code: -32000,
            message: "observe did not return a frame_id".into(),
            data: None,
        })
}

fn click_action(x: f64, y: f64, button: &ButtonArg, space: CoordinateSpace) -> ComputerAction {
    let button = match button {
        ButtonArg::Left => MouseButton::Left,
        ButtonArg::Right => MouseButton::Right,
        ButtonArg::Middle => MouseButton::Middle,
    };
    ComputerAction::Click {
        x,
        y,
        button,
        coordinate_space: space,
    }
}

fn wait_policy_from_args(wait: WaitPolicyArg, fixed: Option<u64>) -> (WaitPolicy, Option<u64>) {
    (wait.policy(), fixed)
}

// ---------------------------------------------------------------------------
// daemon lifecycle
// ---------------------------------------------------------------------------

async fn run_daemon(args: DaemonArgs) -> Result<(), ClientError> {
    match args.action {
        DaemonAction::Run => {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "cu_daemon=info,cu_runtime=warn".into()),
                )
                .with_writer(std::io::stderr)
                .finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
            cu_daemon::run(cu_daemon::DaemonConfig::default())
                .await
                .map_err(|e| ClientError::Rpc {
                    code: -32000,
                    message: format!("daemon exited: {e}"),
                    data: None,
                })
        }
        DaemonAction::Start => daemon_start(args.show_secret).await,
        DaemonAction::Stop => daemon_stop().await,
        DaemonAction::Restart => daemon_restart(args.show_secret).await,
        DaemonAction::Status => daemon_status(args.show_secret).await,
    }
}

/// Print the daemon admin token for debugging. Only reachable via explicit
/// `--show-secret`; the warning is not optional — this token can shut the
/// daemon down.
fn print_admin_token_debug() {
    match cu_core::security::load_daemon_admin_token() {
        Ok(t) => {
            eprintln!(
                "WARNING: the daemon admin token below can shut the daemon down. Do not share it."
            );
            println!(
                "admin_token_path: {}",
                cu_core::config::daemon_admin_path().display()
            );
            println!("admin_token: {}", t.as_str());
        }
        Err(e) => eprintln!("cu: warning: admin token unavailable ({e})"),
    }
}

async fn daemon_start(show_secret: bool) -> Result<(), ClientError> {
    // Already running?
    if request("runtime.health", Value::Null).await.is_ok() {
        println!("daemon already running");
        if show_secret {
            print_admin_token_debug();
        }
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| ClientError::Rpc {
        code: -32000,
        message: format!("cannot locate cu binary: {e}"),
        data: None,
    })?;
    let dir = cu_core::config::runtime_dir();
    std::fs::create_dir_all(&dir).map_err(|e| ClientError::Rpc {
        code: -32000,
        message: format!("cannot create {}: {e}", dir.display()),
        data: None,
    })?;
    let log_path = dir.join("daemon.log");
    let log = std::fs::File::create(&log_path).map_err(|e| ClientError::Rpc {
        code: -32000,
        message: format!("cannot open {}: {e}", log_path.display()),
        data: None,
    })?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon")
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone().map_err(|e| {
            ClientError::Rpc {
                code: -32000,
                message: format!("dup log: {e}"),
                data: None,
            }
        })?))
        .stderr(std::process::Stdio::from(log));
    cmd.spawn().map_err(|e| ClientError::Rpc {
        code: -32000,
        message: format!("failed to launch daemon: {e}"),
        data: None,
    })?;

    // Wait for health (the daemon needs to boot the Swift bridge).
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match request("runtime.health", Value::Null).await {
            Ok(h) => {
                println!(
                    "daemon started (version {})",
                    h.get("version").and_then(|v| v.as_str()).unwrap_or("?")
                );
                println!("log: {}", log_path.display());
                if show_secret {
                    print_admin_token_debug();
                }
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    Err(ClientError::Rpc {
        code: -32000,
        message: "daemon did not become healthy within 10s; check the log".into(),
        data: None,
    })
}

async fn daemon_stop() -> Result<(), ClientError> {
    // runtime.shutdown is authorized only by the daemon admin token, which
    // the daemon persisted at startup (0600). Read it back — never guess, and
    // never fall back to a tokenless request.
    let cred = match cu_core::security::load_daemon_admin_credential() {
        Ok(c) => c,
        Err(cu_core::security::AdminTokenFileError::Missing) => {
            // The file is written before the socket binds and removed on
            // graceful exit — usually no file means no daemon (idempotent
            // stop, not an error). But a daemon that still answers health is
            // a pre-v3 binary without admin-token support; say so instead of
            // claiming it is not running.
            match request("runtime.health", Value::Null).await {
                Ok(_) => {
                    return Err(ClientError::Rpc {
                        code: -32000,
                        message: "daemon is running but has no admin token file (is it an old "
                            .to_string()
                            + "pre-v3 daemon?); stop it manually and start it again with this cu",
                        data: None,
                    });
                }
                Err(ClientError::Connect(_, _)) => {
                    println!("daemon is not running");
                    return Ok(());
                }
                Err(other) => return Err(other),
            }
        }
        Err(e @ cu_core::security::AdminTokenFileError::Corrupt(_)) => {
            // A corrupt token file must never be skipped silently: the daemon
            // would be unstoppable and the user wouldn't know why.
            return Err(ClientError::Rpc {
                code: -32000,
                message: format!("cannot read daemon admin token ({e}); refusing to guess"),
                data: None,
            });
        }
    };

    // The credential is bound to the daemon instance that wrote it. Prove the
    // running daemon *is* that instance before using the token: a credential
    // from a different install (or an older build) must never be used to shut
    // anything down.
    let version = match request("runtime.version", Value::Null).await {
        Ok(v) => v,
        Err(ClientError::Connect(_, _)) => {
            // No daemon behind the socket; the credential file is stale from
            // an earlier run. Idempotent stop, like the Missing case above.
            println!("daemon is not running");
            return Ok(());
        }
        Err(other) => return Err(other),
    };
    let running_instance =
        version["daemon_instance_id"]
            .as_str()
            .ok_or_else(|| ClientError::Rpc {
                code: -32000,
                message:
                    "daemon did not report a daemon_instance_id (is it an old pre-v3 daemon?); "
                        .to_string()
                        + "stop it manually and start it again with this cu",
                data: None,
            })?;
    if running_instance != cred.daemon_instance_id {
        return Err(ClientError::Rpc {
            code: -32000,
            message: format!(
                "admin credential belongs to daemon instance {} but the running daemon is {} — \
                 the credential is stale (different install or older build) and must never be \
                 used to shut this daemon down",
                cred.daemon_instance_id, running_instance
            ),
            data: None,
        });
    }

    match request(
        "runtime.shutdown",
        json!({ "admin_token": cred.admin_token.as_str() }),
    )
    .await
    {
        Ok(_) => {
            // Wait for the socket to disappear.
            let sock = cu_core::config::socket_path();
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if !sock.exists() {
                    break;
                }
            }
            // Defense in depth: the daemon removes its own token file on
            // graceful exit; drop any leftover so a stale token can never
            // mislead the next stop.
            cu_core::security::remove_daemon_admin_token();
            println!("daemon stopped");
            Ok(())
        }
        Err(ClientError::Connect(_, _)) => {
            // Socket unreachable: not running. Clear the stale token file a
            // crashed daemon may have left behind.
            cu_core::security::remove_daemon_admin_token();
            println!("daemon is not running");
            Ok(())
        }
        Err(other) => Err(other),
    }
}

async fn daemon_restart(show_secret: bool) -> Result<(), ClientError> {
    if request("runtime.health", Value::Null).await.is_ok() {
        daemon_stop().await?;
    }
    daemon_start(show_secret).await
}

async fn daemon_status(show_secret: bool) -> Result<(), ClientError> {
    match request("runtime.health", Value::Null).await {
        Ok(h) => {
            let version = h.get("version").and_then(|v| v.as_str()).unwrap_or("?");
            println!("running (version {version})");
            if show_secret {
                print_admin_token_debug();
            }
            Ok(())
        }
        Err(ClientError::Connect(_, _)) => {
            println!("not running");
            Err(ClientError::Connect(
                cu_core::config::socket_path(),
                std::io::Error::new(std::io::ErrorKind::NotFound, "not running"),
            ))
        }
        Err(other) => Err(other),
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

async fn run_doctor() -> Result<(), ClientError> {
    let mut ok = true;

    // 1. daemon reachable
    match request("runtime.health", Value::Null).await {
        Ok(h) => println!(
            "[ ok ] daemon reachable (version {})",
            h.get("version").and_then(|v| v.as_str()).unwrap_or("?")
        ),
        Err(_) => {
            println!("[fail] daemon not reachable — run `cu daemon start`");
            ok = false;
        }
    }

    // 2. permissions
    match request("runtime.permissions", Value::Null).await {
        Ok(p) => {
            let sr = p
                .get("screen_recording")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let ax = p
                .get("accessibility")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let marker = if sr { " ok" } else { "fail" };
            println!("[{marker:3}] screen recording: {sr} | accessibility: {ax}");
            if !sr || !ax {
                ok = false;
            }
        }
        Err(_) => {
            println!("[fail] could not read permissions");
            ok = false;
        }
    }

    // 3. displays (runtime.displays returns a bare array)
    match request("runtime.displays", Value::Null).await {
        Ok(d) => {
            let n = d.as_array().map(|a| a.len()).unwrap_or(0);
            println!("[ ok ] displays: {n}");
            if n == 0 {
                ok = false;
            }
        }
        Err(_) => {
            println!("[fail] could not enumerate displays (bridge broken?)");
            ok = false;
        }
    }

    println!();
    if ok {
        println!("doctor: all checks passed");
    } else {
        println!("doctor: some checks failed — see https://github.com/your/project#permissions");
    }
    if ok {
        Ok(())
    } else {
        Err(ClientError::Rpc {
            code: -32000,
            message: "doctor found problems".into(),
            data: None,
        })
    }
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

/// Resolve the active session's id via the public `session.summary` probe
/// (tokenless). `None` when no session exists. `status` is a sensitive read
/// in v3, so discovery never uses it.
async fn active_session_id() -> Option<String> {
    match request("session.summary", Value::Null).await {
        Ok(v) => v
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        Err(_) => None,
    }
}

async fn run_session(args: SessionArgs) -> Result<(), ClientError> {
    match args.action.as_str() {
        "start" | "status" | "summary" | "pause" | "resume" | "takeover" | "release" | "stop" => {}
        other => {
            return Err(ClientError::Rpc {
                code: -32602,
                message: format!("unknown session action `{other}` (start|status|summary|pause|resume|takeover|release|stop)"),
                data: None,
            })
        }
    }

    if args.action == "summary" {
        // The public coarse view: tokenless, no credential needed.
        let resp = request("session.summary", Value::Null).await?;
        if args.json {
            print_json(resp)?;
        } else {
            let sid = resp.get("session_id").and_then(Value::as_str).unwrap_or("");
            if sid.is_empty() {
                println!("no active session");
            } else {
                println!(
                    "session: {sid} state={} lock={}",
                    resp.get("state").and_then(Value::as_str).unwrap_or("?"),
                    resp.get("lock_held")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                );
                if let Some(owner) = resp.get("owner_client_name").and_then(Value::as_str) {
                    println!("owner: {owner}");
                }
                if let Some(msg) = resp.get("message").and_then(Value::as_str) {
                    println!("{msg}");
                }
            }
        }
        return Ok(());
    }

    if args.action == "status" {
        // Opportunistic cleanup: drop credential files whose sessions are
        // gone (stopped, or the daemon restarted) — their tokens died with
        // the sessions. Read-only status is the natural place for it.
        prune_stale_credentials().await;
    }

    let resolved: Option<String> = if args.action == "start" || args.action == "status" {
        None
    } else {
        // pause / resume / takeover / release / stop act on the session the
        // user is operating — resolve the active one when none is named.
        match &args.session_id {
            Some(id) => Some(id.clone()),
            None => active_session_id().await,
        }
    };

    let mut params = if args.action == "start" {
        let mut p = session_start_params(json!(args.display_id));
        // Round 9 / P0-5: session isolation configuration (target / pointer /
        // focus) exposed at the CLI.
        let mut target = serde_json::Map::new();
        if let Some(b) = &args.bundle_id {
            target.insert("bundle_id".into(), json!(b));
        }
        if let Some(pid) = args.pid {
            target.insert("pid".into(), json!(pid));
        }
        if let Some(wid) = args.window_id {
            target.insert("window_id".into(), json!(wid));
        }
        if !target.is_empty() {
            p["target"] = json!(target);
        }
        if let Some(pp) = &args.pointer_policy {
            p["pointer_policy"] = json!(pp);
        }
        if let Some(fp) = &args.focus_policy {
            p["focus_policy"] = json!(fp);
        }
        p
    } else if args.action == "status" {
        // status resolves the active session daemon-side; without one it is
        // a typed SESSION_NOT_FOUND, which adapters rely on. It is also a
        // sensitive read (v3): this CLI's observation credential for that
        // session is injected when held — a foreign session shows the
        // daemon's refusal instead of its details.
        let mut p = json!({"action": "status"});
        if let Some(id) = &args.session_id {
            p["session_id"] = json!(id);
        }
        let target = match &args.session_id {
            Some(id) => Some(id.clone()),
            None => active_session_id().await,
        };
        if let Some(t) = target.as_ref().and_then(|id| credentials::read_token(id)) {
            p["observation_token"] = json!(t);
        }
        p
    } else {
        let mut p = json!({"action": args.action});
        if let Some(id) = resolved.as_ref().or(args.session_id.as_ref()) {
            p["session_id"] = json!(id);
            // Every mutating action needs the control token the daemon issued
            // at start; only a credential this CLI holds authorizes it.
            if let Some(cred) = credentials::load(id) {
                p["control_token"] = json!(cred.control_token);
            }
        }
        if let Some(d) = &args.display_id {
            p["display_id"] = json!(d);
        }
        p
    };
    let resp = request("computer.session", params).await?;

    if args.action == "start" {
        // The tokens are issued exactly once, here — persist them and never
        // print them (the response is redacted even with --json).
        save_started_credential(&resp);
        emit(&redact_token(&resp), args.json);
        return Ok(());
    }

    if args.action == "stop" {
        // The session is gone and its tokens died with it — drop the file.
        if let Some(id) = resolved.as_ref().or(args.session_id.as_ref()) {
            credentials::delete(id);
        }
    }

    // Status/pause/resume/stop responses are tokenless by contract, but the
    // redaction is cheap defense if a future response ever echoes params.
    emit(&redact_token(&resp), args.json);
    Ok(())
}

// ---------------------------------------------------------------------------
// observe / inspect
// ---------------------------------------------------------------------------

async fn run_observe(args: ObserveArgs) -> Result<(), ClientError> {
    let session_id = resolve_session(&args.session_id).await?;
    let mut params = json!({
        "session_id": session_id,
        "include_image": args.include_image,
    });
    if let Some(d) = &args.display_id {
        params["display_id"] = json!(d);
    }
    // Observe is a sensitive read: inject this CLI's observation credential
    // for sessions it started (the daemon refuses tokenless reads in v3).
    if let Some(t) = credentials::read_token(&session_id) {
        params["observation_token"] = json!(t);
    }
    let resp = request("computer.observe", params).await?;

    if let Some(path) = &args.image_out {
        if let Some(b64) = resp.get("image_base64").and_then(|v| v.as_str()) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| ClientError::Message(format!("bad image data: {e}")))?;
            std::fs::write(path, &bytes).map_err(|e| ClientError::Rpc {
                code: -32000,
                message: format!("cannot write {path:?}: {e}"),
                data: None,
            })?;
        }
    }

    let mut out = resp;
    if args.image_out.is_some() {
        if let Some(v) = out.as_object_mut().and_then(|m| m.get_mut("image_base64")) {
            *v = Value::String("<written to file>".into());
        }
    }

    if args.json {
        print_json(out)
    } else {
        let w = out.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
        let h = out.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
        let frame = out.get("frame_id").and_then(|v| v.as_str()).unwrap_or("?");
        let app = out
            .get("active_application")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("frame_id: {frame}");
        println!("size: {w}x{h}");
        println!("active_application: {app}");
        if let Some(p) = out.get("image_path").and_then(|v| v.as_str()) {
            println!("image: {p}");
        }
        if let Some(path) = &args.image_out {
            println!("image written to: {}", path.display());
        }
        Ok(())
    }
}

async fn run_inspect(args: InspectArgs) -> Result<(), ClientError> {
    let session_id = resolve_session(&args.session_id).await?;
    let frame_id = match &args.frame_id {
        Some(f) => f.clone(),
        None => resolve_frame(&session_id, &None).await?,
    };

    // Parse region "x,y,w,h" (all floats).
    let parts: Vec<&str> = args.region.split(',').collect();
    let parse = |i: usize| -> Result<f64, ClientError> {
        parts
            .get(i)
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| ClientError::Rpc {
                code: -32602,
                message: format!("region must be x,y,width,height (got `{}`)", args.region),
                data: None,
            })
    };
    let region = Region {
        x: parse(0)?,
        y: parse(1)?,
        width: parse(2)?,
        height: parse(3)?,
        coordinate_space: CoordinateSpace::Normalized1000,
    };

    let mut params = json!({
        "session_id": session_id,
        "frame_id": frame_id,
        "region": {
            "x": region.x,
            "y": region.y,
            "width": region.width,
            "height": region.height,
            "coordinate_space": "normalized_1000",
        },
    });
    if let Some(s) = args.scale {
        params["scale"] = json!(s);
    }
    // Inspect is a sensitive read — inject this CLI's observation credential.
    if let Some(t) = credentials::read_token(&session_id) {
        params["observation_token"] = json!(t);
    }
    let resp = request("computer.inspect", params).await?;

    if let Some(path) = &args.image_out {
        if let Some(b64) = resp.get("image_base64").and_then(|v| v.as_str()) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| ClientError::Message(format!("bad image data: {e}")))?;
            std::fs::write(path, &bytes).map_err(|e| ClientError::Rpc {
                code: -32000,
                message: format!("cannot write {path:?}: {e}"),
                data: None,
            })?;
        }
    }

    let mut out = resp;
    if args.image_out.is_some() {
        if let Some(v) = out.as_object_mut().and_then(|m| m.get_mut("image_base64")) {
            *v = Value::String("<written to file>".into());
        }
    }

    if args.json {
        print_json(out)
    } else {
        let w = out.get("width").and_then(|v| v.as_u64()).unwrap_or(0);
        let h = out.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
        println!(
            "cropped {}x{w}x{h}",
            out.get("frame_id").and_then(|v| v.as_str()).unwrap_or("?")
        );
        if let Some(m) = out.get("mapping") {
            println!("global_origin: {:?}", m.get("global_origin"));
            println!(
                "normalized_1000_origin: {:?}",
                m.get("normalized_1000_origin")
            );
        }
        if let Some(path) = &args.image_out {
            println!("image written to: {}", path.display());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// act helpers
// ---------------------------------------------------------------------------

async fn send_act(
    session_id: &str,
    frame_id: &str,
    actions: Vec<ComputerAction>,
    wait: WaitPolicy,
    fixed_wait_ms: Option<u64>,
    screenshot: bool,
) -> Result<Value, ClientError> {
    let mut params = json!({
        "session_id": session_id,
        "frame_id": frame_id,
        "actions": actions,
        "wait_policy": match wait {
            WaitPolicy::None => "none",
            WaitPolicy::Fixed => "fixed",
            WaitPolicy::UntilStable => "until_stable",
        },
        "return_screenshot": screenshot,
    });
    if let Some(ms) = fixed_wait_ms {
        params["fixed_wait_ms"] = json!(ms);
    }
    // Acting is a mutating operation: the daemon verifies the control token
    // before executing anything. Only a credential held for this session
    // authorizes it — a session id alone is refused (CONTROL_TOKEN_REQUIRED).
    if let Some(cred) = credentials::load(session_id) {
        params["control_token"] = json!(cred.control_token);
    }
    request("computer.act", params).await
}

async fn act_and_report(
    actions: Vec<ComputerAction>,
    session_id: &Option<String>,
    frame_id: &Option<String>,
    wait: WaitPolicy,
    fixed: Option<u64>,
    screenshot: bool,
    json: bool,
) -> Result<(), ClientError> {
    let sid = resolve_session(session_id).await?;
    let fid = resolve_frame(&sid, frame_id).await?;
    let resp = send_act(&sid, &fid, actions, wait, fixed, screenshot).await?;
    if json {
        print_json(resp)
    } else {
        let executed = resp
            .get("executed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!("executed: {executed}");
        human_action_result(&resp);
        Ok(())
    }
}

async fn run_pointer(args: PointerActionArgs, double: bool) -> Result<(), ClientError> {
    let (wait, fixed) = wait_policy_from_args(args.wait_policy.clone(), args.fixed_wait_ms);
    let action = if double {
        ComputerAction::DoubleClick {
            x: args.x,
            y: args.y,
            button: match args.button {
                ButtonArg::Left => MouseButton::Left,
                ButtonArg::Right => MouseButton::Right,
                ButtonArg::Middle => MouseButton::Middle,
            },
            coordinate_space: args.coordinate_space.space(),
        }
    } else {
        click_action(args.x, args.y, &args.button, args.coordinate_space.space())
    };
    act_and_report(
        vec![action],
        &args.session_id,
        &args.frame_id,
        wait,
        fixed,
        args.screenshot,
        args.json,
    )
    .await
}

async fn run_move(args: PointerActionArgs) -> Result<(), ClientError> {
    let (wait, fixed) = wait_policy_from_args(args.wait_policy.clone(), args.fixed_wait_ms);
    act_and_report(
        vec![ComputerAction::Move {
            x: args.x,
            y: args.y,
            coordinate_space: args.coordinate_space.space(),
            duration_ms: None,
        }],
        &args.session_id,
        &args.frame_id,
        wait,
        fixed,
        args.screenshot,
        args.json,
    )
    .await
}

async fn run_type(args: TypeArgs) -> Result<(), ClientError> {
    act_and_report(
        vec![ComputerAction::TypeText {
            text: args.text.clone(),
            method: args.method.method(),
        }],
        &args.session_id,
        &args.frame_id,
        WaitPolicy::None,
        None,
        false,
        args.json,
    )
    .await
}

async fn run_key(args: KeyArgs) -> Result<(), ClientError> {
    // Accept `cu key cmd l`, `cu key cmd,l`, and `cu key "cmd l"`.
    let mut keys: Vec<String> = Vec::new();
    for raw in &args.keys {
        for part in raw.split([',', ' ', '+']) {
            if !part.is_empty() {
                keys.push(part.to_lowercase());
            }
        }
    }
    if keys.is_empty() {
        return Err(ClientError::Rpc {
            code: -32602,
            message: "key requires at least one key name".into(),
            data: None,
        });
    }
    act_and_report(
        vec![ComputerAction::Key { keys }],
        &args.session_id,
        &args.frame_id,
        WaitPolicy::None,
        None,
        false,
        args.json,
    )
    .await
}

async fn run_scroll(args: ScrollArgs) -> Result<(), ClientError> {
    act_and_report(
        vec![ComputerAction::Scroll {
            x: args.x,
            y: args.y,
            delta_x: args.delta_x,
            delta_y: args.delta_y,
            coordinate_space: args.coordinate_space.space(),
        }],
        &args.session_id,
        &args.frame_id,
        WaitPolicy::None,
        None,
        false,
        args.json,
    )
    .await
}

async fn run_drag(args: DragArgs) -> Result<(), ClientError> {
    act_and_report(
        vec![ComputerAction::Drag {
            from: Point {
                x: args.from_x,
                y: args.from_y,
            },
            to: Point {
                x: args.to_x,
                y: args.to_y,
            },
            coordinate_space: args.coordinate_space.space(),
            duration_ms: None,
        }],
        &args.session_id,
        &args.frame_id,
        WaitPolicy::None,
        None,
        false,
        args.json,
    )
    .await
}

async fn run_wait(args: WaitArgs) -> Result<(), ClientError> {
    act_and_report(
        vec![ComputerAction::Wait {
            duration_ms: args.ms,
        }],
        &args.session_id,
        &args.frame_id,
        WaitPolicy::None,
        None,
        false,
        args.json,
    )
    .await
}

// ---------------------------------------------------------------------------
// traces
// ---------------------------------------------------------------------------

async fn run_trace(args: TraceArgs) -> Result<(), ClientError> {
    match args.action {
        TraceAction::List => {
            // The cross-session trace listing is daemon-manager only since
            // round 6: `trace.admin_list` requires the daemon admin token — a
            // session capability must never reveal which other sessions ran
            // on the machine. This CLI holds the admin credential; same
            // missing/corrupt handling as `daemon_stop` (never guess).
            let admin = match cu_core::security::load_daemon_admin_token() {
                Ok(t) => t,
                Err(cu_core::security::AdminTokenFileError::Missing) => {
                    match request("runtime.health", Value::Null).await {
                        Ok(_) => {
                            return Err(ClientError::Rpc {
                                code: -32000,
                                message:
                                    "daemon is running but has no admin token file (is it an old "
                                        .to_string()
                                        + "pre-v3 daemon?); no admin credential to list traces with",
                                data: None,
                            });
                        }
                        Err(ClientError::Connect(_, _)) => {
                            println!("daemon is not running");
                            return Ok(());
                        }
                        Err(other) => return Err(other),
                    }
                }
                Err(e @ cu_core::security::AdminTokenFileError::Corrupt(_)) => {
                    return Err(ClientError::Rpc {
                        code: -32000,
                        message: format!("cannot read daemon admin token ({e}); refusing to guess"),
                        data: None,
                    });
                }
            };
            let resp =
                request("trace.admin_list", json!({ "admin_token": admin.as_str() })).await?;
            if let Some(traces) = resp.get("traces").and_then(|v| v.as_array()) {
                if traces.is_empty() {
                    println!("no traces yet");
                } else {
                    println!(
                        "{:<12} {:>6} {:>8}  created_at",
                        "session", "events", "bytes"
                    );
                    for t in traces {
                        let sid = t.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
                        let entries = t.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
                        let bytes = t.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let at = t.get("created_at").and_then(|v| v.as_str()).unwrap_or("?");
                        println!("{sid:<12} {entries:>6} {bytes:>8}  {at}");
                    }
                }
            }
            Ok(())
        }
        TraceAction::Get { session_id } => {
            // Trace contents are a sensitive read — the observation credential
            // is required; trace.list is likewise session-scoped since round 6
            // (only the daemon-manager `trace.admin_list` sees across sessions).
            let mut p = json!({ "session_id": session_id });
            if let Some(t) = credentials::read_token(&session_id) {
                p["observation_token"] = json!(t);
            }
            let resp = request("trace.get", p).await?;
            println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            Ok(())
        }
        TraceAction::Export {
            session_id,
            output,
            force,
        } => {
            // Round 7: trace.export is a pure read. The daemon returns the
            // content inline (no destination path is accepted); writing a
            // user-chosen file is this CLI's job, with overwrite protection.
            let mut p = json!({ "session_id": session_id });
            if let Some(t) = credentials::read_token(&session_id) {
                p["observation_token"] = json!(t);
            }
            let resp = request("trace.export", p).await?;
            let content = resp
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ClientError::BadResponse("trace.export returned no content".into())
                })?;
            let sha256 = resp.get("sha256").and_then(|v| v.as_str()).unwrap_or("?");
            let file_name = resp
                .get("file_name")
                .and_then(|v| v.as_str())
                .unwrap_or("trace.jsonl");
            match &output {
                Some(dest) => {
                    write_export_file(dest, content, force)?;
                    println!(
                        "exported {file_name} → {} ({} bytes, sha256 {sha256})",
                        dest.display(),
                        content.len()
                    );
                }
                None => {
                    print!("{content}");
                    if !content.ends_with('\n') {
                        println!();
                    }
                }
            }
            Ok(())
        }
        TraceAction::Replay { session_id } => {
            let mut p = json!({ "session_id": session_id });
            if let Some(t) = credentials::read_token(&session_id) {
                p["observation_token"] = json!(t);
            }
            let resp = request("trace.replay", p).await?;
            println!("replay: {}", serde_json::to_string_pretty(&resp).unwrap());
            Ok(())
        }
        TraceAction::Analyze { session_id, json } => {
            // The analysis reads the trace through trace.export — the pure
            // read path — with the observation credential, never a bare
            // session id.
            let mut p = json!({ "session_id": session_id });
            if let Some(t) = credentials::read_token(&session_id) {
                p["observation_token"] = json!(t);
            }
            let resp = request("trace.export", p).await?;
            let content = resp
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ClientError::BadResponse("trace.export returned no content".into())
                })?;
            let entries = cu_trace::parse_jsonl(content)
                .map_err(|e| ClientError::Message(format!("cannot parse trace: {e}")))?;
            let analysis = cu_trace::analyze(&entries, 15);
            if json {
                println!("{}", serde_json::to_string_pretty(&analysis).unwrap());
            } else {
                print_analysis(&analysis);
            }
            Ok(())
        }
    }
}

/// Render a trace analysis for humans (the `--json` variant prints the
/// structured analysis as-is).
fn print_analysis(a: &cu_trace::TraceAnalysis) {
    println!("session: {}", a.session_id);
    println!(
        "events: {}  actions: {}  observes: {}  screenshot bytes: {}",
        a.event_count, a.total_actions, a.observe_calls, a.screenshot_bytes
    );
    let duration = a
        .duration_ms
        .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
        .unwrap_or_else(|| "?".into());
    let span = match (a.started_at, a.stopped_at) {
        (Some(s), Some(t)) => format!(" ({s} → {t})"),
        _ => String::new(),
    };
    println!("duration: {duration}{span}");
    if !a.actions_by_type.is_empty() {
        let by_type: Vec<String> = a
            .actions_by_type
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        println!("actions by type: {}", by_type.join(", "));
    }
    println!(
        "results: success {}, failed {}, cancelled {} | stale rejected: {} | timeouts: {} | takeovers: {} | cancel events: {}",
        a.total_actions.saturating_sub(a.failed_action_count),
        a.failed_action_count,
        a.cancelled_request_count,
        a.stale_frame_count,
        a.timeout_count,
        a.user_takeover_count,
        a.cancel_event_count
    );
    match (&a.failure_category, &a.failure_detail) {
        (Some(cat), Some(detail)) => println!("failure: {cat} — {detail}"),
        (Some(cat), None) => println!("failure: {cat}"),
        (None, _) => println!("failure: (no failure signal in trace)"),
    }
    if !a.timeline.is_empty() {
        println!("timeline (last {}):", a.timeline.len());
        for t in &a.timeline {
            println!("  +{:>7}ms  {:<18} {}", t.offset_ms, t.event, t.detail);
        }
    }
}

/// Write an exported trace to a user-chosen path (round 7). The daemon never
/// writes a destination — saving the content is the CLI's job, and it must
/// refuse to overwrite an existing file unless `force` is set.
fn write_export_file(dest: &Path, content: &str, force: bool) -> Result<(), ClientError> {
    if dest.exists() && !force {
        return Err(ClientError::Message(format!(
            "{} already exists — refusing to overwrite (use --force)",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ClientError::Message(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
    }
    std::fs::write(dest, content)
        .map_err(|e| ClientError::Message(format!("cannot write {}: {e}", dest.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_write_protects_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("trace.jsonl");

        // Missing file → saved.
        write_export_file(&dest, "line1\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "line1\n");

        // Existing file, no --force → refused, content untouched.
        let err = write_export_file(&dest, "line2\n", false).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "line1\n",
            "the existing file must be untouched"
        );

        // --force → overwritten.
        write_export_file(&dest, "line2\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "line2\n");
    }

    #[test]
    fn export_write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a").join("b").join("trace.jsonl");
        write_export_file(&dest, "x\n", false).unwrap();
        assert!(dest.exists());
    }

    #[test]
    fn export_write_refuses_directory_targets() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path();
        // An existing directory is refused out of the box…
        let err = write_export_file(dest, "x\n", false).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
        // …and even with --force the OS refuses to write a directory.
        let err = write_export_file(dest, "x\n", true).unwrap_err();
        assert!(err.to_string().contains("cannot write"));
    }
}
