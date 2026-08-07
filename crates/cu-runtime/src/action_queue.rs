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

use cu_core::{ComputerAction, CuError, ImageGeometry, Point};
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
}

impl ActionRun {
    pub fn success(index: usize, duration_ms: u64) -> Self {
        Self {
            index,
            status: "success".into(),
            duration_ms,
            error: None,
        }
    }
    pub fn failed(index: usize, duration_ms: u64, error: String) -> Self {
        Self {
            index,
            status: "failed".into(),
            duration_ms,
            error: Some(error),
        }
    }
    pub fn cancelled(index: usize) -> Self {
        Self {
            index,
            status: "cancelled".into(),
            duration_ms: 0,
            error: None,
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
    ) -> Result<Vec<ActionRun>, CuError> {
        let mut reports = Vec::with_capacity(actions.len());
        // Baseline pointer: where the human (or a previous batch) left it.
        let mut last_pointer = self
            .driver
            .pointer_location()
            .await
            .map(|p| p.location)
            .unwrap_or(Point::new(0.0, 0.0));
        takeover.reset();

        for (i, action) in actions.iter().enumerate() {
            if token.is_cancelled() || self.session_aborted(session) {
                self.fill_cancelled(&mut reports, i, actions.len());
                break;
            }

            // Human Always Wins: if the continuous human-input monitor saw a
            // real user event since the last poll, stop immediately and hand
            // control to the user. Never pull the cursor back; never resume.
            if let Some(h) = human {
                if h.consume_takeover() {
                    let _ = self.apply_takeover(session, takeover);
                    self.fill_cancelled(&mut reports, i, actions.len());
                    break;
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
                        // Human Always Wins applies inside waits too.
                        if let Some(h) = human {
                            if h.consume_takeover() {
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
                _ => self.driver.execute(&resolved).await,
            };
            let duration_ms = started.elapsed().as_millis() as u64;

            let run = if wait_interrupted {
                ActionRun::cancelled(i)
            } else {
                match outcome {
                    Ok(ar) if ar.success => ActionRun::success(i, duration_ms),
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

            // Human-takeover probe. Only meaningful after an action that did
            // NOT move the pointer itself: our own Move/Click/Drag legitimately
            // relocates the pointer and must not count as a human grab.
            if !action_moves_pointer(action) {
                if let Ok(pi) = self.driver.pointer_location().await {
                    let dx = pi.location.x - last_pointer.x;
                    let dy = pi.location.y - last_pointer.y;
                    last_pointer = pi.location;
                    if takeover.observe(dx, dy) {
                        let _ = self.apply_takeover(session, takeover);
                        self.fill_cancelled(&mut reports, i + 1, actions.len());
                        break;
                    }
                }
            } else {
                last_pointer = match self.driver.pointer_location().await {
                    Ok(pi) => pi.location,
                    Err(_) => last_pointer,
                };
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
        })
        .collect()
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
