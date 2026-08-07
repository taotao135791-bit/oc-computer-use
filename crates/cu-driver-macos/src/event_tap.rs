//! macOS Event Tap: continuous, real-time human-input monitoring.
//!
//! Replaces the old "measure pointer distance after the action" heuristic with
//! a true event listener: mouse moves, mouse buttons, scroll wheels, and
//! keys are observed *while* the agent batch is executing. The tap filters out
//! synthetic events posted by our own process (CGEvent source PID == ours), so
//! runtime-generated input can never trigger a HumanTakeover.
//!
//! The tap hands events to a `Fn(u64)` callback (e.g. a closure capturing the
//! daemon's human-input monitor); this driver crate stays free of a cu-runtime
//! dependency to avoid a crate cycle.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::AtomicPtr;
use std::sync::{Arc, OnceLock};

use crate::ffi::*;

/// The sink closure receiving human events (latency_ms) — registered once by
/// the daemon at startup.
static HUMAN_SINK: OnceLock<Arc<dyn Fn(u64) + Send + Sync>> = OnceLock::new();
/// Event tap handle (kept alive for the process lifetime).
static TAP_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Event types we watch. mouseMoved + buttons + scroll + keys.
const EVENT_MASK_HUMAN: u64 = (1 << EVENT_LEFT_MOUSE_DOWN)
    | (1 << EVENT_LEFT_MOUSE_UP)
    | (1 << EVENT_RIGHT_MOUSE_DOWN)
    | (1 << EVENT_RIGHT_MOUSE_UP)
    | (1 << EVENT_MOUSE_MOVED)
    | (1 << EVENT_LEFT_MOUSE_DRAGGED)
    | (1 << EVENT_RIGHT_MOUSE_DRAGGED)
    | (1 << EVENT_OTHER_MOUSE_DRAGGED)
    | (1 << EVENT_SCROLL_WHEEL)
    | (1 << EVENT_KEY_DOWN)
    | (1 << EVENT_KEY_UP)
    | (1 << EVENT_FLAGS_CHANGED)
    | (1 << EVENT_OTHER_MOUSE_DOWN)
    | (1 << EVENT_OTHER_MOUSE_UP);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: extern "C" fn(*mut c_void, u32, *mut c_void) -> *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *mut c_void,
        order: i64,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_char);
    fn CFRunLoopGetMain() -> *mut c_void;
}

const KCG_EVENT_TAP_DEFAULT: u32 = 0;
const KCG_EVENT_TAP_HEAD_INSERT_EVENT: u32 = 0;
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const KCG_EVENT_SOURCE_PROCESS_ID: u32 = 41; // kCGEventSourceProcessID

/// Register the human-input sink. Idempotent (the first sink wins; the daemon
/// registers once at startup).
///
/// Returns `false` when the tap could not be created (e.g. Accessibility
/// permission not granted), so the daemon can report it in health.
pub fn register_monitor<F>(sink: F, include_keyboard: bool) -> bool
where
    F: Fn(u64) + Send + Sync + 'static,
{
    if HUMAN_SINK.get().is_some() {
        return true;
    }
    let mask = if include_keyboard {
        EVENT_MASK_HUMAN
    } else {
        EVENT_MASK_HUMAN & !(1 << EVENT_KEY_DOWN) & !(1 << EVENT_KEY_UP)
    };
    let _ = HUMAN_SINK.set(Arc::new(sink));
    unsafe {
        // Listener (read-only) tap: we never modify or swallow events.
        let tap = CGEventTapCreate(
            TAP_HID,
            KCG_EVENT_TAP_HEAD_INSERT_EVENT,
            KCG_EVENT_TAP_OPTION_DEFAULT,
            mask,
            event_tap_callback,
            ptr::null_mut(),
        );
        if tap.is_null() {
            return false; // e.g. Accessibility permission not granted for the tap
        }
        CGEventTapEnable(tap, true);
        TAP_HANDLE.store(tap, std::sync::atomic::Ordering::SeqCst);

        // Attach the tap's run loop source to the main run loop.
        let source = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
        if !source.is_null() {
            let runloop = CFRunLoopGetMain();
            let common_modes = c"kCFRunLoopCommonModes".as_ptr();
            CFRunLoopAddSource(runloop, source, common_modes);
        }
    }
    true
}

extern "C" fn event_tap_callback(
    _proxy: *mut c_void,
    _type: u32,
    event: *mut c_void,
) -> *mut c_void {
    if event.is_null() {
        return event;
    }
    // Synthetic filter: events posted by our own process (the daemon PID)
    // must never count as human input.
    let source_pid = unsafe { CGEventGetIntegerValueField(event, KCG_EVENT_SOURCE_PROCESS_ID) };
    let self_pid = std::process::id() as i64;
    if source_pid == self_pid {
        return event;
    }
    if let Some(sink) = HUMAN_SINK.get() {
        // The tap resolves within ms of the physical input.
        sink(1);
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_includes_required_event_types() {
        assert_ne!(EVENT_MASK_HUMAN & (1 << EVENT_MOUSE_MOVED), 0);
        assert_ne!(EVENT_MASK_HUMAN & (1 << EVENT_LEFT_MOUSE_DOWN), 0);
        assert_ne!(EVENT_MASK_HUMAN & (1 << EVENT_LEFT_MOUSE_UP), 0);
        assert_ne!(EVENT_MASK_HUMAN & (1 << EVENT_SCROLL_WHEEL), 0);
        assert_ne!(EVENT_MASK_HUMAN & (1 << EVENT_KEY_DOWN), 0);
        assert_ne!(EVENT_MASK_HUMAN & (1 << EVENT_FLAGS_CHANGED), 0);
    }
}
