//! Data types produced by a [`ComputerDriver`]. These are the platform-neutral
//! view of a display, a capture, permissions, and the active app — the macOS
//! implementation fills them from CoreGraphics / AppKit, a future Windows or
//! Linux implementation from its own APIs.

use cu_core::{DisplayBounds, Point};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One physical display attached to the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Stable identifier (macOS uses the CGDirectDisplayID as a string).
    pub id: String,
    /// Human-readable name when available.
    pub name: String,
    /// Logical bounds in global desktop coordinates (may be negative).
    pub bounds: DisplayBounds,
    /// Backing-store pixel dimensions.
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Backing scale factor (2.0 on Retina, 1.0 elsewhere).
    pub scale_factor: f64,
    pub is_main: bool,
}

/// What the caller wants from a capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureRequest {
    /// Display to capture.
    pub display_id: String,
    /// Where to write the encoded image file.
    pub output_path: PathBuf,
    /// Whether the cursor should be drawn into the image.
    pub include_cursor: bool,
    /// Downscale the image so its width does not exceed this (keeps aspect).
    pub max_width: u32,
    /// `png` or `jpeg`.
    pub format: String,
    /// JPEG quality 1..=100 (only used for jpeg).
    pub jpeg_quality: u8,
}

/// A captured frame as delivered by the driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturedFrame {
    pub display_id: String,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    pub bounds: DisplayBounds,
    pub image_path: PathBuf,
    /// Raw encoded bytes; present if the capture produced them directly.
    pub image_bytes: Vec<u8>,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_application: Option<ApplicationInfo>,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

/// A quick, low-resolution snapshot used by stale-frame detection and the
/// stabilizer. Kept cheap on purpose.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickSnapshot {
    pub thumbnail: Vec<u8>,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub display_id: String,
    pub active_application: Option<ApplicationInfo>,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

impl From<QuickSnapshot> for cu_core::ScreenSnapshot {
    fn from(q: QuickSnapshot) -> Self {
        cu_core::ScreenSnapshot {
            thumbnail: q.thumbnail,
            thumb_width: q.thumb_width,
            thumb_height: q.thumb_height,
            active_application: q.active_application.as_ref().map(|a| a.name.clone()),
            active_window_title: q
                .active_application
                .as_ref()
                .and_then(|a| a.window_title.clone()),
            display_id: q.display_id,
            captured_at: q.captured_at,
        }
    }
}

/// Result of executing one action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    pub success: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Permission state for the two macOS capabilities the runtime depends on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub screen_recording: bool,
    pub accessibility: bool,
}

impl PermissionStatus {
    pub fn all_granted(&self) -> bool {
        self.screen_recording && self.accessibility
    }
}

/// The frontmost application and (when accessible) its focused window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplicationInfo {
    pub bundle_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_title: Option<String>,
}

/// Where the pointer currently sits (global logical points), for trace/inspector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointerInfo {
    pub location: Point,
    pub display_id: Option<String>,
}

/// The current desktop layout, for coordinate conversion and change detection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopLayout {
    pub displays: Vec<DisplayInfo>,
    pub primary_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_snapshot_converts_to_core_snapshot() {
        let q = QuickSnapshot {
            thumbnail: vec![0u8; 4],
            thumb_width: 2,
            thumb_height: 2,
            display_id: "1".into(),
            active_application: Some(ApplicationInfo {
                bundle_id: "com.apple.TextEdit".into(),
                name: "TextEdit".into(),
                window_title: Some("Doc".into()),
            }),
            captured_at: chrono::Utc::now(),
        };
        let s: cu_core::ScreenSnapshot = q.into();
        assert_eq!(s.active_application.as_deref(), Some("TextEdit"));
        assert_eq!(s.active_window_title.as_deref(), Some("Doc"));
    }

    #[test]
    fn permission_status_all_granted() {
        let p = PermissionStatus {
            screen_recording: true,
            accessibility: true,
        };
        assert!(p.all_granted());
        let p = PermissionStatus {
            screen_recording: false,
            accessibility: true,
        };
        assert!(!p.all_granted());
    }
}
