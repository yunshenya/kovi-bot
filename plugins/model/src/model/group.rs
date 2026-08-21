use crate::config;
use crate::health_check::HealthChecker;
use crate::memory::{GroupProfile, MEMORY_MANAGER};
use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::interrupt::{ReplyScope, interrupt, is_active};
use crate::model::recall::{has_recalled_messages, send_tracked_group_message};
use crate::model::reply::record_reply_target;
use crate::model::semantic::{MessageUnderstanding, UnderstandingRequest, understand};
use crate::model::utils::{
    is_bot_admin, is_restricted_command, learn_user_profile_from_message, process_group_reply,
    send_sys_info,
};
use crate::redis_store;
use crate::sticker_memory::{
    StickerScope, extract_stickers, has_reply, known_labels, quoted_message_context,
    stickers_for_teaching, teach, teaching_label, with_quoted_context, with_sticker_context,
};
use crate::vision::{
    ImageRequestScope, VisionImage, consume_pending_image_request, extract_image_attachments,
    is_vision_command, merge_image_attachments, resolve_image_urls, strip_vision_command,
    with_social_image_context,
};
use chrono::Local;
use kovi::event::GroupMsgEvent;
use kovi::serde_json::json;
use kovi::tokio::sync::Mutex;
use kovi::{Message, RuntimeBot};
use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct GroupInterjectionState {
    eligible_messages_since_sample: u32,
    last_interjection: Option<Instant>,
    interjection_in_flight: bool,
    decision_attempts: VecDeque<Instant>,
    conversation_until: Option<Instant>,
    last_bot_reply_at: Option<Instant>,
    conversation_participants: HashMap<i64, Instant>,
    next_turn_generation: u64,
    pending_participants: HashMap<i64, PendingConversationTurn>,
}

struct PendingConversationTurn {
    generation: u64,
    expires_at: Instant,
}

/// 未点名接话只维护本地计数和冷却状态；不会为每一条群消息调用模型。
static GROUP_INTERJECTION_STATE: LazyLock<Mutex<HashMap<i64, GroupInterjectionState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static GROUP_MESSAGE_BATCHES: LazyLock<MessageCoalescer<(i64, i64)>> =
    LazyLock::new(Default::default);

struct PendingWindowMessage {
    user_id: i64,
    nickname: String,
    sender: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    understanding: MessageUnderstanding,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Addressing {
    at_self: bool,
    reply_to_self: bool,
    named_in_text: bool,
}

impl Addressing {
    fn detect(message: &Message, text: &str, self_id: i64, replied_sender_id: Option<i64>) -> Self {
        Self {
            at_self: message_at_self(message, self_id),
            reply_to_self: replied_sender_id == Some(self_id),
            named_in_text: text_mentions_bot(text),
        }
    }

    fn directly_addressed(self) -> bool {
        self.at_self || self.reply_to_self || self.named_in_text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupSenderIdentity {
    user_id: i64,
    qq_nickname: String,
    group_card: Option<String>,
}

impl GroupSenderIdentity {
    fn from_event(event: &GroupMsgEvent) -> Self {
        Self {
            user_id: event.user_id,
            qq_nickname: normalized_sender_name(event.sender.nickname.as_deref())
                .unwrap_or_else(|| format!("QQ用户_{}", event.user_id)),
            group_card: normalized_sender_name(event.sender.card.as_deref()),
        }
    }

    fn display_name(&self) -> &str {
        self.group_card.as_deref().unwrap_or(&self.qq_nickname)
    }

    fn model_sender(&self, time: &str) -> String {
        format!(
            "[{}] 群成员身份={}",
            time,
            json!({
                "群名片": self.group_card.as_deref().unwrap_or("未设置"),
                "群内称呼": self.display_name(),
                "QQ昵称": self.qq_nickname,
                "QQ号": self.user_id,
            })
        )
    }

    fn reply_target_label(&self) -> String {
        format!(
            "群名片={}；QQ昵称={}；QQ号={}",
            self.group_card.as_deref().unwrap_or("未设置"),
            self.qq_nickname,
            self.user_id
        )
    }
}

fn normalized_sender_name(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

/// 当前回复期间只保留每个群组的一条待处理窗口消息，避免高频消息把模型请求反复取消。
static PENDING_WINDOW_MESSAGES: LazyLock<Mutex<HashMap<i64, PendingWindowMessage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct DirectTriggerState {
    recent_triggers: VecDeque<Instant>,
    blocked_until: Option<Instant>,
    last_seen: Option<Instant>,
}

/// 防刷状态按“群 + 成员”隔离，不影响群内其他人正常聊天。
static DIRECT_TRIGGER_STATES: LazyLock<Mutex<HashMap<(i64, i64), DirectTriggerState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn group_message_event(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id;
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let sender_identity = GroupSenderIdentity::from_event(&event);
    let nickname = sender_identity.qq_nickname.clone();
    let sender = sender_identity.model_sender(&time);
    let message = event.borrow_text().unwrap_or_default();
    if is_restricted_command(message) && !is_bot_admin(&bot, event.user_id) {
        println!(
            "[INFO] 群聊未授权命令已静默 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }
    let stickers = extract_stickers(&event.message);
    let current_images = extract_image_attachments(&event.message);
    let vision_command = is_vision_command(message);
    let sticker_scope = StickerScope::Group(group_id);
    let reply_scope = ReplyScope::Group(group_id);
    record_reply_target(
        reply_scope,
        event.message_id,
        Some(event.user_id),
        sender_identity.reply_target_label(),
        &event.human_text,
    )
    .await;
    if let Some(label) = teaching_label(message) {
        match stickers_for_teaching(&event.message, &bot).await {
            Ok(teaching_stickers) if !teaching_stickers.is_empty() => {
                match teach(&teaching_stickers, &label, event.user_id, sticker_scope).await {
                    Ok(count) => {
                        send_tracked_group_message(
                            &bot,
                            group_id,
                            format!("记住啦，这 {count} 个表情以后表示“{label}”。"),
                        )
                        .await;
                    }
                    Err(error) => {
                        eprintln!("[ERROR] 群聊保存表情包记忆失败: {}", error);
                        send_tracked_group_message(
                            &bot,
                            group_id,
                            "这次没能记住，稍后再教我一次吧。",
                        )
                        .await;
                    }
                }
            }
            Ok(_) => {
                send_tracked_group_message(
                    &bot,
                    group_id,
                    "请回复（引用）那张表情包，再发送 #教芸汐 这个表情是……哦。",
                )
                .await;
            }
            Err(error) => {
                eprintln!("[ERROR] 群聊读取被引用表情失败: {}", error);
                send_tracked_group_message(
                    &bot,
                    group_id,
                    "我没能读到被引用的表情，请重新引用后再试一次哦。",
                )
                .await;
            }
        }
        return;
    }

    let labels = match known_labels(&stickers, sticker_scope).await {
        Ok(labels) => labels,
        Err(error) => {
            eprintln!("[ERROR] 群聊读取表情包记忆失败: {}", error);
            Vec::new()
        }
    };
    let quoted = match quoted_message_context(&event.message, &bot, sticker_scope).await {
        Ok(quoted) => quoted,
        Err(error) => {
            eprintln!("[ERROR] 群聊读取引用消息失败: {}", error);
            None
        }
    };
    if let Some(quoted) = quoted.as_ref()
        && let Some(message_id) = quoted.message_id
    {
        record_reply_target(
            reply_scope,
            message_id,
            quoted.sender_id,
            quoted.sender_label.as_deref().unwrap_or("引用消息"),
            &quoted.content,
        )
        .await;
    }
    let quoted_images = quoted
        .as_ref()
        .map(|quoted| quoted.images.as_slice())
        .unwrap_or_default();
    let images = merge_image_attachments(&current_images, quoted_images);
    let addressing = Addressing::detect(
        &event.message,
        message,
        event.self_id,
        quoted.as_ref().and_then(|quoted| quoted.sender_id),
    );
    let addressed_to_bot = addressing.directly_addressed();
    let pending_image_request = consume_pending_image_request(
        ImageRequestScope::Group {
            group_id,
            user_id: event.user_id,
        },
        !images.is_empty(),
    )
    .await;
    if message.trim().is_empty() && !images.is_empty() && !vision_command && !pending_image_request
    {
        println!("[INFO] 收到群聊纯图片状态，保持静默 (群组: {})", group_id);
        return;
    }
    let active_reply = is_active(reply_scope).await;
    let conversation_open = has_open_conversation_window(group_id).await;
    let semantic_required = addressed_to_bot
        || vision_command
        || !images.is_empty()
        || active_reply
        || conversation_open;
    let understanding_request = UnderstandingRequest {
        message: message.to_string(),
        context: "group_chat".to_string(),
        quoted_message: quoted.as_ref().map(|value| value.content.clone()),
        has_images: !images.is_empty(),
        quoted_has_images: !quoted_images.is_empty(),
        explicit_vision_command: vision_command,
        pending_image_request,
        addressed_to_bot,
        conversation_open: active_reply || conversation_open,
    };
    let mut understanding = if semantic_required {
        understand(understanding_request.clone()).await
    } else {
        MessageUnderstanding::default()
    };
    let mut asks_for_silence = understanding.wants_no_reply || understanding.wants_stop;
    let can_interrupt = addressed_to_bot || asks_for_silence || vision_command;
    let reply_ticket = if can_interrupt {
        Some(interrupt(reply_scope).await)
    } else {
        None
    };
    if asks_for_silence {
        GROUP_MESSAGE_BATCHES
            .cancel((group_id, event.user_id))
            .await;
        println!("[INFO] 群聊用户打断回复 (群组: {})", group_id);
        return;
    }
    let mut vision_requested = understanding.should_understand_image(&understanding_request);
    if vision_command && images.is_empty() {
        send_tracked_group_message(
            &bot,
            group_id,
            "请把截图和 #看截图 放在一起，或回复那张截图再发送命令哦。",
        )
        .await;
        return;
    }
    if message.trim().is_empty()
        && stickers.is_empty()
        && !has_reply(&event.message)
        && !vision_requested
    {
        return;
    }
    let text_message = if vision_requested {
        strip_vision_command(message)
    } else {
        message.to_string()
    };
    let current_message = with_sticker_context(&text_message, &labels);
    let model_message = quoted.as_ref().map_or(current_message.clone(), |quoted| {
        with_quoted_context(&current_message, quoted)
    });
    let (
        model_message,
        addressed_to_bot,
        plain_text,
        intent_text,
        batch_vision_requested,
        images,
        source_message_ids,
    ) = if !message.trim_start().starts_with('#') {
        let Some(combined) = GROUP_MESSAGE_BATCHES
            .push(
                (group_id, event.user_id),
                MessagePart {
                    text: model_message,
                    intent_text: message.to_string(),
                    addressed: addressed_to_bot,
                    plain_text: stickers.is_empty() && quoted.is_none(),
                    vision_requested,
                    images,
                    message_ids: vec![event.message_id],
                },
            )
            .await
        else {
            return;
        };
        (
            combined.text,
            combined.addressed,
            combined.plain_text,
            combined.intent_text,
            combined.vision_requested,
            combined.images,
            combined.message_ids,
        )
    } else {
        (
            model_message,
            addressed_to_bot,
            false,
            message.to_string(),
            vision_requested,
            images,
            vec![event.message_id],
        )
    };
    if has_recalled_messages(reply_scope, &source_message_ids).await {
        println!(
            "[INFO] 群聊输入已撤回，丢弃尚未开始的回复 (群组: {})",
            group_id
        );
        return;
    }
    let batch_request = UnderstandingRequest {
        message: intent_text.clone(),
        context: "group_chat_batch".to_string(),
        quoted_message: quoted.as_ref().map(|value| value.content.clone()),
        has_images: !images.is_empty(),
        quoted_has_images: !quoted_images.is_empty(),
        explicit_vision_command: false,
        pending_image_request: batch_vision_requested,
        addressed_to_bot,
        conversation_open: active_reply || conversation_open,
    };
    let sampled_for_interjection = if semantic_required {
        false
    } else {
        reserve_interjection_decision(group_id, &intent_text).await
    };
    understanding = if semantic_required || sampled_for_interjection {
        understand(batch_request.clone()).await
    } else {
        MessageUnderstanding::default()
    };
    asks_for_silence = plain_text && (understanding.wants_no_reply || understanding.wants_stop);
    if asks_for_silence {
        if sampled_for_interjection {
            finish_interjection_attempt(group_id, false).await;
        }
        GROUP_MESSAGE_BATCHES
            .cancel((group_id, event.user_id))
            .await;
        println!(
            "[INFO] 合并后的群聊消息请求停止当前回复 (群组: {})",
            group_id
        );
        return;
    }
    if sampled_for_interjection && !understanding.interjection_worthy {
        finish_interjection_attempt(group_id, false).await;
    }
    vision_requested = understanding.should_understand_image(&batch_request);
    if intent_text.trim().is_empty()
        && !vision_requested
        && (!images.is_empty() || !stickers.is_empty())
    {
        println!("[INFO] 收到群聊纯图片状态，保持静默 (群组: {})", group_id);
        return;
    }
    let model_message = if !images.is_empty() && !vision_requested && !intent_text.trim().is_empty()
    {
        with_social_image_context(&model_message)
    } else {
        model_message
    };
    let vision_images = if vision_requested {
        match resolve_image_urls(&images, &bot).await {
            Ok(images) if !images.is_empty() => images,
            Ok(_) => {
                send_tracked_group_message(
                    &bot,
                    group_id,
                    "我暂时拿不到这张截图的内容，再发一次或换张图试试吧。",
                )
                .await;
                return;
            }
            Err(error) => {
                eprintln!("[ERROR] 群聊读取截图失败 (群组: {}): {}", group_id, error);
                send_tracked_group_message(
                    &bot,
                    group_id,
                    "我暂时读不到这张截图，再发一次或换张图试试吧。",
                )
                .await;
                return;
            }
        }
    } else {
        Vec::new()
    };
    if addressed_to_bot
        && !is_bot_admin(&bot, event.user_id)
        && should_suppress_direct_trigger(group_id, event.user_id).await
    {
        println!(
            "[INFO] 群聊重复或高频点名已静默 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }

    if !message.trim().is_empty() {
        update_group_profile(group_id, event.user_id, &understanding).await;
        learn_user_profile_from_message(event.user_id, message, &nickname, false, &understanding)
            .await;
    }
    let participant_follow_up = is_conversation_participant_message(
        group_id,
        event.user_id,
        understanding.conversation_relevant,
    )
    .await;
    if should_defer_active_window_message(
        is_active(reply_scope).await,
        participant_follow_up,
        reply_ticket.is_some(),
    ) {
        println!(
            "[INFO] 群聊已有回复进行中，排队窗口消息 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        queue_pending_window_message(
            group_id,
            event.user_id,
            nickname,
            sender,
            model_message,
            vision_images,
            source_message_ids,
            understanding.clone(),
        )
        .await;
        return;
    }
    match message.trim() {
        "#系统信息" => {
            send_sys_info(Arc::clone(&bot), group_id).await;
        }

        "#健康检查" => {
            let mut health_checker = HealthChecker::new(Arc::clone(&MEMORY_MANAGER));
            let health_status = health_checker.check_health().await;

            let status_msg = if health_status.is_healthy && health_status.warnings.is_empty() {
                format!(
                    "✅ 系统健康状态良好\n📊 记忆数量: {}\n👥 用户档案: {}\n🏢 群组档案: {}\n💾 记忆快照大小: {:.2}MB",
                    health_status.memory_usage.total_memories,
                    health_status.memory_usage.user_profiles,
                    health_status.memory_usage.group_profiles,
                    health_status.memory_usage.storage_size_bytes as f64 / 1024.0 / 1024.0
                )
            } else if health_status.is_healthy {
                format!(
                    "⚠️ 系统可以运行，但有警告\n{}\n📊 记忆数量: {}\n💾 记忆快照大小: {:.2}MB",
                    health_status.warnings.join("\n"),
                    health_status.memory_usage.total_memories,
                    health_status.memory_usage.storage_size_bytes as f64 / 1024.0 / 1024.0,
                )
            } else {
                format!(
                    "❌ 系统健康状态异常\n错误: {}\n警告: {}",
                    health_status.errors.join(", "),
                    health_status.warnings.join(", ")
                )
            };

            send_tracked_group_message(&bot, group_id, status_msg).await;
        }
        _ => {
            // 被点名时始终处理；未点名消息仅由本地节流器偶尔抽样，不逐条调用模型。
            if addressed_to_bot
                || vision_requested
                || matches!(message.trim(), "#禁言" | "#结束禁言")
            {
                let ticket = match reply_ticket {
                    Some(ticket) => ticket,
                    None => interrupt(reply_scope).await,
                };
                let turn_marker = begin_conversation_turn(group_id, event.user_id).await;
                let replied = process_group_reply(
                    group_id,
                    event.user_id,
                    &model_message,
                    Arc::clone(&bot),
                    sender,
                    ticket,
                    None,
                    vision_images.clone(),
                    source_message_ids.clone(),
                    understanding.clone(),
                )
                .await;
                finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
                drain_pending_window_messages(group_id, Arc::clone(&bot)).await;
            } else if should_continue_conversation(
                group_id,
                event.user_id,
                understanding.conversation_relevant,
            )
            .await
            {
                println!("[INFO] 群聊接续对话 (群组: {})", group_id);
                let ticket = match reply_ticket {
                    Some(ticket) => ticket,
                    None => interrupt(reply_scope).await,
                };
                let turn_marker = begin_conversation_turn(group_id, event.user_id).await;
                let replied = process_group_reply(
                    group_id,
                    event.user_id,
                    &model_message,
                    Arc::clone(&bot),
                    sender,
                    ticket,
                    None,
                    vision_images.clone(),
                    source_message_ids.clone(),
                    understanding.clone(),
                )
                .await;
                finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
                drain_pending_window_messages(group_id, Arc::clone(&bot)).await;
            } else if sampled_for_interjection && understanding.interjection_worthy {
                println!("[INFO] 群聊未点名接话 (群组: {})", group_id);
                let ticket = interrupt(reply_scope).await;
                let turn_marker = begin_conversation_turn(group_id, event.user_id).await;
                let max_output_tokens = config::get()
                    .group_interjection()
                    .interjection_max_output_tokens();
                let replied = process_group_reply(
                    group_id,
                    event.user_id,
                    &model_message,
                    Arc::clone(&bot),
                    sender,
                    ticket,
                    Some(max_output_tokens),
                    vision_images.clone(),
                    source_message_ids.clone(),
                    understanding.clone(),
                )
                .await;
                finish_interjection_attempt(group_id, replied).await;
                finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
                drain_pending_window_messages(group_id, Arc::clone(&bot)).await;
            } else if let Err(error) = MEMORY_MANAGER
                .add_conversation_memory_with_hints(
                    group_id,
                    &format!("{}: {}", sender, model_message),
                    "group_observation",
                    Some(understanding.memory_importance()),
                    &understanding.memory_tags(),
                )
                .await
            {
                eprintln!(
                    "[ERROR] 群聊观察记忆记录失败 (群组: {}): {}",
                    group_id, error
                );
            }
        }
    }
}

async fn should_suppress_direct_trigger(group_id: i64, user_id: i64) -> bool {
    let limits = config::get().group_interjection().clone();
    let now = Instant::now();
    let local_suppressed = {
        let mut states = DIRECT_TRIGGER_STATES.lock().await;
        if states.len() > 2_048 {
            let retention =
                Duration::from_secs(limits.direct_spam_cooldown_secs().saturating_mul(2));
            states.retain(|_, state| {
                state
                    .last_seen
                    .is_some_and(|last_seen| now.duration_since(last_seen) < retention)
            });
        }
        let state = states.entry((group_id, user_id)).or_default();
        suppress_direct_trigger(
            state,
            now,
            Duration::from_secs(limits.direct_spam_cooldown_secs()),
            Duration::from_secs(limits.direct_rate_window_secs()),
            limits.direct_rate_limit(),
        )
    };
    if local_suppressed {
        return true;
    }

    let Some(store) = redis_store::get().await else {
        return false;
    };
    let rate_window = Duration::from_secs(limits.direct_rate_window_secs());
    let rate_key = format!("rate:direct-trigger:group:{group_id}:user:{user_id}");
    match store.increment_expiring(&rate_key, rate_window).await {
        Ok(count) if count > limits.direct_rate_limit() as i64 => {
            let mut states = DIRECT_TRIGGER_STATES.lock().await;
            if let Some(state) = states.get_mut(&(group_id, user_id)) {
                state.blocked_until =
                    Some(Instant::now() + Duration::from_secs(limits.direct_spam_cooldown_secs()));
            }
            true
        }
        Ok(_) => false,
        Err(error) => {
            eprintln!("[WARN] Redis 直接点名限流失败，继续使用本地限流: {}", error);
            false
        }
    }
}

fn suppress_direct_trigger(
    state: &mut DirectTriggerState,
    now: Instant,
    cooldown: Duration,
    rate_window: Duration,
    rate_limit: usize,
) -> bool {
    state.last_seen = Some(now);
    if state.blocked_until.is_some_and(|until| until > now) {
        return true;
    }
    state.blocked_until = None;

    while state
        .recent_triggers
        .front()
        .is_some_and(|seen_at| now.duration_since(*seen_at) >= rate_window)
    {
        state.recent_triggers.pop_front();
    }
    state.recent_triggers.push_back(now);
    if state.recent_triggers.len() > rate_limit {
        state.blocked_until = Some(now + cooldown);
        return true;
    }
    false
}

/// 标记正在回复的成员，使其在模型思考和连续气泡发送期间也能自然打断或补充。
async fn begin_conversation_turn(group_id: i64, user_id: i64) -> u64 {
    let deadline = Instant::now()
        + Duration::from_secs(
            config::get()
                .group_interjection()
                .conversation_window_secs(),
        );
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    state.next_turn_generation = state.next_turn_generation.wrapping_add(1);
    let generation = state.next_turn_generation;
    state.pending_participants.insert(
        user_id,
        PendingConversationTurn {
            generation,
            expires_at: deadline,
        },
    );
    generation
}

/// 只有实际回复成功才开启三分钟窗口；代数标记避免旧任务清掉同一成员的新一轮状态。
async fn finish_conversation_turn(
    group_id: i64,
    user_id: i64,
    turn_generation: u64,
    replied: bool,
) {
    let now = Instant::now();
    let duration = Duration::from_secs(
        config::get()
            .group_interjection()
            .conversation_window_secs(),
    );
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return;
    };
    if state
        .pending_participants
        .get(&user_id)
        .is_some_and(|turn| turn.generation == turn_generation)
    {
        state.pending_participants.remove(&user_id);
    }
    if replied {
        let deadline = now + duration;
        state.conversation_until = Some(deadline);
        state.last_bot_reply_at = Some(now);
        state.conversation_participants.insert(user_id, deadline);
    }
}

/// 参与者、待回复成员，或机器人刚发言后的新成员可以自然接续，无需匹配固定词。
async fn is_conversation_participant_message(
    group_id: i64,
    user_id: i64,
    semantic_relevant: bool,
) -> bool {
    let now = Instant::now();
    let open_floor = Duration::from_secs(
        config::get()
            .group_interjection()
            .conversation_open_floor_secs(),
    );
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return false;
    };
    prune_conversation_participants(state, now);
    if !has_active_conversation_window(state.conversation_until, now) {
        state.conversation_until = None;
    }
    let relevant =
        conversation_message_is_relevant(state, user_id, semantic_relevant, now, open_floor);
    if relevant {
        roll_conversation_window(
            state,
            user_id,
            now,
            Duration::from_secs(
                config::get()
                    .group_interjection()
                    .conversation_window_secs(),
            ),
        );
    }
    relevant
}

async fn should_continue_conversation(
    group_id: i64,
    user_id: i64,
    semantic_relevant: bool,
) -> bool {
    is_conversation_participant_message(group_id, user_id, semantic_relevant).await
}

fn conversation_message_is_relevant(
    state: &GroupInterjectionState,
    user_id: i64,
    semantic_relevant: bool,
    now: Instant,
    open_floor: Duration,
) -> bool {
    if !semantic_relevant {
        return false;
    }
    if state
        .pending_participants
        .get(&user_id)
        .is_some_and(|turn| turn.expires_at > now)
    {
        return true;
    }
    if !has_active_conversation_window(state.conversation_until, now) {
        return false;
    }
    state
        .conversation_participants
        .get(&user_id)
        .is_some_and(|deadline| *deadline > now)
        || state
            .last_bot_reply_at
            .is_some_and(|last_reply| now.duration_since(last_reply) < open_floor)
}

/// 有效的连续对话消息会把窗口向后滚动，避免窗口从第一次回复开始固定倒计时。
fn roll_conversation_window(
    state: &mut GroupInterjectionState,
    user_id: i64,
    now: Instant,
    window: Duration,
) {
    let deadline = now + window;
    if state
        .conversation_until
        .is_none_or(|current_deadline| current_deadline < deadline)
    {
        state.conversation_until = Some(deadline);
    }

    let participant_deadline = state
        .conversation_participants
        .entry(user_id)
        .or_insert(deadline);
    if *participant_deadline < deadline {
        *participant_deadline = deadline;
    }
}

fn should_defer_active_window_message(
    active_reply: bool,
    participant_follow_up: bool,
    has_explicit_interrupt: bool,
) -> bool {
    active_reply && participant_follow_up && !has_explicit_interrupt
}

#[allow(clippy::too_many_arguments)]
async fn queue_pending_window_message(
    group_id: i64,
    user_id: i64,
    nickname: String,
    sender: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    understanding: MessageUnderstanding,
) {
    let mut pending = PENDING_WINDOW_MESSAGES.lock().await;
    if let Some(existing) = pending.get_mut(&group_id) {
        existing.user_id = user_id;
        existing.nickname = nickname;
        existing.sender = sender;
        existing.message = message;
        merge_vision_images(&mut existing.vision_images, vision_images);
        merge_message_ids(&mut existing.message_ids, message_ids);
        existing.understanding = understanding;
    } else {
        pending.insert(
            group_id,
            PendingWindowMessage {
                user_id,
                nickname,
                sender,
                message,
                vision_images,
                message_ids,
                understanding,
            },
        );
    }
}

fn merge_vision_images(target: &mut Vec<VisionImage>, incoming: Vec<VisionImage>) {
    for image in incoming {
        if target.iter().any(|existing| existing.url == image.url) {
            continue;
        }
        target.push(image);
        if target.len() >= 4 {
            target.truncate(4);
            break;
        }
    }
}

fn merge_message_ids(target: &mut Vec<i32>, incoming: Vec<i32>) {
    for message_id in incoming {
        if !target.contains(&message_id) {
            target.push(message_id);
        }
    }
}

async fn drain_pending_window_messages(group_id: i64, bot: Arc<RuntimeBot>) {
    for _ in 0..3 {
        let Some(pending) = PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id) else {
            return;
        };
        if is_active(ReplyScope::Group(group_id)).await {
            PENDING_WINDOW_MESSAGES
                .lock()
                .await
                .insert(group_id, pending);
            return;
        }

        println!("[INFO] 群聊开始处理排队窗口消息 (群组: {})", group_id);
        let ticket = interrupt(ReplyScope::Group(group_id)).await;
        let turn_marker = begin_conversation_turn(group_id, pending.user_id).await;
        let replied = crate::model::utils::process_group_reply(
            group_id,
            pending.user_id,
            &pending.message,
            bot.clone(),
            pending.sender,
            ticket,
            None,
            pending.vision_images,
            pending.message_ids,
            pending.understanding,
        )
        .await;
        finish_conversation_turn(group_id, pending.user_id, turn_marker, replied).await;
        if !replied {
            return;
        }
    }
}

fn prune_conversation_participants(state: &mut GroupInterjectionState, now: Instant) {
    state
        .conversation_participants
        .retain(|_, deadline| *deadline > now);
    state
        .pending_participants
        .retain(|_, turn| turn.expires_at > now);
}

fn has_active_conversation_window(until: Option<Instant>, now: Instant) -> bool {
    until.is_some_and(|deadline| deadline > now)
}

async fn has_open_conversation_window(group_id: i64) -> bool {
    let now = Instant::now();
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return false;
    };
    prune_conversation_participants(state, now);
    has_active_conversation_window(state.conversation_until, now)
}

/// 只用消息长度、计数、额度和概率决定是否值得调用一次语义模型。
async fn reserve_interjection_decision(group_id: i64, message: &str) -> bool {
    let config = config::get().group_interjection().clone();
    if !config.enabled() || !has_interjection_candidate(message, config.min_message_chars()) {
        return false;
    }

    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    let now = Instant::now();
    prune_decision_attempts(
        state,
        now,
        Duration::from_secs(config.decision_rate_window_secs()),
    );
    if state.interjection_in_flight {
        return false;
    }
    if state
        .last_interjection
        .is_some_and(|last| now.duration_since(last) < Duration::from_secs(config.cooldown_secs()))
    {
        return false;
    }

    state.eligible_messages_since_sample = state.eligible_messages_since_sample.saturating_add(1);
    if state.eligible_messages_since_sample < config.min_eligible_messages() {
        return false;
    }
    if !decision_budget_available(
        state,
        now,
        Duration::from_secs(config.decision_cooldown_secs()),
        config.decision_rate_limit(),
    ) {
        // 保留已累计的候选；额度恢复后下一条有效消息即可再次抽样。
        state.eligible_messages_since_sample = config.min_eligible_messages();
        return false;
    }
    // 每积累一批候选消息才抽样一次；未抽中也重新累计，避免逐条消耗 token。
    state.eligible_messages_since_sample = 0;
    if !rand::rng().random_ratio(config.response_probability_percent().into(), 100) {
        return false;
    }

    state.interjection_in_flight = true;
    state.decision_attempts.push_back(now);
    true
}

fn has_interjection_candidate(message: &str, min_message_chars: usize) -> bool {
    let text = message.trim();
    !text.starts_with('#') && text.chars().count() >= min_message_chars
}

fn prune_decision_attempts(
    state: &mut GroupInterjectionState,
    now: Instant,
    rate_window: Duration,
) {
    while state
        .decision_attempts
        .front()
        .is_some_and(|attempt| now.duration_since(*attempt) >= rate_window)
    {
        state.decision_attempts.pop_front();
    }
}

fn decision_budget_available(
    state: &GroupInterjectionState,
    now: Instant,
    cooldown: Duration,
    rate_limit: usize,
) -> bool {
    state
        .decision_attempts
        .back()
        .is_none_or(|attempt| now.duration_since(*attempt) >= cooldown)
        && state.decision_attempts.len() < rate_limit
}

/// 模型选择静默时只结束本轮尝试；真正发出消息后才开始冷却。
async fn finish_interjection_attempt(group_id: i64, replied: bool) {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    if let Some(state) = states.get_mut(&group_id) {
        complete_interjection_attempt(state, replied, Instant::now());
    }
}

fn complete_interjection_attempt(
    state: &mut GroupInterjectionState,
    replied: bool,
    completed_at: Instant,
) {
    state.interjection_in_flight = false;
    if replied {
        state.last_interjection = Some(completed_at);
    }
}

fn prune_interjection_states(states: &mut HashMap<i64, GroupInterjectionState>) {
    if states.len() <= 1_024 {
        return;
    }
    let now = Instant::now();
    let interjection_config = config::get().group_interjection().clone();
    let cooldown = Duration::from_secs(interjection_config.cooldown_secs());
    let decision_window = Duration::from_secs(interjection_config.decision_rate_window_secs());
    states.retain(|_, state| {
        prune_conversation_participants(state, now);
        prune_decision_attempts(state, now, decision_window);
        state.interjection_in_flight
            || has_active_conversation_window(state.conversation_until, now)
            || !state.pending_participants.is_empty()
            || !state.decision_attempts.is_empty()
            || state
                .last_interjection
                .is_some_and(|last| now.duration_since(last) < cooldown)
    })
}

fn message_at_self(message: &Message, self_id: i64) -> bool {
    message.iter().any(|segment| {
        if segment.type_ != "at" {
            return false;
        }

        segment.data.get("qq").is_some_and(|qq| {
            qq.as_i64() == Some(self_id)
                || qq.as_str().and_then(|value| value.parse::<i64>().ok()) == Some(self_id)
        })
    })
}

fn text_mentions_bot(message: &str) -> bool {
    ["芸汐", "云汐"].iter().any(|name| message.contains(name))
}

async fn update_group_profile(group_id: i64, user_id: i64, understanding: &MessageUnderstanding) {
    let mut profile = MEMORY_MANAGER
        .get_group_profile(group_id)
        .await
        .unwrap_or_else(|| GroupProfile {
            group_id,
            group_name: format!("群组_{}", group_id),
            active_members: Vec::new(),
            group_personality: "friendly".to_string(),
            conversation_topics: Vec::new(),
            last_activity: Local::now(),
            activity_level: 1,
        });

    // 更新活动信息
    profile.last_activity = Local::now();
    profile.activity_level = (profile.activity_level + 1).min(10);
    if !profile.active_members.contains(&user_id) {
        profile.active_members.push(user_id);
        if profile.active_members.len() > 100 {
            profile.active_members.remove(0);
        }
    }

    for topic in &understanding.topics {
        let topic = topic.trim();
        if topic.is_empty() {
            continue;
        }
        if !profile
            .conversation_topics
            .iter()
            .any(|existing| existing == topic)
        {
            profile.conversation_topics.push(topic.to_string());
        }
    }

    // 限制话题数量
    if profile.conversation_topics.len() > 20 {
        profile
            .conversation_topics
            .drain(0..profile.conversation_topics.len() - 20);
    }

    if !understanding.group_atmosphere.trim().is_empty() {
        profile.group_personality = understanding.group_atmosphere.trim().to_string();
    }

    // 更新群组档案
    if let Err(e) = MEMORY_MANAGER.update_group_profile(group_id, profile).await {
        eprintln!("[ERROR] 更新群组档案失败 (群组: {}): {}", group_id, e);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Addressing, DirectTriggerState, GroupInterjectionState, GroupSenderIdentity,
        complete_interjection_attempt, conversation_message_is_relevant, decision_budget_available,
        has_active_conversation_window, message_at_self, normalized_sender_name,
        prune_decision_attempts, roll_conversation_window, should_defer_active_window_message,
        suppress_direct_trigger, text_mentions_bot,
    };
    use crate::model::utils::{is_group_admin_command, is_restricted_command};
    use kovi::Message;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn structured_at_segments_identify_only_the_bot_account() {
        let self_id = 123_456;

        let string_id = Message::from(vec![Segment::new("at", json!({"qq": "123456"}))]);
        let numeric_id = Message::from(vec![Segment::new("at", json!({"qq": 123456}))]);
        let another_user = Message::from(vec![Segment::new("at", json!({"qq": "654321"}))]);
        let everyone = Message::from(vec![Segment::new("at", json!({"qq": "all"}))]);
        let no_at = Message::from("芸汐在吗");
        let multiple_targets = Message::from(vec![
            Segment::new("at", json!({"qq": "654321"})),
            Segment::new("text", json!({"text": "还有"})),
            Segment::new("at", json!({"qq": "123456"})),
        ]);

        assert!(message_at_self(&string_id, self_id));
        assert!(message_at_self(&numeric_id, self_id));
        assert!(!message_at_self(&another_user, self_id));
        assert!(!message_at_self(&everyone, self_id));
        assert!(!message_at_self(&no_at, self_id));
        assert!(message_at_self(&multiple_targets, self_id));
    }

    #[test]
    fn at_and_reply_are_unified_as_direct_addressing() {
        let self_id = 123_456;
        let at_self = Message::from(vec![Segment::new("at", json!({"qq": "123456"}))]);
        let plain = Message::from("继续说");

        let mention = Addressing::detect(&at_self, "在吗", self_id, None);
        assert!(mention.at_self);
        assert!(!mention.reply_to_self);
        assert!(!mention.named_in_text);
        assert!(mention.directly_addressed());

        let reply = Addressing::detect(&plain, "继续说", self_id, Some(self_id));
        assert!(!reply.at_self);
        assert!(reply.reply_to_self);
        assert!(!reply.named_in_text);
        assert!(reply.directly_addressed());

        let named = Addressing::detect(&plain, "芸汐你在吗", self_id, None);
        assert!(!named.at_self);
        assert!(!named.reply_to_self);
        assert!(named.named_in_text);
        assert!(named.directly_addressed());

        assert!(!Addressing::detect(&plain, "继续说", self_id, Some(654_321)).directly_addressed());
    }

    #[test]
    fn bot_name_aliases_are_direct_text_mentions() {
        assert!(text_mentions_bot("芸汐你在吗"));
        assert!(text_mentions_bot("云汐，看看这个"));
        assert!(text_mentions_bot("你家芸汐好像有点安静"));
        assert!(!text_mentions_bot("今天群里有点安静"));
    }

    #[test]
    fn group_identity_keeps_card_and_qq_nickname_separate() {
        let identity = GroupSenderIdentity {
            user_id: 123,
            qq_nickname: "QQ用户名".to_string(),
            group_card: Some("群内昵称".to_string()),
        };
        assert_eq!(identity.display_name(), "群内昵称");
        let sender = identity.model_sender("12:34:56");
        assert!(sender.contains("群内昵称"));
        assert!(sender.contains("QQ用户名"));
        assert!(sender.contains("123"));
        assert_eq!(
            identity.reply_target_label(),
            "群名片=群内昵称；QQ昵称=QQ用户名；QQ号=123"
        );
    }

    #[test]
    fn sender_names_are_trimmed_without_merging_identity_fields() {
        assert_eq!(
            normalized_sender_name(Some("  群 名片\n测试  ")).as_deref(),
            Some("群 名片 测试")
        );
        assert_eq!(normalized_sender_name(Some("   ")), None);
    }

    #[test]
    fn interjection_cooldown_starts_only_after_a_visible_reply() {
        let mut state = GroupInterjectionState {
            interjection_in_flight: true,
            ..GroupInterjectionState::default()
        };
        let now = Instant::now();
        complete_interjection_attempt(&mut state, false, now);
        assert!(!state.interjection_in_flight);
        assert!(state.last_interjection.is_none());

        state.interjection_in_flight = true;
        complete_interjection_attempt(&mut state, true, now);
        assert!(!state.interjection_in_flight);
        assert_eq!(state.last_interjection, Some(now));
    }

    #[test]
    fn conversation_window_uses_participants_and_timing_instead_of_keywords() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(180);
        let mut state = GroupInterjectionState {
            conversation_until: Some(deadline),
            last_bot_reply_at: Some(now - Duration::from_secs(46)),
            ..GroupInterjectionState::default()
        };
        state.conversation_participants.insert(42, deadline);

        assert!(conversation_message_is_relevant(
            &state,
            42,
            true,
            now,
            Duration::from_secs(45),
        ));
        assert!(conversation_message_is_relevant(
            &state,
            42,
            true,
            now,
            Duration::from_secs(45),
        ));
        assert!(!conversation_message_is_relevant(
            &state,
            42,
            false,
            now,
            Duration::from_secs(45),
        ));
        assert!(!conversation_message_is_relevant(
            &state,
            99,
            false,
            now,
            Duration::from_secs(45),
        ));

        state.last_bot_reply_at = Some(now - Duration::from_secs(10));
        assert!(conversation_message_is_relevant(
            &state,
            99,
            true,
            now,
            Duration::from_secs(45),
        ));
        assert!(!conversation_message_is_relevant(
            &state,
            42,
            false,
            now,
            Duration::from_secs(45),
        ));
    }

    #[test]
    fn unsolicited_model_decisions_obey_cooldown_and_rate_budget() {
        let now = Instant::now();
        let mut state = GroupInterjectionState::default();
        assert!(decision_budget_available(
            &state,
            now,
            Duration::from_secs(60),
            3,
        ));
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(30));
        assert!(!decision_budget_available(
            &state,
            now,
            Duration::from_secs(60),
            3,
        ));
        state.decision_attempts.clear();
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(120));
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(90));
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(30));
        assert!(!decision_budget_available(
            &state,
            now,
            Duration::from_secs(60),
            3,
        ));
        prune_decision_attempts(&mut state, now, Duration::from_secs(100));
        assert_eq!(state.decision_attempts.len(), 2);
    }

    #[test]
    fn conversation_window_remains_open_for_three_minutes_then_expires() {
        let opened_at = Instant::now();
        let deadline = opened_at + Duration::from_secs(180);

        assert!(has_active_conversation_window(Some(deadline), opened_at));
        assert!(has_active_conversation_window(
            Some(deadline),
            opened_at + Duration::from_secs(179)
        ));
        assert!(!has_active_conversation_window(Some(deadline), deadline));
        assert!(!has_active_conversation_window(None, opened_at));
    }

    #[test]
    fn conversation_window_rolls_forward_with_relevant_messages() {
        let opened_at = Instant::now();
        let original_deadline = opened_at + Duration::from_secs(180);
        let first_message_at = opened_at + Duration::from_secs(170);
        let second_message_at = opened_at + Duration::from_secs(340);
        let window = Duration::from_secs(180);
        let mut state = GroupInterjectionState {
            conversation_until: Some(original_deadline),
            ..GroupInterjectionState::default()
        };
        state
            .conversation_participants
            .insert(42, original_deadline);

        assert!(conversation_message_is_relevant(
            &state,
            42,
            true,
            first_message_at,
            Duration::from_secs(45),
        ));
        roll_conversation_window(&mut state, 42, first_message_at, window);

        let first_rolled_deadline = first_message_at + window;
        assert_eq!(state.conversation_until, Some(first_rolled_deadline));
        assert_eq!(
            state.conversation_participants.get(&42),
            Some(&first_rolled_deadline)
        );
        assert!(has_active_conversation_window(
            state.conversation_until,
            second_message_at
        ));

        assert!(conversation_message_is_relevant(
            &state,
            42,
            true,
            second_message_at,
            Duration::from_secs(45),
        ));
        roll_conversation_window(&mut state, 42, second_message_at, window);
        assert!(has_active_conversation_window(
            state.conversation_until,
            second_message_at + Duration::from_secs(179)
        ));
    }

    #[test]
    fn ordinary_window_messages_do_not_cancel_an_active_reply() {
        assert!(should_defer_active_window_message(true, true, false));
        assert!(!should_defer_active_window_message(true, true, true));
        assert!(!should_defer_active_window_message(true, false, false));
        assert!(!should_defer_active_window_message(false, true, false));
    }

    #[test]
    fn formal_commands_are_classified_before_chat_processing() {
        assert!(is_group_admin_command("#健康检查"));
        assert!(is_restricted_command(" #禁言 "));
        assert!(is_restricted_command("#教芸汐 这个表情是开心"));
        assert!(is_restricted_command("#识图"));
        assert!(!is_restricted_command("芸汐，今天开心吗"));
    }

    #[test]
    fn direct_mentions_are_limited_by_rate_window_then_cooled_down() {
        let mut state = DirectTriggerState::default();
        let started = Instant::now();
        let cooldown = Duration::from_secs(600);
        let rate_window = Duration::from_secs(60);

        assert!(!suppress_direct_trigger(
            &mut state,
            started,
            cooldown,
            rate_window,
            4,
        ));
        for offset in [5, 10, 15] {
            assert!(!suppress_direct_trigger(
                &mut state,
                started + Duration::from_secs(offset),
                cooldown,
                Duration::from_secs(60),
                4,
            ));
        }
        assert!(suppress_direct_trigger(
            &mut state,
            started + Duration::from_secs(20),
            cooldown,
            rate_window,
            4,
        ));
        assert!(suppress_direct_trigger(
            &mut state,
            started + Duration::from_secs(30),
            cooldown,
            rate_window,
            4,
        ));
        assert!(!suppress_direct_trigger(
            &mut state,
            started + Duration::from_secs(620),
            cooldown,
            rate_window,
            4,
        ));
    }
}
