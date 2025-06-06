use crate::model::utils::{send_sys_info, silence};
use chrono::Local;
use kovi::{MsgEvent, RuntimeBot};
use std::sync::Arc;

pub async fn group_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id.unwrap();
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let nickname = event.get_sender_nickname();
    let sender = format!("[{}] {}", time, nickname);
    if let Some(message) = event.borrow_text() {
        if message.eq("#系统信息") {
            send_sys_info(Arc::clone(&bot), group_id).await;
            let imgs = event.message.get("image");
            if imgs.is_empty() {
                return;
            }
            let urls: Vec<_> = imgs
                .iter()
                .map(|x| x.data.get("url").unwrap().as_str().unwrap())
                .collect();
            
            bot.send_group_msg(group_id, urls.join("\n"));
        } else {
            silence(group_id, message, bot, sender).await;
        };
    }
}
