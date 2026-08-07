//! The platform driver contract. `cu-runtime` knows nothing about macOS,
//! CoreGraphics, or ScreenCaptureKit — it only knows this trait. New platforms
//! (Windows, Linux) implement the same trait without touching the runtime.

use async_trait::async_trait;
use cu_core::{CuError, MouseButton, Point, TextInputMethod};
use tokio_util::sync::CancellationToken;

use crate::types::{
    ApplicationInfo, CaptureRequest, CapturedFrame, DesktopLayout, DisplayInfo, PermissionStatus,
    PointerInfo, QuickSnapshot,
};

/// An action whose coordinates have already been resolved to **global logical
/// points** by the runtime. Drivers never do coordinate math — they are pure
/// physical actuators. Keeping the raw [`cu_core::ComputerAction`] (which may
/// carry `normalized_1000`/`image_pixels` coordinates) out of the driver
/// guarantees no driver can scatter scale-factor arithmetic into mouse code.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAction {
    Click {
        x: f64,
        y: f64,
        button: MouseButton,
        double: bool,
    },
    Move {
        from: Point,
        to: Point,
        duration_ms: Option<u64>,
    },
    TypeText {
        text: String,
        method: TextInputMethod,
    },
    Key {
        keys: Vec<String>,
    },
    Scroll {
        /// Optional location to move the pointer to before scrolling.
        at: Option<Point>,
        delta_x: f64,
        delta_y: f64,
    },
    Drag {
        from: Point,
        to: Point,
        duration_ms: Option<u64>,
    },
    Wait {
        duration_ms: u64,
    },
}

#[async_trait]
pub trait ComputerDriver: Send + Sync {
    /// List all attached displays with global logical bounds and scale factors.
    async fn list_displays(&self) -> Result<Vec<DisplayInfo>, CuError>;

    /// Current desktop layout (display list + primary id).
    async fn desktop_layout(&self) -> Result<DesktopLayout, CuError>;

    /// Capture a display to an image file per [`CaptureRequest`].
    async fn capture(&self, request: CaptureRequest) -> Result<CapturedFrame, CuError>;

    /// Capture a cheap low-res snapshot (thumbnail + active app) for
    /// stale-frame and stability checks. Must be fast (no full-res encode).
    async fn quick_snapshot(&self, display_id: &str) -> Result<QuickSnapshot, CuError>;

    /// Execute one atomic, fully-resolved action (global-point coordinates).
    async fn execute(&self, action: &ResolvedAction) -> Result<ActionResult, CuError>;

    /// Execute a potentially long-running action with mid-action cancellation
    /// (P0-2): Drag / long Scroll / physical move / wait must be interruptible
    /// between steps. The default delegates to [`execute`](Self::execute), so
    /// test/fake drivers keep working; the macOS driver overrides it with real
    /// step-by-step cancellation. When cancelled mid-drag after mouse-down,
    /// the driver MUST still send the corresponding mouse-up so the system is
    /// never left with a stuck pressed button.
    async fn execute_with_cancel(
        &self,
        action: &ResolvedAction,
        cancel: CancellationToken,
    ) -> Result<ActionResult, CuError> {
        let _ = cancel;
        self.execute(action).await
    }

    /// Current TCC permission state.
    async fn permission_status(&self) -> Result<PermissionStatus, CuError>;

    /// Frontmost application (bundle id, name, focused window title if known).
    async fn active_application(&self) -> Result<Option<ApplicationInfo>, CuError>;

    /// Current pointer location in global logical points.
    async fn pointer_location(&self) -> Result<PointerInfo, CuError>;

    /// Round 8: the agent's virtual pointer moved — the driver should refresh
    /// its ghost-cursor overlay (if any) at that global point. This never
    /// moves the real system cursor. Default implementation is a no-op so
    /// test/fake drivers need not implement it.
    async fn pointer_visualized(&self, _x: f64, _y: f64, _display_id: &str) -> Result<(), CuError> {
        Ok(())
    }

    /// Round 8: hide the ghost cursor overlay (takeover / pause / stop).
    async fn pointer_hidden(&self) -> Result<(), CuError> {
        Ok(())
    }

    /// Release any OS-level resources (CGEvent sources, the bridge process).
    async fn shutdown(&self) -> Result<(), CuError>;

    /// Round 8 / Phase 11: start the driver's continuous human-input monitor
    /// (macOS Event Tap). `sink` receives the real `event_to_tap_ms` latency
    /// for every non-synthetic human event. Returns `true` when a monitor is
    /// running. Default is a no-op so test/fake drivers need not implement it.
    fn start_human_input_monitor(&self, _sink: Box<dyn Fn(u64) + Send + Sync>) -> bool {
        false
    }

    /// Round 9 / P0-7: a PHYSICAL click (warp the real cursor + down/up) at
    /// a global point. This is the physical fallback actuator — it DOES move
    /// the user's cursor and must only be used under `physical_allowed`.
    /// Returns `Ok(false)` when not supported (test/fake drivers).
    async fn physical_click_at(
        &self,
        _button: MouseButton,
        _x: f64,
        _y: f64,
    ) -> Result<bool, CuError> {
        Ok(false)
    }

    /// Round 9 / P0-7: attempt an Accessibility `AXPress` click at a global
    /// point for `pid`'s application. This is an isolated actuator (the real
    /// system cursor is never moved). Returns:
    /// - `Ok(true)` — AXPress executed.
    /// - `Ok(false)` — AX unavailable/unsupported at that point.
    /// - `Err` — permission or driver failure.
    /// Default is `Ok(false)` so test/fake drivers keep working.
    async fn click_via_accessibility(&self, _pid: i32, _x: f64, _y: f64) -> Result<bool, CuError> {
        Ok(false)
    }

    /// Round 9 / P0-8: human-input detection health. Returns the monitor
    /// state string: `"active" | "starting" | "failed" | "stopped"`, or
    /// `None` when no hardware event monitor exists (e.g. test/fake drivers
    /// or a platform without an Event Tap). When `Some("active")`, the
    /// runtime disables the pointer-delta heuristic (real hardware events are
    /// authoritative); otherwise the heuristic fallback may run.
    fn human_input_monitor_state(&self) -> Option<String> {
        None
    }

    /// Round 9 / P0-4: resolve a session target (bundle id / pid / window id)
    /// to a concrete window with current bounds. The runtime calls this at
    /// session start and before every coordinate-bearing action. Default
    /// implementations return `None` (no target support); the macOS driver
    /// implements it via the Swift bridge's `resolve_target`.
    async fn resolve_target(
        &self,
        _target: &cu_core::SessionTarget,
    ) -> Result<Option<crate::types::ResolvedSessionTarget>, CuError> {
        Ok(None)
    }

    /// Round 9 / P0-4: refresh the current bounds of an already-resolved
    /// target. Windows move / resize / minimize / close / recreate, so every
    /// coordinate action re-resolves before acting. `None` when the target
    /// is gone (`TARGET_UNAVAILABLE`).
    async fn resolve_target_bounds(
        &self,
        _window_id: u32,
    ) -> Result<Option<cu_core::DisplayBounds>, CuError> {
        Ok(None)
    }
}

pub use crate::types::ActionResult;
