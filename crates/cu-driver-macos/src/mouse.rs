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

/// Smooth drag with the primary button held down from `from` to `to`.
pub async fn drag(
    from: cu_core::Point,
    to: cu_core::Point,
    duration_ms: Option<u64>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> bool {
    let duration = duration_ms.unwrap_or(400).min(10_000);
    move_pointer(from.x, from.y);
    button_down(MouseButton::Left, from.x, from.y, 1);
    let path = coordinates::drag_path(from, to, duration, 90.0);
    let drag_type = move_type_for_button(MOUSE_BUTTON_LEFT);
    let mut cancelled = false;
    for (p, wait) in &path {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                cancelled = true;
                break;
            }
        }
        let ev = create_mouse_event(drag_type, CGPoint { x: p.x, y: p.y }, MOUSE_BUTTON_LEFT);
        post(&ev);
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
                cancelled = true;
                break;
            }
        }
    }
    // Mouse-down already happened: ALWAYS send mouse-up, even when cancelled,
    // so the system is never left with a stuck pressed button.
    if cancelled {
        button_up(MouseButton::Left, to.x.max(0.0), to.y.max(0.0), 1);
    } else {
        button_up(MouseButton::Left, to.x, to.y, 1);
    }
    !cancelled
}

/// Scroll by pixel deltas, cancellation-aware (round 9 / P0-2): long scrolls
/// are split into chunks and check the token between chunks; never sends one
/// uninterruptible burst. Returns `true` on completion, `false` on cancel.
pub async fn scroll(
    delta_x: f64,
    delta_y: f64,
    at: Option<cu_core::Point>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> bool {
    if let Some(p) = at {
        move_pointer(p.x, p.y);
    }
    // Accumulate total in each axis, post in ~30px chunks.
    let chunk = 30.0;
    let total_x = delta_x.round() as i64;
    let total_y = delta_y.round() as i64;
    let mut remaining_x = total_x;
    let mut remaining_y = total_y;
    while remaining_x != 0 || remaining_y != 0 {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                return false;
            }
        }
        let dx = remaining_x.clamp(-chunk as i64, chunk as i64);
        let dy = remaining_y.clamp(-chunk as i64, chunk as i64);
        let ev = create_scroll_event(dy as i32, dx as i32, true);
        post(&ev);
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
}
