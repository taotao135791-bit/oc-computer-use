//! Manages the Swift `cubridge` helper process.
//!
//! The bridge is the only piece of Swift in the project (ScreenCaptureKit and
//! NSPasteboard have no stable Rust bindings worth depending on). Rust keeps
//! all *runtime* logic; the bridge is a dumb JSON-RPC-per-line worker for
//! capture, display names, the active app, permissions, and the clipboard.
//!
//! The bridge binary is located in this order:
//!   1. `$COMPUTER_USE_BRIDGE`
//!   2. `~/.computer-use/bin/cubridge`
//!   3. next to the running executable
//!   4. `target/<profile>/cubridge` (dev tree)
//!
//! If none exist, it is compiled from the bundled Swift source via `swiftc`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cu_core::{CuError, ErrorCode};
use serde_json::Value;

/// Upper bound on how long we wait for one bridge request. The bridge is a
/// dumb worker; if it does not answer in time it is wedged (e.g. a macOS
/// permission prompt or a dead run loop) and must be treated as failed —
/// an unwedged bridge answers in milliseconds.
const BRIDGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct BridgeProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Lazily-initialized, process-wide bridge.
pub struct Bridge {
    inner: Mutex<Option<BridgeProcess>>,
    binary_path: PathBuf,
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

impl Bridge {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
            binary_path: default_bridge_path(),
        }
    }

    pub fn set_binary_path(&mut self, path: PathBuf) {
        self.binary_path = path;
    }

    fn ensure_path(&self) -> Result<String, CuError> {
        let binary = ensure_bridge_binary(&self.binary_path)?;
        binary
            .to_str()
            .map(|s| s.to_string())
            .ok_or_else(|| CuError::Driver("bridge path is not valid UTF-8".into()))
    }

    /// Send one command and get back the parsed `data` object or an error.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, CuError> {
        let binary = self.ensure_path()?;
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CuError::Internal("bridge mutex poisoned".into()))?;

        // (Re)start the process if it died.
        if guard.is_none() {
            *guard = Some(spawn(&binary)?);
        }
        let bp = guard.as_mut().unwrap();

        let line = serde_json::json!({"id": 1, "method": method, "params": params});
        let mut payload =
            serde_json::to_string(&line).map_err(|e| CuError::Internal(e.to_string()))?;
        payload.push('\n');

        // The process may have died between checks; retry once with a fresh spawn.
        let result = try_request(bp, &payload);
        match result {
            Ok(v) => Ok(v),
            Err(e) if e.code() == ErrorCode::DriverError => {
                *guard = Some(spawn(&binary)?);
                try_request(guard.as_mut().unwrap(), &payload)
            }
            Err(e) => Err(e),
        }
    }

    pub fn shutdown(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(mut bp) = guard.take() {
            let _ = bp.stdin.write_all(b"{\"id\":0,\"method\":\"shutdown\"}\n");
            let _ = bp.stdin.flush();
            let _ = bp.child.kill();
            let _ = bp.child.wait();
        }
    }
}

fn try_request(bp: &mut BridgeProcess, payload: &str) -> Result<Value, CuError> {
    bp.stdin
        .write_all(payload.as_bytes())
        .and_then(|_| bp.stdin.flush())
        .map_err(|e| CuError::Driver(format!("bridge write failed: {e}")))?;

    // Read exactly one response line, bounded by a hard deadline. A plain
    // blocking read_line could stall forever (e.g. the bridge stuck on a
    // macOS permission prompt), and this call runs inside the daemon's async
    // runtime — an uninterruptible block would wedge every client. poll(2)
    // gives us a timeout that actually fires.
    let deadline = Instant::now() + BRIDGE_REQUEST_TIMEOUT;
    let mut line = String::new();
    loop {
        match read_line_deadline(&mut bp.stdout, &mut line, deadline) {
            Ok(true) if line.trim().is_empty() => {
                line.clear();
                continue;
            }
            Ok(true) => break,
            Ok(false) => return Err(CuError::Driver("bridge process exited unexpectedly".into())),
            Err(_) => {
                return Err(CuError::Driver(format!(
                    "bridge request timed out after {}s (is a macOS permission dialog pending?)",
                    BRIDGE_REQUEST_TIMEOUT.as_secs()
                )))
            }
        }
    }

    let value: Value = serde_json::from_str(&line)
        .map_err(|e| CuError::Driver(format!("bridge returned invalid JSON: {e}")))?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(extract_payload(&value))
    } else {
        let msg = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("bridge error")
            .to_string();
        Err(CuError::Driver(msg))
    }
}

/// Read one line (terminated by `\n`) from the bridge's stdout, never blocking
/// past `deadline`. Returns `Ok(true)` when a line was read (without the
/// newline), `Ok(false)` at EOF.
///
/// Why not `read_line` + a timeout check around it: `read_line` blocks inside
/// the kernel with no deadline, and this runs on the daemon's executor. We
/// poll(2) for readability, then consume whatever is buffered, so a silent
/// bridge can never wedge the daemon.
fn read_line_deadline(
    stdout: &mut BufReader<ChildStdout>,
    line: &mut String,
    deadline: Instant,
) -> Result<bool, std::io::Error> {
    loop {
        // poll(2) FIRST: a plain fill_buf() would block in the kernel with no
        // deadline if the child stays silent, and this runs on the daemon's
        // async executor where an uninterruptible block wedges every client.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "bridge read deadline exceeded",
            ));
        }
        let mut pfd = libc::pollfd {
            fd: stdout.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "bridge read deadline exceeded",
            ));
        }

        // Readable (or HUP): the fd now has data or is at EOF, so fill_buf
        // cannot block.
        let buffered = stdout.fill_buf()?;
        if let Some(pos) = buffered.iter().position(|&b| b == b'\n') {
            line.push_str(&String::from_utf8_lossy(&buffered[..pos]));
            let consumed = pos + 1; // ends the borrow of `buffered`
            stdout.consume(consumed);
            return Ok(true);
        }
        if buffered.is_empty() {
            return Ok(false); // EOF
        }
        // Partial line so far; keep it and wait for the rest.
        let consumed = buffered.len(); // ends the borrow of `buffered`
        line.push_str(&String::from_utf8_lossy(&buffered[..consumed]));
        stdout.consume(consumed);
    }
}

/// Extract the payload from a bridge response line.
///
/// The bridge speaks two shapes:
/// - wrapped: `{"ok":true,"data":{...}}`
/// - flat:    `{"ok":true,"width":...,"id":1}` — everything but the metadata
///   keys is the payload (this is what the Swift bridge actually emits).
fn extract_payload(value: &Value) -> Value {
    if let Some(data) = value.get("data") {
        return data.clone();
    }
    let mut payload = value.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.remove("id");
        obj.remove("ok");
        obj.remove("error");
    }
    payload
}

fn spawn(binary: &str) -> Result<BridgeProcess, CuError> {
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| CuError::Driver(format!("failed to launch bridge {binary}: {e}")))?;
    Ok(BridgeProcess {
        stdin: child.stdin.take().expect("stdin"),
        stdout: BufReader::new(child.stdout.take().expect("stdout")),
        child,
    })
}

/// Path candidates for the bridge binary, in priority order.
pub fn bridge_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("COMPUTER_USE_BRIDGE") {
        v.push(PathBuf::from(p));
    }
    if let Ok(home) = std::env::var("HOME") {
        v.push(PathBuf::from(home).join(".computer-use/bin/cubridge"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("cubridge"));
        }
    }
    // Dev tree: <repo>/target/<profile>/cubridge
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        if let Ok(profile) = std::env::var("PROFILE") {
            let base = PathBuf::from(manifest);
            v.push(base.join("../../target").join(&profile).join("cubridge"));
        }
    }
    v
}

pub fn default_bridge_path() -> PathBuf {
    for c in bridge_candidates() {
        if c.exists() {
            return c;
        }
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".computer-use/bin/cubridge"))
        .unwrap_or_else(|_| PathBuf::from("cubridge"))
}

/// Return a usable bridge binary path, building it from the bundled Swift
/// source if no candidate exists yet.
fn ensure_bridge_binary(existing: &Path) -> Result<PathBuf, CuError> {
    if existing.exists() {
        return Ok(existing.to_path_buf());
    }
    for c in bridge_candidates() {
        if c.exists() {
            return Ok(c);
        }
    }
    // Build it into ~/.computer-use/bin once.
    let home = std::env::var("HOME").map_err(|_| CuError::Driver("HOME is not set".into()))?;
    let bin_dir = PathBuf::from(&home).join(".computer-use/bin");
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| CuError::Driver(format!("cannot create {bin_dir:?}: {e}")))?;
    let target = bin_dir.join("cubridge");
    build_bridge(&target)?;
    Ok(target)
}

/// Compile the bundled Swift bridge with `swiftc`.
fn build_bridge(target: &PathBuf) -> Result<(), CuError> {
    let src = swift_source_path()
        .ok_or_else(|| CuError::Driver("cannot locate bundled Swift bridge source".into()))?;
    let status = Command::new("swiftc")
        .args(["-O", "-o"])
        .arg(target)
        .arg(&src)
        .args([
            "-framework",
            "ScreenCaptureKit",
            "-framework",
            "CoreGraphics",
            "-framework",
            "AppKit",
            "-framework",
            "ApplicationServices",
            "-framework",
            "CoreImage",
        ])
        .status()
        .map_err(|e| CuError::Driver(format!("cannot run swiftc: {e}")))?;
    if !status.success() {
        return Err(CuError::Driver(
            "Swift bridge compilation failed (is Xcode command line tools installed?)".into(),
        ));
    }
    Ok(())
}

fn swift_source_path() -> Option<PathBuf> {
    // Runtime override first (packaged installs / tests).
    if let Ok(p) = std::env::var("COMPUTER_USE_SWIFT_SRC") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let candidates = [
        "swift/CUBridge/Sources/CUBridge/main.swift",
        "../../swift/CUBridge/Sources/CUBridge/main.swift",
        "../cu-driver-macos/swift/CUBridge/Sources/CUBridge/main.swift",
    ];
    // `CARGO_MANIFEST_DIR` is a compile-time var; the detached daemon does not
    // have it in its runtime environment, so bake the value in here.
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for c in candidates {
        let p = base.join(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Parse the `displays` array returned by the bridge into driver display info.
pub fn parse_displays(data: &Value) -> Vec<cu_driver::DisplayInfo> {
    let main_id = crate::ffi::main_display_id();
    data.get("displays")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    let id = d.get("id")?.as_str()?.to_string();
                    let bounds = d.get("bounds")?;
                    Some(cu_driver::DisplayInfo {
                        id: id.clone(),
                        name: d
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("Display")
                            .to_string(),
                        bounds: cu_core::DisplayBounds {
                            x: bounds.get("x")?.as_f64()?,
                            y: bounds.get("y")?.as_f64()?,
                            width: bounds.get("width")?.as_f64()?,
                            height: bounds.get("height")?.as_f64()?,
                        },
                        pixel_width: d.get("pixel_width")?.as_u64()? as u32,
                        pixel_height: d.get("pixel_height")?.as_u64()? as u32,
                        scale_factor: d.get("scale_factor").and_then(Value::as_f64).unwrap_or(1.0),
                        is_main: id == main_id.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Convenience accessor for one-off bridge commands from the driver.
pub fn bridge_request_map(method: &str, params: HashMap<&str, Value>) -> Result<Value, CuError> {
    let bridge = Bridge::new();
    bridge.request(
        method,
        Value::Object(
            params
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_payload_handles_both_shapes() {
        // Flat shape (what the Swift bridge emits).
        let flat = serde_json::json!({"id": 1, "ok": true, "width": 100, "height": 50});
        assert_eq!(extract_payload(&flat)["width"], 100);
        assert_eq!(extract_payload(&flat)["height"], 50);
        assert!(extract_payload(&flat).get("id").is_none());
        assert!(extract_payload(&flat).get("ok").is_none());

        // Wrapped shape (backwards-compatible).
        let wrapped = serde_json::json!({"ok": true, "id": 2, "data": {"width": 200}});
        assert_eq!(extract_payload(&wrapped)["width"], 200);
    }

    #[test]
    fn candidate_paths_are_absolute() {
        for c in bridge_candidates() {
            assert!(
                c.is_absolute() || c.components().count() > 1,
                "odd candidate: {c:?}"
            );
        }
    }

    #[test]
    fn read_line_deadline_reads_a_complete_line() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf '{\"ok\":true}\n'")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let ok = read_line_deadline(&mut out, &mut line, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert!(ok);
        assert_eq!(line, r#"{"ok":true}"#);
        child.wait().unwrap();
    }

    #[test]
    fn read_line_deadline_reports_eof() {
        let mut child = Command::new("true").stdout(Stdio::piped()).spawn().unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let ok = read_line_deadline(&mut out, &mut line, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert!(!ok, "EOF should be reported as Ok(false)");
        child.wait().unwrap();
    }

    #[test]
    fn read_line_deadline_times_out_on_a_silent_child() {
        let mut child = Command::new("sleep")
            .arg("10")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let started = Instant::now();
        let res = read_line_deadline(&mut out, &mut line, started + Duration::from_millis(250));
        assert!(res.is_err(), "silent child must time out, got {res:?}");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout took too long: {elapsed:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn read_line_deadline_accumulates_partial_lines() {
        // The child writes half a line, waits, then finishes it — the reader
        // must hold the partial content across polls.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf '{\"ok\":' && sleep 1 && printf 'true}\n'")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let ok = read_line_deadline(&mut out, &mut line, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert!(ok);
        assert_eq!(line, r#"{"ok":true}"#);
        child.wait().unwrap();
    }

    #[test]
    fn parse_displays_handles_bridge_payload() {
        let data = serde_json::json!({
            "displays": [{
                "id": "1",
                "name": "Built-in Retina",
                "bounds": {"x": 0, "y": 0, "width": 1440, "height": 900},
                "pixel_width": 2880,
                "pixel_height": 1800,
                "scale_factor": 2.0,
                "is_main": true
            }]
        });
        let displays = parse_displays(&data);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].pixel_width, 2880);
        assert!((displays[0].scale_factor - 2.0).abs() < 1e-9);
    }
}
