use crate::model::utils::{send_sys_info, silence};
use crate::config;
use chrono::Local;
use kovi::RuntimeBot;
use kovi::event::GroupMsgEvent;
use std::sync::Arc;
use std::time::Duration;

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

            "#启用自动重载" => {
                if config::is_auto_reload_enabled() {
                    bot.send_group_msg(group_id, "自动重载已经启用");
                } else {
                    config::enable_auto_reload(Duration::from_secs(5));
                    bot.send_group_msg(group_id, "自动重载已启用，每5秒检查一次");
                }
            },

            "#禁用自动重载" => {
                if config::is_auto_reload_enabled() {
                    config::disable_auto_reload();
                    bot.send_group_msg(group_id, "自动重载已禁用");
                } else {
                    bot.send_group_msg(group_id, "自动重载未启用");
                }
            },

            "#检查配置变化" => {
                match config::check_and_reload() {
                    Ok(true) => bot.send_group_msg(group_id, "检测到配置变化，已自动重载"),
                    Ok(false) => bot.send_group_msg(group_id, "配置文件无变化"),
                    Err(e) => bot.send_group_msg(group_id, format!("检查配置失败: {}", e)),
                }
            },

            "#自动重载状态" => {
                let status = if config::is_auto_reload_enabled() {
                    "已启用"
                } else {
                    "已禁用"
                };
                bot.send_group_msg(group_id, format!("配置自动重载状态: {}", status));
            },
            _ => {
                silence(group_id, message, bot, sender).await;
            }
        }
    }
}
