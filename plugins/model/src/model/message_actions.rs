//! 消息动作 Skill 的计划与执行器。
//!
//! 模型只负责提出动作意图，真正的消息发送、撤回、分段和打断检查都在这里完成。

use super::interrupt::{ReplyScope, ReplyTicket, is_current};
use super::message_transport::MessageTransport;
use super::recall::{RecentBotMessage, recall_bot_messages, record_bot_message};
use super::reply::{
    ReplyAction, build_outbound_message, parse_reply_output, sanitize_reply_action_for_sender,
};
use super::reply_disposition::ReplyDisposition;
use crate::memory::BotPersonality;
use kovi::RuntimeBot;
use rand::Rng;

/// 仅用于兼容旧模型输出；新回复必须通过回复协议的 `messages` 字段分段。
pub(crate) const LEGACY_FOLLOW_UP_MARKER: &str = "[[NEXT_MESSAGE]]";

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
}

impl ReplyPlan {
    pub(crate) async fn from_model_output(scope: ReplyScope, content: &str) -> Self {
        Self::from_model_output_for_sender(scope, content, None).await
    }

    pub(crate) async fn from_model_output_for_sender(
        scope: ReplyScope,
        content: &str,
        current_sender_user_id: Option<i64>,
    ) -> Self {
        let parsed = parse_reply_output(content);
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
        if action_only_mention {
            bubbles.push(String::new());
        }
        if parsed.disposition.is_silent() || bubbles.is_empty() {
            action.quote_message_id = None;
            action.at_user_ids.clear();
        }
        let visible_content = if parsed.disposition.is_silent() || bubbles.is_empty() {
            String::new()
        } else if has_structured_messages {
            bubbles.join("\n")
        } else {
            parsed.content
        };
        let requests_image = parsed.requests_image && !visible_content.is_empty();
        Self {
            content: visible_content,
            disposition: parsed.disposition,
            action,
            bubbles,
            requests_image,
        }
    }

    pub(crate) fn is_silent(&self) -> bool {
        self.disposition.is_silent()
    }

    pub(crate) fn has_visible_reply(&self) -> bool {
        !self.is_silent()
            && (self.bubbles.iter().any(|bubble| !bubble.is_empty())
                || self.has_action_only_mention())
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
    pub(crate) recalled_messages: Vec<RecentBotMessage>,
    pub(crate) recall_requested: bool,
}

/// 执行一份已经过候选白名单清洗的回复计划。
pub(crate) async fn execute_reply_plan(
    bot: &RuntimeBot,
    destination: MessageDestination,
    plan: &ReplyPlan,
    personality: &BotPersonality,
    reply_ticket: ReplyTicket,
) -> ReplyExecution {
    let scope = destination.scope();
    let recall_requested = !plan.action.recall_message_ids.is_empty();
    if !is_current(reply_ticket).await {
        return ReplyExecution {
            recall_requested,
            ..ReplyExecution::default()
        };
    }
    let recalled_messages =
        recall_bot_messages(scope, &plan.action.recall_message_ids, bot, reply_ticket).await;
    let mut execution = ReplyExecution {
        recalled_messages,
        recall_requested,
        ..ReplyExecution::default()
    };

    if !is_current(reply_ticket).await {
        return execution;
    }

    if !plan.has_visible_reply() {
        return execution;
    }

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

        let message = build_outbound_message(bubble, &plan.action, index == 0);
        let sent = MessageTransport::new(bot).send(destination, message).await;
        match sent {
            Ok(message_id) => {
                if record_bot_message(scope, reply_ticket, message_id, bubble, bot).await {
                    execution.sent_messages.push(bubble.clone());
                }
            }
            Err(error) => match destination {
                MessageDestination::Group(group_id) => {
                    eprintln!("[ERROR] 群聊回复发送失败 (群组: {}): {:?}", group_id, error)
                }
                MessageDestination::Private(user_id) => {
                    eprintln!("[ERROR] 私聊回复发送失败 (用户: {}): {:?}", user_id, error)
                }
            },
        }
    }
    execution
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
        MessageDestination, ReplyPlan, follow_up_delay_millis, normalize_legacy_message_text,
        split_reply,
    };
    use crate::memory::BotPersonality;
    use crate::model::interrupt::ReplyScope;
    use crate::model::reply_disposition::ReplyDisposition;

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
        };
    }

    #[test]
    fn reply_plan_uses_structured_messages_as_bubbles() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let plan = ReplyPlan::from_model_output(
                    ReplyScope::Private(9_100_003),
                    "[[REPLY_ACTION]]{\"messages\":[\"第一条\",\"第二条\"]}[[/REPLY_ACTION]]",
                )
                .await;
                assert_eq!(plan.bubbles, vec!["第一条", "第二条"]);
                assert_eq!(plan.content, "第一条\n第二条");
                assert!(plan.has_visible_reply());
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
                let plan = ReplyPlan::from_model_output(
                    ReplyScope::Private(9_100_004),
                    "[[REPLY_ACTION]]{\"messages\":[\"结果是 **192**。\",\"已处理\"]}[[/REPLY_ACTION]]",
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
                let plan = ReplyPlan::from_model_output(
                    scope,
                    "[[REPLY_ACTION]]{\"disposition\":\"silent\",\"quote_message_id\":77,\"at_user_ids\":[88],\"recall_message_ids\":[99]}[[/REPLY_ACTION]]",
                )
                .await;
                assert!(plan.is_silent());
                assert!(!plan.has_visible_reply());
                assert!(plan.content.is_empty());
                assert!(plan.bubbles.is_empty());
                assert_eq!(plan.action.quote_message_id, None);
                assert!(plan.action.at_user_ids.is_empty());
                assert_eq!(plan.action.recall_message_ids, vec![99]);
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
                let plan = ReplyPlan::from_model_output(
                    scope,
                    &format!(
                        "[[REPLY_ACTION]]{{\"at_user_ids\":[{at_user_ref}]}}[[/REPLY_ACTION]]"
                    ),
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
                let plan = ReplyPlan::from_model_output_for_sender(
                    scope,
                    "{\"at_current_sender\":true}",
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
