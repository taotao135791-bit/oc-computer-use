//! Session state machine and the global control lock.
//!
//! A **control lock** enforces the invariant that only one active session may
//! drive the pointer at a time. `session start` acquires it; `session stop`
//! releases it. Takeover/release transfer it between the agent and the human,
//! never to a second session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use cu_core::{ClientInfo, CuError, SecretTokenHash, SessionState};

/// The runtime-level session object (not the wire-level [`cu_core::SessionStatus`]).
pub struct Session {
    pub id: String,
    pub display_id: String,
    pub created_at: DateTime<Utc>,
    pub started_by: String,
    /// The client that created this session. Ownership matters on exit: only
    /// the creating client may stop the session it started; other clients must
    /// attach without stopping it.
    pub owner: Option<ClientInfo>,
    /// SHA-256 hash of the session's control token — the **only** stored form.
    /// Mutating operations verify the presented token against this before any
    /// side effect. The plaintext token is issued once, on `start`, and never
    /// persisted by the runtime.
    pub control_token_hash: SecretTokenHash,
    state: Mutex<SessionState>,
    paused: AtomicBool,
    user_takeover: AtomicBool,
    pub last_action_at: Mutex<Option<DateTime<Utc>>>,
    pub current_frame_id: Mutex<Option<String>>,
    /// Root token; never cancelled directly, only used to derive fresh children.
    cancel_root: tokio_util::sync::CancellationToken,
    /// One token per in-flight batch (a batch begins before it acquires the
    /// session's `busy` lock, so a waiting request can be cancelled precisely
    /// without touching the executing batch). Pause/takeover/stop cancel
    /// **all** of them; `computer.cancel` cancels exactly one, by request key.
    batch_tokens: std::sync::Mutex<Vec<tokio_util::sync::CancellationToken>>,
    pub trace: Option<cu_trace::TraceRecorder>,
    /// Serializes observe/act on this session so two concurrent batches can
    /// never interleave their pointer events.
    pub busy: tokio::sync::Mutex<()>,
}

impl Session {
    pub fn new(
        id: String,
        display_id: String,
        started_by: String,
        owner: Option<ClientInfo>,
        control_token_hash: SecretTokenHash,
        trace: Option<cu_trace::TraceRecorder>,
    ) -> Self {
        Self {
            id,
            display_id,
            created_at: Utc::now(),
            started_by,
            owner,
            control_token_hash,
            state: Mutex::new(SessionState::Active),
            paused: AtomicBool::new(false),
            user_takeover: AtomicBool::new(false),
            last_action_at: Mutex::new(None),
            current_frame_id: Mutex::new(None),
            cancel_root: tokio_util::sync::CancellationToken::new(),
            batch_tokens: std::sync::Mutex::new(Vec::new()),
            trace,
            busy: tokio::sync::Mutex::new(()),
        }
    }

    /// Verify a presented control token. `None` (no token supplied) is
    /// `CONTROL_TOKEN_REQUIRED`; a mismatch is `INVALID_CONTROL_TOKEN` — the
    /// two are deliberately distinct, but a mismatch never says *why* (length,
    /// hash, wrong session — all look identical).
    pub fn verify_control_token(&self, token: Option<&str>) -> Result<(), CuError> {
        match token {
            None => Err(CuError::ControlTokenRequired),
            Some(t) if self.control_token_hash.verify(t) => Ok(()),
            Some(_) => Err(CuError::InvalidControlToken),
        }
    }

    pub fn state(&self) -> SessionState {
        *self.state.lock().unwrap()
    }

    pub fn set_state(&self, s: SessionState) {
        *self.state.lock().unwrap() = s;
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_user_takeover(&self) -> bool {
        self.user_takeover.load(Ordering::SeqCst)
    }

    /// Transition to a new state, keeping the paused/takeover flags coherent.
    ///
    /// `UserTakeover -> Active` exists here **only** for `session_release` —
    /// the runtime gates it: `session_resume` refuses to resume a session in
    /// `UserTakeover` (`USER_TAKEOVER_ACTIVE`), so a plain `resume` can never
    /// bypass the human's takeover. This table is the flag-coherence layer;
    /// the semantics live in [`crate::runtime::Runtime::session_resume`] /
    /// `session_release`.
    pub fn transition(&self, target: SessionState) -> Result<(), CuError> {
        let current = self.state();
        let legal = match (current, target) {
            (SessionState::Starting | SessionState::Active, SessionState::Active) => true,
            (_, SessionState::Paused) => matches!(current, SessionState::Active),
            (SessionState::Paused, SessionState::Active) => true,
            (_, SessionState::UserTakeover) => {
                matches!(current, SessionState::Active | SessionState::Paused)
            }
            // release-only exit (runtime gates who may take it).
            (SessionState::UserTakeover, SessionState::Active) => true,
            (_, SessionState::Stopping | SessionState::Stopped) => true,
            (_, SessionState::Failed) => true,
            _ => false,
        };
        if !legal {
            return Err(CuError::InvalidSessionState(format!(
                "cannot transition {current:?} -> {target:?}"
            )));
        }
        self.set_state(target);
        match target {
            SessionState::Paused => {
                self.paused.store(true, Ordering::SeqCst);
                self.cancel_in_flight();
            }
            SessionState::UserTakeover => {
                self.user_takeover.store(true, Ordering::SeqCst);
                // A takeover is not a pause: entering it clears any paused
                // flag (even if taken over *from* Paused). `resume` must not
                // be able to recover this session — only `release` can.
                self.paused.store(false, Ordering::SeqCst);
                self.cancel_in_flight();
            }
            SessionState::Stopping | SessionState::Stopped | SessionState::Failed => {
                self.cancel_in_flight();
            }
            SessionState::Active => {
                self.user_takeover.store(false, Ordering::SeqCst);
                self.paused.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(())
    }

    /// Cancel **every** in-flight batch (pause / takeover / stop). Request-
    /// specific cancellation never comes through here — it targets one token
    /// via the runtime's request registry.
    pub fn cancel_in_flight(&self) {
        for t in self.batch_tokens.lock().unwrap().iter() {
            t.cancel();
        }
    }

    /// Begin a new batch: register a fresh child token and return it. The batch
    /// loop holds the returned token and aborts when it is cancelled. A batch
    /// registers before it acquires `busy`, so a queued request can be
    /// cancelled while waiting — and cancelling it never touches the batch
    /// that is executing.
    pub fn begin_batch(&self) -> tokio_util::sync::CancellationToken {
        let token = self.cancel_root.child_token();
        self.batch_tokens.lock().unwrap().push(token.clone());
        token
    }

    /// The batch finished (ran, errored, or was cancelled): drop its token.
    pub fn end_batch(&self, token: &tokio_util::sync::CancellationToken) {
        self.batch_tokens.lock().unwrap().retain(|t| t != token);
    }
}

/// The global control lock. Only one session can hold it at a time.
pub struct ControlLock {
    holder: Mutex<Option<String>>,
}

impl Default for ControlLock {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlLock {
    pub fn new() -> Self {
        Self {
            holder: Mutex::new(None),
        }
    }

    /// Try to acquire the lock for `session_id`. Fails if another session holds
    /// it. The error carries only the holder's session id; the runtime attaches
    /// the holder's (non-secret) owner identity before it reaches the wire.
    pub fn try_acquire(&self, session_id: &str) -> Result<(), CuError> {
        let mut guard = self.holder.lock().unwrap();
        match guard.as_ref() {
            Some(h) if h != session_id => Err(CuError::ControlLocked {
                holder: h.clone(),
                owner: None,
            }),
            _ => {
                *guard = Some(session_id.to_string());
                Ok(())
            }
        }
    }

    pub fn holder(&self) -> Option<String> {
        self.holder.lock().unwrap().clone()
    }

    /// Release the lock if held by `session_id`.
    pub fn release(&self, session_id: &str) {
        let mut guard = self.holder.lock().unwrap();
        if guard.as_deref() == Some(session_id) {
            *guard = None;
        }
    }

    pub fn is_held_by(&self, session_id: &str) -> bool {
        self.holder.lock().unwrap().as_deref() == Some(session_id)
    }
}

/// Convenience: an `Arc<Session>`-backed handle used by the runtime.
pub type SharedSession = Arc<Session>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_acquire_and_release() {
        let lock = ControlLock::new();
        assert!(lock.try_acquire("s1").is_ok());
        assert!(matches!(
            lock.try_acquire("s2"),
            Err(CuError::ControlLocked { .. })
        ));
        lock.release("s1");
        assert!(lock.try_acquire("s2").is_ok());
    }

    #[test]
    fn session_transitions_are_legal() {
        let s = Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            None,
        ));
        assert_eq!(s.state(), SessionState::Active);
        s.transition(SessionState::Paused).unwrap();
        assert!(s.is_paused());
        // Transitioning twice into Paused is illegal (already paused).
        assert!(s.transition(SessionState::Paused).is_err());
        s.transition(SessionState::Active).unwrap();
        assert!(!s.is_paused());
        // Pausing a stopped session is illegal.
        s.transition(SessionState::Stopped).unwrap();
        assert!(s.transition(SessionState::Paused).is_err());
    }

    #[test]
    fn takeover_sets_takeover_flag_not_paused() {
        let s = Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            None,
        ));
        s.transition(SessionState::UserTakeover).unwrap();
        assert!(s.is_user_takeover());
        assert!(
            !s.is_paused(),
            "a takeover is not a pause — resume must not be able to recover it"
        );
        // The transition-table exit exists for `release` only; the runtime
        // gates who may call it (resume refuses with USER_TAKEOVER_ACTIVE).
        s.transition(SessionState::Active).unwrap();
        assert!(!s.is_user_takeover());
        assert!(!s.is_paused());
    }

    #[test]
    fn takeover_from_paused_clears_paused() {
        let s = Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            None,
        ));
        s.transition(SessionState::Paused).unwrap();
        s.transition(SessionState::UserTakeover).unwrap();
        assert!(s.is_user_takeover());
        assert!(!s.is_paused(), "takeover must clear the paused flag");
    }

    #[test]
    fn takeover_cancels_in_flight() {
        let s = Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&cu_core::generate_control_token()),
            None,
        ));
        let batch = s.begin_batch();
        s.transition(SessionState::UserTakeover).unwrap();
        assert!(batch.is_cancelled());
        // A fresh batch after resume gets a live token.
        s.transition(SessionState::Active).unwrap();
        let next = s.begin_batch();
        assert!(!next.is_cancelled());
    }
}
