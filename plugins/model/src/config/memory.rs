use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct MemoryConfig {
    max_entries: usize,
    retention_days: i64,
    max_conversation_messages: usize,
    contextual_memory_limit: usize,
    maintenance_interval_secs: u64,
    summary_keep_recent_messages: usize,
    summary_max_chars: usize,
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

    pub fn maintenance_interval_secs(&self) -> u64 {
        self.maintenance_interval_secs
    }

    pub fn summary_keep_recent_messages(&self) -> usize {
        self.summary_keep_recent_messages
    }

    pub fn summary_max_chars(&self) -> usize {
        self.summary_max_chars
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
        Ok(())
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            retention_days: 30,
            max_conversation_messages: 25,
            contextual_memory_limit: 5,
            maintenance_interval_secs: 86_400,
            summary_keep_recent_messages: 15,
            summary_max_chars: 1_500,
        }
    }
}
