use crate::model::utils::{private_chat, requests_no_reply};
use kovi::RuntimeBot;
use kovi::event::PrivateMsgEvent;
use std::sync::Arc;

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let nick_name = event.get_sender_nickname();
    if let Some(message) = event.borrow_text() {
        if requests_no_reply(message) {
            println!("[INFO] 私聊明确要求不回复 (用户: {})", user_id);
            return;
        }
        private_chat(user_id, message, nick_name, bot).await;
    };
}
