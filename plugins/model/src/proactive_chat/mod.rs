//! # 主动聊天模块
//!
//! 提供智能主动聊天功能，包括：
//! - 基于情绪和社交信心的主动聊天判断
//! - 智能目标选择（群聊或私聊）
//! - 活跃度检测和时机判断
//! - 话题生成和个性化聊天

use crate::memory::MemoryManager;
use crate::mood_system::MoodSystem;
use crate::topic_generator::TopicGenerator;
use anyhow::Result;
use chrono::Local;
use kovi::RuntimeBot;
use kovi::tokio::time::sleep;
use rand::Rng;
use rand::prelude::IndexedRandom;
use std::sync::Arc;
use std::time::Duration;

pub mod startup;

/// 主动聊天管理器
///
/// 负责管理机器人的主动聊天行为，包括判断时机、选择目标、生成话题等
pub struct ProactiveChatManager {
    /// 记忆管理器，用于获取用户和群组信息
    memory_manager: Arc<MemoryManager>,
    /// 话题生成器，用于生成个性化话题
    topic_generator: TopicGenerator,
    /// 情绪系统，用于分析当前情绪状态
    mood_system: MoodSystem,
    /// 机器人实例，用于发送消息
    bot: Arc<RuntimeBot>,
}

impl ProactiveChatManager {
    pub fn new(memory_manager: Arc<MemoryManager>, bot: Arc<RuntimeBot>) -> Self {
        let topic_generator = TopicGenerator::new(Arc::clone(&memory_manager));
        let mood_system = MoodSystem::new(Arc::clone(&memory_manager));

        Self {
            memory_manager,
            topic_generator,
            mood_system,
            bot,
        }
    }

    pub async fn start_proactive_chat_loop(&self) {
        loop {
            let proactive_config = crate::config::get().proactive().clone();
            if !proactive_config.enabled() {
                sleep(Duration::from_secs(proactive_config.check_interval_secs())).await;
                continue;
            }

            // 检查是否应该主动发起对话
            if self.should_initiate_chat().await
                && let Err(e) = self.try_initiate_chat().await
            {
                eprintln!("Failed to initiate chat: {}", e);
            }

            // 等待一段时间再检查
            sleep(Duration::from_secs(proactive_config.check_interval_secs())).await;
        }
    }

    async fn should_initiate_chat(&self) -> bool {
        let personality = self.memory_manager.get_bot_personality().await;

        // 检查基本条件
        if personality.energy_level < 5 || personality.social_confidence < 4 {
            return false;
        }

        let proactive_config = crate::config::get().proactive().clone();
        let recent_memories = self.memory_manager.get_recent_memories(100).await;
        let now = Local::now();
        let inactivity_boundary =
            now - chrono::Duration::seconds(proactive_config.inactivity_threshold_secs() as i64);
        let cooldown_boundary =
            now - chrono::Duration::seconds(proactive_config.cooldown_secs() as i64);

        // 全局冷却，防止循环每次命中时连续推送。
        if recent_memories.iter().any(|memory| {
            memory.context.starts_with("proactive_") && memory.timestamp > cooldown_boundary
        }) {
            return false;
        }

        let recent_activity_count = recent_memories
            .iter()
            .filter(|memory| {
                !memory.context.starts_with("proactive_") && memory.timestamp > inactivity_boundary
            })
            .count();

        if recent_activity_count >= 3 {
            return false;
        }

        let probability = proactive_config.push_probability_percent() as u32;
        probability > 0 && rand::rng().random_ratio(probability, 100)
    }

    async fn try_initiate_chat(&self) -> Result<()> {
        // 获取所有群组和用户
        let groups = self.get_active_groups().await;
        let users = self.get_active_users().await;

        // 随机选择一个目标
        let target = self.select_chat_target(groups, users).await;

        match target {
            ChatTarget::Group(group_id) => {
                self.initiate_group_chat(group_id).await?;
            }
            ChatTarget::User(user_id) => {
                self.initiate_private_chat(user_id).await?;
            }
            ChatTarget::None => {
                // 没有合适的目标，跳过这次主动聊天
            }
        }

        Ok(())
    }

    async fn get_active_groups(&self) -> Vec<i64> {
        // 从群组档案中获取活跃群组
        let group_profiles = self.memory_manager.get_all_group_profiles().await;
        let now = Local::now();
        let one_day_ago = now - chrono::Duration::days(1);

        group_profiles
            .into_iter()
            .filter(|profile| profile.last_activity > one_day_ago && profile.activity_level > 3)
            .map(|profile| profile.group_id)
            .collect()
    }

    async fn get_active_users(&self) -> Vec<i64> {
        // 从用户档案中获取最近活跃的用户
        let user_profiles = self.memory_manager.get_all_user_profiles().await;
        let now = Local::now();
        let three_days_ago = now - chrono::Duration::days(3);

        user_profiles
            .into_iter()
            .filter(|profile| {
                profile
                    .last_private_interaction
                    .is_some_and(|last_private| last_private > three_days_ago)
                    && profile.relationship_level > 2
            })
            .map(|profile| profile.user_id)
            .collect()
    }

    async fn select_chat_target(&self, groups: Vec<i64>, users: Vec<i64>) -> ChatTarget {
        let personality = self.memory_manager.get_bot_personality().await;

        let mut targets = Vec::new();
        if personality.social_confidence >= 5 {
            targets.extend(groups.into_iter().map(ChatTarget::Group));
        }
        targets.extend(users.into_iter().map(ChatTarget::User));

        targets
            .choose(&mut rand::rng())
            .cloned()
            .unwrap_or(ChatTarget::None)
    }

    async fn initiate_group_chat(&self, group_id: i64) -> Result<()> {
        // 检查是否应该在这个群组发起对话
        if !self
            .topic_generator
            .should_initiate_conversation(Some(group_id), None)
            .await
        {
            return Ok(());
        }

        // 生成话题
        if let Some(topic) = self
            .topic_generator
            .generate_topic(Some(group_id), None)
            .await?
        {
            // 添加情绪前缀
            let mood_prefix = self.mood_system.get_mood_based_response_style().await;
            let content = topic.content.clone();
            let message = if mood_prefix.is_empty() {
                content.clone()
            } else {
                format!("{} {}", mood_prefix, content)
            };

            // 发送消息
            self.bot.send_group_msg(group_id, &message);

            // 记录这次主动对话
            self.memory_manager
                .add_conversation_memory(
                    group_id,
                    &format!("主动发起话题: {}", content),
                    "proactive_group_chat",
                )
                .await?;
        }

        Ok(())
    }

    async fn initiate_private_chat(&self, user_id: i64) -> Result<()> {
        // 检查是否应该向这个用户发起对话
        if !self
            .topic_generator
            .should_initiate_conversation(None, Some(user_id))
            .await
        {
            return Ok(());
        }

        // 生成个性化话题
        if let Some(topic) = self
            .topic_generator
            .generate_personalized_topic(user_id)
            .await?
        {
            // 添加情绪前缀
            let mood_prefix = self.mood_system.get_mood_based_response_style().await;
            let content = topic.content.clone();
            let message = if mood_prefix.is_empty() {
                content.clone()
            } else {
                format!("{} {}", mood_prefix, content)
            };

            // 发送消息
            self.bot.send_private_msg(user_id, &message);

            // 记录这次主动对话
            self.memory_manager
                .add_conversation_memory(
                    user_id,
                    &format!("主动发起话题: {}", content),
                    "proactive_private_chat",
                )
                .await?;
        }

        Ok(())
    }

    pub async fn handle_user_response(
        &self,
        user_id: i64,
        message: &str,
        _is_group: bool,
    ) -> Result<()> {
        // 更新用户档案
        self.update_user_profile(user_id, message, _is_group)
            .await?;

        // 分析情绪变化
        let context = if _is_group {
            "group_chat"
        } else {
            "private_chat"
        };
        self.mood_system
            .analyze_and_update_mood(message, context)
            .await?;

        // 记录对话记忆
        self.memory_manager
            .add_conversation_memory(user_id, message, context)
            .await?;

        Ok(())
    }

    async fn update_user_profile(
        &self,
        user_id: i64,
        message: &str,
        _is_group: bool,
    ) -> Result<()> {
        let mut profile = self
            .memory_manager
            .get_user_profile(user_id)
            .await
            .unwrap_or_else(|| crate::memory::UserProfile {
                user_id,
                nickname: format!("User_{}", user_id),
                personality_traits: Vec::new(),
                interests: Vec::new(),
                relationship_level: 1,
                last_interaction: Local::now(),
                interaction_count: 0,
                last_private_interaction: if _is_group { None } else { Some(Local::now()) },
                mood_history: Vec::new(),
            });

        // 更新互动信息
        profile.last_interaction = Local::now();
        profile.interaction_count = profile.interaction_count.saturating_add(1);
        if !_is_group {
            profile.last_private_interaction = Some(Local::now());
        }

        // 根据对话内容更新关系等级
        if message.contains("谢谢") || message.contains("感谢") {
            profile.relationship_level = (profile.relationship_level + 1).min(10);
        }

        // 提取兴趣关键词
        let interests = self.extract_interests_from_message(message);
        for interest in interests {
            if !profile.interests.contains(&interest) {
                profile.interests.push(interest);
            }
        }

        // 更新用户档案
        self.memory_manager
            .update_user_profile(user_id, profile)
            .await?;

        Ok(())
    }

    fn extract_interests_from_message(&self, message: &str) -> Vec<String> {
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
}

#[derive(Debug, Clone)]
enum ChatTarget {
    Group(i64),
    User(i64),
    None,
}
