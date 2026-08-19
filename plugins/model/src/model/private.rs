use crate::model::utils::private_chat;
use kovi::RuntimeBot;
use kovi::event::PrivateMsgEvent;
use std::sync::Arc;

pub async fn private_message_event(event: Arc<PrivateMsgEvent>, bot: Arc<RuntimeBot>) {
    let user_id = event.user_id;
    let nick_name = event.get_sender_nickname();
    if let Some(message) = event.borrow_text() {
        private_chat(user_id, message, nick_name, bot).await;
    };
}
