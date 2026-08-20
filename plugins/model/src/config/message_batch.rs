//! 连续消息气泡的本地合并配置。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct MessageBatchConfig {
    enabled: bool,
    complete_delay_ms: u64,
    normal_delay_ms: u64,
    incomplete_delay_ms: u64,
    max_wait_ms: u64,
    max_parts: usize,
    max_chars: usize,
}

impl MessageBatchConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn complete_delay_ms(&self) -> u64 {
        self.complete_delay_ms
    }

    pub fn normal_delay_ms(&self) -> u64 {
        self.normal_delay_ms
    }

    pub fn incomplete_delay_ms(&self) -> u64 {
        self.incomplete_delay_ms
    }

    pub fn max_wait_ms(&self) -> u64 {
        self.max_wait_ms
    }

    pub fn max_parts(&self) -> usize {
        self.max_parts
    }

    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.complete_delay_ms == 0
            || self.normal_delay_ms == 0
            || self.incomplete_delay_ms == 0
            || self.max_wait_ms == 0
        {
            return Err(anyhow::anyhow!("连续消息合并等待时间必须大于 0"));
        }
        if self.complete_delay_ms > self.normal_delay_ms
            || self.normal_delay_ms > self.incomplete_delay_ms
            || self.incomplete_delay_ms > self.max_wait_ms
        {
            return Err(anyhow::anyhow!(
                "连续消息合并等待时间必须满足 complete <= normal <= incomplete <= max_wait"
            ));
        }
        if !(2..=20).contains(&self.max_parts) {
            return Err(anyhow::anyhow!(
                "message_batch.max_parts 必须在 2 到 20 之间"
            ));
        }
        if !(50..=5_000).contains(&self.max_chars) {
            return Err(anyhow::anyhow!(
                "message_batch.max_chars 必须在 50 到 5000 之间"
            ));
        }
        Ok(())
    }
}

impl Default for MessageBatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            complete_delay_ms: 900,
            normal_delay_ms: 1_600,
            incomplete_delay_ms: 2_300,
            max_wait_ms: 5_000,
            max_parts: 6,
            max_chars: 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MessageBatchConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(MessageBatchConfig::default().validate().is_ok());
    }
}
