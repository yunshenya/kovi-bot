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
    /// 只有达到该长度的消息才会作为候选，过滤“嗯”“哈哈”等短消息。
    min_message_chars: usize,
    /// 机器人成功接话后，允许无称呼继续对话的时间窗口（秒）。
    conversation_window_secs: u64,
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

    pub fn conversation_window_secs(&self) -> u64 {
        self.conversation_window_secs
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
        if self.conversation_window_secs == 0 {
            return Err(anyhow::anyhow!("群聊接话对话窗口必须大于0秒"));
        }
        Ok(())
    }
}

impl Default for GroupInterjectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_eligible_messages: 8,
            cooldown_secs: 180,
            response_probability_percent: 35,
            min_message_chars: 5,
            conversation_window_secs: 120,
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
