//! Accessibility coordinate actuator (round 8, Phase 6).
//!
//! This is **actuation only, never grounding**: the model already chose a
//! visual coordinate from a screenshot. The runtime maps that global point to
//! an accessibility element (`AXUIElementCopyElementAtPosition`), checks the
//! element supports a press action, and performs it — without moving the real
//! system cursor and without ever sending the AX tree to the model.
//!
//! When the element does not support `AXPress` (canvas, games, custom UI,
//! some Electron surfaces) we report `AX_UNSUPPORTED`; the runtime then
//! decides whether physical fallback is allowed under the session's pointer
//! policy.

#![allow(non_camel_case_types, dead_code, clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use cu_core::CuError;
use tokio::task;

// ---------------------------------------------------------------------------
// Minimal CoreFoundation / Accessibility FFI (ApplicationServices)
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

// Accessibility action/attribute names we care about.
pub const AX_PRESS: &str = AX_PRESS_ACTION;
pub const AX_WINDOW: &str = AX_WINDOWS_ATTRIBUTE;

// kAXPressAction / kAXWindowsAttribute constants (stable strings).
const AX_PRESS_ACTION: &str = "AXPress";
const AX_WINDOWS_ATTRIBUTE: &str = "AXWindows";

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AXValueRef(pub *mut c_void);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AXUIElementRef(pub *mut c_void);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(non_camel_case_types)]
pub enum AXError {
    AXErrorSuccess = 0,
    AXErrorFailure = -25200,
    AXErrorIllegalArgument = -25201,
    AXErrorInvalidUIElement = -25202,
    AXErrorInvalidUIElementObserver = -25203,
    AXErrorCannotComplete = -25204,
    AXErrorAttributeUnsupported = -25205,
    AXErrorActionUnsupported = -25206,
    AXErrorNotImplemented = -25208,
    AXErrorNotificationUnsupported = -25209,
    AXErrorNotEnoughPrecision = -25210,
    AXErrorAPIDisabled = -25211,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *mut c_void;
    fn AXUIElementCopyElementAtPosition(
        application: *mut c_void,
        x: f32,
        y: f32,
        element: *mut *mut c_void,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: *mut c_void, names: *mut *mut c_void) -> i32;
    fn AXUIElementPerformAction(element: *mut c_void, action: *const c_char) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: *mut c_void,
        attribute: *const c_char,
        value: *mut *mut c_void,
    ) -> i32;
    fn CFRelease(cf: *const c_void);
    fn CFArrayGetCount(theArray: *mut c_void) -> isize;
    fn CFArrayGetValueAtIndex(theArray: *mut c_void, idx: isize) -> *const c_void;
    fn CFStringGetCStringPtr(theString: *mut c_void, encoding: u32) -> *const c_char;
    fn CFStringGetLength(theString: *mut c_void) -> isize;
    fn CFStringGetCString(
        theString: *mut c_void,
        buffer: *mut c_char,
        bufferSize: isize,
        encoding: u32,
    ) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn AXIsProcessTrusted() -> bool;
}

const KCFSTRING_ENCODING_UTF8: u32 = 0x08000100;

/// Whether the current process is trusted for AX (must be granted in System
/// Settings > Privacy & Security > Accessibility).
pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// True when `element` supports `AXPress`.
pub fn supports_press(element: *mut c_void) -> bool {
    unsafe {
        let mut names: *mut c_void = ptr::null_mut();
        if AXUIElementCopyActionNames(element, &mut names) != AXError::AXErrorSuccess as i32
            || names.is_null()
        {
            return false;
        }
        let mut found = false;
        let count = CFArrayGetCount(names);
        for i in 0..count {
            let v = CFArrayGetValueAtIndex(names, i);
            if v.is_null() {
                continue;
            }
            if CFGetTypeID(v) != CFStringGetTypeID() {
                continue;
            }
            let cptr = CFStringGetCStringPtr(v as *mut c_void, KCFSTRING_ENCODING_UTF8);
            if !cptr.is_null() {
                if let Ok(s) = CStr::from_ptr(cptr).to_str() {
                    if s == AX_PRESS {
                        found = true;
                        break;
                    }
                }
            }
        }
        CFRelease(names);
        found
    }
}

/// Perform `AXPress` on the element at global logical `(x, y)`.
///
/// Returns:
/// - `Ok(true)`  — AXPress executed.
/// - `Ok(false)` — element at that point exists but does not support press
///   (AX_UNSUPPORTED; runtime decides fallback policy).
/// - `Err`       — AX lookup failed / not trusted.
pub async fn press_at(pid: i32, x: f64, y: f64) -> Result<bool, CuError> {
    if !is_trusted() {
        return Err(CuError::permission(
            cu_core::PermissionKind::Accessibility,
            false,
        ));
    }
    // AX calls are quick but can block briefly (AXIsProcessTrusted etc.); run
    // on the blocking pool so the daemon's async executor is never wedged.
    task::spawn_blocking(move || unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Ok(false);
        }
        let mut element: *mut c_void = ptr::null_mut();
        let err = AXUIElementCopyElementAtPosition(app, x as f32, y as f32, &mut element);
        CFRelease(app);
        if err != AXError::AXErrorSuccess as i32 || element.is_null() {
            return Ok(false);
        }
        let supported = supports_press(element);
        if !supported {
            CFRelease(element);
            return Ok(false);
        }
        let action = CString::new(AX_PRESS).expect("AX_PRESS has no NUL");
        let res = AXUIElementPerformAction(element, action.as_ptr());
        CFRelease(element);
        if res == AXError::AXErrorSuccess as i32 {
            Ok(true)
        } else {
            Err(CuError::Driver(format!("AXPress failed with code {res}")))
        }
    })
    .await
    .map_err(|e| CuError::Driver(format!("AX task join error: {e}")))?
}
