//! 消息动作 Skill 的计划与执行器。
//!
//! 模型只负责提出动作意图，真正的消息发送、分段和打断检查都在这里完成。
//!
//! 撤回不在这里：它是 `message.recall` 工具，在模型循环里就地执行（见 `tool_access.rs`）。

use super::interrupt::{
    OutgoingSource, ReplyScope, ReplyTicket, begin_outgoing_commit,
    contextual_outgoing_fingerprint, is_current, mark_outgoing_failed,
    prepare_outgoing_with_semantic_preview,
};
use super::message_transport::MessageTransport;
use super::recall::record_committed_bot_message;
use super::reply::{
    ReplyAction, ReplyTurn, build_outbound_message, parse_reply_output,
    sanitize_reply_action_for_sender,
};
use super::reply_disposition::ReplyDisposition;
use crate::group_access;
use crate::memory::BotPersonality;
use kovi::{Message, RuntimeBot};
use rand::RngExt;

/// 仅用于兼容旧模型输出；新回复必须通过回复协议的 `messages` 字段分段。
pub(crate) const LEGACY_FOLLOW_UP_MARKER: &str = "[[NEXT_MESSAGE]]";

/// Character-bigram set of a text, ignoring punctuation and whitespace.
fn text_bigrams(text: &str) -> std::collections::HashSet<(char, char)> {
    text.chars()
        .filter(|character| !is_ignorable_punct(*character))
        .collect::<Vec<_>>()
        .windows(2)
        .map(|window| (window[0], window[1]))
        .collect()
}

/// Rough "same idea" measure between two texts using character-bigram Jaccard
/// overlap. Used only to collapse a reply's redundant bubbles (复读), never to
/// decide which distinct ideas to keep.
fn text_overlap(left: &str, right: &str) -> f64 {
    let left = text_bigrams(left);
    let right = text_bigrams(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count();
    let union = left.union(&right).count();
    intersection as f64 / union.max(1) as f64
}

fn is_ignorable_punct(character: char) -> bool {
    character.is_whitespace()
        || character.is_ascii_punctuation()
        || matches!(
            character,
            '。' | '，'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | '…'
                | '—'
                | '～'
                | '·'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '（'
                | '）'
                | '《'
                | '》'
                | '「'
                | '」'
                | '『'
                | '』'
        )
}

/// Whether two bubbles of the same reply restate one idea.
///
/// Core's plain-turn contract accepts a model-declared second bubble; this is
/// the host-side guard that keeps that permission from producing an immediate
/// 复读. It measures how much of the *shorter* bubble is covered by the longer
/// one: a restatement with a few extra characters is still a restatement,
/// while a second bubble that mostly carries new information survives.
pub(crate) fn bubbles_are_near_duplicates(left: &str, right: &str) -> bool {
    const THRESHOLD: f64 = 0.6;
    let left = text_bigrams(left);
    let right = text_bigrams(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let intersection = left.intersection(&right).count() as f64;
    let smaller = left.len().min(right.len()) as f64;
    intersection / smaller.max(1.0) >= THRESHOLD
}

/// Merge adjacent bubbles that re-state the idea of the bubble right before
/// them, so a single reply does not send back-to-back near-identical messages
/// (复读). Distinct bubbles (e.g. an explicit "send these two different notes")
/// are preserved.
fn merge_near_duplicate_bubbles(bubbles: Vec<String>) -> Vec<String> {
    const THRESHOLD: f64 = 0.60;
    let mut merged: Vec<String> = Vec::new();
    for bubble in bubbles {
        let trimmed = bubble.trim();
        if trimmed.is_empty() {
            merged.push(bubble);
            continue;
        }
        if let Some(previous) = merged.last_mut()
            && !previous.trim().is_empty()
            && text_overlap(previous, trimmed) >= THRESHOLD
        {
            // Keep the more informative of the two near-identical ideas.
            if trimmed.chars().count() > previous.trim().chars().count() {
                *previous = trimmed.to_string();
            }
            continue;
        }
        merged.push(bubble);
    }
    merged
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageDestination {
    Group(i64),
    Private(i64),
}

impl MessageDestination {
    fn scope(self) -> ReplyScope {
        match self {
            Self::Group(group_id) => ReplyScope::Group(group_id),
            Self::Private(user_id) => ReplyScope::Private(user_id),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReplyPlan {
    pub(crate) content: String,
    pub(crate) disposition: ReplyDisposition,
    pub(crate) action: ReplyAction,
    pub(crate) bubbles: Vec<String>,
    pub(crate) requests_image: bool,
    /// 这一轮的正文用语音发出，而不是文字。
    pub(crate) voice: bool,
    /// 这一轮的正文用歌声发出（值 = 旋律模板 id）。
    ///
    /// 与 `voice` 互斥：歌声本身就是"说出来"，只是带旋律。
    pub(crate) sing: Option<String>,
    /// 这一轮随第一条气泡发一张素材库里的表情包（值 = 标签）。
    ///
    /// 与 `voice`/`sing` 互斥那两种整条替换的形态不同：表情包是**贴在气泡里**的，
    /// 正文照发。正文为空时它自己就是那条消息——"只回一张表情"是很正常的回复。
    pub(crate) sticker: Option<String>,
}

impl ReplyPlan {
    /// An empty visible-reply plan used when a plain-text completion did not
    /// produce usable text. Keeping the disposition as Reply lets the caller
    /// make one bounded repair attempt instead of confusing a model failure
    /// with an intentional silent decision.
    pub(crate) fn empty_reply() -> Self {
        Self {
            content: String::new(),
            disposition: ReplyDisposition::Reply,
            action: ReplyAction::default(),
            bubbles: Vec::new(),
            requests_image: false,
            voice: false,
            sing: None,
            sticker: None,
        }
    }

    /// A host-owned silent plan. No model output is parsed to construct it.
    #[allow(dead_code)]
    pub(crate) fn silent() -> Self {
        Self {
            content: String::new(),
            disposition: ReplyDisposition::Silent,
            action: ReplyAction::default(),
            bubbles: Vec::new(),
            requests_image: false,
            voice: false,
            sing: None,
            sticker: None,
        }
    }

    /// Build a visible reply from host-owned plain text bubbles.
    ///
    /// This is used by Core when it deliberately asks the model for one
    /// natural message at a time. The model never needs to know the reply
    /// action envelope; the host keeps ordering and delivery semantics here.
    pub(crate) fn from_plain_bubbles(_scope: ReplyScope, bubbles: Vec<String>) -> Option<Self> {
        let bubbles = bubbles
            .into_iter()
            .map(|bubble| bubble.trim().to_owned())
            .collect::<Vec<_>>();
        if bubbles.is_empty()
            || bubbles
                .iter()
                .any(|bubble| bubble.trim().is_empty() || bubble == "……")
        {
            return None;
        }
        let content = bubbles.join("\n");
        Some(Self {
            content,
            disposition: ReplyDisposition::Reply,
            action: ReplyAction::default(),
            bubbles,
            requests_image: false,
            voice: false,
            sing: None,
            sticker: None,
        })
    }

    pub(crate) async fn from_model_output(scope: ReplyScope, content: &str) -> Self {
        Self::from_reply_turn(scope, &ReplyTurn::plain(content), None).await
    }

    /// 把"模型这一轮的正文 + 它通过 `reply_action` 提交的动作"落成可执行计划。
    ///
    /// 结构化决策只来自 `turn.action`（provider 已按 schema 约束过的工具参数），正文
    /// 只当正文用；这里不再从正文里解析任何动作标记。
    pub(crate) async fn from_reply_turn(
        scope: ReplyScope,
        turn: &ReplyTurn,
        current_sender_user_id: Option<i64>,
    ) -> Self {
        let parsed = parse_reply_output(&turn.content, turn.action.as_ref());
        let mut action =
            sanitize_reply_action_for_sender(scope, parsed.action, current_sender_user_id).await;
        let has_structured_messages = parsed.messages.is_some();
        let mut bubbles = if parsed.disposition.is_silent() {
            Vec::new()
        } else if let Some(messages) = parsed.messages {
            sanitize_reply_sections(messages)
        } else if parsed.content.is_empty() {
            Vec::new()
        } else {
            split_reply(&parsed.content)
        };

        // A structured @ is itself a visible QQ message. Keep a single empty
        // bubble so the executor can send the at segment without inventing text.
        let action_only_mention =
            !parsed.disposition.is_silent() && bubbles.is_empty() && !action.at_user_ids.is_empty();
        // 只发一张表情包同样是可见回复：正文可以是空的，但必须有东西发出去。
        let sticker = parsed.sticker.filter(|_| !parsed.disposition.is_silent());
        if action_only_mention || (bubbles.is_empty() && sticker.is_some()) {
            bubbles.push(String::new());
        }
        if parsed.disposition.is_silent() || bubbles.is_empty() {
            action.quote_message_id = None;
            action.at_user_ids.clear();
        }
        // Collapse adjacent bubbles that merely re-state the same idea, so a
        // single reply does not read as 复读 (back-to-back near-identical texts).
        if !parsed.disposition.is_silent() {
            bubbles = merge_near_duplicate_bubbles(bubbles);
        }
        let visible_content = if parsed.disposition.is_silent() || bubbles.is_empty() {
            String::new()
        } else if has_structured_messages {
            bubbles.join("\n")
        } else {
            parsed.content
        };
        let requests_image = parsed.requests_image && !visible_content.is_empty();
        // 语音只对真实存在正文的轮次生效；空回复没有可读的内容。
        let voice = parsed.voice && !visible_content.is_empty() && !bubbles.is_empty();
        Self {
            content: visible_content,
            disposition: parsed.disposition,
            action,
            bubbles,
            requests_image,
            voice,
            sing: None,
            sticker,
        }
    }

    /// Intrinsic produces one bounded conversational turn for plain text. Keep
    /// explicit structured message bubbles intact so requests such as "send
    /// two messages" are delivered as separate QQ messages.
    pub(crate) async fn from_intrinsic_output(scope: ReplyScope, content: &str) -> Self {
        let has_structured_messages = parse_reply_output(content, None).messages.is_some();
        let mut plan = Self::from_model_output(scope, content).await;
        if plan.is_silent() || plan.bubbles.is_empty() {
            return plan;
        }
        if has_structured_messages {
            return plan;
        }
        let content = plan.bubbles.join("\n");
        plan.content = content.clone();
        plan.bubbles = vec![content];
        plan
    }

    pub(crate) fn is_silent(&self) -> bool {
        self.disposition.is_silent()
    }

    pub(crate) fn has_visible_reply(&self) -> bool {
        !self.is_silent()
            && (self.bubbles.iter().any(|bubble| !bubble.is_empty())
                || self.has_action_only_mention()
                || self.sticker.is_some())
    }

    fn has_action_only_mention(&self) -> bool {
        !self.is_silent()
            && self.content.is_empty()
            && self.bubbles.len() == 1
            && self.bubbles[0].is_empty()
            && !self.action.at_user_ids.is_empty()
    }
}

#[derive(Debug, Default)]
pub(crate) struct ReplyExecution {
    pub(crate) sent_messages: Vec<String>,
}

/// 执行一份已经过候选白名单清洗的回复计划。
pub(crate) async fn execute_reply_plan(
    bot: &RuntimeBot,
    destination: MessageDestination,
    plan: &ReplyPlan,
    personality: &BotPersonality,
    reply_ticket: ReplyTicket,
) -> ReplyExecution {
    // 撤回不再从这里走：它已经是 `message.recall` 工具，在模型循环里就地执行。
    // 这里只负责把可见回复提交出站并交给 QQ。
    let scope = destination.scope();
    let mut execution = ReplyExecution::default();
    if !is_current(reply_ticket).await {
        return execution;
    }

    if !plan.has_visible_reply() {
        return execution;
    }

    // 从这里开始是"交给 QQ 发送"：这一步与前面的生成/提交分开打点，卡住时后台
    // 能直接指出是卡在发送上（发送阶段有自己的预算，见 `message_transport`）。
    crate::model::waiting_room::TurnWatch::step(crate::model::waiting_room::TurnStep::Send);
    let voice_config = crate::config::get().qq_voice().clone();
    for (index, bubble) in plan.bubbles.iter().enumerate() {
        if !is_current(reply_ticket).await {
            break;
        }
        if index > 0 {
            kovi::tokio::time::sleep(follow_up_delay(personality, index)).await;
            if !is_current(reply_ticket).await {
                break;
            }
        }

        let first_message = index == 0;
        // 模型把这一轮标记成语音时改用 record 段；合成失败会回退成文字，
        // 语音只是表达方式，不该因为 TTS 抖动把回复弄丢。
        let voice_message = if plan.voice {
            crate::voice_reply::build_voice_message(&voice_config, bubble).await
        } else {
            None
        };
        // record 段整条替换消息，图片挂不上去：语音合成成功时不再附带表情包。
        let voiced = voice_message.is_some();
        let mut message = match voice_message {
            Some(voice) => voice,
            None => build_outbound_message(bubble, &plan.action, first_message),
        };
        // 表情包只在第一条气泡上，和正文同一条消息（QQ 允许文字与图片同气泡）；
        // 素材解析不到就退回纯文字——一张图发不出去不该把整条回复带走。
        if first_message
            && !voiced
            && let Some(label) = plan.sticker.as_deref()
        {
            match crate::sticker_library::build_sticker_segment(label) {
                Some(segment) => message.push(segment),
                None => kovi::log::warn!(
                    "表情包素材不可用，本轮只发文字: label={label} conversation={scope:?}"
                ),
            }
        }
        // 绝不发一条什么都没有的消息：只发一张表情的轮次在素材取不到时应当整条跳过，
        // 而不是变成一个空气泡。
        if !message.iter().any(|segment| {
            matches!(
                segment.type_.as_str(),
                "text" | "at" | "image" | "record" | "face" | "mface"
            )
        }) {
            kovi::log::warn!(
                "可见回复没有任何可发送内容，本轮跳过这条气泡: conversation={scope:?}"
            );
            continue;
        }
        let reply_to = first_message
            .then_some(plan.action.quote_message_id)
            .flatten()
            .map(i64::from);
        let mention_user_ids = if first_message {
            plan.action.at_user_ids.as_slice()
        } else {
            &[]
        };
        // 指纹代表"这一条要发出去的东西"：表情包也是内容的一部分，同文不同图
        // 不该被当成同一个信封（只发一张表情、正文为空时更是唯一的区分依据）。
        let fingerprint_content = match (first_message, plan.sticker.as_deref()) {
            (true, Some(label)) => format!("{bubble}\u{1f}{label}"),
            _ => bubble.clone(),
        };
        let fingerprint = contextual_outgoing_fingerprint(
            scope,
            &fingerprint_content,
            reply_to,
            mention_user_ids,
            None,
        );
        let Some(outgoing) = prepare_outgoing_with_semantic_preview(
            reply_ticket,
            fingerprint,
            OutgoingSource::Reply,
            Some(bubble),
        )
        .await
        else {
            break;
        };
        let Ok(precommit) = begin_outgoing_commit(outgoing).await else {
            mark_outgoing_failed(outgoing).await;
            break;
        };
        // 下面那次授权查询是租约里唯一可能变慢的 await：先续一次，把 30 秒从现在起算，
        // 免得"活着只是慢"被当成"进程死了"，让整条已经渲染好的回复被静默丢掉。
        if !precommit.renew().await {
            mark_outgoing_failed(outgoing).await;
            break;
        }
        let authorization = match destination {
            MessageDestination::Group(group_id) => {
                match group_access::authorize_group_send(group_id).await {
                    Ok(authorization) => Some(authorization),
                    Err(error) => {
                        mark_outgoing_failed(outgoing).await;
                        eprintln!(
                            "[WARN] 群聊回复在提交前失去授权 (群组: {}): {}",
                            group_id, error
                        );
                        break;
                    }
                }
            }
            MessageDestination::Private(_) => None,
        };
        let Ok(committed) = precommit.commit(fingerprint, None).await else {
            mark_outgoing_failed(outgoing).await;
            break;
        };
        drop(authorization);
        let sent = MessageTransport::new(bot).send(destination, message).await;
        match sent {
            Ok(message_id) => {
                committed.mark_sent().await;
                if record_committed_bot_message(scope, reply_ticket, message_id, bubble).await {
                    execution.sent_messages.push(bubble.clone());
                }
            }
            Err(error) => {
                if error.is_indeterminate() {
                    drop(committed);
                } else {
                    committed.mark_failed().await;
                }
                match destination {
                    MessageDestination::Group(group_id) => {
                        eprintln!("[ERROR] 群聊回复发送失败 (群组: {}): {:?}", group_id, error)
                    }
                    MessageDestination::Private(user_id) => {
                        eprintln!("[ERROR] 私聊回复发送失败 (用户: {}): {:?}", user_id, error)
                    }
                }
            }
        }
    }
    execution
}

/// Send one host-owned text message as part of an existing reactive reply.
/// Fallbacks and progress notices use this path so they cross the same
/// Prepared -> Committed boundary as model-generated reply bubbles.
pub(crate) async fn send_tracked_reply_text(
    bot: &RuntimeBot,
    destination: MessageDestination,
    content: &str,
    reply_ticket: ReplyTicket,
) -> bool {
    if content.is_empty() || !is_current(reply_ticket).await {
        return false;
    }
    let scope = destination.scope();
    let fingerprint = contextual_outgoing_fingerprint(scope, content, None, &[], None);
    let Some(outgoing) = prepare_outgoing_with_semantic_preview(
        reply_ticket,
        fingerprint,
        OutgoingSource::Reply,
        Some(content),
    )
    .await
    else {
        return false;
    };
    let Ok(precommit) = begin_outgoing_commit(outgoing).await else {
        mark_outgoing_failed(outgoing).await;
        return false;
    };
    // 同上：授权查询之前续租，把租约锚在这一步的起点。
    if !precommit.renew().await {
        mark_outgoing_failed(outgoing).await;
        return false;
    }
    let authorization = match destination {
        MessageDestination::Group(group_id) => {
            match group_access::authorize_group_send(group_id).await {
                Ok(authorization) => Some(authorization),
                Err(error) => {
                    mark_outgoing_failed(outgoing).await;
                    eprintln!(
                        "[WARN] 群聊提示在提交前失去授权 (群组: {}): {}",
                        group_id, error
                    );
                    return false;
                }
            }
        }
        MessageDestination::Private(_) => None,
    };
    let Ok(committed) = precommit.commit(fingerprint, None).await else {
        mark_outgoing_failed(outgoing).await;
        return false;
    };
    drop(authorization);
    match MessageTransport::new(bot)
        .send(destination, Message::from(content.to_owned()))
        .await
    {
        Ok(message_id) => {
            committed.mark_sent().await;
            record_committed_bot_message(scope, reply_ticket, message_id, content).await
        }
        Err(error) => {
            if error.is_indeterminate() {
                drop(committed);
            } else {
                committed.mark_failed().await;
            }
            match destination {
                MessageDestination::Group(group_id) => {
                    eprintln!("[ERROR] 群聊回复发送失败 (群组: {}): {:?}", group_id, error)
                }
                MessageDestination::Private(user_id) => {
                    eprintln!("[ERROR] 私聊回复发送失败 (用户: {}): {:?}", user_id, error)
                }
            }
            false
        }
    }
}

pub(crate) fn split_reply(content: &str) -> Vec<String> {
    let marked_sections = content
        .split(LEGACY_FOLLOW_UP_MARKER)
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if marked_sections.len() > 1 {
        return sanitize_reply_sections(marked_sections);
    }

    let Some(reply) = marked_sections.into_iter().next() else {
        return vec!["……".to_string()];
    };

    sanitize_reply_sections(vec![reply])
}

pub(crate) fn normalize_legacy_message_text(content: &str) -> String {
    strip_markdown_bold_markers(&content.replace(LEGACY_FOLLOW_UP_MARKER, "\n"))
}

fn sanitize_reply_sections(sections: Vec<String>) -> Vec<String> {
    sections
        .into_iter()
        .map(|section| strip_markdown_bold_markers(&strip_leading_stage_directions(&section)))
        .collect()
}

fn strip_markdown_bold_markers(content: &str) -> String {
    content.replace("**", "")
}

fn strip_leading_stage_directions(content: &str) -> String {
    let mut text = content.trim();

    while let Some(rest) = strip_one_leading_bracketed_note(text) {
        text = rest.trim_start_matches(|character: char| {
            character.is_whitespace() || matches!(character, '，' | ',' | '。' | '：' | ':')
        });
    }

    if text.is_empty() {
        "……".to_string()
    } else {
        text.to_string()
    }
}

fn strip_one_leading_bracketed_note(text: &str) -> Option<&str> {
    let (open, close) = if text.starts_with('[') {
        ('[', ']')
    } else if text.starts_with('【') {
        ('【', '】')
    } else {
        return None;
    };

    let after_open = &text[open.len_utf8()..];
    let close_index = after_open.find(close)?;
    if after_open[..close_index].trim().is_empty() {
        return None;
    }
    Some(&after_open[close_index + close.len_utf8()..])
}

fn follow_up_delay(personality: &BotPersonality, message_index: usize) -> std::time::Duration {
    let variation_ms = rand::rng().random_range(-200_i64..=450_i64);
    std::time::Duration::from_millis(follow_up_delay_millis(
        personality,
        message_index,
        variation_ms,
    ))
}

pub(crate) fn follow_up_delay_millis(
    personality: &BotPersonality,
    message_index: usize,
    variation_ms: i64,
) -> u64 {
    let mood_base_ms = match personality.current_mood.as_str() {
        "excited" => 280,
        "playful" => 380,
        "happy" => 480,
        "curious" | "confident" => 560,
        "neutral" => 800,
        "calm" => 1_100,
        "thoughtful" => 1_450,
        "shy" | "lonely" => 1_600,
        "angry" => 1_500,
        "sad" => 1_800,
        _ => 800,
    };
    let energy_adjustment_ms = (5_i64 - i64::from(personality.energy_level)) * 45;
    let confidence_adjustment_ms = (5_i64 - i64::from(personality.social_confidence)) * 25;
    let intensity_adjustment_ms = match personality.current_mood.as_str() {
        "excited" | "playful" | "happy" if personality.mood_intensity >= 7 => -120,
        "sad" | "shy" | "thoughtful" if personality.mood_intensity >= 7 => 160,
        _ => 0,
    };
    let sequence_adjustment_ms = (message_index.saturating_sub(1).min(6) as i64) * 70;
    (mood_base_ms
        + energy_adjustment_ms
        + confidence_adjustment_ms
        + intensity_adjustment_ms
        + sequence_adjustment_ms
        + variation_ms)
        .clamp(180, 4_000) as u64
}

#[cfg(test)]
mod tests {
    use super::{
        MessageDestination, ReplyPlan, bubbles_are_near_duplicates, follow_up_delay_millis,
        normalize_legacy_message_text, split_reply,
    };
    use crate::memory::BotPersonality;
    use crate::model::interrupt::ReplyScope;
    use crate::model::reply::{ReplyActionCall, ReplyTurn};
    use crate::model::reply_disposition::ReplyDisposition;
    use serde_json::{Value, json};

    /// 造一轮"正文 + `reply_action` 工具参数"的模型产物。
    fn reply_turn(content: &str, arguments: Value) -> ReplyTurn {
        let call = ReplyActionCall::from_tool_arguments(
            arguments.as_object().expect("测试参数必须是对象"),
        )
        .expect("测试参数应当通过校验");
        ReplyTurn {
            content: content.to_string(),
            action: Some(call),
        }
    }

    #[test]
    fn bubble_duplicate_guard_keeps_distinct_and_rejects_restatements() {
        // A restatement with a few extra characters is still 复读.
        assert!(bubbles_are_near_duplicates(
            "今天降温了，记得多穿点。",
            "今天降温了，记得要多穿点。"
        ));
        assert!(bubbles_are_near_duplicates("我先去吃饭啦", "我先去吃饭了"));
        // A second bubble that mostly carries new information must survive,
        // even when it reuses the opening words.
        assert!(!bubbles_are_near_duplicates(
            "今天降温了，记得多穿点。",
            "晚上可能下雨，你带伞了吗？"
        ));
        assert!(!bubbles_are_near_duplicates("好", "那你早点休息"));
        // Degenerate input never collapses a bubble on its own.
        assert!(!bubbles_are_near_duplicates("", ""));
        assert!(!bubbles_are_near_duplicates("？！", "。"));
    }

    #[test]
    fn reply_plan_keeps_bubbles_and_destination_scope_is_stable() {
        let personality = BotPersonality::default();
        assert_eq!(MessageDestination::Group(12).scope(), ReplyScope::Group(12));
        assert_eq!(
            MessageDestination::Private(34).scope(),
            ReplyScope::Private(34)
        );
        assert_eq!(split_reply("第一句\n第二句"), vec!["第一句\n第二句"]);
        assert!(follow_up_delay_millis(&personality, 1, 0) > 0);
        let _ = ReplyPlan {
            content: "你好".to_string(),
            disposition: ReplyDisposition::Reply,
            action: Default::default(),
            bubbles: vec!["你好".to_string()],
            requests_image: false,
            voice: false,
            sing: None,
            sticker: None,
        };
    }

    #[test]
    fn collapses_near_duplicate_bubbles_but_keeps_distinct() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                // Distinct notes remain two separate bubbles.
                let distinct = ReplyPlan::from_reply_turn(
                    ReplyScope::Private(9_100_020),
                    &reply_turn("", json!({"messages": ["第一条", "第二条"]})),
                    None,
                )
                .await;
                assert_eq!(distinct.bubbles, vec!["第一条", "第二条"]);

                // Near-identical re-statements collapse into one bubble (复读 guard).
                let repeated = ReplyPlan::from_reply_turn(
                    ReplyScope::Private(9_100_021),
                    &reply_turn(
                        "",
                        json!({"messages": [
                            "哈哈，姜冷笑话管够，素材库都快告急了。",
                            "哈哈，姜冷笑话管够，素材库快告急啦～"
                        ]}),
                    ),
                    None,
                )
                .await;
                assert_eq!(repeated.bubbles.len(), 1);
            });
    }

    #[test]
    fn plain_bubbles_build_a_host_owned_plan_without_protocol_parsing() {
        let plan = ReplyPlan::from_plain_bubbles(
            ReplyScope::Private(9_100_012),
            vec!["第一条 **重点**".to_owned(), "[轻声]第二条".to_owned()],
        )
        .expect("plain bubbles should become a visible plan");
        assert_eq!(plan.bubbles, vec!["第一条 **重点**", "[轻声]第二条"]);
        assert_eq!(plan.content, "第一条 **重点**\n[轻声]第二条");
        assert_eq!(plan.disposition, ReplyDisposition::Reply);
        assert!(plan.action.quote_message_id.is_none());
        assert!(
            ReplyPlan::from_plain_bubbles(
                ReplyScope::Private(9_100_012),
                vec!["第一条".to_owned(), "".to_owned(),]
            )
            .is_none()
        );
        assert!(!ReplyPlan::empty_reply().has_visible_reply());
        assert!(ReplyPlan::silent().is_silent());
    }

    #[test]
    fn reply_plan_uses_structured_messages_as_bubbles() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let plan = ReplyPlan::from_reply_turn(
                    ReplyScope::Private(9_100_003),
                    &reply_turn("", json!({"messages": ["第一条", "第二条"]})),
                    None,
                )
                .await;
                assert_eq!(plan.bubbles, vec!["第一条", "第二条"]);
                assert_eq!(plan.content, "第一条\n第二条");
                assert!(plan.has_visible_reply());
            });
    }

    #[test]
    fn intrinsic_output_collapses_legacy_separator_to_one_logical_bubble() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let plan = ReplyPlan::from_intrinsic_output(
                    ReplyScope::Private(9_100_010),
                    "第一段 [[NEXT_MESSAGE]] 第二段",
                )
                .await;
                assert_eq!(plan.bubbles, vec!["第一段\n第二段"]);
                assert_eq!(plan.content, "第一段\n第二段");
            });
    }

    /// intrinsic 那条路只吃自然语言正文：旧的动作标记被剥掉，也不再能撑出多气泡。
    ///
    /// 多气泡现在只有一条来源——模型通过 `reply_action` 工具提交的 `messages`。
    #[test]
    fn intrinsic_output_no_longer_reads_a_structured_message_batch() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let plan = ReplyPlan::from_intrinsic_output(
                    ReplyScope::Private(9_100_011),
                    "[[REPLY_ACTION]]{\"messages\":[\"第一条\",\"第二条\"]}[[/REPLY_ACTION]]",
                )
                .await;
                assert!(!plan.has_visible_reply(), "标记之后的动作文本不算可见正文");

                let with_body = ReplyPlan::from_intrinsic_output(
                    ReplyScope::Private(9_100_011),
                    "先说一句[[REPLY_ACTION]]{\"messages\":[\"第一条\"]}[[/REPLY_ACTION]]",
                )
                .await;
                assert_eq!(with_body.bubbles, vec!["先说一句"]);
            });
    }

    #[test]
    fn visible_replies_do_not_expose_markdown_bold_markers() {
        assert_eq!(
            normalize_legacy_message_text("结果是 **192**。"),
            "结果是 192。"
        );
        assert_eq!(split_reply("结果是 **192**。"), vec!["结果是 192。"]);

        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let plan = ReplyPlan::from_reply_turn(
                    ReplyScope::Private(9_100_004),
                    &reply_turn("", json!({"messages": ["结果是 **192**。", "已处理"]})),
                    None,
                )
                .await;
                assert_eq!(plan.bubbles, vec!["结果是 192。", "已处理"]);
            });
    }

    #[test]
    fn structured_silence_keeps_recall_but_drops_visible_actions() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_100_001);
                crate::model::reply::record_reply_target(
                    scope,
                    77,
                    Some(88),
                    "测试用户",
                    "测试消息",
                )
                .await;
                let plan = ReplyPlan::from_reply_turn(
                    scope,
                    &reply_turn(
                        "",
                        json!({
                            "disposition": "silent",
                            "quote_message_id": 77,
                            "at_user_ids": [88],
                        }),
                    ),
                    None,
                )
                .await;
                assert!(plan.is_silent());
                assert!(!plan.has_visible_reply());
                assert!(plan.content.is_empty());
                assert!(plan.bubbles.is_empty());
                assert_eq!(plan.action.quote_message_id, None);
                assert!(plan.action.at_user_ids.is_empty());
            });
    }

    #[test]
    fn action_only_mention_is_a_sendable_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_100_008);
                let at_user_ref =
                    crate::model::reply::register_mention_target(scope, 8_765_432_113, "当前成员")
                        .await;
                let plan = ReplyPlan::from_reply_turn(
                    scope,
                    &reply_turn("", json!({"at_user_ids": [at_user_ref]})),
                    None,
                )
                .await;

                assert!(plan.has_visible_reply());
                assert_eq!(plan.bubbles, vec![String::new()]);
                assert!(plan.content.is_empty());
                assert_eq!(plan.action.at_user_ids, vec![8_765_432_113]);
                crate::model::reply::clear_reply_targets(scope).await;
            });
    }

    #[test]
    fn action_only_current_sender_mention_is_a_sendable_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_100_009);
                let plan = ReplyPlan::from_reply_turn(
                    scope,
                    &reply_turn("", json!({"at_current_sender": true})),
                    Some(8_765_432_114),
                )
                .await;

                assert!(plan.has_visible_reply());
                assert_eq!(plan.bubbles, vec![String::new()]);
                assert!(plan.content.is_empty());
                assert_eq!(plan.action.at_user_ids, vec![8_765_432_114]);
            });
    }
}
