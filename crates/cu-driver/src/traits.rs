//! The platform driver contract. `cu-runtime` knows nothing about macOS,
//! CoreGraphics, or ScreenCaptureKit — it only knows this trait. New platforms
//! (Windows, Linux) implement the same trait without touching the runtime.

use async_trait::async_trait;
use cu_core::{CuError, MouseButton, Point, TextInputMethod};

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
}

pub use crate::types::ActionResult;
