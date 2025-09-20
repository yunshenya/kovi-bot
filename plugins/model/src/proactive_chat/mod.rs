use crate::memory::MemoryManager;
use crate::topic_generator::TopicGenerator;
use crate::mood_system::MoodSystem;
use kovi::RuntimeBot;
use std::sync::Arc;
use std::time::Duration;
use kovi::tokio::time::sleep;
use anyhow::Result;
use chrono::Local;

pub struct ProactiveChatManager {
    memory_manager: Arc<MemoryManager>,
    topic_generator: TopicGenerator,
    mood_system: MoodSystem,
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
            // 自然情绪变化
            if let Err(e) = self.mood_system.natural_mood_drift().await {
                eprintln!("Failed to update mood naturally: {}", e);
            }

            // 检查是否应该主动发起对话
            if self.should_initiate_chat().await {
                if let Err(e) = self.try_initiate_chat().await {
                    eprintln!("Failed to initiate chat: {}", e);
                }
            }

            // 等待一段时间再检查
            sleep(Duration::from_secs(300)).await; // 5分钟检查一次
        }
    }

    async fn should_initiate_chat(&self) -> bool {
        let personality = self.memory_manager.get_bot_personality().await;
        
        // 检查基本条件
        if personality.energy_level < 5 || personality.social_confidence < 4 {
            return false;
        }

        // 检查最近是否有足够的活动
        let recent_memories = self.memory_manager.get_recent_memories(20).await;
        let now = Local::now();
        let two_hours_ago = now - chrono::Duration::hours(2);
        
        let recent_activity_count = recent_memories
            .iter()
            .filter(|memory| memory.timestamp > two_hours_ago)
            .count();

        // 如果最近活动太少，增加主动聊天的概率
        recent_activity_count < 3
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
            },
            ChatTarget::User(user_id) => {
                self.initiate_private_chat(user_id).await?;
            },
            ChatTarget::None => {
                // 没有合适的目标，跳过这次主动聊天
            }
        }

        Ok(())
    }

    async fn get_active_groups(&self) -> Vec<i64> {
        // 这里应该从实际的群组列表中获取
        // 暂时返回空列表，实际实现时需要从bot获取群组列表
        vec![]
    }

    async fn get_active_users(&self) -> Vec<i64> {
        // 从用户档案中获取最近活跃的用户
        // 这里需要访问私有字段，暂时返回空列表
        // 实际实现时需要添加公共方法
        vec![]
    }

    async fn select_chat_target(&self, groups: Vec<i64>, users: Vec<i64>) -> ChatTarget {
        let personality = self.memory_manager.get_bot_personality().await;
        
        // 根据社交信心决定是群聊还是私聊
        if personality.social_confidence >= 7 && !groups.is_empty() {
            // 高社交信心，选择群聊
            let group_id = groups[0]; // 简化选择逻辑
            return ChatTarget::Group(group_id);
        } else if !users.is_empty() {
            // 选择私聊
            let user_id = users[0]; // 简化选择逻辑
            return ChatTarget::User(user_id);
        }
        
        ChatTarget::None
    }

    async fn initiate_group_chat(&self, group_id: i64) -> Result<()> {
        // 检查是否应该在这个群组发起对话
        if !self.topic_generator.should_initiate_conversation(Some(group_id), None).await {
            return Ok(());
        }

        // 生成话题
        if let Some(topic) = self.topic_generator.generate_topic(Some(group_id), None).await? {
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
            self.memory_manager.add_conversation_memory(
                group_id,
                &format!("主动发起话题: {}", content),
                "proactive_group_chat"
            ).await?;
        }

        Ok(())
    }

    async fn initiate_private_chat(&self, user_id: i64) -> Result<()> {
        // 检查是否应该向这个用户发起对话
        if !self.topic_generator.should_initiate_conversation(None, Some(user_id)).await {
            return Ok(());
        }

        // 生成个性化话题
        if let Some(topic) = self.topic_generator.generate_personalized_topic(user_id).await? {
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
            self.memory_manager.add_conversation_memory(
                user_id,
                &format!("主动发起话题: {}", content),
                "proactive_private_chat"
            ).await?;
        }

        Ok(())
    }

    pub async fn handle_user_response(&self, user_id: i64, message: &str, _is_group: bool) -> Result<()> {
        // 更新用户档案
        self.update_user_profile(user_id, message, _is_group).await?;
        
        // 分析情绪变化
        let context = if _is_group { "group_chat" } else { "private_chat" };
        self.mood_system.analyze_and_update_mood(message, context).await?;
        
        // 记录对话记忆
        self.memory_manager.add_conversation_memory(
            user_id,
            message,
            context
        ).await?;

        Ok(())
    }

    async fn update_user_profile(&self, user_id: i64, message: &str, _is_group: bool) -> Result<()> {
        let mut profile = self.memory_manager.get_user_profile(user_id).await
            .unwrap_or_else(|| crate::memory::UserProfile {
                user_id,
                nickname: format!("User_{}", user_id),
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
        let interests = self.extract_interests_from_message(message);
        for interest in interests {
            if !profile.interests.contains(&interest) {
                profile.interests.push(interest);
            }
        }

        // 更新用户档案
        self.memory_manager.update_user_profile(user_id, profile).await?;

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

#[derive(Debug)]
enum ChatTarget {
    Group(i64),
    User(i64),
    None,
}
