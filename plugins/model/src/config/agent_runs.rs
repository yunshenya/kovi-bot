use serde::{Deserialize, Serialize};

/// 通用持久化 Agent Run 的资源与恢复边界。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct AgentRunConfig {
    enabled: bool,
    recovery_scan_secs: u64,
    lease_secs: u64,
    request_timeout_secs: u64,
    min_interval_secs: u64,
    max_interval_secs: u64,
    default_interval_secs: u64,
    default_stop_after_minutes: u64,
    max_stop_after_minutes: u64,
    default_max_executions: u32,
    max_executions_per_run: u32,
    max_active_per_user: usize,
    max_active_total: usize,
    max_consecutive_failures: u32,
    max_response_bytes: usize,
    max_body_preview_chars: usize,
    max_notification_chars: usize,
    claim_batch_size: usize,
}

impl AgentRunConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn recovery_scan_secs(&self) -> u64 {
        self.recovery_scan_secs
    }

    pub fn lease_secs(&self) -> u64 {
        self.lease_secs
    }

    pub fn request_timeout_secs(&self) -> u64 {
        self.request_timeout_secs
    }

    pub fn min_interval_secs(&self) -> u64 {
        self.min_interval_secs
    }

    pub fn max_interval_secs(&self) -> u64 {
        self.max_interval_secs
    }

    pub fn default_interval_secs(&self) -> u64 {
        self.default_interval_secs
    }

    pub fn default_stop_after_minutes(&self) -> u64 {
        self.default_stop_after_minutes
    }

    pub fn max_stop_after_minutes(&self) -> u64 {
        self.max_stop_after_minutes
    }

    pub fn default_max_executions(&self) -> u32 {
        self.default_max_executions
    }

    pub fn max_executions_per_run(&self) -> u32 {
        self.max_executions_per_run
    }

    pub fn max_active_per_user(&self) -> usize {
        self.max_active_per_user
    }

    pub fn max_active_total(&self) -> usize {
        self.max_active_total
    }

    pub fn max_consecutive_failures(&self) -> u32 {
        self.max_consecutive_failures
    }

    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    pub fn max_body_preview_chars(&self) -> usize {
        self.max_body_preview_chars
    }

    pub fn max_notification_chars(&self) -> usize {
        self.max_notification_chars
    }

    pub fn claim_batch_size(&self) -> usize {
        self.claim_batch_size
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.recovery_scan_secs == 0 || self.recovery_scan_secs > 300 {
            return Err(anyhow::anyhow!(
                "agent_runs.recovery_scan_secs 必须在 1 到 300 秒之间"
            ));
        }
        if self.request_timeout_secs < 2 || self.request_timeout_secs > 60 {
            return Err(anyhow::anyhow!(
                "agent_runs.request_timeout_secs 必须在 2 到 60 秒之间"
            ));
        }
        if self.lease_secs < self.request_timeout_secs.saturating_add(20) || self.lease_secs > 600 {
            return Err(anyhow::anyhow!(
                "agent_runs.lease_secs 必须至少比 request_timeout_secs 多 20 秒，且不能超过 600 秒"
            ));
        }
        if self.min_interval_secs < 5 || self.min_interval_secs > 3_600 {
            return Err(anyhow::anyhow!(
                "agent_runs.min_interval_secs 必须在 5 到 3600 秒之间"
            ));
        }
        if self.max_interval_secs < self.min_interval_secs || self.max_interval_secs > 86_400 {
            return Err(anyhow::anyhow!(
                "agent_runs.max_interval_secs 必须在 min_interval_secs 到 86400 秒之间"
            ));
        }
        if !(self.min_interval_secs..=self.max_interval_secs).contains(&self.default_interval_secs)
        {
            return Err(anyhow::anyhow!(
                "agent_runs.default_interval_secs 必须在轮询间隔范围内"
            ));
        }
        if self.default_stop_after_minutes == 0
            || self.default_stop_after_minutes > self.max_stop_after_minutes
            || self.max_stop_after_minutes > 10_080
        {
            return Err(anyhow::anyhow!(
                "agent_runs 停止时限必须为正数，默认值不能超过最大值，最大不能超过 10080 分钟"
            ));
        }
        if self.default_max_executions == 0
            || self.default_max_executions > self.max_executions_per_run
            || self.max_executions_per_run > 100_000
        {
            return Err(anyhow::anyhow!(
                "agent_runs 执行次数上限必须为正数，默认值不能超过单任务最大值，最大不能超过 100000"
            ));
        }
        if self.max_active_per_user == 0 || self.max_active_per_user > 100 {
            return Err(anyhow::anyhow!(
                "agent_runs.max_active_per_user 必须在 1 到 100 之间"
            ));
        }
        if self.max_active_total == 0 || self.max_active_total > 10_000 {
            return Err(anyhow::anyhow!(
                "agent_runs.max_active_total 必须在 1 到 10000 之间"
            ));
        }
        if self.max_consecutive_failures == 0 || self.max_consecutive_failures > 100 {
            return Err(anyhow::anyhow!(
                "agent_runs.max_consecutive_failures 必须在 1 到 100 之间"
            ));
        }
        if !(1_024..=2 * 1024 * 1024).contains(&self.max_response_bytes) {
            return Err(anyhow::anyhow!(
                "agent_runs.max_response_bytes 必须在 1024 到 2097152 字节之间"
            ));
        }
        if !(100..=8_000).contains(&self.max_body_preview_chars) {
            return Err(anyhow::anyhow!(
                "agent_runs.max_body_preview_chars 必须在 100 到 8000 之间"
            ));
        }
        if !(20..=2_000).contains(&self.max_notification_chars) {
            return Err(anyhow::anyhow!(
                "agent_runs.max_notification_chars 必须在 20 到 2000 之间"
            ));
        }
        if self.claim_batch_size == 0 || self.claim_batch_size > 128 {
            return Err(anyhow::anyhow!(
                "agent_runs.claim_batch_size 必须在 1 到 128 之间"
            ));
        }
        Ok(())
    }
}

impl Default for AgentRunConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            recovery_scan_secs: 30,
            lease_secs: 60,
            request_timeout_secs: 15,
            min_interval_secs: 5,
            max_interval_secs: 86_400,
            default_interval_secs: 30,
            default_stop_after_minutes: 1_440,
            max_stop_after_minutes: 10_080,
            default_max_executions: 20_000,
            max_executions_per_run: 100_000,
            max_active_per_user: 10,
            max_active_total: 100,
            max_consecutive_failures: 5,
            max_response_bytes: 512 * 1024,
            max_body_preview_chars: 2_000,
            max_notification_chars: 500,
            claim_batch_size: 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AgentRunConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(AgentRunConfig::default().validate().is_ok());
    }

    #[test]
    fn unsafe_polling_and_lease_limits_are_rejected() {
        let too_fast = AgentRunConfig {
            min_interval_secs: 1,
            ..AgentRunConfig::default()
        };
        assert!(too_fast.validate().is_err());

        let short_lease = AgentRunConfig {
            lease_secs: 20,
            request_timeout_secs: 15,
            ..AgentRunConfig::default()
        };
        assert!(short_lease.validate().is_err());
    }
}
