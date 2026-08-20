use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct MemoryConfig {
    max_entries: usize,
    retention_days: i64,
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
