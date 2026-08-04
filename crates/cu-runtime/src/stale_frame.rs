//! Stale-frame detection: the guard that stops a model from acting on an
//! out-of-date screenshot.
//!
//! A referenced frame is treated as stale when *any* of these hold:
//! - the desktop's thumbnail changed more than `threshold` (visual comparison);
//! - the active application changed (window/content swap);
//! - the referenced display is no longer present in the current layout;
//! - the frame is older than `max_frame_age_secs` (wall-clock backstop).
//!
//! Wall-clock age is deliberately a backstop, not the primary signal. The
//! primary signal is visual: a clock ticking or the cursor moving scores below
//! the threshold; a real content change scores above it.

use chrono::Utc;
use cu_core::{ScreenSnapshot, StaleFrameDetail, StaleFrameVerdict};

/// How strictly a referenced `frame_id` must match the current screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaleFramePolicy {
    /// Only the **current** frame is actionable. Acting on any older
    /// `frame_id` is `STALE_FRAME`, regardless of visual similarity. This is
    /// the default: an agent must re-observe between action batches.
    #[default]
    Strict,
    /// Older frames are acceptable as long as the live screen still matches
    /// them (visual comparison + app change + age backstop).
    VisualMatch,
}

impl StaleFramePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            StaleFramePolicy::Strict => "strict",
            StaleFramePolicy::VisualMatch => "visual_match",
        }
    }

    /// Parse from the environment's `COMPUTER_USE_STALE_POLICY` value.
    pub fn from_env(s: Option<&str>) -> Self {
        match s {
            Some("visual_match") => StaleFramePolicy::VisualMatch,
            _ => StaleFramePolicy::Strict,
        }
    }
}

/// Tuning for the stale-frame check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaleFrameConfig {
    /// 0..=1 normalized thumbnail difference above which the frame is stale.
    pub threshold: f64,
    /// Seconds a referenced frame is trusted regardless of visual similarity.
    pub max_age_secs: u64,
    /// Treat a change of active application as stale even if pixels look alike.
    pub app_change_is_stale: bool,
    /// Which stale policy applies. Strict additionally rejects any referenced
    /// frame that is not the current one.
    pub policy: StaleFramePolicy,
}

impl Default for StaleFrameConfig {
    fn default() -> Self {
        Self {
            threshold: cu_core::config::DEFAULT_STALE_THRESHOLD,
            max_age_secs: cu_core::config::DEFAULT_MAX_FRAME_AGE_SECS,
            app_change_is_stale: cu_core::config::DEFAULT_APP_CHANGE_IS_STALE,
            policy: StaleFramePolicy::Strict,
        }
    }
}

/// Pure decision logic; the runtime feeds it a referenced snapshot and the
/// current snapshot.
pub struct StaleFrameChecker {
    pub config: StaleFrameConfig,
}

impl StaleFrameChecker {
    pub fn new(config: StaleFrameConfig) -> Self {
        Self { config }
    }

    pub fn check(
        &self,
        referenced: &ScreenSnapshot,
        current: &ScreenSnapshot,
        referenced_frame_id: &str,
        current_frame_id: &str,
    ) -> StaleFrameVerdict {
        // 0. Strict policy: only the current frame is actionable.
        if self.config.policy == StaleFramePolicy::Strict && referenced_frame_id != current_frame_id
        {
            return StaleFrameVerdict {
                is_stale: true,
                change_score: 1.0,
                referenced_frame_id: referenced_frame_id.into(),
                current_frame_id: current_frame_id.into(),
                reason: format!(
                    "frame {referenced_frame_id} is not the current frame \
                     ({current_frame_id}) under the strict stale-frame policy"
                ),
            };
        }

        // 1. Display change (different display or no comparable snapshot).
        if referenced.display_id != current.display_id {
            return StaleFrameVerdict {
                is_stale: true,
                change_score: 1.0,
                referenced_frame_id: referenced_frame_id.into(),
                current_frame_id: current_frame_id.into(),
                reason: format!(
                    "display changed from {} to {}",
                    referenced.display_id, current.display_id
                ),
            };
        }

        let mut reasons: Vec<String> = Vec::new();
        let mut score: f64 = 0.0;

        // 2. App change.
        if self.config.app_change_is_stale {
            let apps_differ = match (&referenced.active_application, &current.active_application) {
                (Some(a), Some(b)) => a != b,
                (Some(_), None) => false, // missing info should not nuke a valid frame
                (None, Some(_)) => false,
                (None, None) => false,
            };
            if apps_differ {
                reasons.push("active application changed".to_string());
                score = score.max(0.5);
            }
        }

        // 3. Visual change.
        if let Some(diff) = referenced.change_score(current) {
            score = score.max(diff);
            if diff > self.config.threshold {
                reasons.push(format!(
                    "screen content changed ({diff:.3} > {:.3})",
                    self.config.threshold
                ));
            }
        }

        // 4. Age backstop.
        let age = Utc::now()
            .signed_duration_since(referenced.captured_at)
            .num_seconds();
        if age < 0 || age as u64 > self.config.max_age_secs {
            reasons.push(format!(
                "frame is {age}s old (max {})",
                self.config.max_age_secs
            ));
            score = score.max(1.0);
        }

        StaleFrameVerdict {
            is_stale: !reasons.is_empty(),
            change_score: score,
            referenced_frame_id: referenced_frame_id.into(),
            current_frame_id: current_frame_id.into(),
            reason: if reasons.is_empty() {
                "no significant change".into()
            } else {
                reasons.join("; ")
            },
        }
    }

    /// Convert a stale verdict into the wire-level error detail.
    pub fn to_error(&self, verdict: &StaleFrameVerdict) -> cu_core::CuError {
        cu_core::CuError::StaleFrame(StaleFrameDetail {
            referenced_frame_id: verdict.referenced_frame_id.clone(),
            current_frame_id: verdict.current_frame_id.clone(),
            change_score: verdict.change_score,
            reason: verdict.reason.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pixels: Vec<u8>, app: Option<&str>, display: &str) -> ScreenSnapshot {
        ScreenSnapshot {
            thumbnail: pixels,
            thumb_width: 4,
            thumb_height: 4,
            active_application: app.map(|s| s.to_string()),
            active_window_title: None,
            display_id: display.into(),
            captured_at: Utc::now(),
        }
    }

    fn config() -> StaleFrameConfig {
        // The visual-comparison tests below exercise the visual machinery;
        // use VisualMatch so a differing id does not short-circuit them.
        StaleFrameConfig {
            threshold: 0.12,
            max_age_secs: 120,
            app_change_is_stale: true,
            policy: StaleFramePolicy::VisualMatch,
        }
    }

    fn strict_config() -> StaleFrameConfig {
        StaleFrameConfig {
            policy: StaleFramePolicy::Strict,
            ..config()
        }
    }

    #[test]
    fn strict_rejects_any_non_current_frame() {
        let checker = StaleFrameChecker::new(strict_config());
        // Identical pixels, but the ids differ → stale under strict.
        let a = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let b = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(v.is_stale);
        assert!(v.reason.contains("strict"));
        assert_eq!(v.change_score, 1.0);
        // The current frame is fresh.
        let v2 = checker.check(&a, &b, "frame_2", "frame_2");
        assert!(!v2.is_stale);
    }

    #[test]
    fn visual_match_allows_older_identical_frames() {
        let checker = StaleFrameChecker::new(config());
        let a = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let b = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(
            !v.is_stale,
            "visual_match accepts an older frame that still matches"
        );
    }

    #[test]
    fn identical_frames_are_fresh() {
        let checker = StaleFrameChecker::new(config());
        let a = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let b = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(!v.is_stale);
        assert!(v.change_score < 0.1);
    }

    #[test]
    fn major_change_is_stale() {
        let checker = StaleFrameChecker::new(config());
        let a = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let b = snap(vec![255u8; 16], Some("TextEdit"), "1");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(v.is_stale);
        assert!(v.change_score > 0.8);
        let detail = checker.to_error(&v);
        assert!(matches!(detail, cu_core::CuError::StaleFrame(_)));
    }

    #[test]
    fn app_change_is_stale() {
        let checker = StaleFrameChecker::new(config());
        let a = snap(vec![0u8; 16], Some("TextEdit"), "1");
        let b = snap(vec![0u8; 16], Some("Safari"), "1");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(v.is_stale);
        assert!(v.reason.contains("application"));
    }

    #[test]
    fn cursor_only_motion_not_stale() {
        // A thumbnail that differs only slightly (e.g. cursor moved).
        let checker = StaleFrameChecker::new(config());
        let a = vec![0u8; 256];
        let mut b = vec![0u8; 256];
        // Perturb a handful of pixels well under the threshold.
        for i in [0usize, 1, 2, 3, 4, 5] {
            b[i] = 255;
        }
        let s1 = snap(a, Some("TextEdit"), "1");
        let s2 = snap(b, Some("TextEdit"), "1");
        let score = s1.change_score(&s2).unwrap();
        assert!(score < 0.12, "score {score} should stay below threshold");
        let v = checker.check(&s1, &s2, "frame_1", "frame_2");
        assert!(!v.is_stale);
    }

    #[test]
    fn display_change_is_stale() {
        let checker = StaleFrameChecker::new(config());
        let a = snap(vec![0u8; 16], None, "1");
        let b = snap(vec![0u8; 16], None, "2");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(v.is_stale);
    }

    #[test]
    fn old_frame_is_stale_regardless_of_pixels() {
        let mut checker = StaleFrameChecker::new(config());
        checker.config.max_age_secs = 1;
        let mut a = snap(vec![0u8; 16], Some("A"), "1");
        a.captured_at = Utc::now() - chrono::Duration::seconds(60);
        let b = snap(vec![0u8; 16], Some("A"), "1");
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(v.is_stale);
        assert!(v.reason.contains("old"));
    }

    #[test]
    fn differing_display_sizes_not_comparable_handled() {
        // change_score returns None for mismatched thumbnails → not stale
        // unless another signal fires. This guards against panics.
        let checker = StaleFrameChecker::new(config());
        let a = ScreenSnapshot {
            thumbnail: vec![0u8; 16],
            thumb_width: 4,
            thumb_height: 4,
            active_application: Some("A".into()),
            active_window_title: None,
            display_id: "1".into(),
            captured_at: Utc::now(),
        };
        let b = ScreenSnapshot {
            thumbnail: vec![0u8; 64],
            thumb_width: 8,
            thumb_height: 8,
            active_application: Some("A".into()),
            active_window_title: None,
            display_id: "1".into(),
            captured_at: Utc::now(),
        };
        let v = checker.check(&a, &b, "frame_1", "frame_2");
        assert!(
            !v.is_stale,
            "mismatched snapshot sizes must not panic or misfire"
        );
    }
}
