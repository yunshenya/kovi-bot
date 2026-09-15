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
    /// 哪些主动理由可以用**电话**打出去（取值见 `ProactiveMotive`：`check_in`、
    /// `follow_up`、`share`、`react`、`curiosity`）。留空 = 从不主动打电话。
    ///
    /// 默认只放 `check_in`：那个动机是"担心他最近怎么样"，打电话是贴切的；其余
    /// （分享、好奇、回应、跟进）一条消息就够。
    ///
    /// 为什么按动机白名单、而不是让模型在生成话题时自己挑媒介：这条路是**定时器**
    /// 触发的，而人已经不在对话里了。让一个纯文本生成步决定"要不要响他手机"，判错
    /// 收不回来；动机则是 Core 已经算好、可审计的结构化输入。真要放开/收紧，改配置
    /// 即可，不必碰提示词。
    call_motives: Vec<String>,
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
    /// 用户最近主动互动后，暂不追加主动私聊的时间。
    recent_interaction_cooldown_secs: u64,
    /// 群聊随机主动消息的“在场”窗口（秒）。只有在这个窗口内确实有人
    /// 说过话时，芸汐才会挑一个话题插进去；冷清的群不会被冷不丁打扰。
    group_activity_window_secs: u64,
    /// 上述窗口内至少要有多少条真人消息，才算“群里现在有人在聊”。
    /// 芸汐自己的主动消息以 `proactive_` 开头，永远不计入这个数字。
    group_activity_min_messages: u32,
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
    /// 群聊进入自主续聊前至少等待的时间（秒）。群聊默认比私聊更克制。
    autonomous_conversation_group_idle_secs: u64,
    /// 群聊自主续聊选择继续时，两次模型回合之间的最短间隔（秒）。
    autonomous_conversation_group_cooldown_secs: u64,
    /// 单条入站消息之后，最多允许连续多少次自主续聊会话。新的入站会把计数
    /// 归零，因此它约束的是"一次想接话的高潮"，不会在用户再次说话后禁用主动。
    /// 默认 6；设为 0 会被校验拒绝。旧版 `autonomous_conversation_group_max_turns`
    /// 已不再被读取，本字段取代它并同时作用于私聊与群聊。
    autonomous_conversation_max_turns: u64,
    /// 旧版群聊自主续聊上限。保留用于配置反序列化兼容，当前自主会话
    /// 不再读取这个字段，是否继续完全由模型语义与 `autonomous_conversation_max_turns`
    /// 共同决定。
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

    /// 这个主动理由是否允许用电话接触。
    ///
    /// 判据只读配置：能不能拨出去（通道开没开、对方在不在通话名单）由
    /// `qq_call` 那边在执行前再判一次——这里管"该不该"，那里管"行不行"。
    pub fn may_call_for(&self, motive: yunxi_core::ProactiveMotive) -> bool {
        let name = motive.to_string();
        self.call_motives
            .iter()
            .any(|configured| configured == &name)
    }

    pub fn call_motives(&self) -> &[String] {
        &self.call_motives
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

    pub fn group_activity_window_secs(&self) -> u64 {
        self.group_activity_window_secs
    }

    pub fn group_activity_min_messages(&self) -> u32 {
        self.group_activity_min_messages
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

    pub fn autonomous_conversation_group_idle_secs(&self) -> u64 {
        self.autonomous_conversation_group_idle_secs
    }

    pub fn autonomous_conversation_group_cooldown_secs(&self) -> u64 {
        self.autonomous_conversation_group_cooldown_secs
    }

    pub fn autonomous_conversation_max_turns(&self) -> u64 {
        self.autonomous_conversation_max_turns
    }

    /// Builder override used by tests and embedded hosts that need a specific
    /// autonomous-continuation ceiling without re-serialising config.
    #[must_use]
    pub fn with_autonomous_conversation_max_turns(mut self, value: u64) -> Self {
        self.autonomous_conversation_max_turns = value;
        self
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
        // 动机名拼错就当场报错：静默忽略会让"我明明配了打电话"变成一个不生效的开关，
        // 而这类问题在运行时看不出来（只会表现为"她从来不打电话"）。
        for motive in &self.call_motives {
            motive.parse::<yunxi_core::ProactiveMotive>().map_err(|_| {
                anyhow::anyhow!(
                    "proactive.call_motives 里有未知的主动理由：{motive}\
                         （可用：follow_up、check_in、share、react、curiosity）"
                )
            })?;
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
        if self.group_activity_window_secs == 0 {
            return Err(anyhow::anyhow!("群聊主动消息在场窗口必须大于0秒"));
        }
        if self.group_activity_min_messages == 0 || self.group_activity_min_messages > 50 {
            return Err(anyhow::anyhow!("群聊主动消息最少消息数必须在1到50之间"));
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
        if self.autonomous_conversation_group_idle_secs == 0 {
            return Err(anyhow::anyhow!("群聊自主会话空闲阈值必须大于0秒"));
        }
        if self.autonomous_conversation_group_cooldown_secs == 0 {
            return Err(anyhow::anyhow!("群聊自主会话冷却时间必须大于0秒"));
        }
        if self.autonomous_conversation_max_turns == 0 {
            return Err(anyhow::anyhow!("自主会话连续续聊上限必须大于0"));
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
            call_motives: vec!["check_in".to_string()],
            push_probability_percent: 35,
            main_admin: None,
            main_admin_decision_interval_secs: 10_800,
            main_admin_cooldown_secs: 21_600,
            daily_limit: 4,
            main_admin_daily_limit: 2,
            target_cooldown_secs: 21_600,
            recent_interaction_cooldown_secs: 7_200,
            group_activity_window_secs: 300,
            group_activity_min_messages: 2,
            prepared_grace_ms: 500,
            autonomous_conversation_enabled: true,
            autonomous_conversation_check_interval_secs: 3,
            autonomous_conversation_idle_secs: 5,
            autonomous_conversation_cooldown_secs: 3,
            autonomous_conversation_group_idle_secs: 45,
            autonomous_conversation_group_cooldown_secs: 15,
            autonomous_conversation_max_turns: 6,
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

    /// 默认只让「担心他最近怎么样」用电话打出去。
    ///
    /// 这条是产品判断，不是技术细节：主动接触是**定时器**触发的，人已经不在对话里，
    /// 所以默认只放动机最贴切的那一种，其余一条消息就够。
    #[test]
    fn only_check_in_may_call_by_default() {
        use yunxi_core::ProactiveMotive;
        let config = ProactiveConfig::default();
        assert!(config.may_call_for(ProactiveMotive::CheckIn));
        for motive in [
            ProactiveMotive::FollowUp,
            ProactiveMotive::Share,
            ProactiveMotive::React,
            ProactiveMotive::Curiosity,
        ] {
            assert!(!config.may_call_for(motive), "{motive:?} 默认不该打电话");
        }

        // 留空 = 从不主动打电话。
        let never = ProactiveConfig {
            call_motives: Vec::new(),
            ..ProactiveConfig::default()
        };
        assert!(never.validate().is_ok());
        assert!(!never.may_call_for(ProactiveMotive::CheckIn));
    }

    /// 动机名拼错要当场报错。
    ///
    /// 静默忽略的话，"我明明配了打电话"会表现为"她从来不打电话"，而运行时不报任何东西。
    #[test]
    fn an_unknown_call_motive_is_rejected_at_load() {
        let config = ProactiveConfig {
            call_motives: vec!["checkin".to_string()],
            ..ProactiveConfig::default()
        };
        let error = config.validate().expect_err("拼错的动机名必须报错");
        assert!(error.to_string().contains("checkin"), "{error}");
        assert!(
            error.to_string().contains("check_in"),
            "错误里该给可用取值: {error}"
        );
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
        assert_eq!(config.group_activity_window_secs(), 300);
        assert_eq!(config.group_activity_min_messages(), 2);
        assert_eq!(config.autonomous_conversation_check_interval_secs(), 3);
        assert_eq!(config.autonomous_conversation_idle_secs(), 5);
        assert_eq!(config.autonomous_conversation_cooldown_secs(), 3);
        assert_eq!(config.autonomous_conversation_group_idle_secs(), 45);
        assert_eq!(config.autonomous_conversation_group_cooldown_secs(), 15);
        assert_eq!(config.autonomous_conversation_group_max_turns(), 1);
    }

    #[test]
    fn group_activity_gate_requires_a_positive_window_and_threshold() {
        let no_window = ProactiveConfig {
            group_activity_window_secs: 0,
            ..ProactiveConfig::default()
        };
        assert!(no_window.validate().is_err());
        let no_messages = ProactiveConfig {
            group_activity_min_messages: 0,
            ..ProactiveConfig::default()
        };
        assert!(no_messages.validate().is_err());
        let absurd = ProactiveConfig {
            group_activity_min_messages: 51,
            ..ProactiveConfig::default()
        };
        assert!(absurd.validate().is_err());
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
