//! 群聊未点名接话配置。

use serde::{Deserialize, Serialize};

/// 控制机器人在未被点名的群聊中偶尔自然接话的频率。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct GroupInterjectionConfig {
    /// 是否允许机器人偶尔接上未点名的群聊话题。
    enabled: bool,
    /// 两次本地抽样之间至少积累多少条有价值的消息，不会消耗模型 token。
    min_eligible_messages: u32,
    /// 同一群两次未点名接话的最短间隔（秒）。
    cooldown_secs: u64,
    /// 到达抽样时机后，实际调用模型接话的概率（0-100）。
    response_probability_percent: u8,
    /// 只有达到该长度的消息才会作为候选，减少过短消息触发语义判断。
    min_message_chars: usize,
    /// 两次未点名模型判断之间的最短间隔（秒）。
    decision_cooldown_secs: u64,
    /// 未点名模型判断额度的统计窗口（秒）。
    decision_rate_window_secs: u64,
    /// 统计窗口内最多允许多少次未点名模型判断。
    decision_rate_limit: usize,
    /// 未点名接话单次允许生成的最大 token 数。
    interjection_max_output_tokens: u32,
    /// 连续刷屏后暂停处理该成员直接点名的时间（秒）。
    direct_spam_cooldown_secs: u64,
    /// 高频点名计数窗口（秒）。
    direct_rate_window_secs: u64,
    /// 计数窗口内允许同一成员直接触发的最大次数。
    direct_rate_limit: usize,
    /// 芸汐发言后，单独表情包可以被视为情绪回应的时间窗口（秒）。
    sticker_reaction_window_secs: u64,
    /// 同一成员表情回应的最短间隔（秒）。
    sticker_reaction_cooldown_secs: u64,
    /// 表情回应限流统计窗口（秒）。
    sticker_reaction_rate_window_secs: u64,
    /// 限流窗口内同一群最多回应多少次表情包。
    sticker_reaction_rate_limit: usize,
}

impl GroupInterjectionConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn min_eligible_messages(&self) -> u32 {
        self.min_eligible_messages
    }

    pub fn cooldown_secs(&self) -> u64 {
        self.cooldown_secs
    }

    pub fn response_probability_percent(&self) -> u8 {
        self.response_probability_percent
    }

    pub fn min_message_chars(&self) -> usize {
        self.min_message_chars
    }

    pub fn decision_cooldown_secs(&self) -> u64 {
        self.decision_cooldown_secs
    }

    pub fn decision_rate_window_secs(&self) -> u64 {
        self.decision_rate_window_secs
    }

    pub fn decision_rate_limit(&self) -> usize {
        self.decision_rate_limit
    }

    pub fn interjection_max_output_tokens(&self) -> u32 {
        self.interjection_max_output_tokens
    }

    pub fn direct_spam_cooldown_secs(&self) -> u64 {
        self.direct_spam_cooldown_secs
    }

    pub fn direct_rate_window_secs(&self) -> u64 {
        self.direct_rate_window_secs
    }

    pub fn direct_rate_limit(&self) -> usize {
        self.direct_rate_limit
    }

    pub fn sticker_reaction_window_secs(&self) -> u64 {
        self.sticker_reaction_window_secs
    }

    pub fn sticker_reaction_cooldown_secs(&self) -> u64 {
        self.sticker_reaction_cooldown_secs
    }

    pub fn sticker_reaction_rate_window_secs(&self) -> u64 {
        self.sticker_reaction_rate_window_secs
    }

    pub fn sticker_reaction_rate_limit(&self) -> usize {
        self.sticker_reaction_rate_limit
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.min_eligible_messages == 0 {
            return Err(anyhow::anyhow!("群聊接话消息间隔必须大于0"));
        }
        if self.cooldown_secs == 0 {
            return Err(anyhow::anyhow!("群聊接话冷却时间必须大于0秒"));
        }
        if self.response_probability_percent > 100 {
            return Err(anyhow::anyhow!("群聊接话概率必须在0到100之间"));
        }
        if self.min_message_chars == 0 {
            return Err(anyhow::anyhow!("群聊接话最小消息长度必须大于0"));
        }
        if self.decision_cooldown_secs == 0
            || self.decision_rate_window_secs == 0
            || self.decision_rate_limit == 0
            || self.interjection_max_output_tokens == 0
        {
            return Err(anyhow::anyhow!("群聊未点名判断额度必须大于0"));
        }
        if self.direct_spam_cooldown_secs == 0 || self.direct_rate_window_secs == 0 {
            return Err(anyhow::anyhow!("群聊防刷时间配置必须大于0秒"));
        }
        if self.direct_rate_limit < 2 {
            return Err(anyhow::anyhow!("群聊点名频率上限不能小于2"));
        }
        if self.sticker_reaction_window_secs == 0
            || self.sticker_reaction_cooldown_secs == 0
            || self.sticker_reaction_rate_window_secs == 0
            || self.sticker_reaction_rate_limit == 0
        {
            return Err(anyhow::anyhow!("群聊表情回应限流配置必须大于0"));
        }
        Ok(())
    }
}

impl Default for GroupInterjectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_eligible_messages: 4,
            cooldown_secs: 180,
            response_probability_percent: 60,
            min_message_chars: 4,
            decision_cooldown_secs: 60,
            decision_rate_window_secs: 600,
            decision_rate_limit: 3,
            interjection_max_output_tokens: 240,
            direct_spam_cooldown_secs: 600,
            direct_rate_window_secs: 60,
            direct_rate_limit: 4,
            sticker_reaction_window_secs: 90,
            sticker_reaction_cooldown_secs: 30,
            sticker_reaction_rate_window_secs: 300,
            sticker_reaction_rate_limit: 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GroupInterjectionConfig;

    #[test]
    fn defaults_are_valid() {
        assert!(GroupInterjectionConfig::default().validate().is_ok());
    }

    #[test]
    fn probability_over_one_hundred_is_rejected() {
        let config = GroupInterjectionConfig {
            response_probability_percent: 101,
            ..GroupInterjectionConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
