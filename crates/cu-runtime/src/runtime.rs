//! The computer-use runtime: owns sessions, frames, the control lock, and the
//! observe / act / inspect / session / trace operations. Platform-agnostic —
//! it talks only to a [`cu_driver::ComputerDriver`].
//!
//! The runtime enforces, in order, every safety invariant from the spec:
//! 1. a request only reaches the driver when its **session** exists and is
//!    **active** (paused / user-takeover / stopped sessions are rejected);
//! 2. the referenced **frame** is known and not **stale**;
//! 3. every coordinate is **in bounds**;
//! 4. confirmation-required batches are rejected unless authorized;
//! 5. the human grabbing the mouse **pauses or takes over** the session.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::Utc;
use cu_core::{
    generate_control_token, generate_observation_token, ActParams, ActResult, ClientInfo,
    CoordinateSpace, CuError, ErrorCode, ImageGeometry, InspectMapping, InspectParams,
    InspectResult, ObserveParams, ObserveResult, PointerMode, Region, RequestKey, ScreenFrame,
    ScreenSnapshot, SecretToken, SecretTokenHash, SessionAction, SessionResult, SessionState,
    SessionSummary, StabilizationInfo, TraceReport, WaitPolicy,
};
use cu_driver::{
    expected_crop_output_size, ApplicationInfo, CaptureRegion, CaptureRequest, ComputerDriver,
    DesktopLayout, DisplayInfo, PermissionStatus, PointerInfo,
};
use cu_policy::confirmation::Authorization;
use cu_policy::{authorize, ConfirmationPolicy, TakeoverDetector, TakeoverPolicy};
use cu_trace::{TraceConfig, TraceMode, TraceRecorder};

use crate::action_queue::{to_action_result_reports, ActionQueue};
use crate::frames::FrameStore;
use crate::human_input::HumanInputMonitor;
use crate::requests::RequestRegistry;
use crate::sessions::{ControlLock, Session, SharedSession};
use crate::stabilizer::{StabilizeOutcome, Stabilizer, StabilizerConfig};
use crate::stale_frame::{StaleFrameChecker, StaleFrameConfig};

/// Everything the runtime needs beyond a driver.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub stale: StaleFrameConfig,
    pub stabilizer: StabilizerConfig,
    pub frame_cache_limit: usize,
    pub observe_max_width: u32,
    pub observe_format: String,
    pub observe_jpeg_quality: u8,
    pub traces_dir: PathBuf,
    pub frames_dir: PathBuf,
    pub trace_dev_mode: bool,
    /// How strictly traces are recorded (required / best_effort / disabled).
    pub trace_mode: TraceMode,
    pub takeover_policy: TakeoverPolicy,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            stale: StaleFrameConfig::default(),
            stabilizer: StabilizerConfig::default(),
            frame_cache_limit: cu_core::config::DEFAULT_FRAME_CACHE_LIMIT,
            observe_max_width: cu_core::config::DEFAULT_OBSERVE_MAX_WIDTH,
            observe_format: cu_core::config::DEFAULT_OBSERVE_FORMAT.to_string(),
            observe_jpeg_quality: cu_core::config::DEFAULT_OBSERVE_JPEG_QUALITY,
            traces_dir: cu_core::config::traces_dir(),
            frames_dir: cu_core::config::frames_dir(),
            trace_dev_mode: false,
            trace_mode: TraceMode::BestEffort,
            takeover_policy: TakeoverPolicy::AutoPause,
        }
    }
}

/// A running runtime bound to one driver.
pub struct Runtime {
    pub driver: Arc<dyn ComputerDriver>,
    pub config: RuntimeConfig,
    control_lock: ControlLock,
    sessions: Mutex<std::collections::HashMap<String, SharedSession>>,
    frames: Mutex<FrameStore>,
    frame_counter: AtomicU64,
    stale: StaleFrameChecker,
    started_at: Instant,
    /// Per-request cancellation registry (connection_id, request_id) → batch
    /// token. Lets `computer.cancel` abort exactly one request.
    requests: RequestRegistry,
    /// Set by `shutdown` the moment it starts. Every dispatch checks it first,
    /// so requests that arrive during shutdown fail fast with
    /// `DAEMON_SHUTTING_DOWN` instead of starting new work.
    shutting_down: std::sync::atomic::AtomicBool,
    /// Continuous human-input detector (Human Always Wins). The macOS driver
    /// feeds it via an Event Tap; the action queue polls it before each action.
    pub human_input: std::sync::Arc<HumanInputMonitor>,
}

impl Runtime {
    /// Whether `shutdown` has begun. New requests are refused from this point.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Runtime {
    pub fn new(driver: Arc<dyn ComputerDriver>, config: RuntimeConfig) -> Self {
        let stale = StaleFrameChecker::new(config.stale);
        let frames = Mutex::new(FrameStore::new(config.frame_cache_limit));
        Self {
            driver,
            config,
            control_lock: ControlLock::new(),
            sessions: Mutex::new(std::collections::HashMap::new()),
            frames,
            frame_counter: AtomicU64::new(0),
            stale,
            started_at: Instant::now(),
            requests: RequestRegistry::new(),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            human_input: std::sync::Arc::new(HumanInputMonitor::new()),
        }
    }

    /// Path where session traces are written (used by the daemon's trace RPCs).
    pub fn traces_dir(&self) -> &std::path::Path {
        &self.config.traces_dir
    }

    /// P0-1: a real hardware event (Event Tap) must cancel the ACTIVE batch at
    /// event time — not merely set a flag the next loop iteration reads. The
    /// daemon calls this once at startup. The hook runs synchronously on the
    /// Event Tap thread and, for the control-holder session:
    ///
    /// 1. **cancels** its in-flight batches — a long-running action (drag /
    ///    scroll / wait via `execute_with_cancel`) aborts at event time;
    /// 2. **transitions** it to `UserTakeover` — this is a synchronous,
    ///    no-await operation (state mutex + token list + virtual-pointer
    ///    mutex), so it is safe on the Event Tap thread.
    ///
    /// The real-takeover flag is intentionally left set: the action queue
    /// thread consumes it to perform the async ghost-cursor hide and record
    /// the interrupt metrics. `Weak` (not `Arc`) is captured so a dropped
    /// runtime does not leak.
    pub fn install_human_takeover_hook(&self, rt: Arc<Self>) {
        let weak = Arc::downgrade(&rt);
        let hook: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
            let Some(rt) = weak.upgrade() else { return };
            // The control-lock holder is the only session that can drive the
            // pointer; cancelling its batches aborts the in-flight action and
            // any queued batch immediately.
            let Some(sid) = rt.control_lock.holder() else {
                return;
            };
            let Some(session) = rt.sessions.lock().unwrap().get(&sid).cloned() else {
                return;
            };
            session.cancel_in_flight();
            // Synchronous (no-await) state transition so the session reflects
            // the takeover immediately, even before the queue consumes the flag.
            let _ = session.transition(cu_core::SessionState::UserTakeover);
        });
        self.human_input.set_real_takeover_hook(hook);
    }

    /// P0-1: consume a pending real takeover and complete the transition
    /// (state, pointer mode, interrupt metrics, ghost-cursor hide). Used by the
    /// between-batch waits so a human event that lands *after* the queue
    /// drained still forces `UserTakeover`. Returns true if applied.
    async fn consume_real_takeover(&self, session: &SharedSession) -> bool {
        if self.human_input.consume_real_takeover() {
            let _ = session.transition(cu_core::SessionState::UserTakeover);
            session.sync_pointer_mode(cu_core::SessionState::UserTakeover);
            self.human_input.mark_takeover_started();
            // P0-4: input-stop is measured from `last_synthetic_event_at`, never
            // force-marked at takeover time — a stamp set here would fabricate
            // the KPI (the agent's last input may long predate the takeover).
            let _ = self.driver.pointer_hidden().await;
            true
        } else {
            false
        }
    }

    // ------------------------------------------------------------------
    // Introspection (used by `runtime.health` / `runtime.permissions` /
    // `runtime.displays` and the inspector)
    // ------------------------------------------------------------------

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| matches!(s.state(), SessionState::Active | SessionState::Starting))
            .count()
    }

    /// The id of the session currently holding the control lock, if any.
    pub fn active_session_id(&self) -> Option<String> {
        self.control_lock.holder()
    }

    pub async fn permissions(&self) -> Result<PermissionStatus, CuError> {
        self.driver.permission_status().await
    }

    pub async fn displays(&self) -> Result<Vec<DisplayInfo>, CuError> {
        self.driver.list_displays().await
    }

    pub async fn desktop_layout(&self) -> Result<DesktopLayout, CuError> {
        self.driver.desktop_layout().await
    }

    pub async fn active_application(&self) -> Result<Option<ApplicationInfo>, CuError> {
        self.driver.active_application().await
    }

    pub async fn pointer_location(&self) -> Result<PointerInfo, CuError> {
        self.driver.pointer_location().await
    }

    pub async fn health(&self) -> Result<serde_json::Value, CuError> {
        let permissions = self.permissions().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "permission probe failed during health");
            PermissionStatus {
                screen_recording: false,
                accessibility: false,
            }
        });
        let human_monitor = self.driver.human_input_monitor_state();
        Ok(serde_json::json!({
            "version": cu_core::config::RUNTIME_VERSION,
            "ready": permissions.all_granted(),
            "permissions": permissions,
            "active_sessions": self.active_session_count(),
            "uptime_secs": self.uptime_secs(),
            "frame_cache": self.frames.lock().unwrap().len(),
            "human_input_monitor": human_monitor,
        }))
    }

    /// Recreate the driver's native resources (kills the Swift bridge so the
    /// next request respawns it). Used after a timed-out request that might
    /// have left a stale response in the bridge pipe.
    pub async fn restart_bridge(&self) -> Result<(), CuError> {
        self.driver.shutdown().await
    }

    /// Cancel any in-flight action batch for a session (e.g. an explicit
    /// `computer.cancel` request).
    /// Session-wide cancellation: cancels every in-flight batch on the
    /// session. Verifies the control token first — cancelling is a mutating
    /// operation and must not be triggerable by a session-id-only caller —
    /// and refuses stopped sessions (`SESSION_STOPPED`): a stopped session's
    /// token no longer grants anything.
    ///
    /// Records a `cancel` trace event so reports can measure cancel latency
    /// (the event timestamp vs. the cancelled batch's final action).
    pub async fn cancel_in_flight(
        &self,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<(), CuError> {
        let session = self.get_session(session_id)?;
        self.verify_mutating(&session, control_token)?;
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event("cancel", serde_json::json!({ "scope": "session" }))
                .await;
        }
        session.cancel_in_flight();
        Ok(())
    }

    /// Precise cancellation: aborts exactly the request named by `key`
    /// `(connection_id, request_id)` on this connection. The token is verified
    /// against the session's hash first; a wrong token never touches anything.
    ///
    /// Returns `Ok(true)` when a live request was cancelled, `Ok(false)` when
    /// no such request was registered (already finished, or it will be
    /// cancelled at registration via the tombstone path).
    pub async fn cancel_request(
        &self,
        key: &RequestKey,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<bool, CuError> {
        let session = self.get_session(session_id)?;
        self.verify_mutating(&session, control_token)?;
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event(
                    "cancel",
                    serde_json::json!({ "scope": "request", "request_id": key.request_id }),
                )
                .await;
        }
        self.requests.cancel(key, session_id)
    }

    /// Number of registered in-flight requests (tests).
    pub fn request_count(&self) -> usize {
        self.requests.len()
    }

    // ------------------------------------------------------------------
    // Session lifecycle
    // ------------------------------------------------------------------

    pub async fn session_start(
        &self,
        display_id: Option<String>,
        client: ClientInfo,
        target: Option<cu_core::SessionTarget>,
        pointer_policy: Option<cu_core::PointerPolicy>,
        focus_policy: Option<cu_core::FocusPolicy>,
    ) -> Result<SessionResult, CuError> {
        // Round 8: the start-time isolation options. The SANE DEFAULTS are
        // `isolated_preferred` (never silently borrow the user's cursor) and
        // `strict` (never steal foreground). An explicit target scopes the
        // session to that app/window; bounds are resolved by the caller's
        // adapter (true window resolution needs a live desktop, so default
        // bounds are None and a strict target without resolved bounds still
        // enforces the Focus Guard in `act`).
        let id = format!("s_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let display_id = match display_id {
            Some(d) => d,
            None => self.driver.desktop_layout().await?.primary_id,
        };

        // The two session capabilities, generated **independently** from the
        // OS CSPRNG, issued exactly once (in the start response), stored only
        // as hashes. Neither may ever appear in logs, traces, or `status`.
        // The control token authorizes every mutating operation; the
        // observation token authorizes sensitive reads only.
        let control_token = generate_control_token();
        let control_token_hash = SecretTokenHash::from_token(&control_token);
        let observation_token = generate_observation_token();
        let observation_token_hash = SecretTokenHash::from_token(&observation_token);

        // Persist the trace access manifest (token hashes only — plaintext
        // never touches disk): after a daemon restart the session is gone
        // from memory but its trace files remain, and the manifest is what
        // lets the token holder prove access again. A failure degrades
        // gracefully: live access still works, only restart-persistence
        // is lost.
        if let Err(e) = cu_trace::manifest::write_manifest(
            &self.config.traces_dir,
            &id,
            &control_token_hash,
            &observation_token_hash,
        ) {
            tracing::warn!(session = %id, error = %e, "trace access manifest unavailable; trace access will not survive a daemon restart");
        }

        let trace = match self.config.trace_mode {
            // Disabled: no recorder at all; trace RPCs simply find nothing.
            TraceMode::Disabled => None,
            _ => match TraceRecorder::open(
                &id,
                &self.config.traces_dir,
                TraceConfig {
                    dev_mode: self.config.trace_dev_mode,
                    mode: self.config.trace_mode,
                },
            )
            .await
            {
                Ok(t) => Some(t),
                Err(e) => {
                    if self.config.trace_mode == TraceMode::Required {
                        // required: a session whose trace cannot be recorded
                        // must not start silently without one.
                        return Err(e);
                    }
                    tracing::warn!(session = %id, error = %e, "trace recorder unavailable; continuing without traces (degraded)");
                    None
                }
            },
        };

        let session = SharedSession::new(Session::new(
            id.clone(),
            display_id,
            client.client_name.clone(),
            Some(client),
            control_token_hash,
            observation_token_hash,
            trace,
        ));

        // Apply the start-time isolation options. `act` reads these from the
        // session: pointer policy gates physical fallback, target + focus
        // policy gate the keyboard focus guard.
        if let Some(t) = target {
            // Round 9 / P0-4: the RUNTIME resolves the target through the
            // DRIVER (never the adapter) BEFORE storing it, and FAIL-CLOSED:
            // a caller that explicitly scopes the session to an app/window
            // must get a concrete, fully-identified window or no session at
            // all. Silently starting unbound would let the agent operate on
            // arbitrary windows, defeating the target's isolation intent.
            //
            // `Ok(None)` => the window is genuinely gone / identity mismatch;
            // `Err` => the driver could not resolve. Both fail the start, and
            // the trace + manifest opened above are cleaned up so no orphaned
            // session artifacts remain (mirrors the CONTROL_LOCKED path).
            let resolved = match self.driver.resolve_target(&t).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    if let Some(t) = session.trace.as_ref() {
                        let _ = t.close().await;
                    }
                    let _ = cu_trace::manifest::remove_manifest(&self.config.traces_dir, &id);
                    return Err(cu_core::CuError::TargetUnavailable);
                }
                Err(e) => {
                    if let Some(t) = session.trace.as_ref() {
                        let _ = t.close().await;
                    }
                    let _ = cu_trace::manifest::remove_manifest(&self.config.traces_dir, &id);
                    return Err(e);
                }
            };
            // P0-4 full identity: backfill the caller's (possibly partial)
            // target intent with the resolved window's complete identity, so
            // downstream guards (Focus Guard, observe window-scoping) never
            // depend on the caller having supplied every field. The driver
            // verified PID/bundle consistency during resolution.
            let mut t = t;
            t.bundle_id
                .get_or_insert_with(|| resolved.bundle_id.clone());
            if t.pid.is_none() {
                t.pid = Some(resolved.pid as i64);
            }
            if t.window_id.is_none() {
                t.window_id = Some(resolved.window_id as i64);
            }
            session.set_target(Some(t));
            session.set_resolved_target(Some(resolved));
        }
        if let Some(pp) = pointer_policy {
            session.set_pointer_policy(pp);
        }
        if let Some(fp) = focus_policy {
            session.set_focus_policy(fp);
        }

        // Acquire the global control lock. Held for the session's lifetime.
        // A lock held by another session fails with CONTROL_LOCKED carrying
        // the holder's (non-secret) owner identity, so the rejected client can
        // say "Owner: OpenCode" without ever seeing a token.
        if let Err(e) = self.control_lock.try_acquire(&id) {
            if let Some(t) = session.trace.as_ref() {
                let _ = t.close().await;
            }
            // The session never went live and its tokens never left the
            // daemon — do not leave an access record behind.
            let _ = cu_trace::manifest::remove_manifest(&self.config.traces_dir, &id);
            return Err(self.with_holder_owner(e));
        }

        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());

        // Round 8 / Phase 4: seed the session's VIRTUAL POINTER from the live
        // system cursor position. This is a pure READ — we only borrow the
        // position once as the agent's starting coordinate; the system cursor
        // still belongs to the user. Then show the ghost cursor overlay so the
        // user can see where the agent points without the real cursor moving.
        if let Ok(pi) = self.driver.pointer_location().await {
            session.init_virtual_pointer(pi.location, session.display_id.clone());
            let _ = self
                .driver
                .pointer_visualized(
                    pi.location.x,
                    pi.location.y,
                    &session.display_id,
                    PointerMode::Isolated,
                )
                .await;
        }

        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event(
                    "session.start",
                    serde_json::json!({
                        "display_id": session.display_id,
                        "started_by": session.started_by,
                        "client_id": session.owner.as_ref().map(|c| c.client_id.clone()),
                        "client_name": session.owner.as_ref().map(|c| c.client_name.clone()),
                        "client_instance_id": session.owner.as_ref().map(|c| c.client_instance_id.clone()),
                    }),
                )
                .await;
        }
        tracing::info!(session = %id, "session started");
        let mut result = self.session_result(&session);
        // The one and only disclosure of the session's capabilities: the
        // response to the client that just created the session. The control
        // token covers mutating operations; the observation token covers
        // sensitive reads (and is what a read-only attach must hold).
        result.control_token = Some(SecretToken::new(control_token.as_str()));
        result.observation_token = Some(SecretToken::new(observation_token.as_str()));
        Ok(result)
    }

    /// The **public** session view: coarse state + non-secret owner identity.
    /// No token required — this never leaks display ids, frame ids, trace
    /// paths, or any token. Full `status` is a sensitive read.
    pub fn session_summary(&self) -> SessionSummary {
        let holder = self.active_session_id();
        let Some(id) = holder else {
            return SessionSummary {
                session_id: None,
                state: None,
                lock_held: false,
                owner_client_id: None,
                owner_client_name: None,
                message: None,
            };
        };
        let session = self.sessions.lock().unwrap().get(&id).cloned();
        let mut summary = SessionSummary {
            session_id: Some(id.clone()),
            state: session.as_ref().map(|s| s.state()),
            lock_held: true,
            owner_client_id: session
                .as_ref()
                .and_then(|s| s.owner.as_ref())
                .map(|o| o.client_id.clone()),
            owner_client_name: session
                .as_ref()
                .and_then(|s| s.owner.as_ref())
                .map(|o| o.client_name.clone()),
            message: None,
        };
        let session = session.as_ref();
        let owner_name = session
            .and_then(|s| s.owner.as_ref())
            .map(|o| o.client_name.as_str())
            .unwrap_or("another client");
        summary.message = Some(format!(
            "Active session {id} is owned by {owner_name}; knowing its id grants no observation or control permission."
        ));
        summary
    }

    /// Full session status — a sensitive read. Requires the observation **or**
    /// control token (the caller may pass either slot); it never returns a
    /// token itself.
    pub async fn session_status(
        &self,
        session_id: &str,
        observation_token: Option<&str>,
        control_token: Option<&str>,
    ) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        // Verify *before* returning any field: display_id, frame_id, trace_dir
        // and the rest are private until the capability is proven.
        session.verify_read_tokens(observation_token, control_token)?;
        Ok(self.session_result(&session))
    }

    /// Verify the token and refuse operations on a stopped/failed session
    /// with the dedicated codes. Pause/resume/takeover/release/stop are the
    /// only mutating session ops; every one verifies *before* any state change
    /// or trace write.
    fn verify_mutating(
        &self,
        session: &Session,
        control_token: Option<&str>,
    ) -> Result<(), CuError> {
        session.verify_control_token(control_token)?;
        match session.state() {
            SessionState::Stopping | SessionState::Stopped => Err(CuError::SessionStopped),
            SessionState::Failed => Err(CuError::InvalidSessionState("session is failed".into())),
            _ => Ok(()),
        }
    }

    pub async fn session_pause(
        &self,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        self.verify_mutating(&session, control_token)?;
        session.transition(SessionState::Paused)?;
        if let Some(t) = session.trace.as_ref() {
            let _ = t.record_event("session.pause", serde_json::json!({})).await;
        }
        Ok(self.session_result(&session))
    }

    pub async fn session_resume(
        &self,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        self.verify_mutating(&session, control_token)?;
        match session.state() {
            // Only a paused session can be resumed by the agent. A session the
            // user took over cannot be resumed — `release` is the only exit.
            SessionState::UserTakeover => Err(CuError::UserTakeoverActive),
            SessionState::Paused => {
                session.transition(SessionState::Active)?;
                if let Some(t) = session.trace.as_ref() {
                    let _ = t
                        .record_event("session.resume", serde_json::json!({}))
                        .await;
                }
                Ok(self.session_result(&session))
            }
            state => Err(CuError::InvalidSessionState(format!(
                "cannot resume a session in state {state:?}"
            ))),
        }
    }

    /// Human takes over: session enters `user_takeover`, in-flight actions are
    /// cancelled. `release` is the inverse (agent resumes control).
    pub async fn session_takeover(
        &self,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        self.verify_mutating(&session, control_token)?;
        session.transition(SessionState::UserTakeover)?;
        // P0-1: ghost cursor must hide immediately on takeover — the user is
        // back in control and the agent's overlay must not linger.
        let _ = self.driver.pointer_hidden().await;
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event("session.takeover", serde_json::json!({}))
                .await;
        }
        Ok(self.session_result(&session))
    }

    /// Hand control back to the agent. Only valid while the user holds the
    /// session (state `UserTakeover`); releasing an active/paused session is
    /// a no-op error, not a silent state flip.
    pub async fn session_release(
        &self,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        self.verify_mutating(&session, control_token)?;
        if session.state() != SessionState::UserTakeover {
            return Err(CuError::InvalidSessionState(format!(
                "cannot release a session in state {:?}; release is only valid while the user holds control",
                session.state()
            )));
        }
        session.transition(SessionState::Active)?;
        // Round 9 / P0-12: on release the agent is back in control — the
        // ghost cursor must re-appear so the user can see where the agent
        // points (takeover hid it). Drive it from the session's virtual
        // pointer position, in the session's CURRENT pointer mode (after the
        // transition above that is `Isolated`; using the live mode keeps the
        // overlay honest if a mode ever differs).
        {
            // Snapshot the coordinates + mode first: the MutexGuard must not
            // live across the await (the driver may itself touch the pointer).
            let (vx, vy, vd, mode) = {
                let vp = session.virtual_pointer.lock().unwrap();
                (vp.x, vp.y, vp.display_id.clone(), vp.mode)
            };
            let _ = self.driver.pointer_visualized(vx, vy, &vd, mode).await;
        }
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event("session.release", serde_json::json!({}))
                .await;
        }
        Ok(self.session_result(&session))
    }

    /// Stop a session: cancel in-flight work, close its trace, release the
    /// control lock, and mark it `stopped`. Idempotent (a stopped session
    /// reports `Ok` again), but only ever with a valid control token.
    pub async fn session_stop(
        &self,
        session_id: &str,
        control_token: Option<&str>,
    ) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        session.verify_control_token(control_token)?;
        self.stop_session(&session).await
    }

    /// The stop routine itself; callers are responsible for having verified
    /// the control token (or being the runtime's own shutdown path).
    async fn stop_session(&self, session: &SharedSession) -> Result<SessionResult, CuError> {
        let session_id = session.id.clone();
        match session.state() {
            SessionState::Stopped => return Ok(self.session_result(session)),
            SessionState::Stopping => {
                session.set_state(SessionState::Stopped);
                self.control_lock.release(&session_id);
                return Ok(self.session_result(session));
            }
            _ => {}
        }
        session.transition(SessionState::Stopping)?; // cancels in-flight
                                                     // Round 8 / Phase 4: session stop must destroy the ghost cursor
                                                     // overlay — never leave a window, timer, or run-loop source behind.
        let _ = self.driver.pointer_hidden().await;
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event("session.stop", serde_json::json!({ "reason": "requested" }))
                .await;
            let _ = t.close().await;
        }
        // Stamp the trace access manifest so history shows the session ended.
        // Best-effort: a missing manifest only loses the timestamp.
        if let Err(e) = cu_trace::manifest::mark_stopped(&self.config.traces_dir, &session_id) {
            tracing::warn!(session = %session_id, error = %e, "could not stamp trace manifest stopped_at");
        }
        session.set_state(SessionState::Stopped);
        self.control_lock.release(&session_id);
        tracing::info!(session = %session_id, "session stopped");
        Ok(self.session_result(session))
    }

    /// Dispatch a `computer.session` action.
    ///
    /// `control_token` is required for every mutating action except `start`
    /// (which creates the session and issues the tokens). `status` is a
    /// sensitive read: it requires the observation **or** control token.
    #[allow(clippy::too_many_arguments)] // action boundary: every arg is a distinct execution context
    pub async fn session(
        &self,
        action: SessionAction,
        session_id: Option<&str>,
        display_id: Option<String>,
        client: ClientInfo,
        control_token: Option<&str>,
        observation_token: Option<&str>,
        start_options: SessionStartOptions,
    ) -> Result<SessionResult, CuError> {
        match action {
            SessionAction::Start => {
                self.session_start(
                    display_id,
                    client,
                    start_options.target,
                    start_options.pointer_policy,
                    start_options.focus_policy,
                )
                .await
            }
            SessionAction::Status => match session_id {
                Some(id) => {
                    self.session_status(id, observation_token, control_token)
                        .await
                }
                None => {
                    // No active session is a *typed* error, not a malformed
                    // request: adapters auto-start on SESSION_NOT_FOUND and
                    // must not have to sniff an INVALID_PARAMS message.
                    let holder = self.active_session_id().ok_or_else(|| {
                        CuError::SessionNotFound("No active computer-use session exists.".into())
                    })?;
                    self.session_status(&holder, observation_token, control_token)
                        .await
                }
            },
            SessionAction::Pause => {
                self.session_pause(
                    session_id.ok_or_else(|| {
                        CuError::InvalidParams("session.pause requires session_id".into())
                    })?,
                    control_token,
                )
                .await
            }
            SessionAction::Resume => {
                self.session_resume(
                    session_id.ok_or_else(|| {
                        CuError::InvalidParams("session.resume requires session_id".into())
                    })?,
                    control_token,
                )
                .await
            }
            SessionAction::Takeover => {
                self.session_takeover(
                    session_id.ok_or_else(|| {
                        CuError::InvalidParams("session.takeover requires session_id".into())
                    })?,
                    control_token,
                )
                .await
            }
            SessionAction::Release => {
                self.session_release(
                    session_id.ok_or_else(|| {
                        CuError::InvalidParams("session.release requires session_id".into())
                    })?,
                    control_token,
                )
                .await
            }
            SessionAction::Stop => {
                self.session_stop(
                    session_id.ok_or_else(|| {
                        CuError::InvalidParams("session.stop requires session_id".into())
                    })?,
                    control_token,
                )
                .await
            }
        }
    }

    // ------------------------------------------------------------------
    // Observe
    // ------------------------------------------------------------------

    pub async fn observe(
        &self,
        params: ObserveParams,
        request_id: Option<String>,
    ) -> Result<ObserveResult, CuError> {
        let session_id = params
            .session_id
            .clone()
            .ok_or_else(|| CuError::InvalidParams("observe requires session_id".into()))?;
        let session = self.get_session(&session_id)?;
        // Observing captures the desktop: a capability token (observation or
        // control) is verified *before* the busy lock, the frame counter, or
        // any capture — a tokenless or wrong-token observe has zero side
        // effects (no frame written, no frame_id consumed, no trace entry).
        session.verify_read_tokens(
            params.observation_token.as_deref(),
            params.control_token.as_deref(),
        )?;
        self.gate_active(&session)?;
        let _busy = session.busy.lock().await;
        self.observe_inner(&session, params, request_id).await
    }

    /// P0-3: refresh a target-scoped session's resolved target BEFORE every
    /// observe / inspect, so window bounds are always current. Fail-closed:
    /// a resolver error, a vanished window, or a window whose identity no
    /// longer matches the session's target all yield `TARGET_UNAVAILABLE` —
    /// NEVER stale bounds, and NEVER an auto-picked sibling window of the same
    /// app. This is the shared refresh path for observe and inspect; there is
    /// deliberately no best-effort fallback: an isolation system does not
    /// "continue with the old bounds".
    async fn refresh_resolved_target(
        &self,
        session: &Session,
    ) -> Result<cu_driver::ResolvedSessionTarget, CuError> {
        let target = session.get_target().or_else(|| {
            session
                .get_resolved_target()
                .map(|rt| cu_core::SessionTarget {
                    bundle_id: Some(rt.bundle_id),
                    pid: Some(rt.pid as i64),
                    window_id: Some(rt.window_id as i64),
                })
        });
        let target = match target {
            Some(t) => t,
            // Not target-scoped: nothing to refresh.
            None => return Err(CuError::TargetUnavailable),
        };
        // Fail closed on EVERY resolver outcome except a fully-identified
        // window: Err (bridge/resolver failure) and Ok(None) (window gone)
        // both mean "no capture, no act".
        let resolved = match self.driver.resolve_target(&target).await {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => return Err(CuError::TargetUnavailable),
        };
        // Identity re-check: the resolved window MUST be the very window the
        // session is bound to. A different window_id (even the same app /
        // bundle — the original window closed and another took its place)
        // means fail closed, never re-scope.
        if let Some(wid) = target.window_id {
            if resolved.window_id as i64 != wid {
                return Err(CuError::TargetUnavailable);
            }
        }
        if let Some(b) = &target.bundle_id {
            if resolved.bundle_id != *b {
                return Err(CuError::TargetUnavailable);
            }
        }
        if let Some(pid) = target.pid {
            if resolved.pid as i64 != pid {
                return Err(CuError::TargetUnavailable);
            }
        }
        session.set_resolved_target(Some(resolved.clone()));
        Ok(resolved)
    }

    /// Round 8 / P0-2: the pixel-space crop region (relative to the display's
    /// top-left) that isolates the target window for snapshots. Returns
    /// `Ok(None)` when the session is not target-scoped — callers then fall
    /// back to a full-desktop fingerprint. Fails when the display cannot be
    /// found, since no crop can be computed then.
    async fn target_crop_region(
        &self,
        session: &Session,
        bounds: &cu_core::DisplayBounds,
    ) -> Result<Option<CaptureRegion>, CuError> {
        if session.get_target().is_none() && session.get_resolved_target().is_none() {
            return Ok(None);
        }
        let displays = self.driver.list_displays().await?;
        let disp = displays
            .iter()
            .find(|d| d.id == session.display_id)
            .ok_or_else(|| {
                CuError::Driver(format!("act: display {} not found", session.display_id))
            })?;
        let s = disp.scale_factor.max(f64::EPSILON);
        Ok(Some(CaptureRegion {
            x: (bounds.x - disp.bounds.x) * s,
            y: (bounds.y - disp.bounds.y) * s,
            width: bounds.width * s,
            height: bounds.height * s,
        }))
    }

    /// Round 8 / P0-2: the stale-frame / fingerprint / stabilizer snapshot
    /// for a session. Target-scoped sessions sample the TARGET WINDOW CROP
    /// (never the full desktop) and pin `active_application` to the target's
    /// OWN bundle — so an app change in another app can never trip staleness
    /// (section 十二) and the target's own app change can never self-stale a
    /// target frame (section 十三). Non-target sessions keep the full-desktop
    /// snapshot with the live frontmost app (unchanged pre-P0-2 semantics).
    async fn fingerprint_snapshot(
        &self,
        session: &Session,
        region_px: Option<CaptureRegion>,
    ) -> Result<cu_driver::QuickSnapshot, CuError> {
        if region_px.is_some()
            && (session.get_target().is_some() || session.get_resolved_target().is_some())
        {
            let mut q = self
                .driver
                .quick_snapshot_region(&session.display_id, region_px)
                .await?;
            if let Some(rt) = session.get_resolved_target() {
                q.active_application = Some(cu_driver::ApplicationInfo {
                    bundle_id: rt.bundle_id.clone(),
                    name: rt.bundle_id.clone(),
                    window_title: None,
                    pid: Some(rt.pid),
                    window_id: Some(rt.window_id),
                });
            }
            Ok(q)
        } else {
            self.driver.quick_snapshot(&session.display_id).await
        }
    }

    /// The observe core, called while the session's busy lock is held (either
    /// by [`Runtime::observe`] or by `act`'s post-batch re-observe).
    async fn observe_inner(
        &self,
        session: &Session,
        params: ObserveParams,
        request_id: Option<String>,
    ) -> Result<ObserveResult, CuError> {
        let session_id = session.id.clone();
        let display_id = params
            .display_id
            .clone()
            .unwrap_or_else(|| session.display_id.clone());

        let counter = self.frame_counter.fetch_add(1, Ordering::SeqCst);
        let frame_id = cu_core::config::new_frame_id(counter);
        let format = params
            .image_format
            .clone()
            .unwrap_or_else(|| self.config.observe_format.clone());
        let ext = if format == "png" { "png" } else { "jpg" };

        tokio::fs::create_dir_all(&self.config.frames_dir)
            .await
            .map_err(|e| CuError::Driver(format!("cannot create frames dir: {e}")))?;
        let output_path = self
            .config
            .frames_dir
            .join(format!("{session_id}_{counter}.{ext}"));

        // P0-6: window-scoped observe. When the session is scoped to a target
        // window with known bounds, the capture is CROPPED to that window so
        // the model sees only the target (never neighboring apps / chrome), and
        // the stored frame geometry becomes window-relative — coordinates in
        // the model's screenshot map straight to the window's global bounds.
        // P0-3: the target is REFRESHED before every observe (never the stale
        // session-start bounds) — the window may have moved / resized / closed
        // since the last capture. Fail-closed: a vanished or re-identified
        // window is TARGET_UNAVAILABLE, not a stale-bounds crop.
        let window_scope: Option<(cu_core::DisplayBounds, u32)> =
            if session.get_target().is_some() || session.get_resolved_target().is_some() {
                let rt = self.refresh_resolved_target(session).await?;
                rt.bounds.map(|b| (b, rt.window_id))
            } else {
                None
            };
        let region_px: Option<CaptureRegion> = if let Some((wb, _)) = window_scope {
            // Window bounds are global logical points; the crop must be in the
            // captured display's pixel space. `list_displays` provides the
            // display's global origin and scale factor.
            let displays = self.driver.list_displays().await?;
            let disp = displays
                .iter()
                .find(|d| d.id == display_id)
                .ok_or_else(|| {
                    CuError::Driver(format!("observe: display {display_id} not found"))
                })?;
            let s = disp.scale_factor.max(f64::EPSILON);
            Some(CaptureRegion {
                x: (wb.x - disp.bounds.x) * s,
                y: (wb.y - disp.bounds.y) * s,
                width: wb.width * s,
                height: wb.height * s,
            })
        } else {
            None
        };

        let request = CaptureRequest {
            display_id: display_id.clone(),
            output_path: output_path.clone(),
            include_cursor: params.include_cursor.unwrap_or(true),
            max_width: params.max_width.unwrap_or(self.config.observe_max_width),
            format: format.clone(),
            jpeg_quality: params
                .jpeg_quality
                .unwrap_or(self.config.observe_jpeg_quality),
            region: region_px,
        };
        // A capture failure is a CAPTURE_FAILED error, distinct from generic
        // driver failures, so agents can distinguish "screen could not be
        // captured" from other bridge problems.
        // Snapshot the max_width before `request` is moved into the driver.
        let request_max_width = request.max_width;
        let captured = self
            .driver
            .capture(request)
            .await
            .map_err(|e| CuError::CaptureFailed(e.to_string()))?;

        // P0-6/P0-2 fail-closed: a window crop was requested; if the driver
        // could not produce a matching image (the window moved off-screen and
        // the crop fell back to the full frame), surface TARGET_UNAVAILABLE
        // rather than label a full-display image as window-scoped. The expected
        // size is the CROP possibly downscaled to `max_width` — the Swift
        // bridge legitimately shrinks a crop wider than max_width (P0-1), so
        // comparing against the raw crop size would spuriously reject every
        // wide-target capture. Tolerance is a documented ±1px for Swift/Rust
        // rounding differences; any larger mismatch is a real isolation break.
        if let Some(r) = region_px {
            let (exp_w, exp_h) = expected_crop_output_size(
                r.width.round() as u32,
                r.height.round() as u32,
                request_max_width,
            );
            if captured.width.abs_diff(exp_w) > 1 || captured.height.abs_diff(exp_h) > 1 {
                return Err(CuError::TargetUnavailable);
            }
        }

        // A cheap snapshot gives us the thumbnail + live app name for the
        // stale-frame fingerprint of this frame. Round 8 / P0-2: for a
        // window-scoped session this comes from the TARGET WINDOW CROP (never
        // the full desktop) with `active_application` pinned to the target's
        // own bundle, so other-app changes never stale this frame and the
        // target's own app change never self-stales it.
        let snapshot: ScreenSnapshot = self.fingerprint_snapshot(session, region_px).await?.into();

        // P0-6: a window-scoped frame's geometry is WINDOW-relative — the
        // stored bounds are the window's global bounds so `act` maps model
        // coordinates in the cropped image straight to the right screen point.
        let frame_bounds = window_scope
            .as_ref()
            .map(|(b, _)| *b)
            .unwrap_or(captured.bounds);
        let frame = ScreenFrame {
            frame_id: frame_id.clone(),
            session_id: session_id.clone(),
            captured_at: captured.captured_at,
            image_path: Some(captured.image_path.clone()),
            image_bytes: Some(captured.image_bytes.clone()),
            width: captured.width,
            height: captured.height,
            display_id: captured.display_id.clone(),
            bounds: frame_bounds,
            scale_factor: captured.scale_factor,
            active_application: snapshot.active_application.clone(),
            active_window_title: snapshot.active_window_title.clone(),
            perceptual_hash: Some(snapshot.perceptual_hash()),
        };

        {
            let mut store = self.frames.lock().unwrap();
            store.insert(frame.clone(), snapshot.clone());
        }
        *session.current_frame_id.lock().unwrap() = Some(frame_id.clone());
        *session.last_action_at.lock().unwrap() = Some(Utc::now());

        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_observe(
                    request_id.clone(),
                    &frame_id,
                    captured.width,
                    captured.height,
                    &display_id,
                    captured.image_bytes.len() as u64,
                )
                .await;
        }

        let include_image = params.include_image.unwrap_or(false);
        let image_base64 = if include_image {
            Some(base64::engine::general_purpose::STANDARD.encode(&captured.image_bytes))
        } else {
            None
        };

        Ok(ObserveResult {
            session_id,
            frame_id,
            width: captured.width,
            height: captured.height,
            display_id,
            scale_factor: captured.scale_factor,
            active_application: frame.active_application.clone(),
            active_window: frame.active_window_title.clone(),
            image_base64,
            image_path: output_path.to_string_lossy().into_owned(),
            image_mime_type: if format == "png" {
                "image/png".into()
            } else {
                "image/jpeg".into()
            },
            captured_at: frame.captured_at,
            // P0-6: every observe declares its coordinate space; a window-
            // scoped observe also reports the target window's global bounds
            // and id so the caller can map window coords to the screen.
            coordinate_space: Some("normalized_1000".into()),
            target_bounds: window_scope.as_ref().map(|(b, _)| *b),
            window_id: window_scope.as_ref().map(|(_, id)| *id),
        })
    }

    // ------------------------------------------------------------------
    // Act
    // ------------------------------------------------------------------

    pub async fn act(
        &self,
        params: ActParams,
        request_key: Option<RequestKey>,
    ) -> Result<ActResult, CuError> {
        let session = self.get_session(&params.session_id)?;
        // Capability check FIRST: without a valid control token nothing is
        // parsed, queued, or executed — no side effects of any kind.
        session.verify_control_token(params.control_token.as_deref())?;
        self.gate_active(&session)?;

        // The request's batch token registers before the busy lock, so a
        // queued request can be cancelled without touching the batch that is
        // executing, and the registry key (connection_id, request_id) makes
        // the cancellation precise. The scope guard unregisters and ends the
        // batch token on every exit path.
        let token = session.begin_batch();
        if let Some(key) = &request_key {
            self.requests
                .register(key.clone(), params.session_id.clone(), token.clone());
        }
        let _scope = BatchScope {
            registry: &self.requests,
            session: &session,
            key: request_key.clone(),
            token: token.clone(),
        };
        let request_id = request_key.as_ref().map(|k| k.request_id.to_string());

        // Serialize observe/act per session so two batches never interleave.
        let _busy = session.busy.lock().await;
        // Cancelled while queued? Never run a cancelled batch.
        if token.is_cancelled() {
            return Err(CuError::Cancelled);
        }
        // The control lock must be held by this session; a stopped/replaced
        // session must never drive the pointer.
        if !self.control_lock.is_held_by(&params.session_id) {
            return Err(self.control_locked_for());
        }

        let batch = cu_core::ActionBatch {
            actions: params.actions.clone(),
            wait_policy: params.wait_policy.unwrap_or(WaitPolicy::None),
            fixed_wait_ms: params.fixed_wait_ms,
            return_screenshot: params.return_screenshot.unwrap_or(false),
        };
        batch.validate()?;

        // Confirmation gate (declared by the caller; the runtime never judges
        // semantic danger, it only enforces the declared policy).
        let policy = ConfirmationPolicy::from_caller(
            params.requires_confirmation,
            params.risk_level.as_deref(),
            params.policy_context.as_deref(),
            &batch.actions,
        );
        if let Authorization::RequiresConfirmation(p) = authorize(&policy, false) {
            return Err(p.to_error());
        }

        // Frame lookup + geometry for coordinate resolution.
        let (geometry, referenced_snapshot) = {
            let store = self.frames.lock().unwrap();
            let sf = store
                .get(&params.frame_id)
                .ok_or_else(|| CuError::UnknownFrame(params.frame_id.clone()))?;
            let geom = ImageGeometry {
                image_width_px: sf.frame.width,
                image_height_px: sf.frame.height,
                display_bounds: sf.frame.bounds,
            };
            (geom, sf.snapshot.clone())
        };

        // Round 8 / P0-2: refresh the target (identity + bounds) BEFORE the
        // stale fingerprint, so the snapshot region comes from LIVE geometry.
        // A moved/resized window changes the crop → STALE_FRAME (the old code
        // compared against stale bounds); an identity change (the window closed
        // and another took its place, or the pid/bundle changed) →
        // TARGET_UNAVAILABLE. This replaces the old post-stale-check
        // `resolve_target_bounds`, which only re-checked the window_id and ran
        // after the stale comparison.
        //
        // Guarded on `get_resolved_target()`: every act follows an observe
        // that populates it for a target-scoped session, and a session scoped
        // after observe with no resolution keeps the pre-P0-2 act path (the
        // focus guard and bounds checks still protect it).
        if session.get_resolved_target().is_some() {
            self.refresh_resolved_target(&session).await?;
        }
        let target_region: Option<CaptureRegion> = if session.get_resolved_target().is_some() {
            match session.get_target_bounds() {
                Some(b) => self.target_crop_region(&session, &b).await?,
                // Off-screen / no bounds: fall back to a full-desktop
                // fingerprint (there is no window crop to sample).
                None => None,
            }
        } else {
            None
        };

        // Stale-frame check against the *live* desktop. Round 8 / P0-2: for a
        // target-scoped session this is the TARGET WINDOW CROP (region +
        // pinned app), never the full desktop — changes in other apps cannot
        // stale the referenced frame.
        let before_q = self
            .fingerprint_snapshot(&session, target_region)
            .await
            .map_err(|e| {
                CuError::StaleFrame(cu_core::StaleFrameDetail {
                    referenced_frame_id: params.frame_id.clone(),
                    current_frame_id: "unavailable".into(),
                    change_score: 1.0,
                    reason: format!("cannot verify current screen state: {e}"),
                })
            })?;
        let before_screen: ScreenSnapshot = before_q.clone().into();
        // The "current frame" the referenced frame is checked against is the
        // session's latest observed frame (used by the strict policy, which
        // rejects any non-current frame_id). The live visual comparison runs
        // regardless.
        let current_frame_id = session
            .current_frame_id
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| {
                cu_core::config::new_frame_id(self.frame_counter.fetch_add(1, Ordering::SeqCst))
            });
        let verdict = self.stale.check(
            &referenced_snapshot,
            &before_screen,
            &params.frame_id,
            &current_frame_id,
        );
        if verdict.is_stale {
            // Record the rejection so benchmark reports can count stale
            // frames from the trace (the batch never runs, so no action
            // event would otherwise exist).
            if let Some(t) = session.trace.as_ref() {
                let _ = t
                    .record_event(
                        "act.stale_rejected",
                        serde_json::json!({
                            "frame_id": params.frame_id,
                            "change_score": verdict.change_score,
                        }),
                    )
                    .await;
            }
            return Err(self.stale.to_error(&verdict));
        }

        // Bounds pre-check: every location-bearing action must be on-screen
        // before anything moves.
        if !cu_policy::batch_in_bounds(&batch.actions, &geometry) {
            for a in &batch.actions {
                cu_policy::resolve_action_points(a, &geometry)?;
            }
        }

        // Execute the batch with the request-scoped token.
        let mut takeover = TakeoverDetector {
            policy: self.config.takeover_policy,
            ..Default::default()
        };
        let queue = ActionQueue::new(self.driver.as_ref());
        // Round 9 / P0-6: the Focus Guard now re-reads the frontmost app LIVE
        // before every keyboard action inside the queue; no batch-level cache.
        let runs = queue
            .run(
                &session,
                &batch.actions,
                &geometry,
                token.clone(),
                &mut takeover,
                Some(self.human_input.as_ref()),
                session.trace.as_ref(),
                request_id.as_deref(),
                &params.frame_id,
                &session.display_id,
                before_screen.active_application.as_deref(),
                (),
                self.driver.human_input_monitor_state().as_deref(),
            )
            .await?;
        *session.last_action_at.lock().unwrap() = Some(Utc::now());

        // Wait policy. The batch token is passed to the stabilizer so a
        // pause/takeover/stop during the wait cancels it immediately.
        let mut stable = false;
        let mut stabilization: Option<StabilizationInfo> = None;
        match batch.wait_policy {
            WaitPolicy::None => {}
            WaitPolicy::Fixed => {
                // Cancellation-aware: a cancel/stop during the fixed wait stops
                // it at the next tick instead of sleeping it out.
                let ms = batch.fixed_wait_ms.unwrap_or(0);
                if ms > 0 {
                    let deadline = Instant::now() + Duration::from_millis(ms);
                    while Instant::now() < deadline {
                        // P0-1: a REAL human event during the between-batch wait
                        // forces UserTakeover (the hook cancelled the token, but
                        // the token-cancel alone does not transition state).
                        if self.consume_real_takeover(&session).await {
                            return Err(CuError::Cancelled);
                        }
                        if token.is_cancelled() {
                            return Err(CuError::Cancelled);
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        tokio::select! {
                            () = token.cancelled() => {
                                return Err(CuError::Cancelled);
                            }
                            () = tokio::time::sleep(remaining.min(Duration::from_millis(50))) => {}
                        }
                    }
                }
            }
            WaitPolicy::UntilStable => {
                let stabilizer = Stabilizer::new(self.driver.as_ref(), self.config.stabilizer);
                let outcome = stabilizer
                    .until_stable(&session.display_id, target_region, &before_q, &token)
                    .await;
                match outcome {
                    Ok(StabilizeOutcome::Stable {
                        change_score,
                        samples,
                    }) => {
                        stable = true;
                        stabilization = Some(StabilizationInfo {
                            outcome: "stable".into(),
                            change_score,
                            samples,
                            elapsed_ms: None,
                        });
                    }
                    Ok(StabilizeOutcome::TimedOut {
                        change_score,
                        samples,
                        elapsed_ms,
                    }) => {
                        stable = false;
                        stabilization = Some(StabilizationInfo {
                            outcome: "timed_out".into(),
                            change_score,
                            samples,
                            elapsed_ms: Some(elapsed_ms),
                        });
                    }
                    Err(e) => {
                        // P0-1: a real takeover likely aborted the wait (the
                        // hook cancelled the token). Complete the transition
                        // before surfacing the cancellation.
                        let _ = self.consume_real_takeover(&session).await;
                        return Err(e);
                    }
                }
            }
        }

        // Post-batch snapshot → screen_changed / stable. Round 8 / P0-2: the
        // same target-crop region as `before_q` (pinned app for target
        // sessions) so the change comparison is apples-to-apples.
        let after_q = self.fingerprint_snapshot(&session, target_region).await?;
        let after_screen: ScreenSnapshot = after_q.into();
        let change = before_screen.change_score(&after_screen).unwrap_or(1.0);
        let screen_changed = change > self.config.stale.threshold;
        if batch.wait_policy != WaitPolicy::UntilStable {
            stable = change <= self.config.stale.threshold;
        }

        let reports = to_action_result_reports(&runs);
        let executed = runs.iter().any(|r| r.status == "success");

        // Optional post-act re-observe so the agent gets the resulting state in
        // one round trip. Skipped if the session was paused/taken over mid-batch
        // (the batch already told the caller what ran).
        let (screenshot, next_frame_id) =
            if batch.return_screenshot && matches!(session.state(), SessionState::Active) {
                let obs = self
                    .observe_inner(
                        &session,
                        ObserveParams {
                            session_id: Some(params.session_id.clone()),
                            display_id: Some(session.display_id.clone()),
                            include_image: Some(true),
                            // Internal re-observe: the caller's token was
                            // already verified for the mutating batch.
                            control_token: params.control_token.clone(),
                            ..Default::default()
                        },
                        request_id,
                    )
                    .await?;
                (Some(obs.clone()), Some(obs.frame_id.clone()))
            } else {
                (None, None)
            };

        // Trace status for this batch: surfaces best-effort degradation so
        // callers know the trace may be incomplete (see TraceReport).
        let trace = if let Some(t) = session.trace.as_ref() {
            Some(TraceReport {
                mode: self.config.trace_mode.as_str().to_string(),
                degraded: t.is_degraded(),
                warnings: t.warnings().await,
            })
        } else {
            None
        };

        Ok(ActResult {
            executed,
            action_results: reports,
            screen_changed,
            stable,
            next_frame_id,
            screenshot,
            stabilization,
            trace,
        })
    }

    // ------------------------------------------------------------------
    // Inspect
    // ------------------------------------------------------------------

    /// P0-6: clamp an inspect region to the session's target window. The
    /// region is expressed in the frame's image space; the window is expressed
    /// in global logical points. Both are intersected in global space and the
    /// result re-expressed in the region's original coordinate space — a model
    /// that asks to inspect the desktop outside its target window never
    /// receives pixels outside it. A region that does not intersect the window
    /// at all is an OutOfBounds error.
    fn clamp_region_to_window(
        geometry: &ImageGeometry,
        region: &Region,
        window: cu_core::DisplayBounds,
    ) -> Result<Region, CuError> {
        let (gx0, gy0, gx1, gy1) = match region.coordinate_space {
            CoordinateSpace::Normalized1000 => {
                let a = geometry.normalized_1000_to_global(region.x, region.y)?;
                let b = geometry
                    .normalized_1000_to_global(region.x + region.width, region.y + region.height)?;
                (a.x, a.y, b.x, b.y)
            }
            CoordinateSpace::ImagePixels => {
                let a = geometry
                    .image_pixel_to_global(region.x.round() as u32, region.y.round() as u32);
                let b = geometry.image_pixel_to_global(
                    (region.x + region.width).round() as u32,
                    (region.y + region.height).round() as u32,
                );
                (a.x, a.y, b.x, b.y)
            }
        };
        let (wx1, wy1) = (window.x + window.width, window.y + window.height);
        let (ix0, iy0) = (gx0.max(window.x), gy0.max(window.y));
        let (ix1, iy1) = (gx1.min(wx1), gy1.min(wy1));
        if ix1 <= ix0 || iy1 <= iy0 {
            return Err(CuError::OutOfBounds(cu_core::errors::BoundsDetail {
                coordinate_space: region.coordinate_space.as_str().into(),
                x: region.x,
                y: region.y,
                image_width: geometry.image_width_px,
                image_height: geometry.image_height_px,
            }));
        }
        let (x, y, width, height) = match region.coordinate_space {
            CoordinateSpace::Normalized1000 => {
                let a = geometry
                    .global_to_image_pixel(cu_core::Point::new(ix0, iy0))
                    .ok_or_else(|| {
                        CuError::OutOfBounds(cu_core::errors::BoundsDetail {
                            coordinate_space: region.coordinate_space.as_str().into(),
                            x: region.x,
                            y: region.y,
                            image_width: geometry.image_width_px,
                            image_height: geometry.image_height_px,
                        })
                    })?;
                let b = geometry
                    .global_to_image_pixel(cu_core::Point::new(ix1, iy1))
                    .ok_or_else(|| {
                        CuError::OutOfBounds(cu_core::errors::BoundsDetail {
                            coordinate_space: region.coordinate_space.as_str().into(),
                            x: region.x,
                            y: region.y,
                            image_width: geometry.image_width_px,
                            image_height: geometry.image_height_px,
                        })
                    })?;
                let na = geometry.image_pixel_to_normalized_1000(a.0, a.1);
                let nb = geometry.image_pixel_to_normalized_1000(b.0, b.1);
                (na.x, na.y, nb.x - na.x, nb.y - na.y)
            }
            CoordinateSpace::ImagePixels => {
                let s = geometry.scale_factor().max(f64::EPSILON);
                let x = ((ix0 - geometry.display_bounds.x) * s).round();
                let y = ((iy0 - geometry.display_bounds.y) * s).round();
                let x2 = ((ix1 - geometry.display_bounds.x) * s).round();
                let y2 = ((iy1 - geometry.display_bounds.y) * s).round();
                (x, y, x2 - x, y2 - y)
            }
        };
        Ok(Region {
            x,
            y,
            width,
            height,
            coordinate_space: region.coordinate_space,
        })
    }

    pub async fn inspect(&self, params: InspectParams) -> Result<InspectResult, CuError> {
        let session = self.get_session(&params.session_id)?;
        // Inspecting a stored frame exposes desktop pixels: capability token
        // (observation or control) verified before any pixels are read.
        session.verify_read_tokens(
            params.observation_token.as_deref(),
            params.control_token.as_deref(),
        )?;
        let stored = {
            let store = self.frames.lock().unwrap();
            store.get(&params.frame_id).cloned()
        }
        .ok_or_else(|| CuError::UnknownFrame(params.frame_id.clone()))?;

        let geometry = ImageGeometry {
            image_width_px: stored.frame.width,
            image_height_px: stored.frame.height,
            display_bounds: stored.frame.bounds,
        };
        // P0-6: a window-scoped session may only inspect inside its target
        // window. Clamp the request region to the window before mapping it to
        // image pixels (a full-display frame stays clamped too).
        // P0-3: the target is refreshed first — inspect must use the LATEST
        // window bounds, and a window that no longer resolves (closed /
        // re-created / re-identified) fails closed to TARGET_UNAVAILABLE
        // instead of clamping against stale bounds.
        let region = if session.get_target().is_some() || session.get_resolved_target().is_some() {
            let rt = self.refresh_resolved_target(&session).await?;
            match rt.bounds {
                Some(wb) => Self::clamp_region_to_window(&geometry, &params.region, wb)?,
                None => params.region,
            }
        } else {
            params.region
        };
        let (px, py, w, h) = region.to_image_pixels(&geometry)?;

        let img = if let Some(bytes) = &stored.frame.image_bytes {
            image::load_from_memory(bytes)
                .map_err(|e| CuError::Driver(format!("cannot decode stored frame: {e}")))?
        } else if let Some(path) = &stored.frame.image_path {
            image::open(path)
                .map_err(|e| CuError::Driver(format!("cannot open stored frame: {e}")))?
        } else {
            return Err(CuError::UnknownFrame(params.frame_id.clone()));
        };

        let crop = img.crop_imm(px, py, w, h);
        let scale = params.scale.unwrap_or(1).max(1);
        let out = if scale > 1 {
            crop.resize(
                w.saturating_mul(scale),
                h.saturating_mul(scale),
                image::imageops::FilterType::Triangle,
            )
        } else {
            crop
        };

        let mut buf = Vec::new();
        out.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .map_err(|e| CuError::Driver(format!("cannot encode crop: {e}")))?;
        let image_base64 = base64::engine::general_purpose::STANDARD.encode(&buf);

        let global_origin = geometry.image_pixel_to_global(px, py);
        let normalized_origin = geometry.image_pixel_to_normalized_1000(px, py);
        Ok(InspectResult {
            session_id: params.session_id,
            frame_id: params.frame_id,
            width: w,
            height: h,
            image_base64,
            image_mime_type: "image/png".into(),
            mapping: InspectMapping {
                source_image_rect: Region {
                    x: px as f64,
                    y: py as f64,
                    width: w as f64,
                    height: h as f64,
                    coordinate_space: CoordinateSpace::ImagePixels,
                },
                global_origin: (global_origin.x, global_origin.y),
                normalized_1000_origin: (normalized_origin.x, normalized_origin.y),
            },
        })
    }

    // ------------------------------------------------------------------
    // Shutdown
    // ------------------------------------------------------------------

    /// Graceful daemon shutdown.
    ///
    /// Order matters for a clean drain:
    /// 1. The `shutting_down` flag is set **first** (synchronously), so any
    ///    request dispatched from this point on fails fast with
    ///    `DAEMON_SHUTTING_DOWN` instead of starting new work.
    /// 2. Every session is stopped; each stop transitions through `Stopping`,
    ///    which cancels the in-flight action batch, so long-running requests
    ///    return promptly (CANCELLED) rather than blocking the exit.
    /// 3. Driver resources (the Swift bridge) are released.
    ///
    /// The runtime's own shutdown path is the one caller that may stop a
    /// session without a control token.
    pub async fn shutdown(&self) -> Result<(), CuError> {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sessions: Vec<SharedSession> =
            self.sessions.lock().unwrap().values().cloned().collect();
        for session in sessions {
            let _ = self.stop_session(&session).await;
        }
        self.driver.shutdown().await
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn get_session(&self, id: &str) -> Result<SharedSession, CuError> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| CuError::SessionNotFound(format!("session not found: {id}")))
    }

    /// Verify a capability token for a session's **trace** (used by the
    /// daemon's trace methods, which read files off the session's trace dir).
    ///
    /// Round 6: trace access is strictly session-scoped. A live session
    /// verifies its own tokens as before. After a daemon restart the session
    /// is gone from memory, but its trace files remain — the trace access
    /// manifest persisted at `session_start` (token *hashes* only, never
    /// plaintext) proves the token holder again. A session with no live
    /// record and no manifest is `SESSION_NOT_FOUND`; a manifest that exists
    /// but matches nothing is the non-descriptive token error, so a wrong
    /// token never distinguishes "exists but denied" from "you don't hold
    /// this session's credential".
    pub fn verify_trace_access(
        &self,
        session_id: &str,
        observation_token: Option<&str>,
        control_token: Option<&str>,
    ) -> Result<(), CuError> {
        if let Ok(session) = self.get_session(session_id) {
            return session.verify_read_tokens(observation_token, control_token);
        }
        match cu_trace::manifest::check_access(
            &self.config.traces_dir,
            session_id,
            observation_token,
            control_token,
        ) {
            Some(true) => Ok(()),
            Some(false) => match (observation_token, control_token) {
                (None, None) => Err(CuError::ObservationTokenRequired),
                _ => Err(CuError::InvalidObservationToken),
            },
            None => Err(CuError::SessionNotFound(format!(
                "session not found: {session_id}"
            ))),
        }
    }

    /// Verify a capability token against **any** session known to the daemon.
    /// Used by the cross-session sensitive reads (`trace.list`,
    /// `trace.summaries`, `runtime.pointer`, `runtime.active_application`,
    /// `runtime.desktop_layout`) which have no `session_id`: a valid token
    /// proves the caller is a trusted client of this daemon. No token at all
    /// is `OBSERVATION_TOKEN_REQUIRED`; a token matching no session is the
    /// non-descriptive `INVALID_OBSERVATION_TOKEN`. Never touches a file or
    /// driver on the failure path.
    pub fn verify_any_token(
        &self,
        observation_token: Option<&str>,
        control_token: Option<&str>,
    ) -> Result<(), CuError> {
        let sessions: Vec<SharedSession> =
            self.sessions.lock().unwrap().values().cloned().collect();
        if sessions.iter().any(|s| {
            s.verify_read_tokens(observation_token, control_token)
                .is_ok()
        }) {
            return Ok(());
        }
        match (observation_token, control_token) {
            (None, None) => Err(CuError::ObservationTokenRequired),
            _ => Err(CuError::InvalidObservationToken),
        }
    }

    fn gate_active(&self, session: &Session) -> Result<(), CuError> {
        match session.state() {
            SessionState::Active => Ok(()),
            SessionState::Starting => Err(CuError::NotReady("session is still starting".into())),
            SessionState::Paused => Err(CuError::Paused),
            SessionState::UserTakeover => Err(CuError::UserTakeover),
            SessionState::Stopping | SessionState::Stopped => Err(CuError::SessionStopped),
            SessionState::Failed => Err(CuError::InvalidSessionState("session is failed".into())),
        }
    }

    /// Enrich a `ControlLocked` error with the holder's (non-secret) owner
    /// identity, so the rejected caller can identify who owns the session.
    fn with_holder_owner(&self, e: CuError) -> CuError {
        if let CuError::ControlLocked {
            holder,
            owner: None,
        } = e
        {
            let owner = self
                .sessions
                .lock()
                .unwrap()
                .get(&holder)
                .and_then(|s| s.owner.clone());
            CuError::ControlLocked { holder, owner }
        } else {
            e
        }
    }

    /// A `ControlLocked` error for the current control-lock holder. Only
    /// reachable when the caller already established the lock is not held by
    /// its own session; the error reports who actually holds it.
    fn control_locked_for(&self) -> CuError {
        let holder = self.control_lock.holder().unwrap_or_default();
        let owner = self
            .sessions
            .lock()
            .unwrap()
            .get(&holder)
            .and_then(|s| s.owner.clone());
        CuError::ControlLocked { holder, owner }
    }

    fn session_result(&self, session: &Session) -> SessionResult {
        SessionResult {
            session_id: session.id.clone(),
            state: session.state(),
            paused: session.is_paused(),
            user_takeover: session.is_user_takeover(),
            lock_held: self.control_lock.is_held_by(&session.id),
            display_id: session.display_id.clone(),
            created_at: session.created_at,
            last_action_at: *session.last_action_at.lock().unwrap(),
            current_frame_id: session.current_frame_id.lock().unwrap().clone(),
            trace_dir: session
                .trace
                .as_ref()
                .map(|t| t.path().to_string_lossy().into_owned()),
            started_by: session.started_by.clone(),
            // Neither capability token is EVER included in status results —
            // both are issued exactly once, in the start response.
            control_token: None,
            observation_token: None,
            owner_client_id: session.owner.as_ref().map(|c| c.client_id.clone()),
            owner_client_name: session.owner.as_ref().map(|c| c.client_name.clone()),
            owner_instance_id: session.owner.as_ref().map(|c| c.client_instance_id.clone()),
            message: None,
        }
    }
}

/// RAII guard for one `act` batch. Unregisters the request from the
/// cancellation registry and ends the batch token on **every** exit path —
/// success, error, cancellation — so no stale handle or token survives.
struct BatchScope<'a> {
    registry: &'a RequestRegistry,
    session: &'a SharedSession,
    key: Option<RequestKey>,
    token: tokio_util::sync::CancellationToken,
}

impl Drop for BatchScope<'_> {
    fn drop(&mut self) {
        self.session.end_batch(&self.token);
        if let Some(key) = &self.key {
            self.registry.unregister(key);
        }
    }
}

/// Pull the machine-readable error code out of a runtime error (used by the
/// daemon to build JSON-RPC errors).
pub fn error_code(e: &CuError) -> ErrorCode {
    e.code()
}

/// Start-time isolation options for a new session (round 8). All fields are
/// optional; the runtime applies sane defaults when absent.
#[derive(Debug, Clone, Default)]
pub struct SessionStartOptions {
    /// Optional app/window target the session is scoped to.
    pub target: Option<cu_core::SessionTarget>,
    /// Pointer isolation policy (default `isolated_preferred`).
    pub pointer_policy: Option<cu_core::PointerPolicy>,
    /// Keyboard focus policy (default `strict`).
    pub focus_policy: Option<cu_core::FocusPolicy>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::{ComputerAction, Point};
    use std::sync::Arc;

    /// Identity used by tests when starting sessions.
    fn test_client() -> ClientInfo {
        ClientInfo {
            client_id: "test".into(),
            client_name: "Test client".into(),
            client_instance_id: "test-1".into(),
        }
    }

    /// A deterministic in-memory driver that lets us exercise every gate
    /// without a real display. `Wait` actions actually sleep (so in-flight
    /// cancellation can be observed); every execute is counted.
    #[derive(Default)]
    struct FakeDriver {
        pub pointer: std::sync::Mutex<Point>,
        pub executes: std::sync::atomic::AtomicUsize,
        /// When set, the next execute() fails (failure-path trace tests).
        pub fail_next: std::sync::atomic::AtomicBool,
        /// Frontmost bundle id for the keyboard focus guard (None = unknown).
        pub active_bundle: std::sync::Mutex<Option<String>>,
        /// P0-5: frontmost pid / window id for the strict focus guard.
        pub active_pid: std::sync::Mutex<Option<i32>>,
        pub active_window: std::sync::Mutex<Option<u32>>,
        /// P0-4: configured `resolve_target` result (None = unresolved). Lets
        /// tests exercise fail-closed session start and identity backfill.
        pub resolve_result: std::sync::Mutex<Option<cu_driver::ResolvedSessionTarget>>,
        /// P0-6: full-display capture bounds (None = legacy 4x4 dark PNG).
        /// Tests needing realistic display geometry set this to the display's
        /// logical bounds so a full-display observe yields a consistent frame.
        pub capture_bounds: std::sync::Mutex<Option<cu_core::DisplayBounds>>,
        /// Round 8 / P0-2: pinned target-crop thumbnail returned by
        /// `quick_snapshot_region` (None = derive from the region coordinates,
        /// so a moved/resized target produces different crop pixels, like a
        /// real capture of a moved window).
        pub snapshot_region: std::sync::Mutex<Option<Vec<u8>>>,
        /// Round 8 / P0-2: pinned full-desktop thumbnail returned by
        /// `quick_snapshot` (None = all zeros, the pre-P0-2 default). Set to a
        /// different value to simulate "another app changed the desktop".
        pub snapshot_desktop: std::sync::Mutex<Option<Vec<u8>>>,
        /// Round 8 / P0-2: the region passed to the last `quick_snapshot_region`
        /// call — asserts the target-crop path (never the full desktop) is used.
        pub last_region: std::sync::Mutex<Option<CaptureRegion>>,
    }

    #[async_trait::async_trait]
    impl ComputerDriver for FakeDriver {
        async fn list_displays(&self) -> Result<Vec<DisplayInfo>, CuError> {
            Ok(vec![DisplayInfo {
                id: "1".into(),
                name: "fake".into(),
                bounds: cu_core::DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 1280.0,
                    height: 800.0,
                },
                pixel_width: 2560,
                pixel_height: 1600,
                scale_factor: 2.0,
                is_main: true,
            }])
        }
        async fn desktop_layout(&self) -> Result<DesktopLayout, CuError> {
            Ok(DesktopLayout {
                displays: self.list_displays().await?,
                primary_id: "1".into(),
            })
        }
        async fn capture(
            &self,
            request: CaptureRequest,
        ) -> Result<cu_driver::CapturedFrame, CuError> {
            // Dark PNG. A window crop (P0-6) uses the region's pixel size; a
            // full-display capture is 4x4 unless `capture_bounds` is set for a
            // realistic geometry (bounds * display scale 2.0).
            let cfg_bounds = *self.capture_bounds.lock().unwrap();
            let (mut w, mut h) = match request.region {
                Some(r) => (r.width.max(1.0) as u32, r.height.max(1.0) as u32),
                None => match cfg_bounds {
                    Some(b) => ((b.width * 2.0) as u32, (b.height * 2.0) as u32),
                    None => (4, 4),
                },
            };
            // Faithfully mirror the Swift bridge's max-width downscale
            // (P0-1/P0-2): a crop wider than max_width is scaled to
            // max_width wide, `floor(h * max_width / w)` tall — exactly what
            // `captureDisplay` in CUBridge/main.swift produces. Without this
            // the runtime's fail-closed crop check could not be exercised
            // against a real downscaled frame.
            if request.max_width > 0 && w > request.max_width {
                let scale = request.max_width as f64 / w as f64;
                w = request.max_width;
                h = (h as f64 * scale).trunc() as u32;
            }
            let bounds = cfg_bounds.unwrap_or(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 4.0,
                height: 4.0,
            });
            let img = image::RgbImage::from_pixel(w, h, image::Rgb([40, 40, 40]));
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            std::fs::write(&request.output_path, &buf).unwrap();
            Ok(cu_driver::CapturedFrame {
                display_id: request.display_id,
                width: w,
                height: h,
                scale_factor: 1.0,
                bounds,
                image_path: request.output_path,
                image_bytes: buf,
                format: request.format,
                active_application: None,
                captured_at: chrono::Utc::now(),
            })
        }
        async fn quick_snapshot(
            &self,
            display_id: &str,
        ) -> Result<cu_driver::QuickSnapshot, CuError> {
            Ok(cu_driver::QuickSnapshot {
                thumbnail: self
                    .snapshot_desktop
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| vec![0u8; 64]),
                thumb_width: 8,
                thumb_height: 8,
                display_id: display_id.to_string(),
                active_application: None,
                captured_at: chrono::Utc::now(),
            })
        }
        /// Round 8 / P0-2: target-crop snapshots. Records the region (so tests
        /// assert the crop path runs), and returns the pinned crop thumbnail —
        /// or, when unpinned, a thumbnail derived from the region coordinates
        /// so a moved/resized window yields different crop pixels.
        async fn quick_snapshot_region(
            &self,
            display_id: &str,
            region: Option<CaptureRegion>,
        ) -> Result<cu_driver::QuickSnapshot, CuError> {
            *self.last_region.lock().unwrap() = region;
            let thumbnail = match region {
                Some(r) => self
                    .snapshot_region
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| vec![(r.x as u8).wrapping_add(r.y as u8); 64]),
                None => vec![0u8; 64],
            };
            Ok(cu_driver::QuickSnapshot {
                thumbnail,
                thumb_width: 8,
                thumb_height: 8,
                display_id: display_id.to_string(),
                active_application: None,
                captured_at: chrono::Utc::now(),
            })
        }
        async fn execute(
            &self,
            action: &cu_driver::ResolvedAction,
        ) -> Result<cu_driver::ActionResult, CuError> {
            self.executes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(CuError::Driver("injected failure".into()));
            }
            if let cu_driver::ResolvedAction::Move { to, .. } = action {
                *self.pointer.lock().unwrap() = *to;
            }
            if let cu_driver::ResolvedAction::Wait { duration_ms } = action {
                // Actually wait, so a mid-batch takeover can be observed.
                tokio::time::sleep(Duration::from_millis((*duration_ms).min(1000))).await;
            }
            Ok(cu_driver::ActionResult {
                success: true,
                duration_ms: 1,
                detail: None,
            })
        }
        async fn permission_status(&self) -> Result<PermissionStatus, CuError> {
            Ok(PermissionStatus {
                screen_recording: true,
                accessibility: true,
            })
        }
        async fn active_application(&self) -> Result<Option<ApplicationInfo>, CuError> {
            Ok(self
                .active_bundle
                .lock()
                .unwrap()
                .clone()
                .map(|b| ApplicationInfo {
                    bundle_id: b,
                    name: "fake".into(),
                    window_title: None,
                    pid: *self.active_pid.lock().unwrap(),
                    window_id: *self.active_window.lock().unwrap(),
                }))
        }
        async fn resolve_target(
            &self,
            _target: &cu_core::SessionTarget,
        ) -> Result<Option<cu_driver::ResolvedSessionTarget>, CuError> {
            Ok(self.resolve_result.lock().unwrap().clone())
        }
        async fn resolve_target_bounds(
            &self,
            _window_id: u32,
        ) -> Result<Option<cu_core::DisplayBounds>, CuError> {
            // Mirrors the macOS driver: bounds come from the same resolution.
            Ok(self
                .resolve_result
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|r| r.bounds))
        }
        async fn pointer_location(&self) -> Result<PointerInfo, CuError> {
            Ok(PointerInfo {
                location: *self.pointer.lock().unwrap(),
                display_id: Some("1".into()),
            })
        }
        async fn shutdown(&self) -> Result<(), CuError> {
            Ok(())
        }
    }

    fn test_config() -> RuntimeConfig {
        // Stable per-process temp dir (kept alive for the whole test binary,
        // unlike tempfile::tempdir which drops the directory on return).
        let dir = std::env::temp_dir().join(format!("cu-runtime-tests-{}", std::process::id()));
        RuntimeConfig {
            traces_dir: dir.join("traces"),
            frames_dir: dir.join("frames"),
            ..RuntimeConfig::default()
        }
    }

    async fn runtime_with_config(cfg: RuntimeConfig) -> Arc<Runtime> {
        let driver = Arc::new(FakeDriver::default());
        Arc::new(Runtime::new(driver, cfg))
    }

    async fn runtime() -> Arc<Runtime> {
        runtime_with_driver().await.0
    }

    async fn runtime_with_driver() -> (Arc<Runtime>, Arc<FakeDriver>) {
        let driver = Arc::new(FakeDriver::default());
        let rt = Arc::new(Runtime::new(driver.clone(), test_config()));
        // Match production wiring so a `record_human_event` in a test cancels
        // the active batch exactly like the Event Tap would.
        rt.install_human_takeover_hook(rt.clone());
        (rt, driver)
    }

    #[tokio::test]
    async fn session_lifecycle_and_lock() {
        let rt = runtime().await;
        let started = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = started
            .control_token
            .clone()
            .expect("session start must issue the control token");
        assert_eq!(started.state, SessionState::Active);
        assert!(started.lock_held);

        // A second session must be rejected by the control lock.
        let err = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ControlLocked);

        // Pause gates act.
        let p = rt
            .session_pause(&started.session_id, Some(&token))
            .await
            .unwrap();
        assert!(p.paused);
        let act_params = ActParams {
            session_id: started.session_id.clone(),
            frame_id: "frame_1".into(),
            actions: vec![ComputerAction::Wait { duration_ms: 1 }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        };
        let err = rt.act(act_params, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Paused);

        rt.session_resume(&started.session_id, Some(&token))
            .await
            .unwrap();
        rt.session_stop(&started.session_id, Some(&token))
            .await
            .unwrap();
        let status = rt
            .session_status(
                &started.session_id,
                started.observation_token.as_deref(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(status.state, SessionState::Stopped);
        assert!(!status.lock_held);
    }

    /// The first-use contract: `session status` with no active session is a
    /// typed SESSION_NOT_FOUND (never INVALID_PARAMS), so adapters can
    /// auto-start without sniffing error strings.
    #[tokio::test]
    async fn status_without_session_returns_session_not_found() {
        let rt = runtime().await;
        let err = rt
            .session(
                SessionAction::Status,
                None,
                None,
                test_client(),
                None,
                None,
                SessionStartOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
        assert_eq!(err.to_string(), "No active computer-use session exists.");
        let data = err.to_error_data();
        assert_eq!(data["code"], "SESSION_NOT_FOUND");
    }

    /// The session records who started it, and the owner is reported back —
    /// ownership lets a client decide whether it may stop a session it found
    /// (never) versus one it created (yes).
    #[tokio::test]
    async fn session_records_owner_identity() {
        let rt = runtime().await;
        let started = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        assert_eq!(started.owner_client_id.as_deref(), Some("test"));
        assert_eq!(started.owner_client_name.as_deref(), Some("Test client"));
        assert_eq!(started.owner_instance_id.as_deref(), Some("test-1"));
        assert_eq!(started.started_by, "Test client");

        // A second client starting (or querying) sees the same owner — it
        // never becomes the owner by observing.
        let status = rt
            .session_status(
                &started.session_id,
                started.observation_token.as_deref(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(status.owner_client_id.as_deref(), Some("test"));
    }

    fn wait_params(
        session_id: &str,
        frame_id: &str,
        control_token: Option<cu_core::SecretToken>,
    ) -> ActParams {
        ActParams {
            session_id: session_id.into(),
            frame_id: frame_id.into(),
            actions: vec![ComputerAction::Wait { duration_ms: 1 }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token,
        }
    }

    /// The full takeover/resume/release matrix. A takeover must NOT be
    /// bypassable by a plain `resume`: resume only recovers `Paused`, release
    /// is the only exit from `UserTakeover`, and release outside takeover is
    /// itself rejected.
    #[tokio::test]
    async fn takeover_cannot_be_bypassed_by_resume() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let sid = s.session_id.clone();

        // 1. Pause → act rejected.
        rt.session_pause(&sid, Some(&token)).await.unwrap();
        let err = rt
            .act(wait_params(&sid, "frame_x", Some(token.clone())), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Paused);

        // 2. Pause → resume succeeds.
        rt.session_resume(&sid, Some(&token)).await.unwrap();
        let st = rt.session_status(&sid, None, Some(&token)).await.unwrap();
        assert_eq!(st.state, SessionState::Active);

        // 3. Takeover → act rejected.
        rt.session_takeover(&sid, Some(&token)).await.unwrap();
        let err = rt
            .act(wait_params(&sid, "frame_x", Some(token.clone())), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeover);

        // 4. Takeover → resume REJECTED with USER_TAKEOVER_ACTIVE; state holds.
        let err = rt.session_resume(&sid, Some(&token)).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeoverActive);
        assert!(
            err.to_string().contains("release"),
            "error must point at release: {err}"
        );
        let st = rt.session_status(&sid, None, Some(&token)).await.unwrap();
        assert_eq!(st.state, SessionState::UserTakeover);
        assert!(st.user_takeover);
        assert!(!st.paused);

        // 5. Takeover → release succeeds and returns to Active.
        let rel = rt.session_release(&sid, Some(&token)).await.unwrap();
        assert_eq!(rel.state, SessionState::Active);
        assert!(!rel.user_takeover);

        // 6. After release, acting works again (fresh frame, fresh act).
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res = rt
            .act(wait_params(&sid, &obs.frame_id, Some(token.clone())), None)
            .await
            .unwrap();
        assert!(res.executed);

        // 7. Release outside takeover is rejected (no silent no-op).
        let err = rt.session_release(&sid, Some(&token)).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidSessionState);

        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Takeover mid-batch: the in-flight act stops at the next safe boundary
    /// and the remaining actions are reported `cancelled` — none execute.
    #[tokio::test]
    async fn takeover_cancels_in_flight_actions() {
        let (rt, fake) = runtime_with_driver().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let sid = s.session_id.clone();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();

        let params = ActParams {
            session_id: sid.clone(),
            frame_id: obs.frame_id.clone(),
            actions: vec![
                // The first action completes before the takeover: it must be
                // reported as done. A Click reaches the physical driver; the
                // waits are what the takeover interrupts.
                ComputerAction::Click {
                    x: 400.0,
                    y: 400.0,
                    button: cu_core::MouseButton::Left,
                    coordinate_space: cu_core::CoordinateSpace::Normalized1000,
                },
                ComputerAction::Wait { duration_ms: 200 },
                ComputerAction::Wait { duration_ms: 200 },
            ],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        };

        let rt2 = rt.clone();
        let handle = tokio::spawn(async move { rt2.act(params, None).await });
        tokio::time::sleep(Duration::from_millis(40)).await;
        rt.session_takeover(&sid, Some(&token)).await.unwrap();

        let result = handle.await.unwrap().unwrap();
        assert_eq!(
            result.action_results[0].status, "success",
            "first action ran"
        );
        for report in &result.action_results[1..] {
            assert_eq!(
                report.status, "cancelled",
                "actions after takeover must be cancelled, got {report:?}"
            );
        }
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the first action may reach the driver"
        );

        // The session is still under takeover and still rejects actions.
        let err = rt
            .act(wait_params(&sid, &obs.frame_id, Some(token.clone())), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeover);
        rt.session_release(&sid, Some(&token)).await.unwrap();
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// P0-1: a REAL hardware event (the Event Tap -> HumanInputMonitor) must
    /// cancel the ACTIVE batch at event time — a long in-flight action aborts
    /// immediately, not on the next loop iteration. The batch token is
    /// cancelled by the registered hook; the queue consumes the flag and
    /// completes the UserTakeover transition.
    #[tokio::test]
    async fn real_human_event_cancels_active_batch_immediately() {
        let (rt, fake) = runtime_with_driver().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let sid = s.session_id.clone();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();

        let params = ActParams {
            session_id: sid.clone(),
            frame_id: obs.frame_id.clone(),
            actions: vec![
                ComputerAction::Click {
                    x: 400.0,
                    y: 400.0,
                    button: cu_core::MouseButton::Left,
                    coordinate_space: cu_core::CoordinateSpace::Normalized1000,
                },
                // A long wait the hardware event must interrupt mid-flight.
                ComputerAction::Wait { duration_ms: 800 },
            ],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        };

        let rt2 = rt.clone();
        let handle = tokio::spawn(async move { rt2.act(params, None).await });
        tokio::time::sleep(Duration::from_millis(40)).await;

        // The Event Tap fires a real human event. The installed hook cancels
        // the active batch immediately — the in-flight Wait must abort.
        rt.human_input.record_human_event(std::time::Instant::now());

        let result = handle.await.unwrap().unwrap();
        assert_eq!(
            result.action_results[0].status, "success",
            "the click before the event completed"
        );
        assert_eq!(
            result.action_results[1].status, "cancelled",
            "the in-flight wait must abort at event time, got {result:?}"
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the interrupted wait must not reach the driver"
        );
        // The session immediately reflects the human's takeover.
        let status = rt
            .session_status(&sid, None, Some(token.as_str()))
            .await
            .unwrap();
        assert_eq!(
            status.state,
            SessionState::UserTakeover,
            "a real hardware event forces UserTakeover at event time"
        );
        // And the session now rejects further actions.
        let err = rt
            .act(wait_params(&sid, &obs.frame_id, Some(token.clone())), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeover);
        rt.session_release(&sid, Some(&token)).await.unwrap();
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn act_rejects_unknown_frame_and_stale() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let params = |frame: &str| ActParams {
            session_id: s.session_id.clone(),
            frame_id: frame.into(),
            actions: vec![ComputerAction::Wait { duration_ms: 1 }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        };
        // Unknown frame.
        let err = rt.act(params("frame_missing"), None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::UnknownFrame);

        // Observe then act against that frame with a stale threshold that any
        // tiny difference trips (the fake snapshot stays identical, so set an
        // absurdly low threshold + age).
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        // Age the referenced snapshot so the backstop trips.
        {
            let mut store = rt.frames.lock().unwrap();
            let sf = store.get_mut(&obs.frame_id).unwrap();
            sf.snapshot.captured_at = chrono::Utc::now() - chrono::Duration::seconds(3600);
        }
        let err = rt.act(params(&obs.frame_id), None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::StaleFrame);
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    /// The trace must carry the data benchmark reports are built from: the
    /// stale rejection event (which otherwise has no trace record at all),
    /// the cancel event, and the observe screenshot byte count.
    #[tokio::test]
    async fn trace_records_stale_rejection_cancel_and_screenshot_bytes() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    include_image: Some(true),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();

        // Age the referenced snapshot so the backstop trips.
        {
            let mut store = rt.frames.lock().unwrap();
            let sf = store.get_mut(&obs.frame_id).unwrap();
            sf.snapshot.captured_at = chrono::Utc::now() - chrono::Duration::seconds(3600);
        }
        let err = rt
            .act(
                wait_params(&s.session_id, &obs.frame_id, Some(token.clone())),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::StaleFrame);

        // Cancel (no in-flight batch is fine — the event must still record).
        rt.cancel_in_flight(&s.session_id, Some(&token))
            .await
            .unwrap();

        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
        let trace_path = rt.config.traces_dir.join(format!("{}.jsonl", s.session_id));
        let content = std::fs::read_to_string(&trace_path).unwrap();
        assert!(
            content.contains("\"event\":\"act.stale_rejected\""),
            "stale rejection must be recorded in the trace:\n{}",
            content
        );
        assert!(
            content.contains("\"event\":\"cancel\""),
            "cancel must be recorded in the trace:\n{}",
            content
        );
        assert!(
            content.contains("\"screenshot_bytes\":"),
            "observe must record the screenshot byte count:\n{}",
            content
        );
    }

    /// A failed action's detail string must ride into the trace (the
    /// benchmark failure taxonomy is derived from these details).
    #[tokio::test]
    async fn action_failure_detail_is_recorded_in_trace() {
        let (rt, driver) = runtime_with_driver().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        driver
            .fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let res = rt
            .act(
                ActParams {
                    session_id: s.session_id.clone(),
                    frame_id: obs.frame_id.clone(),
                    actions: vec![ComputerAction::Click {
                        x: 100.0,
                        y: 100.0,
                        button: cu_core::MouseButton::Left,
                        coordinate_space: CoordinateSpace::Normalized1000,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await;
        assert!(
            res.is_ok(),
            "per-action failures surface as batch reports, not errors"
        );
        let report = res.unwrap();
        assert_eq!(report.action_results[0].status, "failed");

        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
        let trace_path = rt.config.traces_dir.join(format!("{}.jsonl", s.session_id));
        let content = std::fs::read_to_string(&trace_path).unwrap();
        assert!(
            content.contains("\"status\":\"failed\"") && content.contains("injected failure"),
            "action trace entry must carry status + failure detail:\n{}",
            content
        );
    }

    #[tokio::test]
    async fn act_strict_policy_rejects_older_frames() {
        // Default policy is Strict: only the session's current frame is
        // actionable, even when the older frame's pixels still match.
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs1 = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let obs2 = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_ne!(obs1.frame_id, obs2.frame_id);
        let err = rt
            .act(
                wait_params(&s.session_id, &obs1.frame_id, Some(token.clone())),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::StaleFrame);
        // The current frame still runs (fake screen is unchanged).
        rt.act(
            wait_params(&s.session_id, &obs2.frame_id, Some(token.clone())),
            None,
        )
        .await
        .unwrap();
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn act_visual_match_accepts_older_identical_frames() {
        // VisualMatch policy: an older frame whose content still matches the
        // live screen is actionable (the pre-strict runtime behavior).
        let mut cfg = test_config();
        cfg.stale.policy = crate::stale_frame::StaleFramePolicy::VisualMatch;
        let rt = runtime_with_config(cfg).await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs1 = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let obs2 = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        // The older frame still matches the live screen → allowed.
        rt.act(
            wait_params(&s.session_id, &obs1.frame_id, Some(token.clone())),
            None,
        )
        .await
        .unwrap();
        rt.act(
            wait_params(&s.session_id, &obs2.frame_id, Some(token.clone())),
            None,
        )
        .await
        .unwrap();
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn required_trace_mode_fails_session_start_without_trace() {
        // traces_dir points at a file, so create_dir_all fails → session
        // start must fail under Required (best_effort would degrade instead).
        let file = std::env::temp_dir().join(format!("cu-required-trace-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let mut cfg = test_config();
        cfg.trace_mode = cu_trace::TraceMode::Required;
        cfg.traces_dir = file.clone();
        let rt = runtime_with_config(cfg).await;
        let err = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::TraceError);
        std::fs::remove_file(&file).unwrap();
    }

    #[tokio::test]
    async fn act_reports_trace_mode_and_degradation() {
        // Best-effort (default): act carries a trace report, mode best_effort.
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res = rt
            .act(
                wait_params(&s.session_id, &obs.frame_id, Some(token.clone())),
                None,
            )
            .await
            .unwrap();
        let trace = res.trace.expect("best_effort act must report trace status");
        assert_eq!(trace.mode, "best_effort");
        assert!(!trace.degraded);
        assert!(trace.warnings.is_empty());
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();

        // Disabled: no recorder exists → no trace report.
        let mut cfg = test_config();
        cfg.trace_mode = cu_trace::TraceMode::Disabled;
        let rt2 = runtime_with_config(cfg).await;
        let s2 = rt2
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token2 = s2
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs2 = rt2
            .observe(
                ObserveParams {
                    session_id: Some(s2.session_id.clone()),
                    control_token: Some(token2.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res2 = rt2
            .act(
                wait_params(&s2.session_id, &obs2.frame_id, Some(token2.clone())),
                None,
            )
            .await
            .unwrap();
        assert!(res2.trace.is_none(), "disabled mode has no trace report");
        rt2.session_stop(&s2.session_id, Some(&token2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn act_out_of_bounds_rejected() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let params = ActParams {
            session_id: s.session_id.clone(),
            frame_id: obs.frame_id.clone(),
            actions: vec![ComputerAction::Move {
                x: 2000.0,
                y: 2000.0,
                coordinate_space: CoordinateSpace::Normalized1000,
                duration_ms: None,
            }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        };
        let err = rt.act(params, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::OutOfBounds);
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn confirmation_required_is_enforced() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let params = ActParams {
            session_id: s.session_id.clone(),
            frame_id: obs.frame_id.clone(),
            actions: vec![ComputerAction::Click {
                x: 100.0,
                y: 100.0,
                button: cu_core::MouseButton::Left,
                coordinate_space: CoordinateSpace::Normalized1000,
            }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: Some("high".into()),
            requires_confirmation: None,
            policy_context: Some("deleting files".into()),
            control_token: Some(token.clone()),
        };
        let err = rt.act(params, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfirmationRequired);
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn inspect_crops_and_maps() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res = rt
            .inspect(InspectParams {
                session_id: s.session_id.clone(),
                frame_id: obs.frame_id.clone(),
                region: Region {
                    x: 0.0,
                    y: 0.0,
                    width: 2.0,
                    height: 2.0,
                    coordinate_space: CoordinateSpace::ImagePixels,
                },
                scale: None,
                observation_token: None,
                control_token: Some(token.clone()),
            })
            .await
            .unwrap();
        assert_eq!(res.width, 2);
        assert_eq!(res.height, 2);
        assert!(!res.image_base64.is_empty());
        assert_eq!(res.mapping.source_image_rect.x, 0.0);
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_stops_a_long_wait_fast_with_per_action_reports() {
        // The full cancel chain (SDK abort → computer.cancel → batch token):
        // a 10s wait action inside a batch must stop within ~1s of the cancel,
        // and the report marks the interrupted wait (and everything after it)
        // `cancelled` — not `failed`, and not an internal error.
        let (rt, _driver) = runtime_with_driver().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let sid = s.session_id.clone();
        let frame = obs.frame_id.clone();
        let act_token = token.clone();
        let rt2 = rt.clone();
        let handle = tokio::spawn(async move {
            rt2.act(
                ActParams {
                    session_id: sid,
                    frame_id: frame,
                    actions: vec![
                        ComputerAction::Move {
                            x: 100.0,
                            y: 100.0,
                            coordinate_space: CoordinateSpace::Normalized1000,
                            duration_ms: None,
                        },
                        ComputerAction::Wait {
                            duration_ms: 10_000,
                        },
                        ComputerAction::Key {
                            keys: vec!["return".into()],
                        },
                    ],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(act_token),
                },
                None,
            )
            .await
        });
        // Let the first action run and the wait begin, then cancel.
        tokio::time::sleep(Duration::from_millis(120)).await;
        rt.cancel_in_flight(&s.session_id, Some(&token))
            .await
            .unwrap();
        let started = Instant::now();
        let res = handle.await.unwrap().unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel must stop the 10s wait fast, took {elapsed:?}"
        );
        assert!(res.executed);
        assert_eq!(res.action_results.len(), 3);
        assert_eq!(res.action_results[0].status, "success");
        assert_eq!(res.action_results[1].status, "cancelled");
        assert_eq!(res.action_results[2].status, "cancelled");
        assert!(
            res.action_results[1].error.is_none(),
            "cancelled is not an error"
        );
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_stops_a_fixed_wait_fast_as_an_explicit_cancellation() {
        // wait_policy=fixed with a long duration must also stop quickly on
        // cancel, surfacing CANCELLED (not ACTION_TIMEOUT / internal error).
        let (rt, _driver) = runtime_with_driver().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let sid = s.session_id.clone();
        let frame = obs.frame_id.clone();
        let act_token = token.clone();
        let rt2 = rt.clone();
        let handle = tokio::spawn(async move {
            rt2.act(
                ActParams {
                    session_id: sid,
                    frame_id: frame,
                    actions: vec![ComputerAction::Move {
                        x: 50.0,
                        y: 50.0,
                        coordinate_space: CoordinateSpace::Normalized1000,
                        duration_ms: None,
                    }],
                    wait_policy: Some(WaitPolicy::Fixed),
                    fixed_wait_ms: Some(60_000),
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(act_token),
                },
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        rt.cancel_in_flight(&s.session_id, Some(&token))
            .await
            .unwrap();
        let started = Instant::now();
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel must stop the 60s fixed wait fast, took {:?}",
            started.elapsed()
        );
        assert_eq!(err.code(), ErrorCode::Cancelled);
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn stop_during_until_stable_returns_immediately() {
        // The stabilizer's own cancellation: stopping the session mid
        // until_stable must abort the wait (session.stop cancels the batch
        // token). The act call errors with CANCELLED rather than hanging.
        let (rt, _driver) = runtime_with_driver().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let sid = s.session_id.clone();
        let frame = obs.frame_id.clone();
        let act_token = token.clone();
        let rt2 = rt.clone();
        let handle = tokio::spawn(async move {
            rt2.act(
                ActParams {
                    session_id: sid,
                    frame_id: frame,
                    actions: vec![ComputerAction::Move {
                        x: 80.0,
                        y: 80.0,
                        coordinate_space: CoordinateSpace::Normalized1000,
                        duration_ms: None,
                    }],
                    wait_policy: Some(WaitPolicy::UntilStable),
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(act_token),
                },
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Stopping the session cancels its in-flight batch token.
        rt.session_stop(&s.session_id, Some(&token)).await.unwrap();
        let started = Instant::now();
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop must abort until_stable fast, took {:?}",
            started.elapsed()
        );
        assert_eq!(err.code(), ErrorCode::Cancelled);
    }

    // ------------------------------------------------------------------
    // Server-side ownership (§十七): the control token is the capability.
    // Knowing the session id grants nothing; every mutating operation is
    // refused without a valid token, and a refusal never has side effects.
    // ------------------------------------------------------------------

    /// Helper: start a session and observe a fresh frame for act calls.
    async fn start_observed(rt: &Arc<Runtime>) -> (String, cu_core::SecretToken, String) {
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        (s.session_id, token, obs.frame_id)
    }

    /// Round 8 / P0-2 helper: start a target-scoped session (window 7 of
    /// `com.example.Target`, logical bounds (100,50,300,200)) and observe a
    /// WINDOW-CROP frame. `fake.resolve_result` is wired so the runtime's
    /// pre-observe refresh resolves the very window the session is bound to.
    async fn start_target_observed(
        rt: &Arc<Runtime>,
        fake: &Arc<FakeDriver>,
    ) -> (String, cu_core::SecretToken, String) {
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let token = s
            .control_token
            .clone()
            .expect("session start must issue the control token");
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&s.session_id).unwrap();
            session.set_target(Some(cu_core::SessionTarget {
                bundle_id: Some("com.example.Target".into()),
                pid: Some(42),
                window_id: Some(7),
            }));
        }
        *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 42,
            window_id: 7,
            bounds: Some(cu_core::DisplayBounds {
                x: 100.0,
                y: 50.0,
                width: 300.0,
                height: 200.0,
            }),
        });
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        (s.session_id, token, obs.frame_id)
    }

    /// Round 8 / P0-2 helper: a harmless single-action act against a frame
    /// (Wait — no coordinates, no focus guard), so only the stale check /
    /// target refresh gates decide the outcome.
    fn target_wait_params(sid: &str, frame: &str, token: &cu_core::SecretToken) -> ActParams {
        ActParams {
            session_id: sid.into(),
            frame_id: frame.into(),
            actions: vec![ComputerAction::Wait { duration_ms: 1 }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        }
    }

    #[tokio::test]
    async fn owner_with_token_can_operate() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        rt.session_pause(&sid, Some(&token)).await.unwrap();
        rt.session_resume(&sid, Some(&token)).await.unwrap();
        // Round 8: a Move is virtual-only — it updates the session's virtual
        // pointer and drives the ghost cursor, never the physical driver.
        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame,
                    actions: vec![ComputerAction::Move {
                        x: 500.0,
                        y: 500.0,
                        coordinate_space: CoordinateSpace::Normalized1000,
                        duration_ms: None,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert!(res.executed);
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a Move must never reach the physical driver (virtual-only)"
        );
        // The virtual pointer actually moved. normalized_1000 (500,500) on
        // the fake 4x4 image → pixel (2,2) → global (2.0, 2.0), distinct from
        // the seeded (0,0).
        let vp = rt
            .sessions
            .lock()
            .unwrap()
            .get(&sid)
            .unwrap()
            .virtual_pointer
            .lock()
            .unwrap()
            .location();
        assert!(vp.x > 0.0 && vp.y > 0.0, "virtual pointer must move");
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn missing_token_is_refused_without_side_effects() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        for op in [
            ("pause", SessionAction::Pause),
            ("resume", SessionAction::Resume),
            ("takeover", SessionAction::Takeover),
            ("stop", SessionAction::Stop),
        ] {
            let err = rt
                .session(
                    op.1,
                    Some(&sid),
                    None,
                    test_client(),
                    None,
                    None,
                    SessionStartOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                err.code(),
                ErrorCode::ControlTokenRequired,
                "{} without a token must be CONTROL_TOKEN_REQUIRED",
                op.0
            );
        }
        let err = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame.clone(),
                    actions: vec![ComputerAction::Wait { duration_ms: 1 }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: None,
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ControlTokenRequired);
        assert!(rt.cancel_in_flight(&sid, None).await.is_err());

        // No side effects: session still Active, nothing reached the driver,
        // nothing was cancelled.
        let st = rt.session_status(&sid, None, Some(&token)).await.unwrap();
        assert_eq!(st.state, SessionState::Active);
        assert_eq!(fake.executes.load(std::sync::atomic::Ordering::SeqCst), 0);

        // The owner still holds a fully working session afterwards.
        rt.act(
            ActParams {
                session_id: sid.clone(),
                frame_id: frame,
                actions: vec![ComputerAction::Wait { duration_ms: 1 }],
                wait_policy: None,
                fixed_wait_ms: None,
                return_screenshot: None,
                risk_level: None,
                requires_confirmation: None,
                policy_context: None,
                control_token: Some(token.clone()),
            },
            None,
        )
        .await
        .unwrap();
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_is_refused_and_says_nothing_useful() {
        let rt = runtime().await;
        let (sid, token, _frame) = start_observed(&rt).await;

        // A wrong token fails with INVALID_CONTROL_TOKEN and the message must
        // not hint at why (length, hash, or session mismatch all look alike).
        for wrong in ["wrong-token".to_string(), "a".repeat(43), "b".repeat(43)] {
            let err = rt.session_pause(&sid, Some(&wrong)).await.unwrap_err();
            assert_eq!(err.code(), ErrorCode::InvalidControlToken);
            assert!(
                !err.to_string().contains(&wrong),
                "error must not echo the presented token"
            );
        }
        let err = rt
            .session_stop(&sid, Some("wrong-token"))
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidControlToken);
        assert_eq!(
            err.code().as_str(),
            "INVALID_CONTROL_TOKEN",
            "error string must not distinguish length/hash/ownership"
        );

        // Still Active: the refusals had no side effects.
        let st = rt.session_status(&sid, None, Some(&token)).await.unwrap();
        assert_eq!(st.state, SessionState::Active);
        assert_eq!(
            st.control_token, None,
            "status must never return the control token"
        );
    }

    #[tokio::test]
    async fn other_clients_cannot_stop_takeover_or_cancel() {
        let rt = runtime().await;
        let (sid, token, _frame) = start_observed(&rt).await;

        // A different client (no token — it never received one) trying to
        // stop, takeover, or cancel must be refused.
        let other = ClientInfo {
            client_id: "other-client".into(),
            client_name: "Other".into(),
            client_instance_id: "other-1".into(),
        };
        for action in [
            SessionAction::Stop,
            SessionAction::Takeover,
            SessionAction::Pause,
        ] {
            let err = rt
                .session(
                    action,
                    Some(&sid),
                    None,
                    other.clone(),
                    None,
                    None,
                    SessionStartOptions::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code(), ErrorCode::ControlTokenRequired);
        }
        assert!(rt.cancel_in_flight(&sid, None).await.is_err());

        // The owner's session is untouched and still controllable.
        let st = rt.session_status(&sid, None, Some(&token)).await.unwrap();
        assert_eq!(st.state, SessionState::Active);
        assert_eq!(st.owner_client_id.as_deref(), Some("test"));
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn owner_can_cancel_precisely() {
        let rt = runtime().await;
        let (sid, token, frame) = start_observed(&rt).await;

        // Two requests with the same request_id on different connections:
        // cancelling one must not touch the other (§十六 at runtime level).
        let rt2 = rt.clone();
        let token2 = token.clone();
        let frame2 = frame.clone();
        let sid2 = sid.clone();
        let a = tokio::spawn(async move {
            rt2.act(
                ActParams {
                    session_id: sid2,
                    frame_id: frame2,
                    actions: vec![ComputerAction::Wait {
                        duration_ms: 10_000,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token2),
                },
                Some(RequestKey {
                    connection_id: 1,
                    request_id: serde_json::json!(7),
                }),
            )
            .await
        });
        let rt3 = rt.clone();
        let token3 = token.clone();
        let frame3 = frame.clone();
        let sid3 = sid.clone();
        let b = tokio::spawn(async move {
            rt3.act(
                ActParams {
                    session_id: sid3,
                    frame_id: frame3,
                    actions: vec![ComputerAction::Wait { duration_ms: 500 }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token3),
                },
                Some(RequestKey {
                    connection_id: 2,
                    request_id: serde_json::json!(7),
                }),
            )
            .await
        });
        // Both batches begin (each registers; the fake driver waits).
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(rt.request_count(), 2, "both requests must be registered");

        // Cancel exactly connection 1's request 7.
        assert!(rt
            .cancel_request(
                &RequestKey {
                    connection_id: 1,
                    request_id: serde_json::json!(7),
                },
                &sid,
                Some(&token),
            )
            .await
            .unwrap());
        let started = Instant::now();
        let a_result = a.await.unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "A must stop quickly after its cancel"
        );
        // The interrupted wait reports `cancelled` (queue path) — never
        // success, and never an internal error.
        assert_eq!(a_result.action_results[0].status, "cancelled");
        assert!(!a_result.executed);

        // B is untouched and completes on its own (waits run in the queue,
        // so success is visible through the report, not the driver).
        let b_result = b.await.unwrap().unwrap();
        assert!(b_result.executed);
        assert_eq!(b_result.action_results[0].status, "success");
        assert_eq!(rt.request_count(), 0, "both batches must unregister");

        // Cancelling a key that never existed reports Ok(false) — and still
        // requires the token.
        assert!(rt
            .cancel_request(
                &RequestKey {
                    connection_id: 9,
                    request_id: serde_json::json!(1)
                },
                &sid,
                None
            )
            .await
            .is_err());
        assert!(!rt
            .cancel_request(
                &RequestKey {
                    connection_id: 9,
                    request_id: serde_json::json!(1)
                },
                &sid,
                Some(&token),
            )
            .await
            .unwrap());
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    #[tokio::test]
    async fn token_is_issued_exactly_once() {
        let rt = runtime().await;
        let s = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let issued = s.control_token.clone().expect("start must issue the token");
        assert_eq!(issued.len(), 43, "256-bit base64url token");

        // Every other response form must never contain the token: status,
        // pause, resume, stop results are all tokenless.
        let st = rt
            .session_status(&s.session_id, None, Some(&issued))
            .await
            .unwrap();
        assert_eq!(st.control_token, None);
        let p = rt
            .session_pause(&s.session_id, Some(&issued))
            .await
            .unwrap();
        assert_eq!(p.control_token, None);
        let r = rt
            .session_resume(&s.session_id, Some(&issued))
            .await
            .unwrap();
        assert_eq!(r.control_token, None);
        let stop = rt.session_stop(&s.session_id, Some(&issued)).await.unwrap();
        assert_eq!(stop.control_token, None);
    }

    #[tokio::test]
    async fn token_is_invalid_after_stop() {
        let rt = runtime().await;
        let (sid, token, frame) = start_observed(&rt).await;
        rt.session_stop(&sid, Some(&token)).await.unwrap();

        // With the (previously valid) token: mutating ops on a stopped
        // session are SESSION_STOPPED — the token no longer grants control.
        let err = rt.session_pause(&sid, Some(&token)).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionStopped);
        let err = rt.session_resume(&sid, Some(&token)).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionStopped);
        let err = rt.session_takeover(&sid, Some(&token)).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionStopped);
        let err = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame,
                    actions: vec![ComputerAction::Wait { duration_ms: 1 }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionStopped);
        assert!(rt.cancel_in_flight(&sid, Some(&token)).await.is_err());

        // Stop stays idempotent with the token.
        let again = rt.session_stop(&sid, Some(&token)).await.unwrap();
        assert_eq!(again.state, SessionState::Stopped);
    }

    #[tokio::test]
    async fn token_is_invalid_after_daemon_restart() {
        // A fresh runtime has no sessions: the old session id is gone, so
        // nothing (with or without the old token) can address it.
        let rt = runtime().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        let rt2 = runtime().await;

        let err = rt2.session_status(&sid, None, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
        let err = rt2
            .session(
                SessionAction::Stop,
                Some(&sid),
                None,
                test_client(),
                Some(&token),
                None,
                SessionStartOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
        let err = rt2
            .session(
                SessionAction::Pause,
                Some(&sid),
                None,
                test_client(),
                Some(&token),
                None,
                SessionStartOptions::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::SessionNotFound);
    }

    #[tokio::test]
    async fn takeover_and_release_verify_the_token() {
        let rt = runtime().await;
        let (sid, token, _frame) = start_observed(&rt).await;

        let err = rt.session_takeover(&sid, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::ControlTokenRequired);
        assert_eq!(
            rt.session_status(&sid, None, Some(&token))
                .await
                .unwrap()
                .state,
            SessionState::Active
        );

        rt.session_takeover(&sid, Some(&token)).await.unwrap();
        let err = rt.session_release(&sid, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::ControlTokenRequired);
        assert_eq!(
            rt.session_status(&sid, None, Some(&token))
                .await
                .unwrap()
                .state,
            SessionState::UserTakeover
        );
        rt.session_release(&sid, Some(&token)).await.unwrap();
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// A second session start fails with CONTROL_LOCKED carrying the holder's
    /// non-secret owner identity — and never a token.
    #[tokio::test]
    async fn control_locked_carries_owner_but_never_a_token() {
        let rt = runtime().await;
        let (sid, token, _frame) = start_observed(&rt).await;

        let err = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::ControlLocked);
        let data = err.to_error_data();
        assert_eq!(data["holder"], sid, "rejected client learns the holder id");
        assert_eq!(data["owner"]["client_id"], "test");
        assert_eq!(data["owner"]["client_name"], "Test client");
        assert_eq!(
            data["code"], "CONTROL_LOCKED",
            "the error is CONTROL_LOCKED, not a token oracle"
        );
        assert!(
            !serde_json::to_string(&data)
                .unwrap()
                .contains(token.as_str()),
            "the error must never contain the control token"
        );
    }

    /// Round 8 / Phase 16 — Keyboard Focus Guard: when the session has a
    /// bundle-id target and the frontmost app is something else, a Type action
    /// is rejected with INPUT_FOCUS_MISMATCH and NO keyboard event reaches the
    /// driver. The runtime never steals focus.
    #[tokio::test]
    async fn type_rejected_when_focus_is_not_on_target() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        // Scope the session to Chrome; the fake frontmost app is TextEdit.
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_target(Some(cu_core::SessionTarget {
                bundle_id: Some("com.google.Chrome".into()),
                pid: None,
                window_id: None,
            }));
        }
        *fake.active_bundle.lock().unwrap() = Some("com.apple.TextEdit".into());

        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame.clone(),
                    actions: vec![ComputerAction::TypeText {
                        text: "hi".into(),
                        method: cu_core::TextInputMethod::Keyboard,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.action_results[0].status, "failed");
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("INPUT_FOCUS_MISMATCH")
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no keyboard event may reach the driver on focus mismatch"
        );

        // With focus correctly on Chrome, Type succeeds.
        *fake.active_bundle.lock().unwrap() = Some("com.google.Chrome".into());
        let ok = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame.clone(),
                    actions: vec![ComputerAction::TypeText {
                        text: "hi".into(),
                        method: cu_core::TextInputMethod::Keyboard,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(ok.action_results[0].status, "success");
        assert_eq!(fake.executes.load(std::sync::atomic::Ordering::SeqCst), 1);
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 9 / P0-5 — Keyboard Focus Guard is STRICT: it compares bundle AND
    /// pid AND window id. A bundle match on a recycled pid (app relaunched) or
    /// a different window of the same app is NOT focus — INPUT_FOCUS_MISMATCH
    /// and no keyboard event reaches the driver.
    #[tokio::test]
    async fn strict_focus_compares_pid_and_window() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        // Target: Chrome, pid 111, window 500 (all three identity dimensions).
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_target(Some(cu_core::SessionTarget {
                bundle_id: Some("com.google.Chrome".into()),
                pid: Some(111),
                window_id: Some(500),
            }));
        }
        let act_type = |frame: String| ActParams {
            session_id: sid.clone(),
            frame_id: frame,
            actions: vec![ComputerAction::TypeText {
                text: "hi".into(),
                method: cu_core::TextInputMethod::Keyboard,
            }],
            wait_policy: None,
            fixed_wait_ms: None,
            return_screenshot: None,
            risk_level: None,
            requires_confirmation: None,
            policy_context: None,
            control_token: Some(token.clone()),
        };

        // Same bundle, recycled pid (app relaunched) -> NOT focus.
        *fake.active_bundle.lock().unwrap() = Some("com.google.Chrome".into());
        *fake.active_pid.lock().unwrap() = Some(222);
        *fake.active_window.lock().unwrap() = Some(500);
        let res = rt.act(act_type(frame.clone()), None).await.unwrap();
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("INPUT_FOCUS_MISMATCH"),
            "a recycled pid under the same bundle must not pass as focus"
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no keyboard event may reach the driver on pid mismatch"
        );

        // Same bundle + pid, but a DIFFERENT window of the same app -> NOT focus.
        *fake.active_window.lock().unwrap() = Some(999);
        let res = rt.act(act_type(frame.clone()), None).await.unwrap();
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("INPUT_FOCUS_MISMATCH"),
            "a different window of the same app must not pass as focus"
        );
        assert_eq!(fake.executes.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Exact bundle + pid + window match -> Type succeeds.
        *fake.active_pid.lock().unwrap() = Some(111);
        *fake.active_window.lock().unwrap() = Some(500);
        let res = rt.act(act_type(frame.clone()), None).await.unwrap();
        assert_eq!(res.action_results[0].status, "success");
        assert_eq!(fake.executes.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A pid-only target (no bundle) is still guarded: pid 333 vs live 111.
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_target(Some(cu_core::SessionTarget {
                bundle_id: None,
                pid: Some(333),
                window_id: None,
            }));
        }
        let res = rt.act(act_type(frame.clone()), None).await.unwrap();
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("INPUT_FOCUS_MISMATCH"),
            "a pid-only target must still be guarded"
        );
        assert_eq!(fake.executes.load(std::sync::atomic::Ordering::SeqCst), 1);

        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 9 / P0-6 — a session scoped to a target window observes a CROP of
    /// that window (never the full display), and the result declares the
    /// coordinate space + the window's global bounds + window id so the model
    /// can map window coords to the screen.
    #[tokio::test]
    async fn observe_is_window_scoped_when_target_has_bounds() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        // Window at origin, 2x2 logical points → 4x4 px at the fake's 2x scale.
        // P0-3: observe REFRESHES the target via the driver before cropping,
        // so the fake's resolution must return the same window.
        *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            }),
        });
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_resolved_target(Some(cu_driver::ResolvedSessionTarget {
                bundle_id: "com.example.Target".into(),
                pid: 4242,
                window_id: 777,
                bounds: Some(cu_core::DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 2.0,
                    height: 2.0,
                }),
            }));
        }

        let res = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();

        assert_eq!(res.coordinate_space.as_deref(), Some("normalized_1000"));
        assert_eq!(
            res.target_bounds,
            Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            }),
            "a window-scoped observe must report the window's global bounds"
        );
        assert_eq!(res.window_id, Some(777));
        assert_eq!(
            res.width, 4,
            "the image is the window crop, not the display"
        );
        assert_eq!(res.height, 4);

        // The stored frame's geometry is window-relative so `act` maps model
        // coords in the cropped image straight to the window's global bounds.
        // The guard is block-scoped so it is released before the await below.
        {
            let store = rt.frames.lock().unwrap();
            let sf = store.get(&res.frame_id).expect("frame stored");
            assert_eq!(sf.frame.bounds.width, 2.0);
            assert_eq!(sf.frame.bounds.height, 2.0);
            assert_eq!(sf.frame.width, 4);
        }

        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// P0-1/P0-2 — a target window WIDER than the observe max_width is
    /// legitimately downscaled by the bridge (crop → finalImage → max-width
    /// downscale), and the runtime's fail-closed crop check must ACCEPT the
    /// downscaled frame. This pins the "crop + downscale uses finalImage"
    /// contract: output 1440x800 is the downscaled CROP, never a 2560px
    /// full-desktop downscale.
    #[tokio::test]
    async fn observe_accepts_downscaled_wide_window_crop() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        // Window 900x500 logical @ 2x = 1800x1000 px crop on the 1280x800
        // logical display. Wider than max_width 1440 → downscale to
        // 1440 wide, floor(1000 * 1440/1800) = 800 tall.
        let resolved = cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 900.0,
                height: 500.0,
            }),
        };
        *fake.resolve_result.lock().unwrap() = Some(resolved.clone());
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_resolved_target(Some(resolved));
        }

        let res = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    max_width: Some(1440),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            res.width, 1440,
            "output must be the downscaled CROP (1440 wide), never the display"
        );
        assert_eq!(res.height, 800);
        assert_eq!(res.window_id, Some(777));
        {
            let store = rt.frames.lock().unwrap();
            let sf = store.get(&res.frame_id).expect("frame stored");
            assert_eq!(sf.frame.width, 1440);
            assert_eq!(sf.frame.height, 800);
        }
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// P0-3 — observe re-resolves the target before EVERY capture: after the
    /// window MOVES, the next observe uses the NEW bounds (never the stale
    /// session-start bounds), and the crop follows the new position.
    #[tokio::test]
    async fn observe_uses_refreshed_bounds_after_window_moves() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        let set_bounds =
            |fake: &Arc<FakeDriver>, rt: &Arc<Runtime>, sid: &str, b: cu_core::DisplayBounds| {
                *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
                    bundle_id: "com.example.Target".into(),
                    pid: 4242,
                    window_id: 777,
                    bounds: Some(b),
                });
                let sessions = rt.sessions.lock().unwrap();
                let session = sessions.get(sid).unwrap();
                session.set_resolved_target(Some(cu_driver::ResolvedSessionTarget {
                    bundle_id: "com.example.Target".into(),
                    pid: 4242,
                    window_id: 777,
                    bounds: Some(b),
                }));
            };

        // Window at origin: 2x2 logical → 4x4 px crop.
        set_bounds(
            &fake,
            &rt,
            &sid,
            cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            },
        );
        let obs1 = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(obs1.target_bounds.unwrap().x, 0.0);
        assert_eq!(obs1.width, 4);

        // Window MOVES to (5, 5); resolution reflects it.
        set_bounds(
            &fake,
            &rt,
            &sid,
            cu_core::DisplayBounds {
                x: 5.0,
                y: 5.0,
                width: 2.0,
                height: 2.0,
            },
        );
        let obs2 = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let tb = obs2.target_bounds.unwrap();
        assert_eq!(
            (tb.x, tb.y),
            (5.0, 5.0),
            "a moved window must be observed at its NEW position"
        );
        assert_eq!(obs2.width, 4, "the crop still matches the 2x2 window");
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / P0-2 — section 十四/十五: for a target-scoped session the
    /// stale fingerprint comes from the TARGET WINDOW CROP (never the full
    /// desktop), pinned to the target's OWN bundle. Changes in OTHER apps —
    /// the full-desktop thumbnail AND the live frontmost app — must NOT stale
    /// the referenced frame, and the act goes through. This is the regression
    /// test for "other-app visual changes must not trigger stale" (十二) and
    /// "active-app change must not auto-stale a target frame" (十三).
    #[tokio::test]
    async fn target_scoped_other_app_changes_never_stale_the_frame() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_target_observed(&rt, &fake).await;

        // The observe fingerprint was the CROP (region x=200,y=100 for the
        // window at logical (100,50) on the fake 2x display), never the full
        // desktop. The derived crop thumbnail is [(200+100) as u8; 64] = [44].
        assert!(
            fake.last_region.lock().unwrap().is_some(),
            "a target-scoped observe must snapshot the window crop, not the desktop"
        );

        // "Another app changed the desktop": the full-desktop thumbnail flips
        // to all-white AND the live frontmost app becomes a different app.
        *fake.snapshot_desktop.lock().unwrap() = Some(vec![255u8; 64]);
        *fake.active_bundle.lock().unwrap() = Some("com.other.App".into());
        // Prove the change is real: comparing the crop thumbnail to the
        // desktop thumbnail scores well above the 0.12 stale threshold — a
        // full-desktop fingerprint WOULD have flagged this.
        let crop = cu_core::ScreenSnapshot {
            thumbnail: vec![44u8; 64],
            thumb_width: 8,
            thumb_height: 8,
            active_application: None,
            active_window_title: None,
            display_id: "1".into(),
            captured_at: chrono::Utc::now(),
        };
        let desktop = cu_core::ScreenSnapshot {
            thumbnail: vec![255u8; 64],
            thumb_width: 8,
            thumb_height: 8,
            active_application: None,
            active_window_title: None,
            display_id: "1".into(),
            captured_at: chrono::Utc::now(),
        };
        assert!(
            crop.change_score(&desktop).unwrap() > 0.12,
            "the desktop change would trip a full-desktop stale check"
        );

        let res = rt
            .act(target_wait_params(&sid, &frame, &token), None)
            .await
            .unwrap();
        assert_eq!(
            res.action_results[0].status, "success",
            "other-app desktop changes must not stale a target frame"
        );
        assert!(
            fake.last_region.lock().unwrap().is_some(),
            "the act fingerprint must sample the window crop again — the full \
             desktop was never sampled, which is why the change is invisible"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / P0-2 — section 十五: a change INSIDE the target window (the
    /// crop thumbnail differs from the referenced frame) IS stale. The target
    /// frame only ever reflects the window's own content.
    #[tokio::test]
    async fn target_scoped_target_visual_change_is_stale() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_target_observed(&rt, &fake).await;

        // The referenced crop is [44;64]; the target's content now changed.
        *fake.snapshot_region.lock().unwrap() = Some(vec![255u8; 64]);

        let err = rt
            .act(target_wait_params(&sid, &frame, &token), None)
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::StaleFrame,
            "a target-content change must stale the target frame"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / P0-2 — section 二十: when the target window closes (the
    /// resolver returns None), an act on a target-scoped session fails closed
    /// with TARGET_UNAVAILABLE — never stale bounds, never a re-scope.
    #[tokio::test]
    async fn target_closed_is_target_unavailable_on_act() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_target_observed(&rt, &fake).await;

        *fake.resolve_result.lock().unwrap() = None;

        let err = rt
            .act(target_wait_params(&sid, &frame, &token), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::TargetUnavailable);
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / P0-2 — section 二十: when the target window MOVES (its bounds
    /// change), the act refreshes the LIVE geometry and the crop thumbnail now
    /// differs from the referenced frame → STALE_FRAME. Old geometry is never
    /// used: the frame is rejected, never silently acted on.
    #[tokio::test]
    async fn target_moved_fires_stale_frame_on_act() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_target_observed(&rt, &fake).await;

        // Move the window: (100,50) → (140,50). The refreshed crop region
        // becomes (280,100), deriving a different thumbnail than the
        // referenced (200,100) crop — i.e. the moved window no longer matches
        // what the frame captured.
        *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 42,
            window_id: 7,
            bounds: Some(cu_core::DisplayBounds {
                x: 140.0,
                y: 50.0,
                width: 300.0,
                height: 200.0,
            }),
        });

        let err = rt
            .act(target_wait_params(&sid, &frame, &token), None)
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::StaleFrame,
            "a moved target must stale the frame, never act on old geometry"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / P0-2 — section 十四: a NON-target session keeps the
    /// full-desktop fingerprint (no region), so its stale checks still see the
    /// whole desktop. Only target sessions switch to the window crop.
    #[tokio::test]
    async fn non_target_session_keeps_full_desktop_fingerprint() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        // Observe and act on a non-target session: no crop region ever reaches
        // `quick_snapshot_region`.
        rt.act(target_wait_params(&sid, &frame, &token), None)
            .await
            .unwrap();
        assert!(
            fake.last_region.lock().unwrap().is_none(),
            "a non-target session must keep the full-desktop fingerprint"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// P0-3 — when the target window is gone (resolver returns None), observe
    /// FAILS CLOSED with TARGET_UNAVAILABLE. It never re-captures with stale
    /// bounds and never silently re-scopes to another window.
    #[tokio::test]
    async fn observe_fails_closed_when_target_window_closed() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        let resolved = cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            }),
        };
        *fake.resolve_result.lock().unwrap() = Some(resolved.clone());
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_resolved_target(Some(resolved));
        }
        // Window exists: observe succeeds.
        rt.observe(
            ObserveParams {
                session_id: Some(sid.clone()),
                include_image: Some(false),
                control_token: Some(token.clone()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

        // Window closes: the resolver now returns None.
        *fake.resolve_result.lock().unwrap() = None;
        let err = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::TargetUnavailable,
            "a closed target window must fail closed, not capture stale bounds"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// P0-3 — the refresh must NEVER re-scope: if the original window A is
    /// gone and the resolver would hand back a sibling window B (same app),
    /// observe fails closed instead of silently switching targets.
    #[tokio::test]
    async fn observe_fails_closed_when_refresh_returns_sibling_window() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        let resolved = cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            }),
        };
        *fake.resolve_result.lock().unwrap() = Some(resolved.clone());
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_resolved_target(Some(resolved));
        }
        rt.observe(
            ObserveParams {
                session_id: Some(sid.clone()),
                include_image: Some(false),
                control_token: Some(token.clone()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();

        // The resolver now returns a DIFFERENT window (888) of the same app.
        *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 888,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            }),
        });
        let err = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::TargetUnavailable,
            "a sibling window must never silently replace the bound target"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// P0-3 — inspect also refreshes the target before clamping: a window that
    /// has since closed fails closed to TARGET_UNAVAILABLE instead of
    /// inspecting pixels against stale bounds.
    #[tokio::test]
    async fn inspect_fails_closed_when_target_window_gone() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        let resolved = cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 2.0,
                height: 2.0,
            }),
        };
        *fake.resolve_result.lock().unwrap() = Some(resolved.clone());
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_resolved_target(Some(resolved));
        }
        // Capture a window-scoped frame first.
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let frame_id = obs.frame_id;

        // The window closes before the inspect.
        *fake.resolve_result.lock().unwrap() = None;
        let err = rt
            .inspect(InspectParams {
                session_id: sid.clone(),
                frame_id,
                region: Region {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 1000.0,
                    coordinate_space: CoordinateSpace::Normalized1000,
                },
                scale: None,
                observation_token: None,
                control_token: Some(token.clone()),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::TargetUnavailable);
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 9 / P0-6 — inspect clamps its region to the session's target
    /// window: a request covering the whole display only returns the window's
    /// pixels, and a region entirely outside the window is OutOfBounds.
    #[tokio::test]
    async fn inspect_clamps_region_to_target_window() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, _frame) = start_observed(&rt).await;
        // Realistic full-display geometry: 1280x800 logical @ 2x = 2560x1600.
        *fake.capture_bounds.lock().unwrap() = Some(cu_core::DisplayBounds {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 800.0,
        });
        // Observe the full display FIRST (no target yet). A huge max_width
        // keeps the frame at the fake's native 2560x1600 (no bridge-style
        // downscale) so the clamp math below stays clean.
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    control_token: Some(token.clone()),
                    max_width: Some(10000),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert!(obs.target_bounds.is_none(), "no window scope yet");
        let frame_id = obs.frame_id;

        // Now scope the session to a window inside that display. P0-3:
        // inspect REFRESHES the target before clamping, so the fake's
        // resolution must return the same window.
        *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 100.0,
                y: 100.0,
                width: 400.0,
                height: 300.0,
            }),
        });
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_resolved_target(Some(cu_driver::ResolvedSessionTarget {
                bundle_id: "com.example.Target".into(),
                pid: 4242,
                window_id: 777,
                bounds: Some(cu_core::DisplayBounds {
                    x: 100.0,
                    y: 100.0,
                    width: 400.0,
                    height: 300.0,
                }),
            }));
        }

        // A region covering the whole display is clamped to the window.
        let res = rt
            .inspect(InspectParams {
                session_id: sid.clone(),
                frame_id: frame_id.clone(),
                region: Region {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 1000.0,
                    coordinate_space: CoordinateSpace::Normalized1000,
                },
                scale: None,
                observation_token: None,
                control_token: Some(token.clone()),
            })
            .await
            .unwrap();
        // Window 400x300 logical @ 2x = 800x600 px.
        assert_eq!(
            res.width, 800,
            "inspect must be clamped to the window width"
        );
        assert_eq!(
            res.height, 600,
            "inspect must be clamped to the window height"
        );
        assert!(
            (res.mapping.global_origin.0 - 100.0).abs() < 0.01
                && (res.mapping.global_origin.1 - 100.0).abs() < 0.01,
            "crop top-left is the window's global top-left"
        );

        // A region entirely outside the window is OutOfBounds, not silently
        // inspected.
        let err = rt
            .inspect(InspectParams {
                session_id: sid.clone(),
                frame_id,
                region: Region {
                    x: 0.0,
                    y: 0.0,
                    width: 50.0,
                    height: 50.0,
                    coordinate_space: CoordinateSpace::Normalized1000,
                },
                scale: None,
                observation_token: None,
                control_token: Some(token.clone()),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::OutOfBounds);

        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / Phase 9 — PointerPolicy `isolated_only`: physical-required
    /// actions (Drag, located Scroll) are refused before any driver call, and
    /// the rest of the batch is marked cancelled. The real cursor is never
    /// moved.
    #[tokio::test]
    async fn isolated_only_rejects_physical_drag_and_scroll() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_pointer_policy(cu_core::PointerPolicy::IsolatedOnly);
        }

        // Drag under isolated_only → ISOLATED_DRAG_UNAVAILABLE, no driver call.
        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame.clone(),
                    actions: vec![ComputerAction::Drag {
                        from: cu_core::Point::new(100.0, 100.0),
                        to: cu_core::Point::new(200.0, 200.0),
                        coordinate_space: CoordinateSpace::Normalized1000,
                        duration_ms: None,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.action_results[0].status, "failed");
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("ISOLATED_DRAG_UNAVAILABLE")
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "isolated_only must never move the physical cursor"
        );

        // Located Scroll under isolated_only → PHYSICAL_FALLBACK_NOT_ALLOWED.
        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame,
                    actions: vec![ComputerAction::Scroll {
                        x: Some(100.0),
                        y: Some(100.0),
                        delta_x: 0.0,
                        delta_y: -30.0,
                        coordinate_space: CoordinateSpace::Normalized1000,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.action_results[0].status, "failed");
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("PHYSICAL_FALLBACK_NOT_ALLOWED")
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "isolated_only must never move the physical cursor for scroll"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 8 / Phase 9 / P0-3 — PointerPolicy `isolated_preferred` (the
    /// session default) must NOT silently execute a physical-only Drag /
    /// located Scroll. The runtime has no isolated Drag / located-Scroll
    /// backend (the macOS driver always borrows the real cursor), so these are
    /// refused with PHYSICAL_FALLBACK_REQUIRED and the real cursor is never
    /// moved. Only `physical_allowed` permits them.
    #[tokio::test]
    async fn isolated_preferred_refuses_physical_drag_and_scroll() {
        let (rt, fake) = runtime_with_driver().await;
        let (sid, token, frame) = start_observed(&rt).await;

        // isolated_preferred is the session default; assert it explicitly.
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            assert_eq!(
                session.get_pointer_policy(),
                cu_core::PointerPolicy::IsolatedPreferred
            );
        }

        // Drag under isolated_preferred → PHYSICAL_FALLBACK_REQUIRED.
        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame.clone(),
                    actions: vec![ComputerAction::Drag {
                        from: cu_core::Point::new(100.0, 100.0),
                        to: cu_core::Point::new(200.0, 200.0),
                        coordinate_space: CoordinateSpace::Normalized1000,
                        duration_ms: None,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.action_results[0].status, "failed");
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("PHYSICAL_FALLBACK_REQUIRED")
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "isolated_preferred must never silently move the physical cursor"
        );

        // Located Scroll under isolated_preferred → PHYSICAL_FALLBACK_REQUIRED.
        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame.clone(),
                    actions: vec![ComputerAction::Scroll {
                        x: Some(100.0),
                        y: Some(100.0),
                        delta_x: 0.0,
                        delta_y: -30.0,
                        coordinate_space: CoordinateSpace::Normalized1000,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.action_results[0].status, "failed");
        assert_eq!(
            res.action_results[0].error.as_deref(),
            Some("PHYSICAL_FALLBACK_REQUIRED")
        );
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "isolated_preferred must never silently move the physical cursor for scroll"
        );

        // The same Drag under `physical_allowed` DOES execute (the interruptible
        // physical path is the whole point of that policy).
        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).unwrap();
            session.set_pointer_policy(cu_core::PointerPolicy::PhysicalAllowed);
        }
        let res = rt
            .act(
                ActParams {
                    session_id: sid.clone(),
                    frame_id: frame,
                    actions: vec![ComputerAction::Drag {
                        from: cu_core::Point::new(100.0, 100.0),
                        to: cu_core::Point::new(200.0, 200.0),
                        coordinate_space: CoordinateSpace::Normalized1000,
                        duration_ms: None,
                    }],
                    wait_policy: None,
                    fixed_wait_ms: None,
                    return_screenshot: None,
                    risk_level: None,
                    requires_confirmation: None,
                    policy_context: None,
                    control_token: Some(token.clone()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.action_results[0].status, "success");
        assert_eq!(
            fake.executes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "physical_allowed must permit the physical drag"
        );
        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 9 / P0-4 — Target Resolve FAIL-CLOSED: a caller that explicitly
    /// scopes a session to an app/window must get a concrete window or no
    /// session at all. When the driver cannot resolve the target (window gone
    /// / identity mismatch), session start errors with TARGET_UNAVAILABLE and
    /// leaves NO orphaned session behind — the agent never runs unbound.
    #[tokio::test]
    async fn session_start_fails_closed_when_target_unresolved() {
        let (rt, fake) = runtime_with_driver().await;
        // Default: resolve_result = None (window not found / unresolved).
        assert!(fake.resolve_result.lock().unwrap().is_none());

        let err = rt
            .session_start(
                None,
                test_client(),
                Some(cu_core::SessionTarget {
                    bundle_id: Some("com.example.Gone".into()),
                    pid: Some(99999),
                    window_id: None,
                }),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::TargetUnavailable);
        // No session may exist after the failed start, and the control lock
        // must be free — a ghost session must not block the next real one.
        assert!(
            rt.sessions.lock().unwrap().is_empty(),
            "failed start must not leave a session"
        );
        assert!(
            rt.control_lock.holder().is_none(),
            "failed start must not hold the control lock"
        );
    }

    /// Round 9 / P0-4 — Target Resolve FULL IDENTITY: when the caller provides
    /// partial identity (pid only), the runtime backfills the session target
    /// with the resolved window's complete identity (bundle_id, pid,
    /// window_id) so the Focus Guard and observe window-scoping never depend
    /// on the caller having supplied every field.
    #[tokio::test]
    async fn session_start_backfills_full_target_identity() {
        let (rt, fake) = runtime_with_driver().await;
        *fake.resolve_result.lock().unwrap() = Some(cu_driver::ResolvedSessionTarget {
            bundle_id: "com.example.Target".into(),
            pid: 4242,
            window_id: 777,
            bounds: Some(cu_core::DisplayBounds {
                x: 10.0,
                y: 20.0,
                width: 800.0,
                height: 600.0,
            }),
        });

        let started = rt
            .session_start(
                None,
                test_client(),
                Some(cu_core::SessionTarget {
                    bundle_id: None,
                    pid: Some(4242),
                    window_id: None,
                }),
                None,
                None,
            )
            .await
            .unwrap();

        let sid = started.session_id.clone();
        let token = started
            .control_token
            .expect("start must issue the control token")
            .as_str()
            .to_string();

        {
            let sessions = rt.sessions.lock().unwrap();
            let session = sessions.get(&sid).expect("session must exist");
            let t = session.get_target().expect("target must be stored");
            assert_eq!(
                t.bundle_id.as_deref(),
                Some("com.example.Target"),
                "bundle_id must be backfilled from the resolved window"
            );
            assert_eq!(t.pid, Some(4242));
            assert_eq!(t.window_id, Some(777), "window_id must be backfilled");
            let rt2 = session
                .get_resolved_target()
                .expect("resolved target stored");
            assert_eq!(rt2.bundle_id, "com.example.Target");
            assert_eq!(rt2.pid, 4242);
            assert_eq!(rt2.window_id, 777);
            assert_eq!(
                session.get_target_bounds(),
                rt2.bounds,
                "resolved bounds must mirror into target_bounds"
            );
        }

        rt.session_stop(&sid, Some(&token)).await.unwrap();
    }

    /// Round 6: trace access survives a daemon restart through the persisted
    /// access manifest — the token holder can still read the session's trace,
    /// while a stranger's token and a never-existed session stay denied.
    #[tokio::test]
    async fn trace_access_survives_restart_via_manifest() {
        let mut cfg = test_config();
        cfg.traces_dir = cfg
            .traces_dir
            .join(format!("restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cfg.traces_dir);

        // First daemon: start a session and stop it (the trace file stays).
        let rt = runtime_with_config(cfg.clone()).await;
        let started = rt
            .session_start(None, test_client(), None, None, None)
            .await
            .unwrap();
        let sid = started.session_id.clone();
        let control = started.control_token.clone().unwrap();
        let observation = started.observation_token.clone().unwrap();
        rt.session_stop(&sid, Some(control.as_str())).await.unwrap();

        // The manifest exists and the session's own tokens still verify
        // through the live daemon (stopped sessions stay in memory).
        assert!(rt
            .verify_trace_access(&sid, None, Some(control.as_str()))
            .is_ok());
        assert!(rt
            .verify_trace_access(&sid, Some(observation.as_str()), None)
            .is_ok());

        // "Restart": a brand-new runtime over the same traces dir — the
        // session is gone from memory.
        let rt2 = runtime_with_config(cfg).await;
        assert_eq!(
            rt2.verify_trace_access(&sid, None, None)
                .unwrap_err()
                .code(),
            ErrorCode::ObservationTokenRequired,
            "no token, no access after restart"
        );
        assert!(
            rt2.verify_trace_access(&sid, None, Some(control.as_str()))
                .is_ok(),
            "the control token holder keeps trace access after restart"
        );
        assert!(
            rt2.verify_trace_access(&sid, Some(observation.as_str()), None)
                .is_ok(),
            "the observation token holder keeps trace access after restart"
        );
        let stranger = generate_control_token();
        assert_eq!(
            rt2.verify_trace_access(&sid, Some(stranger.as_str()), None)
                .unwrap_err()
                .code(),
            ErrorCode::InvalidObservationToken,
            "a stranger's token is denied after restart"
        );
        assert_eq!(
            rt2.verify_trace_access("s_never", None, Some(control.as_str()))
                .unwrap_err()
                .code(),
            ErrorCode::SessionNotFound,
            "a session that never existed is SESSION_NOT_FOUND"
        );
        let _ = std::fs::remove_dir_all(&rt2.config.traces_dir);
    }
}
