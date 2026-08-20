use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::interrupt::{ReplyScope, interrupt, is_explicit_stop_message};
use crate::model::utils::{private_chat, requests_no_reply};
use crate::sticker_memory::{
    StickerScope, extract_stickers, has_reply, known_labels, quoted_message_context,
    stickers_for_teaching, teach, teaching_label, with_quoted_context, with_sticker_context,
};
use crate::vision::{
    default_vision_prompt, extract_image_attachments, is_vision_command, merge_image_attachments,
    resolve_image_urls, strip_vision_command,
};
use kovi::RuntimeBot;
use kovi::event::PrivateMsgEvent;
use std::sync::{Arc, LazyLock};

static PRIVATE_MESSAGE_BATCHES: LazyLock<MessageCoalescer<i64>> = LazyLock::new(Default::default);

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let nick_name = event.get_sender_nickname();
    let message = event.borrow_text().unwrap_or_default();
    let stickers = extract_stickers(&event.message);
    let current_images = extract_image_attachments(&event.message);
    let vision_command = is_vision_command(message);
    // 私聊中的任何新消息都应立即使旧模型结果和未发送气泡失效。
    let reply_ticket = interrupt(ReplyScope::Private(user_id)).await;

    if is_explicit_stop_message(message) {
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
                    Ok(count) => bot.send_private_msg(
                        user_id,
                        format!("记住啦，这 {count} 个表情以后表示“{label}”。"),
                    ),
                    Err(error) => {
                        eprintln!("[ERROR] 私聊保存表情包记忆失败: {}", error);
                        bot.send_private_msg(user_id, "这次没能记住，稍后再教我一次吧。");
                    }
                }
            }
            Ok(_) => bot.send_private_msg(
                user_id,
                "请回复（引用）那张表情包，再发送 #教芸汐 这个表情是……哦。",
            ),
            Err(error) => {
                eprintln!("[ERROR] 私聊读取被引用表情失败: {}", error);
                bot.send_private_msg(user_id, "我没能读到被引用的表情，请重新引用后再试一次哦。");
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
    let quoted_images = quoted
        .as_ref()
        .map(|quoted| quoted.images.as_slice())
        .unwrap_or_default();
    let images = merge_image_attachments(&current_images, quoted_images);
    let vision_requested = !images.is_empty();
    if vision_command && images.is_empty() {
        bot.send_private_msg(
            user_id,
            "请把截图和 #看截图 放在一起，或回复那张截图再发送命令哦。",
        );
        return;
    }
    // 陌生且没有文字的表情默认保持静默，不请求模型。
    if message.trim().is_empty()
        && !stickers.is_empty()
        && labels.is_empty()
        && quoted.is_none()
        && !vision_requested
    {
        println!("[INFO] 收到未学习私聊表情，保持静默 (用户: {})", user_id);
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
    let (model_message, plain_text, vision_requested, images) =
        if !message.trim_start().starts_with('#') {
            let Some(combined) = PRIVATE_MESSAGE_BATCHES
                .push(
                    user_id,
                    MessagePart {
                        text: model_message,
                        addressed: false,
                        plain_text: stickers.is_empty() && quoted.is_none(),
                        vision_requested,
                        images,
                    },
                )
                .await
            else {
                return;
            };
            (
                combined.text,
                combined.plain_text,
                combined.vision_requested,
                combined.images,
            )
        } else {
            (model_message, false, vision_requested, images)
        };
    let vision_images = if vision_requested {
        match resolve_image_urls(&images, &bot).await {
            Ok(images) if !images.is_empty() => images,
            Ok(_) => {
                bot.send_private_msg(
                    user_id,
                    "我暂时拿不到这张截图的内容，再发一次或换张图试试吧。",
                );
                return;
            }
            Err(error) => {
                eprintln!("[ERROR] 私聊读取截图失败 (用户: {}): {}", user_id, error);
                bot.send_private_msg(user_id, "我暂时读不到这张截图，再发一次或换张图试试吧。");
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
    private_chat(
        user_id,
        &model_message,
        nick_name,
        bot,
        reply_ticket,
        vision_images,
    )
    .await;
}
