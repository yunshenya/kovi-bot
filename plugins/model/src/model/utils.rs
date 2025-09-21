//! # 模型工具模块
//! 
//! 提供聊天机器人的核心功能，包括：
//! - 群聊和私聊消息处理
//! - 智能记忆管理和上下文注入
//! - 个性化回复生成
//! - 情绪分析和人格调整
//! - 用户档案管理
//! - 系统状态监控

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

/// 群聊对话记忆存储
/// 
/// 存储每个群组的对话历史，用于维护上下文连续性
/// Key: 群组ID, Value: 对话消息列表
static MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 群组禁言状态存储
/// 
/// 记录每个群组的禁言状态，用于控制机器人是否回复
/// Key: 群组ID, Value: 是否被禁言
static IS_BANNED: LazyLock<Mutex<HashMap<i64, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 私聊对话记忆存储
/// 
/// 存储每个用户的私聊历史，用于个性化交互
/// Key: 用户ID, Value: 对话消息列表
static PRIVATE_MESSAGE_MEMORY: LazyLock<Mutex<HashMap<i64, Vec<BotMemory>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 全局记忆管理器实例
/// 
/// 负责管理所有类型的记忆数据，包括对话记忆、用户档案、群组信息等
static MEMORY_MANAGER: LazyLock<Arc<MemoryManager>> =
    LazyLock::new(|| Arc::new(MemoryManager::new("bot_memory.json")));

/// 全局情绪系统实例
/// 
/// 负责分析用户消息的情绪并调整机器人的人格状态
static MOOD_SYSTEM: LazyLock<MoodSystem> =
    LazyLock::new(|| MoodSystem::new(Arc::clone(&MEMORY_MANAGER)));

/// 最大记忆条数限制
/// 
/// 限制单次对话中保留的最大消息数量，防止内存过度使用
const MAX_MEMORY_SIZE: usize = 25;

/// 消息角色枚举
/// 
/// 定义对话中不同参与者的角色类型
#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Roles {
    /// 系统消息：包含系统提示和指令
    System,
    /// 用户消息：来自用户的消息
    User,
    /// 助手消息：机器人的回复
    Assistant,
}

/// 机器人记忆结构体
/// 
/// 存储单条对话消息的完整信息
#[derive(Debug, Serialize, Clone)]
pub struct BotMemory {
    /// 消息角色
    pub(crate) role: Roles,
    /// 消息内容
    pub(crate) content: String,
}

/// 模型配置结构体
/// 
/// 用于向AI模型发送请求时的配置参数
#[derive(Debug, Serialize)]
struct ModelConf<'a> {
    /// 模型名称
    model: &'a str,
    /// 消息列表
    messages: &'a Vec<BotMemory>,
    /// 是否流式输出
    stream: bool,
    /// 温度参数，控制回复的随机性 (0.0-1.0)
    temperature: f32,
}

/// 群聊消息处理主函数
/// 
/// 处理群聊中的消息，包括以下功能：
/// - 情绪分析和人格调整
/// - 对话记忆记录和检索
/// - 相关记忆上下文注入
/// - 智能回复生成
/// - 记忆大小管理
/// 
/// # 参数
/// * `guard` - 群聊记忆的互斥锁守卫
/// * `group_id` - 群组ID
/// * `bot` - 机器人实例
/// * `nickname` - 发送者昵称
/// * `message` - 消息内容
pub async fn control_model(
    guard: &mut MutexGuard<'_, HashMap<i64, Vec<BotMemory>>>,
    group_id: i64,
    bot: Arc<RuntimeBot>,
    nickname: String,
    message: &str,
) {
    // 分析情绪并更新
    if let Err(e) = MOOD_SYSTEM.analyze_and_update_mood(message, "group_chat").await {
        eprintln!("[ERROR] 群聊情绪分析失败 (群组: {}): {}", group_id, e);
    }

    // 记录对话记忆
    if let Err(e) = MEMORY_MANAGER.add_conversation_memory(
        group_id,
        &format!("{}: {}", nickname, message),
        "group_chat"
    ).await {
        eprintln!("[ERROR] 群聊记忆记录失败 (群组: {}): {}", group_id, e);
    }

    // 获取相关记忆来增强上下文
    let contextual_memories = MEMORY_MANAGER.get_contextual_memories(group_id, "group_chat", 5).await;
    let recent_memories = MEMORY_MANAGER.get_recent_memories(10).await;

    match guard.get_mut(&group_id) {
        None => {
            // 创建新的对话记录，包含相关记忆
            let mut system_prompt = config::get().prompt().system_prompt().to_string();
            
            // 添加相关记忆到系统提示中
            if !contextual_memories.is_empty() {
                system_prompt.push_str("\n\n相关记忆：");
                for memory in contextual_memories.iter().take(3) {
                    system_prompt.push_str(&format!("\n- {}", memory.content));
                }
            }

            guard.insert(
                group_id,
                vec![
                    BotMemory {
                        role: Roles::System,
                        content: system_prompt,
                    },
                    BotMemory {
                        role: Roles::User,
                        content: format!("{}:{}", nickname, message),
                    },
                ],
            );
            if let Some(vec) = guard.get_mut(&group_id) {
                println!("[INFO] 群聊新对话开始 (群组: {}, 用户: {})", group_id, nickname);
                let model = params_model(vec).await;
                if !model.content.contains("[sp]") {
                    bot.send_group_msg(group_id, &model.content);
                    println!("[INFO] 群聊消息已发送 (群组: {}): {}", group_id, model.content);
                };
                vec.push(BotMemory {
                    role: Roles::Assistant,
                    content: model.content,
                });

                // 检查并限制记忆大小
                limit_memory_size(vec);
            };
        }
        Some(vec) => {
            // 添加新的用户消息
            vec.push(BotMemory {
                role: Roles::User,
                content: format!("{}:{}", nickname, message),
            });

            // 在生成回复前，检查是否需要添加相关记忆
            if should_add_memory_context(vec.len(), &recent_memories) {
                add_memory_context_to_messages(vec, &contextual_memories);
            }

            println!("[INFO] 群聊继续对话 (群组: {}, 用户: {})", group_id, nickname);
            let resp = params_model(vec).await;
            if !resp.content.contains("[sp]") {
                bot.send_group_msg(group_id, &resp.content);
                println!("[INFO] 群聊消息已发送 (群组: {}): {}", group_id, resp.content);
            };
            vec.push(resp);

            // 检查并限制记忆大小
            limit_memory_size(vec);
        }
    }
}

/// 判断是否需要添加记忆上下文
/// 
/// 当对话较短且存在相关记忆时，将记忆注入到对话上下文中
/// 
/// # 参数
/// * `current_length` - 当前对话长度
/// * `recent_memories` - 最近的相关记忆
/// 
/// # 返回值
/// 是否需要添加记忆上下文
fn should_add_memory_context(current_length: usize, recent_memories: &[crate::memory::MemoryEntry]) -> bool {
    // 如果对话较短且没有太多上下文，添加记忆
    current_length < 10 && !recent_memories.is_empty()
}

/// 将相关记忆添加到消息列表中
/// 
/// 在系统消息后添加相关记忆，增强AI的上下文理解能力
/// 
/// # 参数
/// * `messages` - 消息列表（可变引用）
/// * `memories` - 要添加的相关记忆
fn add_memory_context_to_messages(messages: &mut Vec<BotMemory>, memories: &[crate::memory::MemoryEntry]) {
    if memories.is_empty() {
        return;
    }

    // 在系统消息后添加相关记忆
    if messages.len() > 1 {
        let memory_context = format!("\n\n相关记忆：\n{}", 
            memories.iter()
                .take(2)
                .map(|m| format!("- {}", m.content))
                .collect::<Vec<_>>()
                .join("\n")
        );
        
        if let Some(system_msg) = messages.first_mut() {
            if system_msg.role == Roles::System {
                system_msg.content.push_str(&memory_context);
            }
        }
    }
}

/// 限制对话记忆大小
/// 
/// 保持最多25条记录（包括system prompt），防止内存过度使用
/// 优先保留最近的对话内容
/// 
/// # 参数
/// * `messages` - 消息列表（可变引用）
fn limit_memory_size(messages: &mut Vec<BotMemory>) {
    if messages.len() <= MAX_MEMORY_SIZE {
        return;
    }

    // 保留system prompt (第一条消息)
    let system_message = messages[0].clone();

    // 计算需要保留的消息数量（除了system prompt）
    let keep_count = MAX_MEMORY_SIZE - 1;

    // 保留最近的对话
    let recent_messages = messages.drain(messages.len() - keep_count..).collect::<Vec<_>>();

    // 重新构建消息列表
    messages.clear();
    messages.push(system_message);
    messages.extend(recent_messages);

    println!("[INFO] 对话记忆已清理，当前保留 {} 条记录", messages.len());
}

/// 调用AI模型生成回复
/// 
/// 向配置的AI模型发送请求，生成智能回复。包括以下功能：
/// - 添加情绪化思考过程
/// - 发送HTTP请求到AI模型
/// - 解析响应并清理格式
/// 
/// # 参数
/// * `messages` - 对话消息列表（可变引用）
/// 
/// # 返回值
/// 生成的机器人回复消息
/// 
/// # 错误处理
/// 如果API调用失败，返回默认错误消息
pub async fn params_model(messages: &mut Vec<BotMemory>) -> BotMemory {
    let config = config::get();
    let server_config = config.server_config();

    // 添加思考过程
    let thinking_prompt = generate_thinking_prompt(messages).await;
    if !thinking_prompt.is_empty() {
        messages.push(BotMemory {
            role: Roles::System,
            content: format!("思考过程：{}\n请基于以上思考给出回复。", thinking_prompt),
        });
    }

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

/// 生成情绪化思考过程
/// 
/// 根据机器人的当前人格状态生成个性化的思考过程，包括：
/// - 基于当前情绪调整思考风格
/// - 结合相关记忆增强上下文理解
/// - 根据能量水平调整思考深度
/// 
/// # 参数
/// * `_messages` - 对话消息列表（当前未使用）
/// 
/// # 返回值
/// 生成的思考过程文本
async fn generate_thinking_prompt(_messages: &[BotMemory]) -> String {
    let personality = MEMORY_MANAGER.get_bot_personality().await;
    let recent_memories = MEMORY_MANAGER.get_recent_memories(5).await;
    
    let mut thinking = String::new();
    
    // 根据当前情绪调整思考风格
    match personality.current_mood.as_str() {
        "curious" => {
            thinking.push_str("我需要仔细思考这个问题，看看有什么有趣的角度...");
        },
        "thoughtful" => {
            thinking.push_str("让我深入思考一下这个问题的本质...");
        },
        "playful" => {
            thinking.push_str("哈哈，这个问题挺有意思的，让我想想怎么回答...");
        },
        "happy" => {
            thinking.push_str("好开心！让我想想怎么回应...");
        },
        _ => {
            thinking.push_str("让我思考一下如何回应...");
        }
    }
    
    // 添加相关记忆到思考中
    if !recent_memories.is_empty() {
        thinking.push_str(" 我记得之前讨论过类似的话题...");
    }
    
    // 根据能量水平调整思考深度
    if personality.energy_level > 7 {
        thinking.push_str(" 我有很多想法要分享！");
    } else if personality.energy_level < 4 {
        thinking.push_str(" 虽然有点累，但还是认真想想吧...");
    }
    
    thinking
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
        eprintln!("[ERROR] 私聊情绪分析失败 (用户: {}): {}", user_id, e);
    }

    // 记录对话记忆
    if let Err(e) = MEMORY_MANAGER.add_conversation_memory(
        user_id,
        &format!("{}: {}", format_nickname, message),
        "private_chat"
    ).await {
        eprintln!("[ERROR] 私聊记忆记录失败 (用户: {}): {}", user_id, e);
    }

    // 更新用户档案
    update_user_profile_from_message(user_id, message, &format_nickname).await;

    // 获取用户档案和个性化信息
    let user_profile = MEMORY_MANAGER.get_user_profile(user_id).await;
    let contextual_memories = MEMORY_MANAGER.get_contextual_memories(user_id, "private_chat", 3).await;
    let personality = MEMORY_MANAGER.get_bot_personality().await;

    let mut private = get_private_message_memory().lock().await;
    let history = private.entry(user_id).or_insert(vec![
        BotMemory {
            role: Roles::System,
            content: generate_personalized_system_prompt(&user_profile, &personality, &contextual_memories).await,
        },
    ]);

    // 添加用户消息
    history.push(BotMemory {
        role: Roles::User,
        content: format!("{}:{}", format_nickname, message),
    });

    // 根据用户关系等级调整回复风格
    let relationship_level = user_profile.as_ref().map(|p| p.relationship_level).unwrap_or(1);
    adjust_response_style_for_relationship(history, relationship_level);

    println!("[INFO] 私聊对话 (用户: {})", user_id);
    let bot_content = params_model(history).await;
    bot.send_private_msg(user_id, &bot_content.content);
    println!("[INFO] 私聊消息已发送 (用户: {}): {}", user_id, bot_content.content);

    // 添加机器人回复
    history.push(bot_content);

    // 限制私聊记忆大小
    limit_memory_size(history);
}

async fn generate_personalized_system_prompt(
    user_profile: &Option<crate::memory::UserProfile>,
    personality: &crate::memory::BotPersonality,
    contextual_memories: &[crate::memory::MemoryEntry],
) -> String {
    let mut prompt = config::get().prompt().private_prompt().to_string();
    
    // 添加个性化信息
    if let Some(profile) = user_profile {
        prompt.push_str(&format!("\n\n用户信息：\n- 昵称：{}\n- 关系等级：{}/10\n- 互动次数：{}\n- 兴趣：{}", 
            profile.nickname,
            profile.relationship_level,
            profile.interaction_count,
            profile.interests.join(", ")
        ));
        
        // 根据关系等级调整语气
        match profile.relationship_level {
            8..=10 => prompt.push_str("\n- 语气：亲密友好，可以开玩笑"),
            5..=7 => prompt.push_str("\n- 语气：友好但保持一定距离"),
            1..=4 => prompt.push_str("\n- 语气：礼貌但较为正式"),
            _ => {}
        }
    }
    
    // 添加机器人当前状态
    prompt.push_str(&format!("\n\n当前状态：\n- 情绪：{}\n- 能量水平：{}/10\n- 社交信心：{}/10", 
        personality.current_mood,
        personality.energy_level,
        personality.social_confidence
    ));
    
    // 添加相关记忆
    if !contextual_memories.is_empty() {
        prompt.push_str("\n\n相关记忆：");
        for memory in contextual_memories.iter().take(2) {
            prompt.push_str(&format!("\n- {}", memory.content));
        }
    }
    
    prompt
}

fn adjust_response_style_for_relationship(history: &mut Vec<BotMemory>, relationship_level: u8) {
    if relationship_level >= 8 {
        // 高关系等级，可以更随意
        if let Some(system_msg) = history.first_mut() {
            if system_msg.role == Roles::System {
                system_msg.content.push_str("\n- 可以适当使用表情符号和网络用语");
                system_msg.content.push_str("\n- 可以开玩笑和调侃");
            }
        }
    } else if relationship_level <= 3 {
        // 低关系等级，保持礼貌
        if let Some(system_msg) = history.first_mut() {
            if system_msg.role == Roles::System {
                system_msg.content.push_str("\n- 保持礼貌和正式的语气");
                system_msg.content.push_str("\n- 避免过于随意或开玩笑");
            }
        }
    }
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
    if interests.is_empty() {
        return;
    }
    for interest in interests {
        if !profile.interests.contains(&interest) {
            profile.interests.push(interest);
        }
    };

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