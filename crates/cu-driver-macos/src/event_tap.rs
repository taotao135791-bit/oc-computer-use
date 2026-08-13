//! macOS Event Tap: continuous, real-time human-input monitoring (P0-3).
//! Dedicated native thread + CFRunLoop, real timestamps, joinable shutdown,
//! exposed health state. Sink/state registered only after tap is live.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;
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
    fn CGEventGetTimestamp(event: *mut c_void) -> u64;
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
    fn CFMachPortCreateRunLoopSource(
        a: *const c_void,
        port: *mut c_void,
        order: i64,
    ) -> *mut c_void;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_str: *const i8,
        encoding: u32,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(runloop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRun();
    fn CFRunLoopStop(runloop: *mut c_void);
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRelease(cf: *const c_void);
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// Mach absolute timebase (G-5). `mach_absolute_time()` returns ticks in this
/// unit; on most current hardware the rate is 1 tick = 1 ns, but the only
/// correct way to convert to nanoseconds is `numer / denom` from
/// `mach_timebase_info`. Dividing ticks by 1_000_000 directly would misreport
/// latency by the timebase ratio on machines where the rate is not 1.
#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

impl Default for MachTimebaseInfo {
    fn default() -> Self {
        // 1/1: the documented fallback when `mach_timebase_info` fails. A
        // timebase of 1 tick = 1 ns is correct on every Intel + Apple Silicon
        // Mac shipped in the last decade, so a failure is safe to degrade to.
        Self { numer: 1, denom: 1 }
    }
}

impl MachTimebaseInfo {
    /// Convert mach ticks to nanoseconds.
    fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        let numer = if self.numer != 0 { self.numer } else { 1 };
        let denom = if self.denom != 0 { self.denom } else { 1 };
        // u128: ticks can be huge (uptime), numer/denom are u32; the product
        // can overflow u64 on a 1ns/timebase machine with a long uptime.
        ((ticks as u128 * numer as u128) / denom as u128) as u64
    }
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
    /// P1: the daemon-facing word for the state — "starting" while Starting,
    /// "active" ONLY when Active, "unavailable" when Failed (never "failed":
    /// from the operator's point of view a failed tap means hardware human
    /// detection is unavailable, and the pointer-delta heuristic takes over).
    pub fn as_str(self) -> &'static str {
        match self {
            EventTapState::Starting => "starting",
            EventTapState::Active => "active",
            EventTapState::Failed => "unavailable",
            EventTapState::Stopped => "stopped",
        }
    }

    /// P1: a monitor is REPORTED live only when it is genuinely `Active`.
    /// `Starting` is a pending, not-yet-live state — a caller that treats it
    /// as live would believe hardware detection is authoritative before the
    /// tap thread has finished setting up.
    pub fn is_live(self) -> bool {
        matches!(self, EventTapState::Active)
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
        // Preflight: a HID event tap requires Accessibility trust. On some
        // macOS versions `CGEventTapCreate` BLOCKS (rather than returning NULL)
        // when the calling process is not trusted, which would leave the daemon
        // stuck in `starting` forever with no surfaced error — the runtime then
        // silently runs on the pointer-delta heuristic while reporting the tap
        // as merely "starting". Fail fast instead: report `Failed` (surfaced as
        // "unavailable") so the degraded state is explicit in `daemon.log` and
        // `health`, never a half-reported pending.
        if !crate::ffi::is_process_trusted_for_accessibility() {
            tracing::error!(
                "event tap unavailable: Accessibility permission not granted to this binary \
                 (AXIsProcessTrusted = false); human-input monitor degraded to pointer-delta heuristic"
            );
            *state.lock().unwrap() = EventTapState::Failed;
            return;
        }
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
        // CFRunLoopAddSource REQUIRES a real CFStringRef mode.
        // A C-string pointer or NULL is dereferenced as a CF object by
        // __CFRunLoopCopyMode -> CFSetGetValue -> CFHash and traps with
        // SIGTRAP on arm64 (observed: daemon died at startup).
        let mode = CFStringCreateWithCString(
            ptr::null(),
            c"kCFRunLoopDefaultMode".as_ptr(),
            0x08000100, // kCFStringEncodingUTF8
        );
        if mode.is_null() {
            CFMachPortInvalidate(tap);
            CFRelease(source);
            CFRelease(tap);
            *state.lock().unwrap() = EventTapState::Failed;
            return;
        }
        CFRunLoopAddSource(rl, source, mode);
        CGEventTapEnable(tap, true);
        // The tap is genuinely live once the mach port was created and the
        // run-loop source was added. (Do NOT probe via CGEventTapIsEnabled:
        // the FFI return path is unreliable for a tap owned by another run
        // loop and an accidental false caused a wrong-path CFRelease that
        // crashed the process with SIGTRAP. Create-failure and source-failure
        // are the two authoritative failure gates; both are checked above.)
        runloop.store(rl as usize, Ordering::SeqCst);
        let arc: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(sink);
        // FAT-pointer UB fix: an Arc<dyn Fn> into_raw is a fat pointer; casting
        // it to a thin *mut and dereferencing later read a garbage vtable ->
        // SIGBUS when the daemon started the Event Tap thread (CI never hit it
        // because no real events flowed). The Mutex stores the Arc whole.
        // P0-7 (restart): a Mutex (not OnceLock) so a stop/restart of the tap
        // re-registers the NEW sink instead of silently keeping a stale one
        // (OnceLock's `set` returns Err on the second call and the callback
        // would keep invoking the OLD sink).
        *GLOBAL_SINK.lock().unwrap() = Some(arc);
        *state.lock().unwrap() = EventTapState::Active;
        tracing::info!("event tap active on dedicated thread");
        CFRunLoopRun();
        CFRunLoopRemoveSource(rl, source, mode);
        CFRelease(mode as *const c_void);
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

// The sink is registered on the tap thread, only after the tap is genuinely
// live. Mutex<Option<Arc>> (not OnceLock) for two reasons: (1) storing an
// `Arc<dyn Fn>`'s `Arc::into_raw` (a FAT pointer) as a thin `*mut Arc<...>`
// and dereferencing it later read garbage vtable bytes -> SIGBUS the moment
// the daemon started the Event Tap thread, so the Arc is stored WHOLE; and
// (2) P0-7 restart: a stop/restart of the tap must install the NEW sink, which
// a OnceLock (set-once) cannot do. The Arc is cloned out of the lock before
// the call, so the callback can never deadlock against the registration.
/// The registered latency sink. A `Mutex<Option<Arc>>` (not OnceLock) for two
/// reasons: (1) storing the `Arc<dyn Fn>` whole avoids the FAT-pointer SIGBUS
/// from storing an `Arc::into_raw` as a thin pointer and dereferencing it
/// later; and (2) P0-7 restart: a stop/restart must install the NEW sink, which
/// a set-once OnceLock cannot do. Type alias keeps the static's type readable.
type LatencySink = std::sync::Arc<dyn Fn(u64) + Send + Sync>;
static GLOBAL_SINK: std::sync::Mutex<Option<LatencySink>> = std::sync::Mutex::new(None);

fn with_sink(f: impl FnOnce(&dyn Fn(u64))) {
    let sink = GLOBAL_SINK.lock().unwrap().clone();
    if let Some(sink) = sink {
        f(sink.as_ref());
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
    let source_pid = unsafe { CGEventGetIntegerValueField(event, KCG_EVENT_SOURCE_PROCESS_ID) };
    if source_pid == std::process::id() as i64 {
        return event;
    }
    // Real timestamp (mach absolute timebase) — never a hardcoded 1. Both
    // `CGEventGetTimestamp` and `mach_absolute_time` tick in the SAME mach
    // timebase, so the subtraction is meaningful; converting to milliseconds
    // still needs the timebase ratio (G-5), not a hardcoded /1_000_000.
    let ts = unsafe { CGEventGetTimestamp(event) };
    let now = unsafe { mach_absolute_time() };
    let mut tb = MachTimebaseInfo::default();
    unsafe { mach_timebase_info(&mut tb) };
    let ticks = now.saturating_sub(ts);
    let latency_ms = tb.ticks_to_nanos(ticks) / 1_000_000;
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
    fn only_active_reports_live() {
        // P1 truth table: a monitor is REPORTED active only when it is
        // genuinely `Active`. `Starting` is a pending state — a caller must
        // never treat it as authoritative hardware detection.
        assert!(EventTapState::Active.is_live());
        assert!(
            !EventTapState::Starting.is_live(),
            "Starting must NOT report active (pending, not yet live)"
        );
        assert!(
            !EventTapState::Failed.is_live(),
            "a failed tap must not report active"
        );
        assert!(
            !EventTapState::Stopped.is_live(),
            "a stopped tap must not report active"
        );
        // The daemon-facing word: "starting" / "active" / "unavailable" /
        // "stopped" — a failed tap reads "unavailable", never "failed".
        assert_eq!(EventTapState::Starting.as_str(), "starting");
        assert_eq!(EventTapState::Active.as_str(), "active");
        assert_eq!(EventTapState::Failed.as_str(), "unavailable");
        assert_eq!(EventTapState::Stopped.as_str(), "stopped");
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

    #[test]
    fn timebase_conversion_scales_by_numer_denom() {
        // 1 tick = 2 ns (numer 2, denom 1): 1_000 ticks -> 2_000 ns.
        let tb = MachTimebaseInfo { numer: 2, denom: 1 };
        assert_eq!(tb.ticks_to_nanos(1_000), 2_000);
        // 2 ticks = 1 ns (numer 1, denom 2): 1_000 ticks -> 500 ns.
        let tb = MachTimebaseInfo { numer: 1, denom: 2 };
        assert_eq!(tb.ticks_to_nanos(1_000), 500);
        // 1/1 (the modern default) is identity.
        let tb = MachTimebaseInfo::default();
        assert_eq!(tb.ticks_to_nanos(123), 123);
        // A failed `mach_timebase_info` leaves 0/0 on some paths; the
        // conversion must not divide by zero and falls back to 1/1.
        let tb = MachTimebaseInfo { numer: 0, denom: 0 };
        assert_eq!(tb.ticks_to_nanos(123), 123);
    }

    #[test]
    fn restart_reregisters_the_new_sink() {
        // Section 三十八 test 10 / P0-7d: a stop/start of the Event Tap must
        // install the NEW sink. A OnceLock cannot do this (its second `set`
        // returns Err and the callback would keep invoking the OLD sink), so
        // the sink is stored in a Mutex and replaced on restart. This pins the
        // replacement semantics without needing a live CGEventTap (CI cannot
        // grant Accessibility, and a real tap would observe the user's input).
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Fresh registration state (as if the tap had never run).
        *GLOBAL_SINK.lock().unwrap() = None;
        with_sink(|_| panic!("a cleared sink must never be invoked"));

        // First start installs sink A.
        let a_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let sink_a = {
            let c = a_calls.clone();
            std::sync::Arc::new(move |_: u64| {
                c.fetch_add(1, Ordering::SeqCst);
            })
        };
        *GLOBAL_SINK.lock().unwrap() = Some(sink_a);
        with_sink(|s| s(7));
        assert_eq!(a_calls.load(Ordering::SeqCst), 1, "sink A receives events");

        // Stop + restart installs sink B; the callback path must now reach B.
        let b_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let sink_b = {
            let c = b_calls.clone();
            std::sync::Arc::new(move |_: u64| {
                c.fetch_add(1, Ordering::SeqCst);
            })
        };
        *GLOBAL_SINK.lock().unwrap() = Some(sink_b);
        with_sink(|s| s(9));
        assert_eq!(
            b_calls.load(Ordering::SeqCst),
            1,
            "sink B (new) receives events"
        );
        assert_eq!(
            a_calls.load(Ordering::SeqCst),
            1,
            "the OLD sink must be replaced, not invoked alongside"
        );

        // Clean up so no stale registration leaks into other tests / the daemon.
        *GLOBAL_SINK.lock().unwrap() = None;
    }
}
