//! `cu-cli` library surface: the JSON-RPC client used by the `cu` binary.
//!
//! The client is kept here (rather than in main.rs) so other consumers —
//! the acceptance test harness, future tooling — can reuse it.

pub mod client;

pub use client::{request, request_on, ClientError};
