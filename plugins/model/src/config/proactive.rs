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
    /// 最信任的用户 QQ 号；配置后由模型自主决定是否主动私聊。
    main_admin: Option<i64>,
    /// 两次“是否联系主人”的模型决策之间的最短间隔，避免每轮循环额外消耗 token。
    main_admin_decision_interval_secs: u64,
    /// 两次实际主动联系主人之间的最短间隔。
    main_admin_cooldown_secs: u64,
    /// 全部主动消息每天最多发送的条数。
    daily_limit: u8,
    /// 主人每天最多收到的主动私聊条数。
    main_admin_daily_limit: u8,
    /// 同一个群组或用户再次收到主动消息前的最短间隔。
    target_cooldown_secs: u64,
    /// 用户或群组最近主动互动后，暂不追加主动消息的时间。
    recent_interaction_cooldown_secs: u64,
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

    pub fn main_admin(&self) -> Option<i64> {
        self.main_admin
    }

    pub fn main_admin_decision_interval_secs(&self) -> u64 {
        self.main_admin_decision_interval_secs
    }

    pub fn main_admin_cooldown_secs(&self) -> u64 {
        self.main_admin_cooldown_secs
    }

    pub fn daily_limit(&self) -> u8 {
        self.daily_limit
    }

    pub fn main_admin_daily_limit(&self) -> u8 {
        self.main_admin_daily_limit
    }

    pub fn target_cooldown_secs(&self) -> u64 {
        self.target_cooldown_secs
    }

    pub fn recent_interaction_cooldown_secs(&self) -> u64 {
        self.recent_interaction_cooldown_secs
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
        if self.main_admin_decision_interval_secs == 0 {
            return Err(anyhow::anyhow!("主人主动私聊决策间隔必须大于0秒"));
        }
        if self.main_admin_cooldown_secs == 0 {
            return Err(anyhow::anyhow!("主人主动私聊冷却时间必须大于0秒"));
        }
        if self.daily_limit == 0 {
            return Err(anyhow::anyhow!("主动消息每日上限必须大于0"));
        }
        if self.main_admin_daily_limit == 0 {
            return Err(anyhow::anyhow!("主人主动私聊每日上限必须大于0"));
        }
        if self.target_cooldown_secs == 0 {
            return Err(anyhow::anyhow!("主动消息目标冷却时间必须大于0秒"));
        }
        if self.recent_interaction_cooldown_secs == 0 {
            return Err(anyhow::anyhow!("主动消息互动抑制时间必须大于0秒"));
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
            main_admin: None,
            main_admin_decision_interval_secs: 10_800,
            main_admin_cooldown_secs: 21_600,
            daily_limit: 4,
            main_admin_daily_limit: 2,
            target_cooldown_secs: 21_600,
            recent_interaction_cooldown_secs: 7_200,
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

    #[test]
    fn main_admin_requires_a_positive_decision_interval() {
        let config = ProactiveConfig {
            main_admin: Some(1),
            main_admin_decision_interval_secs: 0,
            ..ProactiveConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn defaults_include_conservative_send_limits() {
        let config = ProactiveConfig::default();
        assert_eq!(config.main_admin_cooldown_secs(), 21_600);
        assert_eq!(config.daily_limit(), 4);
        assert_eq!(config.main_admin_daily_limit(), 2);
        assert_eq!(config.target_cooldown_secs(), 21_600);
    }
}
