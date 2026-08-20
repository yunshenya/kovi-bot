//! 用户撤回消息与芸汐回复的生命周期联动。

use super::interrupt::{ReplyScope, ReplyTicket, interrupt};
use kovi::PluginBuilder;
use kovi::RuntimeBot;
use kovi::event::NoticeEvent;
use kovi::tokio::sync::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

const RECALLED_MESSAGE_RETENTION: Duration = Duration::from_secs(60 * 15);

struct ActiveReply {
    ticket: ReplyTicket,
    source_message_ids: Vec<i32>,
    sent_message_ids: Vec<i32>,
}

struct ReplyLifecycle {
    active: Option<ActiveReply>,
    recalled_message_ids: HashMap<i32, Instant>,
    last_seen: Instant,
}

impl Default for ReplyLifecycle {
    fn default() -> Self {
        Self {
            active: None,
            recalled_message_ids: HashMap::new(),
            last_seen: Instant::now(),
        }
    }
}

static REPLY_LIFECYCLES: LazyLock<Mutex<HashMap<ReplyScope, ReplyLifecycle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 注册一轮回复。若输入消息已经撤回，则整轮回复直接失效。
pub(crate) async fn begin_reply(
    scope: ReplyScope,
    ticket: ReplyTicket,
    source_message_ids: Vec<i32>,
) -> bool {
    let mut lifecycles = REPLY_LIFECYCLES.lock().await;
    prune_lifecycles(&mut lifecycles);
    let lifecycle = lifecycles.entry(scope).or_default();
    lifecycle.last_seen = Instant::now();
    if source_message_ids
        .iter()
        .any(|message_id| lifecycle.recalled_message_ids.contains_key(message_id))
    {
        return false;
    }
    lifecycle.active = Some(ActiveReply {
        ticket,
        source_message_ids,
        sent_message_ids: Vec::new(),
    });
    true
}

pub(crate) async fn finish_reply(scope: ReplyScope, ticket: ReplyTicket) {
    let mut lifecycles = REPLY_LIFECYCLES.lock().await;
    if let Some(lifecycle) = lifecycles.get_mut(&scope) {
        if lifecycle
            .active
            .as_ref()
            .is_some_and(|active| active.ticket == ticket)
        {
            lifecycle.active = None;
        }
        lifecycle.last_seen = Instant::now();
    }
}

/// 记录芸汐实际发出的消息。撤回事件与发送请求竞速时，已经发出的消息会立即补撤。
pub(crate) async fn record_bot_message(
    scope: ReplyScope,
    ticket: ReplyTicket,
    message_id: i32,
    bot: &RuntimeBot,
) {
    let should_delete = {
        let mut lifecycles = REPLY_LIFECYCLES.lock().await;
        if let Some(lifecycle) = lifecycles.get_mut(&scope) {
            lifecycle.last_seen = Instant::now();
            if let Some(active) = lifecycle.active.as_mut()
                && active.ticket == ticket
                && !active
                    .source_message_ids
                    .iter()
                    .any(|source_id| lifecycle.recalled_message_ids.contains_key(source_id))
            {
                active.sent_message_ids.push(message_id);
                false
            } else {
                true
            }
        } else {
            true
        }
    };
    if should_delete {
        bot.delete_msg(message_id);
    }
}

pub(crate) async fn has_recalled_messages(scope: ReplyScope, message_ids: &[i32]) -> bool {
    let lifecycles = REPLY_LIFECYCLES.lock().await;
    lifecycles.get(&scope).is_some_and(|lifecycle| {
        message_ids
            .iter()
            .any(|message_id| lifecycle.recalled_message_ids.contains_key(message_id))
    })
}

/// 注册撤回消息，并取消对应的模型请求、思考提示和后续气泡。
pub(crate) async fn handle_recalled_message(
    scope: ReplyScope,
    message_id: i32,
    bot: Arc<RuntimeBot>,
) {
    let (ticket, sent_message_ids) = {
        let mut lifecycles = REPLY_LIFECYCLES.lock().await;
        prune_lifecycles(&mut lifecycles);
        let lifecycle = lifecycles.entry(scope).or_default();
        lifecycle.last_seen = Instant::now();
        lifecycle
            .recalled_message_ids
            .insert(message_id, Instant::now());

        match lifecycle.active.take() {
            Some(active) if active.source_message_ids.contains(&message_id) => {
                (Some(active.ticket), active.sent_message_ids)
            }
            Some(active) => {
                lifecycle.active = Some(active);
                (None, Vec::new())
            }
            None => (None, Vec::new()),
        }
    };

    if ticket.is_some() {
        interrupt(scope).await;
    }
    for sent_message_id in sent_message_ids {
        bot.delete_msg(sent_message_id);
    }
}

/// Kovi 0.12 的撤回通知是通用 NoticeEvent，需要从原始 OneBot JSON 中提取消息 ID。
pub(crate) async fn recall_notice_event(event: Arc<NoticeEvent>) {
    let Some((scope, message_id)) = parse_recall_notice(&event) else {
        return;
    };
    let bot = PluginBuilder::get_runtime_bot();
    handle_recalled_message(scope, message_id, bot).await;
}

fn parse_recall_notice(event: &NoticeEvent) -> Option<(ReplyScope, i32)> {
    let message_id = event
        .get("message_id")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())?;
    match event.notice_type.as_str() {
        "group_recall" => Some((
            ReplyScope::Group(event.get("group_id")?.as_i64()?),
            message_id,
        )),
        "friend_recall" | "private_recall" => Some((
            ReplyScope::Private(event.get("user_id")?.as_i64()?),
            message_id,
        )),
        _ => None,
    }
}

fn prune_lifecycles(lifecycles: &mut HashMap<ReplyScope, ReplyLifecycle>) {
    let now = Instant::now();
    for lifecycle in lifecycles.values_mut() {
        lifecycle
            .recalled_message_ids
            .retain(|_, seen_at| now.duration_since(*seen_at) < RECALLED_MESSAGE_RETENTION);
    }
    if lifecycles.len() > 2_048 {
        lifecycles.retain(|_, lifecycle| {
            lifecycle.active.is_some()
                || now.duration_since(lifecycle.last_seen) < RECALLED_MESSAGE_RETENTION
        });
    }
}

#[cfg(test)]
mod tests {
    use super::parse_recall_notice;
    use kovi::event::NoticeEvent;
    use kovi::serde_json::{Value, json};

    fn notice(notice_type: &str, extra: Value) -> NoticeEvent {
        let mut value = json!({
            "time": 1,
            "self_id": 2,
            "post_type": "notice",
            "notice_type": notice_type,
            "message_id": 77,
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().expect("测试字段应为对象").clone());
        NoticeEvent {
            time: 1,
            self_id: 2,
            post_type: kovi::event::PostType::Notice,
            notice_type: notice_type.to_string(),
            original_json: value,
        }
    }

    #[test]
    fn parses_group_recall() {
        let parsed = parse_recall_notice(&notice("group_recall", json!({"group_id": 123})))
            .expect("应识别群撤回");
        assert_eq!(parsed.0, super::ReplyScope::Group(123));
        assert_eq!(parsed.1, 77);
    }

    #[test]
    fn parses_private_recall() {
        let parsed = parse_recall_notice(&notice("friend_recall", json!({"user_id": 456})))
            .expect("应识别私聊撤回");
        assert_eq!(parsed.0, super::ReplyScope::Private(456));
        assert_eq!(parsed.1, 77);
    }
}
