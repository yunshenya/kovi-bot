use crate::config;
use crate::utils;
use kovi::RuntimeBot;
use kovi::serde_json::Value;
use kovi::tokio::sync::{Mutex, MutexGuard};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;
use anyhow::Context;
use chrono::{Local, TimeZone};

static MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static IS_BANNED: LazyLock<Mutex<HashMap<i64, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static PRIVATE_MESSAGE_MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Roles {
    System,
    User,
    Assistant,
}

#[derive(Debug, Serialize)]
pub struct BotMemory {
    pub(crate) role: Roles,
    pub(crate) content: String,
}

#[derive(Debug, Serialize)]
struct ModelConf<'a> {
    model: &'a str,
    messages: &'a Vec<BotMemory>,
    stream: bool,
    temperature: f32,
}

pub async fn control_model(
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
pub async fn params_model(messages: &mut Vec<BotMemory>) -> BotMemory {
    let config = config::get();
    let server_config = config.server_config();
    if messages.len() > 9 {
        messages.drain(1..7);
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

pub async fn silence(group_id: i64, message: &str, bot: Arc<RuntimeBot>, sender: String) {
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
}

pub async fn send_sys_info(bot: Arc<RuntimeBot>, group_id: i64) {
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
                        "{} \n系统运行时间：{} \n{} \nLagrange占用: {}MB,\n当前使用的模型为:{}\n配置文件最后修改时间为:{}",
                        "对话功能是正常的哦",
                        system_info.0,
                        system_info.1,
                        (now_status / 1024) / 1024,
                        config::get().server_config().model_name(),
                        get_file_modified_time_formatted().unwrap_or(String::from("获取失败")),
                    ),
                );
            }
        }
        Err(_) => bot.send_group_msg(group_id, "未设置token"),
    }
}

pub async fn private_chat(
    user_id: i64,
    message: &str,
    format_nickname: String,
    bot: Arc<RuntimeBot>,
) {
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
}


pub fn get_file_modified_time_formatted() -> anyhow::Result<String> {
    let config_path = "bot.conf.toml";
    if !Path::new(config_path).exists() {
        return Ok("文件不存在".to_string());
    }

    let metadata = fs::metadata(config_path)
        .with_context(|| anyhow::anyhow!("Failed to get file metadata"))?;

    let modified = metadata.modified()
        .with_context(|| anyhow::anyhow!("Failed to get modification time"))?;

    let since_epoch = modified.duration_since(UNIX_EPOCH)
        .with_context(|| anyhow::anyhow!("Failed to calculate time since epoch"))?;

    // 转换为本地时间并格式化
    let datetime = Local.timestamp_opt(since_epoch.as_secs() as i64, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?;

    Ok(datetime.format("%Y-%m-%d %H:%M:%S").to_string())
}