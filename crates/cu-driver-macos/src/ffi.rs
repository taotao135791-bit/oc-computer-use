//! Minimal, hand-rolled CoreGraphics FFI for mouse, keyboard, scrolling,
//! pointer location, display geometry, and TCC permission preflight.
//!
//! We avoid the `core-graphics` crate here because its event module has
//! churned across versions; a small `#[link]` surface is fully under our
//! control and easy to reason about. Every pointer returned to Rust is wrapped
//! in an RAII guard so `CFRelease` is guaranteed even on early returns.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::c_void;
use std::sync::OnceLock;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

// ---------------------------------------------------------------------------
// CoreGraphics
// ---------------------------------------------------------------------------

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetActiveDisplayList(max_displays: u32, active: *mut u32, count: *mut u32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;

    fn CGEventSourceCreate(state: u32) -> *mut c_void;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        mouse_type: u32,
        point: CGPoint,
        button: i32,
    ) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, keycode: u16, key_down: bool) -> *mut c_void;
    fn CGEventCreateScrollWheelEvent(
        source: *mut c_void,
        unit: u32,
        wheel_count: u32,
        wheel1: i32,
        ...
    ) -> *mut c_void;
    fn CGEventPost(tap: u32, event: *mut c_void) -> i32;
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(event: *mut c_void, length: usize, string: *const u16);
    fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;

    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

// Event types (CoreGraphics).
pub const EVENT_LEFT_MOUSE_DOWN: u32 = 1;
pub const EVENT_LEFT_MOUSE_UP: u32 = 2;
pub const EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
pub const EVENT_RIGHT_MOUSE_UP: u32 = 4;
pub const EVENT_MOUSE_MOVED: u32 = 5;
pub const EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
pub const EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
pub const EVENT_OTHER_MOUSE_DRAGGED: u32 = 8;
pub const EVENT_KEY_DOWN: u32 = 10;
pub const EVENT_KEY_UP: u32 = 11;
pub const EVENT_FLAGS_CHANGED: u32 = 12;
pub const EVENT_SCROLL_WHEEL: u32 = 22;
pub const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
pub const EVENT_OTHER_MOUSE_UP: u32 = 26;

// Event tap locations.
pub const TAP_HID: u32 = 1; // kCGHIDEventTap
pub const TAP_SESSION: u32 = 2; // kCGAnnotatedSessionEventTap

// Event fields.
pub const FIELD_MOUSE_EVENT_CLICK_STATE: u32 = 1;
pub const FIELD_MOUSE_EVENT_NUMBER: u32 = 2;
pub const FIELD_SCROLL_WHEEL_IS_CONTINUOUS: u32 = 88;

// Event source states.
pub const SOURCE_STATE_HID_SYSTEM: u32 = 1;
pub const SOURCE_STATE_COMBINED_SESSION: u32 = 0;

// Modifier flags.
pub const FLAG_ALPHA_SHIFT: u64 = 0x0001_0000;
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALTERNATE: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;
pub const FLAG_NUMERIC_PAD: u64 = 0x0020_0000;
pub const FLAG_HELP: u64 = 0x0040_0000;
pub const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

// Scroll units.
pub const SCROLL_UNIT_LINE: u32 = 0;
pub const SCROLL_UNIT_PIXEL: u32 = 1;

// ---------------------------------------------------------------------------
// RAII wrappers
// ---------------------------------------------------------------------------

/// A CF-managed CGEvent that is released on drop.
pub struct CgEvent(*mut c_void);

// Safe: events are created and posted with no await in between; the async
// runtime may only move the future (and thus this handle) between those
// synchronous calls.
unsafe impl Send for CgEvent {}

impl CgEvent {
    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for CgEvent {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// A shared event source (created once, reused for all events). Raw pointers
/// are not `Sync`; this wrapper is safe because the CGEventSource is created
/// once and never mutated after creation (CoreGraphics owns it).
struct CgSource(*mut c_void);
unsafe impl Send for CgSource {}
unsafe impl Sync for CgSource {}

pub fn shared_source() -> *mut c_void {
    static SOURCE: OnceLock<CgSource> = OnceLock::new();
    SOURCE.get_or_init(|| CgSource(unsafe { CGEventSourceCreate(SOURCE_STATE_HID_SYSTEM) })).0
}

pub fn create_mouse_event(mouse_type: u32, point: CGPoint, button: i32) -> CgEvent {
    unsafe { CgEvent(CGEventCreateMouseEvent(shared_source(), mouse_type, point, button)) }
}

pub fn create_keyboard_event(keycode: u16, key_down: bool) -> CgEvent {
    unsafe { CgEvent(CGEventCreateKeyboardEvent(shared_source(), keycode, key_down)) }
}

pub fn create_scroll_event(wheel1: i32, wheel2: i32, continuous: bool) -> CgEvent {
    unsafe {
        let ev = CgEvent(CGEventCreateScrollWheelEvent(
            shared_source(),
            SCROLL_UNIT_PIXEL,
            2,
            wheel1,
            wheel2,
            0,
        ));
        if continuous {
            CGEventSetIntegerValueField(ev.0, FIELD_SCROLL_WHEEL_IS_CONTINUOUS, 1);
        }
        ev
    }
}

pub fn post(event: &CgEvent) {
    unsafe {
        CGEventPost(TAP_HID, event.0);
    }
}

pub fn set_flags(event: &CgEvent, flags: u64) {
    unsafe {
        CGEventSetFlags(event.0, flags);
    }
}

pub fn set_click_state(event: &CgEvent, state: i64) {
    unsafe {
        CGEventSetIntegerValueField(event.0, FIELD_MOUSE_EVENT_CLICK_STATE, state);
    }
}

pub fn set_unicode(event: &CgEvent, text: &str) {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        CGEventKeyboardSetUnicodeString(event.0, utf16.len(), utf16.as_ptr());
    }
}

pub fn current_mouse_location() -> CGPoint {
    unsafe {
        let ev = CGEventCreate(shared_source());
        let loc = CGEventGetLocation(ev);
        if !ev.is_null() {
            CFRelease(ev);
        }
        loc
    }
}

pub fn preflight_screen_recording() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub fn request_screen_recording() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

pub fn is_process_trusted_for_accessibility() -> bool {
    unsafe { AXIsProcessTrusted() }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

pub fn list_active_displays() -> Vec<u32> {
    let mut ids = [0u32; 32];
    let mut count: u32 = 0;
    unsafe {
        let err = CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut count);
        if err != 0 {
            return Vec::new();
        }
    }
    ids[..count as usize].to_vec()
}

pub fn main_display_id() -> u32 {
    unsafe { CGMainDisplayID() }
}

pub fn display_bounds(display: u32) -> CGRect {
    unsafe { CGDisplayBounds(display) }
}

pub fn display_pixels(display: u32) -> (usize, usize) {
    unsafe { (CGDisplayPixelsWide(display), CGDisplayPixelsHigh(display)) }
}

/// CoreGraphics mouse button constants: 0 left, 1 right, 2 middle/other.
pub const MOUSE_BUTTON_LEFT: i32 = 0;
pub const MOUSE_BUTTON_RIGHT: i32 = 1;
pub const MOUSE_BUTTON_MIDDLE: i32 = 2;

#[cfg(test)]
mod tests {
    // These tests exercise the FFI against the real CoreGraphics; they are
    // cheap and side-effect free (they never post events, only read state).
    #[test]
    fn displays_and_bounds_are_consistent() {
        let ids = super::list_active_displays();
        assert!(!ids.is_empty(), "expected at least one active display");
        for id in ids {
            let bounds = super::display_bounds(id);
            assert!(bounds.size.width > 0.0 && bounds.size.height > 0.0);
            let (w, h) = super::display_pixels(id);
            assert!(w > 0 && h > 0);
        }
    }

    #[test]
    fn main_display_is_in_active_list() {
        let ids = super::list_active_displays();
        assert!(ids.contains(&super::main_display_id()));
    }

    #[test]
    fn scroll_event_constructs() {
        let ev = super::create_scroll_event(-50, 0, true);
        assert!(!ev.as_ptr().is_null());
    }

    #[test]
    fn pointer_location_is_finite() {
        let loc = super::current_mouse_location();
        assert!(loc.x.is_finite() && loc.y.is_finite());
    }

    #[test]
    fn permission_functions_are_callable() {
        // Must not panic/crash even without the TCC entitlement.
        let _ = super::preflight_screen_recording();
        let _ = super::is_process_trusted_for_accessibility();
    }
}
