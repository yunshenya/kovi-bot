use crate::model::utils::{private_chat, requests_no_reply};
use crate::sticker_memory::{
    extract_stickers, known_labels, stickers_for_teaching, teach, teaching_label,
    with_sticker_context,
};
use kovi::RuntimeBot;
use kovi::event::PrivateMsgEvent;
use std::sync::Arc;

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let nick_name = event.get_sender_nickname();
    let message = event.borrow_text().unwrap_or_default();
    let stickers = extract_stickers(&event.message);

    if let Some(label) = teaching_label(message) {
        match stickers_for_teaching(&event.message, &bot).await {
            Ok(teaching_stickers) if !teaching_stickers.is_empty() => {
                match teach(&teaching_stickers, &label, user_id, None).await {
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
            Ok(_) | Err(_) => bot.send_private_msg(
                user_id,
                "请回复（引用）那张表情包，再发送 #教芸汐 这个表情是……哦。",
            ),
        }
        return;
    }

    if requests_no_reply(message) {
        println!("[INFO] 私聊明确要求不回复 (用户: {})", user_id);
        return;
    }

    let labels = match known_labels(&stickers).await {
        Ok(labels) => labels,
        Err(error) => {
            eprintln!("[ERROR] 私聊读取表情包记忆失败: {}", error);
            Vec::new()
        }
    };
    // 陌生且没有文字的表情默认保持静默，不请求模型。
    if message.trim().is_empty() && !stickers.is_empty() && labels.is_empty() {
        println!("[INFO] 收到未学习私聊表情，保持静默 (用户: {})", user_id);
        return;
    }
    if message.trim().is_empty() && stickers.is_empty() {
        return;
    }

    let model_message = with_sticker_context(message, &labels);
    private_chat(user_id, &model_message, nick_name, bot).await;
}
