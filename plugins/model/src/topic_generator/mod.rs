//! # 话题生成器模块
//!
//! 提供智能话题生成功能，包括：
//! - 基于情绪和能量水平的话题选择
//! - 个性化话题生成
//! - 话题模板库管理
//! - 话题分类和标签系统

use crate::memory::MemoryManager;
use crate::model::utils::{BotMemory, Roles, params_model};
use anyhow::Result;
use chrono::{Local, Timelike};
use rand::prelude::IndexedRandom;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 话题结构体
///
/// 表示一个完整的话题，包含内容、分类、要求等信息
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Topic {
    /// 话题内容
    pub content: String,
    /// 话题分类
    pub category: TopicCategory,
    /// 情绪要求（可选）
    pub mood_requirement: Option<String>,
    /// 所需能量水平 (0-10)
    pub energy_level_required: u8,
    /// 话题标签
    pub tags: Vec<String>,
}

/// 话题分类枚举
///
/// 定义不同类型的话题，用于分类和筛选
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum TopicCategory {
    /// 日常闲聊：轻松随意的话题
    Casual,
    /// 深度话题：需要深入思考的话题
    Deep,
    /// 有趣话题：娱乐性较强的话题
    Fun,
    /// 个人话题：涉及个人经历和感受的话题
    Personal,
    /// 时事话题：当前热点和新闻相关话题
    Current,
    /// 创意话题：需要创造力和想象力的话题
    Creative,
    /// 怀旧话题：回忆过去的话题
    Nostalgic,
    /// 未来话题：展望未来的话题
    Future,
    /// 知识话题：分享知识与学习经验
    Knowledge,
    /// 社交话题：围绕朋友、团队与沟通
    Social,
}

pub struct TopicGenerator {
    memory_manager: Arc<MemoryManager>,
    topic_templates: Vec<TopicTemplate>,
}

#[derive(Debug, Clone)]
struct TopicTemplate {
    template: String,
    category: TopicCategory,
    mood_requirement: Option<String>,
    energy_level_required: u8,
    tags: Vec<String>,
}

impl TopicGenerator {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        let topic_templates = Self::init_topic_templates();
        Self {
            memory_manager,
            topic_templates,
        }
    }

    fn init_topic_templates() -> Vec<TopicTemplate> {
        vec![
            TopicTemplate {
                template: "今天天气怎么样？感觉适合做什么呢？".to_string(),
                category: TopicCategory::Casual,
                mood_requirement: None,
                energy_level_required: 3,
                tags: vec!["天气".to_string(), "日常".to_string()],
            },
            TopicTemplate {
                template: "最近有什么好看的电影或电视剧推荐吗？".to_string(),
                category: TopicCategory::Fun,
                mood_requirement: None,
                energy_level_required: 4,
                tags: vec!["娱乐".to_string(), "推荐".to_string()],
            },
            TopicTemplate {
                template: "如果让你选择一种超能力，你会选择什么？为什么？".to_string(),
                category: TopicCategory::Creative,
                mood_requirement: Some("curious".to_string()),
                energy_level_required: 6,
                tags: vec!["想象".to_string(), "超能力".to_string()],
            },
            TopicTemplate {
                template: "你小时候最难忘的一件事是什么？".to_string(),
                category: TopicCategory::Nostalgic,
                mood_requirement: Some("calm".to_string()),
                energy_level_required: 5,
                tags: vec!["回忆".to_string(), "童年".to_string()],
            },
            TopicTemplate {
                template: "你觉得十年后的世界会是什么样子？".to_string(),
                category: TopicCategory::Future,
                mood_requirement: Some("curious".to_string()),
                energy_level_required: 7,
                tags: vec!["未来".to_string(), "科技".to_string()],
            },
            TopicTemplate {
                template: "最近有什么让你开心的小事吗？".to_string(),
                category: TopicCategory::Personal,
                mood_requirement: Some("happy".to_string()),
                energy_level_required: 4,
                tags: vec!["情感".to_string(), "分享".to_string()],
            },
            TopicTemplate {
                template: "如果有一天你变成了动物，你希望是什么动物？".to_string(),
                category: TopicCategory::Fun,
                mood_requirement: None,
                energy_level_required: 5,
                tags: vec!["动物".to_string(), "想象".to_string()],
            },
            TopicTemplate {
                template: "你觉得什么是真正的友谊？".to_string(),
                category: TopicCategory::Deep,
                mood_requirement: Some("thoughtful".to_string()),
                energy_level_required: 8,
                tags: vec!["哲学".to_string(), "友谊".to_string()],
            },
            TopicTemplate {
                template: "最近有什么新的兴趣爱好吗？".to_string(),
                category: TopicCategory::Personal,
                mood_requirement: None,
                energy_level_required: 4,
                tags: vec!["兴趣".to_string(), "学习".to_string()],
            },
            TopicTemplate {
                template: "如果让你设计一个理想的城市，你会怎么设计？".to_string(),
                category: TopicCategory::Creative,
                mood_requirement: Some("curious".to_string()),
                energy_level_required: 7,
                tags: vec!["设计".to_string(), "城市".to_string()],
            },
            TopicTemplate {
                template: "最近学到的哪件新知识最让你惊讶？".to_string(),
                category: TopicCategory::Knowledge,
                mood_requirement: Some("curious".to_string()),
                energy_level_required: 5,
                tags: vec!["学习".to_string(), "知识".to_string()],
            },
            TopicTemplate {
                template: "你最欣赏朋友身上的什么品质？".to_string(),
                category: TopicCategory::Social,
                mood_requirement: None,
                energy_level_required: 4,
                tags: vec!["朋友".to_string(), "社交".to_string()],
            },
            TopicTemplate {
                template: "最近有什么新闻或新鲜事让你印象深刻？".to_string(),
                category: TopicCategory::Current,
                mood_requirement: None,
                energy_level_required: 4,
                tags: vec!["时事".to_string(), "新闻".to_string()],
            },
        ]
    }

    pub async fn generate_topic(
        &self,
        group_id: Option<i64>,
        user_id: Option<i64>,
    ) -> Result<Option<Topic>> {
        let bot_personality = self.memory_manager.get_bot_personality().await;

        // 根据当前情绪和能量水平筛选合适的话题
        let suitable_templates: Vec<&TopicTemplate> = self
            .topic_templates
            .iter()
            .filter(|template| {
                // 检查情绪要求
                if let Some(required_mood) = &template.mood_requirement
                    && bot_personality.current_mood != *required_mood
                {
                    return false;
                }

                // 检查能量水平要求
                template.energy_level_required <= bot_personality.energy_level
            })
            .collect();

        if suitable_templates.is_empty() {
            return Ok(None);
        }

        let recent_memories = self.memory_manager.get_recent_memories(0).await;
        let mut unused_templates = Vec::new();
        for template in &suitable_templates {
            if !Self::topic_used_recently(&recent_memories, &template.template, group_id, user_id) {
                unused_templates.push(*template);
            }
        }
        let candidate_templates = if unused_templates.is_empty() {
            suitable_templates
        } else {
            unused_templates
        };

        let selected_template = self.select_best_template(candidate_templates);

        let topic = Topic {
            content: Self::adapt_topic_to_time(&selected_template.template),
            category: selected_template.category.clone(),
            mood_requirement: selected_template.mood_requirement.clone(),
            energy_level_required: selected_template.energy_level_required,
            tags: selected_template.tags.clone(),
        };

        Ok(Some(topic))
    }

    fn adapt_topic_to_time(topic: &str) -> String {
        match Local::now().hour() {
            5..=10 => format!("早上好，{}", topic),
            11..=13 => format!("到午饭时间啦，{}", topic),
            22..=23 | 0..=4 => format!("这么晚还没睡呀，{}", topic),
            _ => topic.to_string(),
        }
    }

    fn select_best_template(&self, templates: Vec<&TopicTemplate>) -> TopicTemplate {
        let selected = templates
            .choose(&mut rand::rng())
            .expect("templates 已在调用前检查为非空");
        (**selected).clone()
    }

    pub async fn generate_personalized_topic(&self, user_id: i64) -> Result<Option<Topic>> {
        // 获取用户档案
        if let Some(user_profile) = self.memory_manager.get_user_profile(user_id).await {
            let personalized_topic = self.generate_topic_from_profile(&user_profile).await?;
            let recent_memories = self.memory_manager.get_recent_memories(0).await;
            if let Some(mut topic) = personalized_topic
                && !Self::topic_used_recently(&recent_memories, &topic.content, None, Some(user_id))
            {
                topic.content = Self::adapt_topic_to_time(&topic.content);
                return Ok(Some(topic));
            }
        }

        // 如果没有用户档案，使用通用话题
        self.generate_topic(None, Some(user_id)).await
    }

    fn topic_used_recently(
        memories: &[crate::memory::MemoryEntry],
        topic: &str,
        group_id: Option<i64>,
        user_id: Option<i64>,
    ) -> bool {
        let subject_id = group_id.or(user_id);
        let cutoff = Local::now()
            - chrono::Duration::seconds(
                crate::config::get().topic().recent_topic_cooldown_secs() as i64
            );
        memories.iter().any(|memory| {
            memory.subject_id == subject_id
                && memory.context.starts_with("proactive_")
                && memory.timestamp > cutoff
                && memory.content.contains(topic)
        })
    }

    async fn generate_topic_from_profile(
        &self,
        user_profile: &crate::memory::UserProfile,
    ) -> Result<Option<Topic>> {
        let mut messages = vec![
            BotMemory {
                role: Roles::System,
                content: "你在帮芸汐想一条自然的主动聊天开场。根据用户档案和近期兴趣，写一句简短、具体、像熟人之间随口问起的话。不要总结档案，不要解释，不要使用固定模板，不要输出引号或舞台动作。".to_string(),
            },
            BotMemory {
                role: Roles::User,
                content: format!(
                    "昵称：{}\n关系：{}/10\n兴趣：{}\n性格：{}",
                    user_profile.nickname,
                    user_profile.relationship_level,
                    user_profile.interests.join("、"),
                    user_profile.personality_traits.join("、"),
                ),
            },
        ];
        let content = params_model(&mut messages).await.content;
        let content = content
            .trim()
            .trim_matches(|character| matches!(character, '"' | '“' | '”'))
            .to_string();
        if content.is_empty() || content.len() > 240 || content.starts_with("[[") {
            return Ok(None);
        }
        Ok(Some(Topic {
            content,
            category: TopicCategory::Personal,
            mood_requirement: None,
            energy_level_required: 4,
            tags: user_profile.interests.iter().take(6).cloned().collect(),
        }))
    }

    pub async fn should_initiate_conversation(
        &self,
        group_id: Option<i64>,
        user_id: Option<i64>,
    ) -> bool {
        let bot_personality = self.memory_manager.get_bot_personality().await;

        // 检查能量水平和社交信心
        if bot_personality.energy_level < 5 || bot_personality.social_confidence < 4 {
            return false;
        }

        // 检查最近是否有互动
        let recent_memories = self.memory_manager.get_recent_memories(10).await;
        let now = Local::now();
        let one_hour_ago = now - chrono::Duration::hours(1);

        let target_subject = group_id.or(user_id);
        let target_context = if group_id.is_some() {
            "group"
        } else {
            "private"
        };
        let recent_activity = recent_memories.iter().any(|memory| {
            memory.subject_id == target_subject
                && memory.context.contains(target_context)
                && memory.timestamp > one_hour_ago
        });

        // 如果最近有活动，降低主动发起对话的概率
        if recent_activity {
            return bot_personality.curiosity_level > 7;
        }

        // 检查特定群组或用户的活跃度
        if let Some(gid) = group_id
            && let Some(group_profile) = self.memory_manager.get_group_profile(gid).await
        {
            // 如果群组不活跃，增加主动聊天的概率
            if group_profile.activity_level < 3 {
                return bot_personality.curiosity_level > 5;
            }
        }

        if let Some(uid) = user_id
            && let Some(user_profile) = self.memory_manager.get_user_profile(uid).await
        {
            // 根据关系等级调整主动聊天的概率
            match user_profile.relationship_level {
                8..=10 => return bot_personality.curiosity_level > 4, // 高关系等级更容易主动聊天
                5..=7 => return bot_personality.curiosity_level > 6,
                1..=4 => return bot_personality.curiosity_level > 8, // 低关系等级需要更高好奇心
                _ => return false,
            }
        }

        // 根据情绪决定是否主动发起对话
        match bot_personality.current_mood.as_str() {
            "happy" | "curious" | "playful" => true,
            "neutral" => bot_personality.curiosity_level > 6,
            "lonely" => bot_personality.social_confidence > 5, // 孤独时更容易主动聊天
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TopicCategory, TopicGenerator};
    use crate::memory::MemoryManager;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn temporary_memory_path(test_name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kovi-topic-{}-{}-{}.json",
            test_name,
            std::process::id(),
            chrono::Local::now().timestamp_micros()
        ))
    }

    #[test]
    fn template_library_covers_ten_categories() {
        let path = temporary_memory_path("categories");
        let manager = Arc::new(MemoryManager::new(
            path.to_str().expect("临时路径应为 UTF-8"),
        ));
        let generator = TopicGenerator::new(manager);
        let categories: HashSet<String> = generator
            .topic_templates
            .iter()
            .map(|template| format!("{:?}", template.category))
            .collect();
        assert_eq!(categories.len(), 10);
        assert!(
            generator
                .topic_templates
                .iter()
                .any(|template| template.category == TopicCategory::Knowledge)
        );
    }

    #[test]
    fn recently_used_topic_is_not_selected_again() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let path = temporary_memory_path("dedup");
                let manager = Arc::new(MemoryManager::new(
                    path.to_str().expect("临时路径应为 UTF-8"),
                ));
                manager
                    .add_conversation_memory(
                        123,
                        "主动发起话题: 今天天气怎么样？感觉适合做什么呢？",
                        "proactive_group_chat",
                    )
                    .await
                    .expect("应记录主动话题");
                let generator = TopicGenerator::new(manager);
                let topic = generator
                    .generate_topic(Some(123), None)
                    .await
                    .expect("应成功生成话题")
                    .expect("应存在可用话题");
                assert!(!topic.content.contains("今天天气怎么样"));

                std::fs::remove_file(path).expect("应清理测试记忆文件");
            });
    }
}
