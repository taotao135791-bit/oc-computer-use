//! Mouse synthesis via CGEvent: click, double-click, pointer motion, smooth
//! drags, and pixel scrolling. All coordinates are **global logical points**
//! (the CGEvent space); the runtime converts model/image coordinates before
//! calling these functions.

use cu_core::{coordinates, MouseButton};
use tokio::time::{sleep, Duration};

use crate::ffi::*;

fn cg_button(button: MouseButton) -> i32 {
    match button {
        MouseButton::Left => MOUSE_BUTTON_LEFT,
        MouseButton::Right => MOUSE_BUTTON_RIGHT,
        MouseButton::Middle => MOUSE_BUTTON_MIDDLE,
    }
}

fn move_type_for_button(button: i32) -> u32 {
    match button {
        MOUSE_BUTTON_RIGHT => EVENT_RIGHT_MOUSE_DRAGGED,
        MOUSE_BUTTON_MIDDLE => EVENT_OTHER_MOUSE_DRAGGED,
        _ => EVENT_LEFT_MOUSE_DRAGGED,
    }
}

/// Move the pointer to a global point, instantly.
pub fn move_pointer(x: f64, y: f64) {
    let ev = create_mouse_event(EVENT_MOUSE_MOVED, CGPoint { x, y }, 0);
    post(&ev);
}

/// Move the pointer along a smooth path over `duration_ms` (0 = instant).
pub async fn move_pointer_smooth(
    from: cu_core::Point,
    to: cu_core::Point,
    duration_ms: Option<u64>,
) {
    let _ = move_pointer_smooth_cancel(from, to, duration_ms, None).await;
}

/// Cancel-aware variant: returns `false` when cancelled mid-path.
pub async fn move_pointer_smooth_cancel(
    from: cu_core::Point,
    to: cu_core::Point,
    duration_ms: Option<u64>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> bool {
    let duration = duration_ms.unwrap_or(0).min(5000);
    if duration == 0 {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                return false;
            }
        }
        move_pointer(to.x, to.y);
        return true;
    }
    let path = coordinates::move_path(from, to, duration);
    for (p, wait) in &path {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                return false;
            }
        }
        move_pointer(p.x, p.y);
        if *wait > 0 {
            let wait_ok = match cancel {
                Some(c) => tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(*wait)) => true,
                    () = c.cancelled() => false,
                },
                None => {
                    sleep(Duration::from_millis(*wait)).await;
                    true
                }
            };
            if !wait_ok {
                return false;
            }
        }
    }
    true
}

/// Press/release a mouse button at a global point.
pub fn button_down(button: MouseButton, x: f64, y: f64, click_state: i64) {
    let ev_type = match button {
        MouseButton::Left => EVENT_LEFT_MOUSE_DOWN,
        MouseButton::Right => EVENT_RIGHT_MOUSE_DOWN,
        MouseButton::Middle => EVENT_OTHER_MOUSE_DOWN,
    };
    let ev = create_mouse_event(ev_type, CGPoint { x, y }, cg_button(button));
    set_click_state(&ev, click_state);
    post(&ev);
}

pub fn button_up(button: MouseButton, x: f64, y: f64, click_state: i64) {
    let ev_type = match button {
        MouseButton::Left => EVENT_LEFT_MOUSE_UP,
        MouseButton::Right => EVENT_RIGHT_MOUSE_UP,
        MouseButton::Middle => EVENT_OTHER_MOUSE_UP,
    };
    let ev = create_mouse_event(ev_type, CGPoint { x, y }, cg_button(button));
    set_click_state(&ev, click_state);
    post(&ev);
}

/// A single click (down+up) at a global point.
pub fn click(button: MouseButton, x: f64, y: f64) {
    move_pointer(x, y);
    button_down(button, x, y, 1);
    button_up(button, x, y, 1);
}

/// **DirectPositionEvent click** (round 8, pointer isolation): posts mouse
/// down/up at the target position **without** first warping the system cursor.
///
/// This is the isolated actuator candidate: if macOS delivers the click to the
/// element under `(x, y)` while the visible system cursor stays where the user
/// left it, we can click anywhere without stealing the user's mouse. The
/// visible cursor is unchanged by `button_down`/`button_up` themselves (only
/// `EVENT_MOUSE_MOVED` relocates it).
pub fn click_direct(button: MouseButton, x: f64, y: f64) {
    button_down(button, x, y, 1);
    button_up(button, x, y, 1);
}

/// Direct double-click without warping the system cursor.
pub fn double_click_direct(button: MouseButton, x: f64, y: f64) {
    button_down(button, x, y, 1);
    button_up(button, x, y, 1);
    button_down(button, x, y, 2);
    button_up(button, x, y, 2);
}

/// A double-click at a global point. The second down/up pair carries click
/// state 2 so the system treats it as a real double-click.
pub fn double_click(button: MouseButton, x: f64, y: f64) {
    move_pointer(x, y);
    button_down(button, x, y, 1);
    button_up(button, x, y, 1);
    button_down(button, x, y, 2);
    button_up(button, x, y, 2);
}

/// P0-6: one physical mouse operation the driver emits, described neutrally so
/// the drag/scroll cancellation semantics can be verified WITHOUT hijacking
/// the user's pointer — tests inject a recording poster instead of posting to
/// CoreGraphics.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MouseOp {
    Move { x: f64, y: f64 },
    Down { x: f64, y: f64, click_state: i64 },
    Up { x: f64, y: f64, click_state: i64 },
    Dragged { x: f64, y: f64 },
    ScrollWheel { x: i32, y: i32 },
}

/// Production poster: forward a [`MouseOp`] to CoreGraphics.
fn post_op(op: MouseOp) {
    match op {
        MouseOp::Move { x, y } => move_pointer(x, y),
        MouseOp::Down { x, y, click_state } => button_down(MouseButton::Left, x, y, click_state),
        MouseOp::Up { x, y, click_state } => button_up(MouseButton::Left, x, y, click_state),
        MouseOp::Dragged { x, y } => {
            // Drag is left-button only; the mapping stays live for the
            // right/middle cases the FFI still exposes.
            let ev = create_mouse_event(
                move_type_for_button(MOUSE_BUTTON_LEFT),
                CGPoint { x, y },
                MOUSE_BUTTON_LEFT,
            );
            post(&ev);
        }
        MouseOp::ScrollWheel { x, y } => {
            let ev = create_scroll_event(y, x, true);
            post(&ev);
        }
    }
}

/// P0-6: true when the cancellation token has fired.
fn cancelled(cancel: Option<&tokio_util::sync::CancellationToken>) -> bool {
    cancel.map(|c| c.is_cancelled()).unwrap_or(false)
}

/// Smooth drag with the primary button held down from `from` to `to`.
/// P0-6: cancellation is checked BEFORE every physical event:
///   check → move to start → check → mouseDown → check → step → check → step…
/// A drag cancelled AFTER the mouse-down always sends the mouse-up at the
/// LAST ACTUAL drag point (never `to`, never clamped — multi-monitor negative
/// coordinates are legal), so the button is never left stuck.
pub async fn drag(
    from: cu_core::Point,
    to: cu_core::Point,
    duration_ms: Option<u64>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> bool {
    drag_core(from, to, duration_ms, cancel, &post_op).await
}

/// Cancellation-aware drag with an injected poster (P0-6). The real
/// [`drag`] posts to CoreGraphics; tests capture the exact op sequence.
async fn drag_core(
    from: cu_core::Point,
    to: cu_core::Point,
    duration_ms: Option<u64>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    post: &(dyn Fn(MouseOp) + Sync),
) -> bool {
    let duration = duration_ms.unwrap_or(400).min(10_000);
    // P0-6: check BEFORE the first physical event. The user already took over
    // (or the batch was cancelled) — never touch the mouse at all.
    if cancelled(cancel) {
        return false;
    }
    post(MouseOp::Move {
        x: from.x,
        y: from.y,
    });
    // P0-6: check between the move-to-start and the mouse-down. No button is
    // held yet, so no mouse-up is needed — just stop.
    if cancelled(cancel) {
        return false;
    }
    post(MouseOp::Down {
        x: from.x,
        y: from.y,
        click_state: 1,
    });
    let path = coordinates::drag_path(from, to, duration, 90.0);
    let mut aborted = false;
    // P0-6: on cancel the mouse-up fires at the LAST ACTUAL drag point — never
    // at `to` (which may be unreached) and never clamped to 0 (multi-monitor
    // negative coordinates are legal).
    let mut last_actual = from;
    for (p, wait) in &path {
        if cancelled(cancel) {
            aborted = true;
            break;
        }
        post(MouseOp::Dragged { x: p.x, y: p.y });
        last_actual = *p;
        if *wait > 0 {
            let wait_ok = match cancel {
                Some(c) => tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(*wait)) => true,
                    () = c.cancelled() => false,
                },
                None => {
                    sleep(Duration::from_millis(*wait)).await;
                    true
                }
            };
            if !wait_ok {
                aborted = true;
                break;
            }
        }
    }
    // Mouse-down happened: ALWAYS send mouse-up so the system is never left
    // with a stuck pressed button (P0-2). P0-6: cancelled → up at the last
    // actual drag point; completed → up exactly at `to`.
    let up = drag_up_position(aborted, to, last_actual);
    post(MouseOp::Up {
        x: up.x,
        y: up.y,
        click_state: 1,
    });
    !aborted
}

/// The release point for the mouse-up that MUST follow a `drag` (P0-2: "never
/// leave a stuck pressed button"). P0-6: when cancelled mid-path the up fires
/// at the LAST ACTUAL drag point — never at `to` (which may be unreached) and
/// never clamped to 0 (multi-monitor negative coordinates are legal); on
/// completion it lands exactly at `to`. Extracted so the invariant is
/// testable without posting real events (tests never touch the user's
/// pointer).
fn drag_up_position(
    cancelled: bool,
    to: cu_core::Point,
    last_actual: cu_core::Point,
) -> cu_core::Point {
    if cancelled {
        last_actual
    } else {
        to
    }
}

/// Scroll by pixel deltas, cancellation-aware (round 9 / P0-2): long scrolls
/// are split into chunks and check the token between chunks; never sends one
/// uninterruptible burst. P0-6: the token is checked BEFORE the first physical
/// event (the move to `at`) and again between the move and the first scroll
/// event — a grab mid-move suppresses the scroll (never move once then exit).
/// Returns `true` on completion, `false` on cancel.
pub async fn scroll(
    delta_x: f64,
    delta_y: f64,
    at: Option<cu_core::Point>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> bool {
    scroll_core(delta_x, delta_y, at, cancel, &post_op).await
}

/// Cancellation-aware scroll with an injected poster (P0-6). The real
/// [`scroll`] posts to CoreGraphics; tests capture the exact op sequence.
async fn scroll_core(
    delta_x: f64,
    delta_y: f64,
    at: Option<cu_core::Point>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    post: &(dyn Fn(MouseOp) + Sync),
) -> bool {
    // P0-6: check BEFORE the first physical event (the move to `at`).
    if cancelled(cancel) {
        return false;
    }
    if let Some(p) = at {
        post(MouseOp::Move { x: p.x, y: p.y });
        // P0-6: check between the move and the first scroll event — a grab
        // mid-move must suppress the scroll (never move once then exit).
        if cancelled(cancel) {
            return false;
        }
    }
    // Accumulate total in each axis, post in ~30px chunks.
    let chunk = 30.0;
    let total_x = delta_x.round() as i64;
    let total_y = delta_y.round() as i64;
    let mut remaining_x = total_x;
    let mut remaining_y = total_y;
    while remaining_x != 0 || remaining_y != 0 {
        if cancelled(cancel) {
            return false;
        }
        let dx = remaining_x.clamp(-chunk as i64, chunk as i64);
        let dy = remaining_y.clamp(-chunk as i64, chunk as i64);
        post(MouseOp::ScrollWheel {
            x: dx as i32,
            y: dy as i32,
        });
        remaining_x -= dx;
        remaining_y -= dy;
        if remaining_x != 0 || remaining_y != 0 {
            let wait_ok = match cancel {
                Some(c) => tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(8)) => true,
                    () = c.cancelled() => false,
                },
                None => {
                    sleep(Duration::from_millis(8)).await;
                    true
                }
            };
            if !wait_ok {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    // Construction-level tests only; we never post events in tests so the CI
    // does not hijack the user's pointer.
    use super::*;

    #[test]
    fn drag_type_per_button() {
        assert_eq!(
            move_type_for_button(MOUSE_BUTTON_LEFT),
            EVENT_LEFT_MOUSE_DRAGGED
        );
        assert_eq!(
            move_type_for_button(MOUSE_BUTTON_RIGHT),
            EVENT_RIGHT_MOUSE_DRAGGED
        );
        assert_eq!(
            move_type_for_button(MOUSE_BUTTON_MIDDLE),
            EVENT_OTHER_MOUSE_DRAGGED
        );
    }

    #[test]
    fn drag_always_releases_the_button_after_cancel() {
        // Section 三十八 test 2 / P0-2: a drag cancelled mid-path must still
        // send the matching mouse-up (the pressed button must never be left
        // stuck). This pins the release-point decision without posting events
        // — tests must never hijack the user's pointer in CI.
        let to = cu_core::Point::new(300.0, 200.0);
        let last_actual = cu_core::Point::new(150.0, 120.0);
        // Completed drag: the up lands exactly at the destination.
        assert_eq!(drag_up_position(false, to, last_actual), to);
        // Cancelled drag: the up fires at the LAST ACTUAL drag point — never
        // the (unreached) destination.
        assert_eq!(drag_up_position(true, to, last_actual), last_actual);
    }

    /// P0-6: a cancelled drag releases at the LAST ACTUAL drag point. On a
    /// multi-monitor layout the drag can legitimately sit at NEGATIVE global
    /// coordinates — the up must land there, never clamped back onto a single
    /// desktop (the old `max(0, …)` behaviour would yank the up to the wrong
    /// screen).
    #[test]
    fn drag_cancel_release_preserves_negative_monitor_coordinates() {
        let to = cu_core::Point::new(300.0, 200.0);
        // Second monitor to the left: negative global x/y are legal.
        let last_actual = cu_core::Point::new(-120.0, -40.0);
        assert_eq!(
            drag_up_position(true, to, last_actual),
            last_actual,
            "the cancel release must preserve negative monitor coordinates"
        );
        // A partially-negative actual point is preserved too.
        let last_actual = cu_core::Point::new(-120.0, 40.0);
        assert_eq!(drag_up_position(true, to, last_actual), last_actual);
    }

    /// P0-6: a token cancelled BEFORE the first physical event must prevent
    /// ANY mouse op — no move, no down, no up. The user already took over.
    #[tokio::test]
    async fn drag_cancelled_before_first_event_never_touches_mouse() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let posted: std::sync::Arc<std::sync::Mutex<Vec<MouseOp>>> = Default::default();
        let posted2 = posted.clone();
        let post = move |op| posted2.lock().unwrap().push(op);
        let ok = drag_core(
            cu_core::Point::new(10.0, 10.0),
            cu_core::Point::new(100.0, 100.0),
            Some(100),
            Some(&token),
            &post,
        )
        .await;
        assert!(!ok);
        assert!(
            posted.lock().unwrap().is_empty(),
            "no physical event may fire when cancelled before the first one"
        );
    }

    /// P0-6: a drag cancelled AFTER the mouse-down must send the mouse-up at
    /// the LAST ACTUAL drag point (the last posted dragged event), never at
    /// the (unreached) destination.
    #[tokio::test]
    async fn drag_cancelled_after_mouse_down_releases_at_last_actual_point() {
        let token = tokio_util::sync::CancellationToken::new();
        let posted: std::sync::Arc<std::sync::Mutex<Vec<MouseOp>>> = Default::default();
        let posted2 = posted.clone();
        let token2 = token.clone();
        let post = move |op: MouseOp| {
            // Simulate the user grabbing the mouse on the FIRST dragged step.
            if matches!(op, MouseOp::Dragged { .. }) {
                token2.cancel();
            }
            posted2.lock().unwrap().push(op);
        };
        let ok = drag_core(
            cu_core::Point::new(10.0, 10.0),
            cu_core::Point::new(100.0, 100.0),
            Some(50),
            Some(&token),
            &post,
        )
        .await;
        assert!(!ok);
        let ops = posted.lock().unwrap();
        // Move to start, Down, ≥1 Dragged, then Up at the last actual point.
        assert!(ops.len() >= 4, "unexpected op sequence: {ops:?}");
        assert!(matches!(ops[0], MouseOp::Move { x: 10.0, y: 10.0 }));
        assert!(matches!(
            ops[1],
            MouseOp::Down {
                x: 10.0,
                y: 10.0,
                click_state: 1
            }
        ));
        let last_dragged = ops
            .iter()
            .rev()
            .find_map(|op| match op {
                MouseOp::Dragged { x, y } => Some((*x, *y)),
                _ => None,
            })
            .expect("at least one dragged step");
        match ops.last().unwrap() {
            MouseOp::Up {
                x,
                y,
                click_state: 1,
            } => {
                assert_eq!(
                    (*x, *y),
                    last_dragged,
                    "the up must land at the last actual drag point"
                );
            }
            other => panic!("last op must be the mouse-up, got {other:?}"),
        }
    }

    /// P0-6: a token cancelled BEFORE the first scroll event (the move) must
    /// prevent ANY physical op.
    #[tokio::test]
    async fn scroll_cancelled_before_first_event_never_touches_mouse() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let posted: std::sync::Arc<std::sync::Mutex<Vec<MouseOp>>> = Default::default();
        let posted2 = posted.clone();
        let post = move |op| posted2.lock().unwrap().push(op);
        let ok = scroll_core(
            0.0,
            -50.0,
            Some(cu_core::Point::new(10.0, 10.0)),
            Some(&token),
            &post,
        )
        .await;
        assert!(!ok);
        assert!(
            posted.lock().unwrap().is_empty(),
            "no scroll event may fire when cancelled before the first one"
        );
    }

    /// P0-6: a grab mid-move must suppress the scroll — the pointer may have
    /// moved to `at`, but NO scroll event follows (never move once then exit
    /// having already scrolled).
    #[tokio::test]
    async fn scroll_cancelled_after_move_suppresses_scroll() {
        let token = tokio_util::sync::CancellationToken::new();
        let posted: std::sync::Arc<std::sync::Mutex<Vec<MouseOp>>> = Default::default();
        let posted2 = posted.clone();
        let token2 = token.clone();
        let post = move |op: MouseOp| {
            // Cancel right after the move posts — the scroll must be skipped.
            if matches!(op, MouseOp::Move { .. }) {
                token2.cancel();
            }
            posted2.lock().unwrap().push(op);
        };
        let ok = scroll_core(
            0.0,
            -50.0,
            Some(cu_core::Point::new(10.0, 10.0)),
            Some(&token),
            &post,
        )
        .await;
        assert!(!ok);
        let ops = posted.lock().unwrap();
        assert_eq!(
            ops.len(),
            1,
            "the move may post, but no scroll event may follow: {ops:?}"
        );
        assert!(matches!(ops[0], MouseOp::Move { x: 10.0, y: 10.0 }));
    }
}
