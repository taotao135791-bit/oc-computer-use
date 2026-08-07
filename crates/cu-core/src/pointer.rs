//! Pointer isolation primitives: the agent's **virtual pointer**, execution
//! backends, policies, and physical-fallback bookkeeping.
//!
//! The core idea of this module: the agent's logical pointer is *not* the
//! macOS system cursor. [`VirtualPointerState`] is the single source of truth
//! for "where the agent means to point"; the runtime decides how to realize
//! that intent (isolated CGEvent click, accessibility press, or a physical
//! fallback that briefly borrows the real cursor).
//!
//! Nothing in this module posts events or touches CoreGraphics — it is pure
//! state + policy, shared by the runtime, trace, and inspector.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::coordinates::Point;

fn now() -> Instant {
    Instant::now()
}

/// Where the agent *means* to point. Never confused with the system cursor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VirtualPointerState {
    /// Global logical desktop point (CGEvent space).
    pub x: f64,
    pub y: f64,
    /// Display the pointer currently resides on (stable display id string).
    pub display_id: String,
    /// Whether the ghost cursor overlay should be drawn (agent visible).
    pub visible: bool,
    /// Current isolation/ownership mode.
    pub mode: PointerMode,
    /// Monotonic timestamp of the last virtual-pointer update.
    #[serde(skip, default = "now")]
    pub last_updated_at: Instant,
}

impl VirtualPointerState {
    pub fn new(x: f64, y: f64, display_id: impl Into<String>) -> Self {
        Self {
            x,
            y,
            display_id: display_id.into(),
            visible: true,
            mode: PointerMode::Isolated,
            last_updated_at: Instant::now(),
        }
    }

    pub fn location(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn set_location(&mut self, p: Point, display_id: impl Into<String>) {
        self.x = p.x;
        self.y = p.y;
        self.display_id = display_id.into();
        self.last_updated_at = Instant::now();
    }

    pub fn set_mode(&mut self, mode: PointerMode) {
        self.mode = mode;
        self.last_updated_at = Instant::now();
    }
}

impl Default for VirtualPointerState {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            display_id: "0".into(),
            visible: true,
            mode: PointerMode::Isolated,
            last_updated_at: Instant::now(),
        }
    }
}

/// Ownership/isolation mode of the virtual pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointerMode {
    /// Executing isolated actions (system cursor untouched, ghost cursor
    /// shown in its normal state).
    Isolated,
    /// The runtime is (or is about to be) temporarily borrowing the real
    /// system cursor. Ghost cursor switches to the fallback visual state.
    PhysicalFallback,
    /// The session is paused — the agent is not executing. Ghost cursor is
    /// dimmed or hidden.
    Paused,
    /// The user has taken control. Ghost cursor is hidden/red-flashed; only
    /// `release` returns the session to agent control.
    UserTakeover,
}

impl PointerMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PointerMode::Isolated => "isolated",
            PointerMode::PhysicalFallback => "physical_fallback",
            PointerMode::Paused => "paused",
            PointerMode::UserTakeover => "user_takeover",
        }
    }
}

/// Which actuator actually realized an action. Recorded in traces so the
/// provider of truth is the real backend, never a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointerExecutionBackend {
    /// A CoreGraphics mouse event posted at the target position **without**
    /// first warping the system cursor (DirectPositionEvent mode).
    DirectCGEvent,
    /// macOS Accessibility `AXPress` reached via an element hit-test at the
    /// model-chosen coordinate (actuation only — never UI grounding).
    Accessibility,
    /// The real system cursor was temporarily moved/used.
    Physical,
    /// No OS input was synthesized at all (e.g. a pure virtual move that only
    /// updated the ghost cursor).
    VirtualOnly,
}

impl PointerExecutionBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            PointerExecutionBackend::DirectCGEvent => "direct_cg_event",
            PointerExecutionBackend::Accessibility => "accessibility",
            PointerExecutionBackend::Physical => "physical",
            PointerExecutionBackend::VirtualOnly => "virtual_only",
        }
    }
}

/// Deterministic policy deciding when the runtime may borrow the real cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PointerPolicy {
    /// Never touch the real system cursor. Isolated actions that cannot be
    /// realized are rejected (`ISOLATED_POINTER_UNAVAILABLE` /
    /// `ISOLATED_DRAG_UNAVAILABLE`).
    #[default]
    IsolatedOnly,
    /// Prefer isolation; allow physical fallback **only when the caller
    /// explicitly permits it** for the action (e.g. `physical_fallback: true`
    /// on a drag). The default for Pi/OpenCode.
    IsolatedPreferred,
    /// Automatically fall back to the physical cursor when isolation is
    /// unavailable. Human input can always interrupt.
    PhysicalAllowed,
}

impl PointerPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            PointerPolicy::IsolatedOnly => "isolated_only",
            PointerPolicy::IsolatedPreferred => "isolated_preferred",
            PointerPolicy::PhysicalAllowed => "physical_allowed",
        }
    }

    pub fn physical_fallback_allowed(&self, caller_permitted: bool) -> bool {
        match self {
            PointerPolicy::IsolatedOnly => false,
            PointerPolicy::IsolatedPreferred => caller_permitted,
            PointerPolicy::PhysicalAllowed => true,
        }
    }
}

/// Bookkeeping for a physical fallback transaction: we briefly borrow the
/// real cursor, and we must restore it **only** if the user never touched it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PhysicalFallbackState {
    /// Original system cursor position, captured before the transaction.
    pub original_point: Point,
    /// Original display id of the system cursor.
    pub original_display_id: String,
    /// Whether the transaction may still restore the cursor. Set to false the
    /// moment any human input is observed during the transaction — after that
    /// the cursor belongs to the user and must never be pulled back.
    pub may_restore: bool,
    /// Closed when the transaction is done (restored, or abandoned on
    /// human input).
    pub completed: bool,
    /// Wall-clock timestamp when the transaction began.
    #[serde(skip, default = "now")]
    pub started_at: Instant,
}

impl PhysicalFallbackState {
    pub fn begin(point: Point, display_id: impl Into<String>) -> Self {
        Self {
            original_point: point,
            original_display_id: display_id.into(),
            may_restore: true,
            completed: false,
            started_at: Instant::now(),
        }
    }
}

/// Machine-readable eligibility verdict for an isolated drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DragBackend {
    DirectCGEvent,
    Accessibility,
    Physical,
    Unsupported,
}

impl DragBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            DragBackend::DirectCGEvent => "direct_cg_event",
            DragBackend::Accessibility => "accessibility",
            DragBackend::Physical => "physical",
            DragBackend::Unsupported => "unsupported",
        }
    }
}

/// The agent's virtual pointer is a per-session owned value; this is the
/// serializable snapshot the inspector/status exposes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PointerStatusView {
    pub virtual_pointer: VirtualPointerState,
    pub last_backend: Option<PointerExecutionBackend>,
    pub isolated: bool,
    pub physical_fallback_active: bool,
    pub physical_cursor_moved: bool,
    pub physical_cursor_restored: bool,
    pub human_input_detected: bool,
    pub last_human_interrupt_latency_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_pointer_location_round_trips() {
        let mut vp = VirtualPointerState::new(10.0, 20.0, "1");
        assert_eq!(vp.location(), Point::new(10.0, 20.0));
        vp.set_location(Point::new(-1920.0, -120.0), "2");
        assert_eq!(vp.x, -1920.0);
        assert_eq!(vp.display_id, "2");
        assert!(vp.visible);
    }

    #[test]
    fn pointer_mode_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(PointerMode::UserTakeover).unwrap(),
            "user_takeover"
        );
        assert_eq!(PointerMode::PhysicalFallback.as_str(), "physical_fallback");
    }

    #[test]
    fn policy_fallback_rules() {
        assert!(!PointerPolicy::IsolatedOnly.physical_fallback_allowed(true));
        assert!(PointerPolicy::IsolatedPreferred.physical_fallback_allowed(true));
        assert!(!PointerPolicy::IsolatedPreferred.physical_fallback_allowed(false));
        assert!(PointerPolicy::PhysicalAllowed.physical_fallback_allowed(false));
    }

    #[test]
    fn backend_serializes() {
        assert_eq!(
            serde_json::to_value(PointerExecutionBackend::Accessibility).unwrap(),
            "accessibility"
        );
        assert_eq!(
            PointerExecutionBackend::DirectCGEvent.as_str(),
            "direct_cg_event"
        );
    }

    #[test]
    fn fallback_state_defaults() {
        let s = PhysicalFallbackState::begin(Point::new(1.0, 2.0), "1");
        assert!(s.may_restore);
        assert!(!s.completed);
    }
}
