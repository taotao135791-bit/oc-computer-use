//! macOS Event Tap: continuous, real-time human-input monitoring.
//!
//! Replaces the old "measure pointer distance after the action" heuristic with
//! a true event listener: mouse moves, mouse buttons, scroll wheels, and
//! keys are observed *while* the agent batch is executing. The tap filters out
//! synthetic events posted by our own process (CGEvent source PID == ours), so
//! runtime-generated input can never trigger a HumanTakeover.
//!
//! Round 8 / Phase 11: the tap runs on its own **native thread** with its own
//! `CFRunLoop` — never the daemon's main run loop — so it cannot block or be
//! blocked by Tokio. The daemon holds an [`EventTapHandle`] for its lifetime
//! and calls [`EventTapHandle::stop`] on shutdown: the run loop is stopped,
//! the mach port invalidated, and the thread joined (no leaked threads).
//!
//! Latency is measured with the REAL event timestamp. CoreGraphics events carry
//! a mach-absolute-time timestamp (`kCGEventTimestamp`); we convert that to
//! milliseconds via `mach_timebase_info` and compare it to the current mach
//! time, giving `event_to_tap_ms` — the true time between the physical input
//! reaching the window server and our callback seeing it. No fake `1ms`.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::ffi::*;

/// The human-input sink. Receives the real `event_to_tap_ms` latency for every
/// non-synthetic human event.
pub trait EventTapSink: Send + Sync {
    fn on_human_event(&self, event_to_tap_ms: u64);
}

/// Adapts a plain closure to the [`EventTapSink`] trait. The driver's
/// `start_human_input_monitor` wraps the daemon-supplied callback with this so
/// the event tap crate stays free of a cu-runtime dependency.
pub struct ClosureSink(pub std::sync::Arc<dyn Fn(u64) + Send + Sync>);

impl ClosureSink {
    pub fn new(f: Box<dyn Fn(u64) + Send + Sync>) -> Self {
        Self(std::sync::Arc::new(f))
    }
}

impl EventTapSink for ClosureSink {
    fn on_human_event(&self, event_to_tap_ms: u64) {
        (self.0)(event_to_tap_ms);
    }
}

/// A running event-tap instance. Dropping without `stop()` is a bug — the
/// driver's shutdown path always calls `stop()` first.
pub struct EventTapHandle {
    stop_flag: Arc<AtomicBool>,
    runloop_ptr: Arc<AtomicPtr<c_void>>,
    tap_ptr: Arc<AtomicPtr<c_void>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl EventTapHandle {
    /// Stop the tap: signal the run loop, invalidate the mach port, disable
    /// the tap, and join the native thread. Idempotent and safe to call from
    /// any thread (the driver's shutdown path).
    pub fn stop(&self) {
        if self.stop_flag.swap(true, Ordering::SeqCst) {
            // Already stopped; make sure the thread is joined.
            if let Some(h) = self.thread.lock().unwrap().take() {
                let _ = h.join();
            }
            return;
        }
        // Wake the run loop so it exits promptly.
        let rl = self.runloop_ptr.load(Ordering::SeqCst);
        if !rl.is_null() {
            unsafe { CFRunLoopStop(rl) };
        }
        // Disable + invalidate the tap so no further events are delivered.
        let tap = self.tap_ptr.load(Ordering::SeqCst);
        if !tap.is_null() {
            unsafe {
                CGEventTapEnable(tap, false);
                CFMachPortInvalidate(tap);
            }
        }
        if let Some(h) = self.thread.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

impl Drop for EventTapHandle {
    fn drop(&mut self) {
        // Best-effort cleanup if the caller forgot `stop()`: never leak a
        // thread or a live tap past a driver drop.
        if !self.stop_flag.load(Ordering::SeqCst) {
            self.stop();
        }
    }
}

/// Callback context shared between Rust and the C tap callback.
struct TapCtx {
    sink: Arc<dyn EventTapSink>,
}

/// Start the human-input event tap on a dedicated native thread.
///
/// Returns `None` when the tap could not be created (e.g. Accessibility
/// permission not granted for event taps).
pub fn start_monitor_with_sink(
    sink: Arc<dyn EventTapSink>,
    include_keyboard: bool,
) -> Option<Arc<EventTapHandle>> {
    let mask = if include_keyboard {
        EVENT_MASK_HUMAN
    } else {
        EVENT_MASK_HUMAN & !(1 << EVENT_KEY_DOWN) & !(1 << EVENT_KEY_UP)
    };

    let ctx = Box::into_raw(Box::new(TapCtx { sink }));
    TAP_CTX_PTR.store(ctx, Ordering::SeqCst);

    let tap = unsafe {
        CGEventTapCreate(
            TAP_HID,
            KCG_EVENT_TAP_HEAD_INSERT_EVENT,
            KCG_EVENT_TAP_OPTION_DEFAULT,
            mask,
            event_tap_callback,
            ctx as *mut c_void,
        )
    };
    if tap.is_null() {
        unsafe { drop(Box::from_raw(ctx)) };
        TAP_CTX_PTR.store(ptr::null_mut(), Ordering::SeqCst);
        return None;
    }
    unsafe { CGEventTapEnable(tap, true) };

    let handle = Arc::new(EventTapHandle {
        stop_flag: Arc::new(AtomicBool::new(false)),
        runloop_ptr: Arc::new(AtomicPtr::new(ptr::null_mut())),
        tap_ptr: Arc::new(AtomicPtr::new(tap)),
        thread: Mutex::new(None),
    });
    let stop_flag = handle.stop_flag.clone();
    let runloop_ptr = handle.runloop_ptr.clone();
    // The tap pointer is published through a static `AtomicPtr` so the
    // spawned closure captures only `Send` values (`Arc<AtomicBool>` and
    // `Arc<AtomicPtr<..>>`), never a raw `*mut c_void`.
    TAP_RUNNING_PTR.store(tap, Ordering::SeqCst);

    let thread = thread::Builder::new()
        .name("cu-human-input-tap".into())
        .spawn(move || {
            let tap: *mut c_void = TAP_RUNNING_PTR.load(Ordering::SeqCst);
            let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap, 0) };
            if source.is_null() {
                return;
            }
            let rl = unsafe { CFRunLoopGetCurrent() };
            runloop_ptr.store(rl, Ordering::SeqCst);
            let common_modes = c"kCFRunLoopCommonModes".as_ptr();
            unsafe { CFRunLoopAddSource(rl, source, common_modes) };

            // Run until `stop()` flips the flag (it also stops the loop).
            while !stop_flag.load(Ordering::SeqCst) {
                unsafe { CFRunLoopRunInMode(c"kCFRunLoopDefaultMode".as_ptr(), 10.0, false) };
            }
            // Teardown on this thread: remove + release the source, disable +
            // invalidate the tap, free the callback context.
            unsafe {
                CFRunLoopRemoveSource(rl, source, common_modes);
                CGEventTapEnable(tap, false);
                CFMachPortInvalidate(tap);
                CFRelease(source as *const c_void);
            }
            unsafe {
                if !TAP_CTX_PTR.load(Ordering::SeqCst).is_null() {
                    let _ = Box::from_raw(TAP_CTX_PTR.swap(ptr::null_mut(), Ordering::SeqCst));
                }
            }
        })
        .ok()?;
    *handle.thread.lock().unwrap() = Some(thread);
    Some(handle)
}

static TAP_CTX_PTR: AtomicPtr<TapCtx> = AtomicPtr::new(ptr::null_mut());
/// The live CGEventTap pointer, published for the monitor thread. Never
/// accessed after the thread joins (the run loop's teardown path resumes it).
static TAP_RUNNING_PTR: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

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
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRunLoopAddSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_char);
    fn CFRunLoopRemoveSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_char);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRunInMode(mode: *const c_char, seconds: f64, return_after_source_handled: bool);
    fn CFRunLoopStop(rl: *mut c_void);
    fn CFRelease(cf: *const c_void);
}

// Mach time functions live in libSystem; the linker flag is `-lSystem`
// (the "lib" prefix is implied on Apple platforms).
#[link(name = "System", kind = "dylib")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> u32;
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

const KCG_EVENT_TAP_HEAD_INSERT_EVENT: u32 = 0;
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const KCG_EVENT_SOURCE_PROCESS_ID: u32 = 41; // kCGEventSourceProcessID
/// kCGEventTimestamp — mach absolute time of the event (field 0).
const KCG_EVENT_TIMESTAMP: u32 = 0;

/// Convert a mach-absolute-time delta (ticks) to milliseconds.
fn mach_ticks_to_ms(ticks: u64) -> u64 {
    static mut TIMEBASE: MachTimebaseInfo = MachTimebaseInfo { numer: 0, denom: 0 };
    unsafe {
        if TIMEBASE.denom == 0 {
            mach_timebase_info(&raw mut TIMEBASE);
            if TIMEBASE.denom == 0 {
                TIMEBASE.denom = 1;
            }
            if TIMEBASE.numer == 0 {
                TIMEBASE.numer = 1;
            }
        }
        let ns = ticks.saturating_mul(TIMEBASE.numer as u64) / TIMEBASE.denom as u64;
        ns / 1_000_000
    }
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

    // Real human input: measure the TRUE event-to-tap latency. The event's
    // timestamp is mach absolute time (ticks); the current mach time is
    // available from mach_absolute_time(). The difference, converted to ms,
    // is how long the physical input took to reach us. Never a fake 1ms.
    let event_ticks = unsafe { CGEventGetIntegerValueField(event, KCG_EVENT_TIMESTAMP) } as u64;
    let now_ticks = unsafe { mach_absolute_time() };
    let delta_ms = mach_ticks_to_ms(now_ticks.saturating_sub(event_ticks));

    let ctx = TAP_CTX_PTR.load(Ordering::SeqCst);
    if !ctx.is_null() {
        // SAFETY: the ctx lives for the tap's lifetime; the tap thread frees
        // it only after the run loop exits (which requires the callback to
        // have returned). Acquiring here is sound.
        unsafe { (*ctx).sink.on_human_event(delta_ms) };
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

    #[test]
    fn mach_ticks_to_ms_uses_timebase() {
        // Zero delta must be 0ms; a tiny delta must not panic and stays >= 0.
        assert_eq!(mach_ticks_to_ms(0), 0);
        let small = mach_ticks_to_ms(1000);
        assert!(small < 1000, "1k ticks should be sub-second on any mac");
    }
}
