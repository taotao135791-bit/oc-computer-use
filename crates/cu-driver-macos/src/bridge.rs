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
//! If none exist, it is compiled from the bundled Swift source via `swiftc`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cu_core::{CuError, ErrorCode};
use serde_json::Value;

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

impl Bridge {
    pub fn new() -> Self {
        Self { inner: Mutex::new(None), binary_path: default_bridge_path() }
    }

    pub fn set_binary_path(&mut self, path: PathBuf) {
        self.binary_path = path;
    }

    fn ensure_path(&self) -> Result<String, CuError> {
        let binary = ensure_bridge_binary(&self.binary_path)?;
        binary.to_str().map(|s| s.to_string()).ok_or_else(|| {
            CuError::Driver("bridge path is not valid UTF-8".into())
        })
    }

    /// Send one command and get back the parsed `data` object or an error.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, CuError> {
        let binary = self.ensure_path()?;
        let mut guard = self.inner.lock().map_err(|_| CuError::Internal("bridge mutex poisoned".into()))?;

        // (Re)start the process if it died.
        if guard.is_none() {
            *guard = Some(spawn(&binary)?);
        }
        let bp = guard.as_mut().unwrap();

        let line = serde_json::json!({"id": 1, "method": method, "params": params});
        let mut payload = serde_json::to_string(&line).map_err(|e| CuError::Internal(e.to_string()))?;
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

    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            return Err(CuError::Driver("bridge request timed out".into()));
        }
        line.clear();
        let n = bp
            .stdout
            .read_line(&mut line)
            .map_err(|e| CuError::Driver(format!("bridge read failed: {e}")))?;
        if n == 0 {
            return Err(CuError::Driver("bridge process exited unexpectedly".into()));
        }
        if line.trim().is_empty() {
            continue;
        }
        break;
    }

    let value: Value = serde_json::from_str(&line)
        .map_err(|e| CuError::Driver(format!("bridge returned invalid JSON: {e}")))?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    } else {
        let msg = value.get("error").and_then(Value::as_str).unwrap_or("bridge error").to_string();
        Err(CuError::Driver(msg))
    }
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
fn ensure_bridge_binary(existing: &PathBuf) -> Result<PathBuf, CuError> {
    if existing.exists() {
        return Ok(existing.clone());
    }
    for c in bridge_candidates() {
        if c.exists() {
            return Ok(c);
        }
    }
    // Build it into ~/.computer-use/bin once.
    let home = std::env::var("HOME")
        .map_err(|_| CuError::Driver("HOME is not set".into()))?;
    let bin_dir = PathBuf::from(&home).join(".computer-use/bin");
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| CuError::Driver(format!("cannot create {bin_dir:?}: {e}")))?;
    let target = bin_dir.join("cubridge");
    build_bridge(&target)?;
    Ok(target)
}

/// Compile the bundled Swift bridge with `swiftc`.
fn build_bridge(target: &PathBuf) -> Result<(), CuError> {
    let src = swift_source_path().ok_or_else(|| {
        CuError::Driver("cannot locate bundled Swift bridge source".into())
    })?;
    let status = Command::new("swiftc")
        .args([
            "-O", "-o",
        ])
        .arg(target)
        .arg(&src)
        .args([
            "-framework", "ScreenCaptureKit",
            "-framework", "CoreGraphics",
            "-framework", "AppKit",
            "-framework", "ApplicationServices",
            "-framework", "CoreImage",
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
    let candidates = [
        "swift/CUBridge/Sources/CUBridge/main.swift",
        "../../swift/CUBridge/Sources/CUBridge/main.swift",
        "../cu-driver-macos/swift/CUBridge/Sources/CUBridge/main.swift",
    ];
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let base = PathBuf::from(manifest);
        for c in candidates {
            let p = base.join(c);
            if p.exists() {
                return Some(p);
            }
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
                        name: d.get("name").and_then(Value::as_str).unwrap_or("Display").to_string(),
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
    bridge.request(method, Value::Object(params.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_paths_are_absolute() {
        for c in bridge_candidates() {
            assert!(c.is_absolute() || c.components().count() > 1, "odd candidate: {c:?}");
        }
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
