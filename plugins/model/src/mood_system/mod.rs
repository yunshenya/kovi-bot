//! # 情绪系统模块
//!
//! 提供智能的情绪分析和人格调整功能，包括：
//! - 多维度情绪识别和分析
//! - 基于关键词的情绪评分算法
//! - 上下文感知的情绪调整
//! - 自然情绪变化和漂移
//! - 情绪缓存和性能优化
//! - 人格特征动态调整

use crate::memory::{BotPersonality, MemoryManager};
use anyhow::Result;
use chrono::{Duration, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone)]
struct CachedMood {
    mood: Mood,
    analyzed_at: chrono::DateTime<Local>,
    applied_to_personality: bool,
}

type MoodCache = Arc<Mutex<HashMap<String, CachedMood>>>;

/// 情绪状态枚举
///
/// 定义机器人可能的各种情绪状态，用于人格化和个性化交互
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub enum Mood {
    /// 开心：积极正面的情绪状态
    Happy,
    /// 难过：消极悲伤的情绪状态
    Sad,
    /// 生气：愤怒不满的情绪状态
    Angry,
    /// 兴奋：高度活跃的情绪状态
    Excited,
    /// 平静：稳定平和的情绪状态
    Calm,
    /// 好奇：探索求知的情绪状态
    Curious,
    /// 顽皮：活泼调皮的情绪状态
    Playful,
    /// 深思：理性思考的情绪状态
    Thoughtful,
    /// 孤独：缺乏陪伴的情绪状态
    Lonely,
    /// 自信：确信肯定的情绪状态
    Confident,
    /// 害羞：内向拘谨的情绪状态
    Shy,
    /// 中性：平衡稳定的情绪状态
    Neutral,
}

impl fmt::Display for Mood {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Mood::Happy => "happy",
            Mood::Sad => "sad",
            Mood::Angry => "angry",
            Mood::Excited => "excited",
            Mood::Calm => "calm",
            Mood::Curious => "curious",
            Mood::Playful => "playful",
            Mood::Thoughtful => "thoughtful",
            Mood::Lonely => "lonely",
            Mood::Confident => "confident",
            Mood::Shy => "shy",
            Mood::Neutral => "neutral",
        };
        formatter.write_str(value)
    }
}

impl Mood {
    pub fn from_string(s: &str) -> Self {
        match s {
            "happy" => Mood::Happy,
            "sad" => Mood::Sad,
            "angry" => Mood::Angry,
            "excited" => Mood::Excited,
            "calm" => Mood::Calm,
            "curious" => Mood::Curious,
            "playful" => Mood::Playful,
            "thoughtful" => Mood::Thoughtful,
            "lonely" => Mood::Lonely,
            "confident" => Mood::Confident,
            "shy" => Mood::Shy,
            _ => Mood::Neutral,
        }
    }
}

/// 情绪系统结构体
///
/// 负责分析用户消息的情绪并调整机器人的人格状态
/// 包含情绪缓存机制以提高性能
pub struct MoodSystem {
    /// 记忆管理器引用，用于获取和更新机器人人格
    memory_manager: Arc<MemoryManager>,
    /// 情绪分析缓存，避免重复计算相同消息的情绪
    mood_cache: MoodCache,
}

impl MoodSystem {
    /// 创建新的情绪系统实例
    ///
    /// # 参数
    /// * `memory_manager` - 记忆管理器实例
    ///
    /// # 返回值
    /// 初始化的MoodSystem实例
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            memory_manager,
            mood_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 分析消息情绪并更新机器人人格
    ///
    /// 这是情绪系统的核心函数，执行以下步骤：
    /// 1. 检查情绪分析缓存（默认5分钟内有效，可配置）
    /// 2. 分析消息内容确定情绪
    /// 3. 更新缓存并清理过期数据
    /// 4. 调整机器人人格属性
    /// 5. 保存更新后的人格状态
    ///
    /// # 参数
    /// * `message` - 要分析的消息内容
    /// * `context` - 消息上下文（如"group_chat"、"private_chat"）
    ///
    /// # 返回值
    /// 成功时返回分析出的情绪状态，失败时返回错误
    pub async fn analyze_and_update_mood(&self, message: &str, context: &str) -> Result<Mood> {
        // 检查缓存
        let cache_key = format!("{}:{}", message, context);
        let now = Local::now();
        let mood_config = crate::config::get().mood().clone();

        let cached_mood = {
            let cache = self
                .mood_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.get(&cache_key).and_then(|cached| {
                (now.signed_duration_since(cached.analyzed_at)
                    < Duration::seconds(mood_config.cache_ttl_secs() as i64))
                .then(|| (cached.mood.clone(), cached.applied_to_personality))
            })
        };
        if let Some((mood, true)) = cached_mood.as_ref() {
            return Ok(mood.clone());
        }

        let current_personality = self.memory_manager.get_bot_personality().await;
        let new_mood = if let Some((mood, false)) = cached_mood {
            mood
        } else {
            self.analyze_mood_from_message(message, context, &current_personality)
                .await
        };

        // 更新缓存
        {
            let mut cache = self
                .mood_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.insert(
                cache_key,
                CachedMood {
                    mood: new_mood.clone(),
                    analyzed_at: now,
                    applied_to_personality: true,
                },
            );

            // 清理过期缓存
            cache.retain(|_, cached| {
                now.signed_duration_since(cached.analyzed_at)
                    < Duration::seconds(mood_config.cache_retention_secs() as i64)
            });
        }

        // 更新机器人人格
        let mut updated_personality = current_personality;
        updated_personality.current_mood = new_mood.to_string();
        updated_personality.mood_intensity = self.mood_intensity(message, &new_mood);
        updated_personality.last_mood_change = now;

        // 根据情绪调整其他属性
        self.adjust_personality_traits(&mut updated_personality, &new_mood);

        let intensity = updated_personality.mood_intensity;
        self.memory_manager
            .update_bot_personality(updated_personality)
            .await?;
        self.memory_manager
            .add_emotion_memory(&new_mood.to_string(), intensity, context)
            .await?;

        Ok(new_mood)
    }

    /// 只分析消息情绪，不改变机器人人格。用于记录用户自己的情绪历史。
    pub async fn analyze_mood(&self, message: &str, context: &str) -> Mood {
        let cache_key = format!("{}:{}", message, context);
        let now = Local::now();
        let mood_config = crate::config::get().mood().clone();
        if let Some(mood) = self
            .mood_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&cache_key)
            .filter(|cached| {
                now.signed_duration_since(cached.analyzed_at)
                    < Duration::seconds(mood_config.cache_ttl_secs() as i64)
            })
            .map(|cached| cached.mood.clone())
        {
            return mood;
        }

        let personality = self.memory_manager.get_bot_personality().await;
        let mood = self
            .analyze_mood_from_message(message, context, &personality)
            .await;
        let mut cache = self
            .mood_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.insert(
            cache_key,
            CachedMood {
                mood: mood.clone(),
                analyzed_at: now,
                applied_to_personality: false,
            },
        );
        cache.retain(|_, cached| {
            now.signed_duration_since(cached.analyzed_at)
                < Duration::seconds(mood_config.cache_retention_secs() as i64)
        });
        mood
    }

    async fn analyze_mood_from_message(
        &self,
        message: &str,
        context: &str,
        current_personality: &BotPersonality,
    ) -> Mood {
        let message_lower = message.to_lowercase();

        // 情绪关键词分析
        let mood_scores = self.calculate_mood_scores(&message_lower);

        // 上下文分析
        let context_mood = self.analyze_context_mood(context);

        // 结合当前情绪状态
        self.combine_mood_analysis(mood_scores, context_mood, current_personality)
    }

    /// 计算消息的情绪得分
    ///
    /// 使用关键词匹配算法分析消息内容，为每种情绪计算得分
    ///
    /// ## 评分规则
    /// - **高权重关键词** (+2分)：开心、难过、生气、兴奋、孤独等强烈情绪
    /// - **中权重关键词** (+1分)：好奇、顽皮、深思、自信、害羞等温和情绪
    ///
    /// ## 关键词分类
    /// - **开心**：开心、高兴、快乐、哈哈、😊、😄、好棒、太好了、喜欢
    /// - **难过**：难过、伤心、哭、😢、😭、糟糕、不好、讨厌
    /// - **生气**：生气、愤怒、讨厌、烦、😠、😡、气死
    /// - **兴奋**：兴奋、激动、太棒了、哇、！、!!!、😆、😃
    /// - **好奇**：什么、为什么、怎么、？、???、好奇、想知道
    /// - **顽皮**：调皮、顽皮、哈哈、嘿嘿、😏、😜、开玩笑
    /// - **深思**：思考、想想、觉得、认为、可能、也许
    /// - **孤独**：一个人、孤单、寂寞、没人、只有我
    /// - **自信**：肯定、一定、当然、没问题、我可以、我能
    /// - **害羞**：害羞、不好意思、脸红、😳、尴尬
    ///
    /// # 参数
    /// * `message` - 要分析的消息内容
    ///
    /// # 返回值
    /// 各种情绪及其得分的映射表
    fn calculate_mood_scores(&self, message: &str) -> std::collections::HashMap<Mood, i32> {
        let mut scores = std::collections::HashMap::new();

        // 初始化所有情绪分数
        for mood in [
            Mood::Happy,
            Mood::Sad,
            Mood::Angry,
            Mood::Excited,
            Mood::Calm,
            Mood::Curious,
            Mood::Playful,
            Mood::Thoughtful,
            Mood::Lonely,
            Mood::Confident,
            Mood::Shy,
            Mood::Neutral,
        ] {
            scores.insert(mood, 0);
        }

        // 开心关键词
        let happy_keywords = [
            "开心",
            "高兴",
            "快乐",
            "哈哈",
            "😊",
            "😄",
            "好棒",
            "太好了",
            "喜欢",
            "happy",
            "glad",
            "awesome",
            "love",
        ];
        for keyword in &happy_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Happy).unwrap() += 2;
            }
        }

        // 难过关键词
        let sad_keywords = [
            "难过", "伤心", "哭", "😢", "😭", "糟糕", "不好", "讨厌", "sad", "upset", "cry",
        ];
        for keyword in &sad_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Sad).unwrap() += 2;
            }
        }

        // 生气关键词
        let angry_keywords = [
            "生气", "愤怒", "讨厌", "烦", "😠", "😡", "气死", "angry", "hate", "annoyed",
        ];
        for keyword in &angry_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Angry).unwrap() += 2;
            }
        }

        // 兴奋关键词
        let excited_keywords = [
            "兴奋",
            "激动",
            "太棒了",
            "哇",
            "！",
            "!!!",
            "😆",
            "😃",
            "excited",
            "amazing",
            "wow",
        ];
        for keyword in &excited_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Excited).unwrap() += 2;
            }
        }

        // 好奇关键词
        let curious_keywords = [
            "什么",
            "为什么",
            "怎么",
            "？",
            "???",
            "好奇",
            "想知道",
            "what",
            "why",
            "how",
            "curious",
        ];
        for keyword in &curious_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Curious).unwrap() += 1;
            }
        }

        // 顽皮关键词
        let playful_keywords = [
            "调皮",
            "顽皮",
            "哈哈",
            "嘿嘿",
            "😏",
            "😜",
            "开玩笑",
            "joke",
            "funny",
        ];
        for keyword in &playful_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Playful).unwrap() += 1;
            }
        }

        // 深思关键词
        let thoughtful_keywords = [
            "思考", "想想", "觉得", "认为", "可能", "也许", "think", "maybe", "perhaps",
        ];
        for keyword in &thoughtful_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Thoughtful).unwrap() += 1;
            }
        }

        // 孤独关键词
        let lonely_keywords = [
            "一个人",
            "孤单",
            "寂寞",
            "没人",
            "只有我",
            "lonely",
            "alone",
        ];
        for keyword in &lonely_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Lonely).unwrap() += 2;
            }
        }

        // 自信关键词
        let confident_keywords = [
            "肯定",
            "一定",
            "当然",
            "没问题",
            "我可以",
            "我能",
            "sure",
            "definitely",
            "i can",
        ];
        for keyword in &confident_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Confident).unwrap() += 1;
            }
        }

        // 害羞关键词
        let shy_keywords = [
            "害羞",
            "不好意思",
            "脸红",
            "😳",
            "尴尬",
            "shy",
            "embarrassed",
        ];
        for keyword in &shy_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Shy).unwrap() += 1;
            }
        }

        let calm_keywords = [
            "平静", "冷静", "放松", "安心", "calm", "relaxed", "peaceful",
        ];
        for keyword in &calm_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Calm).unwrap() += 1;
            }
        }

        scores
    }

    fn analyze_context_mood(&self, context: &str) -> Option<Mood> {
        let context_lower = context.to_lowercase();

        if context_lower.contains("群聊") || context_lower.contains("group") {
            Some(Mood::Playful) // 群聊时更顽皮
        } else if context_lower.contains("私聊") || context_lower.contains("private") {
            Some(Mood::Thoughtful) // 私聊时更深思
        } else if context_lower.contains("深夜") {
            Some(Mood::Calm) // 深夜更平静
        } else {
            None
        }
    }

    fn combine_mood_analysis(
        &self,
        mood_scores: std::collections::HashMap<Mood, i32>,
        context_mood: Option<Mood>,
        current_personality: &BotPersonality,
    ) -> Mood {
        // 找到得分最高的情绪
        let mut best_mood = Mood::Neutral;
        let mut best_score = 0;

        for (mood, score) in &mood_scores {
            if *score > best_score {
                best_score = *score;
                best_mood = mood.clone();
            }
        }

        // 如果上下文有特殊情绪，给予额外权重
        if let Some(context_mood) = context_mood
            && let Some(context_score) = mood_scores.get(&context_mood)
            && *context_score > 0
        {
            best_mood = context_mood;
        }

        // 如果所有情绪得分都很低，保持当前情绪或转为中性
        if best_score == 0 {
            let current_mood = Mood::from_string(&current_personality.current_mood);
            return if current_personality.energy_level > 5 {
                current_mood
            } else {
                Mood::Neutral
            };
        }

        best_mood
    }

    fn adjust_personality_traits(&self, personality: &mut BotPersonality, mood: &Mood) {
        match mood {
            Mood::Happy | Mood::Excited => {
                personality.energy_level = (personality.energy_level + 1).min(10);
                personality.social_confidence = (personality.social_confidence + 1).min(10);
            }
            Mood::Sad | Mood::Lonely => {
                personality.energy_level = personality.energy_level.saturating_sub(1);
                personality.social_confidence = personality.social_confidence.saturating_sub(1);
            }
            Mood::Angry => {
                personality.energy_level = (personality.energy_level + 1).min(10);
                personality.social_confidence = personality.social_confidence.saturating_sub(1);
            }
            Mood::Calm | Mood::Thoughtful => {
                personality.energy_level = personality.energy_level.saturating_sub(1);
                personality.curiosity_level = (personality.curiosity_level + 1).min(10);
            }
            Mood::Curious => {
                personality.curiosity_level = (personality.curiosity_level + 2).min(10);
            }
            Mood::Playful => {
                personality.energy_level = (personality.energy_level + 1).min(10);
                personality.social_confidence = (personality.social_confidence + 1).min(10);
            }
            Mood::Confident => {
                personality.social_confidence = (personality.social_confidence + 2).min(10);
            }
            Mood::Shy => {
                personality.social_confidence = personality.social_confidence.saturating_sub(2);
            }
            _ => {}
        }
    }

    fn mood_intensity(&self, message: &str, mood: &Mood) -> u8 {
        let scores = self.calculate_mood_scores(&message.to_lowercase());
        let score = scores.get(mood).copied().unwrap_or_default().max(0) as u8;
        if *mood == Mood::Neutral {
            3
        } else {
            (4 + score).min(10)
        }
    }

    pub async fn get_mood_based_response_style(&self) -> String {
        let personality = self.memory_manager.get_bot_personality().await;
        let mood = Mood::from_string(&personality.current_mood);

        match mood {
            Mood::Happy => "开心地 😊".to_string(),
            Mood::Sad => "有点难过地 😢".to_string(),
            Mood::Angry => "有点生气地 😠".to_string(),
            Mood::Excited => "兴奋地 ✨".to_string(),
            Mood::Calm => "平静地 🌙".to_string(),
            Mood::Curious => "好奇地 🤔".to_string(),
            Mood::Playful => "顽皮地 😜".to_string(),
            Mood::Thoughtful => "深思地 💭".to_string(),
            Mood::Lonely => "有点孤单地 🥺".to_string(),
            Mood::Confident => "自信地 😎".to_string(),
            Mood::Shy => "害羞地 😳".to_string(),
            Mood::Neutral => "".to_string(),
        }
    }

    pub async fn should_change_mood_naturally(&self) -> bool {
        let personality = self.memory_manager.get_bot_personality().await;
        let now = Local::now();
        let time_since_last_change = now.signed_duration_since(personality.last_mood_change);

        let drift_after_secs = crate::config::get().mood().natural_drift_after_secs();
        time_since_last_change > Duration::seconds(drift_after_secs as i64)
    }

    pub async fn natural_mood_drift(&self) -> Result<()> {
        if !self.should_change_mood_naturally().await {
            return Ok(());
        }

        let mut personality = self.memory_manager.get_bot_personality().await;

        // 根据当前时间和能量水平自然调整情绪
        let hour = Local::now().hour();
        let new_mood = match hour {
            6..=11 => Mood::Happy,     // 早晨开心
            12..=14 => Mood::Excited,  // 中午兴奋
            15..=17 => Mood::Curious,  // 下午好奇
            18..=20 => Mood::Playful,  // 傍晚顽皮
            21..=23 => Mood::Calm,     // 晚上平静
            0..=5 => Mood::Thoughtful, // 深夜深思
            _ => Mood::Neutral,
        };

        personality.current_mood = new_mood.to_string();
        personality.mood_intensity = 4;
        personality.last_mood_change = Local::now();

        self.memory_manager
            .update_bot_personality(personality)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Mood, MoodSystem};
    use crate::memory::{MemoryManager, MemoryType};
    use chrono::Local;
    use std::sync::Arc;

    fn temporary_memory_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kovi-mood-{}-{}.json",
            std::process::id(),
            Local::now().timestamp_micros()
        ))
    }

    #[test]
    fn detected_mood_updates_personality_and_history() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path();
                let manager = Arc::new(MemoryManager::new(
                    path.to_str().expect("临时路径应为 UTF-8"),
                ));
                let mood_system = MoodSystem::new(Arc::clone(&manager));

                assert_eq!(
                    mood_system
                        .analyze_mood("哈哈，今天太开心了！", "private_chat")
                        .await,
                    Mood::Happy
                );
                let mood = mood_system
                    .analyze_and_update_mood("哈哈，今天太开心了！", "private_chat")
                    .await
                    .expect("情绪应更新");
                assert_eq!(mood, Mood::Happy);
                let personality = manager.get_bot_personality().await;
                assert_eq!(personality.current_mood, "happy");
                assert!(personality.mood_intensity > 4);
                assert_eq!(
                    manager
                        .get_memories_by_type(&MemoryType::Emotion)
                        .await
                        .len(),
                    1
                );
                mood_system
                    .analyze_and_update_mood("哈哈，今天太开心了！", "private_chat")
                    .await
                    .expect("缓存命中仍应返回情绪");
                assert_eq!(
                    manager
                        .get_memories_by_type(&MemoryType::Emotion)
                        .await
                        .len(),
                    1,
                    "五分钟内的相同分析不应重复写入情绪记忆"
                );
                assert_eq!(
                    mood_system
                        .analyze_mood("I feel calm and relaxed", "private_chat")
                        .await,
                    Mood::Calm
                );

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }
}
