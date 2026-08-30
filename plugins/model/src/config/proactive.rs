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
    /// 旧版最信任用户 QQ 号。配置 canonical `identity.owner_person_id`
    /// 后不再承担 owner 语义，仅作为未迁移部署的兼容回退。
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
    /// 主动消息进入 Prepared 后的短竞争窗口；0 关闭，否则限 300-1000ms。
    prepared_grace_ms: u64,
    /// 是否启用 Neuro-sama 风格的自主会话续聊。
    autonomous_conversation_enabled: bool,
    /// 自主会话循环的检查间隔（秒）。
    autonomous_conversation_check_interval_secs: u64,
    /// 用户回复后，进入自主续聊前至少等待的时间（秒）。
    autonomous_conversation_idle_secs: u64,
    /// 自主会话选择继续时，两次模型回合之间的最短间隔（秒）。
    autonomous_conversation_cooldown_secs: u64,
    /// 普通（未明确邀请连续聊天）的私聊互动最多允许连续自主续聊的回合数。
    /// 明确邀请开放式连续聊天的私聊由模型决定何时结束，不受此值限制。
    autonomous_conversation_max_turns: u8,
    /// 群聊进入自主续聊前至少等待的时间（秒）。群聊默认比私聊更克制。
    autonomous_conversation_group_idle_secs: u64,
    /// 群聊自主续聊选择继续时，两次模型回合之间的最短间隔（秒）。
    autonomous_conversation_group_cooldown_secs: u64,
    /// 一次群聊互动最多允许连续自主续聊的回合数。
    autonomous_conversation_group_max_turns: u8,
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

    pub fn prepared_grace_ms(&self) -> u64 {
        self.prepared_grace_ms
    }

    pub fn autonomous_conversation_enabled(&self) -> bool {
        self.autonomous_conversation_enabled
    }

    pub fn autonomous_conversation_check_interval_secs(&self) -> u64 {
        self.autonomous_conversation_check_interval_secs
    }

    pub fn autonomous_conversation_idle_secs(&self) -> u64 {
        self.autonomous_conversation_idle_secs
    }

    pub fn autonomous_conversation_cooldown_secs(&self) -> u64 {
        self.autonomous_conversation_cooldown_secs
    }

    pub fn autonomous_conversation_max_turns(&self) -> u8 {
        self.autonomous_conversation_max_turns
    }

    pub fn autonomous_conversation_group_idle_secs(&self) -> u64 {
        self.autonomous_conversation_group_idle_secs
    }

    pub fn autonomous_conversation_group_cooldown_secs(&self) -> u64 {
        self.autonomous_conversation_group_cooldown_secs
    }

    pub fn autonomous_conversation_group_max_turns(&self) -> u8 {
        self.autonomous_conversation_group_max_turns
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
        if self.prepared_grace_ms != 0 && !(300..=1_000).contains(&self.prepared_grace_ms) {
            return Err(anyhow::anyhow!(
                "主动消息 Prepared 竞争窗口必须为0或300到1000毫秒"
            ));
        }
        if self.autonomous_conversation_check_interval_secs == 0 {
            return Err(anyhow::anyhow!("自主会话循环检查间隔必须大于0秒"));
        }
        if self.autonomous_conversation_idle_secs == 0 {
            return Err(anyhow::anyhow!("自主会话空闲阈值必须大于0秒"));
        }
        if self.autonomous_conversation_cooldown_secs == 0 {
            return Err(anyhow::anyhow!("自主会话冷却时间必须大于0秒"));
        }
        if self.autonomous_conversation_max_turns == 0 || self.autonomous_conversation_max_turns > 8
        {
            return Err(anyhow::anyhow!(
                "普通自主会话单次互动最多连续回合数必须在1到8之间"
            ));
        }
        if self.autonomous_conversation_group_idle_secs == 0 {
            return Err(anyhow::anyhow!("群聊自主会话空闲阈值必须大于0秒"));
        }
        if self.autonomous_conversation_group_cooldown_secs == 0 {
            return Err(anyhow::anyhow!("群聊自主会话冷却时间必须大于0秒"));
        }
        if self.autonomous_conversation_group_max_turns == 0
            || self.autonomous_conversation_group_max_turns > 8
        {
            return Err(anyhow::anyhow!(
                "群聊自主会话单次互动最多连续回合数必须在1到8之间"
            ));
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
            prepared_grace_ms: 500,
            autonomous_conversation_enabled: true,
            autonomous_conversation_check_interval_secs: 15,
            autonomous_conversation_idle_secs: 90,
            autonomous_conversation_cooldown_secs: 15,
            autonomous_conversation_max_turns: 4,
            autonomous_conversation_group_idle_secs: 180,
            autonomous_conversation_group_cooldown_secs: 90,
            autonomous_conversation_group_max_turns: 1,
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
        assert_eq!(config.prepared_grace_ms(), 500);
        assert_eq!(config.autonomous_conversation_check_interval_secs(), 15);
        assert_eq!(config.autonomous_conversation_cooldown_secs(), 15);
        assert_eq!(config.autonomous_conversation_max_turns(), 4);
        assert_eq!(config.autonomous_conversation_group_idle_secs(), 180);
        assert_eq!(config.autonomous_conversation_group_cooldown_secs(), 90);
        assert_eq!(config.autonomous_conversation_group_max_turns(), 1);
    }

    #[test]
    fn prepared_grace_can_be_disabled_but_rejects_long_typing_delays() {
        let disabled = ProactiveConfig {
            prepared_grace_ms: 0,
            ..ProactiveConfig::default()
        };
        assert!(disabled.validate().is_ok());
        let too_short = ProactiveConfig {
            prepared_grace_ms: 299,
            ..ProactiveConfig::default()
        };
        assert!(too_short.validate().is_err());
        let too_long = ProactiveConfig {
            prepared_grace_ms: 1_001,
            ..ProactiveConfig::default()
        };
        assert!(too_long.validate().is_err());
    }
}
