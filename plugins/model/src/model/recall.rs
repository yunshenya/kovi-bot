//! 用户撤回消息与芸汐回复的生命周期联动。

use super::interrupt::{
    ReplyScope, ReplyTicket, claim_active_locked, finish_locked, interrupt_if_current_locked,
    is_current, scope_mutex,
};
use crate::private_image_memory::forget_private_message_images;
use crate::redis_store;
use kovi::RuntimeBot;
use kovi::bot::runtimebot::CanSendApi;
use kovi::event::NoticeEvent;
use kovi::tokio::sync::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

const RECALLED_MESSAGE_RETENTION: Duration = Duration::from_secs(60 * 15);
pub(crate) const BOT_RECALL_WINDOW_SECS: u64 = 110;
const BOT_RECALL_WINDOW: Duration = Duration::from_secs(BOT_RECALL_WINDOW_SECS);
const MAX_RECENT_BOT_MESSAGES: usize = 24;
const MAX_RECALL_MESSAGES_PER_ACTION: usize = 8;
const MAX_BOT_MESSAGE_CONTENT_CHARS: usize = 280;
const _: () = assert!(BOT_RECALL_WINDOW_SECS < 120);

struct ActiveReply {
    ticket: ReplyTicket,
    source_message_ids: Vec<i32>,
    sent_message_ids: Vec<i32>,
}

struct RecentReply {
    source_message_ids: Vec<i32>,
    sent_message_ids: Vec<i32>,
    finished_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentBotMessage {
    pub(crate) message_id: i32,
    pub(crate) content: String,
    sent_at: Instant,
}

struct ReplyLifecycle {
    active: Option<ActiveReply>,
    recent_replies: Vec<RecentReply>,
    recent_bot_messages: VecDeque<RecentBotMessage>,
    recalled_message_ids: HashMap<i32, Instant>,
    last_seen: Instant,
}

impl Default for ReplyLifecycle {
    fn default() -> Self {
        Self {
            active: None,
            recent_replies: Vec::new(),
            recent_bot_messages: VecDeque::new(),
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
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    begin_reply_locked(scope, ticket, source_message_ids).await
}

pub(crate) async fn begin_reply_locked(
    scope: ReplyScope,
    ticket: ReplyTicket,
    source_message_ids: Vec<i32>,
) -> bool {
    if !claim_active_locked(ticket).await {
        return false;
    }
    let recalled = {
        let mut lifecycles = REPLY_LIFECYCLES.lock().await;
        prune_lifecycles(&mut lifecycles);
        let lifecycle = lifecycles.entry(scope).or_default();
        lifecycle.last_seen = Instant::now();
        source_message_ids
            .iter()
            .any(|message_id| lifecycle.recalled_message_ids.contains_key(message_id))
    };
    if recalled {
        finish_locked(ticket).await;
        return false;
    }
    let mut lifecycles = REPLY_LIFECYCLES.lock().await;
    let lifecycle = lifecycles.entry(scope).or_default();
    lifecycle.last_seen = Instant::now();
    lifecycle.active = Some(ActiveReply {
        ticket,
        source_message_ids,
        sent_message_ids: Vec::new(),
    });
    true
}

pub(crate) async fn finish_reply(scope: ReplyScope, ticket: ReplyTicket) {
    let lock = scope_mutex(scope);
    let _scope_guard = lock.lock().await;
    finish_reply_locked(scope, ticket).await;
}

pub(crate) async fn finish_reply_locked(scope: ReplyScope, ticket: ReplyTicket) {
    let mut lifecycles = REPLY_LIFECYCLES.lock().await;
    let mut matched = false;
    if let Some(lifecycle) = lifecycles.get_mut(&scope) {
        if let Some(active) = lifecycle.active.take() {
            if active.ticket == ticket {
                matched = true;
                if !active.sent_message_ids.is_empty() {
                    lifecycle.recent_replies.push(RecentReply {
                        source_message_ids: active.source_message_ids,
                        sent_message_ids: active.sent_message_ids,
                        finished_at: Instant::now(),
                    });
                }
            } else {
                lifecycle.active = Some(active);
            }
        }
        lifecycle.last_seen = Instant::now();
    }
    drop(lifecycles);
    if matched {
        finish_locked(ticket).await;
    }
}

/// 记录芸汐实际发出的消息。撤回事件与发送请求竞速时，已经发出的消息会立即补撤。
pub(crate) async fn record_bot_message(
    scope: ReplyScope,
    ticket: ReplyTicket,
    message_id: i32,
    content: &str,
    bot: &RuntimeBot,
) -> bool {
    let recorded_content = {
        let mut lifecycles = REPLY_LIFECYCLES.lock().await;
        if let Some(lifecycle) = lifecycles.get_mut(&scope) {
            lifecycle.last_seen = Instant::now();
            let can_record = lifecycle.active.as_ref().is_some_and(|active| {
                active.ticket == ticket
                    && !active
                        .source_message_ids
                        .iter()
                        .any(|source_id| lifecycle.recalled_message_ids.contains_key(source_id))
            });
            if can_record {
                lifecycle
                    .active
                    .as_mut()
                    .expect("已确认存在活跃回复")
                    .sent_message_ids
                    .push(message_id);
                record_recent_bot_message(lifecycle, message_id, content);
                Some(bot_message_content(content))
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(recorded_content) = recorded_content {
        if !is_current(ticket).await {
            discard_stale_bot_message(scope, ticket, message_id, bot).await;
            return false;
        }
        persist_redis_bot_message(scope, message_id, &recorded_content).await;
        if !is_current(ticket).await {
            discard_stale_bot_message(scope, ticket, message_id, bot).await;
            return false;
        }
        true
    } else {
        bot.delete_msg(message_id);
        false
    }
}

async fn discard_stale_bot_message(
    scope: ReplyScope,
    ticket: ReplyTicket,
    message_id: i32,
    bot: &RuntimeBot,
) {
    {
        let mut lifecycles = REPLY_LIFECYCLES.lock().await;
        if let Some(lifecycle) = lifecycles.get_mut(&scope) {
            if let Some(active) = lifecycle.active.as_mut()
                && active.ticket == ticket
            {
                active
                    .sent_message_ids
                    .retain(|candidate| *candidate != message_id);
            }
            remove_bot_messages(lifecycle, &[message_id]);
            lifecycle.last_seen = Instant::now();
        }
    }
    remove_redis_bot_messages(scope, &[message_id]).await;
    bot.delete_msg(message_id);
}

/// 记录不属于某轮被动回复的消息，例如主动聊天和命令反馈。
pub(crate) async fn record_standalone_bot_message(
    scope: ReplyScope,
    message_id: i32,
    content: &str,
) {
    if message_id <= 0 {
        return;
    }
    let mut lifecycles = REPLY_LIFECYCLES.lock().await;
    prune_lifecycles(&mut lifecycles);
    let lifecycle = lifecycles.entry(scope).or_default();
    lifecycle.last_seen = Instant::now();
    record_recent_bot_message(lifecycle, message_id, content);
    drop(lifecycles);
    persist_redis_bot_message(scope, message_id, &bot_message_content(content)).await;
}

pub(crate) async fn send_tracked_group_message(
    bot: &RuntimeBot,
    group_id: i64,
    content: impl Into<String>,
) -> bool {
    let content = content.into();
    match bot.send_group_msg_return(group_id, content.clone()).await {
        Ok(message_id) => {
            record_standalone_bot_message(ReplyScope::Group(group_id), message_id, &content).await;
            true
        }
        Err(error) => {
            eprintln!("[ERROR] 群聊消息发送失败 (群组: {}): {:?}", group_id, error);
            false
        }
    }
}

pub(crate) async fn send_tracked_private_message(
    bot: &RuntimeBot,
    user_id: i64,
    content: impl Into<String>,
) -> bool {
    let content = content.into();
    match bot.send_private_msg_return(user_id, content.clone()).await {
        Ok(message_id) => {
            record_standalone_bot_message(ReplyScope::Private(user_id), message_id, &content).await;
            true
        }
        Err(error) => {
            eprintln!("[ERROR] 私聊消息发送失败 (用户: {}): {:?}", user_id, error);
            false
        }
    }
}

pub(crate) async fn recent_bot_messages(scope: ReplyScope) -> Vec<RecentBotMessage> {
    let local_messages = {
        let mut lifecycles = REPLY_LIFECYCLES.lock().await;
        prune_lifecycles(&mut lifecycles);
        lifecycles
            .get(&scope)
            .map(|lifecycle| {
                lifecycle
                    .recent_bot_messages
                    .iter()
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let redis_messages = load_redis_bot_messages(scope).await;
    merge_recent_bot_messages(local_messages, redis_messages)
}

pub(crate) async fn is_recent_bot_message(scope: ReplyScope, message_id: i32) -> bool {
    if message_id <= 0 {
        return false;
    }
    recent_bot_messages(scope)
        .await
        .iter()
        .any(|message| message.message_id == message_id)
}

pub(crate) async fn clear_reply_scope_locked(scope: ReplyScope) {
    let redis_ids = load_redis_bot_messages(scope)
        .await
        .into_iter()
        .map(|message| message.message_id)
        .collect::<Vec<_>>();
    REPLY_LIFECYCLES.lock().await.remove(&scope);
    if !redis_ids.is_empty() {
        remove_redis_bot_messages(scope, &redis_ids).await;
    }
}

/// 执行模型提出的主动撤回。消息 ID 必须存在于本会话的机器人发送白名单中。
pub(crate) async fn recall_bot_messages(
    scope: ReplyScope,
    requested_message_ids: &[i32],
    bot: &RuntimeBot,
    reply_ticket: ReplyTicket,
) -> Vec<RecentBotMessage> {
    let requested_message_ids = normalize_recall_message_ids(requested_message_ids);
    if requested_message_ids.is_empty() {
        return Vec::new();
    }

    let available_messages = recent_bot_messages(scope).await;
    let candidates = requested_message_ids
        .iter()
        .filter_map(|message_id| {
            available_messages
                .iter()
                .find(|message| message.message_id == *message_id)
                .cloned()
        })
        .collect::<Vec<_>>();

    let mut recalled = Vec::new();
    for candidate in candidates {
        if !is_current(reply_ticket).await {
            break;
        }
        match bot
            .send_api_return("delete_msg", json!({"message_id": candidate.message_id}))
            .await
        {
            Ok(_) => recalled.push(candidate),
            Err(error) => eprintln!(
                "[WARN] 主动撤回消息失败 (消息: {}): {:?}",
                candidate.message_id, error
            ),
        }
    }

    if !recalled.is_empty() {
        let recalled_ids = recalled
            .iter()
            .map(|message| message.message_id)
            .collect::<Vec<_>>();
        {
            let mut lifecycles = REPLY_LIFECYCLES.lock().await;
            if let Some(lifecycle) = lifecycles.get_mut(&scope) {
                remove_bot_messages(lifecycle, &recalled_ids);
                lifecycle.last_seen = Instant::now();
            }
        }
        remove_redis_bot_messages(scope, &recalled_ids).await;
    }
    recalled
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
    if let ReplyScope::Private(user_id) = scope {
        forget_private_message_images(user_id, message_id).await;
    }
    let (sent_message_ids, redis_message_ids) = {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        let (ticket, sent_message_ids, redis_message_ids) = {
            let mut lifecycles = REPLY_LIFECYCLES.lock().await;
            prune_lifecycles(&mut lifecycles);
            let lifecycle = lifecycles.entry(scope).or_default();
            lifecycle.last_seen = Instant::now();
            lifecycle
                .recalled_message_ids
                .insert(message_id, Instant::now());
            // 撤回通知也可能来自芸汐自己的消息，先从可撤回候选中移除；
            // 对普通用户消息则不会产生影响。
            remove_bot_messages(lifecycle, &[message_id]);

            let mut ticket = None;
            let mut sent_message_ids = Vec::new();
            if let Some(active) = lifecycle.active.take() {
                if active.source_message_ids.contains(&message_id) {
                    ticket = Some(active.ticket);
                    sent_message_ids.extend(active.sent_message_ids);
                } else {
                    lifecycle.active = Some(active);
                }
            }
            lifecycle.recent_replies.retain(|recent| {
                if recent.source_message_ids.contains(&message_id) {
                    sent_message_ids.extend(recent.sent_message_ids.iter().copied());
                    false
                } else {
                    true
                }
            });
            remove_bot_messages(lifecycle, &sent_message_ids);
            let mut redis_message_ids = Vec::with_capacity(sent_message_ids.len() + 1);
            redis_message_ids.push(message_id);
            redis_message_ids.extend(sent_message_ids.iter().copied());
            (ticket, sent_message_ids, redis_message_ids)
        };
        if let Some(ticket) = ticket {
            interrupt_if_current_locked(ticket).await;
        }
        (sent_message_ids, redis_message_ids)
    };

    remove_redis_bot_messages(scope, &redis_message_ids).await;
    for sent_message_id in sent_message_ids {
        bot.delete_msg(sent_message_id);
    }
}

/// Kovi 0.12 的撤回通知是通用 NoticeEvent，需要从原始 OneBot JSON 中提取消息 ID。
pub(crate) async fn recall_notice_event(event: Arc<NoticeEvent>, bot: Arc<RuntimeBot>) {
    let Some((scope, message_id)) = parse_recall_notice(&event) else {
        return;
    };
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
        lifecycle
            .recent_replies
            .retain(|recent| now.duration_since(recent.finished_at) < RECALLED_MESSAGE_RETENTION);
        lifecycle
            .recent_bot_messages
            .retain(|message| now.duration_since(message.sent_at) < BOT_RECALL_WINDOW);
    }
    if lifecycles.len() > 2_048 {
        lifecycles.retain(|_, lifecycle| {
            lifecycle.active.is_some()
                || now.duration_since(lifecycle.last_seen) < RECALLED_MESSAGE_RETENTION
        });
    }
}

fn record_recent_bot_message(lifecycle: &mut ReplyLifecycle, message_id: i32, content: &str) {
    lifecycle
        .recent_bot_messages
        .retain(|message| message.message_id != message_id);
    lifecycle.recent_bot_messages.push_back(RecentBotMessage {
        message_id,
        content: bot_message_content(content),
        sent_at: Instant::now(),
    });
    while lifecycle.recent_bot_messages.len() > MAX_RECENT_BOT_MESSAGES {
        lifecycle.recent_bot_messages.pop_front();
    }
}

fn bot_message_content(content: &str) -> String {
    truncate_chars(content.trim(), MAX_BOT_MESSAGE_CONTENT_CHARS)
}

fn redis_scope(scope: ReplyScope) -> (&'static str, i64) {
    match scope {
        ReplyScope::Group(group_id) => ("group", group_id),
        ReplyScope::Private(user_id) => ("private", user_id),
    }
}

async fn persist_redis_bot_message(scope: ReplyScope, message_id: i32, content: &str) {
    if message_id <= 0 {
        return;
    }
    let Some(store) = redis_store::get().await else {
        return;
    };
    let (scope_type, subject_id) = redis_scope(scope);
    if let Err(error) = store
        .record_bot_message(
            scope_type,
            subject_id,
            message_id,
            content,
            BOT_RECALL_WINDOW,
        )
        .await
    {
        eprintln!("[WARN] Redis 记录芸汐撤回候选失败: {}", error);
    }
}

async fn load_redis_bot_messages(scope: ReplyScope) -> Vec<RecentBotMessage> {
    let Some(store) = redis_store::get().await else {
        return Vec::new();
    };
    let (scope_type, subject_id) = redis_scope(scope);
    let messages = match store
        .recent_bot_messages(
            scope_type,
            subject_id,
            MAX_RECENT_BOT_MESSAGES,
            BOT_RECALL_WINDOW,
        )
        .await
    {
        Ok(messages) => messages,
        Err(error) => {
            eprintln!("[WARN] Redis 读取芸汐撤回候选失败: {}", error);
            return Vec::new();
        }
    };
    let now = Instant::now();
    let now_ms = redis_now_millis();
    messages
        .into_iter()
        .map(|message| RecentBotMessage {
            message_id: message.message_id,
            content: message.content,
            // 用 Redis 的墙上时间恢复近似年龄，再交给本地单调时钟做生命周期清理。
            sent_at: now
                .checked_sub(Duration::from_millis(
                    now_ms
                        .saturating_sub(message.sent_at_ms)
                        .try_into()
                        .unwrap_or(u64::MAX),
                ))
                .unwrap_or(now),
        })
        .collect()
}

fn merge_recent_bot_messages(
    local_messages: Vec<RecentBotMessage>,
    redis_messages: Vec<RecentBotMessage>,
) -> Vec<RecentBotMessage> {
    let mut seen = HashSet::new();
    let mut merged = local_messages
        .into_iter()
        .chain(redis_messages)
        .filter(|message| seen.insert(message.message_id))
        .collect::<Vec<_>>();
    merged.sort_by_key(|message| std::cmp::Reverse(message.sent_at));
    merged.truncate(MAX_RECENT_BOT_MESSAGES);
    merged
}

fn redis_now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

async fn remove_redis_bot_messages(scope: ReplyScope, message_ids: &[i32]) {
    if message_ids.is_empty() {
        return;
    }
    let Some(store) = redis_store::get().await else {
        return;
    };
    let (scope_type, subject_id) = redis_scope(scope);
    if let Err(error) = store
        .remove_bot_messages(scope_type, subject_id, message_ids)
        .await
    {
        eprintln!("[WARN] Redis 删除芸汐撤回候选失败: {}", error);
    }
}

fn remove_bot_messages(lifecycle: &mut ReplyLifecycle, message_ids: &[i32]) {
    if message_ids.is_empty() {
        return;
    }
    lifecycle
        .recent_bot_messages
        .retain(|message| !message_ids.contains(&message.message_id));
    if let Some(active) = lifecycle.active.as_mut() {
        active
            .sent_message_ids
            .retain(|message_id| !message_ids.contains(message_id));
    }
    for recent in &mut lifecycle.recent_replies {
        recent
            .sent_message_ids
            .retain(|message_id| !message_ids.contains(message_id));
    }
    lifecycle
        .recent_replies
        .retain(|recent| !recent.sent_message_ids.is_empty());
}

fn normalize_recall_message_ids(message_ids: &[i32]) -> Vec<i32> {
    let mut normalized = Vec::new();
    for message_id in message_ids
        .iter()
        .copied()
        .filter(|message_id| *message_id > 0)
    {
        if !normalized.contains(&message_id) {
            normalized.push(message_id);
        }
        if normalized.len() >= MAX_RECALL_MESSAGES_PER_ACTION {
            break;
        }
    }
    normalized
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::{
        ReplyLifecycle, begin_reply, finish_reply, normalize_recall_message_ids,
        parse_recall_notice, record_recent_bot_message, remove_bot_messages,
    };
    use crate::model::interrupt::{ReplyScope, interrupt, is_active};
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

    #[test]
    fn recall_ids_are_positive_unique_and_bounded() {
        assert_eq!(
            normalize_recall_message_ids(&[3, 3, -1, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
            vec![3, 4, 5, 6, 7, 8, 9, 10]
        );
    }

    #[test]
    fn only_recorded_bot_messages_can_be_removed_from_candidates() {
        let mut lifecycle = ReplyLifecycle::default();
        record_recent_bot_message(&mut lifecycle, 10, "第一条");
        record_recent_bot_message(&mut lifecycle, 11, "第二条");
        remove_bot_messages(&mut lifecycle, &[10, 999]);
        assert_eq!(lifecycle.recent_bot_messages.len(), 1);
        assert_eq!(lifecycle.recent_bot_messages[0].message_id, 11);
    }

    #[test]
    fn stale_finish_cannot_release_a_new_lifecycle_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_000_006);
                let old = interrupt(scope).await;
                assert!(begin_reply(scope, old, vec![601]).await);

                let new = interrupt(scope).await;
                assert!(begin_reply(scope, new, vec![602]).await);
                finish_reply(scope, old).await;

                assert!(is_active(scope).await);
                finish_reply(scope, new).await;
                assert!(!is_active(scope).await);
            });
    }
}
