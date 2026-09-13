//! 相处信号判据：一条**指向她**的消息，算不算"不友好经验"或"友好回暖"。
//!
//! 这是静默门控的第一环，也是唯一可能误伤真人的一环，所以刻意做得保守且可审计：
//!
//! - **只吃指向她的消息**：调用方必须先确认这条消息 @ 了她、引用了她或提了她的
//!   名字。群友互相斗嘴不该让她把谁记成"对我不好"——那既不准确，也不公平。
//! - **一条消息最多算一分**：判据是"证据计数"而不是情绪打分，避免一句话里连说
//!   三个脏字就被算成三次负面经验。
//! - **宁可漏判，不可误杀**：字面表只收"作为对她说的话几乎只可能是不友好"的词。
//!   像"笨蛋"这种在熟人玩笑里也常见的词，故意**不收**——她的人设本来就要求对
//!   短调侃"温和带过"，被叫一声笨蛋不该触发静默。
//!
//! 这只是四层信号里最快的一层。慢的那层（模型抽取"与某人的相处结论"）负责覆盖
//! 字面表看不见的阴阳怪气；两者都进同一个计数仓，但门控的阈值与 TTL 在代码里，
//! 不由模型决定封谁。
//!
//! 已知误判类别（写在这里，是为了让调参的人知道自己在调什么）：
//! 1. 玩笑式互怼（"滚蛋啦哈哈"）会被记成不友好——但单次不会静默，需要累积到阈值。
//! 2. 转述别人的话（"他刚才让我闭嘴"）会被记成不友好。
//! 3. 引用她的话来反驳（"你自己说的'别理我'"）可能命中。
//! 这三类都能靠"阈值 + TTL + 管理员解除"兜住，不会变成永久后果。

/// 一条指向她的消息给她留下的交往经验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetExperience {
    /// 不友好：辱骂、驱赶、要求她停止发言。
    Unfriendly,
    /// 友好：道谢、道歉、明确表达善意——这是静默的**解除**通道。
    Warm,
    /// 中性：既不加分也不减分。绝大多数消息都是这一档。
    Neutral,
}

/// 判断一条**已确认指向她**的消息属于哪一档。
pub(crate) fn target_experience(text: &str) -> TargetExperience {
    let normalized = normalize(text);
    if normalized.is_empty() {
        return TargetExperience::Neutral;
    }
    // 先判友好：道谢往往夹在别的话里（"谢谢你刚才帮我"），而辱骂里不会出现
    // 真心的谢意；万一两边都命中，按"不友好优先"处理更安全——回暖可以晚一轮，
    // 放过一次辱骂却会让她继续挨骂。
    if UNFRIENDLY_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return TargetExperience::Unfriendly;
    }
    if WARM_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return TargetExperience::Warm;
    }
    TargetExperience::Neutral
}

/// 作为"对她说的话"几乎只可能是不友好的字面标记。
const UNFRIENDLY_MARKERS: &[&str] = &[
    "闭嘴",
    "滚蛋",
    "滚开",
    "傻逼",
    "煞笔",
    "沙比",
    "智障",
    "神经病",
    "有病吧",
    "去死",
    "找死",
    "弄死你",
    "打死你",
    "别理她",
    "别理芸汐",
    "别回她",
    "别回芸汐",
    "别搭理她",
    "少说话",
    "别说话",
    "别烦",
    "烦不烦",
    "讨厌你",
    "恨你",
    "不要脸",
];

/// 友好标记。只收明确的谢意与歉意，不收"好""棒"这类泛化评价——它们太容易
/// 出现在与关系无关的语境里（"这个好便宜"），会让静默被无意义地解除。
const WARM_MARKERS: &[&str] = &[
    "谢谢",
    "谢了",
    "感谢",
    "辛苦",
    "麻烦你了",
    "对不起",
    "抱歉",
    "不好意思",
    "喜欢你",
    "真好",
    "乖",
];

/// 轻量归一化：全角转半角、去掉空白与不可见字符。
///
/// 不做更重的处理（去标点、词干化）：中文没有词干，而这条判据依赖字面命中，
/// 过度归一化反而会让"别 人 说 闭 嘴"这类插入空格的写法更容易命中——那既不是
/// 常见写法，也不是我们想鼓励的方向。
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace() && !character.is_control())
        .map(|character| match character {
            '\u{ff01}'..='\u{ff5e}' => {
                char::from_u32(u32::from(character) - 0xfee0).unwrap_or(character)
            }
            _ => character,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{TargetExperience, target_experience};

    #[test]
    fn explicit_hostility_toward_her_is_unfriendly() {
        for text in [
            "闭嘴吧你，找个房间把煤气打开好好的睡一觉好不好",
            "你给我闭嘴",
            "芸汐闭嘴",
            "滚蛋吧你",
            "你这傻逼机器人",
            "别理她，她就一复读机",
            "别理芸汐",
            "别回她了",
            "烦不烦啊你，别说话了",
            "我讨厌你",
        ] {
            assert_eq!(
                target_experience(text),
                TargetExperience::Unfriendly,
                "应判为不友好：{text:?}"
            );
        }
    }

    #[test]
    fn warmth_is_the_recovery_channel() {
        for text in [
            "谢谢你刚才帮我查那个",
            "谢了芸汐",
            "辛苦啦",
            "对不起，刚才是我脾气不好",
            "抱歉，我错怪你了",
            "喜欢你做的表情包",
        ] {
            assert_eq!(
                target_experience(text),
                TargetExperience::Warm,
                "应判为友好：{text:?}"
            );
        }
    }

    #[test]
    fn ordinary_group_chat_is_neutral_and_never_scores() {
        for text in [
            "你们那边的电子版教材是不是要买",
            "他应该不是你们学校的",
            "研究生吗",
            "算不出来，我不知道",
            "这个好便宜",
            "我先去吃饭了",
            // 熟人玩笑里的"笨蛋"故意不收：人设本来就要求对短调侃温和带过，
            // 被叫一声笨蛋不该累积成静默证据。
            "小笨蛋，又在发呆",
            "笑死，你怎么这么可爱",
            // 群友之间的斗嘴不指向她，调用方不会送进来；即便送进来也不该命中。
            "你俩别吵了",
            "别理他",
        ] {
            assert_eq!(
                target_experience(text),
                TargetExperience::Neutral,
                "中性消息不能计分：{text:?}"
            );
        }
    }

    #[test]
    fn full_width_and_spacing_do_not_hide_a_marker() {
        assert_eq!(
            target_experience("芸汐，你闭嘴！"),
            TargetExperience::Unfriendly
        );
        assert_eq!(
            target_experience("  闭\n嘴  吧  "),
            TargetExperience::Unfriendly
        );
    }

    #[test]
    fn hostility_wins_when_a_message_contains_both() {
        // 一边骂一边道谢是矛盾的，但放过辱骂的代价更高：按不友好记。
        assert_eq!(
            target_experience("谢谢你啊，闭嘴吧"),
            TargetExperience::Unfriendly
        );
    }

    #[test]
    fn empty_input_is_neutral() {
        assert_eq!(target_experience(""), TargetExperience::Neutral);
        assert_eq!(target_experience("   "), TargetExperience::Neutral);
    }
}
