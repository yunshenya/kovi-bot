use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// 持久化提醒任务配置。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ReminderConfig {
    /// 是否允许模型创建和管理提醒。
    enabled: bool,
    /// 调度器扫描到期任务的间隔（秒）。
    poll_interval_secs: u64,
    /// 单个用户最多保留多少条未完成私聊提醒。
    max_pending_per_user: usize,
    /// 单个群最多保留多少条未完成群聊提醒。
    max_pending_per_group: usize,
    /// 所有会话最多保留多少条未完成提醒。
    max_pending_total: usize,
    /// 单条提醒最多提前多少天。
    max_delay_days: u64,
    /// 未指定时使用的 IANA 时区。
    default_timezone: String,
    /// 提醒正文最大字符数。
    max_message_chars: usize,
    /// 发送失败后的最大尝试次数。
    max_attempts: u8,
    /// 单次任务领取租约时间（秒）。
    lease_secs: u64,
}

impl ReminderConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }

    pub fn max_pending_per_user(&self) -> usize {
        self.max_pending_per_user
    }

    pub fn max_pending_per_group(&self) -> usize {
        self.max_pending_per_group
    }

    pub fn max_pending_total(&self) -> usize {
        self.max_pending_total
    }

    pub fn max_delay_days(&self) -> u64 {
        self.max_delay_days
    }

    pub fn default_timezone(&self) -> &str {
        &self.default_timezone
    }

    pub fn max_message_chars(&self) -> usize {
        self.max_message_chars
    }

    pub fn max_attempts(&self) -> u8 {
        self.max_attempts
    }

    pub fn lease_secs(&self) -> u64 {
        self.lease_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.poll_interval_secs == 0 || self.poll_interval_secs > 60 {
            return Err(anyhow::anyhow!(
                "reminders.poll_interval_secs 必须在 1 到 60 秒之间"
            ));
        }
        if self.max_pending_per_user == 0 || self.max_pending_per_user > 100 {
            return Err(anyhow::anyhow!(
                "reminders.max_pending_per_user 必须在 1 到 100 之间"
            ));
        }
        if self.max_pending_per_group == 0 || self.max_pending_per_group > 500 {
            return Err(anyhow::anyhow!(
                "reminders.max_pending_per_group 必须在 1 到 500 之间"
            ));
        }
        if self.max_pending_total == 0 || self.max_pending_total > 100_000 {
            return Err(anyhow::anyhow!(
                "reminders.max_pending_total 必须在 1 到 100000 之间"
            ));
        }
        if self.max_delay_days == 0 || self.max_delay_days > 3_650 {
            return Err(anyhow::anyhow!(
                "reminders.max_delay_days 必须在 1 到 3650 天之间"
            ));
        }
        self.default_timezone
            .parse::<Tz>()
            .map_err(|_| anyhow::anyhow!("reminders.default_timezone 必须是有效的 IANA 时区"))?;
        if self.max_message_chars == 0 || self.max_message_chars > 2_000 {
            return Err(anyhow::anyhow!(
                "reminders.max_message_chars 必须在 1 到 2000 之间"
            ));
        }
        if self.max_attempts == 0 || self.max_attempts > 10 {
            return Err(anyhow::anyhow!(
                "reminders.max_attempts 必须在 1 到 10 之间"
            ));
        }
        if self.lease_secs < 10 || self.lease_secs > 600 {
            return Err(anyhow::anyhow!(
                "reminders.lease_secs 必须在 10 到 600 秒之间"
            ));
        }
        Ok(())
    }
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: 5,
            max_pending_per_user: 20,
            max_pending_per_group: 100,
            max_pending_total: 10_000,
            max_delay_days: 365,
            default_timezone: "Asia/Shanghai".to_string(),
            max_message_chars: 500,
            max_attempts: 3,
            lease_secs: 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReminderConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(ReminderConfig::default().validate().is_ok());
    }

    #[test]
    fn invalid_limits_are_rejected() {
        let config = ReminderConfig {
            max_attempts: 0,
            ..ReminderConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_timezone_is_rejected() {
        let config = ReminderConfig {
            default_timezone: "Not/A_Timezone".to_string(),
            ..ReminderConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
