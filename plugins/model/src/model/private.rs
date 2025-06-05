use chrono::Local;
use kovi::{MsgEvent, RuntimeBot};
use std::sync::Arc;
use crate::model::utils::private_chat;

pub async fn private_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let nick_name = event.get_sender_nickname();
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let format_nickname = format!("[{}] {}", time, nick_name);
    if let Some(message) = event.borrow_text() {
        private_chat(user_id, message, format_nickname, bot).await;
    };
}
