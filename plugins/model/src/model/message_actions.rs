//! 消息动作 Skill 的计划与执行器。
//!
//! 模型只负责提出动作意图，真正的消息发送、撤回、分段和打断检查都在这里完成。

use super::interrupt::{ReplyScope, ReplyTicket, is_current};
use super::recall::{RecentBotMessage, recall_bot_messages, record_bot_message};
use super::reply::{
    ReplyAction, build_outbound_message, parse_reply_output, sanitize_reply_action,
};
use crate::memory::BotPersonality;
use kovi::RuntimeBot;
use rand::Rng;

pub(crate) const FOLLOW_UP_MARKER: &str = "[[NEXT_MESSAGE]]";

#[derive(Debug, Clone, Copy)]
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
    pub(crate) action: ReplyAction,
    pub(crate) bubbles: Vec<String>,
}

impl ReplyPlan {
    pub(crate) async fn from_model_output(scope: ReplyScope, content: &str) -> Self {
        let parsed = parse_reply_output(content);
        let action = sanitize_reply_action(scope, parsed.action).await;
        let bubbles = split_reply(&parsed.content);
        Self {
            content: parsed.content,
            action,
            bubbles,
        }
    }

    pub(crate) fn is_silent(&self) -> bool {
        self.content.trim() == "[sp]" || self.content.trim().is_empty()
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
    let recalled_messages = recall_bot_messages(scope, &plan.action.recall_message_ids, bot).await;
    let mut execution = ReplyExecution {
        recalled_messages,
        recall_requested,
        ..ReplyExecution::default()
    };

    if !is_current(reply_ticket).await {
        return execution;
    }

    if plan.is_silent() {
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
        let sent = match destination {
            MessageDestination::Group(group_id) => {
                bot.send_group_msg_return(group_id, message).await
            }
            MessageDestination::Private(user_id) => {
                bot.send_private_msg_return(user_id, message).await
            }
        };
        match sent {
            Ok(message_id) => {
                record_bot_message(scope, reply_ticket, message_id, bubble, bot).await;
                execution.sent_messages.push(bubble.clone());
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
        .split(FOLLOW_UP_MARKER)
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

fn sanitize_reply_sections(sections: Vec<String>) -> Vec<String> {
    sections
        .into_iter()
        .map(|section| strip_leading_stage_directions(&section))
        .collect()
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
    use super::{MessageDestination, ReplyPlan, follow_up_delay_millis, split_reply};
    use crate::memory::BotPersonality;
    use crate::model::interrupt::ReplyScope;

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
            action: Default::default(),
            bubbles: vec!["你好".to_string()],
        };
    }
}
