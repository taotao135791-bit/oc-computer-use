//! `cu` — the computer-use runtime command line.
//!
//! Talks JSON-RPC 2.0 to the daemon over `~/.computer-use/runtime.sock`
//! (`cu daemon run` serves in-process; `cu daemon start` launches it detached).
//! Every subcommand exits non-zero on failure and supports `--json` for
//! machine-readable output.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use cu_cli::client::{request, ClientError};
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
    /// Trace inspection: list / get / export / replay / summaries.
    Trace(TraceArgs),
}

// ---------------------------------------------------------------------------
// Subcommand argument structs
// ---------------------------------------------------------------------------

#[derive(Args)]
struct DaemonArgs {
    #[command(subcommand)]
    action: DaemonAction,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Launch the daemon detached and wait until it is healthy.
    Start,
    /// Run the daemon in the foreground (used by `daemon start`).
    Run,
    /// Ask the daemon to shut down gracefully.
    Stop,
    /// Print whether the daemon is running and its version.
    Status,
}

#[derive(Args)]
struct SessionArgs {
    /// start | status | pause | resume | takeover | release | stop
    action: String,
    /// Target session; defaults to the active one for status.
    #[arg(long)]
    session_id: Option<String>,
    /// Display to bind a new session to.
    #[arg(long)]
    display_id: Option<String>,
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
    /// List all trace files.
    List,
    /// Print a trace's JSONL entries.
    Get { session_id: String },
    /// Copy a trace to an external path.
    Export { session_id: String, dest: PathBuf },
    /// Re-run the actions recorded in a trace on the live desktop.
    Replay { session_id: String },
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => {}
        Err(e) => {
            match &e {
                ClientError::Rpc {
                    data,
                    message,
                    code,
                } => {
                    // Render the machine-readable data.code if present.
                    if let Some(Value::String(code_str)) = data.as_ref().and_then(|d| d.get("code"))
                    {
                        eprintln!("cu: {code_str} — {message}");
                    } else if data.is_some() {
                        eprintln!("cu: [{code}] {message}: {data:?}");
                    } else {
                        eprintln!("cu: [{code}] {message}");
                    }
                }
                other => eprintln!("cu: {other}"),
            }
            std::process::exit(e.exit_code());
        }
    }
}

async fn run(cli: Cli) -> Result<(), ClientError> {
    match cli.command {
        Command::Daemon(args) => run_daemon(args).await,
        Command::Doctor => run_doctor().await,
        Command::Permissions => print_json(request("runtime.permissions", Value::Null).await?),
        Command::Displays => print_json(request("runtime.displays", Value::Null).await?),
        Command::Pointer => print_json(request("runtime.pointer", Value::Null).await?),
        Command::ActiveApp => print_json(request("runtime.active_application", Value::Null).await?),
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

/// `--json` flag forms: many commands carry it, but simple list-like commands
/// always print raw JSON (the `print_json` path).
fn print_json(value: Value) -> Result<(), ClientError> {
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
    Ok(())
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

/// Resolve a session: the one named by `session_id`, or the currently active
/// one when the caller left it unspecified.
async fn resolve_session(session_id: &Option<String>) -> Result<String, ClientError> {
    if let Some(id) = session_id {
        return Ok(id.clone());
    }
    let resp = request("computer.session", json!({"action": "status"})).await?;
    resp.get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ClientError::Rpc {
            code: -32602,
            message: "no active session; start one with `cu session start`".into(),
            data: Some(json!({"code": "NO_ACTIVE_SESSION"})),
        })
}

/// Frame id for an action. When the caller did not pin a specific stored
/// frame, capture a fresh one right before acting — a one-shot CLI action
/// should always reference what the screen looks like *now* (the runtime's
/// stale-frame check would reject an old frame against changed pixels).
async fn resolve_frame(session_id: &str, frame_id: &Option<String>) -> Result<String, ClientError> {
    if let Some(f) = frame_id {
        return Ok(f.clone());
    }
    let obs = request(
        "computer.observe",
        json!({"session_id": session_id, "include_image": false}),
    )
    .await?;
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
        DaemonAction::Start => daemon_start().await,
        DaemonAction::Stop => daemon_stop().await,
        DaemonAction::Status => daemon_status().await,
    }
}

async fn daemon_start() -> Result<(), ClientError> {
    // Already running?
    if request("runtime.health", Value::Null).await.is_ok() {
        println!("daemon already running");
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
    // Idempotent: stopping a daemon that is not running is not an error.
    match request("runtime.shutdown", Value::Null).await {
        Ok(_) => {
            // Wait for the socket to disappear.
            let sock = cu_core::config::socket_path();
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if !sock.exists() {
                    break;
                }
            }
            println!("daemon stopped");
            Ok(())
        }
        Err(ClientError::Connect(_, _)) => {
            println!("daemon is not running");
            Ok(())
        }
        Err(other) => Err(other),
    }
}

async fn daemon_status() -> Result<(), ClientError> {
    match request("runtime.health", Value::Null).await {
        Ok(h) => {
            let version = h.get("version").and_then(|v| v.as_str()).unwrap_or("?");
            println!("running (version {version})");
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

async fn run_session(args: SessionArgs) -> Result<(), ClientError> {
    match args.action.as_str() {
        "start" | "status" | "pause" | "resume" | "takeover" | "release" | "stop" => {}
        other => {
            return Err(ClientError::Rpc {
                code: -32602,
                message: format!("unknown session action `{other}` (start|status|pause|resume|takeover|release|stop)"),
                data: None,
            })
        }
    }
    let mut params = json!({"action": args.action});
    if let Some(id) = &args.session_id {
        params["session_id"] = json!(id);
    }
    if let Some(d) = &args.display_id {
        params["display_id"] = json!(d);
    }
    let resp = request("computer.session", params).await?;
    emit(&resp, args.json);
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
    let resp = request("computer.observe", params).await?;

    if let Some(path) = &args.image_out {
        if let Some(b64) = resp.get("image_base64").and_then(|v| v.as_str()) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| ClientError::BadResponse(format!("bad image data: {e}")))?;
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
    let resp = request("computer.inspect", params).await?;

    if let Some(path) = &args.image_out {
        if let Some(b64) = resp.get("image_base64").and_then(|v| v.as_str()) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| ClientError::BadResponse(format!("bad image data: {e}")))?;
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
            let resp = request("trace.list", Value::Null).await?;
            if let Some(traces) = resp.get("traces").and_then(|v| v.as_array()) {
                if traces.is_empty() {
                    println!("no traces yet");
                } else {
                    println!(
                        "{:<12} {:>6} {:>8}  started_at",
                        "session", "entries", "bytes"
                    );
                    for t in traces {
                        let sid = t.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
                        let entries = t.get("entries").and_then(|v| v.as_u64()).unwrap_or(0);
                        let bytes = t.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                        let at = t.get("started_at").and_then(|v| v.as_str()).unwrap_or("?");
                        println!("{sid:<12} {entries:>6} {bytes:>8}  {at}");
                    }
                }
            }
            Ok(())
        }
        TraceAction::Get { session_id } => {
            let resp = request("trace.get", json!({ "session_id": session_id })).await?;
            println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            Ok(())
        }
        TraceAction::Export { session_id, dest } => {
            let resp = request(
                "trace.export",
                json!({ "session_id": session_id, "dest": dest.to_string_lossy() }),
            )
            .await?;
            if let Some(path) = resp.get("path").and_then(|v| v.as_str()) {
                println!(
                    "exported {} → {path}",
                    resp.get("format")
                        .and_then(|v| v.as_str())
                        .unwrap_or("jsonl")
                );
            }
            Ok(())
        }
        TraceAction::Replay { session_id } => {
            let resp = request("trace.replay", json!({ "session_id": session_id })).await?;
            println!("replay: {}", serde_json::to_string_pretty(&resp).unwrap());
            Ok(())
        }
    }
}
