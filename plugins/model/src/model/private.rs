use crate::model::coalesce::MessageCoalescer;
use crate::model::utils::{private_chat, requests_no_reply};
use crate::sticker_memory::{
    StickerScope, extract_stickers, has_reply, known_labels, quoted_message_context,
    stickers_for_teaching, teach, teaching_label, with_quoted_context, with_sticker_context,
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
    // 陌生且没有文字的表情默认保持静默，不请求模型。
    if message.trim().is_empty() && !stickers.is_empty() && labels.is_empty() && quoted.is_none() {
        println!("[INFO] 收到未学习私聊表情，保持静默 (用户: {})", user_id);
        return;
    }
    if message.trim().is_empty() && stickers.is_empty() && !has_reply(&event.message) {
        return;
    }

    let current_message = with_sticker_context(message, &labels);
    let model_message = quoted.as_ref().map_or(current_message.clone(), |quoted| {
        with_quoted_context(&current_message, quoted)
    });
    let model_message = if !message.trim().is_empty()
        && stickers.is_empty()
        && quoted.is_none()
        && !message.trim_start().starts_with('#')
    {
        let Some(combined) = PRIVATE_MESSAGE_BATCHES
            .push(user_id, model_message, false)
            .await
        else {
            return;
        };
        combined.text
    } else {
        model_message
    };
    private_chat(user_id, &model_message, nick_name, bot).await;
}
