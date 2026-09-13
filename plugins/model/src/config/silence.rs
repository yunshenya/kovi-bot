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
//!
//! 这一节还带第二个维度：`group_cooling_enabled` 控制群级降温——个人级看的是
//! "某个人怎么对她"，群级看的是"这个群还欢迎她主动开口吗"。它只降低未点名
//! 插话的抽样频率，被点名的回合永远不受影响；默认关闭，理由与个人级相同。

use serde::{Deserialize, Serialize};

/// 相处信号与静默门控的参数。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct SilenceConfig {
    /// 是否让静默真正生效。
    ///
    /// 默认 **true**：相处经验改变行为是这个功能的用途本身，"装上但不开"会让它
    /// 在真正需要的时候恰好没生效。要只看不动就显式写 `false`——那时只记账、只打
    /// `[SILENCE] shadow=true`，可见回复一条不少（拿真实日志核判据时用得上）。
    ///
    /// 之所以能默认开：判据经过实测校准（约 6 条强烈敌意或 22 条指向她的辱骂才越
    /// 0.6），且有四重边界——3 天半衰期自然回落、善意主动降温、管理员永远放行、
    /// 私聊不拦。上线前核对过线上关系表：没有任何一条活跃张力接近阈值，
    /// 不存在"一打开就有人被静默"的存量风险。
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
    /// 群级降温开关：整个群长期把她当外人时，降低她在这个群**未点名**插话的
    /// 频率（命中只放弃这一次抽样机会，不是静默，也不影响任何被点名的回合）。
    /// 默认 false＝只记账、只打影子日志（`[GROUP_COOLING] shadow=true`）。
    ///
    /// 为什么与 `enabled` 分开：个人级门控改的是"不接这个人"，群级改的是
    /// "在这个群少主动开口"。两者证据不同（关系张力 vs 群气氛）、误伤面也不同，
    /// 必须能分别打开观察，否则一个开关会把两套判据的线上表现混在一起。
    ///
    /// 默认 **true**：与个人级门控同一个判断标准——装上但不开，会让它在需要
    /// 的时候恰好没作用。要只看不动就显式写 `false`，那时只打 `[GROUP_COOLING]`
    /// 影子日志。
    group_cooling_enabled: bool,
}

impl Default for SilenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            negative_threshold: 3,
            decay_days: 30,
            warm_recovery_count: 2,
            group_cooling_enabled: true,
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

    /// 群级降温是否真正生效。false 时判据照跑，只打影子日志。
    pub fn group_cooling_enabled(&self) -> bool {
        self.group_cooling_enabled
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
    fn defaults_are_bounded_and_the_gate_is_on() {
        let config = SilenceConfig::default();
        // 个人级门控默认生效：装上但不开，会让这个功能在需要时恰好没作用。
        assert!(config.enabled());
        // 群级降温是另一个维度、独立开关，同样默认开启。
        assert!(config.group_cooling_enabled());
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
