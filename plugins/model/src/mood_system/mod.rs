//! # 情绪系统模块
//!
//! 提供智能的情绪分析和人格调整功能，包括：
//! - 多维度情绪识别和分析
//! - 基于模型语义理解的情绪估计
//! - 上下文感知的情绪调整
//! - 自然情绪变化和漂移
//! - 情绪缓存和性能优化
//! - 人格特征动态调整

use crate::memory::{BotPersonality, MEMORY_MANAGER, MemoryManager};
use crate::model::semantic::{MessageUnderstanding, UnderstandingRequest, understand};
use anyhow::Result;
use chrono::{Duration, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;

const MAX_MOOD_CACHE_ENTRIES: usize = 2_048;
const MAX_APPLIED_SUBJECTS_PER_ENTRY: usize = 64;

/// 所有聊天与后台漂移共享同一份情绪缓存和人格更新入口。
pub static MOOD_SYSTEM: LazyLock<MoodSystem> =
    LazyLock::new(|| MoodSystem::new(Arc::clone(&MEMORY_MANAGER)));

#[derive(Clone)]
struct CachedMood {
    mood: Mood,
    analyzed_at: chrono::DateTime<Local>,
    /// 已将该分析作用到人格的会话对象。不能只存一个 bool，否则另一个用户发送
    /// 相同文本时会被误认为已经处理。
    applied_subjects: HashSet<String>,
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
    /// 人格是全局状态，所有情绪读改写必须串行，防止并发消息互相覆盖。
    update_lock: Arc<kovi::tokio::sync::Mutex<()>>,
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
            update_lock: Arc::new(kovi::tokio::sync::Mutex::new(())),
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
        let understanding = understand(UnderstandingRequest::text(message, context)).await;
        self.analyze_and_update_mood_with_understanding(message, context, &understanding)
            .await
    }

    pub(crate) async fn analyze_and_update_mood_with_understanding(
        &self,
        message: &str,
        context: &str,
        understanding: &MessageUnderstanding,
    ) -> Result<Mood> {
        self.analyze_and_update_mood_for_subject_with_understanding(
            message,
            context,
            None,
            understanding,
        )
        .await
    }

    /// 分析情绪并按实际发送者去重人格更新。同一条消息在同一会话对象内的重复处理
    /// 只应用一次，但不同用户发送相同文本不会共享“已应用”状态。
    pub(crate) async fn analyze_and_update_mood_for_subject_with_understanding(
        &self,
        message: &str,
        context: &str,
        subject_id: Option<i64>,
        understanding: &MessageUnderstanding,
    ) -> Result<Mood> {
        let _update_guard = self.update_lock.lock().await;
        let cache_key = mood_cache_key(message, context);
        let subject_key = subject_id.map(|subject_id| format!("{}:{}", context, subject_id));
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
                .then(|| {
                    let applied = subject_key
                        .as_ref()
                        .is_some_and(|subject| cached.applied_subjects.contains(subject));
                    (cached.mood.clone(), applied)
                })
            })
        };
        if let Some((mood, true)) = cached_mood.as_ref() {
            return Ok(mood.clone());
        }

        let current_personality = self.memory_manager.get_bot_personality().await;
        let (new_mood, new_intensity) = if let Some((mood, false)) = cached_mood {
            (mood, understanding.mood_intensity.clamp(1, 10))
        } else {
            self.resolve_understanding(understanding, &current_personality)
        };
        let recently_same_mood = current_personality.current_mood == new_mood.to_string()
            && (i16::from(current_personality.mood_intensity) - i16::from(new_intensity)).abs() < 2
            && now.signed_duration_since(current_personality.last_mood_change)
                < Duration::minutes(30);

        if recently_same_mood {
            self.remember_cached_mood(
                cache_key,
                new_mood.clone(),
                now,
                subject_key.as_deref(),
                mood_config.cache_retention_secs(),
            );
            return Ok(new_mood);
        }

        let mood_name = new_mood.to_string();
        self.memory_manager
            .mutate_bot_personality_and_record_emotion(&mood_name, context, |mut personality| {
                personality.current_mood = mood_name.clone();
                personality.mood_intensity = new_intensity;
                personality.last_mood_change = now;
                self.adjust_personality_traits(&mut personality, &new_mood);
                personality
            })
            .await?;

        // 只有持久化成功后才标记已应用；失败时下一次调用仍可重试。
        self.remember_cached_mood(
            cache_key,
            new_mood.clone(),
            now,
            subject_key.as_deref(),
            mood_config.cache_retention_secs(),
        );

        Ok(new_mood)
    }

    /// 只分析消息情绪，不改变机器人人格。用于记录用户自己的情绪历史。
    pub async fn analyze_mood(&self, message: &str, context: &str) -> Mood {
        let understanding = understand(UnderstandingRequest::text(message, context)).await;
        self.analyze_mood_with_understanding(message, context, &understanding)
            .await
    }

    pub(crate) async fn analyze_mood_with_understanding(
        &self,
        message: &str,
        context: &str,
        understanding: &MessageUnderstanding,
    ) -> Mood {
        let cache_key = mood_cache_key(message, context);
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
        let mood = self.resolve_understanding(understanding, &personality).0;
        let mut cache = self
            .mood_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.insert(
            cache_key,
            CachedMood {
                mood: mood.clone(),
                analyzed_at: now,
                applied_subjects: HashSet::new(),
            },
        );
        prune_mood_cache(
            &mut cache,
            now,
            Duration::seconds(mood_config.cache_retention_secs() as i64),
        );
        mood
    }

    fn remember_cached_mood(
        &self,
        cache_key: String,
        mood: Mood,
        analyzed_at: chrono::DateTime<Local>,
        subject_key: Option<&str>,
        retention_secs: u64,
    ) {
        let mut cache = self
            .mood_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cached = cache.entry(cache_key).or_insert_with(|| CachedMood {
            mood: mood.clone(),
            analyzed_at,
            applied_subjects: HashSet::new(),
        });
        cached.mood = mood;
        cached.analyzed_at = analyzed_at;
        if let Some(subject_key) = subject_key {
            if cached.applied_subjects.len() >= MAX_APPLIED_SUBJECTS_PER_ENTRY {
                cached.applied_subjects.clear();
            }
            cached.applied_subjects.insert(subject_key.to_string());
        }
        prune_mood_cache(
            &mut cache,
            analyzed_at,
            Duration::seconds(retention_secs as i64),
        );
    }

    fn resolve_understanding(
        &self,
        understanding: &MessageUnderstanding,
        current_personality: &BotPersonality,
    ) -> (Mood, u8) {
        let candidate = Mood::from_string(&understanding.mood);
        let mood = if understanding.mood_confidence < 35 {
            Mood::from_string(&current_personality.current_mood)
        } else {
            candidate
        };
        let intensity = understanding.mood_intensity.clamp(1, 10);
        (mood, intensity)
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

    pub async fn should_change_mood_naturally(&self) -> bool {
        let personality = self.memory_manager.get_bot_personality().await;
        let now = Local::now();
        let time_since_last_change = now.signed_duration_since(personality.last_mood_change);

        let drift_after_secs = crate::config::get().mood().natural_drift_after_secs();
        time_since_last_change > Duration::seconds(drift_after_secs as i64)
    }

    pub async fn natural_mood_drift(&self) -> Result<()> {
        let _update_guard = self.update_lock.lock().await;
        let personality = self.memory_manager.get_bot_personality().await;
        let now = Local::now();
        let drift_after_secs = crate::config::get().mood().natural_drift_after_secs();
        if now.signed_duration_since(personality.last_mood_change)
            <= Duration::seconds(drift_after_secs as i64)
        {
            return Ok(());
        }

        // 根据当前时间和能量水平自然调整情绪
        let hour = now.hour();
        let new_mood = match hour {
            6..=11 => Mood::Happy,     // 早晨开心
            12..=14 => Mood::Excited,  // 中午兴奋
            15..=17 => Mood::Curious,  // 下午好奇
            18..=20 => Mood::Playful,  // 傍晚顽皮
            21..=23 => Mood::Calm,     // 晚上平静
            0..=5 => Mood::Thoughtful, // 深夜深思
            _ => Mood::Neutral,
        };

        self.memory_manager
            .mutate_bot_personality(|mut personality| {
                personality.current_mood = new_mood.to_string();
                personality.mood_intensity = 4;
                personality.energy_level = move_toward(personality.energy_level, 6);
                personality.social_confidence = move_toward(personality.social_confidence, 6);
                personality.curiosity_level = move_toward(personality.curiosity_level, 6);
                personality.last_mood_change = now;
                personality
            })
            .await?;

        Ok(())
    }
}

fn mood_cache_key(message: &str, context: &str) -> String {
    let mut hasher = DefaultHasher::new();
    context.hash(&mut hasher);
    message.hash(&mut hasher);
    format!("{context}:{:016x}", hasher.finish())
}

fn prune_mood_cache(
    cache: &mut HashMap<String, CachedMood>,
    now: chrono::DateTime<Local>,
    retention: Duration,
) {
    cache.retain(|_, cached| now.signed_duration_since(cached.analyzed_at) < retention);
    if cache.len() <= MAX_MOOD_CACHE_ENTRIES {
        return;
    }
    let mut oldest = cache
        .iter()
        .map(|(key, cached)| (key.clone(), cached.analyzed_at))
        .collect::<Vec<_>>();
    oldest.sort_by_key(|(_, analyzed_at)| *analyzed_at);
    for (key, _) in oldest
        .into_iter()
        .take(cache.len() - MAX_MOOD_CACHE_ENTRIES)
    {
        cache.remove(&key);
    }
}

fn move_toward(value: u8, target: u8) -> u8 {
    match value.cmp(&target) {
        std::cmp::Ordering::Less => value.saturating_add(1),
        std::cmp::Ordering::Greater => value.saturating_sub(1),
        std::cmp::Ordering::Equal => value,
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_MOOD_CACHE_ENTRIES, Mood, MoodSystem};
    use crate::memory::{MemoryManager, MemoryType};
    use crate::model::semantic::MessageUnderstanding;
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
                let happy = MessageUnderstanding {
                    mood: "happy".to_string(),
                    mood_intensity: 8,
                    mood_confidence: 95,
                    ..MessageUnderstanding::default()
                };
                let calm = MessageUnderstanding {
                    mood: "calm".to_string(),
                    mood_intensity: 5,
                    mood_confidence: 90,
                    ..MessageUnderstanding::default()
                };

                assert_eq!(
                    mood_system
                        .analyze_mood_with_understanding(
                            "用户分享了一件让人开心的事",
                            "private_chat",
                            &happy,
                        )
                        .await,
                    Mood::Happy
                );
                let mood = mood_system
                    .analyze_and_update_mood_with_understanding(
                        "用户分享了一件让人开心的事",
                        "private_chat",
                        &happy,
                    )
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
                    .analyze_and_update_mood_with_understanding(
                        "用户分享了一件让人开心的事",
                        "private_chat",
                        &happy,
                    )
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
                        .analyze_mood_with_understanding(
                            "用户说现在想安静一会儿",
                            "private_chat",
                            &calm,
                        )
                        .await,
                    Mood::Calm
                );

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn applied_cache_state_is_isolated_by_subject() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path();
                let manager = Arc::new(MemoryManager::new(
                    path.to_str().expect("临时路径应为 UTF-8"),
                ));
                let mood_system = MoodSystem::new(Arc::clone(&manager));
                let strong = MessageUnderstanding {
                    mood: "happy".to_string(),
                    mood_intensity: 8,
                    mood_confidence: 95,
                    ..MessageUnderstanding::default()
                };
                let mild = MessageUnderstanding {
                    mood: "happy".to_string(),
                    mood_intensity: 4,
                    mood_confidence: 95,
                    ..MessageUnderstanding::default()
                };

                mood_system
                    .analyze_and_update_mood_for_subject_with_understanding(
                        "今天不错",
                        "private_chat",
                        Some(1),
                        &strong,
                    )
                    .await
                    .expect("首个用户应更新情绪");
                mood_system
                    .analyze_and_update_mood_for_subject_with_understanding(
                        "今天不错",
                        "private_chat",
                        Some(1),
                        &mild,
                    )
                    .await
                    .expect("同一用户重复处理应命中缓存");
                assert_eq!(manager.get_bot_personality().await.mood_intensity, 8);

                mood_system
                    .analyze_and_update_mood_for_subject_with_understanding(
                        "今天不错",
                        "private_chat",
                        Some(2),
                        &mild,
                    )
                    .await
                    .expect("另一个用户不应共享已应用状态");
                assert_eq!(manager.get_bot_personality().await.mood_intensity, 4);
                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }

    #[test]
    fn mood_cache_has_a_hard_capacity_limit() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path();
                let manager = Arc::new(MemoryManager::new(
                    path.to_str().expect("临时路径应为 UTF-8"),
                ));
                let mood_system = MoodSystem::new(manager);
                let understanding = MessageUnderstanding::default();
                for index in 0..(MAX_MOOD_CACHE_ENTRIES + 8) {
                    mood_system
                        .analyze_mood_with_understanding(
                            &format!("cache-entry-{index}"),
                            "private_chat",
                            &understanding,
                        )
                        .await;
                }
                assert!(
                    mood_system.mood_cache.lock().expect("缓存锁不应中毒").len()
                        <= MAX_MOOD_CACHE_ENTRIES
                );
            });
    }
}
