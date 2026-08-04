//! Per-request cancellation registry.
//!
//! Cancellation is scoped by [`cu_core::RequestKey`] — `(connection_id,
//! request_id)`. The registry lets `computer.cancel` abort **exactly one**
//! in-flight request: two clients may both use `request_id: 1`, and cancelling
//! client A's request never touches client B's, because the keys differ.
//!
//! A request registers its batch token *before* it starts executing (while it
//! may still be waiting on the session's `busy` lock), so cancelling a queued
//! request prevents it from ever running. Registration happens after every
//! fallible pre-check, so the small gap before registration is covered by a
//! cancellation tombstone: a cancel that lands before registration cancels the
//! token the moment it registers.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use cu_core::{CuError, RequestKey};
use tokio_util::sync::CancellationToken;

/// One registered in-flight request.
#[derive(Clone)]
pub struct RequestHandle {
    pub session_id: String,
    pub token: CancellationToken,
}

/// `Arc`-backed handle used by the runtime to scope a batch's registration.
pub type SharedRequestHandle = std::sync::Arc<RequestHandle>;

/// Bounded size for the cancellation-tombstone set.
const MAX_TOMBSTONES: usize = 256;

struct Inner {
    handles: HashMap<RequestKey, RequestHandle>,
    /// Keys whose cancel arrived before registration. When such a request
    /// registers, its token is cancelled immediately and the tombstone is
    /// consumed.
    tombstones: HashSet<RequestKey>,
}

/// The runtime-wide registry of cancellable requests.
pub struct RequestRegistry {
    inner: Mutex<Inner>,
}

impl Default for RequestRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                handles: HashMap::new(),
                tombstones: HashSet::new(),
            }),
        }
    }

    /// Register a request so it can be cancelled. If a cancel already arrived
    /// for this key (it was queued when the cancel landed), the token is
    /// cancelled immediately and the tombstone consumed.
    pub fn register(&self, key: RequestKey, session_id: String, token: CancellationToken) {
        let mut g = self.inner.lock().unwrap();
        if g.tombstones.remove(&key) {
            token.cancel();
        }
        g.handles.insert(key, RequestHandle { session_id, token });
    }

    /// The request finished: drop its handle. A later cancel for the same key
    /// reports "nothing to cancel" (the request already ended).
    pub fn unregister(&self, key: &RequestKey) {
        self.inner.lock().unwrap().handles.remove(key);
    }

    /// Cancel exactly the request named by `key`, but only if it belongs to
    /// `session_id`. Returns:
    /// - `Ok(true)` — cancelled.
    /// - `Ok(false)` — no matching request (already finished / never started).
    /// - `Err(InvalidParams)` — a request with this key exists but belongs to a
    ///   different session; nothing was cancelled.
    pub fn cancel(&self, key: &RequestKey, session_id: &str) -> Result<bool, CuError> {
        let mut g = self.inner.lock().unwrap();
        match g.handles.get(key) {
            Some(h) if h.session_id == session_id => {
                h.token.cancel();
                Ok(true)
            }
            Some(_) => Err(CuError::InvalidParams(
                "request does not belong to the named session".into(),
            )),
            None => {
                // Request not started yet — remember the intent so registration
                // cancels it the moment it begins.
                g.tombstones.insert(key.clone());
                if g.tombstones.len() > MAX_TOMBSTONES {
                    // Bound the tombstone set; keys are per-connection JSON-RPC
                    // ids, so eviction is harmless.
                    if let Some(oldest) = g.tombstones.iter().next().cloned() {
                        g.tombstones.remove(&oldest);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Number of currently registered requests (tests).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().handles.len()
    }

    /// Whether any request is currently registered (tests).
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().handles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::RequestKey;

    fn key(conn: u64, id: i64) -> RequestKey {
        RequestKey {
            connection_id: conn,
            request_id: serde_json::json!(id),
        }
    }

    #[test]
    fn cancels_exactly_the_named_request() {
        let reg = RequestRegistry::new();
        let a = CancellationToken::new();
        let b = CancellationToken::new();
        reg.register(key(1, 1), "s1".into(), a.clone());
        reg.register(key(1, 2), "s1".into(), b.clone());
        assert!(reg.cancel(&key(1, 1), "s1").unwrap());
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled(), "request B must be untouched");
    }

    #[test]
    fn same_request_id_on_two_connections_is_isolated() {
        let reg = RequestRegistry::new();
        let a = CancellationToken::new();
        let b = CancellationToken::new();
        // Both clients use request_id 1 — the classic collision case.
        reg.register(key(1, 1), "s1".into(), a.clone());
        reg.register(key(2, 1), "s1".into(), b.clone());
        reg.cancel(&key(1, 1), "s1").unwrap();
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled(), "client B's request_id=1 must survive");
    }

    #[test]
    fn wrong_session_is_rejected_without_cancelling() {
        let reg = RequestRegistry::new();
        let a = CancellationToken::new();
        reg.register(key(1, 1), "s1".into(), a.clone());
        assert!(reg.cancel(&key(1, 1), "s-other").is_err());
        assert!(!a.is_cancelled());
    }

    #[test]
    fn cancel_before_registration_is_tombstoned() {
        let reg = RequestRegistry::new();
        assert!(!reg.cancel(&key(1, 1), "s1").unwrap());
        // The request registers later (was queued) — it must be cancelled at
        // registration, not run to completion.
        let a = CancellationToken::new();
        reg.register(key(1, 1), "s1".into(), a.clone());
        assert!(
            a.is_cancelled(),
            "tombstoned cancel must cancel on register"
        );
    }

    #[test]
    fn unregister_removes_the_handle() {
        let reg = RequestRegistry::new();
        let a = CancellationToken::new();
        reg.register(key(1, 1), "s1".into(), a.clone());
        reg.unregister(&key(1, 1));
        assert_eq!(reg.len(), 0);
        assert!(!reg.cancel(&key(1, 1), "s1").unwrap());
        assert!(!a.is_cancelled());
    }
}
