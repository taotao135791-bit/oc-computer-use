//! Session state machine and the global control lock.
//!
//! A **control lock** enforces the invariant that only one active session may
//! drive the pointer at a time. `session start` acquires it; `session stop`
//! releases it. Takeover/release transfer it between the agent and the human,
//! never to a second session.
//!
//! Since round 8 each session also owns the agent's **virtual pointer** (the
//! logical pointer that is *not* the system cursor), a pointer policy deciding
//! when the runtime may borrow the real cursor, an optional app/window target
//! for session-scoped isolation, and the keyboard focus policy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use cu_core::{
    ClientInfo, CuError, FocusPolicy, PointerMode, PointerPolicy, SecretTokenHash, SessionState,
    SessionTarget, VirtualPointerState,
};

/// The runtime-level session object (not the wire-level
/// [`cu_core::SessionStatus`]). Since round 8 each session owns the agent's
/// **virtual pointer** (the logical pointer that is *not* the system cursor),
/// a pointer policy deciding when the runtime may borrow the real cursor, an
/// optional app/window target for session-scoped isolation, and the keyboard
/// focus policy.
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
    /// SHA-256 hash of the session's observation token (read-only capability).
    /// Sensitive reads (observe / inspect / status / trace) verify against
    /// **either** this hash or the control hash — control includes observation.
    pub observation_token_hash: SecretTokenHash,
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
    /// The agent's virtual pointer — the single source of truth for where the
    /// agent *means* to point. Never confused with the system cursor.
    pub virtual_pointer: Mutex<VirtualPointerState>,
    /// Whether (and when) the runtime may borrow the real system cursor.
    pub pointer_policy: Mutex<PointerPolicy>,
    /// Optional app/window target for session-scoped isolation.
    pub target: Mutex<Option<SessionTarget>>,
    /// Bounds of the session's target window in global logical points. Set by
    /// the runtime when a window-frame provider exists; `act` rejects
    /// coordinates outside it with `TARGET_OUTSIDE_SESSION`. `None` = no
    /// window-scoped check (whole display allowed).
    pub target_bounds: Mutex<Option<cu_core::DisplayBounds>>,
    /// Round 9 / P0-4: the runtime-resolved target (bundle + pid + window id +
    /// current bounds). `None` = no target, or resolution failed (the session
    /// may still run whole-desktop but with its isolation scoped off).
    pub resolved_target: Mutex<Option<cu_driver::ResolvedSessionTarget>>,
    /// How strictly keyboard focus is validated before type/key input.
    pub focus_policy: Mutex<FocusPolicy>,
}

impl Session {
    pub fn new(
        id: String,
        display_id: String,
        started_by: String,
        owner: Option<ClientInfo>,
        control_token_hash: SecretTokenHash,
        observation_token_hash: SecretTokenHash,
        trace: Option<cu_trace::TraceRecorder>,
    ) -> Self {
        Self {
            id,
            display_id,
            created_at: Utc::now(),
            started_by,
            owner,
            control_token_hash,
            observation_token_hash,
            state: Mutex::new(SessionState::Active),
            paused: AtomicBool::new(false),
            user_takeover: AtomicBool::new(false),
            last_action_at: Mutex::new(None),
            current_frame_id: Mutex::new(None),
            cancel_root: tokio_util::sync::CancellationToken::new(),
            batch_tokens: std::sync::Mutex::new(Vec::new()),
            trace,
            busy: tokio::sync::Mutex::new(()),
            virtual_pointer: Mutex::new(VirtualPointerState::default()),
            pointer_policy: Mutex::new(PointerPolicy::IsolatedPreferred),
            target: Mutex::new(None),
            target_bounds: Mutex::new(None),
            resolved_target: Mutex::new(None),
            focus_policy: Mutex::new(FocusPolicy::Strict),
        }
    }

    /// Seed the virtual pointer at a real position (called right after
    /// session start, using the live system cursor position).
    pub fn init_virtual_pointer(&self, p: cu_core::Point, display_id: impl Into<String>) {
        let mut vp = self.virtual_pointer.lock().unwrap();
        *vp = VirtualPointerState::new(p.x, p.y, display_id);
    }

    /// Update the virtual pointer position (usually to the target of a Move).
    pub fn set_virtual_pointer(&self, p: cu_core::Point, display_id: impl Into<String>) {
        self.virtual_pointer
            .lock()
            .unwrap()
            .set_location(p, display_id);
    }

    /// Pointer mode currently shown by the ghost cursor.
    pub fn pointer_mode(&self) -> PointerMode {
        self.virtual_pointer.lock().unwrap().mode
    }

    /// Reflect a session state change in the ghost cursor's mode.
    pub fn sync_pointer_mode(&self, state: SessionState) {
        let mode = match state {
            SessionState::UserTakeover => PointerMode::UserTakeover,
            SessionState::Paused => PointerMode::Paused,
            _ => PointerMode::Isolated,
        };
        self.virtual_pointer.lock().unwrap().set_mode(mode);
    }

    pub fn set_pointer_policy(&self, policy: PointerPolicy) {
        *self.pointer_policy.lock().unwrap() = policy;
    }

    pub fn get_pointer_policy(&self) -> PointerPolicy {
        *self.pointer_policy.lock().unwrap()
    }

    pub fn set_target(&self, target: Option<SessionTarget>) {
        *self.target.lock().unwrap() = target;
    }

    pub fn get_target(&self) -> Option<SessionTarget> {
        self.target.lock().unwrap().clone()
    }

    pub fn set_target_bounds(&self, bounds: Option<cu_core::DisplayBounds>) {
        *self.target_bounds.lock().unwrap() = bounds;
    }

    pub fn get_target_bounds(&self) -> Option<cu_core::DisplayBounds> {
        *self.target_bounds.lock().unwrap()
    }

    /// Store the runtime-resolved target (P0-4). Also mirrors its bounds into
    /// `target_bounds` so the existing `TARGET_OUTSIDE_SESSION` check works.
    pub fn set_resolved_target(&self, rt: Option<cu_driver::ResolvedSessionTarget>) {
        let bounds = rt.as_ref().and_then(|r| r.bounds);
        *self.resolved_target.lock().unwrap() = rt;
        *self.target_bounds.lock().unwrap() = bounds;
    }

    pub fn get_resolved_target(&self) -> Option<cu_driver::ResolvedSessionTarget> {
        self.resolved_target.lock().unwrap().clone()
    }

    pub fn set_focus_policy(&self, policy: FocusPolicy) {
        *self.focus_policy.lock().unwrap() = policy;
    }

    pub fn get_focus_policy(&self) -> FocusPolicy {
        *self.focus_policy.lock().unwrap()
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

    /// Verify a presented token for a **sensitive read** (observe / inspect /
    /// status / trace). Either the session's observation token **or** its
    /// control token verifies — control includes observation. No token is
    /// `OBSERVATION_TOKEN_REQUIRED`; any mismatch is `INVALID_OBSERVATION_TOKEN`
    /// (deliberately non-descriptive — it must not reveal which token was
    /// wrong, or even whether the presented value was close to correct).
    pub fn verify_read_token(&self, token: Option<&str>) -> Result<(), CuError> {
        self.verify_read_tokens(token, None)
    }

    /// Verify both token slots of a sensitive read. The observation slot is
    /// tried first, the control slot second (a control token is also a valid
    /// observation credential); if either verifies, the read proceeds. A
    /// missing token entirely is `OBSERVATION_TOKEN_REQUIRED`; any failure
    /// with a token present is the non-descriptive `INVALID_OBSERVATION_TOKEN`.
    pub fn verify_read_tokens(
        &self,
        observation: Option<&str>,
        control: Option<&str>,
    ) -> Result<(), CuError> {
        if let Some(t) = observation {
            if self.observation_token_hash.verify(t) {
                return Ok(());
            }
        }
        if let Some(t) = control {
            if self.control_token_hash.verify(t) {
                return Ok(());
            }
        }
        match (observation, control) {
            (None, None) => Err(CuError::ObservationTokenRequired),
            _ => Err(CuError::InvalidObservationToken),
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
        // Keep the ghost cursor's mode coherent with the session state.
        self.sync_pointer_mode(target);
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
    use cu_core::{PointerMode, SessionTarget};

    /// A test session with fresh independent control + observation hashes.
    fn test_session() -> Arc<Session> {
        let control = cu_core::generate_control_token();
        let observation = cu_core::generate_observation_token();
        Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&control),
            cu_core::SecretTokenHash::from_token(&observation),
            None,
        ))
    }

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
        let s = test_session();
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
        let s = test_session();
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
        let s = test_session();
        s.transition(SessionState::Paused).unwrap();
        s.transition(SessionState::UserTakeover).unwrap();
        assert!(s.is_user_takeover());
        assert!(!s.is_paused(), "takeover must clear the paused flag");
    }

    #[test]
    fn takeover_cancels_in_flight() {
        let s = test_session();
        let batch = s.begin_batch();
        s.transition(SessionState::UserTakeover).unwrap();
        assert!(batch.is_cancelled());
        // A fresh batch after resume gets a live token.
        s.transition(SessionState::Active).unwrap();
        let next = s.begin_batch();
        assert!(!next.is_cancelled());
    }

    #[test]
    fn virtual_pointer_is_seeded_and_updated() {
        let s = test_session();
        s.init_virtual_pointer(cu_core::Point::new(100.0, 200.0), "1");
        assert_eq!(s.virtual_pointer.lock().unwrap().x, 100.0);
        s.set_virtual_pointer(cu_core::Point::new(-1920.0, 0.0), "2");
        let vp = s.virtual_pointer.lock().unwrap();
        assert_eq!(vp.x, -1920.0);
        assert_eq!(vp.display_id, "2");
    }

    #[test]
    fn takeover_syncs_pointer_mode() {
        let s = test_session();
        s.transition(SessionState::UserTakeover).unwrap();
        assert_eq!(s.pointer_mode(), PointerMode::UserTakeover);
        s.transition(SessionState::Active).unwrap();
        assert_eq!(s.pointer_mode(), PointerMode::Isolated);
    }

    #[test]
    fn target_and_policy_are_settable() {
        let s = test_session();
        assert_eq!(s.get_pointer_policy(), PointerPolicy::IsolatedPreferred);
        s.set_pointer_policy(PointerPolicy::IsolatedOnly);
        assert_eq!(s.get_pointer_policy(), PointerPolicy::IsolatedOnly);
        s.set_target(Some(SessionTarget {
            bundle_id: Some("com.google.Chrome".into()),
            pid: Some(42),
            window_id: Some(7),
        }));
        assert_eq!(
            s.get_target().unwrap().bundle_id.as_deref(),
            Some("com.google.Chrome")
        );
        s.set_focus_policy(FocusPolicy::ActivateTarget);
        assert_eq!(s.get_focus_policy(), FocusPolicy::ActivateTarget);
    }

    #[test]
    fn read_token_accepts_observation_or_control_and_rejects_others() {
        let control = cu_core::generate_control_token();
        let observation = cu_core::generate_observation_token();
        let s = Arc::new(Session::new(
            "s".into(),
            "1".into(),
            "test".into(),
            None,
            cu_core::SecretTokenHash::from_token(&control),
            cu_core::SecretTokenHash::from_token(&observation),
            None,
        ));
        // No token → OBSERVATION_TOKEN_REQUIRED.
        assert!(matches!(
            s.verify_read_token(None),
            Err(CuError::ObservationTokenRequired)
        ));
        // A wrong token → INVALID_OBSERVATION_TOKEN (never *which* was wrong).
        assert!(matches!(
            s.verify_read_token(Some("wrong")),
            Err(CuError::InvalidObservationToken)
        ));
        // The observation token verifies.
        assert!(s.verify_read_token(Some(observation.as_str())).is_ok());
        // The control token verifies too — control includes observation. It
        // proves itself in the control slot (a single-arg verify_read_token
        // only ever presents an observation credential).
        assert!(s.verify_read_tokens(None, Some(control.as_str())).is_ok());
        // The observation token does NOT verify as a control token.
        assert!(matches!(
            s.verify_control_token(Some(observation.as_str())),
            Err(CuError::InvalidControlToken)
        ));
    }
}
