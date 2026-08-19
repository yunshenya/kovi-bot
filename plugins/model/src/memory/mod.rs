//! # 记忆管理系统
//!
//! 提供智能的长期记忆存储和检索功能，支持：
//! - 多类型记忆分类存储
//! - 智能重要性评分
//! - 上下文相关记忆检索
//! - 用户和群组档案管理
//! - 机器人人格状态维护
//! - 自动记忆清理和优化

use anyhow::Result;
use chrono::{DateTime, Local};
use kovi::tokio::sync::Mutex;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

static MEMORY_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 全局记忆管理器实例
///
/// 使用LazyLock确保线程安全的单例模式，在首次访问时初始化
/// 记忆文件默认保存为 "bot_memory.json"
pub static MEMORY_MANAGER: LazyLock<Arc<MemoryManager>> =
    LazyLock::new(|| Arc::new(MemoryManager::new("bot_memory.json")));

/// 记忆条目结构体
///
/// 存储单条记忆的完整信息，包括内容、时间戳、类型、重要性等
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryEntry {
    /// 记忆唯一标识符
    pub id: String,
    /// 记忆内容文本
    pub content: String,
    /// 记忆创建时间
    pub timestamp: DateTime<Local>,
    /// 记忆类型分类
    pub memory_type: MemoryType,
    /// 重要性评分 (0-10)，10表示最重要
    pub importance: u8,
    /// 标签列表，用于快速检索和分类
    pub tags: Vec<String>,
    /// 上下文信息，描述记忆产生的环境
    pub context: String,
    /// 该记忆所属的用户或群组。旧版记忆反序列化时为 `None`。
    #[serde(default)]
    pub subject_id: Option<i64>,
}

/// 记忆类型枚举
///
/// 定义不同类型的记忆，用于分类存储和检索
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum MemoryType {
    /// 对话记忆：存储用户与机器人的对话内容
    Conversation,
    /// 用户档案：存储用户的基本信息和偏好
    UserProfile,
    /// 群组信息：存储群组的基本信息和活跃状态
    GroupInfo,
    /// 事件记忆：存储重要事件和里程碑
    Event,
    /// 偏好设置：存储用户或系统的偏好配置
    Preference,
    /// 情绪状态：存储机器人的情绪变化记录
    Emotion,
}

/// 用户档案结构体
///
/// 存储用户的详细信息，用于个性化交互和关系管理
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserProfile {
    /// 用户唯一标识符
    pub user_id: i64,
    /// 用户昵称
    pub nickname: String,
    /// 用户性格特征列表
    pub personality_traits: Vec<String>,
    /// 用户兴趣标签列表
    pub interests: Vec<String>,
    /// 关系亲密度 (0-10)，10表示最亲密
    pub relationship_level: u8,
    /// 最后互动时间
    pub last_interaction: DateTime<Local>,
    /// 总互动次数
    pub interaction_count: u32,
    /// 最近一次私聊时间。只有真正私聊过的用户才会成为主动私聊候选。
    #[serde(default)]
    pub last_private_interaction: Option<DateTime<Local>>,
    /// 情绪历史记录
    pub mood_history: Vec<MoodEntry>,
}

/// 情绪记录条目
///
/// 记录单次情绪变化的信息
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MoodEntry {
    /// 情绪名称
    pub mood: String,
    /// 情绪强度 (0-10)，10表示最强烈
    pub intensity: u8,
    /// 情绪变化时间
    pub timestamp: DateTime<Local>,
    /// 情绪触发原因
    pub trigger: String,
}

/// 群组档案结构体
///
/// 存储群组的基本信息和活跃状态
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GroupProfile {
    /// 群组唯一标识符
    pub group_id: i64,
    /// 群组名称
    pub group_name: String,
    /// 活跃成员ID列表
    pub active_members: Vec<i64>,
    /// 群组整体性格特征
    pub group_personality: String,
    /// 群组常讨论的话题列表
    pub conversation_topics: Vec<String>,
    /// 最后活跃时间
    pub last_activity: DateTime<Local>,
    /// 活跃度等级 (0-10)，10表示最活跃
    pub activity_level: u8,
}

/// 机器人人格结构体
///
/// 存储机器人的当前状态和人格特征
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BotPersonality {
    /// 当前情绪状态
    pub current_mood: String,
    /// 情绪强度 (0-10)
    pub mood_intensity: u8,
    /// 能量水平 (0-10)，影响回复的积极性
    pub energy_level: u8,
    /// 社交信心 (0-10)，影响主动聊天的频率
    pub social_confidence: u8,
    /// 好奇心水平 (0-10)，影响话题探索的积极性
    pub curiosity_level: u8,
    /// 最后情绪变化时间
    pub last_mood_change: DateTime<Local>,
    /// 人格特征列表
    pub personality_traits: Vec<String>,
}

/// 记忆管理器结构体
///
/// 负责管理所有类型的记忆数据，包括：
/// - 对话记忆的存储和检索
/// - 用户和群组档案的管理
/// - 机器人人格状态的维护
/// - 记忆的持久化存储和加载
/// - 智能记忆清理和优化
#[derive(Clone)]
pub struct MemoryManager {
    /// 记忆条目存储 (ID -> MemoryEntry)
    memories: Arc<Mutex<HashMap<String, MemoryEntry>>>,
    /// 用户档案存储 (UserID -> UserProfile)
    user_profiles: Arc<Mutex<HashMap<i64, UserProfile>>>,
    /// 群组档案存储 (GroupID -> GroupProfile)
    group_profiles: Arc<Mutex<HashMap<i64, GroupProfile>>>,
    /// 机器人人格状态
    bot_personality: Arc<Mutex<BotPersonality>>,
    /// 记忆文件路径
    memory_file: String,
    /// 串行化持久化操作，避免多个任务同时覆盖记忆文件。
    save_lock: Arc<Mutex<()>>,
}

impl MemoryManager {
    /// 创建新的记忆管理器实例
    ///
    /// # 参数
    /// * `memory_file` - 记忆数据持久化文件路径
    ///
    /// # 返回值
    /// 返回初始化的MemoryManager实例，包含默认的机器人人格设置
    ///
    /// # 默认人格特征
    /// - 当前情绪：中性
    /// - 情绪强度：5/10
    /// - 能量水平：7/10
    /// - 社交信心：6/10
    /// - 好奇心：8/10
    /// - 性格特征：好奇、顽皮、有同理心、轻微傲娇
    pub fn new(memory_file: &str) -> Self {
        // 构造阶段同步读取，保证第一条消息不会和后台加载任务竞态并覆盖旧数据。
        let data = match fs::read_to_string(memory_file) {
            Ok(json) => match serde_json::from_str::<MemoryData>(&json) {
                Ok(data) => data,
                Err(error) => {
                    let backup_path =
                        format!("{}.corrupt.{}", memory_file, Local::now().timestamp());
                    match fs::copy(memory_file, &backup_path) {
                        Ok(_) => eprintln!(
                            "[ERROR] 记忆文件解析失败，已备份到 {}: {}",
                            backup_path, error
                        ),
                        Err(backup_error) => eprintln!(
                            "[ERROR] 记忆文件解析失败且备份失败 ({}): {}",
                            backup_error, error
                        ),
                    }
                    MemoryData::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MemoryData::default(),
            Err(error) => {
                eprintln!("[ERROR] 记忆文件读取失败，将使用空记忆: {}", error);
                MemoryData::default()
            }
        };

        Self {
            memories: Arc::new(Mutex::new(data.memories)),
            user_profiles: Arc::new(Mutex::new(data.user_profiles)),
            group_profiles: Arc::new(Mutex::new(data.group_profiles)),
            bot_personality: Arc::new(Mutex::new(data.bot_personality)),
            memory_file: memory_file.to_string(),
            save_lock: Arc::new(Mutex::new(())),
        }
    }

    /// 添加新的记忆条目
    ///
    /// # 参数
    /// * `memory` - 要添加的记忆条目
    ///
    /// # 返回值
    /// 成功时返回 `Ok(())`，失败时返回错误信息
    ///
    /// # 注意
    /// 添加记忆后会自动保存到文件
    pub async fn add_memory(&self, memory: MemoryEntry) -> Result<()> {
        {
            let mut memories = self.memories.lock().await;
            memories.insert(memory.id.clone(), memory);
        }
        self.save_memories().await
    }

    /// 根据类型获取记忆条目
    ///
    /// # 参数
    /// * `memory_type` - 要查询的记忆类型
    ///
    /// # 返回值
    /// 返回指定类型的所有记忆条目
    pub async fn get_memories_by_type(&self, memory_type: &MemoryType) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| {
                std::mem::discriminant(&m.memory_type) == std::mem::discriminant(memory_type)
            })
            .cloned()
            .collect()
    }

    /// 获取最近的记忆条目
    ///
    /// # 参数
    /// * `limit` - 返回的最大记忆条目数量
    ///
    /// # 返回值
    /// 按时间倒序排列的最近记忆条目列表
    pub async fn get_recent_memories(&self, limit: usize) -> Vec<MemoryEntry> {
        let mut memories: Vec<MemoryEntry> = self.memories.lock().await.values().cloned().collect();
        memories.sort_by_key(|memory| Reverse(memory.timestamp));
        // limit=0 用于健康检查，表示返回全部记忆。
        if limit > 0 {
            memories.truncate(limit);
        }
        memories
    }

    /// 获取重要性达到指定阈值的记忆条目
    ///
    /// # 参数
    /// * `min_importance` - 最小重要性阈值 (0-10)
    ///
    /// # 返回值
    /// 重要性大于等于阈值的记忆条目列表
    pub async fn get_important_memories(&self, min_importance: u8) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| m.importance >= min_importance)
            .cloned()
            .collect()
    }

    /// 智能搜索记忆条目
    ///
    /// 使用多因素评分算法搜索相关记忆，考虑以下因素：
    /// - 内容完全匹配 (10分)
    /// - 标签匹配 (5分)
    /// - 记忆重要性 (0-10分)
    /// - 时间权重：7天内(3分)，30天内(2分)，90天内(1分)
    ///
    /// # 参数
    /// * `query` - 搜索查询字符串
    ///
    /// # 返回值
    /// 按相关性得分排序的记忆条目列表
    pub async fn search_memories(&self, query: &str) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        let query_lower = query.trim().to_lowercase();
        if query_lower.is_empty() {
            return Vec::new();
        }
        let query_terms: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .collect();

        let mut results: Vec<(MemoryEntry, u8)> = memories
            .values()
            .filter_map(|m| {
                let mut score = 0u8;
                let content_lower = m.content.to_lowercase();
                let content_match = content_lower.contains(&query_lower)
                    || query_terms.iter().any(|term| content_lower.contains(term));
                let tag_matches = m
                    .tags
                    .iter()
                    .filter(|tag| {
                        let tag = tag.to_lowercase();
                        tag.contains(&query_lower)
                            || query_terms.iter().any(|term| tag.contains(term))
                    })
                    .count();

                // 无内容或标签匹配的记忆不应仅凭重要性进入搜索结果。
                if !content_match && tag_matches == 0 {
                    return None;
                }

                // 完全匹配得分最高
                if content_match {
                    score += 10;
                }

                // 标签匹配
                score = score.saturating_add((tag_matches.min(3) as u8) * 5);

                // 重要性权重
                score += m.importance;

                // 时间权重（越近越重要）
                let now = Local::now();
                let days_ago = now.signed_duration_since(m.timestamp).num_days();
                if days_ago < 7 {
                    score += 3;
                } else if days_ago < 30 {
                    score += 2;
                } else if days_ago < 90 {
                    score += 1;
                }

                Some((m.clone(), score))
            })
            .collect();

        // 按得分排序
        results.sort_by_key(|result| Reverse(result.1));

        results.into_iter().map(|(memory, _)| memory).collect()
    }

    pub async fn get_contextual_memories(
        &self,
        user_id: i64,
        context: &str,
        limit: usize,
    ) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        let mut contextual_memories: Vec<(MemoryEntry, u8)> = Vec::new();
        let requested_scope = context_scope(context);

        for memory in memories.values() {
            let mut relevance_score = 0u8;

            // 新版数据精确匹配所属对象；旧版数据仍可通过上下文参与检索。
            match memory.subject_id {
                Some(subject_id) if subject_id == user_id => relevance_score += 5,
                Some(_) => continue,
                // 旧版数据没有所属对象，不能安全注入其他人的上下文。
                None => continue,
            }

            // 用户号和群号都使用 i64，数值偶然相同时也不能跨私聊/群聊注入记忆。
            if requested_scope.is_some() && context_scope(&memory.context) != requested_scope {
                continue;
            }

            // 检查上下文匹配
            if memory.context == context {
                relevance_score += 3;
            }

            // 检查标签匹配
            let context_lower = context.to_lowercase();
            for tag in &memory.tags {
                if context_lower.contains(&tag.to_lowercase()) {
                    relevance_score += 2;
                }
            }

            // 重要性权重
            relevance_score += memory.importance;

            if relevance_score > 0 {
                contextual_memories.push((memory.clone(), relevance_score));
            }
        }

        // 按相关性排序并限制数量
        contextual_memories.sort_by_key(|result| Reverse(result.1));
        contextual_memories.truncate(limit);

        contextual_memories
            .into_iter()
            .map(|(memory, _)| memory)
            .collect()
    }

    pub async fn update_user_profile(&self, user_id: i64, profile: UserProfile) -> Result<()> {
        {
            let mut profiles = self.user_profiles.lock().await;
            profiles.insert(user_id, profile);
        }
        self.save_memories().await
    }

    pub async fn get_user_profile(&self, user_id: i64) -> Option<UserProfile> {
        let profiles = self.user_profiles.lock().await;
        profiles.get(&user_id).cloned()
    }

    pub async fn update_group_profile(&self, group_id: i64, profile: GroupProfile) -> Result<()> {
        {
            let mut profiles = self.group_profiles.lock().await;
            profiles.insert(group_id, profile);
        }
        self.save_memories().await
    }

    pub async fn get_group_profile(&self, group_id: i64) -> Option<GroupProfile> {
        let profiles = self.group_profiles.lock().await;
        profiles.get(&group_id).cloned()
    }

    pub async fn get_all_user_profiles(&self) -> Vec<UserProfile> {
        let profiles = self.user_profiles.lock().await;
        profiles.values().cloned().collect()
    }

    pub async fn get_all_group_profiles(&self) -> Vec<GroupProfile> {
        let profiles = self.group_profiles.lock().await;
        profiles.values().cloned().collect()
    }

    pub async fn update_bot_personality(&self, personality: BotPersonality) -> Result<()> {
        {
            let mut bot_personality = self.bot_personality.lock().await;
            *bot_personality = personality;
        }
        self.save_memories().await
    }

    pub async fn get_bot_personality(&self) -> BotPersonality {
        let bot_personality = self.bot_personality.lock().await;
        bot_personality.clone()
    }

    pub fn memory_file(&self) -> &str {
        &self.memory_file
    }

    /// 主动执行去重、过期清理和持久化，供后台维护任务调用。
    pub async fn compact_memories(&self) -> Result<()> {
        self.save_memories().await
    }

    pub async fn add_conversation_memory(
        &self,
        user_id: i64,
        content: &str,
        context: &str,
    ) -> Result<()> {
        let sequence = MEMORY_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let memory = MemoryEntry {
            id: format!(
                "conv_{}_{}_{}",
                user_id,
                Local::now().timestamp_micros(),
                sequence
            ),
            content: content.to_string(),
            timestamp: Local::now(),
            memory_type: MemoryType::Conversation,
            importance: self.calculate_importance(content),
            tags: self.extract_tags(content),
            context: context.to_string(),
            subject_id: Some(user_id),
        };
        self.add_memory(memory).await
    }

    pub async fn add_emotion_memory(&self, mood: &str, intensity: u8, context: &str) -> Result<()> {
        let sequence = MEMORY_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.add_memory(MemoryEntry {
            id: format!("emotion_{}_{}", Local::now().timestamp_micros(), sequence),
            content: format!("情绪变为 {}，强度 {}/10", mood, intensity.min(10)),
            timestamp: Local::now(),
            memory_type: MemoryType::Emotion,
            importance: intensity.clamp(1, 10),
            tags: vec!["情绪".to_string(), mood.to_string()],
            context: context.to_string(),
            subject_id: None,
        })
        .await
    }

    /// 计算记忆内容的重要性评分
    ///
    /// 使用多维度分析算法评估记忆的重要性，考虑以下因素：
    ///
    /// ## 关键词权重
    /// - **高重要性关键词** (+4分)：喜欢、讨厌、重要、秘密、梦想、目标、家人、朋友、爱、恨、害怕、担心
    /// - **中等重要性关键词** (+2分)：工作、学习、游戏、电影、音乐、食物、旅行、运动、健康
    /// - **低重要性关键词** (-1分)：天气、今天、昨天、明天、现在、刚才
    ///
    /// ## 内容特征
    /// - **长度权重**：>150字符(+2分)，>100字符(+1分)
    /// - **情感表达** (+2分)：开心、难过、生气、兴奋、害怕、担心、惊讶、失望
    /// - **个人信息** (+1分)：我、我的、自己、个人、私人的
    ///
    /// # 参数
    /// * `content` - 要分析的内容文本
    ///
    /// # 返回值
    /// 重要性评分 (0-10)，10表示最重要
    fn calculate_importance(&self, content: &str) -> u8 {
        let mut importance: u8 = 3; // 基础重要性

        // 检查关键词
        let high_importance_keywords = [
            "喜欢", "讨厌", "重要", "秘密", "梦想", "目标", "家人", "朋友", "爱", "恨", "害怕",
            "担心",
        ];
        let medium_importance_keywords = [
            "工作", "学习", "游戏", "电影", "音乐", "食物", "旅行", "运动", "健康",
        ];
        let low_importance_keywords = ["天气", "今天", "昨天", "明天", "现在", "刚才"];

        for keyword in &high_importance_keywords {
            if content.contains(keyword) {
                importance += 4;
            }
        }

        for keyword in &medium_importance_keywords {
            if content.contains(keyword) {
                importance += 2;
            }
        }

        for keyword in &low_importance_keywords {
            if content.contains(keyword) {
                importance = importance.saturating_sub(1);
            }
        }

        // 根据长度调整
        let character_count = content.chars().count();
        if character_count > 150 {
            importance += 2;
        } else if character_count > 100 {
            importance += 1;
        }

        // 检查是否包含情感表达
        let emotional_keywords = [
            "开心", "难过", "生气", "兴奋", "害怕", "担心", "惊讶", "失望",
        ];
        for keyword in &emotional_keywords {
            if content.contains(keyword) {
                importance += 2;
            }
        }

        // 检查是否包含个人信息
        let personal_keywords = ["我", "我的", "自己", "个人", "私人的"];
        for keyword in &personal_keywords {
            if content.contains(keyword) {
                importance += 1;
            }
        }

        importance.min(10)
    }

    fn extract_tags(&self, content: &str) -> Vec<String> {
        let mut tags = Vec::new();

        // 简单的关键词提取
        let common_tags = [
            "游戏", "学习", "工作", "生活", "情感", "技术", "科技", "娱乐", "美食", "旅行", "运动",
            "健康", "音乐", "电影", "读书", "家人", "朋友",
        ];
        for tag in &common_tags {
            if content.contains(tag) {
                tags.push(tag.to_string());
            }
        }

        tags
    }

    async fn save_memories(&self) -> Result<()> {
        let _save_guard = self.save_lock.lock().await;
        // 限制记忆数量，避免内存过度使用
        self.cleanup_old_memories().await?;

        let data = MemoryData {
            memories: self.memories.lock().await.clone(),
            user_profiles: self.user_profiles.lock().await.clone(),
            group_profiles: self.group_profiles.lock().await.clone(),
            bot_personality: self.bot_personality.lock().await.clone(),
        };

        let json = serde_json::to_string_pretty(&data)?;
        let memory_file = self.memory_file.clone();
        kovi::tokio::task::spawn_blocking(move || -> Result<()> {
            let temporary_file = format!("{}.tmp", memory_file);
            fs::write(&temporary_file, json)?;
            #[cfg(windows)]
            if std::path::Path::new(&memory_file).exists() {
                fs::remove_file(&memory_file)?;
            }
            fs::rename(&temporary_file, &memory_file)?;
            Ok(())
        })
        .await
        .map_err(|error| anyhow::anyhow!("记忆持久化任务失败: {}", error))??;
        Ok(())
    }

    /// 清理旧记忆，避免内存过度使用
    ///
    /// 执行以下清理策略：
    /// 1. 移除配置保留期之外的低重要性记忆（重要性 < 7）
    /// 2. 压缩同一对象、上下文和内容的重复记忆
    /// 3. 如果超过配置容量，只保留最重要且较新的记忆
    ///
    /// # 清理规则
    /// - 保留所有高重要性记忆（重要性 >= 7）
    /// - 保留期和最大数量均由 `bot.conf.toml` 的 `[memory]` 控制
    ///
    /// # 返回值
    /// 成功时返回 `Ok(())`，失败时返回错误信息
    async fn cleanup_old_memories(&self) -> Result<()> {
        let mut memories = self.memories.lock().await;
        let original_count = memories.len();
        let now = Local::now();
        let memory_config = crate::config::get().memory().clone();
        let retention_boundary = now - chrono::Duration::days(memory_config.retention_days());

        // 移除保留期之外的低重要性记忆。
        memories
            .retain(|_, memory| memory.timestamp > retention_boundary || memory.importance >= 7);

        // 对相同对象、上下文和内容的重复记忆去重，保留更新且更重要的一条。
        let mut entries: Vec<_> = memories.drain().collect();
        entries.sort_by(|left, right| {
            right
                .1
                .timestamp
                .cmp(&left.1.timestamp)
                .then_with(|| right.1.importance.cmp(&left.1.importance))
        });
        let mut seen = HashSet::new();
        entries.retain(|(_, memory)| {
            let normalized_content = memory
                .content
                .split_whitespace()
                .collect::<String>()
                .to_lowercase();
            seen.insert((
                memory.subject_id,
                memory.context.clone(),
                normalized_content,
            ))
        });

        // 超出容量时综合重要性与时间保留价值最高的记忆。
        if entries.len() > memory_config.max_entries() {
            entries.sort_by(|left, right| {
                right
                    .1
                    .importance
                    .cmp(&left.1.importance)
                    .then_with(|| right.1.timestamp.cmp(&left.1.timestamp))
            });
            entries.truncate(memory_config.max_entries());
        }
        *memories = entries.into_iter().collect();

        if memories.len() < original_count {
            println!(
                "[INFO] 记忆清理完成，移除 {} 条，当前保留 {} 条",
                original_count - memories.len(),
                memories.len()
            );
        }
        Ok(())
    }
}

fn context_scope(context: &str) -> Option<&'static str> {
    if context.contains("private") {
        Some("private")
    } else if context.contains("group") {
        Some("group")
    } else {
        None
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(default)]
struct MemoryData {
    memories: HashMap<String, MemoryEntry>,
    user_profiles: HashMap<i64, UserProfile>,
    group_profiles: HashMap<i64, GroupProfile>,
    bot_personality: BotPersonality,
}

impl Default for BotPersonality {
    fn default() -> Self {
        Self {
            current_mood: "neutral".to_string(),
            mood_intensity: 5,
            energy_level: 7,
            social_confidence: 6,
            curiosity_level: 8,
            last_mood_change: Local::now(),
            personality_traits: vec![
                "curious".to_string(),
                "playful".to_string(),
                "empathetic".to_string(),
                "slightly_tsundere".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryManager, UserProfile};
    use chrono::Local;

    fn temporary_memory_path(test_name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kovi-bot-{}-{}-{}.json",
            test_name,
            std::process::id(),
            Local::now().timestamp_micros(),
        ))
    }

    #[test]
    fn profile_updates_persist_without_deadlock() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("profile");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let profile = UserProfile {
                    user_id: 42,
                    nickname: "tester".to_string(),
                    personality_traits: Vec::new(),
                    interests: vec!["Rust".to_string()],
                    relationship_level: 3,
                    last_interaction: Local::now(),
                    interaction_count: 2,
                    last_private_interaction: Some(Local::now()),
                    mood_history: Vec::new(),
                };

                manager
                    .update_user_profile(42, profile)
                    .await
                    .expect("档案应成功持久化");

                let reloaded = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));
                let saved = reloaded.get_user_profile(42).await.expect("应读回用户档案");
                assert_eq!(saved.nickname, "tester");
                assert_eq!(saved.interests, vec!["Rust"]);

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn contextual_memories_are_isolated_by_subject() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("isolation");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));

                manager
                    .add_conversation_memory(1, "用户一的秘密", "private_chat")
                    .await
                    .expect("应写入用户一记忆");
                manager
                    .add_conversation_memory(2, "用户二的秘密", "private_chat")
                    .await
                    .expect("应写入用户二记忆");

                let user_one = manager.get_contextual_memories(1, "private_chat", 10).await;
                assert_eq!(user_one.len(), 1);
                assert_eq!(user_one[0].subject_id, Some(1));
                assert_eq!(manager.get_recent_memories(0).await.len(), 2);

                manager
                    .add_conversation_memory(1, "同号群聊内容", "group_chat")
                    .await
                    .expect("应写入同号群聊记忆");
                let private_context = manager.get_contextual_memories(1, "private_chat", 10).await;
                assert_eq!(private_context.len(), 1);
                assert!(!private_context[0].content.contains("群聊"));

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn duplicate_memories_are_compressed_and_search_requires_a_match() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("compression");
                let manager = MemoryManager::new(path.to_str().expect("临时路径应为 UTF-8"));

                manager
                    .add_conversation_memory(7, "我喜欢音乐", "private_chat")
                    .await
                    .expect("应写入记忆");
                manager
                    .add_conversation_memory(7, "我 喜欢 音乐", "private_chat")
                    .await
                    .expect("应压缩空白不同的重复记忆");

                assert_eq!(manager.get_recent_memories(0).await.len(), 1);
                assert_eq!(manager.search_memories("音乐").await.len(), 1);
                assert!(manager.search_memories("量子计算").await.is_empty());

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn legacy_profile_defaults_private_interaction() {
        let json = r#"{
            "user_id": 9,
            "nickname": "legacy",
            "personality_traits": [],
            "interests": [],
            "relationship_level": 1,
            "last_interaction": "2026-08-19T22:00:00+08:00",
            "interaction_count": 1,
            "mood_history": []
        }"#;
        let profile: UserProfile = serde_json::from_str(json).expect("旧档案应能迁移");
        assert!(profile.last_private_interaction.is_none());
    }
}
