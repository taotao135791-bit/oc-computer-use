//! macOS Event Tap: continuous, real-time human-input monitoring (P0-3).
//! Dedicated native thread + CFRunLoop, real timestamps, joinable shutdown,
//! exposed health state. Sink/state registered only after tap is live.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;
use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::ffi::*;

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
    fn CGEventTapIsEnabled(tap: *mut c_void) -> bool;
    fn CGEventGetTimestamp(event: *mut c_void) -> u64;
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CFMachPortCreateRunLoopSource(
        a: *const c_void,
        port: *mut c_void,
        order: i64,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_char);
    fn CFRunLoopRemoveSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_char);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRun();
    fn CFRunLoopStop(runloop: *mut c_void);
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRelease(cf: *const c_void);
}

extern "C" {
    fn mach_absolute_time() -> u64;
}

const KCG_EVENT_TAP_HEAD_INSERT_EVENT: u32 = 0;
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const KCG_EVENT_SOURCE_PROCESS_ID: u32 = 41;

pub const EVENT_MASK_HUMAN: u64 = (1 << EVENT_LEFT_MOUSE_DOWN)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTapState {
    Starting,
    Active,
    Failed,
    Stopped,
}

impl EventTapState {
    pub fn as_str(self) -> &'static str {
        match self {
            EventTapState::Starting => "starting",
            EventTapState::Active => "active",
            EventTapState::Failed => "failed",
            EventTapState::Stopped => "stopped",
        }
    }
}

pub struct EventTapMonitor {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<EventTapState>>,
    /// CFRunLoop pointer as usize so the handle is Send/Sync.
    runloop: Arc<AtomicUsize>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

unsafe impl Send for EventTapMonitor {}
unsafe impl Sync for EventTapMonitor {}

impl Default for EventTapMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl EventTapMonitor {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            state: Arc::new(Mutex::new(EventTapState::Stopped)),
            runloop: Arc::new(AtomicUsize::new(0)),
            thread: Mutex::new(None),
        }
    }

    pub fn start<F>(&self, sink: F)
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let mut slot = self.thread.lock().unwrap();
        if slot.is_some() {
            return;
        }
        *self.state.lock().unwrap() = EventTapState::Starting;
        self.stop.store(false, Ordering::SeqCst);
        let stop = self.stop.clone();
        let state = self.state.clone();
        let runloop = self.runloop.clone();
        let handle = std::thread::Builder::new()
            .name("cu-event-tap".into())
            .spawn(move || run_tap_thread(sink, stop, state, runloop));
        match handle {
            Ok(h) => *slot = Some(h),
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn event tap thread");
                *self.state.lock().unwrap() = EventTapState::Failed;
            }
        }
    }

    pub fn state(&self) -> EventTapState {
        *self.state.lock().unwrap()
    }

    pub fn shutdown(&self) {
        let mut slot = self.thread.lock().unwrap();
        if slot.is_none() {
            if *self.state.lock().unwrap() != EventTapState::Failed {
                *self.state.lock().unwrap() = EventTapState::Stopped;
            }
            return;
        }
        self.stop.store(true, Ordering::SeqCst);
        let rl = self.runloop.load(Ordering::SeqCst) as *mut c_void;
        if !rl.is_null() {
            unsafe { CFRunLoopStop(rl) };
        }
        let _ = slot.take().map(JoinHandle::join);
        *self.state.lock().unwrap() = EventTapState::Stopped;
    }
}

fn run_tap_thread<F>(
    sink: F,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<EventTapState>>,
    runloop: Arc<AtomicUsize>,
) where
    F: Fn(u64) + Send + Sync + 'static,
{
    unsafe {
        let tap = CGEventTapCreate(
            TAP_HID,
            KCG_EVENT_TAP_HEAD_INSERT_EVENT,
            KCG_EVENT_TAP_OPTION_DEFAULT,
            EVENT_MASK_HUMAN,
            event_tap_callback,
            ptr::null_mut(),
        );
        if tap.is_null() {
            tracing::error!("event tap create failed (Accessibility not granted?)");
            *state.lock().unwrap() = EventTapState::Failed;
            return;
        }
        let source = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
        if source.is_null() {
            CFMachPortInvalidate(tap);
            CFRelease(tap);
            *state.lock().unwrap() = EventTapState::Failed;
            return;
        }
        let rl = CFRunLoopGetCurrent();
        let common_modes = c"kCFRunLoopCommonModes".as_ptr();
        CFRunLoopAddSource(rl, source, common_modes);
        CGEventTapEnable(tap, true);
        if !CGEventTapIsEnabled(tap) {
            CFRunLoopRemoveSource(rl, source, common_modes);
            CFMachPortInvalidate(tap);
            CFRelease(source);
            CFRelease(tap);
            *state.lock().unwrap() = EventTapState::Failed;
            return;
        }
        // Only now is the tap genuinely live.
        runloop.store(rl as usize, Ordering::SeqCst);
        let arc: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(sink);
        GLOBAL_SINK.store(
            Arc::into_raw(arc) as *mut std::sync::Arc<dyn Fn(u64) + Send + Sync>,
            Ordering::SeqCst,
        );
        *state.lock().unwrap() = EventTapState::Active;
        tracing::info!("event tap active on dedicated thread");
        CFRunLoopRun();
        CFRunLoopRemoveSource(rl, source, common_modes);
        CFMachPortInvalidate(tap);
        CFRelease(source);
        CFRelease(tap);
        runloop.store(0, Ordering::SeqCst);
        *state.lock().unwrap() = if stop.load(Ordering::SeqCst) {
            EventTapState::Stopped
        } else {
            EventTapState::Failed
        };
    }
}

static GLOBAL_SINK: std::sync::atomic::AtomicPtr<std::sync::Arc<dyn Fn(u64) + Send + Sync>> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

fn with_sink(f: impl FnOnce(&dyn Fn(u64))) {
    let p = GLOBAL_SINK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: leaked on install, never freed until process exit.
    let arc: &std::sync::Arc<dyn Fn(u64) + Send + Sync> = unsafe { &*p };
    f(arc.as_ref());
}

extern "C" fn event_tap_callback(
    _proxy: *mut c_void,
    _type: u32,
    event: *mut c_void,
) -> *mut c_void {
    if event.is_null() {
        return event;
    }
    let source_pid = unsafe { CGEventGetIntegerValueField(event, KCG_EVENT_SOURCE_PROCESS_ID) };
    if source_pid == std::process::id() as i64 {
        return event;
    }
    // Real timestamp (mach absolute nanoseconds) — never a hardcoded 1.
    let ts = unsafe { CGEventGetTimestamp(event) };
    let now = unsafe { mach_absolute_time() };
    let latency_ms = now.saturating_sub(ts).saturating_div(1_000_000);
    with_sink(|sink| sink(latency_ms));
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
    fn tap_not_started_is_stopped() {
        let m = EventTapMonitor::new();
        assert_eq!(m.state(), EventTapState::Stopped);
    }

    #[test]
    fn shutdown_without_start_is_stopped_not_failed() {
        let m = EventTapMonitor::new();
        m.shutdown();
        assert_eq!(m.state(), EventTapState::Stopped);
    }

    #[test]
    fn shutdown_is_idempotent() {
        let m = EventTapMonitor::new();
        m.shutdown();
        m.shutdown();
        assert_eq!(m.state(), EventTapState::Stopped);
    }
}
