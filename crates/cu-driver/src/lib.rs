//! `cu-driver`: the platform driver contract and its data types.
//!
//! The runtime talks to the computer through a [`ComputerDriver`]. This crate
//! defines the trait and the neutral data types; `cu-driver-macos` provides the
//! real implementation. Keeping the trait here (instead of in the runtime) lets
//! future Windows/Linux drivers plug in without changes to `cu-runtime`.

pub mod traits;
pub mod types;

pub use traits::{ActionResult, ComputerDriver, ResolvedAction};
pub use types::{
    ApplicationInfo, CaptureRequest, CapturedFrame, DesktopLayout, DisplayInfo, PermissionStatus,
    PointerInfo, QuickSnapshot,
};
