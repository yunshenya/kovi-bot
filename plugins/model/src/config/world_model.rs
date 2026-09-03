use serde::{Deserialize, Serialize};

/// World Model v4 runtime config (plugins/model side).
///
/// Follows the blueprint's feature-flag shape (v4 §215/§216): the runtime is
/// `enabled = false` by default so a fresh deploy never changes behavior;
/// once enabled it still runs in `shadow_mode = true` (records observations,
/// scenes, and metrics only; nothing in the runtime may block or alter chat
/// replies until it is explicitly switched to active by the operator).
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct WorldModelConfig {
    enabled: bool,
    shadow_mode: bool,
    /// Persist the in-memory World Model to Postgres (restart recovery,
    /// v4 §130). Requires a configured database; ignored when `enabled=false`.
    persist: bool,
    /// Persistence write interval (seconds).
    persist_interval_secs: u64,
    /// TTL (seconds) applied to observations derived from chat world facts.
    observation_ttl_secs: u64,
    /// Maximum distinct conversations with a live social scene.
    max_social_scenes: usize,
    /// Group activity window used for the social scene bump (seconds).
    activity_window_secs: u64,
}

impl Default for WorldModelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            shadow_mode: true,
            persist: true,
            persist_interval_secs: 30,
            observation_ttl_secs: 60 * 60 * 24,
            max_social_scenes: 256,
            activity_window_secs: 60,
        }
    }
}

impl WorldModelConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (60..=31_536_000).contains(&self.observation_ttl_secs),
            "world_model.observation_ttl_secs 必须在 60..=31536000"
        );
        anyhow::ensure!(
            self.max_social_scenes >= 1 && self.max_social_scenes <= 4096,
            "world_model.max_social_scenes 必须在 1..=4096"
        );
        anyhow::ensure!(
            self.activity_window_secs >= 10 && self.activity_window_secs <= 600,
            "world_model.activity_window_secs 必须在 10..=600"
        );
        anyhow::ensure!(
            self.persist_interval_secs >= 10 && self.persist_interval_secs <= 3600,
            "world_model.persist_interval_secs 必须在 10..=3600"
        );
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn shadow_mode(&self) -> bool {
        self.shadow_mode
    }

    pub fn persist(&self) -> bool {
        self.persist
    }

    pub fn persist_interval_secs(&self) -> u64 {
        self.persist_interval_secs
    }

    pub fn observation_ttl_secs(&self) -> u64 {
        self.observation_ttl_secs
    }

    pub fn max_social_scenes(&self) -> usize {
        self.max_social_scenes
    }

    pub fn activity_window_secs(&self) -> u64 {
        self.activity_window_secs
    }
}
