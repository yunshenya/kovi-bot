use serde::{Deserialize, Serialize};

/// World Model v4 runtime config (plugins/model side).
///
/// Follows the blueprint's feature-flag shape (v4 §215/§216). The flag that carries
/// the risk is [`Self::enabled`] — it is the master switch:
///
/// - `enabled = false`（默认）：整个世界模型不运行。不建持久化表、不恢复、不记录任何
///   观察（所有记录都走 `with_world`，它第一件事就是看这个开关）。
/// - `enabled = true`：开始记录观察/情境/场景。**行为仍然不变**，因为会改变聊天行为的
///   是 [`Self::reply_context`] 与 [`Self::influence_mode`]，两者默认都是 `"disabled"`。
///
/// 注意 [`Self::shadow_mode`] **不门控任何行为**：全仓只有两个消费点（启动日志与
/// `#world-status`），都只是往状态行里拼一个 `shadow=true` 字样。别把"影子模式 = 安全"
/// 的保证寄托在它身上——真正拦住行为的是上面那两个 `*_mode`。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct WorldModelConfig {
    /// 总开关，见类型文档：false 时记录、持久化、恢复、数据删除联动全都不运行。
    enabled: bool,
    /// 仅影响两处状态**文案**（启动日志、`#world-status`），不门控任何行为。
    shadow_mode: bool,
    /// Persist the in-memory World Model to Postgres (restart recovery,
    /// v4 §130). Requires a configured database; ignored when `enabled=false`.
    persist: bool,
    /// Persistence write interval (seconds).
    persist_interval_secs: u64,
    /// Reply-context injection: "disabled" (default, no effect), "shadow"
    /// (log what would be injected), or "active" (inject bounded world
    /// context into the reply). Active only after shadow review (v4 §217).
    reply_context: String,
    /// Behavioral influence: "disabled" (default) or "active". When active,
    /// the world's interruption cost can suppress unaddressed group speech
    /// (v4 §103, §197). Keep shadow-observed before enabling.
    influence_mode: String,
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
            reply_context: "disabled".to_owned(),
            influence_mode: "disabled".to_owned(),
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
        anyhow::ensure!(
            matches!(
                self.reply_context.as_str(),
                "disabled" | "shadow" | "active"
            ),
            "world_model.reply_context 必须是 disabled / shadow / active"
        );
        anyhow::ensure!(
            matches!(self.influence_mode.as_str(), "disabled" | "active"),
            "world_model.influence_mode 必须是 disabled / active"
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

    /// disabled / shadow / active.
    pub fn reply_context(&self) -> &str {
        &self.reply_context
    }

    pub fn reply_context_active(&self) -> bool {
        self.reply_context == "active"
    }

    pub fn reply_context_shadow(&self) -> bool {
        self.reply_context == "shadow"
    }

    /// Behavioral influence is active (v4 §103/§197 gating).
    pub fn influence_active(&self) -> bool {
        self.influence_mode == "active"
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
