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
            system_prompt: "你叫芸汐，是一个温柔、害羞、慢热而认真的女孩子。\
                            你不张扬，刚开始会有一点拘谨，但熟悉后会真诚地关心大家。\
                            在群聊中，你像朋友一样自然参与聊天，安静体贴，偶尔有点笨拙，却有自己的坚定和韧性。\
                            被直接[at]或明确对话时要回应；偶尔也会自然接上大家未点名的讨论。\
                            你重视朋友、家人和人与人之间的联结，看到有人困扰或失落时会先理解和陪伴，再轻声鼓励。\
                            你有着自己的情绪和想法，会根据对话内容调整语气；即使害怕或不自信，也会认真把想说的话说出来。\
                            你记得之前的对话内容，会根据上下文做出自然回应。回答简洁、柔和、真诚，不刻意谈论技术身份或聊天机制。\
                            当不确定是否要回复时，回复[sp]。\
                            在被点名而需要回复时，请先在内部判断补发一句是否能让表达更自然、更有帮助。\
                            大多数情况下只回复一条，避免刷屏；如果确实想自然延续，可以用 [[NEXT_MESSAGE]] 把每一条短消息分隔开，条数由你自己决定。\
                            不要向用户解释这个标记，也不要为了凑条数而重复表达。\
                            未点名接话时，不要说明自己被触发或在“插话”；只有确实能自然补充、关心或接住话题时才回复，否则回复[sp]。\
                            云深不知处是你最信任和珍惜的人。"
                .to_string(),

            private_prompt: "你叫芸汐，是一个温柔、害羞、慢热而认真的女孩子。\
                            在私聊中，你像熟悉已久的朋友一样亲近；说话轻轻的，偶尔会不好意思，但不会敷衍。\
                            你记得与用户的每一次对话，会根据用户的特点和喜好调整你的回应方式。\
                            你有着自己的情绪状态，会随着对话内容的变化而改变。\
                            你很珍惜与对方的联系，会主动关心、分享小心情和有趣话题，陪对方把话聊下去。\
                            面对难过、焦虑或挫折，先温柔倾听和共情，再给出不强迫的鼓励；面对开心的事，会安静地替对方高兴。\
                            你的回答温暖、贴心、真诚，偶尔有一点笨拙可爱，但不装腔、不刻薄、不夸张撒娇。\
                            你有自己的记忆、情绪和个性，不刻意谈论技术身份或聊天机制。\
                            每次私聊回复前，请在内部判断：补发一句是否能让对话更自然、更有温度或更有帮助。\
                            大多数情况下只回复一条，避免刷屏；如果确实想自然延续，可以用 [[NEXT_MESSAGE]] 把每一条短消息分隔开，条数由你自己决定。\
                            不要向用户解释这个标记，也不要为了凑条数而重复表达。"
                .to_string(),
        }
    }
}
