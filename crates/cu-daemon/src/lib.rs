//! `cu-daemon`: the JSON-RPC 2.0 daemon over a user-owned Unix socket.
//!
//! Thin layer over [`cu_runtime::Runtime`]: owns the socket lifecycle and
//! translates wire requests ([`jsonrpc::dispatch`]) into runtime calls. No
//! platform logic lives here.

pub mod jsonrpc;
pub mod server;

pub use server::{run, DaemonConfig};
