//! 「相处信号 → 静默门控」配置。
//!
//! 背景：宿主对"被 @ / 被引用"的消息一律给一次回复回合，记忆与关系状态在这条
//! 路径上没有任何发言权（2026-09-13 群 641996763 的实测事故）。这一节的参数
//! 决定她能不能因为"这个人一直这样对她"而**主动不接**。
//!
//! 为什么默认 `enabled = false`：门控一旦生效，误判的表现是"她突然不理人"，
//! 而它与"被点名必回"这条地基契约冲突。因此默认只**计数 + 打影子日志**
//! （`[SILENCE] shadow=true` 会写明"如果打开，这条会被静默"），拿真实群里的
//! 日志确认判据不误伤之后再打开。
//!
//! 静默是冷却，不是封禁：有 TTL、有衰减、有每群每日上限
//! （防"一群人轮流测试把她变哑巴"），管理员可随时解除。

use serde::{Deserialize, Serialize};

/// 相处信号与静默门控的参数。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct SilenceConfig {
    /// 是否让静默真正生效。默认 false＝只计数、只打影子日志，可见回复一条不少。
    enabled: bool,
    /// 指向她的一条消息要被记为"不友好经验"所需的负面特征数。
    ///
    /// 计分是"证据计数"而不是"情绪打分"：一条消息最多加一分，避免一句话里
    /// 连说三个脏字就算三次。
    negative_threshold: u32,
    /// 负面证据的时间衰减窗口（天）。
    ///
    /// 实际衰减由 Core 的关系漂移执行（张力半衰期 3 天）；这个值只作为
    /// "多算一次太旧的经验"的说明性上限。
    decay_days: i64,
    /// 静默期间对方主动、友好地说了多少条才提前解除。
    ///
    /// 为什么需要它：只有惩罚没有回暖的话，一次情绪冲突会让她永久冷掉一个人，
    /// 而人是会变的。当前由关系张力的善意降温实现（每次善意按 0.12 的混合率
    /// 往 0 拉），这个值是那条通道的目标条数。
    warm_recovery_count: u32,
}

impl Default for SilenceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            negative_threshold: 3,
            decay_days: 30,
            warm_recovery_count: 2,
        }
    }
}

impl SilenceConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn negative_threshold(&self) -> u32 {
        self.negative_threshold
    }

    pub fn decay_days(&self) -> i64 {
        self.decay_days
    }

    pub fn warm_recovery_count(&self) -> u32 {
        self.warm_recovery_count
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.negative_threshold == 0 {
            return Err(anyhow::anyhow!("静默门控的负面经验阈值必须大于0"));
        }
        if self.decay_days <= 0 {
            return Err(anyhow::anyhow!("静默信号的衰减窗口必须大于0天"));
        }
        if self.warm_recovery_count == 0 {
            return Err(anyhow::anyhow!("提前解除静默所需的友好条数必须大于0"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SilenceConfig;

    #[test]
    fn defaults_are_shadow_first_and_bounded() {
        let config = SilenceConfig::default();
        // 默认不改变任何可见行为：这是这套机制能被安全地装上线的唯一前提。
        assert!(!config.enabled());
        assert!(config.validate().is_ok());
        // 有界：必须有衰减窗口与回暖通道，否则静默会变成永久冷处理。
        assert!(config.decay_days() > 0);
        assert!(config.warm_recovery_count() > 0);
    }

    #[test]
    fn zero_valued_parameters_are_rejected_instead_of_silently_behaving_oddly() {
        for broken in [
            SilenceConfig {
                negative_threshold: 0,
                ..SilenceConfig::default()
            },
            SilenceConfig {
                decay_days: 0,
                ..SilenceConfig::default()
            },
            SilenceConfig {
                warm_recovery_count: 0,
                ..SilenceConfig::default()
            },
        ] {
            assert!(broken.validate().is_err());
        }
    }
}
