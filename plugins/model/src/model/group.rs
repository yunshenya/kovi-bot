use crate::config;
use crate::health_check::HealthChecker;
use crate::memory::{GroupProfile, MEMORY_MANAGER};
use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::interrupt::{ReplyScope, interrupt, is_active, is_explicit_stop_message};
use crate::model::recall::has_recalled_messages;
use crate::model::reply::record_reply_target;
use crate::model::utils::{
    learn_user_profile_from_message, requests_no_reply, send_sys_info, silence,
};
use crate::sticker_memory::{
    StickerScope, extract_stickers, has_reply, known_labels, quoted_message_context,
    stickers_for_teaching, teach, teaching_label, with_quoted_context, with_sticker_context,
};
use crate::vision::{
    ImageIntent, ImageRequestScope, VisionImage, classify_image_intent,
    consume_pending_image_request, extract_image_attachments, is_vision_command,
    merge_image_attachments, message_requests_image, resolve_image_urls, strip_vision_command,
    with_social_image_context,
};
use chrono::Local;
use kovi::RuntimeBot;
use kovi::event::GroupMsgEvent;
use kovi::tokio::sync::Mutex;
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
}

/// 当前回复期间只保留每个群组的一条待处理窗口消息，避免高频消息把模型请求反复取消。
static PENDING_WINDOW_MESSAGES: LazyLock<Mutex<HashMap<i64, PendingWindowMessage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct DirectTriggerState {
    last_message: String,
    repeated_count: u32,
    last_message_at: Option<Instant>,
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
    let nickname = event.get_sender_nickname();
    let sender = format!("[{}] {}", time, nickname);
    let message = event.borrow_text().unwrap_or_default();
    let stickers = extract_stickers(&event.message);
    let current_images = extract_image_attachments(&event.message);
    let vision_command = is_vision_command(message);
    let sticker_scope = StickerScope::Group(group_id);
    let reply_scope = ReplyScope::Group(group_id);
    record_reply_target(
        reply_scope,
        event.message_id,
        Some(event.user_id),
        nickname.clone(),
        &event.human_text,
    )
    .await;
    let directly_addressed = is_addressed_to_bot(&event, message);
    let explicit_stop = is_explicit_stop_message(message);
    let asks_for_silence = explicit_stop || requests_no_reply(message);
    let participant_follow_up = is_conversation_participant_message(
        group_id,
        event.user_id,
        message,
        !current_images.is_empty(),
    )
    .await;
    let can_interrupt = directly_addressed || asks_for_silence || vision_command;
    let mut reply_ticket = if can_interrupt {
        Some(interrupt(reply_scope).await)
    } else {
        None
    };

    if asks_for_silence && can_interrupt {
        GROUP_MESSAGE_BATCHES
            .cancel((group_id, event.user_id))
            .await;
        println!("[INFO] 群聊用户打断回复 (群组: {})", group_id);
        return;
    }

    if is_admin_command(message) && !is_bot_admin(&bot, event.user_id) {
        bot.send_group_msg(group_id, "这个命令只有管理员可以使用哦。");
        return;
    }

    if let Some(label) = teaching_label(message) {
        match stickers_for_teaching(&event.message, &bot).await {
            Ok(teaching_stickers) if !teaching_stickers.is_empty() => {
                match teach(&teaching_stickers, &label, event.user_id, sticker_scope).await {
                    Ok(count) => bot.send_group_msg(
                        group_id,
                        format!("记住啦，这 {count} 个表情以后表示“{label}”。"),
                    ),
                    Err(error) => {
                        eprintln!("[ERROR] 群聊保存表情包记忆失败: {}", error);
                        bot.send_group_msg(group_id, "这次没能记住，稍后再教我一次吧。");
                    }
                }
            }
            Ok(_) => bot.send_group_msg(
                group_id,
                "请回复（引用）那张表情包，再发送 #教芸汐 这个表情是……哦。",
            ),
            Err(error) => {
                eprintln!("[ERROR] 群聊读取被引用表情失败: {}", error);
                bot.send_group_msg(group_id, "我没能读到被引用的表情，请重新引用后再试一次哦。");
            }
        }
        return;
    }

    if requests_no_reply(message) {
        GROUP_MESSAGE_BATCHES
            .cancel((group_id, event.user_id))
            .await;
        println!("[INFO] 群聊明确要求不回复 (群组: {})", group_id);
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
            "引用消息",
            &quoted.content,
        )
        .await;
    }
    let quoted_images = quoted
        .as_ref()
        .map(|quoted| quoted.images.as_slice())
        .unwrap_or_default();
    let images = merge_image_attachments(&current_images, quoted_images);
    let replies_to_bot = quoted.as_ref().and_then(|quoted| quoted.sender_id) == Some(event.self_id);
    let replies_to_image = !quoted_images.is_empty() && !message.trim().is_empty();
    let quoted_message_requests_image = quoted.as_ref().is_some_and(|quoted| {
        quoted.sender_id == Some(event.self_id) && message_requests_image(&quoted.content)
    });
    let pending_image_request = consume_pending_image_request(
        ImageRequestScope::Group {
            group_id,
            user_id: event.user_id,
        },
        !images.is_empty(),
    )
    .await;
    let image_intent = classify_image_intent(
        message,
        !images.is_empty(),
        vision_command,
        replies_to_image,
        quoted_message_requests_image,
        pending_image_request,
    );
    let vision_requested = image_intent == ImageIntent::VisualUnderstand;
    if vision_command && images.is_empty() {
        bot.send_group_msg(
            group_id,
            "请把截图和 #看截图 放在一起，或回复那张截图再发送命令哦。",
        );
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
    if replies_to_bot && reply_ticket.is_none() {
        reply_ticket = Some(interrupt(reply_scope).await);
    }
    if explicit_stop && replies_to_bot {
        GROUP_MESSAGE_BATCHES
            .cancel((group_id, event.user_id))
            .await;
        println!("[INFO] 群聊引用消息打断回复 (群组: {})", group_id);
        return;
    }
    let addressed_to_bot = directly_addressed || replies_to_bot;
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
    let batch_image_intent = classify_image_intent(
        &intent_text,
        !images.is_empty(),
        false,
        !quoted_images.is_empty() && !intent_text.trim().is_empty(),
        false,
        batch_vision_requested,
    );
    let vision_requested =
        batch_vision_requested || batch_image_intent == ImageIntent::VisualUnderstand;
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
                bot.send_group_msg(
                    group_id,
                    "我暂时拿不到这张截图的内容，再发一次或换张图试试吧。",
                );
                return;
            }
            Err(error) => {
                eprintln!("[ERROR] 群聊读取截图失败 (群组: {}): {}", group_id, error);
                bot.send_group_msg(group_id, "我暂时读不到这张截图，再发一次或换张图试试吧。");
                return;
            }
        }
    } else {
        Vec::new()
    };
    if plain_text && requests_no_reply(&model_message) {
        println!("[INFO] 合并后的群聊消息明确要求不回复 (群组: {})", group_id);
        return;
    }
    if addressed_to_bot
        && !is_bot_admin(&bot, event.user_id)
        && should_suppress_direct_trigger(group_id, event.user_id, &model_message).await
    {
        println!(
            "[INFO] 群聊重复或高频点名已静默 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }

    if !message.trim().is_empty() {
        update_group_profile(group_id, event.user_id, message, &nickname).await;
        learn_user_profile_from_message(event.user_id, message, &nickname, false).await;
    }
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
        )
        .await;
        return;
    }
    match message.trim() {
        "#系统信息" => {
            send_sys_info(Arc::clone(&bot), group_id).await;
        }

        "#重载配置文件" => match config::reload_config_from_file() {
            Ok(_) => bot.send_group_msg(group_id, "配置重载成功"),
            Err(e) => bot.send_group_msg(group_id, format!("配置重载失败: {}", e)),
        },

        "#重载全部配置" => match config::reload_config() {
            Ok(_) => bot.send_group_msg(group_id, "全部配置文件重载成功"),
            Err(e) => bot.send_group_msg(group_id, format!("重载失败： {}", e)),
        },

        "#启用自动重载" => {
            if config::is_auto_reload_enabled() {
                bot.send_group_msg(group_id, "自动重载已经启用");
            } else {
                config::enable_auto_reload(Duration::from_secs(5));
                bot.send_group_msg(group_id, "自动重载已启用，每5秒检查一次");
            }
        }

        "#禁用自动重载" => {
            if config::is_auto_reload_enabled() {
                config::disable_auto_reload();
                bot.send_group_msg(group_id, "自动重载已禁用");
            } else {
                bot.send_group_msg(group_id, "自动重载未启用");
            }
        }

        "#检查配置变化" => match config::check_and_reload() {
            Ok(true) => bot.send_group_msg(group_id, "检测到配置变化，已自动重载"),
            Ok(false) => bot.send_group_msg(group_id, "配置文件无变化"),
            Err(e) => bot.send_group_msg(group_id, format!("检查配置失败: {}", e)),
        },

        "#自动重载状态" => {
            let status = if config::is_auto_reload_enabled() {
                "已启用"
            } else {
                "已禁用"
            };
            bot.send_group_msg(group_id, format!("配置自动重载状态: {}", status));
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

            bot.send_group_msg(group_id, &status_msg);
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
                let replied = silence(
                    group_id,
                    event.user_id,
                    &model_message,
                    Arc::clone(&bot),
                    sender,
                    ticket,
                    None,
                    vision_images.clone(),
                    source_message_ids.clone(),
                )
                .await;
                finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
                drain_pending_window_messages(group_id, Arc::clone(&bot)).await;
            } else if should_continue_conversation(
                group_id,
                event.user_id,
                &model_message,
                !images.is_empty(),
            )
            .await
            {
                println!("[INFO] 群聊接续对话 (群组: {})", group_id);
                let ticket = match reply_ticket {
                    Some(ticket) => ticket,
                    None => interrupt(reply_scope).await,
                };
                let turn_marker = begin_conversation_turn(group_id, event.user_id).await;
                let replied = silence(
                    group_id,
                    event.user_id,
                    &model_message,
                    Arc::clone(&bot),
                    sender,
                    ticket,
                    None,
                    vision_images.clone(),
                    source_message_ids.clone(),
                )
                .await;
                finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
                drain_pending_window_messages(group_id, Arc::clone(&bot)).await;
            } else if should_interject(group_id, &model_message).await {
                println!("[INFO] 群聊未点名接话 (群组: {})", group_id);
                let ticket = interrupt(reply_scope).await;
                let turn_marker = begin_conversation_turn(group_id, event.user_id).await;
                let max_output_tokens = config::get()
                    .group_interjection()
                    .interjection_max_output_tokens();
                let replied = silence(
                    group_id,
                    event.user_id,
                    &model_message,
                    Arc::clone(&bot),
                    sender,
                    ticket,
                    Some(max_output_tokens),
                    vision_images.clone(),
                    source_message_ids.clone(),
                )
                .await;
                finish_interjection_attempt(group_id, replied).await;
                finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
                drain_pending_window_messages(group_id, Arc::clone(&bot)).await;
            } else if let Err(error) = MEMORY_MANAGER
                .add_conversation_memory(
                    group_id,
                    &format!("{}: {}", nickname, model_message),
                    "group_observation",
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

async fn should_suppress_direct_trigger(group_id: i64, user_id: i64, message: &str) -> bool {
    let limits = config::get().group_interjection().clone();
    let now = Instant::now();
    let mut states = DIRECT_TRIGGER_STATES.lock().await;
    if states.len() > 2_048 {
        let retention = Duration::from_secs(limits.direct_spam_cooldown_secs().saturating_mul(2));
        states.retain(|_, state| {
            state
                .last_seen
                .is_some_and(|last_seen| now.duration_since(last_seen) < retention)
        });
    }
    let state = states.entry((group_id, user_id)).or_default();
    suppress_direct_trigger(
        state,
        &normalize_for_spam_detection(message),
        now,
        Duration::from_secs(limits.direct_repeat_window_secs()),
        Duration::from_secs(limits.direct_spam_cooldown_secs()),
        Duration::from_secs(limits.direct_rate_window_secs()),
        limits.direct_rate_limit(),
    )
}

fn suppress_direct_trigger(
    state: &mut DirectTriggerState,
    normalized_message: &str,
    now: Instant,
    repeat_window: Duration,
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

    let repeated = !normalized_message.is_empty()
        && normalized_message == state.last_message
        && state
            .last_message_at
            .is_some_and(|last_at| now.duration_since(last_at) < repeat_window);
    state.repeated_count = if repeated {
        state.repeated_count.saturating_add(1)
    } else {
        1
    };
    state.last_message.clear();
    state.last_message.push_str(normalized_message);
    state.last_message_at = Some(now);

    if state.repeated_count >= 3 {
        state.blocked_until = Some(now + cooldown);
    }
    repeated
}

fn normalize_for_spam_detection(message: &str) -> String {
    message
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .take(200)
        .collect()
}

fn is_bot_admin(bot: &RuntimeBot, user_id: i64) -> bool {
    bot.get_all_admin()
        .map(|admins| admins.contains(&user_id))
        .unwrap_or(false)
}

fn is_admin_command(message: &str) -> bool {
    matches!(
        message.trim(),
        "#系统信息"
            | "#重载配置文件"
            | "#重载全部配置"
            | "#启用自动重载"
            | "#禁用自动重载"
            | "#检查配置变化"
            | "#自动重载状态"
            | "#健康检查"
            | "#禁言"
            | "#结束禁言"
    )
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
    message: &str,
    has_image: bool,
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
    conversation_message_is_relevant(state, user_id, message, has_image, now, open_floor)
}

async fn should_continue_conversation(
    group_id: i64,
    user_id: i64,
    message: &str,
    has_image: bool,
) -> bool {
    is_conversation_participant_message(group_id, user_id, message, has_image).await
}

fn conversation_message_is_relevant(
    state: &GroupInterjectionState,
    user_id: i64,
    message: &str,
    has_image: bool,
    now: Instant,
    open_floor: Duration,
) -> bool {
    if !is_meaningful_conversation_message(message, has_image) {
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

fn is_meaningful_conversation_message(message: &str, _has_image: bool) -> bool {
    let text = message.trim();
    if text.starts_with('#') {
        return false;
    }
    text.chars().any(|character| character.is_alphanumeric())
}

fn should_defer_active_window_message(
    active_reply: bool,
    participant_follow_up: bool,
    has_explicit_interrupt: bool,
) -> bool {
    active_reply && participant_follow_up && !has_explicit_interrupt
}

async fn queue_pending_window_message(
    group_id: i64,
    user_id: i64,
    nickname: String,
    sender: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
) {
    let mut pending = PENDING_WINDOW_MESSAGES.lock().await;
    if let Some(existing) = pending.get_mut(&group_id) {
        existing.user_id = user_id;
        existing.nickname = nickname;
        existing.sender = sender;
        existing.message = message;
        merge_vision_images(&mut existing.vision_images, vision_images);
        merge_message_ids(&mut existing.message_ids, message_ids);
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
        let replied = crate::model::utils::silence(
            group_id,
            pending.user_id,
            &pending.message,
            bot.clone(),
            pending.sender,
            ticket,
            None,
            pending.vision_images,
            pending.message_ids,
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

/// 仅用本地有效内容、计数、额度和概率筛选未点名接话机会；这里不会请求模型。
async fn should_interject(group_id: i64, message: &str) -> bool {
    let config = config::get().group_interjection().clone();
    if !config.enabled() || !is_interjection_candidate(message, config.min_message_chars()) {
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
    });
}

fn is_interjection_candidate(message: &str, min_message_chars: usize) -> bool {
    let text = message.trim();
    if text.starts_with('#') {
        return false;
    }
    // 只在本地统计有实际文字内容的消息；是否值得接话仍由抽样后的模型判断。
    text.chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        >= min_message_chars
}

fn is_addressed_to_bot(event: &GroupMsgEvent, message: &str) -> bool {
    let self_id = event.self_id.to_string();
    let mentioned = event.message.iter().any(|segment| {
        if segment.type_ != "at" {
            return false;
        }
        let qq = segment.data.get("qq");
        qq.and_then(|value| value.as_str()) == Some(self_id.as_str())
            || qq.and_then(|value| value.as_i64()) == Some(event.self_id)
    });
    let text = message.trim_start();
    mentioned || text.starts_with("芸汐") || text.starts_with("云汐")
}

async fn update_group_profile(group_id: i64, user_id: i64, message: &str, _nickname: &str) {
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

    // 提取话题关键词
    let topics = extract_topics_from_message(message);
    for topic in topics {
        if !profile.conversation_topics.contains(&topic) {
            profile.conversation_topics.push(topic);
        }
    }

    // 限制话题数量
    if profile.conversation_topics.len() > 20 {
        profile
            .conversation_topics
            .drain(0..profile.conversation_topics.len() - 20);
    }

    profile.group_personality = infer_group_personality(message, &profile.group_personality);

    // 更新群组档案
    if let Err(e) = MEMORY_MANAGER.update_group_profile(group_id, profile).await {
        eprintln!("[ERROR] 更新群组档案失败 (群组: {}): {}", group_id, e);
    }
}

fn infer_group_personality(message: &str, current: &str) -> String {
    if ["哈哈", "笑死", "好玩", "开心"]
        .iter()
        .any(|keyword| message.contains(keyword))
    {
        "lively".to_string()
    } else if ["技术", "代码", "编程", "论文", "学习"]
        .iter()
        .any(|keyword| message.contains(keyword))
    {
        "knowledgeable".to_string()
    } else if ["难过", "担心", "安慰", "加油"]
        .iter()
        .any(|keyword| message.contains(keyword))
    {
        "supportive".to_string()
    } else {
        current.to_string()
    }
}

fn extract_topics_from_message(message: &str) -> Vec<String> {
    let mut topics = Vec::new();
    let message_lower = message.to_lowercase();

    let topic_keywords = [
        (
            "游戏",
            vec!["游戏", "打游戏", "玩", "lol", "王者", "吃鸡", "steam"],
        ),
        ("学习", vec!["学习", "考试", "课程", "知识", "作业", "论文"]),
        ("工作", vec!["工作", "上班", "加班", "项目", "会议", "同事"]),
        ("生活", vec!["生活", "日常", "今天", "昨天", "明天", "计划"]),
        ("娱乐", vec!["电影", "音乐", "看书", "听歌", "追剧", "综艺"]),
        ("美食", vec!["吃", "美食", "餐厅", "料理", "做饭", "外卖"]),
        (
            "旅行",
            vec!["旅行", "旅游", "出去玩", "度假", "景点", "攻略"],
        ),
        ("运动", vec!["运动", "跑步", "健身", "锻炼", "瑜伽", "游泳"]),
        ("科技", vec!["科技", "AI", "编程", "技术", "互联网", "手机"]),
        ("情感", vec!["情感", "心情", "开心", "难过", "生气", "担心"]),
    ];

    for (category, keywords) in &topic_keywords {
        for keyword in keywords {
            if message_lower.contains(keyword) {
                topics.push(category.to_string());
                break;
            }
        }
    }

    topics
}

#[cfg(test)]
mod tests {
    use super::{
        DirectTriggerState, GroupInterjectionState, complete_interjection_attempt,
        conversation_message_is_relevant, decision_budget_available, extract_topics_from_message,
        has_active_conversation_window, infer_group_personality, is_admin_command,
        is_interjection_candidate, normalize_for_spam_detection, prune_decision_attempts,
        should_defer_active_window_message, suppress_direct_trigger,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn group_topics_and_personality_are_learned() {
        let topics = extract_topics_from_message("最近在学习 Rust 编程和 AI 技术");
        assert!(topics.contains(&"学习".to_string()));
        assert!(topics.contains(&"科技".to_string()));
        assert_eq!(
            infer_group_personality("一起讨论代码和技术吧", "friendly"),
            "knowledgeable"
        );
    }

    #[test]
    fn ordinary_meaningful_messages_become_interjection_candidates() {
        assert!(is_interjection_candidate("你们觉得 Rust 好学吗？", 4));
        assert!(is_interjection_candidate("今天晚上吃火锅", 4));
        assert!(is_interjection_candidate("刚刚下班回家", 4));
        assert!(!is_interjection_candidate("嗯嗯", 4));
        assert!(!is_interjection_candidate("[图片]", 4));
        assert!(!is_interjection_candidate("#某个命令", 4));
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
            "今天买了新的杯子",
            false,
            now,
            Duration::from_secs(45),
        ));
        assert!(conversation_message_is_relevant(
            &state,
            42,
            "嗯",
            false,
            now,
            Duration::from_secs(45),
        ));
        assert!(!conversation_message_is_relevant(
            &state,
            42,
            "",
            true,
            now,
            Duration::from_secs(45),
        ));
        assert!(!conversation_message_is_relevant(
            &state,
            99,
            "这是群里另一段聊天",
            false,
            now,
            Duration::from_secs(45),
        ));

        state.last_bot_reply_at = Some(now - Duration::from_secs(10));
        assert!(conversation_message_is_relevant(
            &state,
            99,
            "我也想说一句",
            false,
            now,
            Duration::from_secs(45),
        ));
        assert!(!conversation_message_is_relevant(
            &state,
            42,
            "#系统信息",
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
    fn ordinary_window_messages_do_not_cancel_an_active_reply() {
        assert!(should_defer_active_window_message(true, true, false));
        assert!(!should_defer_active_window_message(true, true, true));
        assert!(!should_defer_active_window_message(true, false, false));
        assert!(!should_defer_active_window_message(false, true, false));
    }

    #[test]
    fn operational_commands_require_an_admin() {
        assert!(is_admin_command("#健康检查"));
        assert!(is_admin_command(" #禁言 "));
        assert!(!is_admin_command("芸汐，今天开心吗"));
        assert!(!is_admin_command("#教芸汐 这个表情是开心"));
    }

    #[test]
    fn repeated_direct_mentions_are_silenced_then_cooled_down() {
        let mut state = DirectTriggerState::default();
        let started = Instant::now();
        let repeat_window = Duration::from_secs(120);
        let cooldown = Duration::from_secs(600);
        let rate_window = Duration::from_secs(60);

        assert!(!suppress_direct_trigger(
            &mut state,
            "芸汐你好",
            started,
            repeat_window,
            cooldown,
            rate_window,
            4,
        ));
        assert!(suppress_direct_trigger(
            &mut state,
            "芸汐你好",
            started + Duration::from_secs(5),
            repeat_window,
            cooldown,
            rate_window,
            4,
        ));
        assert!(suppress_direct_trigger(
            &mut state,
            "芸汐你好",
            started + Duration::from_secs(10),
            repeat_window,
            cooldown,
            rate_window,
            4,
        ));
        assert!(suppress_direct_trigger(
            &mut state,
            "换一句也还在冷却",
            started + Duration::from_secs(20),
            repeat_window,
            cooldown,
            rate_window,
            4,
        ));
        assert!(!suppress_direct_trigger(
            &mut state,
            "冷却后正常聊天",
            started + Duration::from_secs(620),
            repeat_window,
            cooldown,
            rate_window,
            4,
        ));
    }

    #[test]
    fn varied_high_frequency_mentions_are_rate_limited() {
        let mut state = DirectTriggerState::default();
        let started = Instant::now();
        for offset in [0, 5, 10, 15] {
            assert!(!suppress_direct_trigger(
                &mut state,
                &format!("不同消息{offset}"),
                started + Duration::from_secs(offset),
                Duration::from_secs(120),
                Duration::from_secs(300),
                Duration::from_secs(60),
                4,
            ));
        }
        assert!(suppress_direct_trigger(
            &mut state,
            "第五条",
            started + Duration::from_secs(20),
            Duration::from_secs(120),
            Duration::from_secs(300),
            Duration::from_secs(60),
            4,
        ));
        assert_eq!(
            normalize_for_spam_detection(" 芸汐，你好！！！ "),
            "芸汐你好"
        );
    }
}
