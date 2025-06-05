mod config;
mod utils;

pub(crate) use crate::utils::system_info;
use chrono::Local;
use kovi::serde_json::Value;
use kovi::tokio::sync::{Mutex, MutexGuard};
use kovi::{MsgEvent, PluginBuilder, RuntimeBot};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

static MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static IS_BANNED: LazyLock<Mutex<HashMap<i64, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static PRIVATE_MESSAGE_MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum Roles {
    System,
    User,
    Assistant,
}

#[derive(Debug, Serialize)]
struct BotMemory {
    role: Roles,
    content: String,
}

#[derive(Debug, Serialize)]
struct ModelConf<'a> {
    model: &'a str,
    messages: &'a Vec<BotMemory>,
    stream: bool,
    temperature: f32,
}

#[kovi::plugin]
async fn main() {
    let bot_shore = PluginBuilder::get_runtime_bot();
    let group_message = {
        let bot = bot_shore.clone();
        move |event| {
            let bot = bot.clone();
            async move {
                group_message_event(event, bot).await;
            }
        }
    };

    let private_message = {
        let bot = bot_shore.clone();
        move |event| {
            let bot = bot.clone();
            async move {
                private_message_event(event, bot).await;
            }
        }
    };
    PluginBuilder::on_group_msg(group_message);
    PluginBuilder::on_private_msg(private_message);
}

async fn private_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>) {
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
                content: config::get().prompt().private_prompt().to_string(),
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

async fn group_message_event(event: Arc<MsgEvent>, bot: Arc<RuntimeBot>) {
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

async fn control_model(
    guard: &mut MutexGuard<'_, HashMap<i64, Vec<BotMemory>>>,
    group_id: i64,
    bot: Arc<RuntimeBot>,
    nickname: String,
    message: &str,
) {
    match guard.get_mut(&group_id) {
        None => {
            guard.insert(
                group_id,
                vec![
                    BotMemory {
                        role: Roles::System,
                        content: config::get().prompt().system_prompt().to_string(),
                    },
                    BotMemory {
                        role: Roles::User,
                        content: format!("{}:{}", nickname, message),
                    },
                ],
            );
            if let Some(vec) = guard.get_mut(&group_id) {
                let model = params_model(vec).await;
                if !model.content.contains("[sp]") {
                    bot.send_group_msg(group_id, &model.content);
                };
                vec.push(BotMemory {
                    role: Roles::Assistant,
                    content: model.content,
                })
            };
        }
        Some(vec) => {
            vec.push(BotMemory {
                role: Roles::User,
                content: format!("{}:{}", nickname, message),
            });
            let resp = params_model(vec).await;
            if !resp.content.contains("[sp]") {
                bot.send_group_msg(group_id, &resp.content);
            };
            vec.push(resp);
        }
    }
}
async fn params_model(messages: &mut Vec<BotMemory>) -> BotMemory {
    let server_config = config::get().server_config();
    if messages.len() > 9 {
        messages.drain(1..3);
    };
    let bot_conf = ModelConf {
        model: server_config.model_name(),
        messages,
        stream: false,
        temperature: 0.7,
    };
    let mut header = HeaderMap::new();
    let token = std::env::var("BOT_API_TOKEN").expect("BOT_API_TOKEN must be set");
    header.insert(AUTHORIZATION, format!("Bearer {}", token).parse().unwrap());
    header.insert(CONTENT_TYPE, "application/json".parse().unwrap());
    let client = Client::new();
    let resp = client
        .post(server_config.url())
        .headers(header)
        .json(&bot_conf)
        .send()
        .await
        .unwrap();
    let text = resp.json::<Value>().await.unwrap();
    let bot_content = text
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("余额不足或者文档有更改")
        .trim()
        .replace("芸汐：", "")
        .to_string();
    BotMemory {
        role: Roles::Assistant,
        content: bot_content,
    }
}

fn instance_is_ban() -> &'static Mutex<HashMap<i64, bool>> {
    &IS_BANNED
}

fn get_memory() -> &'static Mutex<HashMap<i64, Vec<BotMemory>>> {
    &MEMORY
}

fn get_private_message_memory() -> &'static Mutex<HashMap<i64, Vec<BotMemory>>> {
    &PRIVATE_MESSAGE_MEMORY
}

#[cfg(test)]
mod tests {
    use crate::config;
    use kovi::tokio;

    #[tokio::test]
    async fn test() {
        let server_config = config::get().server_config();
        println!("{}", server_config.model_name());
        let model = config::get().prompt();
        println!("{}", model.system_prompt());
    }
}
