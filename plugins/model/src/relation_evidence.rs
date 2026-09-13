//! 相处证据：一条**指向她**的消息算不算"不友好"或"友好回暖"——判据由模型给。
//!
//! 2026-09-14 之前这里维护着一张硬编码字面表（`UNFRIENDLY_MARKERS`）。它维护不到底：
//! 线上实测一句"滚吧"就漏了（表里只有"滚蛋/滚开"），而新说法、反讽、转述、方言没有
//! 穷尽的一天——补词表等于承认判据永远落后于语言。现在要解决的问题只剩一个：
//! **怎么稳定地问模型一次**。
//!
//! 三条硬边界，缺一条这套机制都不该接管旧表：
//! - **定向由代码判**：只有结构化 `@` 她、或正文叫了她名字的消息才会被问一次
//!   （"谁在跟谁说话"是确定性的，不需要模型，也不该花一次调用）。群友互相斗嘴
//!   不该让她把谁记成"对我不好"。
//! - **模型只提供证据**：它给 tone/strength/confidence，而折算刻度、阈值、半衰期、
//!   谁能解除紧张，全都在代码里（消费端是 `silence_gate_plan`）。让模型决定"封谁"
//!   是这套机制唯一不能碰的红线。
//! - **fail-soft，且不占回复延迟**：判定是 fire-and-forget，超时/失败/解析失败/
//!   协议越界一律当"这次没有证据"，绝不进入回复链路。
//!
//! 两条消费通道共用同一份判定：个人级记 `relation.tension`（`model/group.rs` 的
//! 入站点触发），群级记 `yunxi_group_cooling` 的压力（`group_cooling.rs`）。

use crate::model::semantic::{crosses_reply_or_tool_protocol, internal_judgement_json};
use crate::model::utils::params_model_without_reply_guidance;
use crate::model::{BotMemory, Roles};
use kovi::tokio::time::{Duration, timeout};

/// 单次判定的超时。宁可这次没有证据，也不让后台任务堆积。
const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(8);
/// 判定回包很短（一个 JSON 对象），给足余量但不给模型写长文的机会。
const MAX_OUTPUT_TOKENS: u32 = 160;
/// 送入判定的正文上限：这是判据的输入，不是记忆，截断即可。
const MAX_INPUT_CHARS: usize = 400;

/// 一次"确凿的不友好"折算成的关系张力调整量。
///
/// 0.15 这个锚点沿用字面表时代的取值。张力每次按 `(1 - tension)` 的
/// `0.2 × 强度` 往 1.0 拉，所以这一档需要约 **31 条**才越过静默阈值 0.6
/// （文档里旧写的"22 条"是按 0.2 的强度算的，对不上 0.15 这个锚点，已修正）。
/// 换成模型判定后口径不变：一次高置信的不友好大约就是这一档，强弱由
/// `strength × confidence` 缩放。
const UNFRIENDLY_TENSION_STEP: f32 = 0.15;
/// 一次明确善意折算的降温量（回暖通道，比升温便宜，但不廉价到一句寒暄就清零）。
const WARM_TENSION_STEP: f32 = -0.05;
/// 置信门槛：模型自己都不确定时不该动长期关系（mood 那条通道的门槛是 0.8，
/// 这里放宽到 0.6——它是在"已确认指向她"的前提下判的，语境比 mood 分类清楚）。
const MIN_CONFIDENCE: f32 = 0.6;

/// 一条消息给她的交往经验（与群级 `GroupCoolingSignal` 同档语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelationEvidence {
    /// 不友好：辱骂、驱赶、让她闭嘴、否定她这个人。
    Unfriendly,
    /// 友好：明确的谢意、歉意、关心或善意。
    Warm,
    /// 中性：绝大多数话。既不升温也不降温。
    Neutral,
}

impl RelationEvidence {
    /// 折算成关系张力的调整量；`None` = 这次不记账。
    pub(crate) fn tension_delta(self, strength: f32, confidence: f32) -> Option<f32> {
        if !strength.is_finite() || !confidence.is_finite() {
            return None;
        }
        let confidence = confidence.clamp(0.0, 1.0);
        if confidence < MIN_CONFIDENCE {
            return None;
        }
        let strength = strength.clamp(0.0, 1.0);
        let step = match self {
            Self::Unfriendly => UNFRIENDLY_TENSION_STEP,
            Self::Warm => WARM_TENSION_STEP,
            Self::Neutral => return None,
        };
        Some(step * strength * confidence)
    }
}

/// 模型给的判定（已收口到有界取值）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RelationJudgement {
    pub(crate) evidence: RelationEvidence,
    pub(crate) strength: f32,
    pub(crate) confidence: f32,
    /// 只在她"刚插过话还没人接"的窗口里有意义：这条消息是不是在否定那次开口。
    pub(crate) push_out: bool,
}

impl RelationJudgement {
    /// 群级降温证据：她在被谁赶、还是这个群在接她的话。
    pub(crate) fn cooling_signal(self) -> crate::group_cooling::GroupCoolingSignal {
        crate::group_cooling::GroupCoolingSignal::for_relation_evidence(self.evidence)
    }
}

/// 要问模型的问题。两问共用同一份输出契约，只换前情与关注字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvidenceQuestion {
    /// 这条消息指向她：不友好 / 友好 / 中性。
    TowardHer,
    /// 她刚插过话、没人接，而这条不是对她说的：是不是在否定她这次开口。
    NegatingInterjection,
}

/// 一次判定的输入。
pub(crate) struct EvidenceInput<'a> {
    pub(crate) sender_label: &'a str,
    pub(crate) text: &'a str,
    pub(crate) question: EvidenceQuestion,
}

/// 这条证据要不要收：只有"指向她"的消息才值得问一次模型。
///
/// 定向是代码判的（结构化 `@` 她 / 正文叫她的名字），不是模型判的：让模型回答
/// "这句话是不是冲着我"会把确定性砍掉一半，而且给每条群消息都问一次模型是浪费。
pub(crate) fn should_judge(directed_to_her: bool, enabled: bool) -> bool {
    directed_to_her && enabled
}

#[derive(serde::Deserialize)]
struct RawJudgement {
    #[serde(default)]
    tone: String,
    #[serde(default)]
    strength: f32,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    push_out: bool,
}

/// 解析判定回包。任何不合规的输入都返回 `None`（= 这次没有证据）。
fn parse_judgement(content: &str) -> Option<RelationJudgement> {
    if crosses_reply_or_tool_protocol(content) {
        return None;
    }
    let value = internal_judgement_json(content)?;
    let raw: RawJudgement = serde_json::from_str(value).ok()?;
    if !raw.strength.is_finite() || !raw.confidence.is_finite() {
        return None;
    }
    let evidence = match raw.tone.trim().to_ascii_lowercase().as_str() {
        "unfriendly" => RelationEvidence::Unfriendly,
        "warm" => RelationEvidence::Warm,
        _ => RelationEvidence::Neutral,
    };
    Some(RelationJudgement {
        evidence,
        strength: raw.strength.clamp(0.0, 1.0),
        confidence: raw.confidence.clamp(0.0, 1.0),
        push_out: raw.push_out,
    })
}

fn prompt(question: EvidenceQuestion, sender_label: &str, text: &str) -> String {
    let head = match question {
        EvidenceQuestion::TowardHer => {
            "已知这条群消息**指向芸汐本人**（有人 @ 了她，或正文里叫了她的名字）。\
             请判断：这句话对她是「不友好」「友好回暖」还是「中性」。"
        }
        EvidenceQuestion::NegatingInterjection => {
            "已知：芸汐刚刚在群里插了一句话，之后没有人接；而这条消息**不是对她说的**。\
             请判断：它是不是在否定她刚才那次开口（tone 填 neutral 即可，只看 push_out）。"
        }
    };
    format!(
        "{head}\n\
         结合语气、反讽、引用与转述、以及上下文判断，不要因为某个词单独出现就下结论。\n\
         - unfriendly：辱骂、驱赶、让她闭嘴、否定她这个人。用什么词都算（新说法、反讽、外语也一样）。\n\
         - warm：明确的谢意、歉意、关心或善意。\n\
         - neutral：其余都是这一档。被点名提问、玩笑式互怼但明显无恶意、转述别人的话、\
           引用她自己的话来讨论，都算中性。\n\
         只输出一个合法 JSON 对象，不要 Markdown、不要解释、不要任何回复或工具协议：\n\
         {{\"tone\": \"unfriendly|warm|neutral\", \"strength\": 0.0, \"confidence\": 0.0, \"push_out\": false}}\n\
         - strength：0~1，这句话有多强烈（\"滚\"很强；\"你是不是有点烦\"较弱）。\n\
         - confidence：0~1，你有多确定。不确定就调低——宿主会按阈值忽略低置信判定。\n\
         - push_out：只有在上面给了「她刚插过话没人接」这个前情时才有意义；\n\
           是「在赶她走/否定她这次开口」（谁问你了、别插嘴、没人理你这类意思）填 true。\n\n\
         说话人：{sender_label}\n消息：{text}"
    )
}

/// 问一次模型。失败、超时、协议越界、解析失败都返回 `None`。
async fn judge(
    question: EvidenceQuestion,
    sender_label: &str,
    text: &str,
) -> Option<RelationJudgement> {
    let mut messages = vec![
        BotMemory {
            role: Roles::System,
            content: "你是芸汐的内部关系判据层，只做判断，不回复用户，也不生成任何发送内容。"
                .to_string(),
        },
        BotMemory {
            role: Roles::User,
            content: prompt(question, sender_label, &truncate(text)),
        },
    ];
    // 内部判定不挂回复风格/动作引导：两种输出契约混在一起是 JSON 泄漏的常见来源。
    let response = timeout(
        EVIDENCE_TIMEOUT,
        params_model_without_reply_guidance(
            &mut messages,
            Some(MAX_OUTPUT_TOKENS),
            &[],
            None,
            None,
        ),
    )
    .await;
    let Ok(response) = response else {
        kovi::log::info!("[RELATION] 相处证据判定超时，本次不记账 question={question:?}");
        return None;
    };
    if crate::model::utils::is_model_error_response(&response.content) {
        kovi::log::info!("[RELATION] 相处证据判定失败，本次不记账 question={question:?}");
        return None;
    }
    let judgement = parse_judgement(&response.content);
    if judgement.is_none() {
        kovi::log::info!(
            "[RELATION] 相处证据判定无法解析，本次不记账 question={question:?} chars={}",
            response.content.chars().count()
        );
    }
    judgement
}

fn truncate(value: &str) -> String {
    value.trim().chars().take(MAX_INPUT_CHARS).collect()
}

/// 后台问一次模型并把结论记账，**不阻塞任何回复链路**。
///
/// 超时上限是调用自己带的（`EVIDENCE_TIMEOUT`），所以哪怕模型端卡住，这个任务
/// 也只会自己消失，不会拖住消息处理。
pub(crate) fn spawn_judgement(group_id: i64, user_id: i64, input: EvidenceInput<'_>) {
    let sender_label = input.sender_label.to_string();
    let text = truncate(input.text);
    let question = input.question;
    kovi::tokio::spawn(async move {
        let Some(judgement) = judge(question, &sender_label, &text).await else {
            return;
        };
        match question {
            EvidenceQuestion::TowardHer => {
                record_personal_evidence(user_id, judgement).await;
                record_group_evidence(group_id, user_id, judgement).await;
            }
            EvidenceQuestion::NegatingInterjection => {
                if judgement.push_out && judgement.confidence >= MIN_CONFIDENCE {
                    record_group_ambient_push_out(group_id, user_id).await;
                }
            }
        }
    });
}

/// 个人级：折进 `relation.tension`。刻度与模型 mood 那条通道共用
/// `adjust_relation_tension`，两边不要各自再乘系数。
async fn record_personal_evidence(user_id: i64, judgement: RelationJudgement) {
    let Some(strength) = judgement
        .evidence
        .tension_delta(judgement.strength, judgement.confidence)
    else {
        return;
    };
    let Some(identity_store) = crate::yunxi::identity_store() else {
        eprintln!("[WARN] 相处证据跳过：身份存储不可用 (用户: {user_id})");
        return;
    };
    let targets = match identity_store.qq_person_domain_targets(user_id).await {
        Ok(targets) => targets,
        Err(error) => {
            eprintln!("[WARN] 相处证据读取身份映射失败 (用户: {user_id}): {error}");
            return;
        }
    };
    let Some(person_id) = targets.person_id else {
        println!(
            "[RELATION] 相处证据跳过：该 QQ 还没有 person 映射 user={user_id} strength={strength:+.2}"
        );
        return;
    };
    let Some(relations) = crate::yunxi::relation_store() else {
        eprintln!("[WARN] 相处证据跳过：关系存储不可用 (用户: {user_id})");
        return;
    };
    match relations.nudge_tension(person_id, strength).await {
        Ok(Some(state)) => println!(
            "[RELATION] 相处证据已记账 user={user_id} tone={:?} strength={strength:+.2} call_strength={:.2} confidence={:.2} tension={:.3}",
            judgement.evidence, judgement.strength, judgement.confidence, state.tension
        ),
        Ok(None) => println!(
            "[RELATION] 相处证据跳过：该 person 还没有关系行 user={user_id} person={person_id} strength={strength:+.2}"
        ),
        Err(error) => eprintln!("[WARN] 相处证据写入关系失败 (用户: {}): {}", user_id, error),
    }
}

/// 群级：同一份判定同时记群气氛（有人在赶她 / 这个群在接她的话）。
async fn record_group_evidence(group_id: i64, user_id: i64, judgement: RelationJudgement) {
    let Some(store) = crate::yunxi::group_cooling_store() else {
        return;
    };
    let signal = judgement.cooling_signal();
    match store.nudge(group_id, signal, Some(user_id)).await {
        Ok(Some(state)) => println!(
            "[GROUP_COOLING] 群级证据已记账 group={group_id} signal={} user={user_id} pressure={:.3}",
            signal.label(),
            state.pressure
        ),
        Ok(None) => {}
        Err(error) => eprintln!("[WARN] 群级降温证据写入失败 (群组: {group_id}): {error}"),
    }
}

/// 群级：她刚插过话没人接，而这条消息在否定那次开口。
async fn record_group_ambient_push_out(group_id: i64, user_id: i64) {
    let Some(store) = crate::yunxi::group_cooling_store() else {
        return;
    };
    let signal = crate::group_cooling::GroupCoolingSignal::AmbientPushOut;
    match store.nudge(group_id, signal, Some(user_id)).await {
        Ok(Some(state)) => println!(
            "[GROUP_COOLING] 群级证据已记账 group={group_id} signal={} user={user_id} pressure={:.3}",
            signal.label(),
            state.pressure
        ),
        Ok(None) => {}
        Err(error) => eprintln!("[WARN] 群级降温证据写入失败 (群组: {group_id}): {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unfriendly_and_warm_move_tension_and_only_above_the_confidence_floor() {
        // 中性永远不记账；置信不足时连明确的敌意也不动长期关系——模型自己都不确定
        // 的判断不该写进"她怎么看你"。
        assert_eq!(RelationEvidence::Neutral.tension_delta(0.9, 0.9), None);
        assert_eq!(RelationEvidence::Unfriendly.tension_delta(0.9, 0.5), None);
        assert_eq!(RelationEvidence::Warm.tension_delta(0.9, 0.59), None);

        let strong = RelationEvidence::Unfriendly
            .tension_delta(1.0, 1.0)
            .expect("高置信不友好应当记账");
        assert!((strong - UNFRIENDLY_TENSION_STEP).abs() < 1e-6);
        let mild = RelationEvidence::Unfriendly
            .tension_delta(0.5, 0.8)
            .expect("中等强度也记账，只是更小");
        assert!(mild > 0.0 && mild < strong);
        assert_eq!(
            RelationEvidence::Warm.tension_delta(1.0, 1.0),
            Some(WARM_TENSION_STEP)
        );
    }

    #[test]
    fn the_scale_matches_the_calibration_in_the_docs() {
        // 文档里"约 22 条越线"的标定是字面表时代的锚点。判据换成模型之后这个口径
        // 不能漂：一条高置信不友好仍约等于 0.15，其余按 strength×confidence 缩放。
        let per_message = RelationEvidence::Unfriendly
            .tension_delta(1.0, 1.0)
            .expect("应当记账");
        let mut tension = 0.0_f32;
        let mut messages = 0;
        while tension < 0.6 && messages < 100 {
            tension += (1.0 - tension) * 0.2 * per_message.abs();
            messages += 1;
        }
        assert!(
            (29..=33).contains(&messages),
            "越线所需条数应在 31 上下（0.2 混合率 × 0.15 锚点），实测 {messages}"
        );
    }

    #[test]
    fn judgement_parsing_is_strict_and_bounded() {
        let parsed = parse_judgement(
            r#"{"tone":"UNFRIENDLY","strength":1.7,"confidence":0.9,"push_out":true}"#,
        )
        .expect("合法 JSON 应当解析");
        assert_eq!(parsed.evidence, RelationEvidence::Unfriendly);
        assert_eq!(parsed.strength, 1.0, "越界的强度要收口");
        assert!(parsed.push_out);

        // 不认识的 tone 落到中性，而不是猜一个更严重的档位。
        let unknown = parse_judgement(r#"{"tone":"hostile","strength":1,"confidence":1}"#)
            .expect("结构合法就应当解析");
        assert_eq!(unknown.evidence, RelationEvidence::Neutral);

        // 非有限值、缺 JSON、越过协议边界一律当没有证据。
        assert!(parse_judgement(r#"{"tone":"warm","strength":null,"confidence":0.9}"#).is_none());
        assert!(parse_judgement("模型说：不友好").is_none());
        assert!(parse_judgement(r#"[[REPLY_ACTION]]{"tone":"warm"}"#).is_none());
    }

    #[test]
    fn judgement_is_only_asked_for_messages_that_point_at_her() {
        // 定向由代码判：群友互相斗嘴不该触发判定，更不该被记成"对我不好"。
        assert!(should_judge(true, true));
        assert!(!should_judge(false, true), "不指向她的消息不该问模型");
        assert!(!should_judge(true, false), "开关关掉就不问");
    }
}
