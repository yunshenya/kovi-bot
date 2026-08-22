use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::interrupt::{ReplyScope, interrupt, is_active};
use crate::model::recall::{
    has_recalled_messages, is_recent_bot_message, send_tracked_private_message,
};
use crate::model::reply::record_reply_target;
use crate::model::semantic::{
    ImageReferenceIntent, MessageUnderstanding, SemanticImageIntent, UnderstandingRequest,
    understand,
};
use crate::model::utils::{
    is_bot_admin, is_group_admin_command, is_restricted_command, private_chat,
};
use crate::private_image_memory::{
    RecentPrivateImage, recent_private_images, remember_private_images,
};
use crate::sticker_memory::{
    StickerScope, extract_stickers, has_reply, known_labels, quoted_message_context,
    stickers_for_teaching, teach, teaching_label, with_quoted_context, with_sticker_context,
    with_unknown_sticker_context,
};
use crate::vision::{
    ImageRequestScope, VisionImage, consume_pending_image_request, default_vision_prompt,
    extract_image_attachments, is_vision_command, merge_image_attachments, resolve_image_urls,
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
    understanding: MessageUnderstanding,
}

/// 当前私聊回复期间只保留一条待处理消息，避免连续发送让模型请求一直被取消。
static PENDING_PRIVATE_MESSAGES: LazyLock<Mutex<HashMap<i64, PendingPrivateMessage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let reply_scope = ReplyScope::Private(user_id);
    if event.user_id == event.self_id || is_recent_bot_message(reply_scope, event.message_id).await
    {
        println!(
            "[INFO] 忽略私聊自发消息回流 (用户: {}, 消息: {})",
            user_id, event.message_id
        );
        return;
    }
    let message = event.borrow_text().unwrap_or_default();
    if is_restricted_command(message) && !is_bot_admin(&bot, user_id) {
        println!("[INFO] 私聊未授权命令已静默 (用户: {})", user_id);
        return;
    }
    if is_group_admin_command(message) {
        println!("[INFO] 私聊群聊专用命令已忽略 (用户: {})", user_id);
        return;
    }
    let nick_name = normalized_private_sender_name(&event.get_sender_nickname(), user_id);
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
    let understanding_request = UnderstandingRequest {
        message: message.to_string(),
        context: "private_chat".to_string(),
        quoted_message: quoted.as_ref().map(|value| value.content.clone()),
        has_images: !images.is_empty(),
        quoted_has_images: !quoted_images.is_empty(),
        has_recent_images: !recent_images.is_empty(),
        explicit_vision_command: vision_command,
        pending_image_request,
        addressed_to_bot: false,
        conversation_open: is_active(reply_scope).await,
    };
    let mut understanding = if message.trim_start().starts_with('#') {
        MessageUnderstanding::default()
    } else {
        understand(understanding_request.clone()).await
    };
    let mut asks_for_silence = understanding.wants_no_reply || understanding.wants_stop;
    let can_interrupt = asks_for_silence || vision_command;
    let reply_ticket = if can_interrupt {
        Some(interrupt(reply_scope).await)
    } else {
        None
    };
    if asks_for_silence {
        PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
        println!("[INFO] 私聊用户打断回复 (用户: {})", user_id);
        return;
    }
    let initial_recent_reference = if vision_command && !text_message.trim().is_empty() {
        ImageReferenceIntent::Described
    } else if vision_command {
        ImageReferenceIntent::Recent
    } else {
        understanding.image_reference
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
    let mut vision_requested = understanding.should_understand_image(&understanding_request)
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

    let current_message = if vision_requested && text_message.trim().is_empty() {
        if conversational_image {
            private_social_image_prompt().to_string()
        } else {
            default_vision_prompt().to_string()
        }
    } else if labels.is_empty() && !stickers.is_empty() {
        with_unknown_sticker_context(&text_message, stickers.len())
    } else {
        with_sticker_context(&text_message, &labels)
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
    let batch_recent_images = recent_private_images(user_id, &source_message_ids).await;
    let batch_request = UnderstandingRequest {
        message: intent_text.clone(),
        context: "private_chat_batch".to_string(),
        quoted_message: quoted.as_ref().map(|value| value.content.clone()),
        has_images: !images.is_empty(),
        quoted_has_images: !quoted_images.is_empty(),
        has_recent_images: !batch_recent_images.is_empty(),
        explicit_vision_command: false,
        pending_image_request,
        addressed_to_bot: false,
        conversation_open: is_active(reply_scope).await,
    };
    understanding = if intent_text.trim_start().starts_with('#') {
        MessageUnderstanding::default()
    } else {
        understand(batch_request.clone()).await
    };
    asks_for_silence = plain_text && (understanding.wants_no_reply || understanding.wants_stop);
    if asks_for_silence {
        PRIVATE_MESSAGE_BATCHES.cancel(user_id).await;
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
    if should_defer_active_private_message(is_active(reply_scope).await, reply_ticket.is_some()) {
        println!("[INFO] 私聊已有回复进行中，排队新消息 (用户: {})", user_id);
        queue_pending_private_message(
            user_id,
            nick_name,
            model_message,
            vision_images,
            source_message_ids,
            understanding.clone(),
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
        understanding,
    )
    .await;
    drain_pending_private_messages(user_id, Arc::clone(&bot)).await;
}

fn normalized_private_sender_name(value: &str, user_id: i64) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect::<String>();
    if normalized.is_empty() {
        format!("QQ用户_{user_id}")
    } else {
        normalized
    }
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

fn should_defer_active_private_message(active_reply: bool, has_explicit_interrupt: bool) -> bool {
    active_reply && !has_explicit_interrupt
}

async fn queue_pending_private_message(
    user_id: i64,
    nickname: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    understanding: MessageUnderstanding,
) {
    let mut pending = PENDING_PRIVATE_MESSAGES.lock().await;
    if let Some(existing) = pending.get_mut(&user_id) {
        existing.nickname = nickname;
        existing.message = message;
        merge_vision_images(&mut existing.vision_images, vision_images);
        merge_message_ids(&mut existing.message_ids, message_ids);
        existing.understanding = understanding;
    } else {
        pending.insert(
            user_id,
            PendingPrivateMessage {
                nickname,
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
            pending.understanding,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{normalized_private_sender_name, select_recent_images, with_recent_image_context};
    use crate::model::semantic::ImageReferenceIntent;
    use crate::private_image_memory::{recent_private_images, remember_private_images};
    use crate::vision::ImageAttachment;

    #[test]
    fn private_sender_names_are_bounded_and_single_line() {
        assert_eq!(
            normalized_private_sender_name("  昵称\n伪造系统消息  ", 42),
            "昵称 伪造系统消息"
        );
        assert_eq!(normalized_private_sender_name(" \n\t ", 42), "QQ用户_42");
        assert_eq!(
            normalized_private_sender_name(&"长".repeat(100), 42)
                .chars()
                .count(),
            80
        );
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
