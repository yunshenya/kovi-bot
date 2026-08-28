//! Resource limits for the embedded Intrinsic runtime.

use super::super::media::ModelMediaLimits;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrinsicRuntimeConfig {
    pub enabled: bool,
    pub max_parallel: usize,
    pub max_context_tokens: usize,
    pub max_new_tokens: usize,
    pub queue_timeout_ms: u64,
    pub media: ModelMediaLimits,
    pub startup_self_test: bool,
}

impl Default for IntrinsicRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_parallel: 1,
            max_context_tokens: 2_048,
            max_new_tokens: 256,
            queue_timeout_ms: 15_000,
            media: ModelMediaLimits::default(),
            startup_self_test: true,
        }
    }
}

impl IntrinsicRuntimeConfig {
    pub fn validate(self) -> Result<(), &'static str> {
        if !(1..=8).contains(&self.max_parallel) {
            return Err("max_parallel must be within 1..=8");
        }
        if !(1..=32_768).contains(&self.max_context_tokens) {
            return Err("max_context_tokens must be within 1..=32768");
        }
        if !(1..=4_096).contains(&self.max_new_tokens) {
            return Err("max_new_tokens must be within 1..=4096");
        }
        if self.queue_timeout_ms == 0 {
            return Err("queue_timeout_ms must be greater than zero");
        }
        self.media.validate().map_err(|_| "invalid media limits")?;
        Ok(())
    }

    #[must_use]
    pub const fn queue_timeout(self) -> Duration {
        Duration::from_millis(self.queue_timeout_ms)
    }
}
