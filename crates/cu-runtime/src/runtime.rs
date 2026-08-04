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
    ActParams, ActResult, ClientInfo, CoordinateSpace, CuError, ErrorCode, ImageGeometry,
    InspectMapping, InspectParams, InspectResult, ObserveParams, ObserveResult, Region,
    ScreenFrame, ScreenSnapshot, SessionAction, SessionResult, SessionState, StabilizationInfo,
    TraceReport, WaitPolicy,
};
use cu_driver::{
    ApplicationInfo, CaptureRequest, ComputerDriver, DesktopLayout, DisplayInfo, PermissionStatus,
    PointerInfo,
};
use cu_policy::confirmation::Authorization;
use cu_policy::{authorize, ConfirmationPolicy, TakeoverDetector, TakeoverPolicy};
use cu_trace::{TraceConfig, TraceMode, TraceRecorder};

use crate::action_queue::{to_action_result_reports, ActionQueue};
use crate::frames::FrameStore;
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
        }
    }

    /// Path where session traces are written (used by the daemon's trace RPCs).
    pub fn traces_dir(&self) -> &std::path::Path {
        &self.config.traces_dir
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
        Ok(serde_json::json!({
            "version": cu_core::config::RUNTIME_VERSION,
            "ready": permissions.all_granted(),
            "permissions": permissions,
            "active_sessions": self.active_session_count(),
            "uptime_secs": self.uptime_secs(),
            "frame_cache": self.frames.lock().unwrap().len(),
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
    pub fn cancel_in_flight(&self, session_id: &str) -> Result<(), CuError> {
        let session = self.get_session(session_id)?;
        session.cancel_in_flight();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Session lifecycle
    // ------------------------------------------------------------------

    pub async fn session_start(
        &self,
        display_id: Option<String>,
        client: ClientInfo,
    ) -> Result<SessionResult, CuError> {
        let id = format!("s_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let display_id = match display_id {
            Some(d) => d,
            None => self.driver.desktop_layout().await?.primary_id,
        };

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
            trace,
        ));

        // Acquire the global control lock. Held for the session's lifetime.
        if let Err(e) = self.control_lock.try_acquire(&id) {
            if let Some(t) = session.trace.as_ref() {
                let _ = t.close().await;
            }
            return Err(e);
        }

        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());

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
        Ok(self.session_result(&session))
    }

    pub async fn session_status(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        Ok(self.session_result(&session))
    }

    pub async fn session_pause(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        session.transition(SessionState::Paused)?;
        if let Some(t) = session.trace.as_ref() {
            let _ = t.record_event("session.pause", serde_json::json!({})).await;
        }
        Ok(self.session_result(&session))
    }

    pub async fn session_resume(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
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
    pub async fn session_takeover(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        session.transition(SessionState::UserTakeover)?;
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
    pub async fn session_release(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        if session.state() != SessionState::UserTakeover {
            return Err(CuError::InvalidSessionState(format!(
                "cannot release a session in state {:?}; release is only valid while the user holds control",
                session.state()
            )));
        }
        session.transition(SessionState::Active)?;
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event("session.release", serde_json::json!({}))
                .await;
        }
        Ok(self.session_result(&session))
    }

    /// Stop a session: cancel in-flight work, close its trace, release the
    /// control lock, and mark it `stopped`. Idempotent.
    pub async fn session_stop(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
        match session.state() {
            SessionState::Stopped => return Ok(self.session_result(&session)),
            SessionState::Stopping => {
                session.set_state(SessionState::Stopped);
                self.control_lock.release(session_id);
                return Ok(self.session_result(&session));
            }
            _ => {}
        }
        session.transition(SessionState::Stopping)?; // cancels in-flight
        if let Some(t) = session.trace.as_ref() {
            let _ = t
                .record_event("session.stop", serde_json::json!({ "reason": "requested" }))
                .await;
            let _ = t.close().await;
        }
        session.set_state(SessionState::Stopped);
        self.control_lock.release(session_id);
        tracing::info!(session = %session_id, "session stopped");
        Ok(self.session_result(&session))
    }

    /// Dispatch a `computer.session` action.
    pub async fn session(
        &self,
        action: SessionAction,
        session_id: Option<&str>,
        display_id: Option<String>,
        client: ClientInfo,
    ) -> Result<SessionResult, CuError> {
        match action {
            SessionAction::Start => self.session_start(display_id, client).await,
            SessionAction::Status => match session_id {
                Some(id) => self.session_status(id).await,
                None => {
                    // No active session is a *typed* error, not a malformed
                    // request: adapters auto-start on SESSION_NOT_FOUND and
                    // must not have to sniff an INVALID_PARAMS message.
                    let holder = self.active_session_id().ok_or_else(|| {
                        CuError::SessionNotFound("No active computer-use session exists.".into())
                    })?;
                    self.session_status(&holder).await
                }
            },
            SessionAction::Pause => {
                self.session_pause(session_id.ok_or_else(|| {
                    CuError::InvalidParams("session.pause requires session_id".into())
                })?)
                .await
            }
            SessionAction::Resume => {
                self.session_resume(session_id.ok_or_else(|| {
                    CuError::InvalidParams("session.resume requires session_id".into())
                })?)
                .await
            }
            SessionAction::Takeover => {
                self.session_takeover(session_id.ok_or_else(|| {
                    CuError::InvalidParams("session.takeover requires session_id".into())
                })?)
                .await
            }
            SessionAction::Release => {
                self.session_release(session_id.ok_or_else(|| {
                    CuError::InvalidParams("session.release requires session_id".into())
                })?)
                .await
            }
            SessionAction::Stop => {
                self.session_stop(session_id.ok_or_else(|| {
                    CuError::InvalidParams("session.stop requires session_id".into())
                })?)
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
        self.gate_active(&session)?;
        let _busy = session.busy.lock().await;
        self.observe_inner(&session, params, request_id).await
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

        let request = CaptureRequest {
            display_id: display_id.clone(),
            output_path: output_path.clone(),
            include_cursor: params.include_cursor.unwrap_or(true),
            max_width: params.max_width.unwrap_or(self.config.observe_max_width),
            format: format.clone(),
            jpeg_quality: params
                .jpeg_quality
                .unwrap_or(self.config.observe_jpeg_quality),
        };
        // A capture failure is a CAPTURE_FAILED error, distinct from generic
        // driver failures, so agents can distinguish "screen could not be
        // captured" from other bridge problems.
        let captured = self
            .driver
            .capture(request)
            .await
            .map_err(|e| CuError::CaptureFailed(e.to_string()))?;

        // A cheap snapshot gives us the thumbnail + live app name for the
        // stale-frame fingerprint of this frame.
        let snapshot: ScreenSnapshot = self.driver.quick_snapshot(&display_id).await?.into();

        let frame = ScreenFrame {
            frame_id: frame_id.clone(),
            session_id: session_id.clone(),
            captured_at: captured.captured_at,
            image_path: Some(captured.image_path.clone()),
            image_bytes: Some(captured.image_bytes.clone()),
            width: captured.width,
            height: captured.height,
            display_id: captured.display_id.clone(),
            bounds: captured.bounds,
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
        })
    }

    // ------------------------------------------------------------------
    // Act
    // ------------------------------------------------------------------

    pub async fn act(
        &self,
        params: ActParams,
        request_id: Option<String>,
    ) -> Result<ActResult, CuError> {
        let session = self.get_session(&params.session_id)?;
        self.gate_active(&session)?;
        // Serialize observe/act per session so two batches never interleave.
        let _busy = session.busy.lock().await;
        // The control lock must be held by this session; a stopped/replaced
        // session must never drive the pointer.
        if !self.control_lock.is_held_by(&params.session_id) {
            return Err(CuError::ControlLocked {
                holder: self.control_lock.holder().unwrap_or_default(),
            });
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

        // Stale-frame check against the *live* desktop.
        let before_q = self
            .driver
            .quick_snapshot(&session.display_id)
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
            return Err(self.stale.to_error(&verdict));
        }

        // Bounds pre-check: every location-bearing action must be on-screen
        // before anything moves.
        if !cu_policy::batch_in_bounds(&batch.actions, &geometry) {
            for a in &batch.actions {
                cu_policy::resolve_action_points(a, &geometry)?;
            }
        }

        // Execute the batch.
        let token = session.begin_batch();
        let mut takeover = TakeoverDetector {
            policy: self.config.takeover_policy,
            ..Default::default()
        };
        let queue = ActionQueue::new(self.driver.as_ref());
        let runs = queue
            .run(
                &session,
                &batch.actions,
                &geometry,
                token.clone(),
                &mut takeover,
                session.trace.as_ref(),
                request_id.as_deref(),
                &params.frame_id,
                &session.display_id,
                before_screen.active_application.as_deref(),
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
                match stabilizer
                    .until_stable(&session.display_id, &before_q, &token)
                    .await
                {
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
                    Err(e) => return Err(e),
                }
            }
        }

        // Post-batch snapshot → screen_changed / stable.
        let after_q = self.driver.quick_snapshot(&session.display_id).await?;
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

    pub async fn inspect(&self, params: InspectParams) -> Result<InspectResult, CuError> {
        let _session = self.get_session(&params.session_id)?;
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
        let (px, py, w, h) = params.region.to_image_pixels(&geometry)?;

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

    /// Stop every session, close traces, and release driver resources.
    pub async fn shutdown(&self) -> Result<(), CuError> {
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.session_stop(&id).await;
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

    fn gate_active(&self, session: &Session) -> Result<(), CuError> {
        match session.state() {
            SessionState::Active => Ok(()),
            SessionState::Starting => Err(CuError::NotReady("session is still starting".into())),
            SessionState::Paused => Err(CuError::Paused),
            SessionState::UserTakeover => Err(CuError::UserTakeover),
            SessionState::Stopping | SessionState::Stopped | SessionState::Failed => Err(
                CuError::InvalidSessionState(format!("session is {:?}", session.state())),
            ),
        }
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
            owner_client_id: session.owner.as_ref().map(|c| c.client_id.clone()),
            owner_client_name: session.owner.as_ref().map(|c| c.client_name.clone()),
            owner_instance_id: session.owner.as_ref().map(|c| c.client_instance_id.clone()),
            message: None,
        }
    }
}

/// Pull the machine-readable error code out of a runtime error (used by the
/// daemon to build JSON-RPC errors).
pub fn error_code(e: &CuError) -> ErrorCode {
    e.code()
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
            // 4x4 dark PNG.
            let img = image::RgbImage::from_pixel(4, 4, image::Rgb([40, 40, 40]));
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            std::fs::write(&request.output_path, &buf).unwrap();
            Ok(cu_driver::CapturedFrame {
                display_id: request.display_id,
                width: 4,
                height: 4,
                scale_factor: 1.0,
                bounds: cu_core::DisplayBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 4.0,
                    height: 4.0,
                },
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
                thumbnail: vec![0u8; 64],
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
            Ok(None)
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
        (rt, driver)
    }

    #[tokio::test]
    async fn session_lifecycle_and_lock() {
        let rt = runtime().await;
        let started = rt.session_start(None, test_client()).await.unwrap();
        assert_eq!(started.state, SessionState::Active);
        assert!(started.lock_held);

        // A second session must be rejected by the control lock.
        let err = rt.session_start(None, test_client()).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::ControlLocked);

        // Pause gates act.
        let p = rt.session_pause(&started.session_id).await.unwrap();
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
        };
        let err = rt.act(act_params, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::Paused);

        rt.session_resume(&started.session_id).await.unwrap();
        rt.session_stop(&started.session_id).await.unwrap();
        let status = rt.session_status(&started.session_id).await.unwrap();
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
            .session(SessionAction::Status, None, None, test_client())
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
        let started = rt.session_start(None, test_client()).await.unwrap();
        assert_eq!(started.owner_client_id.as_deref(), Some("test"));
        assert_eq!(started.owner_client_name.as_deref(), Some("Test client"));
        assert_eq!(started.owner_instance_id.as_deref(), Some("test-1"));
        assert_eq!(started.started_by, "Test client");

        // A second client starting (or querying) sees the same owner — it
        // never becomes the owner by observing.
        let status = rt.session_status(&started.session_id).await.unwrap();
        assert_eq!(status.owner_client_id.as_deref(), Some("test"));
    }

    fn wait_params(session_id: &str, frame_id: &str) -> ActParams {
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
        }
    }

    /// The full takeover/resume/release matrix. A takeover must NOT be
    /// bypassable by a plain `resume`: resume only recovers `Paused`, release
    /// is the only exit from `UserTakeover`, and release outside takeover is
    /// itself rejected.
    #[tokio::test]
    async fn takeover_cannot_be_bypassed_by_resume() {
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let sid = s.session_id.clone();

        // 1. Pause → act rejected.
        rt.session_pause(&sid).await.unwrap();
        let err = rt
            .act(wait_params(&sid, "frame_x"), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::Paused);

        // 2. Pause → resume succeeds.
        rt.session_resume(&sid).await.unwrap();
        let st = rt.session_status(&sid).await.unwrap();
        assert_eq!(st.state, SessionState::Active);

        // 3. Takeover → act rejected.
        rt.session_takeover(&sid).await.unwrap();
        let err = rt
            .act(wait_params(&sid, "frame_x"), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeover);

        // 4. Takeover → resume REJECTED with USER_TAKEOVER_ACTIVE; state holds.
        let err = rt.session_resume(&sid).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeoverActive);
        assert!(
            err.to_string().contains("release"),
            "error must point at release: {err}"
        );
        let st = rt.session_status(&sid).await.unwrap();
        assert_eq!(st.state, SessionState::UserTakeover);
        assert!(st.user_takeover);
        assert!(!st.paused);

        // 5. Takeover → release succeeds and returns to Active.
        let rel = rt.session_release(&sid).await.unwrap();
        assert_eq!(rel.state, SessionState::Active);
        assert!(!rel.user_takeover);

        // 6. After release, acting works again (fresh frame, fresh act).
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
                    include_image: Some(false),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res = rt
            .act(wait_params(&sid, &obs.frame_id), None)
            .await
            .unwrap();
        assert!(res.executed);

        // 7. Release outside takeover is rejected (no silent no-op).
        let err = rt.session_release(&sid).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::InvalidSessionState);

        rt.session_stop(&sid).await.unwrap();
    }

    /// Takeover mid-batch: the in-flight act stops at the next safe boundary
    /// and the remaining actions are reported `cancelled` — none execute.
    #[tokio::test]
    async fn takeover_cancels_in_flight_actions() {
        let (rt, fake) = runtime_with_driver().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let sid = s.session_id.clone();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(sid.clone()),
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
                // reported as done. The waits are what the takeover interrupts.
                ComputerAction::Move {
                    x: 400.0,
                    y: 400.0,
                    coordinate_space: cu_core::CoordinateSpace::Normalized1000,
                    duration_ms: None,
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
        };

        let rt2 = rt.clone();
        let handle = tokio::spawn(async move { rt2.act(params, None).await });
        tokio::time::sleep(Duration::from_millis(40)).await;
        rt.session_takeover(&sid).await.unwrap();

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
            .act(wait_params(&sid, &obs.frame_id), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::UserTakeover);
        rt.session_release(&sid).await.unwrap();
        rt.session_stop(&sid).await.unwrap();
    }

    #[tokio::test]
    async fn act_rejects_unknown_frame_and_stale() {
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
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
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn act_strict_policy_rejects_older_frames() {
        // Default policy is Strict: only the session's current frame is
        // actionable, even when the older frame's pixels still match.
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs1 = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
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
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_ne!(obs1.frame_id, obs2.frame_id);
        let err = rt
            .act(wait_params(&s.session_id, &obs1.frame_id), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), ErrorCode::StaleFrame);
        // The current frame still runs (fake screen is unchanged).
        rt.act(wait_params(&s.session_id, &obs2.frame_id), None)
            .await
            .unwrap();
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn act_visual_match_accepts_older_identical_frames() {
        // VisualMatch policy: an older frame whose content still matches the
        // live screen is actionable (the pre-strict runtime behavior).
        let mut cfg = test_config();
        cfg.stale.policy = crate::stale_frame::StaleFramePolicy::VisualMatch;
        let rt = runtime_with_config(cfg).await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs1 = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
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
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        // The older frame still matches the live screen → allowed.
        rt.act(wait_params(&s.session_id, &obs1.frame_id), None)
            .await
            .unwrap();
        rt.act(wait_params(&s.session_id, &obs2.frame_id), None)
            .await
            .unwrap();
        rt.session_stop(&s.session_id).await.unwrap();
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
        let err = rt.session_start(None, test_client()).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::TraceError);
        std::fs::remove_file(&file).unwrap();
    }

    #[tokio::test]
    async fn act_reports_trace_mode_and_degradation() {
        // Best-effort (default): act carries a trace report, mode best_effort.
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res = rt
            .act(wait_params(&s.session_id, &obs.frame_id), None)
            .await
            .unwrap();
        let trace = res.trace.expect("best_effort act must report trace status");
        assert_eq!(trace.mode, "best_effort");
        assert!(!trace.degraded);
        assert!(trace.warnings.is_empty());
        rt.session_stop(&s.session_id).await.unwrap();

        // Disabled: no recorder exists → no trace report.
        let mut cfg = test_config();
        cfg.trace_mode = cu_trace::TraceMode::Disabled;
        let rt2 = runtime_with_config(cfg).await;
        let s2 = rt2.session_start(None, test_client()).await.unwrap();
        let obs2 = rt2
            .observe(
                ObserveParams {
                    session_id: Some(s2.session_id.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let res2 = rt2
            .act(wait_params(&s2.session_id, &obs2.frame_id), None)
            .await
            .unwrap();
        assert!(res2.trace.is_none(), "disabled mode has no trace report");
        rt2.session_stop(&s2.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn act_out_of_bounds_rejected() {
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
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
        };
        let err = rt.act(params, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::OutOfBounds);
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn confirmation_required_is_enforced() {
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
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
        };
        let err = rt.act(params, None).await.unwrap_err();
        assert_eq!(err.code(), ErrorCode::ConfirmationRequired);
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn inspect_crops_and_maps() {
        let rt = runtime().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
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
            })
            .await
            .unwrap();
        assert_eq!(res.width, 2);
        assert_eq!(res.height, 2);
        assert!(!res.image_base64.is_empty());
        assert_eq!(res.mapping.source_image_rect.x, 0.0);
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_stops_a_long_wait_fast_with_per_action_reports() {
        // The full cancel chain (SDK abort → computer.cancel → batch token):
        // a 10s wait action inside a batch must stop within ~1s of the cancel,
        // and the report marks the interrupted wait (and everything after it)
        // `cancelled` — not `failed`, and not an internal error.
        let (rt, _driver) = runtime_with_driver().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let sid = s.session_id.clone();
        let frame = obs.frame_id.clone();
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
                },
                None,
            )
            .await
        });
        // Let the first action run and the wait begin, then cancel.
        tokio::time::sleep(Duration::from_millis(120)).await;
        rt.cancel_in_flight(&s.session_id).unwrap();
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
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_stops_a_fixed_wait_fast_as_an_explicit_cancellation() {
        // wait_policy=fixed with a long duration must also stop quickly on
        // cancel, surfacing CANCELLED (not ACTION_TIMEOUT / internal error).
        let (rt, _driver) = runtime_with_driver().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let sid = s.session_id.clone();
        let frame = obs.frame_id.clone();
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
                },
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        rt.cancel_in_flight(&s.session_id).unwrap();
        let started = Instant::now();
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel must stop the 60s fixed wait fast, took {:?}",
            started.elapsed()
        );
        assert_eq!(err.code(), ErrorCode::Cancelled);
        rt.session_stop(&s.session_id).await.unwrap();
    }

    #[tokio::test]
    async fn stop_during_until_stable_returns_immediately() {
        // The stabilizer's own cancellation: stopping the session mid
        // until_stable must abort the wait (session.stop cancels the batch
        // token). The act call errors with CANCELLED rather than hanging.
        let (rt, _driver) = runtime_with_driver().await;
        let s = rt.session_start(None, test_client()).await.unwrap();
        let obs = rt
            .observe(
                ObserveParams {
                    session_id: Some(s.session_id.clone()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let sid = s.session_id.clone();
        let frame = obs.frame_id.clone();
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
                },
                None,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Stopping the session cancels its in-flight batch token.
        rt.session_stop(&s.session_id).await.unwrap();
        let started = Instant::now();
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop must abort until_stable fast, took {:?}",
            started.elapsed()
        );
        assert_eq!(err.code(), ErrorCode::Cancelled);
    }
}
