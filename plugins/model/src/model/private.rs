use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::interrupt::{ReplyScope, interrupt, is_active, is_explicit_stop_message};
use crate::model::recall::{has_recalled_messages, send_tracked_private_message};
use crate::model::reply::record_reply_target;
use crate::model::utils::{
    is_bot_admin, is_group_admin_command, is_restricted_command, private_chat, requests_no_reply,
};
use crate::sticker_memory::{
    StickerScope, extract_stickers, has_reply, known_labels, quoted_message_context,
    stickers_for_teaching, teach, teaching_label, with_quoted_context, with_sticker_context,
};
use crate::vision::{
    ImageIntent, ImageRequestScope, VisionImage, classify_image_intent,
    consume_pending_image_request, default_vision_prompt, extract_image_attachments,
    is_vision_command, merge_image_attachments, message_requests_image, resolve_image_urls,
    strip_vision_command, with_social_image_context,
};
use kovi::RuntimeBot;
use kovi::event::PrivateMsgEvent;
use kovi::tokio::sync::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

static PRIVATE_MESSAGE_BATCHES: LazyLock<MessageCoalescer<i64>> = LazyLock::new(Default::default);

struct PendingPrivateMessage {
    nickname: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
}

/// 当前私聊回复期间只保留一条待处理消息，避免连续发送让模型请求一直被取消。
static PENDING_PRIVATE_MESSAGES: LazyLock<Mutex<HashMap<i64, PendingPrivateMessage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let message = event.borrow_text().unwrap_or_default();
    if is_restricted_command(message) && !is_bot_admin(&bot, user_id) {
        println!("[INFO] 私聊未授权命令已静默 (用户: {})", user_id);
        return;
    }
    if is_group_admin_command(message) {
        println!("[INFO] 私聊群聊专用命令已忽略 (用户: {})", user_id);
        return;
    }
    let nick_name = event.get_sender_nickname();
    let stickers = extract_stickers(&event.message);
    let current_images = extract_image_attachments(&event.message);
    let vision_command = is_vision_command(message);
    let reply_scope = ReplyScope::Private(user_id);
    record_reply_target(
        reply_scope,
        event.message_id,
        Some(user_id),
        nick_name.clone(),
        &event.human_text,
    )
    .await;
    let explicit_stop = is_explicit_stop_message(message);
    let asks_for_silence = explicit_stop || requests_no_reply(message);
    let can_interrupt = asks_for_silence || vision_command;
    let reply_ticket = if can_interrupt {
        Some(interrupt(ReplyScope::Private(user_id)).await)
    } else {
        None
    };

    if asks_for_silence {
        PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
        println!("[INFO] 私聊用户打断回复 (用户: {})", user_id);
        return;
    }

    if let Some(label) = teaching_label(message) {
        match stickers_for_teaching(&event.message, &bot).await {
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

    if requests_no_reply(message) {
        PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
        println!("[INFO] 私聊明确要求不回复 (用户: {})", user_id);
        return;
    }

    let sticker_scope = StickerScope::Private(user_id);
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
    let images = merge_image_attachments(&current_images, quoted_images);
    let quoted_message_requests_image = quoted
        .as_ref()
        .is_some_and(|quoted| message_requests_image(&quoted.content));
    let pending_image_request =
        consume_pending_image_request(ImageRequestScope::Private(user_id), !images.is_empty())
            .await;
    let image_intent = classify_image_intent(
        message,
        !images.is_empty(),
        vision_command,
        !quoted_images.is_empty() && !message.trim().is_empty(),
        quoted_message_requests_image,
        pending_image_request,
        true,
    );
    let vision_requested = image_intent == ImageIntent::VisualUnderstand;
    if vision_command && images.is_empty() {
        send_tracked_private_message(
            &bot,
            user_id,
            "请把截图和 #看截图 放在一起，或回复那张截图再发送命令哦。",
        )
        .await;
        return;
    }
    if message.trim().is_empty() && stickers.is_empty() && !has_reply(&event.message) {
        return;
    }

    let text_message = if vision_command {
        strip_vision_command(message)
    } else {
        message.to_string()
    };
    let current_message = if vision_requested && text_message.trim().is_empty() {
        default_vision_prompt().to_string()
    } else {
        with_sticker_context(&text_message, &labels)
    };
    let model_message = quoted.as_ref().map_or(current_message.clone(), |quoted| {
        with_quoted_context(&current_message, quoted)
    });
    let (
        model_message,
        plain_text,
        intent_text,
        batch_vision_requested,
        images,
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
            combined.images,
            combined.message_ids,
        )
    } else {
        (
            model_message,
            false,
            text_message.clone(),
            vision_requested,
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
    let batch_image_intent = classify_image_intent(
        &intent_text,
        !images.is_empty(),
        false,
        !quoted_images.is_empty() && !intent_text.trim().is_empty(),
        false,
        batch_vision_requested,
        true,
    );
    let vision_requested =
        batch_vision_requested || batch_image_intent == ImageIntent::VisualUnderstand;
    if intent_text.trim().is_empty()
        && !vision_requested
        && (!images.is_empty() || !stickers.is_empty())
    {
        println!("[INFO] 收到私聊纯图片状态，保持静默 (用户: {})", user_id);
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
    if plain_text && requests_no_reply(&model_message) {
        println!("[INFO] 合并后的私聊消息明确要求不回复 (用户: {})", user_id);
        return;
    }
    if should_defer_active_private_message(is_active(reply_scope).await, reply_ticket.is_some()) {
        println!("[INFO] 私聊已有回复进行中，排队新消息 (用户: {})", user_id);
        queue_pending_private_message(
            user_id,
            nick_name,
            model_message,
            vision_images,
            source_message_ids,
        )
        .await;
        return;
    }
    let reply_ticket = match reply_ticket {
        Some(ticket) => ticket,
        None => interrupt(reply_scope).await,
    };
    private_chat(
        user_id,
        &model_message,
        nick_name,
        Arc::clone(&bot),
        reply_ticket,
        vision_images,
        source_message_ids,
    )
    .await;
    drain_pending_private_messages(user_id, Arc::clone(&bot)).await;
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
) {
    let mut pending = PENDING_PRIVATE_MESSAGES.lock().await;
    if let Some(existing) = pending.get_mut(&user_id) {
        existing.nickname = nickname;
        existing.message = message;
        merge_vision_images(&mut existing.vision_images, vision_images);
        merge_message_ids(&mut existing.message_ids, message_ids);
    } else {
        pending.insert(
            user_id,
            PendingPrivateMessage {
                nickname,
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

async fn drain_pending_private_messages(user_id: i64, bot: Arc<RuntimeBot>) {
    for _ in 0..3 {
        let Some(pending) = PENDING_PRIVATE_MESSAGES.lock().await.remove(&user_id) else {
            return;
        };
        if is_active(ReplyScope::Private(user_id)).await {
            PENDING_PRIVATE_MESSAGES
                .lock()
                .await
                .insert(user_id, pending);
            return;
        }

        println!("[INFO] 私聊开始处理排队消息 (用户: {})", user_id);
        let ticket = interrupt(ReplyScope::Private(user_id)).await;
        private_chat(
            user_id,
            &pending.message,
            pending.nickname,
            bot.clone(),
            ticket,
            pending.vision_images,
            pending.message_ids,
        )
        .await;
    }
}
