use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use yunxi_core::{IntrinsicRuntimeConfig, ModelFallbackPolicy, ModelMediaLimits};

#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct CognitiveModelConfig {
    intrinsic: IntrinsicConfig,
    fallback: ModelFallbackConfig,
    turn_gate: TurnGateConfig,
}

/// TurnGate 完成度分类器 (Phase 2 接线, doc §8.2/§9):
/// - mode = "active":TurnGate 优先决定 flush/hold; abstain 或无 bundle 时
///   回退现有 lexical + MiniMind 路径;
/// - mode = "shadow":仅记录 TurnGate 会怎么说与现有路径的分歧,不改变路由;
/// - mode = "disabled":完全不参与。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct TurnGateConfig {
    enabled: bool,
    /// "disabled" | "shadow" | "active"。
    mode: String,
    /// bundle 目录 (manifest.toml + turn_gate.bin),相对 WorkingDirectory。
    asset_dir: String,
}

impl Default for TurnGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: "active".to_owned(),
            asset_dir: "models/yunxi-turngate".to_owned(),
        }
    }
}

impl TurnGateConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn mode(&self) -> &str {
        self.mode.as_str()
    }

    pub fn asset_dir(&self) -> &str {
        self.asset_dir.as_str()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.mode.as_str(), "disabled" | "shadow" | "active"),
            "model.turn_gate.mode 必须是 disabled / shadow / active"
        );
        anyhow::ensure!(
            !self.asset_dir.trim().is_empty(),
            "model.turn_gate.asset_dir 不能为空"
        );
        Ok(())
    }
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
        self.turn_gate.validate()?;
        Ok(())
    }

    #[must_use]
    pub const fn turn_gate(&self) -> &TurnGateConfig {
        &self.turn_gate
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

#[cfg(test)]
mod tests {
    use super::TurnGateConfig;

    #[test]
    fn turn_gate_defaults_are_valid() {
        assert!(TurnGateConfig::default().validate().is_ok());
    }

    #[test]
    fn unknown_turn_gate_mode_is_rejected() {
        let config = TurnGateConfig {
            mode: "chaotic".to_owned(),
            ..TurnGateConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
