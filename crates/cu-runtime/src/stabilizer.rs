//! Screen stabilizer: after a batch of actions, optionally wait until the
//! desktop stops changing before telling the agent "done".
//!
//! - `WaitPolicy::None`   — no wait.
//! - `WaitPolicy::Fixed`  — sleep a fixed duration.
//! - `WaitPolicy::UntilStable` — poll cheap thumbnails until `N` consecutive
//!   samples differ from the previous one by less than a threshold, or the
//!   maximum wait elapses.
//!
//! On timeout the outcome carries the **last measured** change score (never a
//! hardcoded 0.0): a screen that kept animating reports a high score, a screen
//! that nearly settled reports a low one. `samples` and `elapsed_ms` let the
//! caller reason about how close the wait came to success.

use std::time::{Duration, Instant};

use cu_core::CuError;
use cu_driver::{ComputerDriver, QuickSnapshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StabilizerConfig {
    pub initial_delay_ms: u64,
    pub sample_interval_ms: u64,
    pub required_stable_samples: u32,
    pub difference_threshold: f64,
    pub max_wait_ms: u64,
}

impl Default for StabilizerConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: cu_core::config::DEFAULT_STABILIZER_INITIAL_DELAY_MS,
            sample_interval_ms: cu_core::config::DEFAULT_STABILIZER_SAMPLE_INTERVAL_MS,
            required_stable_samples: cu_core::config::DEFAULT_STABILIZER_REQUIRED_STABLE_SAMPLES,
            difference_threshold: cu_core::config::DEFAULT_STABILIZER_DIFFERENCE_THRESHOLD,
            max_wait_ms: cu_core::config::DEFAULT_STABILIZER_MAX_WAIT_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StabilizeOutcome {
    /// Screen became quiet within the max wait.
    Stable { change_score: f64, samples: u32 },
    /// Never quietened in time. `change_score` is the *last measured*
    /// thumbnail difference (1.0 if no sample could be taken), `samples` the
    /// number of comparisons performed, `elapsed_ms` the time waited.
    TimedOut {
        change_score: f64,
        samples: u32,
        elapsed_ms: u64,
    },
}

/// A scriptable snapshot source so the stabilizer can be tested without a
/// display. `next()` is called once per sample; returning `None` makes the
/// source report its final snapshot forever.
pub trait SnapshotSource {
    fn next(&mut self) -> QuickSnapshot;
}

pub struct Stabilizer<'a> {
    driver: &'a dyn ComputerDriver,
    pub config: StabilizerConfig,
}

impl<'a> Stabilizer<'a> {
    pub fn new(driver: &'a dyn ComputerDriver, config: StabilizerConfig) -> Self {
        Self { driver, config }
    }

    /// Wait until the desktop is quiet. `initial` is the pre-action baseline
    /// snapshot; `token` (usually the session's in-flight batch token) aborts
    /// the wait immediately when cancelled (pause/takeover/stop) with
    /// `CuError::Cancelled`.
    pub async fn until_stable(
        &self,
        display_id: &str,
        initial: &QuickSnapshot,
        token: &CancellationToken,
    ) -> Result<StabilizeOutcome, CuError> {
        // Honour a cancellation that already fired before we started.
        if token.is_cancelled() {
            return Err(CuError::Cancelled);
        }
        let initial_delay = Duration::from_millis(self.config.initial_delay_ms);
        let sample_interval = Duration::from_millis(self.config.sample_interval_ms);
        let max_wait = Duration::from_millis(self.config.max_wait_ms);
        let started = Instant::now();

        let sleep = |d: Duration| async move {
            tokio::select! {
                () = tokio::time::sleep(d) => {}
                () = token.cancelled() => {}
            }
        };

        sleep(initial_delay).await;
        if token.is_cancelled() {
            return Err(CuError::Cancelled);
        }

        let mut prev = initial.clone();
        let mut stable_samples: u32 = 0;
        let mut last_change_score: f64 = 1.0; // pessimistic: unknown screen state
        let mut samples: u32 = 0;

        loop {
            if token.is_cancelled() {
                return Err(CuError::Cancelled);
            }
            if started.elapsed() >= max_wait {
                // Report the last measured difference — never a hardcoded 0.
                return Ok(StabilizeOutcome::TimedOut {
                    change_score: last_change_score,
                    samples,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                });
            }
            sleep(sample_interval).await;
            if token.is_cancelled() {
                return Err(CuError::Cancelled);
            }
            let cur = self.driver.quick_snapshot(display_id).await?;
            let score = cu_core::ScreenSnapshot::from(prev.clone())
                .change_score(&cur.clone().into())
                .unwrap_or(1.0);
            samples += 1;
            last_change_score = score;
            if score <= self.config.difference_threshold {
                stable_samples += 1;
            } else {
                stable_samples = 0;
            }
            if stable_samples >= self.config.required_stable_samples {
                return Ok(StabilizeOutcome::Stable {
                    change_score: score,
                    samples,
                });
            }
            prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_core::ScreenSnapshot;
    use std::sync::{Arc, Mutex};

    struct FakeStabilizeDriver {
        /// Scripted thumbnails played in a **cycle** (a continuously animating
        /// screen never settles; a single-entry script is a still screen).
        script: Mutex<std::collections::VecDeque<Vec<u8>>>,
        /// Thumbnails produced so far (counted via calls()).
        calls: Mutex<usize>,
    }

    impl FakeStabilizeDriver {
        fn new(script: Vec<Vec<u8>>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                calls: Mutex::new(0),
            }
        }
        #[allow(dead_code)]
        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl ComputerDriver for FakeStabilizeDriver {
        async fn quick_snapshot(&self, _display_id: &str) -> Result<QuickSnapshot, CuError> {
            *self.calls.lock().unwrap() += 1;
            let bytes = {
                let mut q = self.script.lock().unwrap();
                let b = q.pop_front().unwrap_or_default();
                q.push_back(b.clone()); // circular: animates forever
                b
            };
            Ok(QuickSnapshot {
                thumbnail: bytes,
                thumb_width: 4,
                thumb_height: 4,
                display_id: "1".into(),
                active_application: None,
                captured_at: chrono::Utc::now(),
            })
        }

        async fn list_displays(&self) -> Result<Vec<cu_driver::DisplayInfo>, CuError> {
            unimplemented!()
        }
        async fn desktop_layout(&self) -> Result<cu_driver::DesktopLayout, CuError> {
            unimplemented!()
        }
        async fn capture(
            &self,
            _request: cu_driver::CaptureRequest,
        ) -> Result<cu_driver::CapturedFrame, CuError> {
            unimplemented!()
        }
        async fn execute(
            &self,
            _action: &cu_driver::ResolvedAction,
        ) -> Result<cu_driver::ActionResult, CuError> {
            unimplemented!()
        }
        async fn permission_status(&self) -> Result<cu_driver::PermissionStatus, CuError> {
            unimplemented!()
        }
        async fn active_application(&self) -> Result<Option<cu_driver::ApplicationInfo>, CuError> {
            unimplemented!()
        }
        async fn pointer_location(&self) -> Result<cu_driver::PointerInfo, CuError> {
            unimplemented!()
        }
        async fn shutdown(&self) -> Result<(), CuError> {
            Ok(())
        }
    }

    fn snapshot(bytes: Vec<u8>) -> QuickSnapshot {
        QuickSnapshot {
            thumbnail: bytes,
            thumb_width: 4,
            thumb_height: 4,
            display_id: "1".into(),
            active_application: None,
            captured_at: chrono::Utc::now(),
        }
    }

    fn config() -> StabilizerConfig {
        StabilizerConfig {
            initial_delay_ms: 0,
            sample_interval_ms: 10,
            required_stable_samples: 2,
            difference_threshold: 0.05,
            max_wait_ms: 500,
        }
    }

    /// All black (identical) — diff 0 against the black baseline.
    fn black() -> Vec<u8> {
        vec![0u8; 16]
    }
    /// All white — diff ~1.0 against black.
    fn white() -> Vec<u8> {
        vec![255u8; 16]
    }

    fn score(a: &[u8], b: &[u8]) -> f64 {
        ScreenSnapshot {
            thumbnail: a.to_vec(),
            thumb_width: 4,
            thumb_height: 4,
            active_application: None,
            active_window_title: None,
            display_id: "1".into(),
            captured_at: chrono::Utc::now(),
        }
        .change_score(&ScreenSnapshot {
            thumbnail: b.to_vec(),
            thumb_width: 4,
            thumb_height: 4,
            active_application: None,
            active_window_title: None,
            display_id: "1".into(),
            captured_at: chrono::Utc::now(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn fully_still_screen_detects_stable() {
        // Every sample identical to the baseline: diff 0 < threshold → stable
        // after `required_stable_samples` consecutive quiet samples.
        let driver = Arc::new(FakeStabilizeDriver::new(vec![black(), black(), black()]));
        let s = Stabilizer::new(driver.as_ref(), config());
        let token = CancellationToken::new();
        let outcome = s
            .until_stable("1", &snapshot(black()), &token)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            StabilizeOutcome::Stable {
                change_score: 0.0,
                samples: 2
            },
            "two identical samples must be declared stable"
        );
    }

    #[tokio::test]
    async fn ever_changing_screen_times_out_with_last_real_score() {
        // Baseline black; the screen alternates black↔white forever — every
        // diff is 1.0 → timeout. The reported score must be the *real*
        // measured 1.0, not 0.0.
        let driver = Arc::new(FakeStabilizeDriver::new(vec![white(), black()]));
        let s = Stabilizer::new(driver.as_ref(), config());
        let token = CancellationToken::new();
        let outcome = s
            .until_stable("1", &snapshot(black()), &token)
            .await
            .unwrap();
        match outcome {
            StabilizeOutcome::TimedOut {
                change_score,
                samples,
                elapsed_ms,
            } => {
                assert!(
                    change_score > 0.9,
                    "timeout must report the last real score (got {change_score})"
                );
                assert!(samples >= 1, "at least one comparison must have run");
                assert!(elapsed_ms < 2000, "timeout should respect max_wait");
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_score_reflects_near_stable_screen() {
        // Baseline black; the screen cycles gray(100) ↔ white forever — every
        // diff is 155/255 ≈ 0.61, above the 0.05 threshold, so it never
        // settles. The reported timeout score must be the real 0.61, not 0.
        let driver = Arc::new(FakeStabilizeDriver::new(vec![
            vec![100; 16], // diff vs black ≈ 0.39
            white(),       // diff vs gray ≈ 0.61, then 0.61 forever
        ]));
        let s = Stabilizer::new(driver.as_ref(), config());
        let token = CancellationToken::new();
        let outcome = s
            .until_stable("1", &snapshot(black()), &token)
            .await
            .unwrap();
        match outcome {
            StabilizeOutcome::TimedOut { change_score, .. } => {
                assert!(
                    (0.5..0.75).contains(&change_score),
                    "last measured diff was ~0.61, got {change_score}"
                );
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_score_is_never_always_zero() {
        // A noisy cycle that never settles: consecutive frames differ by
        // 50/255 ≈ 0.196 > 0.05 each step. The reported score must be the
        // last real measured diff — never a hardcoded 0.
        let script: Vec<Vec<u8>> = [10u8, 60, 110, 160, 210, 255]
            .iter()
            .map(|&v| vec![v; 16])
            .collect();
        let driver = Arc::new(FakeStabilizeDriver::new(script));
        let s = Stabilizer::new(driver.as_ref(), config());
        let token = CancellationToken::new();
        let outcome = s
            .until_stable("1", &snapshot(black()), &token)
            .await
            .unwrap();
        match outcome {
            StabilizeOutcome::TimedOut {
                change_score,
                samples,
                ..
            } => {
                assert!(samples > 1, "should have sampled repeatedly");
                assert!(
                    change_score > 0.1,
                    "every consecutive diff is ~0.196, got {change_score}"
                );
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cursor_only_motion_not_confused_for_stability() {
        // A cursor-sized perturbation: one pixel brightened to 200 in a 4x4
        // thumbnail (200/16/255 ≈ 0.049) sits just below the 0.05 threshold.
        // It must not push the screen into "forever changing" territory —
        // an identical wobble frame is declared stable.
        let base = black();
        let mut wobble = black();
        wobble[0] = 200; // ≈ 0.049 < threshold
        assert!(
            score(&base, &wobble) < 0.05,
            "cursor-sized perturbation must be below the threshold (got {})",
            score(&base, &wobble)
        );
        let driver = Arc::new(FakeStabilizeDriver::new(vec![wobble]));
        let s = Stabilizer::new(driver.as_ref(), config());
        let token = CancellationToken::new();
        let outcome = s.until_stable("1", &snapshot(base), &token).await.unwrap();
        assert!(
            matches!(outcome, StabilizeOutcome::Stable { .. }),
            "wobble below threshold counts as quiet — got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_immediately() {
        // max_wait 10s but the token fires after ~30ms: the wait must return
        // CANCELLED almost immediately and stop sampling.
        let mut cfg = config();
        cfg.max_wait_ms = 10_000;
        cfg.sample_interval_ms = 1_000;
        let driver = Arc::new(FakeStabilizeDriver::new(vec![black()]));
        let token = CancellationToken::new();
        let token_in_task = token.clone();
        let handle = tokio::spawn(async move {
            let s = Stabilizer::new(driver.as_ref(), cfg);
            let started = Instant::now();
            let r = s
                .until_stable("1", &snapshot(black()), &token_in_task)
                .await;
            (r, started.elapsed())
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        token.cancel();
        let (result, elapsed) = handle.await.unwrap();
        assert!(
            matches!(result, Err(CuError::Cancelled)),
            "cancelled wait must return Cancelled, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "cancellation must abort the wait immediately (elapsed {elapsed:?})"
        );
    }

    #[test]
    fn config_defaults_are_sane() {
        let c = StabilizerConfig::default();
        assert!(c.initial_delay_ms <= 1000);
        assert!(c.required_stable_samples >= 1);
        assert!(c.difference_threshold > 0.0 && c.difference_threshold < 0.5);
    }
}
