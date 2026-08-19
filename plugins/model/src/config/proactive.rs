//! 主动消息推送配置。

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ProactiveConfig {
    /// 是否启用随机主动消息。
    enabled: bool,
    /// 两次检查之间的间隔（秒）。
    check_interval_secs: u64,
    /// 最近没有足够互动多久后，才考虑主动发送（秒）。
    inactivity_threshold_secs: u64,
    /// 两次主动消息之间的最短间隔（秒）。
    cooldown_secs: u64,
    /// 每次满足条件后实际发送的概率（0-100）。
    push_probability_percent: u8,
}

impl ProactiveConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn check_interval_secs(&self) -> u64 {
        self.check_interval_secs
    }

    pub fn inactivity_threshold_secs(&self) -> u64 {
        self.inactivity_threshold_secs
    }

    pub fn cooldown_secs(&self) -> u64 {
        self.cooldown_secs
    }

    pub fn push_probability_percent(&self) -> u8 {
        self.push_probability_percent
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.check_interval_secs == 0 {
            return Err(anyhow::anyhow!("主动消息检查间隔必须大于0秒"));
        }
        if self.inactivity_threshold_secs == 0 {
            return Err(anyhow::anyhow!("主动消息空闲阈值必须大于0秒"));
        }
        if self.cooldown_secs == 0 {
            return Err(anyhow::anyhow!("主动消息冷却时间必须大于0秒"));
        }
        if self.push_probability_percent > 100 {
            return Err(anyhow::anyhow!("主动消息发送概率必须在0到100之间"));
        }
        Ok(())
    }
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: 300,
            inactivity_threshold_secs: 7200,
            cooldown_secs: 7200,
            push_probability_percent: 35,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProactiveConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(ProactiveConfig::default().validate().is_ok());
    }

    #[test]
    fn probability_over_one_hundred_is_rejected() {
        let config = ProactiveConfig {
            push_probability_percent: 101,
            ..ProactiveConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
