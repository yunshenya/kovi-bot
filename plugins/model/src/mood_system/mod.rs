use crate::memory::{MemoryManager, BotPersonality};
use chrono::{Duration, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use anyhow::Result;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub enum Mood {
    Happy,      // 开心
    Sad,        // 难过
    Angry,      // 生气
    Excited,    // 兴奋
    Calm,       // 平静
    Curious,    // 好奇
    Playful,    // 顽皮
    Thoughtful, // 深思
    Lonely,     // 孤独
    Confident,  // 自信
    Shy,        // 害羞
    Neutral,    // 中性
}

impl Mood {
    pub fn to_string(&self) -> String {
        match self {
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
        }.to_string()
    }

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

pub struct MoodSystem {
    memory_manager: Arc<MemoryManager>,
}

impl MoodSystem {
    pub fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self { memory_manager }
    }

    pub async fn analyze_and_update_mood(&self, message: &str, context: &str) -> Result<Mood> {
        let current_personality = self.memory_manager.get_bot_personality().await;
        let new_mood = self.analyze_mood_from_message(message, context, &current_personality).await;
        
        // 更新机器人人格
        let mut updated_personality = current_personality;
        updated_personality.current_mood = new_mood.to_string();
        updated_personality.last_mood_change = Local::now();
        
        // 根据情绪调整其他属性
        self.adjust_personality_traits(&mut updated_personality, &new_mood);
        
        self.memory_manager.update_bot_personality(updated_personality).await?;
        
        Ok(new_mood)
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
        let final_mood = self.combine_mood_analysis(mood_scores, context_mood, current_personality);

        final_mood
    }

    fn calculate_mood_scores(&self, message: &str) -> std::collections::HashMap<Mood, i32> {
        let mut scores = std::collections::HashMap::new();
        
        // 初始化所有情绪分数
        for mood in [Mood::Happy, Mood::Sad, Mood::Angry, Mood::Excited, Mood::Calm, 
                     Mood::Curious, Mood::Playful, Mood::Thoughtful, Mood::Lonely, 
                     Mood::Confident, Mood::Shy, Mood::Neutral] {
            scores.insert(mood, 0);
        }

        // 开心关键词
        let happy_keywords = ["开心", "高兴", "快乐", "哈哈", "😊", "😄", "好棒", "太好了", "喜欢"];
        for keyword in &happy_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Happy).unwrap() += 2;
            }
        }

        // 难过关键词
        let sad_keywords = ["难过", "伤心", "哭", "😢", "😭", "糟糕", "不好", "讨厌"];
        for keyword in &sad_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Sad).unwrap() += 2;
            }
        }

        // 生气关键词
        let angry_keywords = ["生气", "愤怒", "讨厌", "烦", "😠", "😡", "气死"];
        for keyword in &angry_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Angry).unwrap() += 2;
            }
        }

        // 兴奋关键词
        let excited_keywords = ["兴奋", "激动", "太棒了", "哇", "！", "!!!", "😆", "😃"];
        for keyword in &excited_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Excited).unwrap() += 2;
            }
        }

        // 好奇关键词
        let curious_keywords = ["什么", "为什么", "怎么", "？", "???", "好奇", "想知道"];
        for keyword in &curious_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Curious).unwrap() += 1;
            }
        }

        // 顽皮关键词
        let playful_keywords = ["调皮", "顽皮", "哈哈", "嘿嘿", "😏", "😜", "开玩笑"];
        for keyword in &playful_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Playful).unwrap() += 1;
            }
        }

        // 深思关键词
        let thoughtful_keywords = ["思考", "想想", "觉得", "认为", "可能", "也许"];
        for keyword in &thoughtful_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Thoughtful).unwrap() += 1;
            }
        }

        // 孤独关键词
        let lonely_keywords = ["一个人", "孤单", "寂寞", "没人", "只有我"];
        for keyword in &lonely_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Lonely).unwrap() += 2;
            }
        }

        // 自信关键词
        let confident_keywords = ["肯定", "一定", "当然", "没问题", "我可以", "我能"];
        for keyword in &confident_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Confident).unwrap() += 1;
            }
        }

        // 害羞关键词
        let shy_keywords = ["害羞", "不好意思", "脸红", "😳", "尴尬"];
        for keyword in &shy_keywords {
            if message.contains(keyword) {
                *scores.get_mut(&Mood::Shy).unwrap() += 1;
            }
        }

        scores
    }

    fn analyze_context_mood(&self, context: &str) -> Option<Mood> {
        let context_lower = context.to_lowercase();
        
        if context_lower.contains("群聊") {
            Some(Mood::Playful) // 群聊时更顽皮
        } else if context_lower.contains("私聊") {
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
        if let Some(context_mood) = context_mood {
            if let Some(context_score) = mood_scores.get(&context_mood) {
                if *context_score > 0 {
                    best_mood = context_mood;
                }
            }
        }

        // 如果所有情绪得分都很低，保持当前情绪或转为中性
        if best_score == 0 {
            let current_mood = Mood::from_string(&current_personality.current_mood);
            return if current_personality.energy_level > 5 {
                current_mood
            } else {
                Mood::Neutral
            }
        }

        best_mood
    }

    fn adjust_personality_traits(&self, personality: &mut BotPersonality, mood: &Mood) {
        match mood {
            Mood::Happy | Mood::Excited => {
                personality.energy_level = (personality.energy_level + 1).min(10);
                personality.social_confidence = (personality.social_confidence + 1).min(10);
            },
            Mood::Sad | Mood::Lonely => {
                personality.energy_level = personality.energy_level.saturating_sub(1);
                personality.social_confidence = personality.social_confidence.saturating_sub(1);
            },
            Mood::Angry => {
                personality.energy_level = (personality.energy_level + 1).min(10);
                personality.social_confidence = personality.social_confidence.saturating_sub(1);
            },
            Mood::Calm | Mood::Thoughtful => {
                personality.energy_level = personality.energy_level.saturating_sub(1);
                personality.curiosity_level = (personality.curiosity_level + 1).min(10);
            },
            Mood::Curious => {
                personality.curiosity_level = (personality.curiosity_level + 2).min(10);
            },
            Mood::Playful => {
                personality.energy_level = (personality.energy_level + 1).min(10);
                personality.social_confidence = (personality.social_confidence + 1).min(10);
            },
            Mood::Confident => {
                personality.social_confidence = (personality.social_confidence + 2).min(10);
            },
            Mood::Shy => {
                personality.social_confidence = personality.social_confidence.saturating_sub(2);
            },
            _ => {}
        }
    }

    pub async fn get_mood_based_response_style(&self) -> String {
        let personality = self.memory_manager.get_bot_personality().await;
        let mood = Mood::from_string(&personality.current_mood);
        
        match mood {
            Mood::Happy => "开心地".to_string(),
            Mood::Sad => "有点难过地".to_string(),
            Mood::Angry => "有点生气地".to_string(),
            Mood::Excited => "兴奋地".to_string(),
            Mood::Calm => "平静地".to_string(),
            Mood::Curious => "好奇地".to_string(),
            Mood::Playful => "顽皮地".to_string(),
            Mood::Thoughtful => "深思地".to_string(),
            Mood::Lonely => "有点孤单地".to_string(),
            Mood::Confident => "自信地".to_string(),
            Mood::Shy => "害羞地".to_string(),
            Mood::Neutral => "".to_string(),
        }
    }

    pub async fn should_change_mood_naturally(&self) -> bool {
        let personality = self.memory_manager.get_bot_personality().await;
        let now = Local::now();
        let time_since_last_change = now.signed_duration_since(personality.last_mood_change);
        
        // 如果超过2小时没有情绪变化，考虑自然变化
        time_since_last_change > Duration::hours(2)
    }

    pub async fn natural_mood_drift(&self) -> Result<()> {
        if !self.should_change_mood_naturally().await {
            return Ok(());
        }

        let mut personality = self.memory_manager.get_bot_personality().await;
        
        // 根据当前时间和能量水平自然调整情绪
        let hour = Local::now().hour();
        let new_mood = match hour {
            6..=11 => Mood::Happy,      // 早晨开心
            12..=14 => Mood::Excited,   // 中午兴奋
            15..=17 => Mood::Curious,   // 下午好奇
            18..=20 => Mood::Playful,   // 傍晚顽皮
            21..=23 => Mood::Calm,      // 晚上平静
            0..=5 => Mood::Thoughtful,  // 深夜深思
            _ => Mood::Neutral,
        };

        personality.current_mood = new_mood.to_string();
        personality.last_mood_change = Local::now();
        
        self.memory_manager.update_bot_personality(personality).await?;
        
        Ok(())
    }
}
