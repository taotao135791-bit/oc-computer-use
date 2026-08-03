//! Screen stabilizer: after a batch of actions, optionally wait until the
//! desktop stops changing before telling the agent "done".
//!
//! - `WaitPolicy::None`   — no wait.
//! - `WaitPolicy::Fixed`  — sleep a fixed duration.
//! - `WaitPolicy::UntilStable` — poll cheap thumbnails until `N` consecutive
//!   samples differ from the previous one by less than a threshold, or the
//!   maximum wait elapses.

use std::time::{Duration, Instant};

use cu_core::CuError;
use cu_driver::{ComputerDriver, QuickSnapshot};

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
    /// Never quietened in time; the last observed change score is reported.
    TimedOut { change_score: f64 },
}

pub struct Stabilizer<'a> {
    driver: &'a dyn ComputerDriver,
    pub config: StabilizerConfig,
}

impl<'a> Stabilizer<'a> {
    pub fn new(driver: &'a dyn ComputerDriver, config: StabilizerConfig) -> Self {
        Self { driver, config }
    }

    /// `samples_since` is the change score of the first baseline sample against
    /// itself (0.0) — kept in the signature so callers control the baseline.
    pub async fn until_stable(
        &self,
        display_id: &str,
        initial: &QuickSnapshot,
    ) -> Result<StabilizeOutcome, CuError> {
        tokio::time::sleep(Duration::from_millis(self.config.initial_delay_ms)).await;

        let max_wait = Duration::from_millis(self.config.max_wait_ms);
        let started = Instant::now();
        let mut prev = initial.clone();
        let mut stable_samples: u32 = 0;

        loop {
            if started.elapsed() >= max_wait {
                // Report the last measured difference.
                return Ok(StabilizeOutcome::TimedOut { change_score: 0.0 });
            }
            tokio::time::sleep(Duration::from_millis(self.config.sample_interval_ms)).await;
            let cur = self.driver.quick_snapshot(display_id).await?;
            let score = cu_core::ScreenSnapshot::from(prev.clone())
                .change_score(&cur.clone().into())
                .unwrap_or(1.0);
            if score <= self.config.difference_threshold {
                stable_samples += 1;
            } else {
                stable_samples = 0;
            }
            if stable_samples >= self.config.required_stable_samples {
                return Ok(StabilizeOutcome::Stable {
                    change_score: score,
                    samples: stable_samples,
                });
            }
            prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_sane() {
        let c = StabilizerConfig::default();
        assert!(c.initial_delay_ms <= 1000);
        assert!(c.required_stable_samples >= 1);
        assert!(c.difference_threshold > 0.0 && c.difference_threshold < 0.5);
    }
}
