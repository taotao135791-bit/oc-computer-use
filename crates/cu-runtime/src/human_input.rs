//! Continuous human-input monitoring and the **Human Always Wins** contract.
//!
//! Two independent detection channels (round 9 / P0-1, P0-8):
//!
//! 1. **Real hardware events** (Event Tap): physical mouse move / down / up /
//!    scroll and keys. These are *always* a UserTakeover — the configurable
//!    `TakeoverPolicy` (Ignore / AutoPause / ImmediateTakeover) never applies
//!    to them. `force_user_takeover` semantics: current action is cancelled,
//!    the queue drains, the session enters `UserTakeover`, only `release`
//!    recovers it.
//! 2. **Heuristic pointer-delta detection** (fallback): only used when the
//!    Event Tap is degraded/unavailable. This channel MAY use the old
//!    configurable policy.
//!
//! The monitor also records the real interrupt-latency chain:
//! `human_event_at` → `takeover_started_at` → `last_synthetic_event_at`, and
//! exposes `event_to_input_stop_ms` (the metric that matters: real hardware
//! event → last runtime synthetic event).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A sink that receives real human input events. Implemented by the
/// platform-neutral [`HumanInputMonitor`]; the macOS Event Tap driver holds an
/// `Arc<dyn HumanInputSink>` and calls [`on_human_event`] for every real
/// (non-synthetic) user event.
pub trait HumanInputSink: Send + Sync {
    /// A real human input event was observed. `latency_ms` is the measured
    /// time from the event timestamp to the moment the runtime noticed.
    fn on_human_event(&self, latency_ms: u64);
}

/// Continuous human-input detector shared by the runtime and the macOS driver.
///
/// - `on_human_event` is called by the platform Event Tap as soon as a real
///   event is seen. It sets the **real-takeover** flag, which the action queue
///   consumes as an unconditional UserTakeover.
/// - `consume_real_takeover` atomically clears the real-takeover flag and
///   reports whether the batch must enter UserTakeover **now**.
/// - `consume_takeover` (legacy channel, P0-8) handles the heuristic fallback
///   path and may still honor the old configurable policy.
pub struct HumanInputMonitor {
    last_human: Mutex<Option<Instant>>,
    /// Real hardware event pending → unconditional UserTakeover.
    pending_real_takeover: AtomicBool,
    /// Heuristic (pointer-delta) takeover pending → policy-governed.
    pending_takeover: AtomicBool,
    last_latency_ms: Mutex<Option<u64>>,
    /// Timestamp (monotonic) of the last real human event.
    human_event_at: Mutex<Option<Instant>>,
    /// Timestamp when the takeover was actually applied (transition to
    /// UserTakeover + cancellation).
    takeover_started_at: Mutex<Option<Instant>>,
    /// Timestamp of the last runtime synthetic input event.
    last_synthetic_event_at: Mutex<Option<Instant>>,
    /// Computed when the action loop observes the input stop.
    event_to_input_stop_ms: AtomicU64,
    synthetic_count: std::sync::atomic::AtomicU64,
    /// P0-2: monotonic generation counter, incremented on every REAL human
    /// event. A transaction can snapshot it before borrowing the cursor and
    /// compare afterwards to learn "did the user touch anything mid-flight?"
    human_event_generation: AtomicU64,
    /// P0-1: registered by the runtime. Invoked synchronously (on the Event Tap
    /// thread) the moment a REAL human event is observed, so the active batch
    /// is cancelled **at event time** — not merely flagged for the next loop
    /// iteration. The runtime's hook finds the control-holder session and
    /// cancels its in-flight batches; the queue thread still performs the
    /// `UserTakeover` state transition (it needs async driver calls).
    real_takeover_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for HumanInputMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl HumanInputMonitor {
    pub fn new() -> Self {
        Self {
            last_human: Mutex::new(None),
            pending_real_takeover: AtomicBool::new(false),
            pending_takeover: AtomicBool::new(false),
            last_latency_ms: Mutex::new(None),
            human_event_at: Mutex::new(None),
            takeover_started_at: Mutex::new(None),
            last_synthetic_event_at: Mutex::new(None),
            event_to_input_stop_ms: AtomicU64::new(u64::MAX),
            synthetic_count: std::sync::atomic::AtomicU64::new(0),
            human_event_generation: AtomicU64::new(0),
            real_takeover_hook: Mutex::new(None),
        }
    }

    /// Register the P0-1 real-takeover hook. The runtime installs a hook that
    /// cancels the active session's in-flight batches the instant a real human
    /// event fires, so a long-running action (drag / scroll / wait) aborts
    /// immediately instead of waiting for the loop's next poll.
    pub fn set_real_takeover_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.real_takeover_hook.lock().unwrap() = Some(hook);
    }

    /// Remove the P0-1 real-takeover hook (runtime shutdown / test teardown).
    pub fn clear_real_takeover_hook(&self) {
        *self.real_takeover_hook.lock().unwrap() = None;
    }

    /// Invoke the registered hook, if any. The Arc is cloned out of the lock
    /// before the call so a hook that re-enters the monitor cannot deadlock.
    fn fire_real_takeover_hook(&self) {
        let hook = self.real_takeover_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// True when a **real** takeover is pending (a real user event arrived
    /// since the last consume).
    pub fn real_takeover_requested(&self) -> bool {
        self.pending_real_takeover.load(Ordering::SeqCst)
    }

    /// True when a heuristic takeover is pending (event-tap degraded path).
    pub fn takeover_requested(&self) -> bool {
        self.pending_takeover.load(Ordering::SeqCst)
    }

    /// Atomically consume the **real** takeover flag. When true, the batch
    /// must enter UserTakeover immediately — this path is never governed by
    /// the configurable Ignore/AutoPause policy.
    pub fn consume_real_takeover(&self) -> bool {
        self.pending_real_takeover.swap(false, Ordering::SeqCst)
    }

    /// Atomically consume the heuristic takeover flag (policy-governed).
    pub fn consume_takeover(&self) -> bool {
        self.pending_takeover.swap(false, Ordering::SeqCst)
    }

    /// P0-2: current human-event generation. Snapshot before borrowing the
    /// cursor and compare after to detect human input mid-transaction.
    pub fn human_event_generation(&self) -> u64 {
        self.human_event_generation.load(Ordering::SeqCst)
    }

    /// Clear all state (called when a session starts or releases control).
    pub fn reset(&self) {
        self.pending_real_takeover.store(false, Ordering::SeqCst);
        self.pending_takeover.store(false, Ordering::SeqCst);
        *self.last_human.lock().unwrap() = None;
        *self.last_latency_ms.lock().unwrap() = None;
        *self.human_event_at.lock().unwrap() = None;
        *self.takeover_started_at.lock().unwrap() = None;
        *self.last_synthetic_event_at.lock().unwrap() = None;
        self.event_to_input_stop_ms
            .store(u64::MAX, Ordering::SeqCst);
        self.synthetic_count.store(0, Ordering::SeqCst);
        self.human_event_generation.store(0, Ordering::SeqCst);
    }

    /// Record a **real** human event. `event_instant` should be the event's own
    /// timestamp so the latency is measured from the input, not the poll.
    /// Always raises the real-takeover flag (P0-1): Ignore/AutoPause can never
    /// swallow a physical user input.
    pub fn record_human_event(&self, event_instant: Instant) {
        let latency_ms = event_instant.elapsed().as_millis().min(u64::MAX as u128) as u64;
        *self.last_human.lock().unwrap() = Some(Instant::now());
        *self.last_latency_ms.lock().unwrap() = Some(latency_ms);
        *self.human_event_at.lock().unwrap() = Some(event_instant);
        self.pending_real_takeover.store(true, Ordering::SeqCst);
        self.human_event_generation.fetch_add(1, Ordering::SeqCst);
        // P0-1: cancel the ACTIVE batch NOW. A long in-flight action (drag /
        // scroll / wait via execute_with_cancel) must abort at event time —
        // not merely be flagged for the next loop iteration.
        self.fire_real_takeover_hook();
    }

    /// Record a **heuristic** detection (pointer jump while the Event Tap is
    /// degraded). Policy-governed; never the real-takeover flag.
    pub fn record_heuristic_detection(&self) {
        self.pending_takeover.store(true, Ordering::SeqCst);
    }

    /// Called by the runtime when the takeover is actually applied (session
    /// transitioned to UserTakeover, queue cancelled).
    pub fn mark_takeover_started(&self) {
        *self.takeover_started_at.lock().unwrap() = Some(Instant::now());
    }

    /// Called by the action loop once it observes the last synthetic input
    /// event stopped (i.e. no further runtime input will be posted).
    pub fn mark_input_stopped(&self) {
        if let Some(started) = *self.takeover_started_at.lock().unwrap() {
            let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            self.event_to_input_stop_ms.store(ms, Ordering::SeqCst);
        }
    }

    /// Record the last runtime synthetic input event (for the interrupt chain).
    pub fn record_synthetic_event(&self, instant: Instant) {
        *self.last_synthetic_event_at.lock().unwrap() = Some(instant);
        self.synthetic_count.fetch_add(1, Ordering::SeqCst);
    }

    /// The real metric: human event → last runtime synthetic input event, ms.
    pub fn event_to_input_stop_ms(&self) -> Option<u64> {
        let v = self.event_to_input_stop_ms.load(Ordering::SeqCst);
        if v == u64::MAX {
            None // sentinel: never measured
        } else {
            Some(v)
        }
    }

    /// Human event → takeover-applied latency, ms.
    pub fn event_to_takeover_ms(&self) -> Option<u64> {
        let event_at = *self.human_event_at.lock().unwrap();
        let started_at = *self.takeover_started_at.lock().unwrap();
        match (event_at, started_at) {
            (Some(e), Some(s)) => {
                // saturating: event and takeover may share the same tick.
                let delta = s.saturating_duration_since(e);
                let ms = delta.as_millis().min(u64::MAX as u128) as u64;
                Some(ms)
            }
            _ => None,
        }
    }

    /// Human interrupt latency (audit G): time from the LAST runtime synthetic
    /// input event to the most recent real human event, ms. This is how long
    /// after the agent's input the user grabbed the machine — the number the
    /// action result + trace report as `human_interrupt_latency_ms`. `None`
    /// when no synthetic event precedes the human event (a purely
    /// human-initiated interrupt, or the monitor has seen no synthetic input).
    pub fn human_interrupt_latency_ms(&self) -> Option<u64> {
        let last_synth = *self.last_synthetic_event_at.lock().unwrap();
        let human_at = *self.human_event_at.lock().unwrap();
        match (last_synth, human_at) {
            (Some(s), Some(h)) if h >= s => {
                let delta = h.saturating_duration_since(s);
                Some(delta.as_millis().min(u64::MAX as u128) as u64)
            }
            _ => None,
        }
    }

    /// Last measured human-interrupt latency in ms, if any.
    pub fn last_latency_ms(&self) -> Option<u64> {
        *self.last_latency_ms.lock().unwrap()
    }

    /// Number of synthetic events seen (diagnostics).
    pub fn synthetic_count(&self) -> u64 {
        self.synthetic_count.load(Ordering::SeqCst)
    }

    /// Seconds since the last human event (or `None` if none observed yet).
    pub fn idle_secs(&self) -> Option<u64> {
        self.last_human
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs())
    }
}

impl HumanInputSink for HumanInputMonitor {
    fn on_human_event(&self, latency_ms: u64) {
        let now = Instant::now();
        *self.last_human.lock().unwrap() = Some(now);
        *self.last_latency_ms.lock().unwrap() = Some(latency_ms);
        *self.human_event_at.lock().unwrap() = Some(now);
        // P0-1: a real hardware event ALWAYS raises the real-takeover flag.
        self.pending_real_takeover.store(true, Ordering::SeqCst);
        self.human_event_generation.fetch_add(1, Ordering::SeqCst);
        // P0-1: cancel the ACTIVE batch at event time (see
        // `record_human_event`). This is the sink path the Event Tap drives.
        self.fire_real_takeover_hook();
    }
}

/// Convenience time window used to decide whether a pointer jump is human:
/// anything arriving within this window of a synthetic post is treated as
/// part of our own motion (conservative, prevents self-triggering).
pub const SYNTHETIC_WINDOW: Duration = Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_event_forces_takeover_flag() {
        let m = HumanInputMonitor::new();
        assert!(!m.real_takeover_requested());
        m.record_human_event(Instant::now());
        assert!(m.real_takeover_requested());
        assert!(m.consume_real_takeover());
        assert!(!m.real_takeover_requested());
    }

    #[test]
    fn ignore_policy_cannot_clear_real_takeover() {
        // P0-1 regression: the old policy channel (pending_takeover) is
        // entirely separate from the real-event channel.
        let m = HumanInputMonitor::new();
        m.record_human_event(Instant::now());
        // Simulating "Ignore" would have consumed the heuristic flag; the
        // real flag must remain set.
        let _ = m.consume_takeover();
        assert!(
            m.real_takeover_requested(),
            "heuristic consume must not affect the real-event flag"
        );
        m.consume_real_takeover();
        assert!(!m.real_takeover_requested());
    }

    #[test]
    fn heuristic_detection_uses_separate_channel() {
        let m = HumanInputMonitor::new();
        m.record_heuristic_detection();
        assert!(m.takeover_requested());
        assert!(!m.real_takeover_requested(), "heuristic != real event");
        assert!(m.consume_takeover());
        assert!(!m.consume_real_takeover());
    }

    #[test]
    fn synthetic_events_never_raise_takeover() {
        let m = HumanInputMonitor::new();
        m.record_synthetic_event(Instant::now());
        m.record_synthetic_event(Instant::now());
        assert!(!m.takeover_requested());
        assert!(!m.real_takeover_requested());
        assert_eq!(m.synthetic_count(), 2);
        assert!(m.last_latency_ms().is_none());
    }

    #[test]
    fn latency_is_recorded() {
        let m = HumanInputMonitor::new();
        m.record_human_event(Instant::now() - Duration::from_millis(5));
        let lat = m.last_latency_ms().unwrap();
        assert!((5..1000).contains(&lat), "unexpected latency {lat}");
    }

    #[test]
    fn reset_clears_pending() {
        let m = HumanInputMonitor::new();
        m.record_human_event(Instant::now());
        assert!(m.real_takeover_requested());
        m.reset();
        assert!(!m.real_takeover_requested());
        assert!(!m.takeover_requested());
        assert!(m.last_latency_ms().is_none());
    }

    #[test]
    fn sink_trait_contract_forces_real_takeover() {
        let m = HumanInputMonitor::new();
        let sink: &dyn HumanInputSink = &m;
        sink.on_human_event(3);
        assert!(m.real_takeover_requested());
        assert_eq!(m.last_latency_ms(), Some(3));
    }

    #[test]
    fn real_event_fires_the_takeover_hook_immediately() {
        // P0-1: the hook (which cancels the active batch) must fire the MOMENT
        // a real event arrives — not on the next loop poll. Both the record
        // path and the Event Tap sink path must invoke it.
        use std::sync::atomic::AtomicUsize;
        let m = HumanInputMonitor::new();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        m.set_real_takeover_hook(std::sync::Arc::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        }));
        m.record_human_event(Instant::now());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "record path must fire the hook"
        );
        let sink: &dyn HumanInputSink = &m;
        sink.on_human_event(3);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "Event Tap sink path must fire the hook too"
        );
        // reset() must not clear the persistent registration.
        m.reset();
        m.record_human_event(Instant::now());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cleared_hook_does_not_fire() {
        use std::sync::atomic::AtomicUsize;
        let m = HumanInputMonitor::new();
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        m.set_real_takeover_hook(std::sync::Arc::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        }));
        m.clear_real_takeover_hook();
        m.record_human_event(Instant::now());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "cleared hook must not fire"
        );
    }

    #[test]
    fn interrupt_chain_metrics() {
        let m = HumanInputMonitor::new();
        m.record_human_event(Instant::now() - Duration::from_millis(10));
        m.mark_takeover_started();
        m.mark_input_stopped();
        assert!(m.event_to_takeover_ms().is_some());
        assert!(m.event_to_input_stop_ms().is_some());
    }
}
