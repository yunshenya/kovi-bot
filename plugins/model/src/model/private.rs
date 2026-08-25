use crate::agent_tasks;
use crate::config;
use crate::group_access;
use crate::memory::MEMORY_MANAGER;
use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::conversation_coordinator::{ConversationCoordinator, PendingTurn};
use crate::model::interrupt::{ReplyScope, clear_reply_state_locked, is_active, scope_mutex};
use crate::model::recall::{
    clear_reply_scope_locked, has_recalled_messages, is_recent_bot_message,
    recent_bot_message_for_reaction, send_tracked_private_message,
};
use crate::model::reply::{clear_reply_targets, record_reply_target};
use crate::model::semantic::{
    ImageReferenceIntent, MessageUnderstanding, SemanticImageIntent, UnderstandingRequest,
    understand,
};
use crate::model::traffic::{InboundScope, bounded_input, clear_private_traffic, should_suppress};
use crate::model::utils::{
    clear_private_runtime_data, command_help, is_bot_admin, is_group_admin_command,
    is_help_command, is_restricted_command, private_chat_claimed, send_sys_info_private,
};
use crate::private_image_memory::{
    RecentPrivateImage, forget_private_user_images, recent_private_images, remember_private_images,
};
use crate::reminders;
use crate::sticker_memory;
use crate::sticker_memory::{
    StickerCandidateCommand, StickerScope, confirm_candidate, dismiss_candidate, extract_stickers,
    format_candidate_list, has_reply, has_usage, known_labels, parse_candidate_command,
    pending_candidates, quoted_message_context, stickers_for_teaching, teach, teaching_label,
    with_quoted_context, with_sticker_context, with_sticker_reaction_context,
    with_unknown_sticker_context,
};
use crate::vision::{
    ImageRequestScope, VisionImage, clear_user_pending_image_requests,
    consume_pending_image_request, default_vision_prompt, extract_image_attachments,
    is_vision_command, merge_image_attachments, resolve_image_urls, strip_vision_command,
    with_social_image_context,
};
use kovi::event::PrivateMsgEvent;
use kovi::tokio::sync::Mutex;
use kovi::{Message, RuntimeBot};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

static PRIVATE_MESSAGE_BATCHES: LazyLock<MessageCoalescer<i64>> = LazyLock::new(Default::default);

type PendingPrivateMessage = PendingTurn;

/// 当前私聊回复期间保存有界 FIFO，完整保留每个 turn 的正文、附件和消息 ID。
static PENDING_PRIVATE_MESSAGES: LazyLock<Mutex<HashMap<i64, VecDeque<PendingPrivateMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let reply_scope = ReplyScope::Private(user_id);
    if event.user_id == event.self_id {
        println!(
            "[INFO] 忽略私聊自发消息回流 (用户: {}, 消息: {})",
            user_id, event.message_id
        );
        return;
    }
    let bounded_message = bounded_input(event.borrow_text().unwrap_or_default());
    let message = bounded_message.as_str();
    let sender_is_admin = is_bot_admin(&bot, user_id);
    if is_restricted_command(message) && !sender_is_admin {
        println!("[INFO] 私聊未授权命令已静默 (用户: {})", user_id);
        return;
    }
    if is_help_command(message) {
        send_tracked_private_message(&bot, user_id, command_help()).await;
        return;
    }
    if group_access::is_authorization_command(message) {
        let reply = group_access::handle_command(&bot, message, None, user_id)
            .await
            .unwrap_or_else(|| group_access::command_help().to_string());
        send_tracked_private_message(&bot, user_id, &reply).await;
        return;
    }
    if let Some(command) = parse_agent_task_command(message) {
        if bot.get_main_admin().ok() != Some(user_id) {
            println!("[INFO] 私聊跨群任务命令未授权 (用户: {})", user_id);
            return;
        }
        if matches!(command, AgentTaskCommand::Cancel(_)) {
            stop_private_reply(user_id).await;
        }
        let reply = match command {
            AgentTaskCommand::List => agent_tasks::task_status_report(user_id, None).await,
            AgentTaskCommand::Status(task_id) => {
                agent_tasks::task_status_report(user_id, task_id).await
            }
            AgentTaskCommand::Cancel(task_id) => agent_tasks::cancel_task(user_id, task_id).await,
            AgentTaskCommand::Help => Ok(agent_tasks::task_command_help().to_string()),
            AgentTaskCommand::Invalid => Ok(agent_tasks::task_command_help().to_string()),
        }
        .unwrap_or_else(|error| {
            eprintln!("[ERROR] 处理私聊跨群任务命令失败: {}", error);
            "这次没能读取跨群问答任务，请稍后再试。".to_string()
        });
        send_tracked_private_message(&bot, user_id, reply).await;
        return;
    }
    if message.trim() == "#系统信息" && sender_is_admin {
        send_sys_info_private(Arc::clone(&bot), user_id).await;
        return;
    }
    if is_group_admin_command(message) {
        println!(
            "[INFO] 私聊群聊专用命令已忽略 (用户: {}, 命令: {})",
            user_id,
            message.trim()
        );
        return;
    }
    if let Some(command) = parse_candidate_command(message) {
        let reply = match command {
            StickerCandidateCommand::List => match pending_candidates(None, 20).await {
                Ok(candidates) => format_candidate_list(&candidates),
                Err(error) => {
                    eprintln!("[ERROR] 读取私聊表情包候选失败: {}", error);
                    "暂时读取不到待确认表情候选，请稍后再试。".to_string()
                }
            },
            StickerCandidateCommand::Confirm {
                candidate_id,
                label,
            } => match confirm_candidate(candidate_id, &label, user_id, None).await {
                Ok(true) => format!("已确认候选 {}，以后这个表情表示“{}”。", candidate_id, label),
                Ok(false) => "找不到这个待确认候选，可能已经处理过。".to_string(),
                Err(error) => {
                    eprintln!("[ERROR] 确认私聊表情包候选失败: {}", error);
                    "这次没能确认这个表情候选，请稍后再试。".to_string()
                }
            },
            StickerCandidateCommand::Reject { candidate_id } => {
                match dismiss_candidate(candidate_id, user_id, None, false, 30).await {
                    Ok(true) => format!("已驳回候选 {}，近期不会重复提醒。", candidate_id),
                    Ok(false) => "找不到这个待确认候选，可能已经处理过。".to_string(),
                    Err(error) => {
                        eprintln!("[ERROR] 驳回私聊表情包候选失败: {}", error);
                        "这次没能驳回这个表情候选，请稍后再试。".to_string()
                    }
                }
            }
            StickerCandidateCommand::Ignore { candidate_id, days } => {
                match dismiss_candidate(candidate_id, user_id, None, true, days).await {
                    Ok(true) => format!("已忽略候选 {}，{} 天内不会重复提醒。", candidate_id, days),
                    Ok(false) => "找不到这个待确认候选，可能已经处理过。".to_string(),
                    Err(error) => {
                        eprintln!("[ERROR] 忽略私聊表情包候选失败: {}", error);
                        "这次没能忽略这个表情候选，请稍后再试。".to_string()
                    }
                }
            }
            StickerCandidateCommand::Invalid => {
                "格式：#待确认表情、#确认表情 编号 含义、#驳回表情 编号、#忽略表情 编号 [天数]。"
                    .to_string()
            }
        };
        send_tracked_private_message(&bot, user_id, reply).await;
        return;
    }
    match message.trim() {
        "#删除我的数据" => {
            send_tracked_private_message(
                &bot,
                user_id,
                "这会删除你的私聊记忆、用户档案、摘要、近期图片、与你关联的表情记忆、角色动作记录，以及 Yunxi Core 中的身份映射、关系、情绪、待办和目标。若确认，请发送：#删除我的数据 确认",
            )
            .await;
            return;
        }
        "#删除我的数据 确认" => {
            delete_private_user_data(user_id, event.self_id, &bot).await;
            return;
        }
        _ => {}
    }
    if should_suppress(InboundScope::Private(user_id), sender_is_admin).await {
        println!("[INFO] 私聊入站流量已抑制 (用户: {})", user_id);
        return;
    }
    if is_recent_bot_message(reply_scope, event.message_id).await {
        println!(
            "[INFO] 忽略私聊已记录消息回流 (用户: {}, 消息: {})",
            user_id, event.message_id
        );
        return;
    }
    let nick_name = normalized_private_sender_name(&event.get_sender_nickname());
    let stickers = extract_stickers(&event.message);
    let current_images = extract_image_attachments(&event.message);
    let vision_command = is_vision_command(message);
    record_reply_target(
        reply_scope,
        event.message_id,
        Some(user_id),
        nick_name.clone(),
        &event.human_text,
    )
    .await;
    if let Some(label) = teaching_label(message) {
        match stickers_for_teaching(&event.message, &bot, StickerScope::Private(user_id)).await {
            Ok(teaching_stickers) if !teaching_stickers.is_empty() => {
                match teach(
                    &teaching_stickers,
                    &label,
                    user_id,
                    StickerScope::Private(user_id),
                )
                .await
                {
                    Ok(count) => {
                        send_tracked_private_message(
                            &bot,
                            user_id,
                            format!("记住啦，这 {count} 个表情以后表示“{label}”。"),
                        )
                        .await;
                    }
                    Err(error) => {
                        eprintln!("[ERROR] 私聊保存表情包记忆失败: {}", error);
                        send_tracked_private_message(
                            &bot,
                            user_id,
                            "这次没能记住，稍后再教我一次吧。",
                        )
                        .await;
                    }
                }
            }
            Ok(_) => {
                send_tracked_private_message(
                    &bot,
                    user_id,
                    "请回复（引用）那张表情包，再发送 #教芸汐 这个表情是……哦。",
                )
                .await;
            }
            Err(error) => {
                eprintln!("[ERROR] 私聊读取被引用表情失败: {}", error);
                send_tracked_private_message(
                    &bot,
                    user_id,
                    "我没能读到被引用的表情，请重新引用后再试一次哦。",
                )
                .await;
            }
        }
        return;
    }

    let sticker_scope = StickerScope::Private(user_id);
    let sticker_teaching_message = (sender_is_admin
        && (!stickers.is_empty() || has_reply(&event.message)))
    .then(|| event.message.clone());
    let labels = match known_labels(&stickers, sticker_scope).await {
        Ok(labels) => labels,
        Err(error) => {
            eprintln!("[ERROR] 私聊读取表情包记忆失败: {}", error);
            Vec::new()
        }
    };
    let quoted = match quoted_message_context(&event.message, &bot, sticker_scope).await {
        Ok(quoted) => quoted,
        Err(error) => {
            eprintln!("[ERROR] 私聊读取引用消息失败: {}", error);
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
    let text_message = if vision_command {
        strip_vision_command(message)
    } else {
        message.to_string()
    };
    remember_private_images(user_id, event.message_id, &current_images, &text_message).await;
    if let Some(quoted) = quoted.as_ref()
        && let Some(message_id) = quoted.message_id
    {
        remember_private_images(user_id, message_id, &quoted.images, &quoted.content).await;
    }
    let mut excluded_recent_message_ids = vec![event.message_id];
    if let Some(message_id) = quoted.as_ref().and_then(|quoted| quoted.message_id) {
        excluded_recent_message_ids.push(message_id);
    }
    let recent_images = recent_private_images(user_id, &excluded_recent_message_ids).await;
    let mut images = merge_image_attachments(&current_images, quoted_images);
    let pending_image_request =
        consume_pending_image_request(ImageRequestScope::Private(user_id), !images.is_empty())
            .await;
    let recent_sticker_reaction =
        if !stickers.is_empty() && !vision_command && !pending_image_request {
            recent_bot_message_for_reaction(
                reply_scope,
                Duration::from_secs(
                    config::get()
                        .group_interjection()
                        .sticker_reaction_window_secs(),
                ),
            )
            .await
        } else {
            None
        };
    let sticker_reaction = recent_sticker_reaction.is_some();
    let active_reply = is_active(reply_scope).await;
    let immediate_stop = active_reply && looks_like_immediate_stop_request(message);
    let can_interrupt = vision_command;
    let reply_ticket = if can_interrupt {
        Some(ConversationCoordinator::interrupt(reply_scope).await)
    } else {
        None
    };
    if immediate_stop {
        stop_private_reply(user_id).await;
        println!("[INFO] 私聊用户打断回复 (用户: {})", user_id);
        return;
    }
    let initial_recent_reference = if vision_command && !text_message.trim().is_empty() {
        ImageReferenceIntent::Described
    } else if vision_command {
        ImageReferenceIntent::Recent
    } else {
        ImageReferenceIntent::None
    };
    let initial_recent_images = if images.is_empty() {
        select_recent_images(&recent_images, initial_recent_reference)
    } else {
        Vec::new()
    };
    if !initial_recent_images.is_empty() {
        images = merge_image_attachments(
            &images,
            &initial_recent_images
                .iter()
                .map(|image| image.attachment.clone())
                .collect::<Vec<_>>(),
        );
    }
    let conversational_image =
        !images.is_empty() && labels.is_empty() && !vision_command && !pending_image_request;
    // 合并前只做确定性判断；完整语义分析在批次完成后仅调用一次。
    let mut vision_requested = vision_command
        || pending_image_request
        || conversational_image
        || !initial_recent_images.is_empty();
    if vision_command && images.is_empty() {
        send_tracked_private_message(
            &bot,
            user_id,
            "我没在最近的对话里找到可以看的图片，引用那张图或重新发一次给我吧。",
        )
        .await;
        return;
    }
    if message.trim().is_empty() && stickers.is_empty() && !has_reply(&event.message) {
        return;
    }

    let sticker_used_before = if stickers.is_empty() {
        false
    } else {
        match has_usage(&stickers, sticker_scope).await {
            Ok(used) => used,
            Err(error) => {
                eprintln!("[ERROR] 私聊读取表情包使用记录失败: {}", error);
                false
            }
        }
    };
    let current_message = if vision_requested && text_message.trim().is_empty() {
        if conversational_image {
            private_social_image_prompt().to_string()
        } else {
            default_vision_prompt().to_string()
        }
    } else if labels.is_empty() && !stickers.is_empty() {
        with_unknown_sticker_context(&text_message, stickers.len(), sticker_used_before)
    } else {
        with_sticker_context(&text_message, &labels)
    };
    let current_message = if sticker_reaction {
        recent_sticker_reaction
            .as_ref()
            .map_or(current_message.clone(), |bot_message| {
                with_sticker_reaction_context(&current_message, &bot_message.content)
            })
    } else {
        current_message
    };
    let mut model_message = quoted.as_ref().map_or(current_message.clone(), |quoted| {
        with_quoted_context(&current_message, quoted)
    });
    if !initial_recent_images.is_empty() {
        model_message = with_recent_image_context(
            &model_message,
            &initial_recent_images,
            initial_recent_reference,
        );
    }
    let (
        model_message,
        plain_text,
        intent_text,
        batch_vision_requested,
        batch_sticker_reaction,
        mut images,
        source_message_ids,
    ) = if !message.trim_start().starts_with('#') {
        let Some(combined) = PRIVATE_MESSAGE_BATCHES
            .push(
                user_id,
                MessagePart {
                    text: model_message,
                    intent_text: text_message.clone(),
                    addressed: false,
                    plain_text: stickers.is_empty() && quoted.is_none(),
                    vision_requested,
                    sticker_reaction,
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
            combined.plain_text,
            combined.intent_text,
            combined.vision_requested,
            combined.sticker_reaction,
            combined.images,
            combined.message_ids,
        )
    } else {
        (
            model_message,
            false,
            text_message.clone(),
            vision_requested,
            sticker_reaction,
            images,
            vec![event.message_id],
        )
    };
    if has_recalled_messages(reply_scope, &source_message_ids).await {
        println!(
            "[INFO] 私聊输入已撤回，丢弃尚未开始的回复 (用户: {})",
            user_id
        );
        return;
    }
    let batch_recent_images = recent_private_images(user_id, &source_message_ids).await;
    let batch_request = UnderstandingRequest {
        message: intent_text.clone(),
        context: "private_chat_batch".to_string(),
        quoted_message: quoted.as_ref().map(|value| value.content.clone()),
        has_images: !images.is_empty(),
        quoted_has_images: !quoted_images.is_empty(),
        has_recent_images: !batch_recent_images.is_empty(),
        explicit_vision_command: batch_vision_requested,
        pending_image_request: false,
        addressed_to_bot: false,
        conversation_open: is_active(reply_scope).await,
        sticker_reaction: batch_sticker_reaction,
    };
    let understanding = if intent_text.trim_start().starts_with('#') {
        MessageUnderstanding::default()
    } else {
        understand(batch_request.clone()).await
    };
    let asks_for_silence = plain_text && (understanding.wants_no_reply || understanding.wants_stop);
    if asks_for_silence {
        stop_private_reply(user_id).await;
        println!(
            "[INFO] 合并后的私聊消息请求停止当前回复 (用户: {})",
            user_id
        );
        return;
    }
    let selected_recent_images = if images.is_empty() {
        select_recent_images(&batch_recent_images, understanding.image_reference)
    } else {
        Vec::new()
    };
    if !selected_recent_images.is_empty() {
        images = merge_image_attachments(
            &images,
            &selected_recent_images
                .iter()
                .map(|image| image.attachment.clone())
                .collect::<Vec<_>>(),
        );
    }
    vision_requested = batch_vision_requested
        || understanding.should_understand_image(&batch_request)
        || !selected_recent_images.is_empty();
    let social_vision_requested = vision_requested
        && batch_vision_requested
        && !vision_command
        && !pending_image_request
        && understanding.image_intent != SemanticImageIntent::Understand;
    if intent_text.trim().is_empty()
        && !vision_requested
        && model_message.trim().is_empty()
        && (!images.is_empty() || !stickers.is_empty())
    {
        println!("[INFO] 收到私聊纯图片状态，保持静默 (用户: {})", user_id);
        return;
    }
    let model_message = if !selected_recent_images.is_empty() {
        with_recent_image_context(
            &model_message,
            &selected_recent_images,
            understanding.image_reference,
        )
    } else if understanding.image_reference != ImageReferenceIntent::None && images.is_empty() {
        with_missing_recent_image_context(&model_message)
    } else if (!images.is_empty() && !vision_requested && !intent_text.trim().is_empty())
        || (vision_requested && understanding.image_intent == SemanticImageIntent::Conversational)
        || social_vision_requested
    {
        with_social_image_context(&model_message)
    } else {
        model_message
    };
    let vision_images = if vision_requested {
        match resolve_image_urls(&images, &bot).await {
            Ok(images) if !images.is_empty() => images,
            Ok(_) => {
                send_tracked_private_message(
                    &bot,
                    user_id,
                    "我暂时拿不到这张截图的内容，再发一次或换张图试试吧。",
                )
                .await;
                return;
            }
            Err(error) => {
                eprintln!("[ERROR] 私聊读取截图失败 (用户: {}): {}", user_id, error);
                send_tracked_private_message(
                    &bot,
                    user_id,
                    "我暂时读不到这张截图，再发一次或换张图试试吧。",
                )
                .await;
                return;
            }
        }
    } else {
        Vec::new()
    };
    if !stickers.is_empty()
        && let Err(error) = sticker_memory::record_usage(
            &stickers,
            sticker_scope,
            event.message_id,
            &text_message,
            recent_sticker_reaction
                .as_ref()
                .map(|message| message.content.as_str())
                .unwrap_or_default(),
            Arc::clone(&bot),
        )
        .await
    {
        eprintln!("[ERROR] 私聊保存表情包使用记录失败: {}", error);
    }
    let Some(reply_ticket) = claim_or_queue_private_reply(
        reply_scope,
        reply_ticket,
        user_id,
        nick_name.clone(),
        model_message.clone(),
        vision_images.clone(),
        source_message_ids.clone(),
        sticker_teaching_message.clone(),
        understanding.clone(),
    )
    .await
    else {
        return;
    };
    private_chat_claimed(
        user_id,
        &model_message,
        nick_name,
        Arc::clone(&bot),
        reply_ticket,
        vision_images,
        source_message_ids,
        sticker_teaching_message,
        understanding,
    )
    .await;
    drain_pending_private_messages(user_id, Arc::clone(&bot), reply_ticket).await;
}

#[allow(clippy::too_many_arguments)]
async fn claim_or_queue_private_reply(
    scope: ReplyScope,
    ticket: Option<crate::model::ReplyTicket>,
    user_id: i64,
    nickname: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    sticker_teaching_message: Option<Message>,
    understanding: MessageUnderstanding,
) -> Option<crate::model::ReplyTicket> {
    let scope_lock = scope_mutex(scope);
    let _scope_guard = scope_lock.lock().await;
    let active = ConversationCoordinator::is_active_locked(scope).await;
    let has_queued = PENDING_PRIVATE_MESSAGES
        .lock()
        .await
        .get(&user_id)
        .is_some_and(|queue| !queue.is_empty());
    if should_defer_active_private_message(active || has_queued, ticket.is_some()) {
        println!(
            "[INFO] 私聊已有回复或排队消息进行中，排队新消息 (用户: {})",
            user_id
        );
        queue_pending_private_message(
            user_id,
            nickname,
            message,
            vision_images,
            message_ids,
            sticker_teaching_message,
            understanding,
        )
        .await;
        return None;
    }
    let ticket = match ticket {
        Some(ticket) => ticket,
        None => ConversationCoordinator::interrupt_locked(scope).await,
    };
    ConversationCoordinator::begin_reply_locked(scope, ticket, message_ids)
        .await
        .then_some(ticket)
}

async fn stop_private_reply(user_id: i64) {
    let scope = ReplyScope::Private(user_id);
    let scope_lock = scope_mutex(scope);
    let _scope_guard = scope_lock.lock().await;
    ConversationCoordinator::interrupt_locked(scope).await;
    PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
    PENDING_PRIVATE_MESSAGES.lock().await.remove(&user_id);
}

async fn delete_private_user_data(user_id: i64, self_id: i64, bot: &RuntimeBot) {
    let scope = ReplyScope::Private(user_id);
    {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        ConversationCoordinator::interrupt_locked(scope).await;
        PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
        PENDING_PRIVATE_MESSAGES.lock().await.remove(&user_id);
        clear_reply_scope_locked(scope).await;
    }

    // `begin_qq_user_data_erasure` synchronously closes the ingress gate on
    // its first poll. Keep it immediately after releasing the reply-scope lock
    // so an old model turn is cancelled before the Core FIFO barrier waits for
    // it, without holding that same lock across the barrier acknowledgement.
    let erasure = match crate::yunxi::begin_qq_user_data_erasure(user_id).await {
        Ok(erasure) => erasure,
        Err(error) => {
            eprintln!(
                "[ERROR] 启动 Yunxi 数据删除屏障失败 (用户: {}): {error}",
                user_id
            );
            send_tracked_private_message(
                bot,
                user_id,
                "数据删除安全屏障启动失败，未开始删除；请稍后重试。",
            )
            .await;
            return;
        }
    };
    let qq_user_ids = erasure.qq_user_ids().to_vec();
    for alias_user_id in &qq_user_ids {
        clear_private_erasure_runtime_state(*alias_user_id).await;
    }

    let mut memory_rows = 0_u64;
    let mut sticker_rows = 0_u64;
    let mut reminder_rows = 0_u64;
    let mut agent_goal_rows = 0_u64;
    let mut agent_run_rows = 0_u64;
    let mut failures = Vec::new();
    for alias_user_id in &qq_user_ids {
        let (memory, stickers, reminders, agent_goals, agent_runs) = kovi::tokio::join!(
            MEMORY_MANAGER.delete_user_data(*alias_user_id),
            sticker_memory::delete_user_data(*alias_user_id),
            reminders::delete_user_data(*alias_user_id),
            crate::agent_runtime::delete_user_data(*alias_user_id),
            crate::agent_runs::delete_user_data(*alias_user_id),
        );
        match memory {
            Ok(rows) => memory_rows = memory_rows.saturating_add(rows),
            Err(error) => failures.push(format!("memory[{alias_user_id}]: {error}")),
        }
        match stickers {
            Ok(rows) => sticker_rows = sticker_rows.saturating_add(rows),
            Err(error) => failures.push(format!("stickers[{alias_user_id}]: {error}")),
        }
        match reminders {
            Ok(rows) => reminder_rows = reminder_rows.saturating_add(rows),
            Err(error) => failures.push(format!("reminders[{alias_user_id}]: {error}")),
        }
        match agent_goals {
            Ok(rows) => agent_goal_rows = agent_goal_rows.saturating_add(rows),
            Err(error) => failures.push(format!("agent-goals[{alias_user_id}]: {error}")),
        }
        match agent_runs {
            Ok(rows) => agent_run_rows = agent_run_rows.saturating_add(rows),
            Err(error) => failures.push(format!("agent-runs[{alias_user_id}]: {error}")),
        }
    }
    let yunxi_rows = match crate::yunxi::delete_qq_person_domain_data(self_id, user_id).await {
        Ok(rows) => rows,
        Err(error) => {
            failures.push(format!("yunxi-core: {error}"));
            0
        }
    };
    if let Err(error) = erasure.finish().await {
        failures.push(format!("core-barrier-resume: {error}"));
    }

    if failures.is_empty() {
        send_untracked_private_message(
            bot,
            user_id,
            format!(
                "你的可归属数据已删除（QQ 身份 {} 个，记忆/档案/摘要 {memory_rows} 项，表情记忆 {sticker_rows} 项，提醒 {reminder_rows} 项，角色目标 {agent_goal_rows} 项，Agent Run {agent_run_rows} 项，Yunxi Core {yunxi_rows} 项）。",
                qq_user_ids.len()
            ),
        )
        .await;
    } else {
        eprintln!(
            "[ERROR] 用户数据删除未完全成功 (发起用户: {}, 失败: {})",
            user_id,
            failures.join("; ")
        );
        send_untracked_private_message(
            bot,
            user_id,
            "数据删除没有全部完成；已尝试其余可归属数据，请稍后重试或联系管理员检查日志。",
        )
        .await;
    }
}

async fn clear_private_erasure_runtime_state(user_id: i64) {
    let scope = ReplyScope::Private(user_id);
    {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        ConversationCoordinator::interrupt_locked(scope).await;
        PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
        PENDING_PRIVATE_MESSAGES.lock().await.remove(&user_id);
        clear_reply_scope_locked(scope).await;
        clear_reply_state_locked(scope).await;
    }
    clear_private_runtime_data(user_id).await;
    forget_private_user_images(user_id).await;
    clear_reply_targets(scope).await;
    clear_user_pending_image_requests(user_id).await;
    clear_private_traffic(user_id).await;
}

async fn send_untracked_private_message(
    bot: &RuntimeBot,
    user_id: i64,
    content: impl Into<String>,
) -> bool {
    match bot.send_private_msg_return(user_id, content.into()).await {
        Ok(_) => true,
        Err(error) => {
            eprintln!(
                "[ERROR] 私聊无追踪消息发送失败 (用户: {}): {:?}",
                user_id, error
            );
            false
        }
    }
}

fn normalized_private_sender_name(value: &str) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect::<String>();
    if normalized.is_empty() {
        "未设置昵称".to_string()
    } else {
        normalized
    }
}

fn looks_like_immediate_stop_request(message: &str) -> bool {
    let normalized = message
        .trim()
        .trim_matches(|character: char| {
            character.is_ascii_punctuation() || "，。！？…".contains(character)
        })
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "别说了"
            | "不要说了"
            | "别回复了"
            | "不要回复了"
            | "停下"
            | "停止回复"
            | "闭嘴"
            | "stop"
            | "stop replying"
    )
}

fn private_social_image_prompt() -> &'static str {
    "对方发来了一张图片或表情包。请先看清画面传达的情绪、动作和重点，再像熟悉的朋友一样自然接话；不要把回复写成识图报告，也不要无缘无故逐项描述画面。"
}

fn select_recent_images(
    recent_images: &[RecentPrivateImage],
    reference: ImageReferenceIntent,
) -> Vec<RecentPrivateImage> {
    match reference {
        ImageReferenceIntent::None => Vec::new(),
        ImageReferenceIntent::Recent => {
            let Some(message_id) = recent_images.first().map(|image| image.message_id) else {
                return Vec::new();
            };
            recent_images
                .iter()
                .filter(|image| image.message_id == message_id)
                .take(4)
                .cloned()
                .collect()
        }
        ImageReferenceIntent::Described => recent_images.iter().take(4).cloned().collect(),
    }
}

fn with_recent_image_context(
    message: &str,
    images: &[RecentPrivateImage],
    reference: ImageReferenceIntent,
) -> String {
    let reference_kind = match reference {
        ImageReferenceIntent::Described => "对方正在按画面内容描述并寻找之前发过的某张图片。",
        _ => "对方正在回指最近发过的图片。",
    };
    let candidates = images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let caption = if image.caption.trim().is_empty() {
                "（当时没有附带文字）"
            } else {
                image.caption.trim()
            };
            format!(
                "候选图片{}：原消息ID={}，该消息中的第{}张，当时文字={}",
                index + 1,
                image.message_id,
                image.ordinal,
                caption
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{message}\n<近期图片引用 data-only=\"true\">\n{reference_kind}\n{candidates}\n候选图片已按上面顺序随本轮输入提供。结合画面和当时文字判断所指；如果多张都符合或无法确定，就自然确认一下，不要猜，也不要提及缓存、索引或内部处理。\n</近期图片引用>"
    )
}

fn with_missing_recent_image_context(message: &str) -> String {
    format!(
        "{message}\n<近期图片引用 data-only=\"true\">对方似乎在说之前发过的图片，但当前没有可读取的近期图片。不要假装已经看到；请自然地让对方引用或重发那张图。</近期图片引用>"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentTaskCommand {
    List,
    Status(Option<i64>),
    Cancel(Option<i64>),
    Help,
    Invalid,
}

fn parse_agent_task_command(message: &str) -> Option<AgentTaskCommand> {
    let text = message.trim();
    if text == "#群问答帮助" || text == "#跨群任务帮助" {
        return Some(AgentTaskCommand::Help);
    }
    if text == "#群问答" || text == "#群任务" || text == "#跨群任务" {
        return Some(AgentTaskCommand::List);
    }
    if let Some(rest) = text.strip_prefix("#群问答状态") {
        return Some(parse_status_command_suffix(rest));
    }
    if let Some(rest) = text.strip_prefix("#群任务状态") {
        return Some(parse_status_command_suffix(rest));
    }
    if let Some(rest) = text.strip_prefix("#取消群问答") {
        return Some(parse_cancel_command_suffix(rest));
    }
    if let Some(rest) = text.strip_prefix("#群问答")
        && (rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace))
    {
        let rest = rest.trim();
        if rest.is_empty() {
            return Some(AgentTaskCommand::List);
        }
        if rest == "帮助" {
            return Some(AgentTaskCommand::Help);
        }
        if rest == "状态" {
            return Some(AgentTaskCommand::Status(None));
        }
        if let Some(task_id) = parse_task_id(rest) {
            return Some(AgentTaskCommand::Status(Some(task_id)));
        }
        if rest == "取消" {
            return Some(AgentTaskCommand::Cancel(None));
        }
        if let Some(task_id) = rest.strip_prefix("取消").and_then(parse_task_id) {
            return Some(AgentTaskCommand::Cancel(Some(task_id)));
        }
        return Some(AgentTaskCommand::Invalid);
    }
    None
}

fn parse_status_command_suffix(rest: &str) -> AgentTaskCommand {
    if rest.is_empty() {
        AgentTaskCommand::Status(None)
    } else if rest.chars().next().is_some_and(char::is_whitespace) {
        rest.trim()
            .is_empty()
            .then_some(AgentTaskCommand::Status(None))
            .or_else(|| parse_task_id(rest.trim()).map(|id| AgentTaskCommand::Status(Some(id))))
            .unwrap_or(AgentTaskCommand::Invalid)
    } else {
        AgentTaskCommand::Invalid
    }
}

fn parse_cancel_command_suffix(rest: &str) -> AgentTaskCommand {
    if rest.is_empty() {
        AgentTaskCommand::Cancel(None)
    } else if rest.chars().next().is_some_and(char::is_whitespace) {
        let rest = rest.trim();
        if rest.is_empty() {
            AgentTaskCommand::Cancel(None)
        } else {
            parse_task_id(rest)
                .map(|id| AgentTaskCommand::Cancel(Some(id)))
                .unwrap_or(AgentTaskCommand::Invalid)
        }
    } else {
        AgentTaskCommand::Invalid
    }
}

fn parse_task_id(value: &str) -> Option<i64> {
    let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
    let task_id = value.parse::<i64>().ok()?;
    (task_id > 0).then_some(task_id)
}

fn should_defer_active_private_message(active_reply: bool, has_explicit_interrupt: bool) -> bool {
    active_reply && !has_explicit_interrupt
}

async fn queue_pending_private_message(
    user_id: i64,
    nickname: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    sticker_teaching_message: Option<Message>,
    understanding: MessageUnderstanding,
) {
    let mut pending = PENDING_PRIVATE_MESSAGES.lock().await;
    let queue = pending.entry(user_id).or_default();
    ConversationCoordinator::enqueue(
        queue,
        PendingPrivateMessage {
            user_id,
            sender: nickname,
            message,
            reply_expected: true,
            vision_images,
            message_ids,
            sticker_teaching_message,
            understanding,
        },
        "私聊",
        user_id,
    );
}

async fn drain_pending_private_messages(
    user_id: i64,
    bot: Arc<RuntimeBot>,
    mut completed: crate::model::ReplyTicket,
) {
    loop {
        let Some((pending, ticket)) = take_pending_private_turn(user_id, completed).await else {
            return;
        };

        println!("[INFO] 私聊开始处理排队消息 (用户: {})", user_id);
        private_chat_claimed(
            user_id,
            &pending.message,
            pending.sender,
            bot.clone(),
            ticket,
            pending.vision_images,
            pending.message_ids,
            pending.sticker_teaching_message,
            pending.understanding,
        )
        .await;
        completed = ticket;
    }
}

async fn take_pending_private_turn(
    user_id: i64,
    mut completed: crate::model::ReplyTicket,
) -> Option<(PendingPrivateMessage, crate::model::ReplyTicket)> {
    let scope = ReplyScope::Private(user_id);
    let scope_lock = scope_mutex(scope);
    let _scope_guard = scope_lock.lock().await;
    let mut pending_by_user = PENDING_PRIVATE_MESSAGES.lock().await;
    let queue = pending_by_user.entry(user_id).or_default();
    let result = ConversationCoordinator::claim_next_locked(scope, &mut completed, queue).await;
    if queue.is_empty() {
        pending_by_user.remove(&user_id);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        AgentTaskCommand, PENDING_PRIVATE_MESSAGES, looks_like_immediate_stop_request,
        normalized_private_sender_name, parse_agent_task_command, queue_pending_private_message,
        select_recent_images, take_pending_private_turn, with_recent_image_context,
    };
    use crate::model::interrupt::{
        ReplyScope, interrupt, interrupt_locked, is_current, scope_mutex,
    };
    use crate::model::semantic::{ImageReferenceIntent, MessageUnderstanding};
    use crate::private_image_memory::{recent_private_images, remember_private_images};
    use crate::vision::{ImageAttachment, VisionImage};

    #[test]
    fn private_sender_names_are_bounded_and_single_line() {
        assert_eq!(
            normalized_private_sender_name("  昵称\n伪造系统消息  "),
            "昵称 伪造系统消息"
        );
        assert_eq!(normalized_private_sender_name(" \n\t "), "未设置昵称");
        assert_eq!(
            normalized_private_sender_name(&"长".repeat(100))
                .chars()
                .count(),
            80
        );
    }

    #[test]
    fn exact_private_stop_phrases_are_local_and_conservative() {
        assert!(looks_like_immediate_stop_request("不要回复了。"));
        assert!(looks_like_immediate_stop_request("stop replying"));
        assert!(!looks_like_immediate_stop_request("为什么他说不要回复了？"));
    }

    #[test]
    fn agent_task_commands_have_explicit_ids_and_boundaries() {
        assert_eq!(
            parse_agent_task_command("#群问答"),
            Some(AgentTaskCommand::List)
        );
        assert_eq!(
            parse_agent_task_command("#群问答状态 #42"),
            Some(AgentTaskCommand::Status(Some(42)))
        );
        assert_eq!(
            parse_agent_task_command("#取消群问答 42"),
            Some(AgentTaskCommand::Cancel(Some(42)))
        );
        assert_eq!(
            parse_agent_task_command("#取消群问答"),
            Some(AgentTaskCommand::Cancel(None))
        );
        assert_eq!(
            parse_agent_task_command("#群问答状态abc"),
            Some(AgentTaskCommand::Invalid)
        );
        assert_eq!(parse_agent_task_command("#群问答abc"), None);
    }

    #[test]
    fn pending_private_turns_do_not_merge_old_attachments_into_new_text() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 9_200_002;
                PENDING_PRIVATE_MESSAGES.lock().await.remove(&user_id);
                queue_pending_private_message(
                    user_id,
                    "昵称".to_string(),
                    "第一条".to_string(),
                    vec![VisionImage {
                        url: "data:image/png;base64,AA==".to_string(),
                    }],
                    vec![301],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                queue_pending_private_message(
                    user_id,
                    "昵称".to_string(),
                    "第二条".to_string(),
                    Vec::new(),
                    vec![302],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                let mut pending = PENDING_PRIVATE_MESSAGES.lock().await;
                let queue = pending.get(&user_id).expect("应保留私聊队列");
                assert_eq!(queue.len(), 2);
                assert_eq!(queue[0].message, "第一条");
                assert_eq!(queue[0].message_ids, vec![301]);
                assert_eq!(queue[0].vision_images.len(), 1);
                assert_eq!(queue[1].message, "第二条");
                assert_eq!(queue[1].message_ids, vec![302]);
                assert!(queue[1].vision_images.is_empty());
                pending.remove(&user_id);
            });
    }

    #[test]
    fn stale_private_drainer_leaves_a_turn_queued_after_a_new_message_wins() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 9_200_003;
                let scope = ReplyScope::Private(user_id);
                PENDING_PRIVATE_MESSAGES.lock().await.remove(&user_id);
                let completed = interrupt(scope).await;
                queue_pending_private_message(
                    user_id,
                    "昵称".to_string(),
                    "旧排队消息".to_string(),
                    Vec::new(),
                    vec![303],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;

                let new_message_won = std::sync::Arc::new(kovi::tokio::sync::Notify::new());
                let new_task = {
                    let scope_lock = scope_mutex(scope);
                    let new_message_won = std::sync::Arc::clone(&new_message_won);
                    kovi::tokio::spawn(async move {
                        let _scope_guard = scope_lock.lock().await;
                        let ticket = interrupt_locked(scope).await;
                        new_message_won.notify_one();
                        ticket
                    })
                };
                new_message_won.notified().await;

                let drainer = kovi::tokio::spawn(async move {
                    take_pending_private_turn(user_id, completed).await
                });
                let new_ticket = new_task.await.expect("新消息任务应正常结束");
                assert!(drainer.await.expect("旧 drainer 应正常结束").is_none());

                let mut pending = PENDING_PRIVATE_MESSAGES.lock().await;
                let queue = pending.get(&user_id).expect("旧 turn 应被放回队列");
                assert_eq!(queue.len(), 1);
                assert_eq!(queue[0].message, "旧排队消息");
                assert_eq!(queue[0].message_ids, vec![303]);
                pending.remove(&user_id);
                assert!(is_current(new_ticket).await);
            });
    }
    fn image(key: &str) -> ImageAttachment {
        ImageAttachment {
            key: key.to_string(),
            file: Some(format!("{key}.png")),
            url: None,
        }
    }

    #[test]
    fn generic_reference_uses_the_latest_message_while_description_keeps_candidates() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 8_700_001;
                remember_private_images(
                    user_id,
                    41,
                    &[image("older-one"), image("older-two")],
                    "前一组",
                )
                .await;
                remember_private_images(user_id, 42, &[image("latest")], "最新一张").await;
                let recent = recent_private_images(user_id, &[]).await;

                let generic = select_recent_images(&recent, ImageReferenceIntent::Recent);
                assert_eq!(generic.len(), 1);
                assert_eq!(generic[0].message_id, 42);

                let described = select_recent_images(&recent, ImageReferenceIntent::Described);
                assert_eq!(described.len(), 3);
                assert_eq!(described[0].attachment.key, "latest");
                assert_eq!(described[1].attachment.key, "older-one");
                assert_eq!(described[2].attachment.key, "older-two");
            });
    }

    #[test]
    fn recent_image_context_keeps_candidate_order_and_asks_not_to_guess() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let user_id = 8_700_002;
                remember_private_images(user_id, 51, &[image("cat")], "猫在窗边").await;
                let recent = recent_private_images(user_id, &[]).await;
                let context = with_recent_image_context(
                    "我说的是有猫的那张",
                    &recent,
                    ImageReferenceIntent::Described,
                );
                assert!(context.contains("候选图片1"));
                assert!(context.contains("猫在窗边"));
                assert!(context.contains("不要猜"));
            });
    }
}
