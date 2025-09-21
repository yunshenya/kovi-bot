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
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};

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
        let manager = Self {
            memories: Arc::new(Mutex::new(HashMap::new())),
            user_profiles: Arc::new(Mutex::new(HashMap::new())),
            group_profiles: Arc::new(Mutex::new(HashMap::new())),
            bot_personality: Arc::new(Mutex::new(BotPersonality {
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
            })),
            memory_file: memory_file.to_string(),
        };
        
        // 尝试加载现有记忆
        let manager_clone = manager.clone();
        kovi::tokio::spawn(async move {
            if let Err(e) = manager_clone.load_memories().await {
                eprintln!("Failed to load memories: {}", e);
            }
        });
        
        manager
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
            .filter(|m| std::mem::discriminant(&m.memory_type) == std::mem::discriminant(memory_type))
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
        memories.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        memories.truncate(limit);
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
        let query_lower = query.to_lowercase();
        
        let mut results: Vec<(MemoryEntry, u8)> = memories
            .values()
            .map(|m| {
                let mut score = 0u8;
                let content_lower = m.content.to_lowercase();
                
                // 完全匹配得分最高
                if content_lower.contains(&query_lower) {
                    score += 10;
                }
                
                // 标签匹配
                for tag in &m.tags {
                    if tag.to_lowercase().contains(&query_lower) {
                        score += 5;
                    }
                }
                
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
                
                (m.clone(), score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();
        
        // 按得分排序
        results.sort_by(|a, b| b.1.cmp(&a.1));
        
        results.into_iter().map(|(memory, _)| memory).collect()
    }

    pub async fn get_contextual_memories(&self, user_id: i64, context: &str, limit: usize) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        let mut contextual_memories: Vec<(MemoryEntry, u8)> = Vec::new();
        
        for memory in memories.values() {
            let mut relevance_score = 0u8;
            
            // 检查是否与用户相关
            if memory.content.contains(&format!("{}", user_id)) {
                relevance_score += 5;
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
        contextual_memories.sort_by(|a, b| b.1.cmp(&a.1));
        contextual_memories.truncate(limit);
        
        contextual_memories.into_iter().map(|(memory, _)| memory).collect()
    }

    pub async fn update_user_profile(&self, user_id: i64, profile: UserProfile) -> Result<()> {
        let mut profiles = self.user_profiles.lock().await;
        profiles.insert(user_id, profile);
        self.save_memories().await
    }

    pub async fn get_user_profile(&self, user_id: i64) -> Option<UserProfile> {
        let profiles = self.user_profiles.lock().await;
        profiles.get(&user_id).cloned()
    }

    pub async fn update_group_profile(&self, group_id: i64, profile: GroupProfile) -> Result<()> {
        let mut profiles = self.group_profiles.lock().await;
        profiles.insert(group_id, profile);
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

    pub async fn add_conversation_memory(&self, user_id: i64, content: &str, context: &str) -> Result<()> {
        let memory = MemoryEntry {
            id: format!("conv_{}_{}", user_id, Local::now().timestamp_millis()),
            content: content.to_string(),
            timestamp: Local::now(),
            memory_type: MemoryType::Conversation,
            importance: self.calculate_importance(content),
            tags: self.extract_tags(content),
            context: context.to_string(),
        };
        self.add_memory(memory).await
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
        let high_importance_keywords = ["喜欢", "讨厌", "重要", "秘密", "梦想", "目标", "家人", "朋友", "爱", "恨", "害怕", "担心"];
        let medium_importance_keywords = ["工作", "学习", "游戏", "电影", "音乐", "食物", "旅行", "运动", "健康"];
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
        if content.len() > 150 {
            importance += 2;
        } else if content.len() > 100 {
            importance += 1;
        }
        
        // 检查是否包含情感表达
        let emotional_keywords = ["开心", "难过", "生气", "兴奋", "害怕", "担心", "惊讶", "失望"];
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
        let common_tags = ["游戏", "学习", "工作", "生活", "情感", "技术", "娱乐", "美食", "旅行"];
        for tag in &common_tags {
            if content.contains(tag) {
                tags.push(tag.to_string());
            }
        }
        
        tags
    }

    async fn load_memories(&self) -> Result<()> {
        if !Path::new(&self.memory_file).exists() {
            return Ok(());
        }

        let data = fs::read_to_string(&self.memory_file)?;
        let data: MemoryData = serde_json::from_str(&data)?;
        
        {
            let mut memories = self.memories.lock().await;
            *memories = data.memories;
        }
        
        {
            let mut user_profiles = self.user_profiles.lock().await;
            *user_profiles = data.user_profiles;
        }
        
        {
            let mut group_profiles = self.group_profiles.lock().await;
            *group_profiles = data.group_profiles;
        }
        
        {
            let mut bot_personality = self.bot_personality.lock().await;
            *bot_personality = data.bot_personality;
        }

        Ok(())
    }

    async fn save_memories(&self) -> Result<()> {
        // 限制记忆数量，避免内存过度使用
        self.cleanup_old_memories().await?;
        
        let data = MemoryData {
            memories: self.memories.lock().await.clone(),
            user_profiles: self.user_profiles.lock().await.clone(),
            group_profiles: self.group_profiles.lock().await.clone(),
            bot_personality: self.bot_personality.lock().await.clone(),
        };

        let json = serde_json::to_string_pretty(&data)?;
        fs::write(&self.memory_file, json)?;
        Ok(())
    }

    /// 清理旧记忆，避免内存过度使用
    /// 
    /// 执行以下清理策略：
    /// 1. 移除30天前的低重要性记忆（重要性 < 7）
    /// 2. 如果记忆数量超过1000条，只保留最重要的记忆
    /// 
    /// # 清理规则
    /// - 保留所有高重要性记忆（重要性 >= 7）
    /// - 移除30天前的低重要性记忆
    /// - 限制总记忆数量不超过1000条
    /// 
    /// # 返回值
    /// 成功时返回 `Ok(())`，失败时返回错误信息
    async fn cleanup_old_memories(&self) -> Result<()> {
        let mut memories = self.memories.lock().await;
        let now = Local::now();
        let thirty_days_ago = now - chrono::Duration::days(30);
        
        // 移除30天前的低重要性记忆
        memories.retain(|_, memory| {
            memory.timestamp > thirty_days_ago || memory.importance >= 7
        });
        
        // 如果记忆数量仍然过多，只保留最重要的
        if memories.len() > 1000 {
            let mut memory_vec: Vec<_> = memories.drain().collect();
            memory_vec.sort_by(|a, b| b.1.importance.cmp(&a.1.importance));
            memory_vec.truncate(1000);
            *memories = memory_vec.into_iter().collect();
        }
        
        println!("[INFO] 记忆清理完成，当前记忆数量: {}", memories.len());
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryData {
    memories: HashMap<String, MemoryEntry>,
    user_profiles: HashMap<i64, UserProfile>,
    group_profiles: HashMap<i64, GroupProfile>,
    bot_personality: BotPersonality,
}