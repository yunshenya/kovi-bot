use std::sync::Arc;
use chrono::Local;
use kovi::{MsgEvent, RuntimeBot};
use crate::model::utils::{control_model, get_memory, instance_is_ban};
use crate::utils;

pub async fn group_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id.unwrap();
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let nickname = event.get_sender_nickname();
    let sender = format!("[{}] {}", time, nickname);
    if let Some(message) = event.borrow_text() {
        if message.eq("#系统信息") {
            match std::env::var("BOT_API_TOKEN") {
                Ok(_) => {
                    let system_info = utils::system_info_get();
                    let option_status = bot.get_status().await;
                    if let Ok(status) = option_status {
                        let now_status = status
                            .data
                            .get("memory")
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0);
                        bot.send_group_msg(
                            group_id,
                            format!(
                                "{} \n系统运行时间：{} \n{} \nLagrange占用: {}MB",
                                "对话功能是正常的哦",
                                system_info.0,
                                system_info.1,
                                (now_status / 1024) / 1024
                            ),
                        );
                    }
                }
                Err(_) => bot.send_group_msg(group_id, "未设置token"),
            }
        } else {
            let mut banned_list = instance_is_ban().lock().await;
            match banned_list.get_mut(&group_id) {
                None => {
                    if message.eq("#禁言") {
                        banned_list.insert(group_id, true);
                        bot.send_group_msg(group_id, "禁言成功");
                    } else {
                        banned_list.insert(group_id, false);
                    }
                }
                Some(is_ban) => {
                    if !*is_ban {
                        if message.eq("#禁言") {
                            *is_ban = true;
                            bot.send_group_msg(group_id, "禁言成功");
                        } else {
                            let mut guard = get_memory().lock().await;
                            control_model(&mut guard, group_id, bot, sender, message).await;
                        }
                    } else if message.eq("#结束禁言") {
                        *is_ban = false;
                        bot.send_group_msg(group_id, "结束成功");
                    }
                }
            }
        };
    }
}