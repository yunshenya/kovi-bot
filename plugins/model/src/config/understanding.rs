use serde::{Deserialize, Serialize};

/// Core 接管回合的会话理解配置。
///
/// 背景：`understand()`（mood / gratitude / interests / 画像学习）一直只挂在 Host
/// 的两个入站处理器上，而普通私聊文本与"指向她"的群消息都归 Core——于是这些回合
/// 既没有情绪、也没有兴趣与关系等级的更新，README 承诺的"私聊会持续更新用户的
/// 兴趣、性格、关系等级和情绪历史"自切换起就不成立。
///
/// 打开之后，Core 接管的回合也会跑一次同样的会话理解：结果只喂给情绪、关系与
/// 画像（`project_interaction_cues` + `learn_user_profile_from_message`），不参与
/// 这一轮该不该回、回什么——理解是后台任务，不占回复延迟，也不改变消息去向。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct UnderstandingConfig {
    /// 是否为 Core 接管的回合也跑一次会话理解。
    ///
    /// 代价是每条这样的消息多一次有界分类调用（输入截断、输出上限 420 token，
    /// 与群聊那条相处证据判定同量级）。关掉它省下这次调用，代价是那些回合不再
    /// 更新情绪/兴趣/关系等级，相处证据也只剩"指向她的群消息"那一路。
    core_owned_enabled: bool,
}

impl Default for UnderstandingConfig {
    fn default() -> Self {
        Self {
            // 默认开启：这是被 Core 接管"顺手关掉"的既有行为，不是新增开销。
            core_owned_enabled: true,
        }
    }
}

impl UnderstandingConfig {
    pub fn core_owned_enabled(&self) -> bool {
        self.core_owned_enabled
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        // 一个布尔开关没有取值域可校验；留这个方法是为了与其它分区同一形状，
        // 也让"这里以后加参数"有一个已经接好的落点。
        Ok(())
    }
}
