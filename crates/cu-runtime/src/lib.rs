//! `cu-runtime`: the computer-use runtime — sessions, the control lock, frame
//! storage, stale-frame detection, the stabilizer, the action queue, and the
//! observe / act / inspect / session operations.
//!
//! The runtime is platform-agnostic: it drives a [`cu_driver::ComputerDriver`]
//! and enforces every safety invariant (paused / takeover / no-lock / stale /
//! out-of-bounds / confirmation) before anything reaches the driver.

pub mod action_queue;
pub mod frames;
pub mod runtime;
pub mod sessions;
pub mod stabilizer;
pub mod stale_frame;

pub use frames::{FrameStore, StoredFrame};
pub use runtime::{error_code, Runtime, RuntimeConfig};
pub use sessions::{ControlLock, Session, SharedSession};
pub use stabilizer::{StabilizeOutcome, Stabilizer, StabilizerConfig};
pub use stale_frame::{StaleFrameChecker, StaleFrameConfig};
