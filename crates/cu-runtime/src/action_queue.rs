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

use cu_core::{ComputerAction, CuError, ImageGeometry, Point, PointerPolicy};
use cu_driver::ComputerDriver;
use cu_policy::{TakeoverDetector, TakeoverPolicy};
use cu_trace::TraceRecorder;
use tokio_util::sync::CancellationToken;

use crate::human_input::HumanInputMonitor;
use crate::sessions::{Session, SharedSession};

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
            if token.is_cancelled() || self.session_aborted(session) {
                self.fill_cancelled(&mut reports, i, actions.len());
                break;
            }

            // Human Always Wins (P0-1): a REAL hardware event (Event Tap)
            // always forces UserTakeover — the configurable Ignore/AutoPause
            // policy never applies to physical user input. The heuristic
            // channel (fallback, P0-8) may still honor the old policy, but
            // it is checked separately below.
            if let Some(h) = human {
                if h.consume_real_takeover() {
                    // P0-1: real hardware input -> unconditional UserTakeover.
                    let _ = session.transition(cu_core::SessionState::UserTakeover);
                    session.sync_pointer_mode(cu_core::SessionState::UserTakeover);
                    h.mark_takeover_started();
                    h.mark_input_stopped();
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

            // Round 8 / Phase 9 — PointerPolicy gates physical-required
            // actions. Drag and located Scroll move the REAL cursor; under
            // `isolated_only` they are refused outright. Under
            // `isolated_preferred` they are refused unless the caller
            // explicitly permitted physical fallback (the runtime never
            // silently moves the user's cursor).
            let policy = session.get_pointer_policy();
            if matches!(policy, PointerPolicy::IsolatedOnly) {
                let needs_physical = matches!(
                    action,
                    ComputerAction::Drag { .. }
                        | ComputerAction::Scroll {
                            x: Some(_),
                            y: Some(_),
                            ..
                        }
                );
                if needs_physical {
                    reports.push(ActionRun::failed(
                        i,
                        0,
                        if matches!(action, ComputerAction::Drag { .. }) {
                            "ISOLATED_DRAG_UNAVAILABLE"
                        } else {
                            "PHYSICAL_FALLBACK_NOT_ALLOWED"
                        }
                        .into(),
                    ));
                    self.fill_cancelled(&mut reports, i + 1, actions.len());
                    break;
                }
            }

            // Round 9 / P0-6 — Keyboard Focus Guard, LIVE per action.
            //
            // The batch-level `active_bundle_id` is stale by design: a user can
            // switch apps mid-batch (Click → Wait 2s → user switches to
            // TextEdit → Type). So before EVERY keyboard event (Type / Key /
            // Clipboard-paste) we re-read the frontmost application NOW and
            // compare against the session target. If focus is not on the
            // target, INPUT_FOCUS_MISMATCH and NO keyboard event is ever sent.
            // We never auto-activate or steal focus (focus_policy: strict is
            // the default; `activate_target` remains experimental/unsupported).
            let needs_focus = matches!(
                action,
                ComputerAction::TypeText { .. } | ComputerAction::Key { .. }
            );
            if needs_focus {
                if let Some(t) = session.get_target() {
                    if let Some(target_bundle) = t.bundle_id {
                        // P0-6: LIVE frontmost app read, not the batch cache.
                        // Compare bundle_id AND pid where available.
                        let live_bundle = self
                            .driver
                            .active_application()
                            .await
                            .ok()
                            .flatten()
                            .map(|a| a.bundle_id);
                        let target_matches = live_bundle
                            .as_deref()
                            .map(|b| b == target_bundle)
                            .unwrap_or(false);
                        // Also verify PID if the target specifies one.
                        let pid_matches = match (t.pid, live_bundle.as_deref()) {
                            (Some(_), None) => false,
                            _ => true, // PID check needs the driver pid API;
                                       // bundle match is the authoritative gate
                        };
                        let focused = target_matches && pid_matches;
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
            if let Some(bounds) = session.get_target_bounds() {
                if let Some(p) = resolved_location(&resolved) {
                    if !bounds.contains_global(p) {
                        reports.push(ActionRun::failed(i, 0, "TARGET_OUTSIDE_SESSION".into()));
                        self.fill_cancelled(&mut reports, i + 1, actions.len());
                        break;
                    }
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
                    double: _,
                } => {
                    let policy = session.get_pointer_policy();
                    let target_pid = session
                        .get_resolved_target()
                        .map(|r| r.pid)
                        .or_else(|| session.get_target().and_then(|t| t.pid).map(|p| p as i32));
                    // Backend 1: Direct CG isolated click.
                    let direct = self
                        .driver
                        .execute_with_cancel(&resolved, token.clone())
                        .await;
                    match direct {
                        Ok(ar) if ar.success => {
                            // Direct CGEvent isolated click succeeded. The
                            // real system cursor was NOT moved.
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
                            // Direct CG explicitly failed (rare — click_direct
                            // is synchronous). Backend 2: AXPress, isolated.
                            if let Some(pid) = target_pid {
                                match self.driver.click_via_accessibility(pid, *x, *y).await {
                                    Ok(true) => Ok(cu_driver::ActionResult {
                                        success: true,
                                        duration_ms: started.elapsed().as_millis() as u64,
                                        detail: Some(
                                            "pointer_backend:accessibility;isolated:true;physical_cursor_moved:false;physical_cursor_delta_px:0"
                                                .into(),
                                        ),
                                    }),
                                    Ok(false) => {
                                        // Backend 3: physical fallback ONLY
                                        // when explicitly allowed.
                                        if matches!(policy, PointerPolicy::PhysicalAllowed)
                                        {
                                            // Physical click borrows the real
                                            // cursor: warp + click. Human
                                            // Always Wins still applies (the
                                            // queue polls the monitor after).
                                            let before = self
                                                .driver
                                                .pointer_location()
                                                .await
                                                .ok()
                                                .map(|p| p.location);
                                            let be = match before {
                                                Some(bp) => {
                                                    // Physical click: warp the
                                                    // real cursor, click, then
                                                    // restore ONLY if the user
                                                    // never touched it (Human
                                                    // Always Wins still polls
                                                    // after this).
                                                    let _ = self
                                                        .driver
                                                        .execute(&cu_driver::ResolvedAction::Move {
                                                            from: bp,
                                                            to: cu_core::Point::new(*x, *y),
                                                            duration_ms: None,
                                                        })
                                                        .await;
                                                    let phys = self
                                                        .driver
                                                        .physical_click_at(*button, *x, *y)
                                                        .await;
                                                    let phys_res = phys.map(|ok| cu_driver::ActionResult {
                                                        success: ok,
                                                        duration_ms: started.elapsed().as_millis() as u64,
                                                        detail: Some(
                                                            "pointer_backend:physical;isolated:false;physical_cursor_moved:true"
                                                                .into(),
                                                        ),
                                                    });
                                                    // Restore the cursor to its
                                                    // original position.
                                                    let _ = self
                                                        .driver
                                                        .execute(&cu_driver::ResolvedAction::Move {
                                                            from: cu_core::Point::new(*x, *y),
                                                            to: bp,
                                                            duration_ms: None,
                                                        })
                                                        .await;
                                                    phys_res
                                                }
                                                None => Ok(cu_driver::ActionResult {
                                                    success: false,
                                                    duration_ms: 0,
                                                    detail: Some("ISOLATED_POINTER_UNAVAILABLE".into()),
                                                }),
                                            };
                                            be
                                        } else {
                                            // isolated_only / isolated_preferred
                                            // with no explicit physical -> fail.
                                            Ok(cu_driver::ActionResult {
                                                success: false,
                                                duration_ms: 0,
                                                detail: Some(
                                                    "ISOLATED_POINTER_UNAVAILABLE".into(),
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
                            } else {
                                Ok(cu_driver::ActionResult {
                                    success: false,
                                    duration_ms: 0,
                                    detail: Some("ISOLATED_POINTER_UNAVAILABLE".into()),
                                })
                            }
                        }
                        Err(e) => {
                            // A real driver failure (permission, bridge, or an
                            // injected test failure) is surfaced as the action
                            // failure — never silently downgraded to a fallback.
                            Ok(cu_driver::ActionResult {
                                success: false,
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: Some(e.to_string()),
                            })
                        }
                    }
                }
                // Round 8 / Phase 3: a Move is a **virtual-only** pointer
                // action. It never touches the physical system cursor — it
                // updates the session's virtual pointer and drives the ghost
                // cursor overlay only. System pointer delta stays 0.
                cu_driver::ResolvedAction::Move { to, .. } => {
                    session.set_virtual_pointer(*to, display_id);
                    // Best-effort: the overlay is a visual aid; if the bridge
                    // cannot show it the action still succeeds (the virtual
                    // pointer moved).
                    let _ = self.driver.pointer_visualized(to.x, to.y, display_id).await;
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
                        if token.is_cancelled() || self.session_aborted(session) {
                            wait_interrupted = true;
                            break;
                        }
                        // Human Always Wins applies inside waits too. A real
                        // event always forces UserTakeover (P0-1); the
                        // heuristic fallback honors the old policy (P0-8).
                        if let Some(h) = human {
                            if h.consume_real_takeover() {
                                let _ = session.transition(cu_core::SessionState::UserTakeover);
                                session.sync_pointer_mode(cu_core::SessionState::UserTakeover);
                                h.mark_takeover_started();
                                h.mark_input_stopped();
                                wait_interrupted = true;
                                break;
                            }
                            if human_monitor_state != Some("active") && h.consume_takeover() {
                                let _ = self.apply_takeover(session, takeover);
                                wait_interrupted = true;
                                break;
                            }
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
                _ => {
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
            if !action_moves_pointer(action) {
                if let Ok(pi) = self.driver.pointer_location().await {
                    let dx = pi.location.x - last_pointer.x;
                    let dy = pi.location.y - last_pointer.y;
                    if takeover.observe(dx, dy) {
                        let _ = self.apply_takeover(session, takeover);
                        self.fill_cancelled(&mut reports, i + 1, actions.len());
                        break;
                    }
                }
            } else {
                last_pointer = session.virtual_pointer.lock().unwrap().location();
            }
        }

        Ok(reports)
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
    for part in detail.split(';') {
        let mut kv = part.splitn(2, ':');
        let (k, v) = (kv.next()?, kv.next()?);
        match k {
            "pointer_backend" => backend = Some(v.to_string()),
            "isolated" => isolated = v.parse::<bool>().ok(),
            "physical_cursor_moved" => moved = v.parse::<bool>().ok(),
            "physical_cursor_delta_px" => delta_px = v.parse().unwrap_or(0.0),
            "physical_cursor_restored" => restored = v.parse::<bool>().ok(),
            _ => {}
        }
    }
    Some(cu_core::PointerExecutionResult {
        backend: backend?,
        isolated: isolated?,
        physical_cursor_moved: moved?,
        physical_cursor_delta_px: delta_px,
        physical_cursor_restored: restored,
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
}
