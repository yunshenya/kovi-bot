//! # 主动聊天模块
//!
//! 提供智能主动聊天功能，包括：
//! - 基于情绪和社交信心的主动聊天判断
//! - 智能目标选择（群聊或私聊）
//! - 活跃度检测和时机判断
//! - 话题生成和个性化聊天

use crate::memory::MemoryManager;
use crate::model::semantic::{MessageUnderstanding, UnderstandingRequest, understand};
use crate::model::utils::{BotMemory, Roles, params_model};
use crate::model::{send_tracked_group_message, send_tracked_private_message};
use crate::mood_system::MOOD_SYSTEM;
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
    /// 机器人实例，用于发送消息
    bot: Arc<RuntimeBot>,
}

impl ProactiveChatManager {
    pub fn new(memory_manager: Arc<MemoryManager>, bot: Arc<RuntimeBot>) -> Self {
        let topic_generator = TopicGenerator::new(Arc::clone(&memory_manager));
        Self {
            memory_manager,
            topic_generator,
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

            // 最信任用户有独立、严格限额的关心频率，不与群聊随机目标竞争。
            let main_admin_message_sent = match self.try_initiate_main_admin_chat().await {
                Ok(sent) => sent,
                Err(error) => {
                    eprintln!("Failed to initiate main-admin chat: {}", error);
                    false
                }
            };

            // 常规主动聊天仍遵循全局冷却与活跃度限制。
            if !main_admin_message_sent
                && self.should_initiate_chat().await
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

    /// 由模型结合关系、情绪与近期互动决定是否主动关心最信任用户。
    async fn try_initiate_main_admin_chat(&self) -> Result<bool> {
        let proactive_config = crate::config::get().proactive().clone();
        let Some(main_admin) = proactive_config.main_admin() else {
            return Ok(false);
        };
        if !self.should_decide_main_admin_chat(main_admin).await {
            return Ok(false);
        }

        let message = self.decide_main_admin_chat(main_admin).await;
        let decision_record = if message.is_some() {
            "主人主动关心决策：发送消息"
        } else {
            "主人主动关心决策：暂不打扰"
        };
        self.memory_manager
            .add_conversation_memory(main_admin, decision_record, "proactive_main_admin_decision")
            .await?;

        let Some(message) = message else {
            return Ok(false);
        };
        if !send_tracked_private_message(&self.bot, main_admin, message.clone()).await {
            return Ok(false);
        }
        self.memory_manager
            .add_conversation_memory(
                main_admin,
                &format!("主动关心: {}", message),
                "proactive_private_chat",
            )
            .await?;
        Ok(true)
    }

    /// 本地仅控制模型决策频率；是否真正发送完全交由模型决定。
    async fn should_decide_main_admin_chat(&self, main_admin: i64) -> bool {
        let proactive_config = crate::config::get().proactive().clone();
        let recent_memories = self.memory_manager.get_recent_memories(0).await;
        let decision_boundary = Local::now()
            - chrono::Duration::seconds(proactive_config.main_admin_decision_interval_secs() as i64);
        !recent_memories.iter().any(|memory| {
            memory.subject_id == Some(main_admin)
                && memory.context == "proactive_main_admin_decision"
                && memory.timestamp > decision_boundary
        })
    }

    async fn decide_main_admin_chat(&self, main_admin: i64) -> Option<String> {
        let personality = self.memory_manager.get_bot_personality().await;
        let profile = self.memory_manager.get_user_profile(main_admin).await;
        let summary = self
            .memory_manager
            .get_conversation_summary("private_chat", main_admin)
            .await;
        let contextual_memories = self
            .memory_manager
            .get_contextual_memories(main_admin, "private_chat", "主动关心近况", 5)
            .await;
        let recent_outreach = self
            .memory_manager
            .get_recent_memories(0)
            .await
            .into_iter()
            .filter(|memory| {
                memory.subject_id == Some(main_admin) && memory.context == "proactive_private_chat"
            })
            .take(3)
            .map(|memory| {
                format!(
                    "{}：{}",
                    memory.timestamp.format("%m-%d %H:%M"),
                    memory.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let profile_description = profile.map_or_else(
            || "暂时没有详细档案".to_string(),
            |profile| {
                format!(
                    "昵称：{}；互动次数：{}；关系：{}/10；兴趣：{}",
                    profile.nickname,
                    profile.interaction_count,
                    profile.relationship_level,
                    profile.interests.join("、")
                )
            },
        );
        let memories = contextual_memories
            .iter()
            .map(|memory| memory.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let now = Local::now().format("%Y-%m-%d %H:%M");
        let mut request = vec![
            BotMemory {
                role: Roles::System,
                content: "你是芸汐，一位温柔、害羞、慢热而真诚的女孩子。现在请自主决定是否要主动联系你最信任的人。\
                          这不是定时任务：请根据当前时间、自己的情绪、近期互动与已有关心记录判断。\
                          不要为了刷存在感而发送；对方刚忙完、频繁被打扰、没有自然的话题时，选择不打扰。\
                          真心想问候、关心、分享一个自然的小心情时才发送。\
                          严格按以下格式回答：如果不发送，只输出 [[SKIP]]；如果发送，先输出 [[SEND]]，随后写一条自然、简短、不过分黏人的私聊消息。不要解释规则。"
                    .to_string(),
            },
            BotMemory {
                role: Roles::User,
                content: format!(
                    "当前时间：{now}\n当前情绪：{}，能量：{}/10，社交信心：{}/10\n主人档案：{}\n滚动摘要：{}\n相关记忆：{}\n最近主动关心记录：{}",
                    personality.current_mood,
                    personality.energy_level,
                    personality.social_confidence,
                    profile_description,
                    summary.unwrap_or_else(|| "（无）".to_string()),
                    if memories.is_empty() { "（无）" } else { &memories },
                    if recent_outreach.is_empty() { "（无）" } else { &recent_outreach },
                ),
            },
        ];
        parse_main_admin_decision(&params_model(&mut request).await.content)
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
        let main_admin = crate::config::get().proactive().main_admin();

        user_profiles
            .into_iter()
            .filter(|profile| {
                Some(profile.user_id) != main_admin
                    && profile
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
            let content = topic.content.clone();

            // 发送消息
            if !send_tracked_group_message(&self.bot, group_id, content.clone()).await {
                return Ok(());
            }

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

    async fn initiate_private_chat(&self, user_id: i64) -> Result<bool> {
        // 检查是否应该向这个用户发起对话
        if !self
            .topic_generator
            .should_initiate_conversation(None, Some(user_id))
            .await
        {
            return Ok(false);
        }

        // 生成个性化话题
        if let Some(topic) = self
            .topic_generator
            .generate_personalized_topic(user_id)
            .await?
        {
            let content = topic.content.clone();

            // 发送消息
            if !send_tracked_private_message(&self.bot, user_id, content.clone()).await {
                return Ok(false);
            }

            // 记录这次主动对话
            self.memory_manager
                .add_conversation_memory(
                    user_id,
                    &format!("主动发起话题: {}", content),
                    "proactive_private_chat",
                )
                .await?;
            return Ok(true);
        }

        Ok(false)
    }

    pub async fn handle_user_response(
        &self,
        user_id: i64,
        message: &str,
        _is_group: bool,
    ) -> Result<()> {
        let context = if _is_group {
            "group_chat"
        } else {
            "private_chat"
        };
        let understanding = understand(UnderstandingRequest::text(message, context)).await;

        // 更新用户档案
        self.update_user_profile(user_id, _is_group, &understanding)
            .await?;

        // 分析情绪变化
        MOOD_SYSTEM
            .analyze_and_update_mood_with_understanding(message, context, &understanding)
            .await?;

        // 记录对话记忆
        let memory_tags = understanding.memory_tags();
        self.memory_manager
            .add_conversation_memory_with_hints(
                user_id,
                message,
                context,
                Some(understanding.memory_importance()),
                &memory_tags,
            )
            .await?;

        Ok(())
    }

    async fn update_user_profile(
        &self,
        user_id: i64,
        _is_group: bool,
        understanding: &MessageUnderstanding,
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

        if understanding.gratitude {
            profile.relationship_level = (profile.relationship_level + 1).min(10);
        }

        for interest in &understanding.interests {
            if !profile.interests.contains(interest) {
                profile.interests.push(interest.clone());
            }
        }
        profile.interests.truncate(20);

        // 更新用户档案
        self.memory_manager
            .update_user_profile(user_id, profile)
            .await?;

        Ok(())
    }
}

fn parse_main_admin_decision(content: &str) -> Option<String> {
    let message = content.trim().strip_prefix("[[SEND]]")?.trim();
    if message.is_empty() {
        return None;
    }
    Some(message.replace("[[NEXT_MESSAGE]]", "\n"))
}

#[derive(Debug, Clone)]
enum ChatTarget {
    Group(i64),
    User(i64),
    None,
}

#[cfg(test)]
mod tests {
    use super::parse_main_admin_decision;

    #[test]
    fn main_admin_model_decision_requires_send_marker() {
        assert_eq!(
            parse_main_admin_decision("[[SEND]] 刚刚想到你啦，今天还好吗？"),
            Some("刚刚想到你啦，今天还好吗？".to_string())
        );
        assert_eq!(parse_main_admin_decision("[[SKIP]]"), None);
        assert_eq!(parse_main_admin_decision("随便问候一下"), None);
    }
}
