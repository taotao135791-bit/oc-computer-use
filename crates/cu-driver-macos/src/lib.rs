//! `cu-driver-macos`: the real macOS driver.
//!
//! - Displays, captures, the active app, permissions, and clipboard come from
//!   the small Swift [`bridge`] (ScreenCaptureKit / AppKit are the mature APIs).
//! - Mouse, keyboard, scrolling, and pointer location are CoreGraphics FFI in
//!   [`mouse`], [`keyboard`], and [`ffi`].
//! - The driver never converts coordinates. It receives
//!   [`cu_driver::ResolvedAction`]s in global logical points and posts events.

pub mod bridge;
pub mod capture;
pub mod ffi;
pub mod keyboard;
pub mod mouse;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cu_core::{CuError, DisplayBounds, ImageGeometry, Point, TextInputMethod};
use cu_driver::{
    ApplicationInfo, CaptureRequest, CapturedFrame, ComputerDriver, DesktopLayout, PermissionStatus,
    PointerInfo, QuickSnapshot, ResolvedAction,
};
use serde_json::Value;

use crate::bridge::Bridge;

/// A driver instance bound to the local machine.
pub struct MacosDriver {
    bridge: Bridge,
    /// Cached desktop layout, refreshed when displays change.
    layout: Mutex<Option<DesktopLayout>>,
}

impl Default for MacosDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosDriver {
    pub fn new() -> Self {
        Self { bridge: Bridge::new(), layout: Mutex::new(None) }
    }

    /// Build a driver that uses a specific bridge binary (used by the CLI's
    /// `doctor` command and tests).
    pub fn with_bridge(path: std::path::PathBuf) -> Self {
        let mut b = Bridge::new();
        b.set_binary_path(path);
        Self { bridge: b, layout: Mutex::new(None) }
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
                let scale = if b.size.width > 0.0 { pw as f64 / b.size.width } else { 1.0 };
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
        let params = serde_json::json!({
            "display": request.display_id,
            "output": request.output_path.to_string_lossy(),
            "shows_cursor": request.include_cursor,
            "max_width": request.max_width,
            "format": request.format,
            "quality": request.jpeg_quality,
        });
        let data = self.bridge.request("capture", params)?;
        let width = data
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| CuError::Driver("capture returned no width".into()))? as u32;
        let height = data
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| CuError::Driver("capture returned no height".into()))? as u32;
        let scale = data.get("display_scale_factor").and_then(Value::as_f64).unwrap_or(1.0);

        // Permission failures from ScreenCaptureKit surface as a missing frame;
        // map them to a structured PERMISSION error with guidance.
        let perm = self.permission_status().await?;
        if !perm.screen_recording {
            return Err(CuError::permission(cu_core::PermissionKind::ScreenRecording, false));
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
            image_bytes: std::fs::read(&request.output_path).map_err(|e| {
                CuError::Driver(format!("cannot read captured image: {e}"))
            })?,
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

    async fn execute(&self, action: &ResolvedAction) -> Result<cu_driver::ActionResult, CuError> {
        let started = Instant::now();
        let outcome = match action {
            ResolvedAction::Click { x, y, button, double } => {
                if *double {
                    mouse::double_click(*button, *x, *y);
                } else {
                    mouse::click(*button, *x, *y);
                }
                Ok(())
            }
            ResolvedAction::Move { from, to, duration_ms } => {
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
            ResolvedAction::Scroll { at, delta_x, delta_y } => {
                mouse::scroll(*delta_x, *delta_y, *at).await;
                Ok(())
            }
            ResolvedAction::Drag { from, to, duration_ms } => {
                mouse::drag(*from, *to, *duration_ms).await;
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
        let bundle_id = data.get("bundle_id").and_then(Value::as_str).unwrap_or("unknown");
        let name = data.get("name").and_then(Value::as_str).unwrap_or("unknown");
        let window_title = data.get("window_title").and_then(Value::as_str).map(|s| s.to_string());
        if bundle_id == "unknown" && name == "unknown" {
            Ok(None)
        } else {
            Ok(Some(ApplicationInfo {
                bundle_id: bundle_id.to_string(),
                name: name.to_string(),
                window_title,
            }))
        }
    }

    async fn pointer_location(&self) -> Result<PointerInfo, CuError> {
        let p = ffi::current_mouse_location();
        Ok(PointerInfo { location: Point::new(p.x, p.y), display_id: None })
    }

    async fn shutdown(&self) -> Result<(), CuError> {
        self.bridge.shutdown();
        Ok(())
    }
}

impl MacosDriver {
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
        keyboard::post_combo(&keyboard::parse_combo(&["CMD".to_string(), "V".to_string()])?);
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
pub fn geometry_for(display: &cu_driver::DisplayInfo, image_width: u32, image_height: u32) -> ImageGeometry {
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
            bounds: DisplayBounds { x: 0.0, y: 0.0, width: 1440.0, height: 900.0 },
            pixel_width: 2880,
            pixel_height: 1800,
            scale_factor: 2.0,
            is_main: true,
        };
        let g = geometry_for(&d, 2880, 1800);
        assert!((g.scale_factor() - 2.0).abs() < 1e-9);
    }
}
