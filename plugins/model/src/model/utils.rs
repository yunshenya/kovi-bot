use crate::config;
use crate::utils;
use crate::memory::{MemoryManager, UserProfile};
use crate::mood_system::MoodSystem;
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

// 全局记忆管理器
static MEMORY_MANAGER: LazyLock<Arc<MemoryManager>> = 
    LazyLock::new(|| Arc::new(MemoryManager::new("bot_memory.json")));

// 全局情绪系统
static MOOD_SYSTEM: LazyLock<MoodSystem> = 
    LazyLock::new(|| MoodSystem::new(Arc::clone(&MEMORY_MANAGER)));

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
    // 分析情绪并更新
    if let Err(e) = MOOD_SYSTEM.analyze_and_update_mood(message, "group_chat").await {
        eprintln!("Failed to analyze mood: {}", e);
    }

    // 记录对话记忆
    if let Err(e) = MEMORY_MANAGER.add_conversation_memory(
        group_id,
        &format!("{}: {}", nickname, message),
        "group_chat"
    ).await {
        eprintln!("Failed to add conversation memory: {}", e);
    }

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
    // 分析情绪并更新
    if let Err(e) = MOOD_SYSTEM.analyze_and_update_mood(message, "private_chat").await {
        eprintln!("Failed to analyze mood: {}", e);
    }

    // 记录对话记忆
    if let Err(e) = MEMORY_MANAGER.add_conversation_memory(
        user_id,
        &format!("{}: {}", format_nickname, message),
        "private_chat"
    ).await {
        eprintln!("Failed to add conversation memory: {}", e);
    }

    // 更新用户档案
    update_user_profile_from_message(user_id, message, &format_nickname).await;

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

async fn update_user_profile_from_message(user_id: i64, message: &str, nickname: &str) {
    let mut profile = MEMORY_MANAGER.get_user_profile(user_id).await
        .unwrap_or_else(|| UserProfile {
            user_id,
            nickname: nickname.to_string(),
            personality_traits: Vec::new(),
            interests: Vec::new(),
            relationship_level: 1,
            last_interaction: Local::now(),
            interaction_count: 0,
            mood_history: Vec::new(),
        });

    // 更新互动信息
    profile.last_interaction = Local::now();
    profile.interaction_count += 1;
    
    // 根据对话内容更新关系等级
    if message.contains("谢谢") || message.contains("感谢") {
        profile.relationship_level = (profile.relationship_level + 1).min(10);
    }

    // 提取兴趣关键词
    let interests = extract_interests_from_message(message);
    for interest in interests {
        if !profile.interests.contains(&interest) {
            profile.interests.push(interest);
        }
    }

    // 更新用户档案
    if let Err(e) = MEMORY_MANAGER.update_user_profile(user_id, profile).await {
        eprintln!("Failed to update user profile: {}", e);
    }
}

fn extract_interests_from_message(message: &str) -> Vec<String> {
    let mut interests = Vec::new();
    let message_lower = message.to_lowercase();
    
    let interest_keywords = [
        ("游戏", vec!["游戏", "打游戏", "玩", "lol", "王者", "吃鸡"]),
        ("音乐", vec!["音乐", "歌", "听歌", "唱歌", "演唱会"]),
        ("电影", vec!["电影", "看片", "影院", "大片"]),
        ("读书", vec!["书", "读书", "小说", "文学"]),
        ("运动", vec!["运动", "跑步", "健身", "锻炼"]),
        ("美食", vec!["吃", "美食", "餐厅", "料理", "做饭"]),
        ("旅行", vec!["旅行", "旅游", "出去玩", "度假"]),
        ("学习", vec!["学习", "考试", "课程", "知识"]),
    ];

    for (category, keywords) in &interest_keywords {
        for keyword in keywords {
            if message_lower.contains(keyword) {
                interests.push(category.to_string());
                break;
            }
        }
    }

    interests
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