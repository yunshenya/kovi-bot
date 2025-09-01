use crate::model::utils::{send_sys_info, silence};
use chrono::Local;
use kovi::event::GroupMsgEvent;
use kovi::RuntimeBot;
use std::sync::Arc;

pub async fn group_message_event(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id;
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let nickname = event.get_sender_nickname();
    let sender = format!("[{}] {}", time, nickname);
    if let Some(message) = event.borrow_text() {
        match message {
            "#系统信息" => {
                send_sys_info(bot.clone(), group_id).await;
                let get = crate::config::get().server_config();
                bot.send_group_msg(group_id, get.model_name());
            }
            _ => {
                silence(group_id, message, bot, sender).await;
            }
        }
    }
}
