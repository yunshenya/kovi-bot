use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use yunxi_core::{IntrinsicRuntimeConfig, ModelFallbackPolicy, ModelMediaLimits};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct CognitiveModelConfig {
    intrinsic: IntrinsicConfig,
    fallback: ModelFallbackConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct IntrinsicConfig {
    enabled: bool,
    shadow_routing: bool,
    asset_dir: String,
    max_parallel: usize,
    max_context_tokens: usize,
    max_new_tokens: usize,
    max_images_per_turn: usize,
    max_image_bytes: usize,
    max_image_pixels: u64,
    queue_timeout_ms: u64,
    startup_self_test: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ModelFallbackConfig {
    strong_to_intrinsic: bool,
    max_model_attempts: u8,
}

impl Default for CognitiveModelConfig {
    fn default() -> Self {
        Self {
            intrinsic: IntrinsicConfig::default(),
            fallback: ModelFallbackConfig::default(),
        }
    }
}

impl Default for IntrinsicConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            shadow_routing: true,
            asset_dir: "models/yunxi-intrinsic/minimind-3o".to_owned(),
            max_parallel: 1,
            max_context_tokens: 2_048,
            max_new_tokens: 256,
            max_images_per_turn: 1,
            max_image_bytes: 8 * 1_024 * 1_024,
            max_image_pixels: 4_000_000,
            queue_timeout_ms: 15_000,
            startup_self_test: true,
        }
    }
}

impl Default for ModelFallbackConfig {
    fn default() -> Self {
        Self {
            strong_to_intrinsic: true,
            max_model_attempts: 2,
        }
    }
}

impl CognitiveModelConfig {
    pub fn validate(&self) -> Result<()> {
        self.intrinsic.validate()?;
        self.fallback.validate()?;
        Ok(())
    }

    #[must_use]
    pub const fn intrinsic(&self) -> &IntrinsicConfig {
        &self.intrinsic
    }

    #[must_use]
    pub const fn fallback(&self) -> &ModelFallbackConfig {
        &self.fallback
    }
}

impl IntrinsicConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.asset_dir.is_empty()
                && self.asset_dir.len() <= 512
                && !std::path::Path::new(&self.asset_dir).is_absolute()
                && !self
                    .asset_dir
                    .split('/')
                    .any(|part| part == ".." || part.is_empty())
                && !self.asset_dir.contains('\\')
                && !self.asset_dir.chars().any(char::is_control),
            "model.intrinsic.asset_dir 必须是安全相对路径"
        );
        self.runtime_config().validate().map_err(anyhow::Error::msg)
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn shadow_routing(&self) -> bool {
        self.shadow_routing
    }

    #[must_use]
    pub fn asset_dir(&self) -> &str {
        &self.asset_dir
    }

    #[must_use]
    pub const fn runtime_config(&self) -> IntrinsicRuntimeConfig {
        IntrinsicRuntimeConfig {
            enabled: self.enabled,
            max_parallel: self.max_parallel,
            max_context_tokens: self.max_context_tokens,
            max_new_tokens: self.max_new_tokens,
            queue_timeout_ms: self.queue_timeout_ms,
            media: ModelMediaLimits {
                max_bytes: self.max_image_bytes,
                max_pixels: self.max_image_pixels,
                max_images_per_turn: self.max_images_per_turn,
            },
            startup_self_test: self.startup_self_test,
        }
    }
}

impl ModelFallbackConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=2).contains(&self.max_model_attempts),
            "model.fallback.max_model_attempts 必须在 1..=2"
        );
        Ok(())
    }

    #[must_use]
    pub const fn policy(&self) -> ModelFallbackPolicy {
        ModelFallbackPolicy {
            strong_to_intrinsic: self.strong_to_intrinsic,
            max_model_attempts: self.max_model_attempts,
        }
    }

    #[must_use]
    pub const fn strong_to_intrinsic(&self) -> bool {
        self.strong_to_intrinsic
    }
}
