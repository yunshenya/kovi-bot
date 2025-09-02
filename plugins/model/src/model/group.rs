use crate::model::utils::{send_sys_info, silence};
use crate::config;
use chrono::Local;
use kovi::RuntimeBot;
use kovi::event::GroupMsgEvent;
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
                send_sys_info(Arc::clone(&bot), group_id).await;
            },
            
            "#重载配置文件" => {
                match config::reload_config_from_file() {
                    Ok(_) => bot.send_group_msg(group_id, "配置重载成功"),
                    Err(e) => bot.send_group_msg(group_id, format!("配置重载失败: {}", e)),
                }
            },
            
            "#重载全部配置" => {
                match config::reload_config() {
                    Ok(_) => bot.send_group_msg(group_id, "全部配置文件重载成功"),
                    Err(e) => bot.send_group_msg(group_id, format!("重载失败： {}", e))
                }
            },
            
            _ => {
                silence(group_id, message, bot, sender).await;
            }
        }
    }
}
