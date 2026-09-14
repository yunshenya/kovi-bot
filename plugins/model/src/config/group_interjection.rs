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
    ///
    /// 仅保留解析兼容：表情回应的频次目前只由 Core 侧的会话节奏约束，
    /// 生产代码不读这三个值。示例配置里已删除，避免它们看起来像生效的旋钮。
    sticker_reaction_cooldown_secs: u64,
    /// 表情回应限流统计窗口（秒）。仅保留解析兼容，见上。
    sticker_reaction_rate_window_secs: u64,
    /// 限流窗口内同一群最多回应多少次表情包。仅保留解析兼容，见上。
    sticker_reaction_rate_limit: usize,
    /// 熟人（熟悉度 ≥ familiarity_threshold）的未点名消息直接进入语义
    /// 评估，不等待抽样；是否真的回复仍由评估模型与 Core 决定。
    familiar_admit_enabled: bool,
    /// 视为"熟人"的熟悉度阈值（0..=1）。
    familiarity_threshold: f64,
    /// 熟人确定性放行的限流统计窗口（秒）。
    familiar_rate_window_secs: u64,
    /// 限流窗口内同一群最多放行多少条熟人消息进入语义评估。
    familiar_rate_limit: usize,
    /// 接续对话窗口：芸汐最近一次在本群发出可见消息后，未点名消息在
    /// 这个时长内走"接续对话"语义评估（由相关性判定是否回复），窗口外
    /// 回到低频插话抽样。窗口越长，群内每条消息请求模型的概率越高。
    ///
    /// 取值必须 ≤ `reply_gap_secs`：接续窗口比"她下次可以发言"的等待还长
    /// 时，窗口会变成常开状态——一次可见回复之后的整段时间里，群里每条
    /// 未点名消息都确定性进入语义评估，低频抽样与候选冷却全部作废。超限
    /// 时按 `reply_gap_secs` 收口并在启动时告警（与
    /// `effective_addressed_reply_gap_secs` 同一约定，但不拒绝启动）。
    continuation_window_secs: u64,
    /// 未点名的群聊回合要有可见输出，必须由 Mind 提出一个带由头的开口
    /// （open question / agenda / interest）；Mind 判定 silent 时直接保持
    /// 观察，不再把判定交给模型"自己看着办"。
    ///
    /// 关闭后回到旧行为（silent 只作为提示）。线上实测（2026-09-10~13）：
    /// 未点名回合里 Mind 判定 silent 却仍发出可见回复的有 271 条，占全部
    /// 群回复的 75%，是"没问她也要答"的主通道。
    ambient_requires_mind_intent: bool,
    /// 对话焦点：她在群里可见回复某人之后，记下"正在跟这个人对话"。
    ///
    /// 焦点活着时，这个人的未点名后续消息按"接续"处理——可以产生可见回复，
    /// 用 `continuation_reply_gap_secs` 那一档间隔，而不是未点名的防刷屏档；
    /// 连发的几条还会走完成度合批，按"说完了没"合成一轮。
    ///
    /// 为什么需要它（2026-09-14 实测）："@ 一次再连发几条"的形态下，后几条
    /// 未点名消息里 89% 连语义评估都进不去，进了评估的也会被 90 秒未点名
    /// 间隔静默——而接续窗口只有 60 秒，在她能再次开口之前就关了。焦点把
    /// 判据从"过了几秒"换成"是谁在跟我说话、这段对话还在不在"。
    continuation_enabled: bool,
    /// 焦点存活时长（秒）：她最后一次可见回复之后，多久内仍算在对话中。
    continuation_focus_ttl_secs: u64,
    /// 接续回合成轮前的最短停留（毫秒）。
    ///
    /// 完成度判定回答的是"这句话说完了没"，不是"这条请求说完了没"：用户分三条
    /// 发一个请求时，前两条往往各自都是完整句子，没有停留就会一条一轮。这个
    /// 值给"判为说完"的接续消息留一个窗口，让紧接着的下一条并进来。0 = 判完
    /// 即成轮（Host 链路的默认行为），取值受 `[message_batch] max_wait_ms` 封顶。
    continuation_merge_dwell_ms: u64,
    /// 接续回答的最短间隔（秒）。默认 0：接续是在同一个对话里接着说，
    /// "不要每句都回"那条防刷屏间隔不适用；总量仍由 `reply_rate_limit`
    /// 和焦点本身（别人插话即结束、TTL 到期）约束。想留一个下限时填正数。
    /// 取值必须 ≤ `reply_gap_secs`，否则退回 `reply_gap_secs`。
    continuation_reply_gap_secs: u64,
    /// 同群两次群聊可见回复（点名或未点名）之间的最短间隔（秒）。
    /// 对"每句话都回"的刷屏波次做硬性控制；管理员豁免。
    reply_gap_secs: u64,
    /// 被明确点名的消息（`@` 她本人或引用她）在两次可见回复之间的最短
    /// 间隔（秒）。它只放松"等一等"，不放松频率上限：直接提问不该被
    /// 静默丢掉，但预算耗尽时仍然按 `reply_rate_limit` 拒绝。
    /// 取值必须 ≤ `reply_gap_secs`，否则退回 `reply_gap_secs`。
    addressed_reply_gap_secs: u64,
    /// 群聊可见回复频率统计窗口（秒）。
    reply_rate_window_secs: u64,
    /// 统计窗口内同一群最多输出多少条可见回复（Admin/命令/识图等显式
    /// 请求不受限）。
    reply_rate_limit: usize,
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

    pub fn familiar_admit_enabled(&self) -> bool {
        self.familiar_admit_enabled
    }

    pub fn familiarity_threshold(&self) -> f64 {
        self.familiarity_threshold
    }

    pub fn familiar_rate_window_secs(&self) -> u64 {
        self.familiar_rate_window_secs
    }

    pub fn familiar_rate_limit(&self) -> usize {
        self.familiar_rate_limit
    }

    pub fn continuation_window_secs(&self) -> u64 {
        self.continuation_window_secs
    }

    /// 接续对话窗口的有效时长。窗口长于 `reply_gap_secs` 时它在她下一次
    /// 可以发言之前仍然敞开着，"低频接话"会退化成常开语义评估，因此超限
    /// 一律按 `reply_gap_secs` 处理。
    pub fn effective_continuation_window_secs(&self) -> u64 {
        self.continuation_window_secs.min(self.reply_gap_secs)
    }

    pub fn ambient_requires_mind_intent(&self) -> bool {
        self.ambient_requires_mind_intent
    }

    pub fn continuation_enabled(&self) -> bool {
        self.continuation_enabled
    }

    pub fn continuation_focus_ttl_secs(&self) -> u64 {
        self.continuation_focus_ttl_secs
    }

    pub fn continuation_merge_dwell_ms(&self) -> u64 {
        self.continuation_merge_dwell_ms
    }

    /// 接续回答使用的回复间隔。配置值超过普通间隔时视为无效，退回普通间隔：
    /// 这条通道只允许比默认节奏更宽松的显式配置生效（与点名档同一约定）。
    pub fn effective_continuation_reply_gap_secs(&self) -> u64 {
        if self.continuation_reply_gap_secs > self.reply_gap_secs {
            return self.reply_gap_secs;
        }
        self.continuation_reply_gap_secs
    }

    pub fn reply_gap_secs(&self) -> u64 {
        self.reply_gap_secs
    }

    /// 被点名消息使用的回复间隔。配置值大于普通间隔（或为 0）时视为无效，
    /// 退回普通间隔——这条通道只允许比默认节奏更宽松的显式配置生效。
    pub fn effective_addressed_reply_gap_secs(&self) -> u64 {
        if self.addressed_reply_gap_secs == 0 || self.addressed_reply_gap_secs > self.reply_gap_secs
        {
            return self.reply_gap_secs;
        }
        self.addressed_reply_gap_secs
    }

    pub fn reply_rate_window_secs(&self) -> u64 {
        self.reply_rate_window_secs
    }

    pub fn reply_rate_limit(&self) -> usize {
        self.reply_rate_limit
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
        if self.continuation_focus_ttl_secs == 0 {
            return Err(anyhow::anyhow!("群聊对话焦点存活时间必须大于0秒"));
        }
        if self.sticker_reaction_window_secs == 0
            || self.sticker_reaction_cooldown_secs == 0
            || self.sticker_reaction_rate_window_secs == 0
            || self.sticker_reaction_rate_limit == 0
        {
            return Err(anyhow::anyhow!("群聊表情回应限流配置必须大于0"));
        }
        if self.familiarity_threshold < 0.0 || self.familiarity_threshold > 1.0 {
            return Err(anyhow::anyhow!("熟人熟悉度阈值必须在0到1之间"));
        }
        if self.familiar_rate_window_secs == 0 || self.familiar_rate_limit == 0 {
            return Err(anyhow::anyhow!("熟人放行限流配置必须大于0"));
        }
        if self.continuation_window_secs == 0 {
            return Err(anyhow::anyhow!("接续对话窗口必须大于0秒"));
        }
        if self.reply_gap_secs == 0
            || self.reply_rate_window_secs == 0
            || self.reply_rate_limit == 0
        {
            return Err(anyhow::anyhow!("群聊回复节奏配置必须大于0"));
        }
        if self.addressed_reply_gap_secs > self.reply_gap_secs {
            return Err(anyhow::anyhow!(
                "被点名回复间隔不能大于普通回复间隔（{0} > {1}）",
                self.addressed_reply_gap_secs,
                self.reply_gap_secs
            ));
        }
        if self.continuation_window_secs > self.reply_gap_secs {
            // 只告警、不拒绝：读取侧一律按 `effective_continuation_window_secs`
            // 收口到回复间隔，行为仍然正确，这条不变量有安全的降级路径。
            // 而部署脚本会把服务端已有的 bot.conf.toml 原样带进新版本，硬失败
            // 会把一个陈旧的数值变成"起不来"的事故。
            eprintln!(
                "[WARN] 接续对话窗口大于群聊回复间隔，已按回复间隔收口（{0} > {1} 秒）：\
                 窗口比\"她下次可以发言\"的等待还长时，未点名消息会在整个窗口里逐条进入语义评估",
                self.continuation_window_secs, self.reply_gap_secs
            );
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
            familiar_admit_enabled: false,
            familiarity_threshold: 0.5,
            familiar_rate_window_secs: 600,
            familiar_rate_limit: 6,
            continuation_window_secs: 60,
            ambient_requires_mind_intent: true,
            continuation_enabled: true,
            continuation_focus_ttl_secs: 120,
            continuation_merge_dwell_ms: 2_500,
            continuation_reply_gap_secs: 0,
            reply_gap_secs: 90,
            addressed_reply_gap_secs: 20,
            reply_rate_window_secs: 600,
            reply_rate_limit: 4,
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

    #[test]
    fn addressed_reply_gap_must_not_exceed_the_normal_gap() {
        let config = GroupInterjectionConfig {
            reply_gap_secs: 90,
            addressed_reply_gap_secs: 120,
            ..GroupInterjectionConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_addressed_reply_gap_falls_back_to_the_normal_gap() {
        let default_gap = GroupInterjectionConfig::default().reply_gap_secs();
        for invalid in [0, default_gap + 1] {
            let config = GroupInterjectionConfig {
                addressed_reply_gap_secs: invalid,
                ..GroupInterjectionConfig::default()
            };
            assert_eq!(config.effective_addressed_reply_gap_secs(), default_gap);
        }

        let configured = GroupInterjectionConfig {
            reply_gap_secs: 90,
            addressed_reply_gap_secs: 30,
            ..GroupInterjectionConfig::default()
        };
        assert_eq!(configured.effective_addressed_reply_gap_secs(), 30);
    }

    #[test]
    fn continuation_window_over_the_reply_gap_warns_instead_of_failing_startup() {
        // 部署脚本会把服务端已有的 bot.conf.toml 原样带进新版本：陈旧数值
        // 不该让服务起不来，读取侧的收口已经保证行为正确。
        let config = GroupInterjectionConfig {
            reply_gap_secs: 90,
            continuation_window_secs: 180,
            ..GroupInterjectionConfig::default()
        };
        assert!(config.validate().is_ok());
        assert_eq!(config.effective_continuation_window_secs(), 90);
    }

    #[test]
    fn continuation_window_clamps_to_the_reply_gap() {
        let config = GroupInterjectionConfig {
            reply_gap_secs: 90,
            continuation_window_secs: 600,
            ..GroupInterjectionConfig::default()
        };
        assert_eq!(config.effective_continuation_window_secs(), 90);

        let configured = GroupInterjectionConfig {
            reply_gap_secs: 90,
            continuation_window_secs: 60,
            ..GroupInterjectionConfig::default()
        };
        assert_eq!(configured.effective_continuation_window_secs(), 60);
    }

    #[test]
    fn ambient_mind_intent_gate_defaults_on() {
        assert!(
            GroupInterjectionConfig::default().ambient_requires_mind_intent(),
            "未点名回合默认必须由 Mind 提出由头才允许可见输出"
        );
    }
}
