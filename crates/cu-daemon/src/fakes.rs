//! Shared test doubles for the daemon crate (test builds only).
//!
//! [`FakeDriver`] is a deterministic in-memory driver so the full dispatch
//! and server paths can be exercised without a real display. Waits actually
//! sleep (so in-flight cancellation is observable); side-effect counters
//! prove which driver calls were (or were not) reached.
#![cfg(test)]

use cu_core::errors::CuError;
use cu_core::security::SecretTokenHash;
use cu_driver::{
    ApplicationInfo, CaptureRequest, DesktopLayout, DisplayInfo, PermissionStatus, PointerInfo,
};
use cu_runtime::RuntimeConfig;

/// A deterministic in-memory driver so the full dispatch path can be
/// exercised without a real display. Waits actually sleep (so in-flight
/// cancellation is observable); every execute is counted.
#[derive(Default)]
pub struct FakeDriver {
    pub executes: std::sync::atomic::AtomicUsize,
    /// Capture count — the observable side effect of `computer.observe`.
    /// A rejected observe must leave this at zero.
    pub captures: std::sync::atomic::AtomicUsize,
    /// Side-effect counters for the sensitive runtime introspection
    /// reads. A rejected `runtime.pointer` / `runtime.active_application`
    /// must leave both at zero — the token check happens before any
    /// driver call.
    pub pointer_calls: std::sync::atomic::AtomicUsize,
    pub active_app_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl cu_driver::ComputerDriver for FakeDriver {
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
    async fn capture(&self, request: CaptureRequest) -> Result<cu_driver::CapturedFrame, CuError> {
        self.captures
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // A real (tiny) PNG so the inspect pixel-read path can decode it.
        let png: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 2, 0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207,
            192, 0, 0, 3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
            130,
        ];
        std::fs::write(&request.output_path, png).unwrap();
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
            image_bytes: png.to_vec(),
            format: request.format,
            active_application: None,
            captured_at: chrono::Utc::now(),
        })
    }
    async fn quick_snapshot(&self, display_id: &str) -> Result<cu_driver::QuickSnapshot, CuError> {
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
        if let cu_driver::ResolvedAction::Wait { duration_ms } = action {
            tokio::time::sleep(std::time::Duration::from_millis((*duration_ms).min(1000))).await;
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
        self.active_app_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }
    async fn pointer_location(&self) -> Result<PointerInfo, CuError> {
        self.pointer_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PointerInfo {
            location: cu_core::Point::new(0.0, 0.0),
            display_id: Some("1".into()),
        })
    }
    async fn shutdown(&self) -> Result<(), CuError> {
        Ok(())
    }
}

/// A runtime config pointing at a per-process temp dir, so daemon tests never
/// touch the real user state directories.
pub fn test_config() -> RuntimeConfig {
    let dir = std::env::temp_dir().join(format!("cu-daemon-tests-{}", std::process::id()));
    RuntimeConfig {
        traces_dir: dir.join("traces"),
        frames_dir: dir.join("frames"),
        ..RuntimeConfig::default()
    }
}

/// The daemon's admin credential for tests: one shared token/hash pair so
/// shutdown tests can present the token they verified against.
pub fn test_admin() -> (cu_core::security::DaemonAdminToken, SecretTokenHash) {
    static ADMIN: std::sync::OnceLock<(cu_core::security::DaemonAdminToken, SecretTokenHash)> =
        std::sync::OnceLock::new();
    ADMIN
        .get_or_init(|| {
            let token = cu_core::security::generate_daemon_admin_token();
            let hash = SecretTokenHash::from_token(&token);
            (token, hash)
        })
        .clone()
}
