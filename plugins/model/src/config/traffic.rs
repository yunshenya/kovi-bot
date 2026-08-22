//! 入站流量、排队和响应资源边界。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct TrafficConfig {
    enabled: bool,
    window_secs: u64,
    per_user_limit: usize,
    global_limit: usize,
    cooldown_secs: u64,
    max_pending_turns: usize,
    max_input_chars: usize,
    max_model_response_bytes: usize,
    max_model_queue: usize,
    model_queue_timeout_secs: u64,
}

impl TrafficConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn window_secs(&self) -> u64 {
        self.window_secs
    }

    pub fn per_user_limit(&self) -> usize {
        self.per_user_limit
    }

    pub fn global_limit(&self) -> usize {
        self.global_limit
    }

    pub fn cooldown_secs(&self) -> u64 {
        self.cooldown_secs
    }

    pub fn max_pending_turns(&self) -> usize {
        self.max_pending_turns
    }

    pub fn max_input_chars(&self) -> usize {
        self.max_input_chars
    }

    pub fn max_model_response_bytes(&self) -> usize {
        self.max_model_response_bytes
    }

    pub fn max_model_queue(&self) -> usize {
        self.max_model_queue
    }

    pub fn model_queue_timeout_secs(&self) -> u64 {
        self.model_queue_timeout_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.window_secs == 0 || self.cooldown_secs == 0 {
            return Err(anyhow::anyhow!("流量限制窗口和冷却时间必须大于 0"));
        }
        if self.per_user_limit == 0 || self.global_limit < self.per_user_limit {
            return Err(anyhow::anyhow!(
                "traffic.global_limit 必须不小于 traffic.per_user_limit，且都大于 0"
            ));
        }
        if !(1..=128).contains(&self.max_pending_turns) {
            return Err(anyhow::anyhow!(
                "traffic.max_pending_turns 必须在 1 到 128 之间"
            ));
        }
        if !(256..=32_000).contains(&self.max_input_chars) {
            return Err(anyhow::anyhow!(
                "traffic.max_input_chars 必须在 256 到 32000 之间"
            ));
        }
        if !(64 * 1024..=16 * 1024 * 1024).contains(&self.max_model_response_bytes) {
            return Err(anyhow::anyhow!(
                "traffic.max_model_response_bytes 必须在 64 KiB 到 16 MiB 之间"
            ));
        }
        if !(4..=1_024).contains(&self.max_model_queue) || self.model_queue_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "traffic.max_model_queue 必须在 4 到 1024 之间，队列超时必须大于 0"
            ));
        }
        Ok(())
    }
}

impl Default for TrafficConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_secs: 60,
            per_user_limit: 20,
            global_limit: 300,
            cooldown_secs: 120,
            max_pending_turns: 16,
            max_input_chars: 6_000,
            max_model_response_bytes: 2 * 1024 * 1024,
            max_model_queue: 64,
            model_queue_timeout_secs: 15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TrafficConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(TrafficConfig::default().validate().is_ok());
    }
}
