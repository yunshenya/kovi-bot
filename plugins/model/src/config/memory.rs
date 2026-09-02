use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct MemoryConfig {
    max_entries: usize,
    retention_days: i64,
    /// Episode records have a longer, independent retention window than the
    /// short-lived V1 memory cache.
    episode_retention_days: i64,
    /// Hard maximum for known Episode statuses in one Mind scope. Protected
    /// known records are ranked first; unknown statuses are retained fail-closed
    /// and do not participate in eviction.
    episode_max_per_scope: usize,
    /// Salience at or above this value protects an episode from maintenance
    /// cleanup. The same threshold also protects strongly emotional episodes.
    episode_protected_salience: f32,
    profile_ttl_days: i64,
    summary_ttl_days: i64,
    sticker_ttl_days: i64,
    data_minimization: bool,
    runtime_history_ttl_secs: u64,
    max_conversation_messages: usize,
    max_conversation_tokens: usize,
    contextual_memory_limit: usize,
    maintenance_interval_secs: u64,
    summary_keep_recent_messages: usize,
    summary_max_chars: usize,
    /// 是否允许模型在上下文不足时自主检索当前会话范围内的长期记忆。
    autonomous_query_enabled: bool,
    /// 单次回复最多允许执行多少轮自主记忆查询。
    autonomous_query_max_rounds: u8,
    /// 每轮自主查询最多返回多少条记忆。
    autonomous_query_max_results: usize,
    /// 自主查询允许回看的最长天数。
    autonomous_query_max_days: u32,
}

impl MemoryConfig {
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub fn retention_days(&self) -> i64 {
        self.retention_days
    }

    pub fn episode_retention_days(&self) -> i64 {
        self.episode_retention_days
    }

    pub fn episode_max_per_scope(&self) -> usize {
        self.episode_max_per_scope
    }

    pub fn episode_protected_salience(&self) -> f32 {
        self.episode_protected_salience
    }

    pub fn profile_ttl_days(&self) -> i64 {
        self.profile_ttl_days
    }

    pub fn summary_ttl_days(&self) -> i64 {
        self.summary_ttl_days
    }

    pub fn sticker_ttl_days(&self) -> i64 {
        self.sticker_ttl_days
    }

    pub fn data_minimization(&self) -> bool {
        self.data_minimization
    }

    pub fn runtime_history_ttl_secs(&self) -> u64 {
        self.runtime_history_ttl_secs
    }

    pub fn max_conversation_messages(&self) -> usize {
        self.max_conversation_messages
    }

    pub fn contextual_memory_limit(&self) -> usize {
        self.contextual_memory_limit
    }

    pub fn max_conversation_tokens(&self) -> usize {
        self.max_conversation_tokens
    }

    pub fn maintenance_interval_secs(&self) -> u64 {
        self.maintenance_interval_secs
    }

    pub fn summary_keep_recent_messages(&self) -> usize {
        self.summary_keep_recent_messages
    }

    pub fn summary_max_chars(&self) -> usize {
        self.summary_max_chars
    }

    pub fn autonomous_query_enabled(&self) -> bool {
        self.autonomous_query_enabled
    }

    pub fn autonomous_query_max_rounds(&self) -> u8 {
        self.autonomous_query_max_rounds
    }

    pub fn autonomous_query_max_results(&self) -> usize {
        self.autonomous_query_max_results
    }

    pub fn autonomous_query_max_days(&self) -> u32 {
        self.autonomous_query_max_days
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_entries == 0 {
            return Err(anyhow::anyhow!("memory.max_entries 必须大于 0"));
        }
        if self.retention_days <= 0 {
            return Err(anyhow::anyhow!("memory.retention_days 必须大于 0"));
        }
        if !(1..=3_650).contains(&self.episode_retention_days) {
            return Err(anyhow::anyhow!(
                "memory.episode_retention_days 必须在 1 到 3650 天之间"
            ));
        }
        if !(1..=4_096).contains(&self.episode_max_per_scope) {
            return Err(anyhow::anyhow!(
                "memory.episode_max_per_scope 必须在 1 到 4096 之间"
            ));
        }
        if !self.episode_protected_salience.is_finite()
            || !(0.0..=1.0).contains(&self.episode_protected_salience)
        {
            return Err(anyhow::anyhow!(
                "memory.episode_protected_salience 必须在 0 到 1 之间"
            ));
        }
        if self.profile_ttl_days <= 0 {
            return Err(anyhow::anyhow!("memory.profile_ttl_days 必须大于 0"));
        }
        if self.summary_ttl_days <= 0 {
            return Err(anyhow::anyhow!("memory.summary_ttl_days 必须大于 0"));
        }
        if self.sticker_ttl_days <= 0 {
            return Err(anyhow::anyhow!("memory.sticker_ttl_days 必须大于 0"));
        }
        if self.runtime_history_ttl_secs == 0 {
            return Err(anyhow::anyhow!(
                "memory.runtime_history_ttl_secs 必须大于 0"
            ));
        }
        if self.max_conversation_messages < 3 {
            return Err(anyhow::anyhow!(
                "memory.max_conversation_messages 不能小于 3"
            ));
        }
        if self.max_conversation_tokens < 512 {
            return Err(anyhow::anyhow!(
                "memory.max_conversation_tokens 不能小于 512"
            ));
        }
        if self.contextual_memory_limit == 0 {
            return Err(anyhow::anyhow!("memory.contextual_memory_limit 必须大于 0"));
        }
        if self.maintenance_interval_secs == 0 {
            return Err(anyhow::anyhow!(
                "memory.maintenance_interval_secs 必须大于 0"
            ));
        }
        if self.summary_keep_recent_messages == 0
            || self.summary_keep_recent_messages.saturating_add(2) > self.max_conversation_messages
        {
            return Err(anyhow::anyhow!(
                "memory.summary_keep_recent_messages 必须大于0，且至少为下一轮用户和机器人回复预留两个位置"
            ));
        }
        if self.summary_max_chars < 100 {
            return Err(anyhow::anyhow!("memory.summary_max_chars 不能小于 100"));
        }
        if self.autonomous_query_max_rounds == 0 || self.autonomous_query_max_rounds > 3 {
            return Err(anyhow::anyhow!(
                "memory.autonomous_query_max_rounds 必须在 1 到 3 之间"
            ));
        }
        if self.autonomous_query_max_results == 0 || self.autonomous_query_max_results > 20 {
            return Err(anyhow::anyhow!(
                "memory.autonomous_query_max_results 必须在 1 到 20 之间"
            ));
        }
        if self.autonomous_query_max_days == 0 {
            return Err(anyhow::anyhow!(
                "memory.autonomous_query_max_days 必须大于 0"
            ));
        }
        Ok(())
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            retention_days: 30,
            episode_retention_days: 365,
            episode_max_per_scope: 128,
            episode_protected_salience: 0.7,
            profile_ttl_days: 90,
            summary_ttl_days: 30,
            sticker_ttl_days: 90,
            data_minimization: true,
            runtime_history_ttl_secs: 3_600,
            max_conversation_messages: 25,
            max_conversation_tokens: 6_000,
            contextual_memory_limit: 5,
            maintenance_interval_secs: 86_400,
            summary_keep_recent_messages: 15,
            summary_max_chars: 1_500,
            autonomous_query_enabled: true,
            autonomous_query_max_rounds: 2,
            autonomous_query_max_results: 8,
            autonomous_query_max_days: 3_650,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryConfig;

    #[test]
    fn episode_retention_defaults_are_longer_and_bounded() {
        let config = MemoryConfig::default();
        assert_eq!(config.episode_retention_days(), 365);
        assert_eq!(config.episode_max_per_scope(), 128);
        assert!((config.episode_protected_salience() - 0.7).abs() < f32::EPSILON);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn episode_retention_rejects_invalid_limits() {
        let config = MemoryConfig {
            episode_retention_days: 0,
            ..MemoryConfig::default()
        };
        assert!(config.validate().is_err());

        let config = MemoryConfig {
            episode_max_per_scope: 0,
            ..MemoryConfig::default()
        };
        assert!(config.validate().is_err());

        let config = MemoryConfig {
            episode_protected_salience: f32::NAN,
            ..MemoryConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn older_memory_configuration_uses_episode_defaults() {
        let config: MemoryConfig = kovi::toml::from_str(
            "max_entries = 1000\nretention_days = 30\nprofile_ttl_days = 90\nsummary_ttl_days = 30\nsticker_ttl_days = 90\nruntime_history_ttl_secs = 3600\nmax_conversation_messages = 25\nmax_conversation_tokens = 6000\ncontextual_memory_limit = 5\nmaintenance_interval_secs = 86400\nsummary_keep_recent_messages = 15\nsummary_max_chars = 1500\nautonomous_query_enabled = true\nautonomous_query_max_rounds = 2\nautonomous_query_max_results = 8\nautonomous_query_max_days = 3650\n",
        )
        .expect("older memory configuration should remain compatible");
        assert_eq!(config.episode_retention_days(), 365);
        assert_eq!(config.episode_max_per_scope(), 128);
        assert!((config.episode_protected_salience() - 0.7).abs() < f32::EPSILON);
    }
}
