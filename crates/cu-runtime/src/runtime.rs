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
    ActParams, ActResult, CoordinateSpace, CuError, ErrorCode, ImageGeometry, InspectMapping,
    InspectParams, InspectResult, ObserveParams, ObserveResult, Region, ScreenFrame,
    ScreenSnapshot, SessionAction, SessionResult, SessionState, WaitPolicy,
};
use cu_driver::{
    ApplicationInfo, CaptureRequest, ComputerDriver, DesktopLayout, DisplayInfo, PermissionStatus,
    PointerInfo,
};
use cu_policy::confirmation::Authorization;
use cu_policy::{authorize, ConfirmationPolicy, TakeoverDetector, TakeoverPolicy};
use cu_trace::{TraceConfig, TraceRecorder};

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
        started_by: String,
    ) -> Result<SessionResult, CuError> {
        let id = format!("s_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let display_id = match display_id {
            Some(d) => d,
            None => self.driver.desktop_layout().await?.primary_id,
        };

        let trace = match TraceRecorder::open(
            &id,
            &self.config.traces_dir,
            TraceConfig {
                dev_mode: self.config.trace_dev_mode,
            },
        )
        .await
        {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "trace recorder unavailable; continuing without traces");
                None
            }
        };

        let session = SharedSession::new(Session::new(
            id.clone(),
            display_id,
            started_by.clone(),
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
                    serde_json::json!({ "display_id": session.display_id, "started_by": started_by }),
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
            SessionState::Paused | SessionState::UserTakeover => {
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

    pub async fn session_release(&self, session_id: &str) -> Result<SessionResult, CuError> {
        let session = self.get_session(session_id)?;
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
        started_by: String,
    ) -> Result<SessionResult, CuError> {
        match action {
            SessionAction::Start => self.session_start(display_id, started_by).await,
            SessionAction::Status => match session_id {
                Some(id) => self.session_status(id).await,
                None => {
                    let holder = self.active_session_id().ok_or_else(|| {
                        CuError::InvalidParams("no active session; start one first".into())
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
        let captured = self.driver.capture(request).await?;

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
        let live_id =
            cu_core::config::new_frame_id(self.frame_counter.fetch_add(1, Ordering::SeqCst));
        let verdict = self.stale.check(
            &referenced_snapshot,
            &before_screen,
            &params.frame_id,
            &live_id,
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
                token,
                &mut takeover,
                session.trace.as_ref(),
                request_id.as_deref(),
                &params.frame_id,
                &session.display_id,
                before_screen.active_application.as_deref(),
            )
            .await?;
        *session.last_action_at.lock().unwrap() = Some(Utc::now());

        // Wait policy.
        let mut stable = false;
        match batch.wait_policy {
            WaitPolicy::None => {}
            WaitPolicy::Fixed => {
                let ms = batch.fixed_wait_ms.unwrap_or(0);
                if ms > 0 {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
            WaitPolicy::UntilStable => {
                let stabilizer = Stabilizer::new(self.driver.as_ref(), self.config.stabilizer);
                match stabilizer
                    .until_stable(&session.display_id, &before_q)
                    .await
                {
                    Ok(StabilizeOutcome::Stable { .. }) => stable = true,
                    Ok(StabilizeOutcome::TimedOut { .. }) => stable = false,
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

        Ok(ActResult {
            executed,
            action_results: reports,
            screen_changed,
            stable,
            next_frame_id,
            screenshot,
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
            .ok_or_else(|| CuError::SessionNotFound(id.to_string()))
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

    /// A deterministic in-memory driver that lets us exercise every gate
    /// without a real display.
    #[derive(Default)]
    struct FakeDriver {
        pub pointer: std::sync::Mutex<Point>,
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
            if let cu_driver::ResolvedAction::Move { to, .. } = action {
                *self.pointer.lock().unwrap() = *to;
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
        let dir = tempfile::tempdir().unwrap();
        RuntimeConfig {
            traces_dir: dir.path().join("traces"),
            frames_dir: dir.path().join("frames"),
            ..RuntimeConfig::default()
        }
    }

    async fn runtime() -> Runtime {
        let driver = Arc::new(FakeDriver::default());
        Runtime::new(driver, test_config())
    }

    #[tokio::test]
    async fn session_lifecycle_and_lock() {
        let rt = runtime().await;
        let started = rt.session_start(None, "test".into()).await.unwrap();
        assert_eq!(started.state, SessionState::Active);
        assert!(started.lock_held);

        // A second session must be rejected by the control lock.
        let err = rt.session_start(None, "test".into()).await.unwrap_err();
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

    #[tokio::test]
    async fn act_rejects_unknown_frame_and_stale() {
        let rt = runtime().await;
        let s = rt.session_start(None, "test".into()).await.unwrap();
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
    async fn act_out_of_bounds_rejected() {
        let rt = runtime().await;
        let s = rt.session_start(None, "test".into()).await.unwrap();
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
        let s = rt.session_start(None, "test".into()).await.unwrap();
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
        let s = rt.session_start(None, "test".into()).await.unwrap();
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
}
