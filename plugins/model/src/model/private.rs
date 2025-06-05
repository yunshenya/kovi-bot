use std::sync::Arc;
use chrono::Local;
use kovi::{MsgEvent, RuntimeBot};
use crate::model::utils::{get_private_message_memory, params_model, BotMemory, Roles};

pub async fn private_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let nick_name = event.get_sender_nickname();
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let format_nickname = format!("[{}] {}", time, nick_name);
    if let Some(message) = event.borrow_text() {
        let mut private = get_private_message_memory().lock().await;
        let history = private.entry(user_id).or_insert(vec![
            BotMemory {
                role: Roles::System,
                content: crate::config::get().prompt().private_prompt().to_string(),
            },
            BotMemory {
                role: Roles::User,
                content: message.to_string(),
            },
        ]);
        history.push(BotMemory {
            role: Roles::User,
            content: format!("{}:{}", format_nickname, message),
        });
        let bot_content = params_model(history).await;
        bot.send_private_msg(user_id, bot_content.content);
    };
}
