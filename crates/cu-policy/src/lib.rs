//! `cu-policy`: safety policies enforced by the runtime.
//!
//! - [`bounds`]: coordinate bounds enforcement for every location-bearing action.
//! - [`confirmation`]: high-risk / confirmation-required gating declared by the caller.
//! - [`takeover`]: configurable reaction when the physical user grabs the mouse.

pub mod bounds;
pub mod confirmation;
pub mod takeover;

pub use bounds::{batch_in_bounds, resolve_action_points};
pub use confirmation::{authorize, ConfirmationPolicy, RiskLevel};
pub use takeover::{TakeoverDetector, TakeoverPolicy};
