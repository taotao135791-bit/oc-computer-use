//! `cu-driver-macos`: the real macOS driver.
//!
//! - Displays, captures, the active app, permissions, and clipboard come from
//!   the small Swift [`bridge`] (ScreenCaptureKit / AppKit are the mature APIs).
//! - Mouse, keyboard, scrolling, and pointer location are CoreGraphics FFI in
//!   [`mouse`], [`keyboard`], and [`ffi`].
//! - The driver never converts coordinates. It receives
//!   [`cu_driver::ResolvedAction`]s in global logical points and posts events.

pub mod accessibility;
pub mod bridge;
pub mod capture;
pub mod event_tap;
pub mod ffi;
pub mod keyboard;
pub mod mouse;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cu_core::{CuError, DisplayBounds, ImageGeometry, MouseButton, Point, TextInputMethod};
use cu_driver::{
    ApplicationInfo, CaptureRequest, CapturedFrame, ComputerDriver, DesktopLayout,
    PermissionStatus, PointerInfo, QuickSnapshot, ResolvedAction,
};
use serde_json::Value;

use crate::bridge::Bridge;

/// Visual states of the agent's Ghost Cursor overlay (round 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMode {
    Isolated,
    PhysicalFallback,
    Paused,
    UserTakeover,
}

impl OverlayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayMode::Isolated => "isolated",
            OverlayMode::PhysicalFallback => "physical_fallback",
            OverlayMode::Paused => "paused",
            OverlayMode::UserTakeover => "user_takeover",
        }
    }
}

impl From<cu_core::PointerMode> for OverlayMode {
    fn from(m: cu_core::PointerMode) -> Self {
        match m {
            cu_core::PointerMode::Isolated => OverlayMode::Isolated,
            cu_core::PointerMode::PhysicalFallback => OverlayMode::PhysicalFallback,
            cu_core::PointerMode::Paused => OverlayMode::Paused,
            cu_core::PointerMode::UserTakeover => OverlayMode::UserTakeover,
        }
    }
}

/// A driver instance bound to the local machine.
pub struct MacosDriver {
    bridge: Bridge,
    /// Cached desktop layout, refreshed when displays change.
    layout: Mutex<Option<DesktopLayout>>,
    /// The Event Tap human-input monitor (P0-3): dedicated native thread,
    /// real timestamps, joinable shutdown, exposed health state.
    event_tap: std::sync::Arc<event_tap::EventTapMonitor>,
}

impl Default for MacosDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosDriver {
    pub fn new() -> Self {
        Self {
            bridge: Bridge::new(),
            layout: Mutex::new(None),
            event_tap: std::sync::Arc::new(event_tap::EventTapMonitor::new()),
        }
    }

    /// Current Event Tap health state (`active` / `failed` / `stopped` /
    /// `starting`). The runtime / CLI / status surface this so a degraded
    /// Human Always Wins is never silently assumed active.
    pub fn human_input_state(&self) -> event_tap::EventTapState {
        self.event_tap.state()
    }

    /// Build a driver that uses a specific bridge binary (used by the CLI's
    /// `doctor` command and tests).
    pub fn with_bridge(path: std::path::PathBuf) -> Self {
        let mut b = Bridge::new();
        b.set_binary_path(path);
        Self {
            bridge: b,
            layout: Mutex::new(None),
            event_tap: std::sync::Arc::new(event_tap::EventTapMonitor::new()),
        }
    }

    /// Path to the Swift bridge binary, building it on first use if needed.
    pub fn bridge_path(&self) -> Result<std::path::PathBuf, CuError> {
        // Trigger ensure by doing a cheap request; capture the resolved path.
        let _ = self.bridge.request("permissions", Value::Null)?;
        Ok(crate::bridge::default_bridge_path())
    }

    fn resolve_display(&self, display_id: &str) -> Result<cu_driver::DisplayInfo, CuError> {
        let layout = self.desktop_layout_sync()?;
        layout
            .displays
            .iter()
            .find(|d| d.id == display_id)
            .cloned()
            .ok_or_else(|| CuError::InvalidParams(format!("unknown display_id `{display_id}`")))
    }

    fn desktop_layout_sync(&self) -> Result<DesktopLayout, CuError> {
        let data = self.bridge.request("displays", Value::Null)?;
        let displays = crate::bridge::parse_displays(&data);
        if displays.is_empty() {
            // Fall back to pure FFI geometry (no bridge needed).
            let ffi_displays = self.ffi_displays();
            if ffi_displays.is_empty() {
                return Err(CuError::Driver("no displays enumerated".into()));
            }
            *self.layout.lock().unwrap() = Some(DesktopLayout {
                primary_id: ffi::main_display_id().to_string(),
                displays: ffi_displays,
            });
        } else {
            *self.layout.lock().unwrap() = Some(DesktopLayout {
                primary_id: ffi::main_display_id().to_string(),
                displays,
            });
        }
        Ok(self.layout.lock().unwrap().clone().unwrap())
    }

    /// Pure-FFI fallback display list (works even without the Swift bridge).
    fn ffi_displays(&self) -> Vec<cu_driver::DisplayInfo> {
        let main = ffi::main_display_id();
        ffi::list_active_displays()
            .into_iter()
            .map(|id| {
                let b = ffi::display_bounds(id);
                let (pw, ph) = ffi::display_pixels(id);
                let scale = if b.size.width > 0.0 {
                    pw as f64 / b.size.width
                } else {
                    1.0
                };
                cu_driver::DisplayInfo {
                    id: id.to_string(),
                    name: format!("Display {id}"),
                    bounds: DisplayBounds {
                        x: b.origin.x,
                        y: b.origin.y,
                        width: b.size.width,
                        height: b.size.height,
                    },
                    pixel_width: pw as u32,
                    pixel_height: ph as u32,
                    scale_factor: scale,
                    is_main: id == main,
                }
            })
            .collect()
    }
}

#[async_trait]
impl ComputerDriver for MacosDriver {
    async fn list_displays(&self) -> Result<Vec<cu_driver::DisplayInfo>, CuError> {
        Ok(self.desktop_layout_sync()?.displays)
    }

    async fn desktop_layout(&self) -> Result<DesktopLayout, CuError> {
        self.desktop_layout_sync()
    }

    async fn capture(&self, request: CaptureRequest) -> Result<CapturedFrame, CuError> {
        let mut params = serde_json::json!({
            "display": request.display_id,
            "output": request.output_path.to_string_lossy(),
            "shows_cursor": request.include_cursor,
            "max_width": request.max_width,
            "format": request.format,
            "quality": request.jpeg_quality,
        });
        // P0-6: window-scoped observe crops the capture to a pixel rectangle.
        if let Some(r) = request.region {
            params["region"] = serde_json::json!([r.x, r.y, r.width, r.height]);
        }
        let data = self.bridge.request("capture", params)?;
        let width = data
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| CuError::Driver("capture returned no width".into()))?
            as u32;
        let height = data
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| CuError::Driver("capture returned no height".into()))?
            as u32;
        let scale = data
            .get("display_scale_factor")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);

        // Permission failures from ScreenCaptureKit surface as a missing frame;
        // map them to a structured PERMISSION error with guidance.
        let perm = self.permission_status().await?;
        if !perm.screen_recording {
            return Err(CuError::permission(
                cu_core::PermissionKind::ScreenRecording,
                false,
            ));
        }

        let display = self.resolve_display(&request.display_id)?;
        let active = self.active_application().await?;
        Ok(CapturedFrame {
            display_id: request.display_id.clone(),
            width,
            height,
            scale_factor: scale,
            bounds: display.bounds,
            image_path: request.output_path.clone(),
            image_bytes: std::fs::read(&request.output_path)
                .map_err(|e| CuError::Driver(format!("cannot read captured image: {e}")))?,
            format: request.format,
            active_application: active,
            captured_at: chrono::Utc::now(),
        })
    }

    async fn quick_snapshot(&self, display_id: &str) -> Result<QuickSnapshot, CuError> {
        self.resolve_display(display_id)?;
        let tmp_dir = std::env::temp_dir().join("computer-use-quick");
        std::fs::create_dir_all(&tmp_dir).ok();
        let path = tmp_dir.join(format!("quick-{display_id}-{}.png", std::process::id()));
        let req = CaptureRequest {
            display_id: display_id.to_string(),
            output_path: path.clone(),
            include_cursor: true,
            max_width: 96,
            format: "png".into(),
            jpeg_quality: 70,
            region: None,
        };
        let frame = self.capture(req).await?;
        let thumbnail = capture::to_grayscale_thumbnail(&frame.image_bytes, 64, 64)?;
        let active = self.active_application().await?;
        let _ = std::fs::remove_file(&path);
        Ok(QuickSnapshot {
            thumbnail,
            thumb_width: 64,
            thumb_height: 64,
            display_id: display_id.to_string(),
            active_application: active,
            captured_at: chrono::Utc::now(),
        })
    }

    /// Execute a long-running action with mid-action cancellation (P0-2).
    /// Drag / long Scroll / physical move / wait check the token between
    /// steps; a cancelled drag ALWAYS sends mouse-up (never a stuck button).
    async fn execute_with_cancel(
        &self,
        action: &ResolvedAction,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<cu_driver::ActionResult, CuError> {
        let started = Instant::now();
        let outcome = match action {
            ResolvedAction::Click {
                x,
                y,
                button,
                double,
            } => {
                if *double {
                    mouse::double_click_direct(*button, *x, *y);
                } else {
                    mouse::click_direct(*button, *x, *y);
                }
                Ok(())
            }
            ResolvedAction::Move {
                from,
                to,
                duration_ms,
            } => {
                if mouse::move_pointer_smooth_cancel(*from, *to, *duration_ms, Some(&cancel)).await
                {
                    Ok(())
                } else {
                    Err(CuError::Cancelled)
                }
            }
            ResolvedAction::TypeText { text, method } => match method {
                TextInputMethod::Keyboard => {
                    if text.is_empty() {
                        Ok(())
                    } else {
                        keyboard::type_text(text);
                        Ok(())
                    }
                }
                TextInputMethod::Clipboard => self.type_via_clipboard(text).await,
            },
            ResolvedAction::Key { keys } => {
                let combo = keyboard::parse_combo(keys)?;
                keyboard::post_combo(&combo);
                Ok(())
            }
            ResolvedAction::Scroll {
                at,
                delta_x,
                delta_y,
            } => {
                if mouse::scroll(*delta_x, *delta_y, *at, Some(&cancel)).await {
                    Ok(())
                } else {
                    Err(CuError::Cancelled)
                }
            }
            ResolvedAction::Drag {
                from,
                to,
                duration_ms,
            } => {
                if mouse::drag(*from, *to, *duration_ms, Some(&cancel)).await {
                    Ok(())
                } else {
                    Err(CuError::Cancelled)
                }
            }
            ResolvedAction::Wait { duration_ms } => {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(*duration_ms);
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {}
                    () = cancel.cancelled() => {}
                }
                if cancel.is_cancelled() {
                    Err(CuError::Cancelled)
                } else {
                    Ok(())
                }
            }
        };
        match outcome {
            Ok(()) => Ok(cu_driver::ActionResult {
                success: true,
                duration_ms: started.elapsed().as_millis() as u64,
                detail: None,
            }),
            Err(CuError::Cancelled) => Ok(cu_driver::ActionResult {
                success: false,
                duration_ms: started.elapsed().as_millis() as u64,
                detail: Some("cancelled by user takeover".into()),
            }),
            Err(e) => Ok(cu_driver::ActionResult {
                success: false,
                duration_ms: started.elapsed().as_millis() as u64,
                detail: Some(e.to_string()),
            }),
        }
    }

    async fn execute(&self, action: &ResolvedAction) -> Result<cu_driver::ActionResult, CuError> {
        let started = Instant::now();
        let outcome = match action {
            ResolvedAction::Click {
                x,
                y,
                button,
                double,
            } => {
                // Round 8 pointer isolation: the default click path is
                // **DirectPositionEvent** — post down/up at the target WITHOUT
                // warping the system cursor. The visible cursor stays where the
                // user left it. (Legacy `click` still exists for the physical
                // fallback path and the pointer-lab A/B experiment.)
                if *double {
                    mouse::double_click_direct(*button, *x, *y);
                } else {
                    mouse::click_direct(*button, *x, *y);
                }
                Ok(())
            }
            ResolvedAction::Move {
                from,
                to,
                duration_ms,
            } => {
                mouse::move_pointer_smooth(*from, *to, *duration_ms).await;
                Ok(())
            }
            ResolvedAction::TypeText { text, method } => match method {
                TextInputMethod::Keyboard => {
                    if text.is_empty() {
                        Ok(())
                    } else {
                        keyboard::type_text(text);
                        Ok(())
                    }
                }
                TextInputMethod::Clipboard => self.type_via_clipboard(text).await,
            },
            ResolvedAction::Key { keys } => {
                let combo = keyboard::parse_combo(keys)?;
                keyboard::post_combo(&combo);
                Ok(())
            }
            ResolvedAction::Scroll {
                at,
                delta_x,
                delta_y,
            } => {
                mouse::scroll(*delta_x, *delta_y, *at, None).await;
                Ok(())
            }
            ResolvedAction::Drag {
                from,
                to,
                duration_ms,
            } => {
                mouse::drag(*from, *to, *duration_ms, None).await;
                Ok(())
            }
            ResolvedAction::Wait { duration_ms } => {
                tokio::time::sleep(Duration::from_millis(*duration_ms)).await;
                Ok(())
            }
        };
        match outcome {
            Ok(()) => Ok(cu_driver::ActionResult {
                success: true,
                duration_ms: started.elapsed().as_millis() as u64,
                detail: None,
            }),
            Err(e) => Ok(cu_driver::ActionResult {
                success: false,
                duration_ms: started.elapsed().as_millis() as u64,
                detail: Some(e.to_string()),
            }),
        }
    }

    async fn permission_status(&self) -> Result<PermissionStatus, CuError> {
        let data = self.bridge.request("permissions", Value::Null)?;
        Ok(PermissionStatus {
            screen_recording: data
                .get("screen_recording")
                .and_then(Value::as_bool)
                .unwrap_or_else(ffi::preflight_screen_recording),
            accessibility: data
                .get("accessibility")
                .and_then(Value::as_bool)
                .unwrap_or_else(ffi::is_process_trusted_for_accessibility),
        })
    }

    async fn active_application(&self) -> Result<Option<ApplicationInfo>, CuError> {
        let data = self.bridge.request("active", Value::Null)?;
        let bundle_id = data
            .get("bundle_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let name = data
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let window_title = data
            .get("window_title")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        // P0-5: the strict focus guard needs the frontmost app's pid and window
        // id too — a bundle match on a recycled pid must not pass as focus.
        let pid = data.get("pid").and_then(Value::as_i64).map(|p| p as i32);
        let window_id = data
            .get("window_id")
            .and_then(Value::as_i64)
            .map(|w| w as u32);
        if bundle_id == "unknown" && name == "unknown" {
            Ok(None)
        } else {
            Ok(Some(ApplicationInfo {
                bundle_id: bundle_id.to_string(),
                name: name.to_string(),
                window_title,
                pid,
                window_id,
            }))
        }
    }

    async fn pointer_location(&self) -> Result<PointerInfo, CuError> {
        let p = ffi::current_mouse_location();
        Ok(PointerInfo {
            location: Point::new(p.x, p.y),
            display_id: None,
        })
    }

    async fn pointer_visualized(
        &self,
        x: f64,
        y: f64,
        display_id: &str,
        mode: cu_core::PointerMode,
    ) -> Result<(), CuError> {
        // Audit: the overlay mode is the SESSION's real pointer mode, so the
        // physical_fallback / paused / user_takeover visual states are
        // reachable (before, `OverlayMode::Isolated` was hardcoded and
        // `From<PointerMode>` was dead).
        self.overlay_show(x, y, display_id, mode.into());
        Ok(())
    }

    async fn pointer_hidden(&self) -> Result<(), CuError> {
        self.overlay_hide();
        Ok(())
    }

    async fn pointer_click_ripple(&self, x: f64, y: f64) -> Result<(), CuError> {
        self.overlay_click_ripple(x, y);
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), CuError> {
        // P0-3: join the Event Tap thread (disable + stop run loop + join) so
        // no native thread or CFMachPort leaks. Then stop the bridge child.
        self.event_tap.shutdown();
        self.bridge.shutdown();
        Ok(())
    }

    fn start_human_input_monitor(&self, sink: Box<dyn Fn(u64) + Send + Sync>) -> bool {
        // P0-3: dedicated native thread with its own CFRunLoop. `start` is
        // async in the sense that the tap becomes live on the thread shortly
        // after; health is visible via `human_input_state()`.
        self.event_tap.start(sink);
        // Return true ONLY when the monitor is genuinely live. If the thread
        // failed to spawn or the tap failed to create (e.g. Accessibility not
        // granted), `state()` will be `Failed` and this returns false — no
        // fake success.
        matches!(
            self.event_tap.state(),
            event_tap::EventTapState::Active | event_tap::EventTapState::Starting
        )
    }

    fn human_input_monitor_state(&self) -> Option<String> {
        Some(self.event_tap.state().as_str().to_string())
    }

    /// Round 9 / P0-7: physical click = warp cursor + down/up.
    async fn physical_click_at(
        &self,
        button: MouseButton,
        x: f64,
        y: f64,
    ) -> Result<bool, CuError> {
        // Synchronous CGEvent post; the async wrapper is just for the trait.
        crate::mouse::click(button, x, y);
        Ok(true)
    }

    /// Round 9 / P0-7: Accessibility `AXPress` click — an isolated actuator
    /// that never moves the real system cursor. Returns `Ok(false)` when AX
    /// lookup failed or the element does not support press.
    async fn click_via_accessibility(&self, pid: i32, x: f64, y: f64) -> Result<bool, CuError> {
        crate::accessibility::press_at(pid, x, y).await
    }

    /// Round 9 / P0-4: resolve a session target via the Swift bridge
    /// (`resolve_target` uses CGWindowListCopyWindowInfo). The bridge returns
    /// the frontmost visible normal window for a bundle/pid, or verifies an
    /// exact window id — never a random pick.
    async fn resolve_target(
        &self,
        target: &cu_core::SessionTarget,
    ) -> Result<Option<cu_driver::ResolvedSessionTarget>, CuError> {
        let mut params = serde_json::json!({});
        if let Some(b) = &target.bundle_id {
            params["bundle_id"] = serde_json::json!(b);
        }
        if let Some(pid) = target.pid {
            params["pid"] = serde_json::json!(pid);
        }
        if let Some(wid) = target.window_id {
            params["window_id"] = serde_json::json!(wid);
        }
        let data = self.bridge.request("resolve_target", params)?;
        parse_resolved_target(&data)
    }

    /// Round 9 / P0-4: refresh bounds for an already-resolved window.
    async fn resolve_target_bounds(
        &self,
        window_id: u32,
    ) -> Result<Option<cu_core::DisplayBounds>, CuError> {
        let data = self.bridge.request(
            "resolve_target",
            serde_json::json!({ "window_id": window_id }),
        )?;
        if data.get("found").and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let w = data
            .get("window")
            .ok_or_else(|| CuError::Driver("resolve_target_bounds: missing window".into()))?;
        Ok(w.get("bounds").and_then(|b| {
            Some(cu_core::DisplayBounds {
                x: b.get("x")?.as_f64()?,
                y: b.get("y")?.as_f64()?,
                width: b.get("width")?.as_f64()?,
                height: b.get("height")?.as_f64()?,
            })
        }))
    }
}

/// Parse the bridge's `resolve_target` response into a resolved session
/// target. Identity is the Focus Guard's foundation (P0-4/P0-5): the bundle id
/// MUST be the RESOLVED WINDOW's own owner bundle — never the active app's,
/// which would silently bind a session to the wrong app. A window that cannot
/// be fully identified (missing / `"unknown"` bundle) fails closed to `None`.
/// Extracted so the identity contract is testable without a live Swift bridge.
fn parse_resolved_target(
    data: &Value,
) -> Result<Option<cu_driver::ResolvedSessionTarget>, CuError> {
    if data.get("found").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let w = data
        .get("window")
        .ok_or_else(|| CuError::Driver("resolve_target: missing window".into()))?;
    let pid = w
        .get("pid")
        .and_then(Value::as_i64)
        .ok_or_else(|| CuError::Driver("resolve_target: missing pid".into()))? as i32;
    let window_id = w
        .get("window_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| CuError::Driver("resolve_target: missing window_id".into()))?
        as u32;
    let bundle_id = match w.get("bundle_id").and_then(Value::as_str) {
        Some(b) if b != "unknown" => b.to_string(),
        _ => return Ok(None),
    };
    let bounds = w.get("bounds").and_then(|b| {
        Some(cu_core::DisplayBounds {
            x: b.get("x")?.as_f64()?,
            y: b.get("y")?.as_f64()?,
            width: b.get("width")?.as_f64()?,
            height: b.get("height")?.as_f64()?,
        })
    });
    Ok(Some(cu_driver::ResolvedSessionTarget {
        bundle_id,
        pid,
        window_id,
        bounds,
    }))
}

impl MacosDriver {
    /// Show the agent's Ghost Cursor overlay at a global point (CGEvent space).
    pub fn overlay_show(&self, x: f64, y: f64, display_id: &str, mode: OverlayMode) {
        let _ = self.bridge.request(
            "cursor_overlay",
            serde_json::json!({
                "action": "show",
                "x": x,
                "y": y,
                "display": display_id,
                "mode": mode.as_str(),
            }),
        );
    }

    /// Hide the agent's Ghost Cursor overlay.
    pub fn overlay_hide(&self) {
        let _ = self
            .bridge
            .request("cursor_overlay", serde_json::json!({"action": "hide"}));
    }

    /// Play the click ripple at a global point (visible confirmation).
    pub fn overlay_click_ripple(&self, x: f64, y: f64) {
        let _ = self.bridge.request(
            "cursor_overlay",
            serde_json::json!({"action": "click_ripple", "x": x, "y": y}),
        );
    }

    /// Type text by swapping the pasteboard, pasting, and restoring the
    /// previous pasteboard contents. The clipboard text is never logged.
    async fn type_via_clipboard(&self, text: &str) -> Result<(), CuError> {
        // 1. Save current pasteboard.
        let before = self
            .bridge
            .request("clipboard_get", Value::Null)
            .ok()
            .and_then(|v| v.get("payload").cloned())
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        // 2. Set temp text.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let payload = format!("public.utf8-plain-text:{b64}");
        let set = self
            .bridge
            .request("clipboard_set", serde_json::json!({"payload": payload}))?;
        let set_ok = set.get("set").and_then(Value::as_bool).unwrap_or(false);
        if !set_ok {
            return Err(CuError::Driver("clipboard set failed".into()));
        }

        // 3. Paste via CMD+V.
        keyboard::post_combo(&keyboard::parse_combo(&[
            "CMD".to_string(),
            "V".to_string(),
        ])?);
        tokio::time::sleep(Duration::from_millis(60)).await;

        // 4. Restore previous contents (best-effort; failure is logged, not fatal).
        if !before.is_empty() {
            let restore_ok = self
                .bridge
                .request("clipboard_set", serde_json::json!({"payload": before}))
                .ok()
                .and_then(|v| v.get("set").and_then(Value::as_bool))
                .unwrap_or(false);
            if !restore_ok {
                tracing::warn!("could not restore clipboard contents after text input");
            }
        }
        Ok(())
    }
}

/// Helper to build an [`ImageGeometry`] for a captured frame.
pub fn geometry_for(
    display: &cu_driver::DisplayInfo,
    image_width: u32,
    image_height: u32,
) -> ImageGeometry {
    ImageGeometry {
        image_width_px: image_width,
        image_height_px: image_height,
        display_bounds: display.bounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_for_uses_display_bounds() {
        let d = cu_driver::DisplayInfo {
            id: "1".into(),
            name: "D".into(),
            bounds: DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
            },
            pixel_width: 2880,
            pixel_height: 1800,
            scale_factor: 2.0,
            is_main: true,
        };
        let g = geometry_for(&d, 2880, 1800);
        assert!((g.scale_factor() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn resolve_parse_returns_the_resolved_windows_own_identity() {
        // Section 三十八 test 6 / P0-4-P0-5: the target's bundle/pid/window all
        // come from the WINDOW the resolver actually picked (the bridge echoes
        // the owner bundle inside the window entry), never the active app's.
        let data = serde_json::json!({
            "found": true,
            "window": {
                "pid": 4242,
                "window_id": 777,
                "bundle_id": "com.apple.Safari",
                "bounds": {"x": 10.0, "y": 20.0, "width": 1200.0, "height": 800.0}
            }
        });
        let r = parse_resolved_target(&data).unwrap().unwrap();
        assert_eq!(r.pid, 4242, "pid is the resolved window's pid");
        assert_eq!(r.window_id, 777, "window id is the resolved window's id");
        assert_eq!(
            r.bundle_id, "com.apple.Safari",
            "bundle is the resolved window's OWN owner"
        );
        let b = r.bounds.unwrap();
        assert_eq!(b.x, 10.0);
        assert_eq!(b.y, 20.0);
        assert_eq!(b.width, 1200.0);
        assert_eq!(b.height, 800.0);
    }

    #[test]
    fn resolve_parse_fails_closed_on_missing_or_unknown_bundle() {
        // Section 三十八 test 6: a window we cannot fully identify must never
        // become a session target. Unresolved, missing bundle, and an
        // `"unknown"` bundle all resolve to None (fail closed).
        assert!(
            parse_resolved_target(&serde_json::json!({"found": false}))
                .unwrap()
                .is_none(),
            "unresolved target -> None"
        );
        assert!(
            parse_resolved_target(&serde_json::json!({
                "found": true,
                "window": {"pid": 1, "window_id": 2, "bundle_id": "unknown"}
            }))
            .unwrap()
            .is_none(),
            "'unknown' bundle -> None (would break the Focus Guard)"
        );
        assert!(
            parse_resolved_target(&serde_json::json!({
                "found": true,
                "window": {"pid": 1, "window_id": 2}
            }))
            .unwrap()
            .is_none(),
            "missing bundle -> None (cannot identify the window)"
        );
    }
}
