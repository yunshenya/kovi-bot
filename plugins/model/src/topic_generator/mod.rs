use crate::memory::MemoryManager;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use anyhow::Result;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Topic {
    pub content: String,
    pub category: TopicCategory,
    pub mood_requirement: Option<String>,
    pub energy_level_required: u8,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum TopicCategory {
    Casual,      // 日常闲聊
    Deep,        // 深度话题
    Fun,         // 有趣话题
    Personal,    // 个人话题
    Current,     // 时事话题
    Creative,    // 创意话题
    Nostalgic,   // 怀旧话题
    Future,      // 未来话题
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
                mood_requirement: Some("warm".to_string()),
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
                mood_requirement: Some("creative".to_string()),
                energy_level_required: 7,
                tags: vec!["设计".to_string(), "城市".to_string()],
            },
        ]
    }

    pub async fn generate_topic(&self, group_id: Option<i64>, user_id: Option<i64>) -> Result<Option<Topic>> {
        let bot_personality = self.memory_manager.get_bot_personality().await;
        
        // 根据当前情绪和能量水平筛选合适的话题
        let suitable_templates: Vec<&TopicTemplate> = self.topic_templates
            .iter()
            .filter(|template| {
                // 检查情绪要求
                if let Some(required_mood) = &template.mood_requirement {
                    if bot_personality.current_mood != *required_mood {
                        return false;
                    }
                }
                
                // 检查能量水平要求
                template.energy_level_required <= bot_personality.energy_level
            })
            .collect();

        if suitable_templates.is_empty() {
            return Ok(None);
        }

        // 根据群组或用户的历史记录调整话题选择
        let selected_template = self.select_best_template(suitable_templates, group_id, user_id).await?;
        
        let topic = Topic {
            content: selected_template.template.clone(),
            category: selected_template.category.clone(),
            mood_requirement: selected_template.mood_requirement.clone(),
            energy_level_required: selected_template.energy_level_required,
            tags: selected_template.tags.clone(),
        };

        Ok(Some(topic))
    }

    async fn select_best_template(
        &self,
        templates: Vec<&TopicTemplate>,
        _group_id: Option<i64>,
        _user_id: Option<i64>,
    ) -> Result<TopicTemplate> {
        // 简单的随机选择，后续可以加入更复杂的逻辑
        let now = Local::now();
        let seed = now.timestamp() as usize;
        let index = seed % templates.len();
        
        Ok(templates[index].clone())
    }

    pub async fn generate_personalized_topic(&self, user_id: i64) -> Result<Option<Topic>> {
        // 获取用户档案
        if let Some(user_profile) = self.memory_manager.get_user_profile(user_id).await {
            // 根据用户兴趣生成个性化话题
            let personalized_topic = self.generate_topic_based_on_interests(&user_profile).await?;
            return Ok(personalized_topic);
        }
        
        // 如果没有用户档案，使用通用话题
        self.generate_topic(None, Some(user_id)).await
    }

    async fn generate_topic_based_on_interests(&self, user_profile: &crate::memory::UserProfile) -> Result<Option<Topic>> {
        let interests = &user_profile.interests;
        
        // 根据用户兴趣生成话题
        let interest_topics = vec![
            ("游戏", "最近在玩什么游戏？有什么好玩的推荐吗？"),
            ("音乐", "最近有什么好听的歌吗？"),
            ("电影", "有什么好看的电影推荐吗？"),
            ("读书", "最近在读什么书？有什么好书推荐吗？"),
            ("运动", "最近有做什么运动吗？"),
            ("美食", "最近有吃到什么好吃的东西吗？"),
            ("旅行", "最近有去哪里玩吗？"),
            ("学习", "最近在学什么新东西吗？"),
        ];

        for (interest, topic) in interest_topics {
            if interests.iter().any(|i| i.contains(interest)) {
                return Ok(Some(Topic {
                    content: topic.to_string(),
                    category: TopicCategory::Personal,
                    mood_requirement: None,
                    energy_level_required: 4,
                    tags: vec![interest.to_string()],
                }));
            }
        }

        Ok(None)
    }

    pub async fn should_initiate_conversation(&self, _group_id: Option<i64>, _user_id: Option<i64>) -> bool {
        let bot_personality = self.memory_manager.get_bot_personality().await;
        
        // 检查能量水平和社交信心
        if bot_personality.energy_level < 5 || bot_personality.social_confidence < 4 {
            return false;
        }

        // 检查最近是否有互动
        let recent_memories = self.memory_manager.get_recent_memories(10).await;
        let now = Local::now();
        let one_hour_ago = now - chrono::Duration::hours(1);
        
        let recent_activity = recent_memories
            .iter()
            .any(|memory| memory.timestamp > one_hour_ago);
        
        // 如果最近有活动，降低主动发起对话的概率
        if recent_activity {
            return bot_personality.curiosity_level > 7;
        }

        // 根据情绪决定是否主动发起对话
        match bot_personality.current_mood.as_str() {
            "happy" | "curious" | "playful" => true,
            "neutral" => bot_personality.curiosity_level > 6,
            _ => false,
        }
    }
}
