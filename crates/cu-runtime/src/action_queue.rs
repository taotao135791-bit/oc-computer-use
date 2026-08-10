//! The action queue: executes a batch of actions strictly one-at-a-time with
//! cancellation, per-action reporting, trace recording, and human-takeover
//! detection between steps.
//!
//! The queue never does coordinate math — every [`cu_driver::ResolvedAction`]
//! it builds has already been converted to global logical points via
//! [`cu_core::ImageGeometry`]. Resolution happens *here*, at execution time, so
//! a `Move` can start from the live pointer position (which is unknown until the
//! previous action has run).

use std::time::{Duration, Instant};

use cu_core::{
    ComputerAction, CuError, ImageGeometry, MouseButton, Point, PointerMode, PointerPolicy,
};
use cu_driver::ComputerDriver;
use cu_policy::{TakeoverDetector, TakeoverPolicy};
use cu_trace::TraceRecorder;
use tokio_util::sync::CancellationToken;

use crate::human_input::HumanInputMonitor;
use crate::sessions::{Session, SharedSession};

/// P0-2: a physical fallback refuses to borrow the real cursor while the user
/// has interacted within this many seconds — the user is actively operating.
pub const PHYSICAL_FALLBACK_IDLE_GUARD_SECS: u64 = 2;

/// Outcome of one action inside a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRun {
    pub index: usize,
    pub status: String, // success | failed | cancelled
    pub duration_ms: u64,
    pub error: Option<String>,
    /// Round 9 / P0-9: which pointer backend actually realized the action.
    /// Retained in the wire result and the trace for real verification.
    pub pointer: Option<cu_core::PointerExecutionResult>,
}

impl ActionRun {
    pub fn success(index: usize, duration_ms: u64) -> Self {
        Self {
            index,
            status: "success".into(),
            duration_ms,
            error: None,
            pointer: None,
        }
    }
    pub fn failed(index: usize, duration_ms: u64, error: String) -> Self {
        Self {
            index,
            status: "failed".into(),
            duration_ms,
            error: Some(error),
            pointer: None,
        }
    }
    pub fn cancelled(index: usize) -> Self {
        Self {
            index,
            status: "cancelled".into(),
            duration_ms: 0,
            error: None,
            pointer: None,
        }
    }
    /// Attach the pointer-execution detail to a successful run.
    pub fn with_pointer(mut self, pointer: cu_core::PointerExecutionResult) -> Self {
        self.pointer = Some(pointer);
        self
    }
}

/// Round 8 / P0-1: what the `window_at_point` hit-test found at a global point
/// in a target-scoped session. Direct CG is ONLY permitted for
/// [`Clear`](OcclusionVerdict::Clear) (and non-target sessions, where there is
/// nothing to be occluded against).
#[derive(Debug, Clone, PartialEq)]
enum OcclusionVerdict {
    /// No session target → Direct CG is permitted (nothing to occlude it).
    NotTargetScoped,
    /// The topmost window at the point IS the session target (window_id + pid,
    /// bundle auxiliary) → Direct CG is permitted.
    Clear,
    /// The topmost window at the point is a DIFFERENT window → the target is
    /// occluded → Direct CG is forbidden.
    Blocked(cu_driver::WindowAtPoint),
    /// No normal window contains the point → Direct CG is forbidden (fail
    /// closed: a click into the void is never sent).
    NoWindow,
    /// The hit-test could not be answered (driver error) → Direct CG is
    /// forbidden (fail closed).
    Unverifiable,
}

impl OcclusionVerdict {
    /// Short stable label recorded in action detail so the trace shows WHY the
    /// Direct CG path was bypassed.
    fn label(&self) -> &'static str {
        match self {
            OcclusionVerdict::NotTargetScoped => "not_target_scoped",
            OcclusionVerdict::Clear => "clear",
            OcclusionVerdict::Blocked(_) => "blocked",
            OcclusionVerdict::NoWindow => "no_window_at_point",
            OcclusionVerdict::Unverifiable => "hit_test_unavailable",
        }
    }
}

/// Executes one batch against the driver. A fresh instance is created per batch
/// so the takeover detector never carries state across calls.
pub struct ActionQueue<'a> {
    driver: &'a dyn ComputerDriver,
}

impl<'a> ActionQueue<'a> {
    pub fn new(driver: &'a dyn ComputerDriver) -> Self {
        Self { driver }
    }

    /// Execute `actions` in order. Stops early (marking the remainder
    /// `cancelled`) when the session is paused/taken over/stopped, when the
    /// cancellation token fires, or when the human grabs the mouse.
    #[allow(clippy::too_many_arguments)] // action boundary: every arg is a distinct execution context
    pub async fn run(
        &self,
        session: &SharedSession,
        actions: &[ComputerAction],
        geometry: &ImageGeometry,
        token: CancellationToken,
        takeover: &mut TakeoverDetector,
        human: Option<&HumanInputMonitor>,
        trace: Option<&TraceRecorder>,
        request_id: Option<&str>,
        frame_id: &str,
        display_id: &str,
        active_app: Option<&str>,
        // Round 9 / P0-6: `active_bundle_id` was the old batch-level cache.
        // The Focus Guard now re-reads the frontmost app LIVE before every
        // keyboard action, so this parameter is intentionally removed.
        _active_bundle_id_removed_p06: (),
        // Round 9 / P0-8: "active" means the hardware Event Tap is the
        // authoritative human-input detector and the pointer-delta heuristic
        // is OFF. Anything else (degraded/unavailable) allows the fallback.
        human_monitor_state: Option<&str>,
    ) -> Result<Vec<ActionRun>, CuError> {
        let mut reports = Vec::with_capacity(actions.len());
        // Round 8 / Phase 2-3: the agent's pointer is the SESSION VIRTUAL
        // POINTER, never the physical system cursor. Every agent pointer
        // action starts from this logical position; the real cursor belongs
        // to the human and is only consulted for the takeover fallback.
        let mut last_pointer = session.virtual_pointer.lock().unwrap().location();
        takeover.reset();

        for (i, action) in actions.iter().enumerate() {
            // Human Always Wins (P0-1): a REAL hardware event (Event Tap)
            // always forces UserTakeover — the configurable Ignore/AutoPause
            // policy never applies to physical user input. The runtime hook
            // already cancelled the batch token AT EVENT TIME (so an in-flight
            // action aborted); this check must run BEFORE the token check so
            // the flag is still consumed here and the transition + ghost-cursor
            // hide + interrupt metrics complete even though the token fired.
            // The heuristic channel (fallback, P0-8) may still honor the old
            // policy; it is checked separately below.
            if let Some(h) = human {
                if h.consume_real_takeover() {
                    // P0-1: real hardware input -> unconditional UserTakeover.
                    let _ = session.transition(cu_core::SessionState::UserTakeover);
                    session.sync_pointer_mode(cu_core::SessionState::UserTakeover);
                    h.mark_takeover_started();
                    // P0-4: `human_to_input_stop_ms` is derived from
                    // `last_synthetic_event_at`, never force-marked here.
                    // P0-1: ghost cursor must be hidden immediately — the user
                    // is back in control.
                    let _ = self.driver.pointer_hidden().await;
                    self.fill_cancelled(&mut reports, i, actions.len());
                    break;
                }
                // Fallback heuristic channel: only when the Event Tap is NOT
                // authoritative (degraded / unavailable).
                if human_monitor_state != Some("active") && h.consume_takeover() {
                    // Old configurable policy applies to the heuristic.
                    let _ = self.apply_takeover(session, takeover);
                    self.fill_cancelled(&mut reports, i, actions.len());
                    break;
                }
            }
            if token.is_cancelled() || self.session_aborted(session) {
                self.fill_cancelled(&mut reports, i, actions.len());
                break;
            }

            // Round 8 / Phase 9 — PointerPolicy gates physical-required
            // actions. Drag and located Scroll move the REAL cursor:
            //   - `isolated_only`: refused outright — the runtime NEVER moves
            //     the user's cursor under this policy.
            //   - `isolated_preferred` (P0-3): refused with
            //     PHYSICAL_FALLBACK_REQUIRED whenever the only available
            //     backend is physical-only. Today the runtime exposes no
            //     isolated Drag / located-Scroll backend — the macOS driver's
            //     Drag and Scroll always borrow the real cursor via
            //     mouse::drag / mouse::scroll — so every Drag / located
            //     Scroll is physical-only and must NOT be silently executed.
            //   - `physical_allowed`: physical execution is permitted (and
            //     interruptible; see the Click dispatcher's Backend 3).
            let policy = session.get_pointer_policy();
            let needs_physical = matches!(
                action,
                ComputerAction::Drag { .. }
                    | ComputerAction::Scroll {
                        x: Some(_),
                        y: Some(_),
                        ..
                    }
            );
            if needs_physical && !matches!(policy, PointerPolicy::PhysicalAllowed) {
                let error = match policy {
                    PointerPolicy::IsolatedOnly
                        if matches!(action, ComputerAction::Drag { .. }) =>
                    {
                        "ISOLATED_DRAG_UNAVAILABLE"
                    }
                    PointerPolicy::IsolatedOnly => "PHYSICAL_FALLBACK_NOT_ALLOWED",
                    PointerPolicy::IsolatedPreferred => "PHYSICAL_FALLBACK_REQUIRED",
                    PointerPolicy::PhysicalAllowed => unreachable!(),
                };
                reports.push(ActionRun::failed(i, 0, error.into()));
                self.fill_cancelled(&mut reports, i + 1, actions.len());
                break;
            }

            // Round 9 / P0-5 — Keyboard Focus Guard, LIVE per action.
            //
            // The batch-level `active_bundle_id` is stale by design: a user can
            // switch apps mid-batch (Click → Wait 2s → user switches to
            // TextEdit → Type). So before EVERY keyboard event (Type / Key /
            // Clipboard-paste) we re-read the frontmost application NOW and
            // compare STRICTLY against the session target — bundle AND pid AND
            // (when both windows are known) window id. Focus means the target's
            // OWN window is in front; a bundle match on a recycled pid (app
            // relaunched into a new window) or a different window of the same
            // app does NOT count. On mismatch INPUT_FOCUS_MISMATCH and NO
            // keyboard event is ever sent. We never auto-activate or steal
            // focus (focus_policy: strict is the default).
            let needs_focus = matches!(
                action,
                ComputerAction::TypeText { .. } | ComputerAction::Key { .. }
            );
            if needs_focus {
                if let Some(t) = session.get_target() {
                    // The guard needs SOME identity to compare; with the P0-4
                    // full-identity backfill a resolved target always has one.
                    let has_identity =
                        t.bundle_id.is_some() || t.pid.is_some() || t.window_id.is_some();
                    if has_identity {
                        let live = self.driver.active_application().await.ok().flatten();
                        let bundle_matches = match (&t.bundle_id, live.as_ref()) {
                            (Some(tb), Some(a)) => a.bundle_id == *tb,
                            (Some(_), None) => false, // constraint but no live info
                            (None, _) => true,        // no bundle constraint
                        };
                        let pid_matches = match (t.pid, live.as_ref().and_then(|a| a.pid)) {
                            (Some(tp), Some(lp)) => lp == tp as i32,
                            (Some(_), None) => false, // constraint but no live pid
                            (None, _) => true,
                        };
                        let window_matches =
                            match (t.window_id, live.as_ref().and_then(|a| a.window_id)) {
                                (Some(tw), Some(lw)) => lw == tw as u32,
                                (Some(_), None) => false, // constraint but no live window
                                (None, _) => true,
                            };
                        let focused = bundle_matches && pid_matches && window_matches;
                        if !focused {
                            reports.push(ActionRun::failed(i, 0, "INPUT_FOCUS_MISMATCH".into()));
                            self.fill_cancelled(&mut reports, i + 1, actions.len());
                            break;
                        }
                    }
                }
            }

            let resolved = match self.to_resolved(action, geometry, last_pointer) {
                Ok(r) => r,
                Err(e) => {
                    reports.push(ActionRun::failed(i, 0, e.to_string()));
                    self.fill_cancelled(&mut reports, i + 1, actions.len());
                    break;
                }
            };

            // Target Isolation (round 8): if the session is scoped to a target
            // window, every location-bearing action's coordinate must land
            // inside that window's bounds. Outside -> TARGET_OUTSIDE_SESSION.
            //
            // Locationless scroll in a target session -> TARGET_COORDINATE_REQUIRED:
            // the scroll would land at the CURRENT pointer, which may be in a
            // different app. P0-7: the gate is the RESOLVED target itself, not
            // its currently-known bounds — a scoped session refuses a
            // locationless scroll even when the window's bounds are unknown or
            // stale (off-screen / refresh probe failed), not only when they
            // happen to be Some.
            if session.get_resolved_target().is_some()
                && matches!(
                    action,
                    ComputerAction::Scroll {
                        x: None,
                        y: None,
                        ..
                    }
                )
            {
                reports.push(ActionRun::failed(i, 0, "TARGET_COORDINATE_REQUIRED".into()));
                self.fill_cancelled(&mut reports, i + 1, actions.len());
                break;
            }
            // P0-7: containment is enforced on EVERY global point the action
            // will touch. A Drag spans two points — the real cursor travels
            // from `from` to `to` — so BOTH must be inside the window; a drag
            // that begins outside would sweep the cursor across other apps
            // before entering the target.
            if let Some(bounds) = session.get_target_bounds() {
                if !resolved_within_bounds(&resolved, &bounds) {
                    reports.push(ActionRun::failed(i, 0, "TARGET_OUTSIDE_SESSION".into()));
                    self.fill_cancelled(&mut reports, i + 1, actions.len());
                    break;
                }
            }

            let started = Instant::now();
            let mut wait_interrupted = false;
            let outcome = match &resolved {
                // Round 9 / P0-7 — UNIQUE Click Dispatcher.
                //
                // Clicks no longer go straight to `driver.execute()`: the
                // backend is selected here by the session's PointerPolicy.
                //   1. Direct CGEvent isolated click (never warps the cursor).
                //   2. If that fails and a target pid is known: AXPress.
                //   3. Physical fallback is ONLY allowed under
                //      `physical_allowed` (never silently — see below).
                //
                // The chosen backend + isolation + cursor delta are recorded
                // into the ActionResult detail, retained by P0-9 through the
                // ActionRun into the wire result and the trace.
                cu_driver::ResolvedAction::Click {
                    x,
                    y,
                    button,
                    double,
                } => {
                    let policy = session.get_pointer_policy();
                    // P0-3: snapshot the human-event generation at the top of
                    // the click so every pre-emit check below (occlusion
                    // hit-test, Direct CG down/up, AXPress) can detect a grab
                    // that happened DURING this action's setup — not just one
                    // taken at the last instant.
                    let generation_before = human.map(|h| h.human_event_generation());
                    let resolved_target = session.get_resolved_target();
                    let target_pid = resolved_target
                        .as_ref()
                        .map(|r| r.pid)
                        .or_else(|| session.get_target().and_then(|t| t.pid).map(|p| p as i32));

                    // Round 8 / P0-1 — occlusion guard. In a target-scoped
                    // session, a Direct CG click at (x,y) is ONLY permitted
                    // when the topmost window at that point IS the session
                    // target. A point inside the target's bounds but covered
                    // by another window must never receive a Direct CG click —
                    // it would land on the covering window. Non-target
                    // sessions are never occluded (there is nothing to compare
                    // against). Fail closed whenever the hit-test cannot
                    // confirm the target is topmost.
                    let occlusion = self.occlusion_verdict(session, *x, *y).await;
                    let occluded = !matches!(
                        occlusion,
                        OcclusionVerdict::NotTargetScoped | OcclusionVerdict::Clear
                    );

                    if occluded {
                        // Direct CG forbidden → fail closed with
                        // TARGET_OCCLUDED. Only a CONFIRMED different topmost
                        // window (`Blocked`) may retry via the isolated AX
                        // fallback: a target-PID-scoped AXPress can realize a
                        // SINGLE click without moving the cursor. `NoWindow` and
                        // `Unverifiable` fail closed immediately — when the
                        // target's topmost-ness is NOT verifiable, no input is
                        // emitted at all.
                        //   - a double-click NEVER degrades to a single AXPress
                        //     (AX cannot express a double-press, P1);
                        //   - the PHYSICAL fallback NEVER runs under occlusion,
                        //     even under `physical_allowed` (section 九) —
                        //     warping the real cursor to click through the
                        //     covering window is exactly the cross-app action
                        //     this guard exists to forbid.
                        let occlusion_label = occlusion.label().to_string();
                        let fail_occluded = || {
                            Ok(cu_driver::ActionResult {
                                success: false,
                                duration_ms: 0,
                                detail: Some("TARGET_OCCLUDED".into()),
                            })
                        };
                        match occlusion {
                            OcclusionVerdict::Blocked(_) if !*double => match target_pid {
                                Some(pid) => {
                                    // P0-3: never emit AX input if the human
                                    // grabbed during the occlusion hit-test
                                    // (checked against the click-arm snapshot).
                                    if human_grabbed(&token, generation_before, human) {
                                        let mut detail = "cancelled by user takeover".to_string();
                                        detail.push_str(&interrupt_telemetry_suffix(human));
                                        Ok(cu_driver::ActionResult {
                                            success: false,
                                            duration_ms: started.elapsed().as_millis() as u64,
                                            detail: Some(detail),
                                        })
                                    } else {
                                        match self.driver.click_via_accessibility(pid, *x, *y).await
                                        {
                                            Ok(true) => {
                                                // P0-3: the synthetic timestamp is
                                                // updated AFTER the real AX emit
                                                // (never before), so
                                                // human_to_input_stop_ms measures
                                                // the actual input landing.
                                                stamp_synthetic(human);
                                                let _ =
                                                    self.driver.pointer_click_ripple(*x, *y).await;
                                                Ok(cu_driver::ActionResult {
                                                    success: true,
                                                    duration_ms: started.elapsed().as_millis()
                                                        as u64,
                                                    detail: Some(format!(
                                                        "pointer_backend:accessibility;isolated:true;physical_cursor_moved:false;physical_cursor_delta_px:0;occlusion:{occlusion_label}"
                                                    )),
                                                })
                                            }
                                            Ok(false) => fail_occluded(),
                                            Err(e) => Ok(cu_driver::ActionResult {
                                                success: false,
                                                duration_ms: 0,
                                                detail: Some(format!(
                                                    "TARGET_OCCLUDED;ax_error:{e}"
                                                )),
                                            }),
                                        }
                                    }
                                }
                                None => fail_occluded(),
                            },
                            _ => fail_occluded(),
                        }
                    } else {
                        // Direct CG permitted: the topmost window at (x,y) is
                        // the target (or the session is not target-scoped).
                        //
                        // Backend 1: Direct CG isolated click. P0-3: never
                        // emit the down/up if the human grabbed during the
                        // occlusion hit-test / setup — a click is real machine
                        // input even though it never moves the cursor.
                        if human_grabbed(&token, generation_before, human) {
                            let mut detail = "cancelled by user takeover".to_string();
                            detail.push_str(&interrupt_telemetry_suffix(human));
                            Ok(cu_driver::ActionResult {
                                success: false,
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: Some(detail),
                            })
                        } else {
                            let direct = self
                                .driver
                                .execute_with_cancel(&resolved, token.clone())
                                .await;
                            match direct {
                                Ok(ar) if ar.success => {
                                    // P0-3: the synthetic timestamp is updated
                                    // AFTER the real CG emit (never before), so
                                    // human_to_input_stop_ms measures the actual
                                    // input landing.
                                    stamp_synthetic(human);
                                    // Direct CGEvent isolated click succeeded.
                                    // The real system cursor was NOT moved.
                                    // Audit: play the click-ripple visual
                                    // confirmation.
                                    let _ = self.driver.pointer_click_ripple(*x, *y).await;
                                    Ok(cu_driver::ActionResult {
                                        success: true,
                                        duration_ms: started.elapsed().as_millis() as u64,
                                        detail: Some(
                                            "pointer_backend:direct_cg_event;isolated:true;physical_cursor_moved:false;physical_cursor_delta_px:0"
                                                .into(),
                                        ),
                                    })
                                }
                                Ok(_) => {
                                    // Direct CG explicitly failed (rare —
                                    // click_direct is synchronous) OR the batch
                                    // was cancelled at the emit point (P0-3):
                                    // the down/up was suppressed, or for a
                                    // double-click the state-1 pair landed and
                                    // the state-2 pair was dropped. When the
                                    // human grabbed / the token fired, NEVER
                                    // fall back to AXPress / physical — that
                                    // would emit input after the human event.
                                    if human_grabbed(&token, generation_before, human) {
                                        // A double-click's state-1 pair may have
                                        // already landed — stamp so
                                        // human_to_input_stop_ms records that
                                        // real input.
                                        if *double {
                                            stamp_synthetic(human);
                                        }
                                        let mut detail = "cancelled by user takeover".to_string();
                                        detail.push_str(&interrupt_telemetry_suffix(human));
                                        Ok(cu_driver::ActionResult {
                                            success: false,
                                            duration_ms: started.elapsed().as_millis() as u64,
                                            detail: Some(detail),
                                        })
                                    } else {
                                        // Backend 2: AXPress, isolated — EXCEPT
                                        // for double-clicks (P1). AX cannot
                                        // express a double-press, so a
                                        // double-click is NEVER degraded to a
                                        // single AXPress: it skips the AX backend
                                        // entirely and goes straight to the
                                        // physical double-click (only under
                                        // `physical_allowed`), or fails closed
                                        // with AX_UNSUPPORTED_FOR_DOUBLE_CLICK.
                                        if *double {
                                            if matches!(policy, PointerPolicy::PhysicalAllowed) {
                                                self.physical_fallback_click(
                                                    session,
                                                    token.clone(),
                                                    human,
                                                    display_id,
                                                    *button,
                                                    *x,
                                                    *y,
                                                    true,
                                                    started,
                                                )
                                                .await
                                            } else {
                                                Ok(cu_driver::ActionResult {
                                                    success: false,
                                                    duration_ms: 0,
                                                    detail: Some(
                                                        "AX_UNSUPPORTED_FOR_DOUBLE_CLICK".into(),
                                                    ),
                                                })
                                            }
                                        } else if let Some(pid) = target_pid {
                                            // P0-3: never emit AX input if the
                                            // human grabbed since the last check.
                                            if human_grabbed(&token, generation_before, human) {
                                                let mut detail =
                                                    "cancelled by user takeover".to_string();
                                                detail.push_str(&interrupt_telemetry_suffix(human));
                                                Ok(cu_driver::ActionResult {
                                                    success: false,
                                                    duration_ms: started.elapsed().as_millis()
                                                        as u64,
                                                    detail: Some(detail),
                                                })
                                            } else {
                                                match self
                                                    .driver
                                                    .click_via_accessibility(pid, *x, *y)
                                                    .await
                                                {
                                                    Ok(true) => {
                                                        // P0-3: the synthetic
                                                        // timestamp is updated
                                                        // AFTER the real AX emit.
                                                        stamp_synthetic(human);
                                                        // Audit: AXPress realized
                                                        // the click — play the
                                                        // ripple confirmation.
                                                        let _ = self
                                                            .driver
                                                            .pointer_click_ripple(*x, *y)
                                                            .await;
                                                        Ok(cu_driver::ActionResult {
                                                            success: true,
                                                            duration_ms: started.elapsed().as_millis()
                                                                as u64,
                                                            detail: Some(
                                                                "pointer_backend:accessibility;isolated:true;physical_cursor_moved:false;physical_cursor_delta_px:0"
                                                                    .into(),
                                                            ),
                                                        })
                                                    }
                                                    Ok(false) => {
                                                        // Backend 3: physical
                                                        // fallback ONLY when
                                                        // explicitly allowed.
                                                        if matches!(
                                                            policy,
                                                            PointerPolicy::PhysicalAllowed
                                                        ) {
                                                            // The full interruptible
                                                            // physical transaction
                                                            // lives in
                                                            // `physical_fallback_click`
                                                            // (P0-2, P0-5, P1):
                                                            // cursor snapshot +
                                                            // idle guard + human-grab
                                                            // checks before AND
                                                            // after the warp and
                                                            // after the click, a
                                                            // DRIVER-confirmed
                                                            // restore, and the P0-4
                                                            // interrupt telemetry.
                                                            self.physical_fallback_click(
                                                                session,
                                                                token.clone(),
                                                                human,
                                                                display_id,
                                                                *button,
                                                                *x,
                                                                *y,
                                                                false,
                                                                started,
                                                            )
                                                            .await
                                                        } else {
                                                            // isolated_only /
                                                            // isolated_preferred with
                                                            // no explicit physical ->
                                                            // fail.
                                                            Ok(cu_driver::ActionResult {
                                                                success: false,
                                                                duration_ms: 0,
                                                                detail: Some(
                                                                    "ISOLATED_POINTER_UNAVAILABLE"
                                                                        .into(),
                                                                ),
                                                            })
                                                        }
                                                    }
                                                    Err(e) => Ok(cu_driver::ActionResult {
                                                        success: false,
                                                        duration_ms: 0,
                                                        detail: Some(e.to_string()),
                                                    }),
                                                }
                                            }
                                        } else {
                                            Ok(cu_driver::ActionResult {
                                                success: false,
                                                duration_ms: 0,
                                                detail: Some("ISOLATED_POINTER_UNAVAILABLE".into()),
                                            })
                                        }
                                    }
                                }
                                Err(e) => {
                                    // A real driver failure (permission, bridge,
                                    // or an injected test failure) is surfaced
                                    // as the action failure — never silently
                                    // downgraded to a fallback. No event was
                                    // emitted, so no synthetic stamp.
                                    Ok(cu_driver::ActionResult {
                                        success: false,
                                        duration_ms: started.elapsed().as_millis() as u64,
                                        detail: Some(e.to_string()),
                                    })
                                }
                            }
                        }
                    }
                }
                // Round 8 / Phase 3: a Move is a **virtual-only** pointer
                // action. It never touches the physical system cursor — it
                // updates the session's virtual pointer and drives the ghost
                // cursor overlay only. System pointer delta stays 0.
                cu_driver::ResolvedAction::Move { to, .. } => {
                    // Audit G: the virtual move is a synthetic input event for
                    // the interrupt chain (the agent "was doing something" up
                    // to this point), even though it never touches the real
                    // cursor — it only updates the session pointer + ghost.
                    stamp_synthetic(human);
                    session.set_virtual_pointer(*to, display_id);
                    // Best-effort: the overlay is a visual aid; if the bridge
                    // cannot show it the action still succeeds (the virtual
                    // pointer moved). The mode is the session's LIVE pointer
                    // mode so the overlay reflects physical-fallback / paused
                    // / takeover states (audit: it was hardcoded isolated).
                    let mode = session.pointer_mode();
                    let _ = self
                        .driver
                        .pointer_visualized(to.x, to.y, display_id, mode)
                        .await;
                    Ok(cu_driver::ActionResult {
                        success: true,
                        duration_ms: started.elapsed().as_millis() as u64,
                        detail: Some(
                            "pointer_backend:virtual;isolated:true;physical_cursor_moved:false"
                                .into(),
                        ),
                    })
                }
                // Cancellation-aware wait: a 10s wait must stop immediately
                // when the batch token fires (cancel / stop) or the session
                // aborts mid-wait (pause/takeover/stopped), not sleep it out.
                cu_driver::ResolvedAction::Wait { duration_ms } => {
                    let deadline = Instant::now() + Duration::from_millis(*duration_ms);
                    while Instant::now() < deadline {
                        // Human Always Wins applies inside waits too. A real
                        // event always forces UserTakeover (P0-1); the
                        // heuristic fallback honors the old policy (P0-8).
                        // Checked BEFORE the token: the runtime hook cancelled
                        // the token at event time, but the flag must still be
                        // consumed here so the session transitions.
                        if let Some(h) = human {
                            if h.consume_real_takeover() {
                                let _ = session.transition(cu_core::SessionState::UserTakeover);
                                session.sync_pointer_mode(cu_core::SessionState::UserTakeover);
                                h.mark_takeover_started();
                                // P0-4: no force-mark of input-stop (see above).
                                let _ = self.driver.pointer_hidden().await;
                                wait_interrupted = true;
                                break;
                            }
                            if human_monitor_state != Some("active") && h.consume_takeover() {
                                let _ = self.apply_takeover(session, takeover);
                                wait_interrupted = true;
                                break;
                            }
                        }
                        if token.is_cancelled() || self.session_aborted(session) {
                            wait_interrupted = true;
                            break;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        tokio::select! {
                            () = token.cancelled() => {
                                wait_interrupted = true;
                                break;
                            }
                            () = tokio::time::sleep(remaining.min(Duration::from_millis(50))) => {}
                        }
                    }
                    if wait_interrupted {
                        Ok(cu_driver::ActionResult {
                            success: false,
                            duration_ms: started.elapsed().as_millis() as u64,
                            detail: Some("wait interrupted by cancellation".into()),
                        })
                    } else {
                        Ok(cu_driver::ActionResult {
                            success: true,
                            duration_ms: *duration_ms,
                            detail: None,
                        })
                    }
                }
                // P0-2: long-running actions (Drag / long Scroll / physical
                // move / Wait inside the driver) are cancellable between
                // steps via the batch token. The macOS driver sends the
                // matching mouse-up when a drag is cancelled mid-way.
                // Audit G: Drag / located Scroll / Type / Key all post REAL
                // machine input — stamp a synthetic event before each.
                _ => {
                    stamp_synthetic(human);
                    self.driver
                        .execute_with_cancel(&resolved, token.clone())
                        .await
                }
            };
            let duration_ms = started.elapsed().as_millis() as u64;

            let run = if wait_interrupted {
                ActionRun::cancelled(i)
            } else {
                match outcome {
                    Ok(ar) if ar.success => {
                        // P0-9: parse the backend detail (set by the Click
                        // Dispatcher / Move arm) into the structured
                        // PointerExecutionResult retained in the wire result.
                        let pointer = ar.detail.as_deref().and_then(parse_pointer_detail);
                        match pointer {
                            Some(p) => {
                                let mut r = ActionRun::success(i, duration_ms);
                                r.pointer = Some(p);
                                r
                            }
                            None => ActionRun::success(i, duration_ms),
                        }
                    }
                    Ok(ar) => ActionRun::failed(
                        i,
                        duration_ms,
                        ar.detail.unwrap_or_else(|| "action failed".into()),
                    ),
                    Err(e) => ActionRun::failed(i, duration_ms, e.to_string()),
                }
            };
            reports.push(run);
            if wait_interrupted {
                self.fill_cancelled(&mut reports, i + 1, actions.len());
                break;
            }

            if let Some(t) = trace {
                // Failure detail rides along with the status so benchmark
                // reports can classify failures from the trace alone (e.g.
                // a driver "permission denied" vs a "window not found").
                let error_detail = reports
                    .last()
                    .and_then(|r| r.error.clone())
                    .filter(|e| !e.is_empty());
                let mut result_json = serde_json::json!({
                    "status": reports.last().map(|r| &r.status).cloned().unwrap_or_else(|| "unknown".into()),
                    "duration_ms": duration_ms,
                });
                if let Some(detail) = error_detail {
                    result_json["error"] = serde_json::json!(detail);
                }
                // Audit G: the pointer-execution result (backend, isolation,
                // cursor deltas, and the REAL P0-4 interrupt telemetry —
                // event_detection_latency_ms / human_to_takeover_ms /
                // human_to_input_stop_ms) is recorded into the trace so
                // benchmark / latency analysis can read it without re-deriving
                // it from the action type.
                if let Some(pointer) = reports.last().and_then(|r| r.pointer.clone()) {
                    result_json["pointer"] = serde_json::to_value(&pointer).unwrap_or_default();
                }
                // Required mode propagates trace-write failures (the batch
                // fails); best-effort mode degrades inside the recorder.
                t.record_action(
                    request_id.map(|s| s.to_string()),
                    Some(frame_id.to_string()),
                    action,
                    result_json,
                    duration_ms,
                    Some(display_id.to_string()),
                    active_app.map(|s| s.to_string()),
                )
                .await?;
            }

            // Human-takeover probe. Since round 8 the physical pointer delta
            // heuristic is a FALLBACK only — the Event Tap is the primary
            // detector. This branch compares the REAL cursor to the position
            // it was left at after actions that do not move it by design.
            // The agent's own position bookkeeping (last_pointer) always uses
            // the session's virtual pointer, never the physical cursor.
            //
            // P0-1 / P0-8: the post-action pointer-delta heuristic is gated on
            // Event Tap state. When the hardware Event Tap is active and
            // authoritative, only the pre-action real-takeover check runs; this
            // heuristic probe is suppressed (no need for a redundant fallback).
            if human_monitor_state != Some("active") && !action_moves_pointer(action) {
                if let Ok(pi) = self.driver.pointer_location().await {
                    let dx = pi.location.x - last_pointer.x;
                    let dy = pi.location.y - last_pointer.y;
                    if takeover.observe(dx, dy) {
                        let _ = self.apply_takeover(session, takeover);
                        self.fill_cancelled(&mut reports, i + 1, actions.len());
                        break;
                    }
                }
            } else if action_moves_pointer(action) {
                last_pointer = session.virtual_pointer.lock().unwrap().location();
            }
        }

        Ok(reports)
    }

    /// Round 9 / P0-2 + P0-5 + P1: the PHYSICAL fallback is a REAL
    /// interruptible transaction. EVERY stage re-checks the batch token AND
    /// the human-event generation before proceeding:
    ///
    /// 1. snapshot cursor + generation BEFORE borrowing the cursor;
    /// 2. refuse to start while the user is actively operating
    ///    (HUMAN_TAKEOVER);
    /// 3. warp the real cursor (shared token);
    ///    → human during/after warp: DO NOT CLICK, DO NOT RESTORE;
    /// 4. click (synchronous CGEvent post); `double == true` posts the
    ///    state-1 pair then the state-2 pair via
    ///    [`ComputerDriver::physical_double_click_at`] so the OS treats it as
    ///    a real double-click — NEVER two single clicks, and never a single
    ///    AXPress (P1);
    ///    → human after click: DO NOT RESTORE;
    /// 5. restore ONLY when no human input occurred at ANY stage; the restore
    ///    success is the DRIVER's real result, never assumed.
    #[allow(clippy::too_many_arguments)] // one coherent transaction boundary
    async fn physical_fallback_click(
        &self,
        session: &SharedSession,
        token: CancellationToken,
        human: Option<&HumanInputMonitor>,
        display_id: &str,
        button: MouseButton,
        x: f64,
        y: f64,
        double: bool,
        started: Instant,
    ) -> Result<cu_driver::ActionResult, CuError> {
        let before = self
            .driver
            .pointer_location()
            .await
            .ok()
            .map(|p| p.location);
        let generation_before = human.map(|h| h.human_event_generation());
        let be = match before {
            Some(bp) => {
                // Req 2: the user is at the mouse right now — do not borrow it.
                if human
                    .and_then(|h| h.idle_secs())
                    .map(|s| s < PHYSICAL_FALLBACK_IDLE_GUARD_SECS)
                    .unwrap_or(false)
                {
                    Ok(cu_driver::ActionResult {
                        success: false,
                        duration_ms: started.elapsed().as_millis() as u64,
                        detail: Some("HUMAN_TAKEOVER".into()),
                    })
                } else if human_grabbed(&token, generation_before, human) {
                    // P0-5: the user grabbed between the snapshot and the
                    // warp — never borrow the cursor.
                    Ok(cu_driver::ActionResult {
                        success: false,
                        duration_ms: started.elapsed().as_millis() as u64,
                        detail: Some("cancelled by user takeover".into()),
                    })
                } else {
                    // Req 3a: warp the real cursor through the shared batch
                    // token. Audit G: stamp the synthetic event BEFORE the
                    // warp — the human can grab DURING the warp, and the
                    // interrupt chain measures the gap from this stamp to
                    // that grab. The ghost cursor switches to the
                    // physical-fallback state while the real cursor is
                    // borrowed (restored below).
                    session
                        .virtual_pointer
                        .lock()
                        .unwrap()
                        .set_mode(PointerMode::PhysicalFallback);
                    let _ = self
                        .driver
                        .pointer_visualized(bp.x, bp.y, display_id, PointerMode::PhysicalFallback)
                        .await;
                    stamp_synthetic(human);
                    let warp = self
                        .driver
                        .execute_with_cancel(
                            &cu_driver::ResolvedAction::Move {
                                from: bp,
                                to: cu_core::Point::new(x, y),
                                duration_ms: None,
                            },
                            token.clone(),
                        )
                        .await;
                    if !matches!(warp, Ok(ar) if ar.success)
                        || human_grabbed(&token, generation_before, human)
                    {
                        // P0-5: the warp was interrupted OR the user grabbed
                        // during/after it. DO NOT CLICK, DO NOT RESTORE.
                        // Reflect the takeover in the ghost mode + hide the
                        // overlay (the real cursor is in the user's hands).
                        session
                            .virtual_pointer
                            .lock()
                            .unwrap()
                            .set_mode(PointerMode::UserTakeover);
                        let _ = self.driver.pointer_hidden().await;
                        // P0-4: the interrupt telemetry rides the failure
                        // detail so the interrupt is never lost.
                        let mut detail = "cancelled by user takeover".to_string();
                        detail.push_str(&interrupt_telemetry_suffix(human));
                        Ok(cu_driver::ActionResult {
                            success: false,
                            duration_ms: started.elapsed().as_millis() as u64,
                            detail: Some(detail),
                        })
                    } else {
                        // The click is a synchronous CGEvent post
                        // (microseconds). Audit G (P0-4): the click is itself
                        // real machine input — stamp before posting. P1: a
                        // double-click posts the state-1 pair then the
                        // state-2 pair — the physical backend preserves
                        // double-click semantics, never a single click.
                        stamp_synthetic(human);
                        let phys = if double {
                            self.driver.physical_double_click_at(button, x, y).await
                        } else {
                            self.driver.physical_click_at(button, x, y).await
                        };
                        // Audit: visible click-ripple confirmation at the
                        // click point.
                        let _ = self.driver.pointer_click_ripple(x, y).await;
                        if human_grabbed(&token, generation_before, human) {
                            // P0-5: the user grabbed after the click — DO NOT
                            // RESTORE (never yank the cursor back).
                            session
                                .virtual_pointer
                                .lock()
                                .unwrap()
                                .set_mode(PointerMode::UserTakeover);
                            let _ = self.driver.pointer_hidden().await;
                            // Audit G (P0-4): the interrupt telemetry is REAL
                            // and rides the action result into the trace.
                            let mut detail =
                                "pointer_backend:physical;isolated:false;physical_cursor_moved:true;physical_cursor_restored:false;human_input_during_fallback:true"
                                    .to_string();
                            detail.push_str(&interrupt_telemetry_suffix(human));
                            phys.map(|ok| cu_driver::ActionResult {
                                success: ok,
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: Some(detail),
                            })
                        } else {
                            // Req 3b: restore through the SAME token (a grab
                            // during restore aborts). P0-5: the restore's
                            // success is the DRIVER's real result, never
                            // assumed.
                            let restore = self
                                .driver
                                .execute_with_cancel(
                                    &cu_driver::ResolvedAction::Move {
                                        from: cu_core::Point::new(x, y),
                                        to: bp,
                                        duration_ms: None,
                                    },
                                    token.clone(),
                                )
                                .await;
                            let restore_ok = matches!(restore, Ok(ar) if ar.success);
                            // Audit: the fallback ended without a grab —
                            // restore the ghost mode + re-show the overlay at
                            // the home position.
                            session
                                .virtual_pointer
                                .lock()
                                .unwrap()
                                .set_mode(PointerMode::Isolated);
                            let _ = self
                                .driver
                                .pointer_visualized(bp.x, bp.y, display_id, PointerMode::Isolated)
                                .await;
                            phys.map(|ok| cu_driver::ActionResult {
                                success: ok,
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: Some(format!(
                                    "pointer_backend:physical;isolated:false;physical_cursor_moved:true;physical_cursor_restored:{restore_ok};human_input_during_fallback:false"
                                )),
                            })
                        }
                    }
                }
            }
            None => Ok(cu_driver::ActionResult {
                success: false,
                duration_ms: 0,
                detail: Some("ISOLATED_POINTER_UNAVAILABLE".into()),
            }),
        };
        be
    }

    /// Round 8 / P0-1: before a Direct CG click, in a target-scoped session,
    /// verify the coordinate's topmost window IS the session target. The
    /// identity comparison is window_id + pid (the primary signal), with the
    /// bundle as an auxiliary consistency check — a window whose owner PID is
    /// the target's PID and whose window id equals the target's window id IS
    /// the target, so a bundle mismatch between two known bundles is treated
    /// as a block (fail closed, never click through a possibly-recycled id).
    /// `Ok(None)` from the hit-test and driver errors both fail closed: Direct
    /// CG is NEVER attempted when the target's topmost-ness is unverifiable.
    async fn occlusion_verdict(&self, session: &SharedSession, x: f64, y: f64) -> OcclusionVerdict {
        let Some(target) = session.get_resolved_target() else {
            return OcclusionVerdict::NotTargetScoped;
        };
        match self.driver.window_at_point(x, y).await {
            Ok(Some(top)) => {
                let identity_matches = top.window_id == target.window_id && top.pid == target.pid;
                let bundle_consistent = top.bundle_id == "unknown"
                    || target.bundle_id == "unknown"
                    || top.bundle_id == target.bundle_id;
                if identity_matches && bundle_consistent {
                    OcclusionVerdict::Clear
                } else {
                    OcclusionVerdict::Blocked(top)
                }
            }
            Ok(None) => OcclusionVerdict::NoWindow,
            Err(_) => OcclusionVerdict::Unverifiable,
        }
    }

    /// Convert a wire action into a driver action in global points.
    fn to_resolved(
        &self,
        action: &ComputerAction,
        geometry: &ImageGeometry,
        current_pointer: Point,
    ) -> Result<cu_driver::ResolvedAction, CuError> {
        Ok(match action {
            ComputerAction::Click {
                x,
                y,
                button,
                coordinate_space,
            } => {
                let p = geometry.to_global(*coordinate_space, Point::new(*x, *y))?;
                cu_driver::ResolvedAction::Click {
                    x: p.x,
                    y: p.y,
                    button: *button,
                    double: false,
                }
            }
            ComputerAction::DoubleClick {
                x,
                y,
                button,
                coordinate_space,
            } => {
                let p = geometry.to_global(*coordinate_space, Point::new(*x, *y))?;
                cu_driver::ResolvedAction::Click {
                    x: p.x,
                    y: p.y,
                    button: *button,
                    double: true,
                }
            }
            ComputerAction::Move {
                x,
                y,
                coordinate_space,
                duration_ms,
            } => {
                let to = geometry.to_global(*coordinate_space, Point::new(*x, *y))?;
                cu_driver::ResolvedAction::Move {
                    from: current_pointer,
                    to,
                    duration_ms: *duration_ms,
                }
            }
            ComputerAction::TypeText { text, method } => cu_driver::ResolvedAction::TypeText {
                text: text.clone(),
                method: *method,
            },
            ComputerAction::Key { keys } => cu_driver::ResolvedAction::Key { keys: keys.clone() },
            ComputerAction::Scroll {
                x,
                y,
                delta_x,
                delta_y,
                coordinate_space,
            } => {
                let at = match (x, y) {
                    (Some(x), Some(y)) => {
                        Some(geometry.to_global(*coordinate_space, Point::new(*x, *y))?)
                    }
                    _ => None,
                };
                cu_driver::ResolvedAction::Scroll {
                    at,
                    delta_x: *delta_x,
                    delta_y: *delta_y,
                }
            }
            ComputerAction::Drag {
                from,
                to,
                coordinate_space,
                duration_ms,
            } => {
                let from = geometry.to_global(*coordinate_space, *from)?;
                let to = geometry.to_global(*coordinate_space, *to)?;
                cu_driver::ResolvedAction::Drag {
                    from,
                    to,
                    duration_ms: *duration_ms,
                }
            }
            ComputerAction::Wait { duration_ms } => cu_driver::ResolvedAction::Wait {
                duration_ms: *duration_ms,
            },
        })
    }

    fn session_aborted(&self, session: &Session) -> bool {
        session.is_paused()
            || session.is_user_takeover()
            || matches!(
                session.state(),
                cu_core::SessionState::Stopping
                    | cu_core::SessionState::Stopped
                    | cu_core::SessionState::Failed
            )
    }

    fn fill_cancelled(&self, reports: &mut Vec<ActionRun>, from: usize, len: usize) {
        for j in from..len {
            reports.push(ActionRun::cancelled(j));
        }
    }

    fn apply_takeover(
        &self,
        session: &Session,
        detector: &TakeoverDetector,
    ) -> Result<(), CuError> {
        match detector.policy {
            TakeoverPolicy::Ignore => {
                // Keep going; the caller explicitly configured this. Reset the
                // streak so a single grab does not compound.
                Ok(())
            }
            TakeoverPolicy::AutoPause => session.transition(cu_core::SessionState::Paused),
            TakeoverPolicy::ImmediateTakeover => {
                session.transition(cu_core::SessionState::UserTakeover)
            }
        }
    }
}

/// Audit G: stamp a synthetic input event at the moment a backend POSTS real
/// input to the machine. The human-interrupt chain measures
/// `human_event → last synthetic event`; without a stamp here every interrupt
/// would look human-initiated (latency `None`). `record_synthetic_event` has
/// no other callers — the backends below (virtual, direct CG, AXPress,
/// physical fallback, drag / scroll / type / key) are the whole set of places
/// the agent emits input.
fn stamp_synthetic(human: Option<&HumanInputMonitor>) {
    if let Some(h) = human {
        h.record_synthetic_event(std::time::Instant::now());
    }
}

/// P0-4: build the `;key:value` interrupt-telemetry suffix for a physical
/// fallback detail string from the monitor's REAL measurements. Only fields
/// that have a value are appended; a grab with nothing measurable stays empty.
fn interrupt_telemetry_suffix(human: Option<&HumanInputMonitor>) -> String {
    let mut s = String::new();
    if let Some(d) = human.and_then(|h| h.event_detection_latency_ms()) {
        s.push_str(&format!(";event_detection_latency_ms:{d}"));
    }
    if let Some(t) = human.and_then(|h| h.event_to_takeover_ms()) {
        s.push_str(&format!(";human_to_takeover_ms:{t}"));
    }
    if let Some(st) = human.and_then(|h| h.human_to_input_stop_ms()) {
        s.push_str(&format!(";human_to_input_stop_ms:{st}"));
    }
    s
}

/// P0-5: did the user grab the machine since the transaction snapshot? The
/// batch token firing (cancel / stop — the human-event hook cancels the batch
/// AT EVENT TIME) OR a real human event (the generation moved past the
/// snapshot) both mean the user is in control. EVERY physical fallback stage
/// re-checks this before proceeding.
fn human_grabbed(
    token: &CancellationToken,
    generation_before: Option<u64>,
    human: Option<&HumanInputMonitor>,
) -> bool {
    token.is_cancelled()
        || match (generation_before, human.map(|h| h.human_event_generation())) {
            (Some(b), Some(a)) => a != b,
            _ => false,
        }
}

/// Whether an action relocates the pointer by design (so its own movement is
/// never mistaken for a human grab).
/// The global location (if any) an action will act on, for target-bound checks.
fn resolved_location(r: &cu_driver::ResolvedAction) -> Option<Point> {
    match r {
        cu_driver::ResolvedAction::Click { x, y, .. }
        | cu_driver::ResolvedAction::Move {
            to: Point { x, y }, ..
        }
        | cu_driver::ResolvedAction::Drag {
            to: Point { x, y }, ..
        }
        | cu_driver::ResolvedAction::Scroll {
            at: Some(Point { x, y }),
            ..
        } => Some(Point::new(*x, *y)),
        _ => None,
    }
}

/// Whether every global point an action will actually touch falls inside
/// `bounds`. Most actions act on one point; a Drag acts on TWO (its real
/// cursor sweeps from `from` to `to`), so both endpoints must be inside —
/// otherwise the drag starts outside the session window and drags across
/// other apps (P0-7).
fn resolved_within_bounds(r: &cu_driver::ResolvedAction, bounds: &cu_core::DisplayBounds) -> bool {
    match r {
        cu_driver::ResolvedAction::Drag { from, to, .. } => {
            bounds.contains_global(*from) && bounds.contains_global(*to)
        }
        other => match resolved_location(other) {
            Some(p) => bounds.contains_global(p),
            None => true,
        },
    }
}

/// Whether an action relocates the pointer by design (so its own movement is
/// never mistaken for a human grab).
fn action_moves_pointer(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::Move { .. }
            | ComputerAction::Click { .. }
            | ComputerAction::DoubleClick { .. }
            | ComputerAction::Drag { .. }
            | ComputerAction::Scroll {
                x: Some(_),
                y: Some(_),
                ..
            }
    )
}

/// Convenience used by `act()` to build the wire-level report list.
pub fn to_action_result_reports(runs: &[ActionRun]) -> Vec<cu_core::ActionResultReport> {
    runs.iter()
        .map(|r| cu_core::ActionResultReport {
            index: r.index,
            status: r.status.clone(),
            duration_ms: r.duration_ms,
            error: r.error.clone(),
            pointer: r.pointer.clone(),
        })
        .collect()
}

/// Parse the `pointer_backend:...;isolated:...;physical_cursor_moved:...`
/// detail string emitted by the Click Dispatcher / Move arm into a structured
/// [`cu_core::PointerExecutionResult`] (round 9 / P0-9). Unknown/absent -> None.
fn parse_pointer_detail(detail: &str) -> Option<cu_core::PointerExecutionResult> {
    let mut backend = None;
    let mut isolated = None;
    let mut moved = None;
    let mut delta_px = 0.0f64;
    let mut restored = None;
    let mut human_during_fallback = None;
    let mut event_detection_latency_ms = None;
    let mut human_to_takeover_ms = None;
    let mut human_to_input_stop_ms = None;
    for part in detail.split(';') {
        let mut kv = part.splitn(2, ':');
        let (k, v) = (kv.next()?, kv.next()?);
        match k {
            "pointer_backend" => backend = Some(v.to_string()),
            "isolated" => isolated = v.parse::<bool>().ok(),
            "physical_cursor_moved" => moved = v.parse::<bool>().ok(),
            "physical_cursor_delta_px" => delta_px = v.parse().unwrap_or(0.0),
            "physical_cursor_restored" => restored = v.parse::<bool>().ok(),
            "human_input_during_fallback" => human_during_fallback = v.parse::<bool>().ok(),
            "event_detection_latency_ms" => event_detection_latency_ms = v.parse::<u64>().ok(),
            "human_to_takeover_ms" => human_to_takeover_ms = v.parse::<u64>().ok(),
            "human_to_input_stop_ms" => human_to_input_stop_ms = v.parse::<u64>().ok(),
            _ => {}
        }
    }
    Some(cu_core::PointerExecutionResult {
        backend: backend?,
        isolated: isolated?,
        physical_cursor_moved: moved?,
        physical_cursor_delta_px: delta_px,
        physical_cursor_restored: restored,
        human_input_during_fallback: human_during_fallback,
        event_detection_latency_ms,
        human_to_takeover_ms,
        human_to_input_stop_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_moves_pointer_truth_table() {
        use cu_core::{CoordinateSpace, MouseButton, TextInputMethod};
        assert!(action_moves_pointer(&ComputerAction::Move {
            x: 0.0,
            y: 0.0,
            coordinate_space: CoordinateSpace::Normalized1000,
            duration_ms: None,
        }));
        assert!(action_moves_pointer(&ComputerAction::Click {
            x: 0.0,
            y: 0.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        }));
        assert!(!action_moves_pointer(&ComputerAction::TypeText {
            text: "hi".into(),
            method: TextInputMethod::Keyboard,
        }));
        assert!(!action_moves_pointer(&ComputerAction::Wait {
            duration_ms: 10
        }));
        assert!(!action_moves_pointer(&ComputerAction::Scroll {
            x: None,
            y: None,
            delta_x: 0.0,
            delta_y: -10.0,
            coordinate_space: CoordinateSpace::Normalized1000,
        }));
    }

    #[test]
    fn report_conversion_preserves_fields() {
        let runs = vec![
            ActionRun::success(0, 12),
            ActionRun::failed(1, 3, "boom".into()),
            ActionRun::cancelled(2),
        ];
        let reports = to_action_result_reports(&runs);
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[0].status, "success");
        assert_eq!(reports[1].status, "failed");
        assert_eq!(reports[2].status, "cancelled");
        assert_eq!(reports[1].error.as_deref(), Some("boom"));
    }

    // ------------------------------------------------------------------
    // P0-2: Physical Fallback Transaction
    // ------------------------------------------------------------------

    use async_trait::async_trait;
    use cu_core::{CoordinateSpace, MouseButton, SessionTarget};
    use cu_driver::ResolvedAction;
    use cu_policy::TakeoverDetector;
    use std::sync::Arc;

    /// Fake driver that exercises the P0-2 physical fallback transaction. It
    /// records every Move it receives (warp + restore) so the test can assert
    /// the restore happened (or, on human input, did NOT).
    struct PhysicalFallbackDriver {
        /// Moves executed (warp + optional restore), in order.
        moves: std::sync::Mutex<Vec<cu_core::Point>>,
        /// Physical SINGLE clicks posted (P0-5: human during warp must
        /// suppress the click entirely; assertable as clicks == 0).
        clicks: std::sync::atomic::AtomicUsize,
        /// Physical DOUBLE-clicks posted via `physical_double_click_at` (P1:
        /// a double-click's physical fallback must be a real click-click, not
        /// two single clicks — assertable as clicks == 0 && double_clicks == 1).
        double_clicks: std::sync::atomic::AtomicUsize,
        /// AXPress attempts (P1: a double-click must NEVER reach the single
        /// AXPress backend — assertable as ax_calls == 0).
        ax_calls: std::sync::atomic::AtomicUsize,
        /// When true, the first Move (the warp) fires a real human event
        /// mid-path — simulating the user grabbing the mouse during the warp.
        human_grab_during_warp: std::sync::atomic::AtomicBool,
        /// When true, the physical click fires a real human event AFTER the
        /// click posts — simulating the user grabbing the mouse right after
        /// the click (P0-5: must suppress the restore, not the click).
        human_grab_at_click: std::sync::atomic::AtomicBool,
        /// The human monitor the warp reports into (for the grab simulation).
        human: std::sync::Mutex<Option<std::sync::Arc<HumanInputMonitor>>>,
        /// Current physical pointer (what `pointer_location` reports).
        pointer: std::sync::Mutex<cu_core::Point>,
    }

    impl Default for PhysicalFallbackDriver {
        fn default() -> Self {
            Self {
                moves: std::sync::Mutex::new(Vec::new()),
                clicks: std::sync::atomic::AtomicUsize::new(0),
                double_clicks: std::sync::atomic::AtomicUsize::new(0),
                ax_calls: std::sync::atomic::AtomicUsize::new(0),
                human_grab_during_warp: std::sync::atomic::AtomicBool::new(false),
                human_grab_at_click: std::sync::atomic::AtomicBool::new(false),
                human: std::sync::Mutex::new(None),
                pointer: std::sync::Mutex::new(cu_core::Point::new(10.0, 10.0)),
            }
        }
    }

    #[async_trait]
    impl ComputerDriver for PhysicalFallbackDriver {
        async fn execute_with_cancel(
            &self,
            action: &ResolvedAction,
            _cancel: CancellationToken,
        ) -> Result<cu_driver::ActionResult, CuError> {
            match action {
                // The direct CG click "fails" so the dispatcher falls through
                // to AXPress and then the physical fallback.
                ResolvedAction::Click { .. } => Ok(cu_driver::ActionResult {
                    success: false,
                    duration_ms: 1,
                    detail: None,
                }),
                ResolvedAction::Move { to, .. } => {
                    // Simulate the user grabbing the mouse at the moment of
                    // the warp (only the FIRST move, the warp, not the restore).
                    if self
                        .human_grab_during_warp
                        .load(std::sync::atomic::Ordering::SeqCst)
                        && self.moves.lock().unwrap().is_empty()
                    {
                        if let Some(h) = self.human.lock().unwrap().clone() {
                            h.record_human_event(std::time::Instant::now());
                        }
                    }
                    *self.pointer.lock().unwrap() = *to;
                    self.moves.lock().unwrap().push(*to);
                    Ok(cu_driver::ActionResult {
                        success: true,
                        duration_ms: 1,
                        detail: None,
                    })
                }
                _ => Ok(cu_driver::ActionResult {
                    success: true,
                    duration_ms: 1,
                    detail: None,
                }),
            }
        }

        async fn execute(
            &self,
            action: &ResolvedAction,
        ) -> Result<cu_driver::ActionResult, CuError> {
            self.execute_with_cancel(action, CancellationToken::new())
                .await
        }

        async fn physical_click_at(
            &self,
            _button: MouseButton,
            _x: f64,
            _y: f64,
        ) -> Result<bool, CuError> {
            // P0-5 case 3: simulate the user grabbing the mouse AFTER the click
            // posts — before the restore would run.
            if self
                .human_grab_at_click
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(h) = self.human.lock().unwrap().clone() {
                    h.record_human_event(std::time::Instant::now());
                }
            }
            self.clicks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        }

        async fn physical_double_click_at(
            &self,
            _button: MouseButton,
            _x: f64,
            _y: f64,
        ) -> Result<bool, CuError> {
            self.double_clicks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        }

        async fn click_via_accessibility(
            &self,
            _pid: i32,
            _x: f64,
            _y: f64,
        ) -> Result<bool, CuError> {
            // P1: a double-click must NEVER reach the single AXPress backend.
            self.ax_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(false) // unavailable -> forces the physical fallback
        }

        async fn pointer_location(&self) -> Result<cu_driver::PointerInfo, CuError> {
            Ok(cu_driver::PointerInfo {
                location: *self.pointer.lock().unwrap(),
                display_id: None,
            })
        }

        async fn list_displays(&self) -> Result<Vec<cu_driver::DisplayInfo>, CuError> {
            unimplemented!()
        }
        async fn desktop_layout(&self) -> Result<cu_driver::DesktopLayout, CuError> {
            unimplemented!()
        }
        async fn capture(
            &self,
            _request: cu_driver::CaptureRequest,
        ) -> Result<cu_driver::CapturedFrame, CuError> {
            unimplemented!()
        }
        async fn quick_snapshot(
            &self,
            _display_id: &str,
        ) -> Result<cu_driver::QuickSnapshot, CuError> {
            unimplemented!()
        }
        async fn permission_status(&self) -> Result<cu_driver::PermissionStatus, CuError> {
            unimplemented!()
        }
        async fn active_application(&self) -> Result<Option<cu_driver::ApplicationInfo>, CuError> {
            unimplemented!()
        }
        async fn shutdown(&self) -> Result<(), CuError> {
            Ok(())
        }
    }

    /// Fake driver that exercises the P0-1 occlusion guard. `window_at_point`
    /// returns a configurable topmost window (or none / an error), and the
    /// Direct CG click / AXPress / physical backends are counted so a test can
    /// assert which backend actually ran — a blocked click must never reach
    /// Direct CG, and an occluded double-click must never reach AX or physical.
    struct OcclusionDriver {
        /// What `window_at_point` reports as topmost. None = no window at point.
        topmost: std::sync::Mutex<Option<cu_driver::WindowAtPoint>>,
        /// When true, `window_at_point` fails (driver error) → unverifiable.
        hit_test_fails: std::sync::atomic::AtomicBool,
        /// Whether the Direct CG click reports success (when it is reached).
        direct_click_succeeds: std::sync::atomic::AtomicBool,
        /// Direct CG clicks attempted (the exact thing occlusion must prevent).
        direct_clicks: std::sync::atomic::AtomicUsize,
        /// AXPress attempts (occluded singles may retry via AX).
        ax_calls: std::sync::atomic::AtomicUsize,
        /// Whether AXPress reports success.
        ax_succeeds: std::sync::atomic::AtomicBool,
        /// Physical clicks attempted (must stay 0 under occlusion).
        physical_clicks: std::sync::atomic::AtomicUsize,
        /// P0-3: the human monitor the fake reports into.
        human: std::sync::Mutex<Option<std::sync::Arc<HumanInputMonitor>>>,
        /// P0-3: when true, `window_at_point` fires a REAL human event mid
        /// hit-test — the grab lands after the click-arm generation snapshot
        /// but before the Direct CG / AX emit, exercising the pre-emit
        /// `human_grabbed` checks.
        human_grab_in_hit_test: std::sync::atomic::AtomicBool,
        /// P0-3: when true, the Direct CG `execute_with_cancel` fires a REAL
        /// human event — the grab lands DURING the emit itself, so the
        /// `Ok(_)` fallback arm must NOT degrade to AXPress.
        human_grab_at_direct: std::sync::atomic::AtomicBool,
    }

    impl Default for OcclusionDriver {
        fn default() -> Self {
            Self {
                topmost: std::sync::Mutex::new(None),
                hit_test_fails: std::sync::atomic::AtomicBool::new(false),
                direct_click_succeeds: std::sync::atomic::AtomicBool::new(true),
                direct_clicks: std::sync::atomic::AtomicUsize::new(0),
                ax_calls: std::sync::atomic::AtomicUsize::new(0),
                ax_succeeds: std::sync::atomic::AtomicBool::new(true),
                physical_clicks: std::sync::atomic::AtomicUsize::new(0),
                human: std::sync::Mutex::new(None),
                human_grab_in_hit_test: std::sync::atomic::AtomicBool::new(false),
                human_grab_at_direct: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait]
    impl ComputerDriver for OcclusionDriver {
        async fn execute_with_cancel(
            &self,
            action: &ResolvedAction,
            _cancel: CancellationToken,
        ) -> Result<cu_driver::ActionResult, CuError> {
            match action {
                ResolvedAction::Click { .. } => {
                    // P0-3: simulate the user grabbing the mouse DURING the
                    // Direct CG emit — the runtime's `Ok(_)` arm must see the
                    // grab and NOT degrade to AXPress / physical.
                    if self
                        .human_grab_at_direct
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        if let Some(h) = self.human.lock().unwrap().clone() {
                            h.record_human_event(std::time::Instant::now());
                        }
                    }
                    self.direct_clicks
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(cu_driver::ActionResult {
                        success: self
                            .direct_click_succeeds
                            .load(std::sync::atomic::Ordering::SeqCst),
                        duration_ms: 1,
                        detail: None,
                    })
                }
                ResolvedAction::Move { .. } => Ok(cu_driver::ActionResult {
                    success: true,
                    duration_ms: 1,
                    detail: None,
                }),
                _ => Ok(cu_driver::ActionResult {
                    success: true,
                    duration_ms: 1,
                    detail: None,
                }),
            }
        }

        async fn execute(
            &self,
            action: &ResolvedAction,
        ) -> Result<cu_driver::ActionResult, CuError> {
            self.execute_with_cancel(action, CancellationToken::new())
                .await
        }

        async fn window_at_point(
            &self,
            _x: f64,
            _y: f64,
        ) -> Result<Option<cu_driver::WindowAtPoint>, CuError> {
            // P0-3: simulate the user grabbing the mouse DURING the hit-test —
            // the grab lands after the click-arm generation snapshot but before
            // any emit.
            if self
                .human_grab_in_hit_test
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(h) = self.human.lock().unwrap().clone() {
                    h.record_human_event(std::time::Instant::now());
                }
            }
            if self
                .hit_test_fails
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(CuError::Driver("hit-test failed".into()));
            }
            Ok(self.topmost.lock().unwrap().clone())
        }

        async fn click_via_accessibility(
            &self,
            _pid: i32,
            _x: f64,
            _y: f64,
        ) -> Result<bool, CuError> {
            self.ax_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.ax_succeeds.load(std::sync::atomic::Ordering::SeqCst))
        }

        async fn physical_click_at(
            &self,
            _button: MouseButton,
            _x: f64,
            _y: f64,
        ) -> Result<bool, CuError> {
            self.physical_clicks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        }

        async fn physical_double_click_at(
            &self,
            _button: MouseButton,
            _x: f64,
            _y: f64,
        ) -> Result<bool, CuError> {
            self.physical_clicks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        }

        async fn pointer_location(&self) -> Result<cu_driver::PointerInfo, CuError> {
            Ok(cu_driver::PointerInfo {
                location: cu_core::Point::new(10.0, 10.0),
                display_id: None,
            })
        }

        async fn list_displays(&self) -> Result<Vec<cu_driver::DisplayInfo>, CuError> {
            unimplemented!()
        }
        async fn desktop_layout(&self) -> Result<cu_driver::DesktopLayout, CuError> {
            unimplemented!()
        }
        async fn capture(
            &self,
            _request: cu_driver::CaptureRequest,
        ) -> Result<cu_driver::CapturedFrame, CuError> {
            unimplemented!()
        }
        async fn quick_snapshot(
            &self,
            _display_id: &str,
        ) -> Result<cu_driver::QuickSnapshot, CuError> {
            unimplemented!()
        }
        async fn permission_status(&self) -> Result<cu_driver::PermissionStatus, CuError> {
            unimplemented!()
        }
        async fn active_application(&self) -> Result<Option<cu_driver::ApplicationInfo>, CuError> {
            unimplemented!()
        }
        async fn shutdown(&self) -> Result<(), CuError> {
            Ok(())
        }
    }

    /// Run a single Click through the queue under `physical_allowed`.
    async fn run_physical_click(
        fake: &dyn ComputerDriver,
        human: Option<&HumanInputMonitor>,
    ) -> Vec<ActionRun> {
        run_physical_click_with_session(fake, human).await.1
    }

    /// `run_physical_click` + the session handle, so tests can inspect the
    /// ghost-cursor pointer mode after the transaction.
    async fn run_physical_click_with_session(
        fake: &dyn ComputerDriver,
        human: Option<&HumanInputMonitor>,
    ) -> (std::sync::Arc<Session>, Vec<ActionRun>) {
        let queue = ActionQueue::new(fake);
        let session = std::sync::Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            cu_core::SecretTokenHash::from_token(&cu_core::generate_observation_token()),
            None,
        ));
        session.set_pointer_policy(PointerPolicy::PhysicalAllowed);
        // A target pid routes the dispatcher through AXPress -> physical.
        session.set_target(Some(SessionTarget {
            bundle_id: None,
            pid: Some(42),
            window_id: None,
        }));
        let geometry = ImageGeometry {
            image_width_px: 1280,
            image_height_px: 800,
            display_bounds: cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
        };
        let actions = vec![ComputerAction::Click {
            x: 1000.0,
            y: 800.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        }];
        let token = CancellationToken::new();
        let mut takeover = TakeoverDetector {
            policy: TakeoverPolicy::AutoPause,
            ..Default::default()
        };
        let runs = queue
            .run(
                &session,
                &actions,
                &geometry,
                token,
                &mut takeover,
                human,
                None,
                None,
                "f",
                "1",
                None,
                (),
                Some("active"),
            )
            .await
            .unwrap();
        (session, runs)
    }

    /// Run a single DoubleClick through the queue under a given pointer policy
    /// (P1): a double-click whose Direct CG path fails must skip the single
    /// AXPress backend entirely and reach the physical double-click only under
    /// `PhysicalAllowed`, or fail closed with AX_UNSUPPORTED_FOR_DOUBLE_CLICK.
    async fn run_double_click_with_policy(
        fake: &dyn ComputerDriver,
        human: Option<&HumanInputMonitor>,
        policy: PointerPolicy,
    ) -> (std::sync::Arc<Session>, Vec<ActionRun>) {
        let queue = ActionQueue::new(fake);
        let session = std::sync::Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            cu_core::SecretTokenHash::from_token(&cu_core::generate_observation_token()),
            None,
        ));
        session.set_pointer_policy(policy);
        session.set_target(Some(SessionTarget {
            bundle_id: None,
            pid: Some(42),
            window_id: None,
        }));
        let geometry = ImageGeometry {
            image_width_px: 1280,
            image_height_px: 800,
            display_bounds: cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
        };
        let actions = vec![ComputerAction::DoubleClick {
            x: 1000.0,
            y: 800.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        }];
        let token = CancellationToken::new();
        let mut takeover = TakeoverDetector {
            policy: TakeoverPolicy::AutoPause,
            ..Default::default()
        };
        let runs = queue
            .run(
                &session,
                &actions,
                &geometry,
                token,
                &mut takeover,
                human,
                None,
                None,
                "f",
                "1",
                None,
                (),
                Some("active"),
            )
            .await
            .unwrap();
        (session, runs)
    }

    #[tokio::test]
    async fn physical_fallback_restores_when_no_human_input() {
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        let runs = run_physical_click(fake.as_ref(), Some(human.as_ref())).await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "success");
        // Warp to (1280, 800) then restore to (10, 10): exactly two moves.
        assert_eq!(
            fake.moves.lock().unwrap().len(),
            2,
            "warp + restore: the cursor must return to its origin"
        );
        let last = fake.moves.lock().unwrap().last().copied().unwrap();
        assert_eq!(last.x, 10.0, "restore must return to the before-position");
        assert_eq!(last.y, 10.0);
        let p = runs[0].pointer.clone().unwrap();
        assert_eq!(p.backend, "physical");
        assert!(!p.isolated);
        assert!(p.physical_cursor_moved);
        assert_eq!(p.physical_cursor_restored, Some(true));
        assert_eq!(p.human_input_during_fallback, Some(false));
    }

    #[tokio::test]
    async fn physical_fallback_does_not_click_when_human_grabs_during_warp() {
        // P0-5: the user grabbed the mouse DURING the warp. The click must NOT
        // be posted (human after warp → DO NOT CLICK) and the cursor must NOT
        // be restored (only the warp move happened).
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        // The warp itself triggers a real human event mid-path (user grab).
        fake.human_grab_during_warp
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *fake.human.lock().unwrap() = Some(human.clone());
        let runs = run_physical_click(fake.as_ref(), Some(human.as_ref())).await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(
            fake.moves.lock().unwrap().len(),
            1,
            "only the warp — no restore after a grab"
        );
        assert_eq!(
            fake.clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "human after warp → DO NOT CLICK"
        );
        // P0-4: the real interrupt telemetry rides the failure detail so the
        // interrupt is never lost.
        let err = runs[0]
            .error
            .as_deref()
            .expect("a failed action carries an error");
        assert!(err.contains("cancelled by user takeover"), "got: {err}");
        assert!(
            err.contains("event_detection_latency_ms:"),
            "detection latency lost from the failure detail: {err}"
        );
        assert!(
            err.contains("human_to_input_stop_ms:"),
            "input-stop latency lost from the failure detail: {err}"
        );
        // Audit G: the physical fallback stamped synthetic events (the warp).
        assert!(
            human.synthetic_count() > 0,
            "the physical fallback must stamp synthetic input events"
        );
    }

    #[tokio::test]
    async fn physical_fallback_never_restores_when_human_grabs_after_click() {
        // P0-5: the click landed, THEN the user grabbed the mouse. The click
        // is reported (it did happen), but the cursor must NEVER be yanked
        // back — no restore (human after click → DO NOT RESTORE).
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        fake.human_grab_at_click
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *fake.human.lock().unwrap() = Some(human.clone());
        let runs = run_physical_click(fake.as_ref(), Some(human.as_ref())).await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "success", "the click did land");
        assert_eq!(
            fake.clicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the click was posted"
        );
        assert_eq!(
            fake.moves.lock().unwrap().len(),
            1,
            "warp only — no restore after a grab"
        );
        let p = runs[0].pointer.clone().unwrap();
        assert_eq!(p.backend, "physical");
        assert_eq!(p.physical_cursor_restored, Some(false));
        assert_eq!(p.human_input_during_fallback, Some(true));
        // P0-4: the KPI is real — the click stamp preceded the grab, so the
        // agent had already stopped: 0. Detection latency is carried too.
        assert_eq!(p.human_to_input_stop_ms, Some(0));
        assert!(
            p.event_detection_latency_ms.is_some(),
            "a human interrupt must carry detection latency"
        );
    }

    #[tokio::test]
    async fn human_to_input_stop_is_real_number_when_synthetic_follows_human() {
        // Directly exercise the P0-4 KPI the detail string carries: a real
        // human event followed by a LATE synthetic stamp yields a positive
        // `human_to_input_stop_ms` (how long after the user's input the agent's
        // LAST input landed). A human event with NO synthetic after it yields
        // exactly 0 — never None, never negative.
        let m = HumanInputMonitor::new();
        assert_eq!(m.human_to_input_stop_ms(), None);
        m.record_human_event(std::time::Instant::now());
        std::thread::sleep(std::time::Duration::from_millis(2));
        m.record_synthetic_event(std::time::Instant::now());
        let lat = m.human_to_input_stop_ms();
        assert!(
            lat.is_some(),
            "a late synthetic after the grab must carry a stop latency"
        );
        assert!(lat.unwrap() >= 1, "a 2ms gap must not read as 0ms");
        // A human event with no synthetic after it: exactly 0.
        let m2 = HumanInputMonitor::new();
        m2.record_human_event(std::time::Instant::now());
        assert_eq!(m2.human_to_input_stop_ms(), Some(0));
    }

    #[tokio::test]
    async fn physical_fallback_refuses_while_user_actively_operating() {
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        // The user just operated the mouse. Clear the takeover flag so the
        // batch itself proceeds — the guard under test is the fallback's own
        // refusal to borrow the cursor while the user is actively operating
        // (defense-in-depth independent of the batch-level takeover flag).
        human.record_human_event(std::time::Instant::now());
        assert!(human.consume_real_takeover());
        let runs = run_physical_click(fake.as_ref(), Some(human.as_ref())).await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("HUMAN_TAKEOVER"));
        assert_eq!(
            fake.moves.lock().unwrap().len(),
            0,
            "the real cursor must never be borrowed while the user is active"
        );
    }

    /// P0-7: helper — a session scoped to a resolved target window (or not),
    /// run through the queue under `physical_allowed` so Drag / located Scroll
    /// pass the pointer-policy gate and reach the TARGET isolation checks.
    async fn run_targeted(
        fake: &dyn ComputerDriver,
        resolved: Option<cu_driver::ResolvedSessionTarget>,
        actions: Vec<ComputerAction>,
    ) -> Vec<ActionRun> {
        let queue = ActionQueue::new(fake);
        let session = std::sync::Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            cu_core::SecretTokenHash::from_token(&cu_core::generate_observation_token()),
            None,
        ));
        session.set_pointer_policy(PointerPolicy::PhysicalAllowed);
        session.set_resolved_target(resolved);
        let geometry = ImageGeometry {
            image_width_px: 1280,
            image_height_px: 800,
            display_bounds: cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
        };
        let token = CancellationToken::new();
        let mut takeover = TakeoverDetector {
            policy: TakeoverPolicy::AutoPause,
            ..Default::default()
        };
        queue
            .run(
                &session,
                &actions,
                &geometry,
                token,
                &mut takeover,
                None,
                None,
                None,
                "f",
                "1",
                None,
                (),
                Some("active"),
            )
            .await
            .unwrap()
    }

    // ------------------------------------------------------------------
    // Round 8 / P0-1: Target Occlusion Guard (window_at_point hit-test)
    // ------------------------------------------------------------------

    /// The resolved target window every occlusion test clicks inside
    /// (bounds (0,0,400,300) contains the normalized (100,100) → (128,80)).
    fn occlusion_target() -> Option<cu_driver::ResolvedSessionTarget> {
        Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 42,
            window_id: 7,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 400.0,
                height: 300.0,
            }),
        })
    }

    /// P0-1: run action(s) through a target-scoped session under
    /// `physical_allowed` — the strictest occlusion case, because the physical
    /// fallback must NOT run even when it is explicitly permitted.
    async fn run_occlusion(
        fake: &dyn ComputerDriver,
        resolved: Option<cu_driver::ResolvedSessionTarget>,
        actions: Vec<ComputerAction>,
        human: Option<&HumanInputMonitor>,
    ) -> Vec<ActionRun> {
        let queue = ActionQueue::new(fake);
        let session = std::sync::Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            cu_core::SecretTokenHash::from_token(&cu_core::generate_observation_token()),
            None,
        ));
        session.set_pointer_policy(PointerPolicy::PhysicalAllowed);
        session.set_resolved_target(resolved);
        let geometry = ImageGeometry {
            image_width_px: 1280,
            image_height_px: 800,
            display_bounds: cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0,
            },
        };
        let token = CancellationToken::new();
        let mut takeover = TakeoverDetector {
            policy: TakeoverPolicy::AutoPause,
            ..Default::default()
        };
        queue
            .run(
                &session,
                &actions,
                &geometry,
                token,
                &mut takeover,
                human,
                None,
                None,
                "f",
                "1",
                None,
                (),
                Some("active"),
            )
            .await
            .unwrap()
    }

    fn occlusion_click() -> ComputerAction {
        ComputerAction::Click {
            x: 100.0,
            y: 100.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        }
    }

    fn occlusion_double_click() -> ComputerAction {
        ComputerAction::DoubleClick {
            x: 100.0,
            y: 100.0,
            button: MouseButton::Left,
            coordinate_space: CoordinateSpace::Normalized1000,
        }
    }

    /// Case A: the target IS the topmost window at the point → Direct CG is
    /// permitted and used.
    #[tokio::test]
    async fn occlusion_case_a_target_topmost_allows_direct_cg() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 7,
            pid: 42,
            bundle_id: "com.example.Target".into(),
        });
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "success");
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Case A: the target IS topmost → Direct CG click permitted"
        );
        assert_eq!(fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            fake.physical_clicks
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let p = runs[0].pointer.clone().unwrap();
        assert_eq!(p.backend, "direct_cg_event");
    }

    /// Case B: another window covers the target at the point → Direct CG is
    /// forbidden; a single click retries a target-PID-scoped AXPress; when AX
    /// cannot realize it, the action fails closed with TARGET_OCCLUDED.
    #[tokio::test]
    async fn occlusion_case_b_occluded_single_ax_fails_closes() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 99,
            pid: 9,
            bundle_id: "com.other.Window".into(),
        });
        fake.ax_succeeds
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_OCCLUDED"));
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "occluded → Direct CG forbidden, even though the point is inside the target's bounds"
        );
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "occluded single click retries target-pid AXPress"
        );
        assert_eq!(
            fake.physical_clicks
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "physical fallback NEVER runs under occlusion, even under physical_allowed"
        );
    }

    /// Case B2: occluded single click where AX CAN realize it → success via
    /// the accessibility backend (isolated, no cursor movement, no Direct CG).
    #[tokio::test]
    async fn occlusion_case_b2_occluded_single_ax_succeeds() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 99,
            pid: 9,
            bundle_id: "com.other.Window".into(),
        });
        fake.ax_succeeds
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "success");
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "occluded → Direct CG never used even when AX succeeds"
        );
        assert_eq!(fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let p = runs[0].pointer.clone().unwrap();
        assert_eq!(p.backend, "accessibility");
    }

    /// Case C: an occluded DOUBLE-click never degrades to a single AXPress and
    /// never reaches the physical backend — even under `physical_allowed` — and
    /// fails closed with TARGET_OCCLUDED.
    #[tokio::test]
    async fn occlusion_case_c_occluded_double_click_never_degrades_never_physical() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 99,
            pid: 9,
            bundle_id: "com.other.Window".into(),
        });
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_double_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_OCCLUDED"));
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a double-click is NEVER degraded to a single AXPress"
        );
        assert_eq!(
            fake.physical_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "physical double-click NEVER runs when the target is occluded — even under physical_allowed"
        );
    }

    /// Case D: no normal window at the point → Direct CG forbidden (fail
    /// closed); the single-click AX retry still fails closed to TARGET_OCCLUDED.
    #[tokio::test]
    async fn occlusion_case_d_no_window_at_point_fails_closed() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = None;
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_OCCLUDED"));
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a click into the void is never sent as Direct CG"
        );
        assert_eq!(
            fake.physical_clicks
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    /// Case D2: the hit-test itself fails (driver error) → Direct CG forbidden
    /// (fail closed); the target's topmost-ness is unverifiable.
    #[tokio::test]
    async fn occlusion_case_d2_hit_test_error_fails_closed() {
        let fake = Arc::new(OcclusionDriver::default());
        fake.hit_test_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_OCCLUDED"));
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "unverifiable topmost-ness → never click through"
        );
    }

    /// Case E: a NON-target session is never occluded — there is no window to
    /// compare against, so Direct CG proceeds exactly as before.
    #[tokio::test]
    async fn occlusion_case_e_non_target_session_not_guarded() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 99,
            pid: 9,
            bundle_id: "com.other.Window".into(),
        });
        let runs = run_occlusion(fake.as_ref(), None, vec![occlusion_click()], None).await;
        assert_eq!(runs[0].status, "success");
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "non-target session → Direct CG unaffected by the hit-test"
        );
        let p = runs[0].pointer.clone().unwrap();
        assert_eq!(p.backend, "direct_cg_event");
    }

    /// Case F: window_id + pid match the target but the bundle differs — the
    /// auxiliary bundle check blocks (fail closed) rather than clicking through.
    #[tokio::test]
    async fn occlusion_case_f_bundle_mismatch_blocks_even_with_matching_ids() {
        let fake = Arc::new(OcclusionDriver::default());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 7,
            pid: 42,
            bundle_id: "com.other.Window".into(),
        });
        fake.ax_succeeds
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            None,
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_OCCLUDED"));
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "auxiliary bundle mismatch blocks Direct CG even with matching window_id+pid"
        );
    }

    /// P0-3: the user grabbed DURING the occlusion hit-test — before any emit.
    /// The Direct CG down/up must NOT be posted (direct_clicks == 0), no
    /// synthetic timestamp may be stamped (nothing was emitted), and the
    /// action fails with the real interrupt telemetry.
    #[tokio::test]
    async fn p03_direct_cg_suppressed_when_human_grabs_before_emit() {
        let fake = Arc::new(OcclusionDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        *fake.human.lock().unwrap() = Some(human.clone());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 7,
            pid: 42,
            bundle_id: "com.example.Target".into(),
        });
        fake.human_grab_in_hit_test
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            Some(human.as_ref()),
        )
        .await;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        let err = runs[0]
            .error
            .as_deref()
            .expect("a failed action carries an error");
        assert!(err.contains("cancelled by user takeover"), "got: {err}");
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a Direct CG click must never be posted after a human grab"
        );
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no fallback may run after a human grab"
        );
        assert_eq!(
            human.synthetic_count(),
            0,
            "no real input was emitted → no synthetic stamp"
        );
    }

    /// P0-3: under occlusion, a human grab during the hit-test must also
    /// suppress the isolated AXPress retry — never emit input after a human
    /// event through any backend.
    #[tokio::test]
    async fn p03_occluded_ax_suppressed_when_human_grabs_before_emit() {
        let fake = Arc::new(OcclusionDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        *fake.human.lock().unwrap() = Some(human.clone());
        // A DIFFERENT window is topmost → Blocked → single clicks may retry
        // via the isolated AXPress backend.
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 99,
            pid: 9,
            bundle_id: "com.other.Window".into(),
        });
        fake.human_grab_in_hit_test
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            Some(human.as_ref()),
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        let err = runs[0]
            .error
            .as_deref()
            .expect("a failed action carries an error");
        assert!(err.contains("cancelled by user takeover"), "got: {err}");
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "AXPress must never run after a human grab"
        );
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Direct CG must never run under occlusion"
        );
    }

    /// P0-3 core race fix: a Direct CG click that FAILED (or was cancelled)
    /// DURING the emit — the human grabbed inside `execute_with_cancel` — must
    /// NOT degrade to the AXPress fallback. The old code fell through to AX on
    /// any `Ok(_)`, so a cancelled Direct CG click would emit AX input after
    /// the human event.
    #[tokio::test]
    async fn p03_cancelled_direct_cg_does_not_degrade_to_axpress() {
        let fake = Arc::new(OcclusionDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        *fake.human.lock().unwrap() = Some(human.clone());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 7,
            pid: 42,
            bundle_id: "com.example.Target".into(),
        });
        fake.direct_click_succeeds
            .store(false, std::sync::atomic::Ordering::SeqCst);
        fake.human_grab_at_direct
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            Some(human.as_ref()),
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        let err = runs[0]
            .error
            .as_deref()
            .expect("a failed action carries an error");
        assert!(err.contains("cancelled by user takeover"), "got: {err}");
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a cancelled Direct CG click must NOT fall through to AXPress"
        );
        assert_eq!(
            fake.physical_clicks
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a cancelled Direct CG click must NOT fall through to physical"
        );
        assert_eq!(
            human.synthetic_count(),
            0,
            "single-click: nothing landed, so nothing is stamped"
        );
    }

    /// P0-3: a DOUBLE-click cancelled during the emit (state-1 pair already
    /// landed) stamps exactly ONE synthetic event — the real single click that
    /// DID land — but never degrades to AX / physical.
    #[tokio::test]
    async fn p03_double_click_cancelled_after_state1_stamps_the_landed_click() {
        let fake = Arc::new(OcclusionDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        *fake.human.lock().unwrap() = Some(human.clone());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 7,
            pid: 42,
            bundle_id: "com.example.Target".into(),
        });
        fake.direct_click_succeeds
            .store(false, std::sync::atomic::Ordering::SeqCst);
        fake.human_grab_at_direct
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_double_click()],
            Some(human.as_ref()),
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        let err = runs[0]
            .error
            .as_deref()
            .expect("a failed action carries an error");
        assert!(err.contains("cancelled by user takeover"), "got: {err}");
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a double-click must NEVER degrade to single AXPress"
        );
        assert_eq!(
            fake.physical_clicks
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a cancelled double-click must not borrow the physical cursor"
        );
        assert_eq!(
            human.synthetic_count(),
            1,
            "the state-1 single click that landed is stamped exactly once"
        );
    }

    /// P0-3: a successful Direct CG click stamps exactly ONE synthetic event,
    /// and the stamp happens AFTER the emit (the click landed).
    #[tokio::test]
    async fn p03_successful_direct_cg_stamps_one_synthetic_after_emit() {
        let fake = Arc::new(OcclusionDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        *fake.human.lock().unwrap() = Some(human.clone());
        *fake.topmost.lock().unwrap() = Some(cu_driver::WindowAtPoint {
            window_id: 7,
            pid: 42,
            bundle_id: "com.example.Target".into(),
        });
        let runs = run_occlusion(
            fake.as_ref(),
            occlusion_target(),
            vec![occlusion_click()],
            Some(human.as_ref()),
        )
        .await;
        assert_eq!(runs[0].status, "success");
        assert_eq!(
            fake.direct_clicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the Direct CG click ran"
        );
        assert_eq!(
            human.synthetic_count(),
            1,
            "exactly one synthetic stamp for one landed click"
        );
        assert!(
            human.human_to_input_stop_ms().is_none() || human.human_to_input_stop_ms() == Some(0),
            "with no human event, the KPI stays None/0"
        );
    }

    /// P0-7a: a Drag whose START point is outside the target window is refused
    /// even when the end point is inside — the real cursor would otherwise
    /// sweep across other apps before entering the window.
    #[tokio::test]
    async fn target_isolation_requires_drag_start_inside_window() {
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let window = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 42,
            window_id: 7,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 400.0,
                height: 300.0,
            }),
        });
        // from (500,500) -> global (640,400): x=640 OUTSIDE the window (x<400).
        // to (200,200) -> global (256,160): inside. Old code only checked `to`.
        let runs = run_targeted(
            fake.as_ref(),
            window,
            vec![ComputerAction::Drag {
                from: cu_core::Point::new(500.0, 500.0),
                to: cu_core::Point::new(200.0, 200.0),
                coordinate_space: CoordinateSpace::Normalized1000,
                duration_ms: None,
            }],
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_OUTSIDE_SESSION"));
    }

    /// P0-7a: a Drag with BOTH endpoints inside the window executes normally.
    #[tokio::test]
    async fn target_isolation_allows_drag_fully_inside_window() {
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let window = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 42,
            window_id: 7,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 400.0,
                height: 300.0,
            }),
        });
        // from (100,100) -> (128,80), to (200,200) -> (256,160): both inside.
        let runs = run_targeted(
            fake.as_ref(),
            window,
            vec![ComputerAction::Drag {
                from: cu_core::Point::new(100.0, 100.0),
                to: cu_core::Point::new(200.0, 200.0),
                coordinate_space: CoordinateSpace::Normalized1000,
                duration_ms: None,
            }],
        )
        .await;
        assert_eq!(runs[0].status, "success");
    }

    /// P0-7b: a resolved target whose window bounds are UNKNOWN (off-screen /
    /// stale) still means "scoped to a target" — a locationless scroll is
    /// refused because it would land at the current pointer, possibly in a
    /// different app. Gating on bounds (`Some`) alone would let it through.
    #[tokio::test]
    async fn target_isolation_refuses_locationless_scroll_without_known_bounds() {
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let window = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 42,
            window_id: 7,
            bounds: None,
        });
        let runs = run_targeted(
            fake.as_ref(),
            window,
            vec![ComputerAction::Scroll {
                x: None,
                y: None,
                delta_x: 0.0,
                delta_y: -10.0,
                coordinate_space: CoordinateSpace::Normalized1000,
            }],
        )
        .await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].error.as_deref(), Some("TARGET_COORDINATE_REQUIRED"));
    }

    /// Audit: the ghost cursor's pointer mode is switched to the
    /// physical-fallback state while the real cursor is borrowed, then
    /// restored. A clean transaction ends back at `Isolated`; one the user
    /// interrupted ends at `UserTakeover` (never a stale `PhysicalFallback`).
    #[tokio::test]
    async fn physical_fallback_toggles_ghost_mode_around_transaction() {
        // Clean fallback (no human grab): mode must return to Isolated.
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        let (session, runs) =
            run_physical_click_with_session(fake.as_ref(), Some(human.as_ref())).await;
        assert_eq!(runs[0].status, "success");
        assert_eq!(
            session.pointer_mode(),
            cu_core::PointerMode::Isolated,
            "a clean physical fallback must restore the ghost to the isolated state"
        );

        // Interrupted fallback (human grabs during the warp): P0-5 — no click,
        // no restore, and the ghost stays in the takeover state.
        let fake2 = Arc::new(PhysicalFallbackDriver::default());
        let human2 = Arc::new(HumanInputMonitor::new());
        fake2
            .human_grab_during_warp
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *fake2.human.lock().unwrap() = Some(human2.clone());
        let (session2, runs2) =
            run_physical_click_with_session(fake2.as_ref(), Some(human2.as_ref())).await;
        assert_eq!(runs2[0].status, "failed"); // no click was posted
        assert_eq!(
            session2.pointer_mode(),
            cu_core::PointerMode::UserTakeover,
            "a human interrupt must leave the ghost in the takeover state"
        );
    }

    // ------------------------------------------------------------------
    // P1: DoubleClick fallback keeps double-click semantics
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn double_click_physical_fallback_preserves_semantics() {
        // P1: a double-click whose Direct CG path failed must NEVER be degraded
        // to a single AXPress. The physical fallback (under physical_allowed)
        // is a real click-click — the driver's `physical_double_click_at`
        // posts the state-1 pair then the state-2 pair.
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let human = Arc::new(HumanInputMonitor::new());
        let (session, runs) = run_double_click_with_policy(
            fake.as_ref(),
            Some(human.as_ref()),
            PointerPolicy::PhysicalAllowed,
        )
        .await;
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].status, "success",
            "the physical double-click landed"
        );
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a double-click must NEVER be degraded to a single AXPress"
        );
        assert_eq!(
            fake.clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the physical fallback must NOT post two single clicks"
        );
        assert_eq!(
            fake.double_clicks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the physical fallback posts ONE click-click pair (state 1 then 2)"
        );
        assert_eq!(
            fake.moves.lock().unwrap().len(),
            2,
            "warp + restore: the cursor must return to its origin"
        );
        let p = runs[0].pointer.clone().unwrap();
        assert_eq!(p.backend, "physical");
        assert!(!p.isolated);
        assert!(p.physical_cursor_moved);
        assert_eq!(p.physical_cursor_restored, Some(true));
        assert_eq!(p.human_input_during_fallback, Some(false));
        assert_eq!(
            session.pointer_mode(),
            cu_core::PointerMode::Isolated,
            "a clean physical double-click restores the ghost to isolated"
        );
    }

    #[tokio::test]
    async fn double_click_fails_closed_when_physical_not_allowed() {
        // P1: without `physical_allowed`, the double-click fails CLOSED with
        // the explicit AX_UNSUPPORTED_FOR_DOUBLE_CLICK — it is never silently
        // degraded to a single AXPress or a single physical click.
        let fake = Arc::new(PhysicalFallbackDriver::default());
        let runs =
            run_double_click_with_policy(fake.as_ref(), None, PointerPolicy::IsolatedPreferred)
                .await
                .1;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "failed");
        assert_eq!(
            runs[0].error.as_deref(),
            Some("AX_UNSUPPORTED_FOR_DOUBLE_CLICK")
        );
        assert_eq!(
            fake.ax_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "never a single AXPress, even when failing closed"
        );
        assert_eq!(
            fake.double_clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no physical input without physical_allowed"
        );
        assert_eq!(
            fake.clicks.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no single click either"
        );
        assert_eq!(fake.moves.lock().unwrap().len(), 0, "no cursor borrowed");
    }
}
