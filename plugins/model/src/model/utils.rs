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
use crate::memory::{BotPersonality, MEMORY_MANAGER, MoodEntry, UserProfile};
use crate::mood_system::{Mood, MoodSystem};
use crate::utils;
use anyhow::Context;
use chrono::{Local, TimeZone};
use kovi::RuntimeBot;
use kovi::serde_json::Value;
use kovi::tokio::sync::Mutex;
use rand::Rng;
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;

/// 群聊对话记忆存储
///
/// 存储每个群组的对话历史，用于维护上下文连续性
/// Key: 群组ID, Value: 对话消息列表
type ConversationHistory = Arc<Mutex<Vec<BotMemory>>>;

static MEMORY: LazyLock<Mutex<HashMap<i64, ConversationHistory>>> =
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
static PRIVATE_MESSAGE_MEMORY: LazyLock<Mutex<HashMap<i64, ConversationHistory>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 全局情绪系统实例
///
/// 负责分析用户消息的情绪并调整机器人的人格状态
static MOOD_SYSTEM: LazyLock<MoodSystem> =
    LazyLock::new(|| MoodSystem::new(Arc::clone(&MEMORY_MANAGER)));

/// 聊天中由模型决定是否继续发送下一条消息的分隔标记。
const FOLLOW_UP_MARKER: &str = "[[NEXT_MESSAGE]]";

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
    messages: &'a [BotMemory],
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
    group_id: i64,
    bot: Arc<RuntimeBot>,
    nickname: String,
    message: &str,
) -> bool {
    // 分析情绪并更新
    if let Err(e) = MOOD_SYSTEM
        .analyze_and_update_mood(message, "group_chat")
        .await
    {
        eprintln!("[ERROR] 群聊情绪分析失败 (群组: {}): {}", group_id, e);
    }

    // 记录对话记忆
    if let Err(e) = MEMORY_MANAGER
        .add_conversation_memory(
            group_id,
            &format!("{}: {}", nickname, message),
            "group_chat",
        )
        .await
    {
        eprintln!("[ERROR] 群聊记忆记录失败 (群组: {}): {}", group_id, e);
    }

    // 获取相关记忆来增强上下文
    let contextual_memories = MEMORY_MANAGER
        .get_contextual_memories(
            group_id,
            "group_chat",
            config::get().memory().contextual_memory_limit(),
        )
        .await;
    let history = group_history(group_id).await;
    // 同一群内保持消息顺序，但不同群可以并发调用模型。
    let mut messages = history.lock().await;
    let is_new_conversation = messages.is_empty();
    if is_new_conversation {
        messages.push(BotMemory {
            role: Roles::System,
            content: String::new(),
        });
    }
    messages.push(BotMemory {
        role: Roles::User,
        content: format!("{}:{}", nickname, message),
    });
    let rolling_summary = maybe_compress_conversation(&mut messages, "group_chat", group_id).await;
    let system_prompt = group_system_prompt(&contextual_memories, rolling_summary.as_deref());
    if let Some(first) = messages.first_mut() {
        first.content = system_prompt;
    }

    println!(
        "[INFO] 群聊{}对话 (群组: {}, 用户: {})",
        if is_new_conversation { "新" } else { "继续" },
        group_id,
        nickname
    );
    let response = params_model(&mut messages).await;
    if !response.content.contains("[sp]") {
        let outbound_messages = split_reply(&response.content);
        let stored_reply = outbound_messages.join("\n");
        let personality = MEMORY_MANAGER.get_bot_personality().await;
        for (index, outbound_message) in outbound_messages.iter().enumerate() {
            if index > 0 {
                kovi::tokio::time::sleep(follow_up_delay(&personality, index)).await;
            }
            bot.send_group_msg(group_id, outbound_message);
        }
        println!(
            "[INFO] 群聊消息已发送 (群组: {}): {}",
            group_id, stored_reply
        );
        if let Err(error) = MEMORY_MANAGER
            .add_conversation_memory(group_id, &format!("芸汐: {}", stored_reply), "group_chat")
            .await
        {
            eprintln!(
                "[ERROR] 群聊回复记忆记录失败 (群组: {}): {}",
                group_id, error
            );
        }
        messages.push(BotMemory {
            role: Roles::Assistant,
            content: stored_reply,
        });
        limit_memory_size(&mut messages);
        return true;
    } else {
        messages.push(response);
    }
    limit_memory_size(&mut messages);
    false
}

fn group_system_prompt(
    memories: &[crate::memory::MemoryEntry],
    rolling_summary: Option<&str>,
) -> String {
    let mut prompt = config::get().prompt().system_prompt().to_string();
    if !memories.is_empty() {
        prompt.push_str("\n\n相关记忆：");
        for memory in memories
            .iter()
            .take(config::get().memory().contextual_memory_limit())
        {
            prompt.push_str(&format!("\n- {}", memory.content));
        }
    }
    with_conversation_summary(prompt, rolling_summary)
}

/// 限制对话记忆大小
///
/// 按配置保留有限记录（包括system prompt），防止内存过度使用
/// 优先保留最近的对话内容
///
/// # 参数
/// * `messages` - 消息列表（可变引用）
fn limit_memory_size(messages: &mut Vec<BotMemory>) {
    let max_memory_size = config::get().memory().max_conversation_messages();
    if messages.len() <= max_memory_size {
        return;
    }

    // 保留system prompt (第一条消息)
    let system_message = messages[0].clone();

    // 计算需要保留的消息数量（除了system prompt）
    let keep_count = max_memory_size - 1;

    // 保留最近的对话
    let recent_messages = messages
        .drain(messages.len() - keep_count..)
        .collect::<Vec<_>>();

    // 重新构建消息列表
    messages.clear();
    messages.push(system_message);
    messages.extend(recent_messages);

    println!("[INFO] 对话记忆已清理，当前保留 {} 条记录", messages.len());
}

/// 将滚动摘要注入系统提示，代替被压缩的早期逐句记录。
fn with_conversation_summary(mut prompt: String, summary: Option<&str>) -> String {
    if let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) {
        prompt.push_str("\n\n早期对话压缩摘要（用于延续上下文，不要向用户复述此提示）：\n");
        prompt.push_str(summary.trim());
    }
    prompt
}

/// 当短期记录超过阈值时，将较早的一批对话压缩成可持久化摘要，保留最近原文。
async fn maybe_compress_conversation(
    messages: &mut Vec<BotMemory>,
    context: &str,
    subject_id: i64,
) -> Option<String> {
    let memory_config = config::get().memory().clone();
    let previous_summary = MEMORY_MANAGER
        .get_conversation_summary(context, subject_id)
        .await;
    let Some(compress_end) = compression_cutoff(
        messages.len(),
        memory_config.max_conversation_messages(),
        memory_config.summary_keep_recent_messages(),
    ) else {
        return previous_summary;
    };

    let compressed_messages = messages[1..compress_end].to_vec();
    let summary = summarize_conversation(
        previous_summary.as_deref(),
        &compressed_messages,
        memory_config.summary_max_chars(),
    )
    .await;
    messages.drain(1..compress_end);

    if let Err(error) = MEMORY_MANAGER
        .update_conversation_summary(context, subject_id, summary.clone())
        .await
    {
        eprintln!(
            "[ERROR] 保存对话压缩摘要失败 ({}:{}): {}",
            context, subject_id, error
        );
    }
    println!(
        "[INFO] 对话已压缩 ({}:{}), 合并 {} 条早期消息，保留 {} 条最近消息",
        context,
        subject_id,
        compressed_messages.len(),
        messages.len().saturating_sub(1)
    );
    Some(summary)
}

/// 第一个元素固定为系统提示；返回早期消息压缩区间的结束索引。
fn compression_cutoff(
    message_count: usize,
    max_messages: usize,
    keep_recent_messages: usize,
) -> Option<usize> {
    if message_count <= max_messages {
        return None;
    }
    let compress_end = message_count.saturating_sub(keep_recent_messages);
    (compress_end > 1).then_some(compress_end)
}

async fn summarize_conversation(
    previous_summary: Option<&str>,
    messages: &[BotMemory],
    max_chars: usize,
) -> String {
    let transcript = conversation_transcript(messages, max_chars.saturating_mul(3));
    let mut request = vec![
        BotMemory {
            role: Roles::System,
            content: format!(
                "你是聊天记录压缩器。将早期对话更新为一段不超过 {max_chars} 个字符的中文摘要。\
                 保留：用户身份/偏好、已确认的事实与计划、承诺、未解决问题、重要情绪与关系上下文，以及必要的说话者归属。\
                 忽略寒暄和重复。只输出摘要，不要回答对话，不要使用 [[NEXT_MESSAGE]]。"
            ),
        },
        BotMemory {
            role: Roles::User,
            content: format!(
                "已有摘要：\n{}\n\n需要合并的较早对话：\n{}",
                previous_summary.unwrap_or("（无）"),
                transcript
            ),
        },
    ];
    let response = params_model(&mut request).await;
    let summary = response
        .content
        .replace(FOLLOW_UP_MARKER, "\n")
        .trim()
        .to_string();
    if summary.is_empty() || summary.starts_with("抱歉，模型服务暂时不可用") {
        return fallback_summary(previous_summary, &transcript, max_chars);
    }
    truncate_chars(&summary, max_chars)
}

fn conversation_transcript(messages: &[BotMemory], max_chars: usize) -> String {
    let mut transcript = String::new();
    for message in messages {
        let role = match &message.role {
            Roles::System => "系统",
            Roles::User => "用户",
            Roles::Assistant => "芸汐",
        };
        transcript.push_str(role);
        transcript.push('：');
        transcript.push_str(&message.content);
        transcript.push('\n');
        if transcript.chars().count() >= max_chars {
            break;
        }
    }
    truncate_chars(&transcript, max_chars)
}

fn fallback_summary(previous_summary: Option<&str>, transcript: &str, max_chars: usize) -> String {
    let previous_limit = max_chars / 2;
    let transcript_limit = max_chars.saturating_sub(previous_limit);
    format!(
        "{}\n近期压缩片段：{}",
        previous_summary
            .map(|summary| truncate_chars(summary, previous_limit))
            .unwrap_or_default(),
        truncate_chars(transcript, transcript_limit)
    )
    .trim()
    .to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
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
pub async fn params_model(messages: &mut [BotMemory]) -> BotMemory {
    let config = config::get();
    let server_config = config.server_config();

    // 思考提示只用于本次请求，不写回长期会话，避免 system 消息不断累积。
    let mut request_messages = messages.to_owned();
    let thinking_prompt = generate_thinking_prompt(messages).await;
    if !thinking_prompt.is_empty() {
        request_messages.push(BotMemory {
            role: Roles::System,
            content: format!("思考过程：{}\n请基于以上思考给出回复。", thinking_prompt),
        });
    }

    let bot_conf = ModelConf {
        model: server_config.model_name(),
        messages: &request_messages,
        stream: false,
        temperature: 0.7,
    };
    let token = match std::env::var("BOT_API_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token,
        _ => return model_error("未设置 BOT_API_TOKEN，暂时无法调用对话模型"),
    };
    let client = match Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(error) => return model_error(&format!("创建模型客户端失败: {}", error)),
    };
    let resp = match client
        .post(server_config.url())
        .bearer_auth(token)
        .json(&bot_conf)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
    {
        Ok(response) => response,
        Err(error) => return model_error(&format!("模型请求失败: {}", error)),
    };
    let text = match resp.json::<Value>().await {
        Ok(text) => text,
        Err(error) => return model_error(&format!("模型响应解析失败: {}", error)),
    };
    let bot_content = match text
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        Some(content) => content,
        None => return model_error("模型响应中缺少 choices[0].message.content"),
    }
    .trim()
    .replace("芸汐：", "")
    .to_string();
    BotMemory {
        role: Roles::Assistant,
        content: bot_content,
    }
}

fn model_error(error: &str) -> BotMemory {
    eprintln!("[ERROR] {}", error);
    BotMemory {
        role: Roles::Assistant,
        content: format!("抱歉，模型服务暂时不可用（{}）。", error),
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
/// * `messages` - 对话消息列表，用于判断是否注入了相关记忆
///
/// # 返回值
/// 生成的思考过程文本
async fn generate_thinking_prompt(messages: &[BotMemory]) -> String {
    let personality = MEMORY_MANAGER.get_bot_personality().await;

    let mut thinking = String::new();

    // 根据当前情绪调整思考风格
    match personality.current_mood.as_str() {
        "curious" => {
            thinking.push_str("我需要仔细思考这个问题，看看有什么有趣的角度...");
        }
        "thoughtful" => {
            thinking.push_str("让我深入思考一下这个问题的本质...");
        }
        "playful" => {
            thinking.push_str("哈哈，这个问题挺有意思的，让我想想怎么回答...");
        }
        "happy" => {
            thinking.push_str("好开心！让我想想怎么回应...");
        }
        _ => {
            thinking.push_str("让我思考一下如何回应...");
        }
    }

    // 添加相关记忆到思考中
    let has_contextual_memories = messages
        .first()
        .is_some_and(|message| message.content.contains("相关记忆："));
    if has_contextual_memories {
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

async fn group_history(group_id: i64) -> ConversationHistory {
    MEMORY
        .lock()
        .await
        .entry(group_id)
        .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

async fn private_history(user_id: i64) -> ConversationHistory {
    PRIVATE_MESSAGE_MEMORY
        .lock()
        .await
        .entry(user_id)
        .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
        .clone()
}

pub async fn silence(group_id: i64, message: &str, bot: Arc<RuntimeBot>, sender: String) -> bool {
    if message == "#禁言" {
        instance_is_ban().lock().await.insert(group_id, true);
        bot.send_group_msg(group_id, "禁言成功");
        return false;
    }
    if message == "#结束禁言" {
        instance_is_ban().lock().await.insert(group_id, false);
        bot.send_group_msg(group_id, "结束成功");
        return false;
    }

    // 读取状态后立即释放锁，避免一次模型网络请求阻塞其他群的状态操作。
    let is_banned = *instance_is_ban()
        .lock()
        .await
        .get(&group_id)
        .unwrap_or(&false);
    if !is_banned {
        control_model(group_id, bot, sender, message).await
    } else {
        false
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

pub async fn private_chat(user_id: i64, message: &str, nickname: String, bot: Arc<RuntimeBot>) {
    // 分析情绪并更新
    let detected_mood = match MOOD_SYSTEM
        .analyze_and_update_mood(message, "private_chat")
        .await
    {
        Ok(mood) => Some(mood),
        Err(e) => {
            eprintln!("[ERROR] 私聊情绪分析失败 (用户: {}): {}", user_id, e);
            None
        }
    };

    // 记录对话记忆
    if let Err(e) = MEMORY_MANAGER
        .add_conversation_memory(
            user_id,
            &format!("{}: {}", nickname, message),
            "private_chat",
        )
        .await
    {
        eprintln!("[ERROR] 私聊记忆记录失败 (用户: {}): {}", user_id, e);
    }

    // 更新用户档案
    update_user_profile_from_message(user_id, message, &nickname, true, detected_mood).await;

    // 获取用户档案和个性化信息
    let user_profile = MEMORY_MANAGER.get_user_profile(user_id).await;
    let contextual_memories = MEMORY_MANAGER
        .get_contextual_memories(
            user_id,
            "private_chat",
            config::get().memory().contextual_memory_limit(),
        )
        .await;
    let personality = MEMORY_MANAGER.get_bot_personality().await;

    let personalized_prompt =
        generate_personalized_system_prompt(&user_profile, &personality, &contextual_memories)
            .await;
    let private = private_history(user_id).await;
    // 同一用户的私聊按顺序处理，不阻塞其他用户。
    let mut history = private.lock().await;
    if history.is_empty() {
        history.push(BotMemory {
            role: Roles::System,
            content: String::new(),
        });
    }
    // 添加用户消息后才判断是否需要压缩，使本轮消息也会进入滚动摘要的范围。
    history.push(BotMemory {
        role: Roles::User,
        content: format!("{}:{}", nickname, message),
    });
    let rolling_summary = maybe_compress_conversation(&mut history, "private_chat", user_id).await;
    if let Some(system_message) = history.first_mut() {
        system_message.content =
            with_conversation_summary(personalized_prompt, rolling_summary.as_deref());
    }

    println!("[INFO] 私聊对话 (用户: {})", user_id);
    let bot_content = params_model(&mut history).await;
    let outbound_messages = split_reply(&bot_content.content);
    let stored_reply = outbound_messages.join("\n");
    let personality = MEMORY_MANAGER.get_bot_personality().await;
    for (index, outbound_message) in outbound_messages.iter().enumerate() {
        if index > 0 {
            kovi::tokio::time::sleep(follow_up_delay(&personality, index)).await;
        }
        bot.send_private_msg(user_id, outbound_message);
    }
    println!(
        "[INFO] 私聊消息已发送 (用户: {}): {}",
        user_id, stored_reply
    );
    if let Err(error) = MEMORY_MANAGER
        .add_conversation_memory(user_id, &format!("芸汐: {}", stored_reply), "private_chat")
        .await
    {
        eprintln!(
            "[ERROR] 私聊回复记忆记录失败 (用户: {}): {}",
            user_id, error
        );
    }

    // 添加机器人回复
    history.push(BotMemory {
        role: Roles::Assistant,
        content: stored_reply,
    });

    // 限制私聊记忆大小
    limit_memory_size(&mut history);
}

/// 将模型给出的回复拆成任意数量的消息。
///
/// `[[NEXT_MESSAGE]]` 是明确的分段指令；某些模型会自然地以换行分段而省略标记，
/// 此时也把每个非空行当作一个气泡，避免一段本应连续说出的内容挤成单条消息。
fn split_reply(content: &str) -> Vec<String> {
    let marked_sections = content
        .split(FOLLOW_UP_MARKER)
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if marked_sections.len() > 1 {
        return marked_sections;
    }

    let Some(reply) = marked_sections.into_iter().next() else {
        return vec!["……".to_string()];
    };

    let line_sections = reply
        .lines()
        .map(str::trim)
        .filter(|section| !section.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if line_sections.len() > 1 {
        line_sections
    } else {
        vec![reply]
    }
}

/// 根据当前情绪、能量、社交信心和少量随机浮动，决定下一条消息前的停顿。
/// 活跃情绪更快，内敛或低落情绪会留出更长的思考空隙。
fn follow_up_delay(personality: &BotPersonality, message_index: usize) -> std::time::Duration {
    let variation_ms = rand::rng().random_range(-200_i64..=450_i64);
    std::time::Duration::from_millis(follow_up_delay_millis(
        personality,
        message_index,
        variation_ms,
    ))
}

fn follow_up_delay_millis(
    personality: &BotPersonality,
    message_index: usize,
    variation_ms: i64,
) -> u64 {
    let mood_base_ms = match personality.current_mood.as_str() {
        "excited" => 280,
        "playful" => 380,
        "happy" => 480,
        "curious" | "confident" => 560,
        "neutral" => 800,
        "calm" => 1_100,
        "thoughtful" => 1_450,
        "shy" | "lonely" => 1_600,
        "angry" => 1_500,
        "sad" => 1_800,
        _ => 800,
    };
    let energy_adjustment_ms = (5_i64 - i64::from(personality.energy_level)) * 45;
    let confidence_adjustment_ms = (5_i64 - i64::from(personality.social_confidence)) * 25;
    let intensity_adjustment_ms = match personality.current_mood.as_str() {
        "excited" | "playful" | "happy" if personality.mood_intensity >= 7 => -120,
        "sad" | "shy" | "thoughtful" if personality.mood_intensity >= 7 => 160,
        _ => 0,
    };
    // 连续表达越往后稍留空隙，但不会形成固定节拍。
    let sequence_adjustment_ms = (message_index.saturating_sub(1).min(6) as i64) * 70;
    (mood_base_ms
        + energy_adjustment_ms
        + confidence_adjustment_ms
        + intensity_adjustment_ms
        + sequence_adjustment_ms
        + variation_ms)
        .clamp(180, 4_500) as u64
}

async fn generate_personalized_system_prompt(
    user_profile: &Option<crate::memory::UserProfile>,
    personality: &crate::memory::BotPersonality,
    contextual_memories: &[crate::memory::MemoryEntry],
) -> String {
    let mut prompt = config::get().prompt().private_prompt().to_string();

    // 添加个性化信息
    if let Some(profile) = user_profile {
        prompt.push_str(&format!(
            "\n\n用户信息：\n- 昵称：{}\n- 关系等级：{}/10\n- 互动次数：{}\n- 兴趣：{}",
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
    prompt.push_str(&format!(
        "\n\n当前状态：\n- 情绪：{}\n- 能量水平：{}/10\n- 社交信心：{}/10",
        personality.current_mood, personality.energy_level, personality.social_confidence
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

pub(crate) async fn learn_user_profile_from_message(
    user_id: i64,
    message: &str,
    nickname: &str,
    is_private: bool,
) {
    let detected_mood = MOOD_SYSTEM
        .analyze_mood(
            message,
            if is_private {
                "private_chat"
            } else {
                "group_chat"
            },
        )
        .await;
    update_user_profile_from_message(user_id, message, nickname, is_private, Some(detected_mood))
        .await;
}

async fn update_user_profile_from_message(
    user_id: i64,
    message: &str,
    nickname: &str,
    is_private: bool,
    detected_mood: Option<Mood>,
) {
    let mut profile = MEMORY_MANAGER
        .get_user_profile(user_id)
        .await
        .unwrap_or_else(|| UserProfile {
            user_id,
            nickname: nickname.to_string(),
            personality_traits: Vec::new(),
            interests: Vec::new(),
            relationship_level: 1,
            last_interaction: Local::now(),
            interaction_count: 0,
            last_private_interaction: None,
            mood_history: Vec::new(),
        });

    // 更新互动信息
    if !nickname.trim().is_empty() {
        profile.nickname = nickname.to_string();
    }
    profile.last_interaction = Local::now();
    profile.interaction_count = profile.interaction_count.saturating_add(1);
    if is_private {
        profile.last_private_interaction = Some(Local::now());
    }

    // 随互动次数自然提升关系等级，感谢类表达再额外提升一级。
    profile.relationship_level = profile
        .relationship_level
        .max(1 + (profile.interaction_count / 20).min(9) as u8);

    // 配置中的最信任用户始终保持最高关系等级，供主动关心和语气个性化使用。
    if config::get().proactive().main_admin() == Some(user_id) {
        profile.relationship_level = 10;
    }

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
    profile.interests.truncate(20);

    for personality_trait in extract_personality_traits(message) {
        if !profile.personality_traits.contains(&personality_trait) {
            profile.personality_traits.push(personality_trait);
        }
    }
    profile.personality_traits.truncate(20);

    if let Some(mood) = detected_mood {
        profile.mood_history.push(MoodEntry {
            mood: mood.to_string(),
            intensity: 5,
            timestamp: Local::now(),
            trigger: message.chars().take(80).collect(),
        });
        if profile.mood_history.len() > 50 {
            profile
                .mood_history
                .drain(0..profile.mood_history.len() - 50);
        }
    }

    // 更新用户档案
    if let Err(e) = MEMORY_MANAGER.update_user_profile(user_id, profile).await {
        eprintln!("[ERROR] 更新用户档案失败 (用户: {}): {}", user_id, e);
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

fn extract_personality_traits(message: &str) -> Vec<String> {
    let trait_keywords = [
        ("友善", ["谢谢", "感谢", "辛苦", "关心"]),
        ("好奇", ["为什么", "怎么", "想知道", "好奇"]),
        ("幽默", ["哈哈", "笑死", "开玩笑", "有趣"]),
        ("勤奋", ["学习", "工作", "努力", "练习"]),
        ("体贴", ["没事吧", "注意休息", "保重", "别难过"]),
    ];
    trait_keywords
        .iter()
        .filter(|(_, keywords)| keywords.iter().any(|keyword| message.contains(keyword)))
        .map(|(name, _)| (*name).to_string())
        .collect()
}

pub fn get_file_modified_time_formatted() -> anyhow::Result<String> {
    let config_path = "bot.conf.toml";
    if !Path::new(config_path).exists() {
        return Ok("文件不存在".to_string());
    }

    let metadata = fs::metadata(config_path)
        .with_context(|| anyhow::anyhow!("Failed to get file metadata"))?;

    let modified = metadata
        .modified()
        .with_context(|| anyhow::anyhow!("Failed to get modification time"))?;

    let since_epoch = modified
        .duration_since(UNIX_EPOCH)
        .with_context(|| anyhow::anyhow!("Failed to calculate time since epoch"))?;

    // 转换为本地时间并格式化
    let datetime = Local
        .timestamp_opt(since_epoch.as_secs() as i64, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?;

    Ok(datetime.format("%Y-%m-%d %H:%M:%S").to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        BotMemory, Roles, compression_cutoff, extract_interests_from_message,
        extract_personality_traits, follow_up_delay_millis, limit_memory_size, split_reply,
        with_conversation_summary,
    };
    use crate::memory::BotPersonality;
    use chrono::Local;

    #[test]
    fn profile_signals_are_extracted_from_messages() {
        let interests = extract_interests_from_message("谢谢，我最近在学习 Rust，也常听音乐");
        assert!(interests.contains(&"学习".to_string()));
        assert!(interests.contains(&"音乐".to_string()));

        let traits = extract_personality_traits("谢谢你的关心，我会继续努力学习");
        assert!(traits.contains(&"友善".to_string()));
        assert!(traits.contains(&"勤奋".to_string()));
    }

    #[test]
    fn short_term_history_keeps_system_prompt_and_recent_messages() {
        let mut messages = vec![BotMemory {
            role: Roles::System,
            content: "system".to_string(),
        }];
        for index in 0..40 {
            messages.push(BotMemory {
                role: Roles::User,
                content: format!("message-{index}"),
            });
        }

        limit_memory_size(&mut messages);

        assert_eq!(
            messages.len(),
            crate::config::get().memory().max_conversation_messages()
        );
        assert_eq!(messages[0].role, Roles::System);
        assert_eq!(messages[0].content, "system");
        assert_eq!(
            messages.last().map(|message| message.content.as_str()),
            Some("message-39")
        );
    }

    #[test]
    fn long_conversation_is_compressed_before_old_messages_are_dropped() {
        assert_eq!(compression_cutoff(25, 25, 15), None);
        // 系统提示位于 0；压缩 1..11 共 10 条，仍保留最近 15 条原文。
        assert_eq!(compression_cutoff(26, 25, 15), Some(11));
        let prompt =
            with_conversation_summary("system".to_string(), Some("用户偏好 Rust，正在准备考试。"));
        assert!(prompt.contains("用户偏好 Rust"));
    }

    #[test]
    fn reply_is_single_message_without_follow_up_marker() {
        assert_eq!(
            split_reply("今天也要好好休息呀"),
            vec!["今天也要好好休息呀"]
        );
    }

    #[test]
    fn reply_can_send_every_model_selected_message() {
        assert_eq!(
            split_reply("第一句 [[NEXT_MESSAGE]] 第二句 [[NEXT_MESSAGE]] 第三句"),
            vec!["第一句", "第二句", "第三句"]
        );
    }

    #[test]
    fn reply_uses_natural_line_breaks_as_follow_up_fallback() {
        assert_eq!(
            split_reply("第一句\n第二句\n\n第三句"),
            vec!["第一句", "第二句", "第三句"]
        );
    }

    #[test]
    fn follow_up_pacing_reflects_mood_and_energy() {
        let lively = personality("excited", 9, 9, 8);
        let reserved = personality("sad", 2, 2, 8);

        assert!(follow_up_delay_millis(&lively, 1, 0) < follow_up_delay_millis(&reserved, 1, 0));
    }

    fn personality(
        mood: &str,
        energy_level: u8,
        social_confidence: u8,
        mood_intensity: u8,
    ) -> BotPersonality {
        BotPersonality {
            current_mood: mood.to_string(),
            mood_intensity,
            energy_level,
            social_confidence,
            curiosity_level: 5,
            last_mood_change: Local::now(),
            personality_traits: Vec::new(),
        }
    }
}
