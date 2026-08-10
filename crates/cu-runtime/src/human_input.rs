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
//! The monitor also records the real interrupt-latency chain (P0-4). The
//! metric that matters is `human_to_input_stop_ms`:
//!
//! ```text
//! human_event_at ──► takeover_started_at ──► last_synthetic_event_at
//!        │                                       │
//!        │             human_to_takeover_ms      │
//!        └───────────────────────────────────────┘
//!             human_to_input_stop_ms = last_synthetic − human (saturating)
//! ```
//!
//! `human_to_input_stop_ms` answers "how long after the user's real input the
//! agent's LAST synthetic input landed": 0 when the agent had already stopped
//! before the user touched anything, positive when a synthetic event slipped
//! in AFTER the human event. It is NOT `synthetic → human` (the old
//! `human_interrupt_latency` mislabel); that inverse direction is exposed
//! separately as `agent_input_to_human_ms` for analysis only.

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
    /// P0-4: timestamp when the Event Tap callback observed the same human
    /// event (set by `on_human_event`). When the tap is live this is within
    /// microseconds of `human_event_at`; the difference is `event_detection`.
    event_callback_at: Mutex<Option<Instant>>,
    /// Timestamp when the takeover was actually applied (transition to
    /// UserTakeover + cancellation).
    takeover_started_at: Mutex<Option<Instant>>,
    /// Timestamp of the last runtime synthetic input event (updated on EVERY
    /// synthetic mouse/keyboard/scroll/physical input, including any that slip
    /// in AFTER a human event — that is exactly what `human_to_input_stop_ms`
    /// must measure).
    last_synthetic_event_at: Mutex<Option<Instant>>,
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
            event_callback_at: Mutex::new(None),
            takeover_started_at: Mutex::new(None),
            last_synthetic_event_at: Mutex::new(None),
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
        *self.event_callback_at.lock().unwrap() = None;
        *self.takeover_started_at.lock().unwrap() = None;
        *self.last_synthetic_event_at.lock().unwrap() = None;
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

    /// Record the last runtime synthetic input event (for the interrupt chain).
    /// Called on EVERY synthetic mouse move/down/up, drag step, scroll event,
    /// keyboard event, physical fallback warp and restore (P0-4). The
    /// timestamp is intentionally updated even when it lands AFTER a human
    /// event — that is the "accidental late synthetic" case
    /// `human_to_input_stop_ms` must measure, not paper over.
    pub fn record_synthetic_event(&self, instant: Instant) {
        *self.last_synthetic_event_at.lock().unwrap() = Some(instant);
        self.synthetic_count.fetch_add(1, Ordering::SeqCst);
    }

    /// **The** Human Interrupt KPI (P0-4): time from the real hardware human
    /// event to the LAST runtime synthetic input event, ms.
    ///
    /// - No synthetic event AFTER the human event (the agent had already
    ///   stopped — including the never-input case, where there is no last
    ///   synthetic to measure) → `0`, never a negative number. Section 四十七:
    ///   "no synthetic event after human → 0 latency".
    /// - A synthetic event that slipped in after the human event → its
    ///   positive distance from the human event, e.g. human at t100, stray
    ///   synthetic at t112 → `12`.
    /// - No human event at all → `None` (the metric is undefined without one).
    ///
    /// This is what the action result, the trace, and the benchmark report
    /// must carry as the interrupt latency — never the old
    /// `synthetic → human` direction.
    pub fn human_to_input_stop_ms(&self) -> Option<u64> {
        let last_synth = *self.last_synthetic_event_at.lock().unwrap();
        let human_at = *self.human_event_at.lock().unwrap();
        match (last_synth, human_at) {
            (Some(s), Some(h)) => {
                let delta = s.saturating_duration_since(h);
                Some(delta.as_millis().min(u64::MAX as u128) as u64)
            }
            // No synthetic ever -> the agent had no input to stop; the KPI is
            // a real 0 (P0-4: "0 when none"), not an undefined None.
            (None, Some(_)) => Some(0),
            _ => None,
        }
    }

    /// Analysis-only inverse metric (P0-4): time from the agent's last
    /// synthetic input to the human event — "how long after the agent's input
    /// did the user grab the machine". This is the OLD `human_interrupt_latency`
    /// meaning and must NOT be labelled as the interrupt KPI.
    pub fn agent_input_to_human_ms(&self) -> Option<u64> {
        let last_synth = *self.last_synthetic_event_at.lock().unwrap();
        let human_at = *self.human_event_at.lock().unwrap();
        match (last_synth, human_at) {
            (Some(s), Some(h)) => {
                let delta = h.saturating_duration_since(s);
                Some(delta.as_millis().min(u64::MAX as u128) as u64)
            }
            _ => None,
        }
    }

    /// Detection latency: hardware human event → Event Tap callback (ms).
    /// This is the `latency_ms` the tap measured from the event's own
    /// `CGEventGetTimestamp` to the callback (P0-4 `event_detection_latency`).
    pub fn event_detection_latency_ms(&self) -> Option<u64> {
        *self.last_latency_ms.lock().unwrap()
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

    /// Back-compat alias for the real KPI (kept so existing trace consumers
    /// do not break); the value IS `human_to_input_stop_ms`.
    pub fn event_to_input_stop_ms(&self) -> Option<u64> {
        self.human_to_input_stop_ms()
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
        // P0-3: `human_event_at` must be the HARDWARE event's timestamp in
        // monotonic space, NOT the callback time. The Event Tap measured
        // `latency_ms` from the event's own `CGEventGetTimestamp` to this
        // callback, so the event happened `latency_ms` before now. Using
        // `Instant::now()` here would understate `human_to_input_stop_ms` by
        // the entire detection latency.
        let event_at = now
            .checked_sub(Duration::from_millis(latency_ms))
            .unwrap_or(now);
        *self.last_human.lock().unwrap() = Some(now);
        *self.last_latency_ms.lock().unwrap() = Some(latency_ms);
        *self.human_event_at.lock().unwrap() = Some(event_at);
        // P0-4: the Event Tap callback time — the far end of
        // `event_detection_latency_ms` (event → callback).
        *self.event_callback_at.lock().unwrap() = Some(now);
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
        assert!(m.event_to_takeover_ms().is_some());
        // With no synthetic after the human event the real KPI is exactly 0.
        assert_eq!(m.event_to_input_stop_ms(), Some(0));
        assert_eq!(m.human_to_input_stop_ms(), Some(0));
    }

    /// P0-4 key test: human event with NO synthetic after it → `0` latency.
    /// Human at t100, last synthetic before it at t90 → input stop = 0 (the
    /// agent had already stopped); never a negative number.
    #[test]
    fn human_to_input_stop_is_zero_when_no_synthetic_after_human() {
        let m = HumanInputMonitor::new();
        let human = Instant::now();
        m.record_synthetic_event(human - Duration::from_millis(10));
        m.record_human_event(human);
        assert_eq!(
            m.human_to_input_stop_ms(),
            Some(0),
            "0 latency when the last synthetic preceded the human event"
        );
        // The inverse (analysis-only) metric is 10ms — but it is NOT the KPI.
        assert_eq!(m.agent_input_to_human_ms(), Some(10));
    }

    /// P0-4 key test: a synthetic event that slips in AFTER the human event is
    /// measured. Human at t100, stray synthetic at t112 → `12` ms.
    #[test]
    fn human_to_input_stop_is_positive_when_synthetic_slips_in_after_human() {
        let m = HumanInputMonitor::new();
        let human = Instant::now();
        m.record_human_event(human);
        m.record_synthetic_event(human + Duration::from_millis(12));
        assert_eq!(
            m.human_to_input_stop_ms(),
            Some(12),
            "a late synthetic after the human event must be measured, not hidden"
        );
        // A fresh monitor with no human event yet has no KPI.
        let m2 = HumanInputMonitor::new();
        assert_eq!(m2.human_to_input_stop_ms(), None);
    }

    /// P0-3: `on_human_event(latency_ms)` must place `human_event_at` at the
    /// HARDWARE event's timestamp (monotonic space, `now - latency_ms`), NOT at
    /// the Event Tap callback time. If it used the callback time, a synthetic
    /// stamped right after the callback would read ~0 ms of input-stop latency
    /// — understating the KPI by the whole detection latency.
    #[test]
    fn sink_event_at_is_hardware_time_not_callback_time() {
        let m = HumanInputMonitor::new();
        let sink: &dyn HumanInputSink = &m;
        // A real event measured 50 ms from its own CGEventGetTimestamp to this
        // callback. The event therefore happened 50 ms before the callback.
        sink.on_human_event(50);
        assert_eq!(m.event_detection_latency_ms(), Some(50));
        // A synthetic posted IMMEDIATELY after the callback. If `human_event_at`
        // were the callback time, input-stop would read ~0; with the true
        // hardware timestamp it must read ~50.
        m.record_synthetic_event(Instant::now());
        let stop = m.human_to_input_stop_ms().expect("the KPI must exist");
        assert!(
            stop >= 50,
            "human_to_input_stop_ms must include the detection latency: {stop}"
        );
        // Control: a zero-latency event (hardware == callback) reads ~0.
        let m2 = HumanInputMonitor::new();
        let sink2: &dyn HumanInputSink = &m2;
        sink2.on_human_event(0);
        m2.record_synthetic_event(Instant::now());
        let stop0 = m2.human_to_input_stop_ms().expect("the KPI must exist");
        assert!(
            stop0 < 50,
            "a zero-latency event must not invent latency: {stop0}"
        );
    }
}
