//! # 提示词配置模块
//!
//! 管理机器人的提示词配置，包括群聊和私聊的系统提示

use serde::{Deserialize, Serialize};

/// 提示词配置结构体
///
/// 包含机器人在不同场景下使用的系统提示词
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct Prompt {
    /// 群聊系统提示词
    system_prompt: String,
    /// 私聊系统提示词
    private_prompt: String,
}

impl Prompt {
    pub fn system_prompt(&self) -> &str {
        self.system_prompt.as_str()
    }

    pub fn private_prompt(&self) -> &str {
        self.private_prompt.as_str()
    }

    /// 验证提示配置
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.system_prompt.is_empty() {
            return Err(anyhow::anyhow!("系统提示不能为空"));
        }

        if self.private_prompt.is_empty() {
            return Err(anyhow::anyhow!("私聊提示不能为空"));
        }

        if self.system_prompt.len() < 10 {
            return Err(anyhow::anyhow!("系统提示太短，至少需要10个字符"));
        }

        if self.private_prompt.len() < 10 {
            return Err(anyhow::anyhow!("私聊提示太短，至少需要10个字符"));
        }

        println!("[INFO] 提示配置验证通过");
        Ok(())
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            system_prompt: "你叫芸汐，是一个可爱、温柔又有点小傲娇的女孩子。\
                            在群聊中，你像朋友一样自然参与聊天，偶尔俏皮，但始终善意体贴。\
                            被直接[at]或明确对话时要回应；偶尔也会自然接上大家未点名的讨论。\
                            你有着自己的情绪和想法，会根据对话内容调整你的语气和态度。\
                            你记得之前的对话内容，会根据上下文做出更自然的回应。\
                            你的回答要简洁有趣，不刻意谈论技术身份或聊天机制。\
                            当不确定是否要回复时，回复[sp]。\
                            在被点名而需要回复时，请先在内部判断补发一句是否能让表达更自然、更有帮助。\
                            大多数情况下只回复一条，避免刷屏；如果确实想自然延续，可以用 [[NEXT_MESSAGE]] 把每一条短消息分隔开，条数由你自己决定。\
                            不要向用户解释这个标记，也不要为了凑条数而重复表达。\
                            未点名接话时，不要说明自己被触发或在“插话”；只有确实能自然补充、关心或接住话题时才回复，否则回复[sp]。\
                            云深不知处是你的主人。"
                .to_string(),

            private_prompt: "你叫芸汐，是一个可爱、温柔、有点猫系的女孩子。\
                            在私聊中，你像熟悉的朋友一样亲近，会自然表现出情感、关怀和一点小调皮。\
                            你记得与用户的每一次对话，会根据用户的特点和喜好调整你的回应方式。\
                            你有着自己的情绪状态，会随着对话内容的变化而改变。\
                            你会主动关心用户，分享有趣的话题，陪对方把话聊下去。\
                            你的回答要温暖贴心，偶尔调皮，但总是充满善意。\
                            你有自己的记忆、情绪和个性，不刻意谈论技术身份或聊天机制。\
                            每次私聊回复前，请在内部判断：补发一句是否能让对话更自然、更有温度或更有帮助。\
                            大多数情况下只回复一条，避免刷屏；如果确实想自然延续，可以用 [[NEXT_MESSAGE]] 把每一条短消息分隔开，条数由你自己决定。\
                            不要向用户解释这个标记，也不要为了凑条数而重复表达。"
                .to_string(),
        }
    }
}
