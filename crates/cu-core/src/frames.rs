//! Screen frames: the objects a model reasons over and the fingerprints that
//! let the runtime reject out-of-date observations.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A captured screen frame plus the metadata that makes it addressable.
///
/// Raw image bytes have an explicit lifetime: the runtime keeps them in memory
/// only for recently-created frames (see the frame store's retention policy),
/// then drops the bytes and retains the on-disk `image_path` plus a perceptual
/// fingerprint. Upper layers always receive the image (base64 or path) at
/// observe time; they must not assume bytes are cached forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenFrame {
    pub frame_id: String,
    pub session_id: String,
    pub captured_at: DateTime<Utc>,
    pub image_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_bytes: Option<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub display_id: String,
    pub scale_factor: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_application: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_window_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub perceptual_hash: Option<String>,
}

/// The lightweight fingerprint used to decide whether the desktop changed
/// since a referenced frame was captured. This is what [`crate::frames::StaleFrameChecker`]
/// compares against the *current* desktop, not the full-resolution image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    /// Small grayscale thumbnail of the desktop (e.g. 64x64), flattened row-major.
    pub thumbnail: Vec<u8>,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub active_application: Option<String>,
    pub active_window_title: Option<String>,
    pub display_id: String,
    pub captured_at: DateTime<Utc>,
}

impl ScreenSnapshot {
    /// Mean absolute pixel difference in `[0,1]` between two snapshots that
    /// have the same display and thumbnail size. Returns `None` when the
    /// snapshots are not directly comparable (different display or size).
    pub fn change_score(&self, other: &ScreenSnapshot) -> Option<f64> {
        if self.display_id != other.display_id
            || self.thumb_width != other.thumb_width
            || self.thumb_height != other.thumb_height
            || self.thumbnail.len() != other.thumbnail.len()
        {
            return None;
        }
        let n = self.thumbnail.len() as f64;
        if n == 0.0 {
            return Some(0.0);
        }
        let sum: u64 = self
            .thumbnail
            .iter()
            .zip(other.thumbnail.iter())
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum();
        Some(sum as f64 / n / 255.0)
    }

    /// A coarse 64-bit average hash used as an equality/neighborhood key.
    pub fn perceptual_hash(&self) -> String {
        if self.thumbnail.is_empty() {
            return String::new();
        }
        let sum: u64 = self.thumbnail.iter().map(|&b| u64::from(b)).sum();
        let avg = sum / self.thumbnail.len() as u64;
        let mut bits: u64 = 0;
        for (i, &b) in self.thumbnail.iter().enumerate().take(64) {
            if u64::from(b) >= avg {
                bits |= 1 << (63 - i);
            }
        }
        format!("{bits:016x}")
    }

    /// Hash distance (number of differing bits) between two hashes.
    pub fn hash_distance(&self, other_hash: &str) -> u32 {
        let a = u64::from_str_radix(&self.perceptual_hash(), 16).unwrap_or(0);
        let b = u64::from_str_radix(other_hash, 16).unwrap_or(0);
        (a ^ b).count_ones()
    }
}

/// Parameters for resizing/downscaling an image into a thumbnail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThumbnailRequest {
    pub width: u32,
    pub height: u32,
}

impl Default for ThumbnailRequest {
    fn default() -> Self {
        Self { width: 64, height: 64 }
    }
}

/// The outcome of the runtime's stale-frame check for one referenced frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StaleFrameVerdict {
    pub is_stale: bool,
    pub change_score: f64,
    pub referenced_frame_id: String,
    pub current_frame_id: String,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pixels: Vec<u8>, app: Option<&str>) -> ScreenSnapshot {
        ScreenSnapshot {
            thumbnail: pixels,
            thumb_width: 4,
            thumb_height: 4,
            active_application: app.map(|s| s.to_string()),
            active_window_title: None,
            display_id: "1".into(),
            captured_at: Utc::now(),
        }
    }

    #[test]
    fn identical_snapshots_score_zero() {
        let a = snap(vec![0u8; 16], Some("TextEdit"));
        assert_eq!(a.change_score(&a).unwrap(), 0.0);
    }

    #[test]
    fn very_different_snapshots_score_high() {
        let a = snap(vec![0u8; 16], Some("TextEdit"));
        let b = snap(vec![255u8; 16], Some("TextEdit"));
        let score = a.change_score(&b).unwrap();
        assert!(score > 0.9);
    }

    #[test]
    fn differing_app_keeps_score_comparable() {
        // App changes do not affect the pixel score; the stale-frame checker
        // combines app change separately.
        let a = snap(vec![10u8; 16], Some("A"));
        let b = snap(vec![10u8; 16], Some("B"));
        assert_eq!(a.change_score(&b).unwrap(), 0.0);
    }

    #[test]
    fn different_display_not_comparable() {
        let mut a = snap(vec![0u8; 16], None);
        a.display_id = "1".into();
        let mut b = snap(vec![255u8; 16], None);
        b.display_id = "2".into();
        assert_eq!(a.change_score(&b), None);
    }

    #[test]
    fn perceptual_hash_stable_and_distinguishing() {
        // aHash is brightness-invariant: two *uniform* images hash identically.
        let a = snap(vec![0u8; 16], None);
        let b = snap(vec![255u8; 16], None);
        assert_eq!(a.perceptual_hash(), b.perceptual_hash());
        assert_eq!(a.hash_distance(&b.perceptual_hash()), 0);
        // Two structurally different patterns must differ.
        let checker: Vec<u8> = (0..16).map(|i| if i % 2 == 0 { 0 } else { 255 }).collect();
        let other: Vec<u8> = (0..16).map(|i| if i % 2 == 0 { 255 } else { 0 }).collect();
        let c = snap(checker, None);
        let d = snap(other, None);
        let dist = c.hash_distance(&d.perceptual_hash());
        assert!(dist > 0, "complementary patterns should differ in hash");
        assert_eq!(c.perceptual_hash(), c.perceptual_hash());
    }
}
