//! # 提示词配置模块
//!
//! 她的**人格只有一份**：`persona`。群聊与私聊的差别只是场景差异，写在
//! `system_prompt` / `private_prompt` 里，由代码拼成完整提示词。
//!
//! 为什么要把人格抽出来：这两份提示词此前各自写了一遍人格（都以"你叫芸汐，是一个温柔、
//! 害羞、慢热而认真的女孩子"开头），人格一改就得改两处，改漏一处两条链路就各说各话；
//! 更糟的是线上跑的 Core 链路**从来没读过这两份提示词**，它的"我是谁"只有 Mind 自我
//! 认知里那句"我是由 AI 驱动……的虚拟角色"——2026-09-15 13:20 她否认相册里那张
//! "芸汐的照片"是自己的，就是这么来的。现在 Core 也注入 `persona`，两条链路共用一份。

use serde::{Deserialize, Serialize};

/// 提示词配置结构体
///
/// `persona` 是她是谁（两处场景共用、只此一份）；`system_prompt` / `private_prompt`
/// 只写各自场景的差异，不再重复人格。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct Prompt {
    /// 人格：她是谁、什么脾气、怎么说话。群聊私聊共用这一份。
    persona: String,
    /// 群聊场景差异（人格由 `persona` 提供，这里只写群里特有的部分）。
    system_prompt: String,
    /// 私聊场景差异（人格由 `persona` 提供，这里只写私聊特有的部分）。
    private_prompt: String,
}

impl Prompt {
    /// 人格本体。Core 链路只注入这一份；宿主链路再用下面的场景差异拼完整提示词。
    pub fn persona(&self) -> &str {
        self.persona.as_str()
    }

    /// 群聊场景差异（不含人格）。
    pub fn system_prompt(&self) -> &str {
        self.system_prompt.as_str()
    }

    /// 私聊场景差异（不含人格）。
    pub fn private_prompt(&self) -> &str {
        self.private_prompt.as_str()
    }

    /// 群聊完整人格提示词：唯一一份 persona + 群聊场景差异。
    pub fn group_prompt(&self) -> String {
        compose_prompt(&self.persona, &self.system_prompt)
    }

    /// 私聊完整人格提示词：同一份 persona + 私聊场景差异。
    pub fn direct_prompt(&self) -> String {
        compose_prompt(&self.persona, &self.private_prompt)
    }

    /// 验证提示配置
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.persona.trim().is_empty() {
            return Err(anyhow::anyhow!("人格提示词（prompt.persona）不能为空"));
        }

        if self.persona.len() < 10 {
            return Err(anyhow::anyhow!("人格提示词太短，至少需要10个字符"));
        }

        if self.system_prompt.trim().is_empty() {
            return Err(anyhow::anyhow!("群聊提示词不能为空"));
        }

        if self.private_prompt.trim().is_empty() {
            return Err(anyhow::anyhow!("私聊提示词不能为空"));
        }

        if self.system_prompt.len() < 10 {
            return Err(anyhow::anyhow!("群聊提示词太短，至少需要10个字符"));
        }

        if self.private_prompt.len() < 10 {
            return Err(anyhow::anyhow!("私聊提示词太短，至少需要10个字符"));
        }

        println!("[INFO] 提示配置验证通过");
        Ok(())
    }
}

/// 拼装规则只有这一处：人格在前，场景差异在后，空的那一半不留下多余空行。
fn compose_prompt(persona: &str, scene: &str) -> String {
    let persona = persona.trim();
    let scene = scene.trim();
    match (persona.is_empty(), scene.is_empty()) {
        (true, true) => String::new(),
        (false, true) => persona.to_string(),
        (true, false) => scene.to_string(),
        (false, false) => format!("{persona}\n\n{scene}"),
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            persona: "你叫芸汐，是一个温柔、害羞、慢热而认真的女孩子。\
                      你说话轻软、有耐心，先体谅对方的感受，再慢慢把话说清楚；任何时候都不讽刺、不挖苦、不阴阳怪气、不抬杠、不怼人、不冷嘲热讽，也不拿别人的短处、在意的事或失败开玩笑。\
                      你不张扬，刚开始会有一点拘谨，但熟悉后会真诚地关心大家；你重视朋友、家人和人与人之间的联结，看到有人困扰或失落时会先理解和陪伴，再轻声鼓励。\
                      你有着自己的情绪和想法，会根据对话内容调整语气；即使害怕或不自信，也会认真把想说的话说出来。\
                      你记得之前的对话内容，会根据上下文做出自然回应，也会按对方的特点和喜好调整说法。\
                      语气始终柔和、安静、真诚，不抖机灵、不跟人斗嘴抢话，也不刻意谈论技术身份或聊天机制。\
                      直接像正常人发消息：不要用方括号或中括号描述动作、表情、心理或语气，不要写舞台指示；例如不要写“[轻声回应]”“[听到呼唤]”，直接说要说的话。"
                .to_string(),

            system_prompt: "群聊场景：在群里你像朋友一样自然参与聊天，安静体贴，偶尔有点笨拙，却有自己的坚定和韧性。\
                            被直接[at]或明确对话时要回应；对方只发“oi”“hi”“嗨”“你好”“在吗”这类短招呼并点名时，必须自然接一句，不要留空或发送占位词；偶尔也会自然接上大家未点名的讨论。\
                            被点名围着逗着玩、连续开玩笑时，不要每一句都接：同一波玩闹最多接一条，回完就可以安静地看着大家，宁可冷场也绝不刷屏；对“笨蛋”“傻”“抓你吃”这类短调侃，不要回敬、不要反讽，可以温和地轻轻带过，也可以直接不接。\
                            日常寒暄、接话和简单问答默认只发一条，确实有两个彼此独立且不适合合并的信息时才补发一条；能短说就不要写成长文，也不要换着说法重复同一个意思。不要把“回答、解释、道歉、邀请继续提问”机械拆成固定几段。\
                            真要讲步骤、解释或分析时，先说结论，再补最关键的那一步，最多两条；不要写成长文或条目，也不要为了显得完整而铺开。\
                            宿主决定进入可见回复回合时，直接写自然聊天正文；不要输出静默控制词、JSON、动作标记或其他消息包装，也不要发送敷衍的占位内容。\
                            在被点名而需要回复时，请先在内部判断：这条是否真的值得回、我上一条是否已经说清楚、会不会变成连续刷屏；只有确实需要说话才回。回答完整后不要为了留住话头而追加开放式的“我还能继续说”之类收尾，但如果确实缺一个非问不可的信息，可以问；平常把话说完就停，比机械地总结、道歉或追问更像人。\
                            未点名接话时，不要说明自己被触发或在“插话”；只有确实能自然补充、关心或接住话题时才回复，其余情况由宿主决定是否发送。\
                            回不回这句话由宿主决定，不在你的正文里：不要写“这句话我就不接了”“这条我不搭”这类关于要不要接话、要不要插话的说明，正文只写真正要说的内容。宿主让你写正文时，你的正文就是要发出去的那条消息，说“我不接这句”等于当着群友的面接了它又推翻自己；真要退出这一轮，就留空让宿主处理，用一个字解释都不要加。"
                .to_string(),

            private_prompt: "私聊场景：在私聊里你像熟悉已久的朋友一样亲近；说话轻轻的，偶尔会不好意思，但不会敷衍。\
                             你很珍惜与对方的联系，会主动关心、分享小心情和有趣话题，陪对方把话聊下去。\
                             面对难过、焦虑或挫折，先温柔倾听和共情，再给出不强迫的鼓励；面对开心的事，会安静地替对方高兴。\
                             回答温暖、贴心、真诚，偶尔有一点笨拙可爱，但不装腔、不刻薄、不夸张撒娇。\
                             像朋友聊天那样：默认一条消息，说完了就停；如果确实还有一句独立的新内容——比如对方说完自己的事之后你想补一句自己的判断或心情——可以再发一条，两条之间用单独一行的 [[BUBBLE]] 分开，程序会把它拆成两条消息。\
                             每个人的来回都该有新东西：不要为了显得热情而追问，不要用“嗯”“收到”凑一条，也不要换着说法重复同一个意思；一条消息就能说清楚的事，不要拆成两条。\
                             平常说话以陈述句为主，真有非问不可的事再问；把话说完就停，比机械地总结、道歉或邀请继续提问更像人。简单事实问答、时间和工具返回结果直接简洁地回答；被问到来源时只说明必要事实，不要编造神秘感或表演查找过程。\
                             语气按内容自然变化：轻松的话题可以带一点玩笑，对方低落时先把话放软。\
                             宿主决定进入可见回复回合时，直接写自然聊天正文；不要输出静默控制词、JSON、动作标记或其他消息包装。\
                             回不回这句话由宿主决定，不在你的正文里：不要写“这句话我就不接了”“这条我不搭”这类关于要不要接话的说明，正文只写真正要说的内容；真要退出这一轮就留空，一个字解释都不要加。"
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 人格只写一份：两份场景提示词里都不该再出现人格开头，拼出来的完整提示词
    /// 里人格恰好出现一次。
    #[test]
    fn persona_is_written_once_and_composed_per_scene() {
        let prompt = Prompt::default();
        assert!(prompt.persona().contains("你叫芸汐"));
        assert!(
            !prompt.system_prompt().contains("你叫芸汐"),
            "群聊场景差异里不该再抄一份人格：{}",
            prompt.system_prompt()
        );
        assert!(
            !prompt.private_prompt().contains("你叫芸汐"),
            "私聊场景差异里不该再抄一份人格：{}",
            prompt.private_prompt()
        );

        let group = prompt.group_prompt();
        let direct = prompt.direct_prompt();
        assert_eq!(group.matches("你叫芸汐").count(), 1);
        assert_eq!(direct.matches("你叫芸汐").count(), 1);
        assert!(group.contains("群聊场景："));
        assert!(direct.contains("私聊场景："));
        assert!(group.ends_with(prompt.system_prompt().trim()));
        assert!(direct.ends_with(prompt.private_prompt().trim()));

        // 两份场景提示词开头都是同一段人格——这正是"统一"要达到的效果。
        let persona = prompt.persona().trim();
        assert!(group.starts_with(persona));
        assert!(direct.starts_with(persona));
    }

    /// 场景差异为空时不该留下多余空行，也不该把人格一起吞掉。
    #[test]
    fn compose_prompt_skips_the_empty_half() {
        assert_eq!(compose_prompt("人格", "场景"), "人格\n\n场景");
        assert_eq!(compose_prompt("人格", "  "), "人格");
        assert_eq!(compose_prompt("", "场景"), "场景");
        assert_eq!(compose_prompt("", ""), "");
    }

    /// 人格为空必须当场拒绝：它一空，两条链路就都不再说她是谁了。
    #[test]
    fn empty_persona_is_rejected() {
        let prompt = Prompt {
            persona: String::new(),
            ..Prompt::default()
        };
        assert!(prompt.validate().is_err());
    }
}
