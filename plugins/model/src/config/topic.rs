use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct TopicConfig {
    recent_topic_cooldown_secs: u64,
}

impl TopicConfig {
    pub fn recent_topic_cooldown_secs(&self) -> u64 {
        self.recent_topic_cooldown_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.recent_topic_cooldown_secs == 0 {
            return Err(anyhow::anyhow!(
                "topic.recent_topic_cooldown_secs 必须大于 0"
            ));
        }
        Ok(())
    }
}

impl Default for TopicConfig {
    fn default() -> Self {
        Self {
            recent_topic_cooldown_secs: 604_800,
        }
    }
}
