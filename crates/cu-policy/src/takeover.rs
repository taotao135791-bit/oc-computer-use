//! User-takeover policy: what to do when the physical user starts moving the
//! mouse while the runtime is driving it. The first version detects large
//! pointer jumps (far larger than any programmatic move) and applies a
//! configurable reaction. It must never recurse infinitely: reactions pause or
//! take over the session — they do not emit new pointer actions.

use serde::{Deserialize, Serialize};

/// Reaction when the physical user takes over the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverPolicy {
    /// Do nothing.
    Ignore,
    /// Pause the session (no new actions; can be resumed).
    AutoPause,
    /// Immediately hand control back to the user.
    ImmediateTakeover,
}

impl Default for TakeoverPolicy {
    fn default() -> Self {
        TakeoverPolicy::AutoPause
    }
}

/// Parameters for the pointer-motion takeover heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TakeoverDetector {
    /// Pointer moves larger than this many logical points within one poll are
    /// treated as human.
    pub jump_threshold_points: f64,
    /// Only react after this many consecutive human-sized jumps.
    pub required_jumps: u32,
    pub policy: TakeoverPolicy,
    /// Running streak of human-sized jumps (not serialized).
    #[serde(skip)]
    pub consecutive_jumps: u32,
}

impl Default for TakeoverDetector {
    fn default() -> Self {
        Self {
            jump_threshold_points: 60.0,
            required_jumps: 2,
            policy: TakeoverPolicy::AutoPause,
            consecutive_jumps: 0,
        }
    }
}

impl TakeoverDetector {
    /// Feed one pointer sample delta. Returns true when the configured policy
    /// should trigger.
    pub fn observe(&mut self, dx: f64, dy: f64) -> bool {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist > self.jump_threshold_points {
            self.consecutive_jumps += 1;
        } else {
            self.consecutive_jumps = 0;
        }
        self.consecutive_jumps >= self.required_jumps
    }

    pub fn reset(&mut self) {
        self.consecutive_jumps = 0;
    }
}

/// Query the current jump streak (used by tests and the inspector).
impl TakeoverDetector {
    pub fn jump_count(&self) -> u32 {
        self.consecutive_jumps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_small_move_does_not_trigger() {
        let mut d = TakeoverDetector::default();
        assert!(!d.observe(5.0, 5.0));
        assert!(!d.observe(10.0, 10.0));
        assert_eq!(d.jump_count(), 0);
    }

    #[test]
    fn repeated_large_jumps_trigger() {
        let mut d = TakeoverDetector { required_jumps: 2, ..Default::default() };
        assert!(!d.observe(200.0, 0.0));
        assert!(d.observe(0.0, 300.0));
    }

    #[test]
    fn reset_clears_state() {
        let mut d = TakeoverDetector::default();
        d.observe(500.0, 0.0);
        d.reset();
        assert_eq!(d.jump_count(), 0);
    }

    #[test]
    fn human_move_resets_counter() {
        let mut d = TakeoverDetector::default();
        d.observe(500.0, 0.0);
        d.observe(2.0, 1.0);
        assert_eq!(d.jump_count(), 0);
    }
}
