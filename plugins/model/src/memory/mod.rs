use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use kovi::tokio::sync::Mutex;
use chrono::{DateTime, Local};
use anyhow::Result;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub timestamp: DateTime<Local>,
    pub memory_type: MemoryType,
    pub importance: u8, // 0-10, 10最重要
    pub tags: Vec<String>,
    pub context: String, // 上下文信息
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum MemoryType {
    Conversation, // 对话记忆
    UserProfile,  // 用户档案
    GroupInfo,    // 群组信息
    Event,        // 事件记忆
    Preference,   // 偏好设置
    Emotion,      // 情绪状态
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserProfile {
    pub user_id: i64,
    pub nickname: String,
    pub personality_traits: Vec<String>,
    pub interests: Vec<String>,
    pub relationship_level: u8, // 0-10, 关系亲密度
    pub last_interaction: DateTime<Local>,
    pub interaction_count: u32,
    pub mood_history: Vec<MoodEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MoodEntry {
    pub mood: String,
    pub intensity: u8, // 0-10
    pub timestamp: DateTime<Local>,
    pub trigger: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GroupProfile {
    pub group_id: i64,
    pub group_name: String,
    pub active_members: Vec<i64>,
    pub group_personality: String,
    pub conversation_topics: Vec<String>,
    pub last_activity: DateTime<Local>,
    pub activity_level: u8, // 0-10
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BotPersonality {
    pub current_mood: String,
    pub mood_intensity: u8,
    pub energy_level: u8,
    pub social_confidence: u8,
    pub curiosity_level: u8,
    pub last_mood_change: DateTime<Local>,
    pub personality_traits: Vec<String>,
}

#[derive(Clone)]
pub struct MemoryManager {
    memories: Arc<Mutex<HashMap<String, MemoryEntry>>>,
    user_profiles: Arc<Mutex<HashMap<i64, UserProfile>>>,
    group_profiles: Arc<Mutex<HashMap<i64, GroupProfile>>>,
    bot_personality: Arc<Mutex<BotPersonality>>,
    memory_file: String,
}

impl MemoryManager {
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

    pub async fn add_memory(&self, memory: MemoryEntry) -> Result<()> {
        let mut memories = self.memories.lock().await;
        memories.insert(memory.id.clone(), memory);
        self.save_memories().await
    }

    pub async fn get_memories_by_type(&self, memory_type: &MemoryType) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| std::mem::discriminant(&m.memory_type) == std::mem::discriminant(memory_type))
            .cloned()
            .collect()
    }

    pub async fn get_recent_memories(&self, limit: usize) -> Vec<MemoryEntry> {
        let mut memories: Vec<MemoryEntry> = self.memories.lock().await.values().cloned().collect();
        memories.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        memories.truncate(limit);
        memories
    }

    pub async fn get_important_memories(&self, min_importance: u8) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| m.importance >= min_importance)
            .cloned()
            .collect()
    }

    pub async fn search_memories(&self, query: &str) -> Vec<MemoryEntry> {
        let memories = self.memories.lock().await;
        memories
            .values()
            .filter(|m| m.content.to_lowercase().contains(&query.to_lowercase()) ||
                       m.tags.iter().any(|tag| tag.to_lowercase().contains(&query.to_lowercase())))
            .cloned()
            .collect()
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

    pub async fn update_bot_personality(&self, personality: BotPersonality) -> Result<()> {
        let mut bot_personality = self.bot_personality.lock().await;
        *bot_personality = personality;
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

    fn calculate_importance(&self, content: &str) -> u8 {
        let mut importance = 3; // 基础重要性
        
        // 检查关键词
        let high_importance_keywords = ["喜欢", "讨厌", "重要", "秘密", "梦想", "目标", "家人", "朋友"];
        let medium_importance_keywords = ["工作", "学习", "游戏", "电影", "音乐", "食物"];
        
        for keyword in &high_importance_keywords {
            if content.contains(keyword) {
                importance += 3;
            }
        }
        
        for keyword in &medium_importance_keywords {
            if content.contains(keyword) {
                importance += 1;
            }
        }
        
        // 根据长度调整
        if content.len() > 100 {
            importance += 1;
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
}

#[derive(Debug, Serialize, Deserialize)]
struct MemoryData {
    memories: HashMap<String, MemoryEntry>,
    user_profiles: HashMap<i64, UserProfile>,
    group_profiles: HashMap<i64, GroupProfile>,
    bot_personality: BotPersonality,
}