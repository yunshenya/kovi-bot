use crate::config;
use crate::health_check::HealthChecker;
use crate::memory::{GroupProfile, MEMORY_MANAGER};
use crate::model::utils::{
    learn_user_profile_from_message, requests_no_reply, send_sys_info, silence,
};
use chrono::Local;
use kovi::RuntimeBot;
use kovi::event::GroupMsgEvent;
use kovi::tokio::sync::Mutex;
use rand::Rng;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct GroupInterjectionState {
    eligible_messages_since_sample: u32,
    last_interjection: Option<Instant>,
    conversation_until: Option<Instant>,
}

/// 未点名接话只维护本地计数和冷却状态；不会为每一条群消息调用模型。
static GROUP_INTERJECTION_STATE: LazyLock<Mutex<HashMap<i64, GroupInterjectionState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn group_message_event(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id;
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let nickname = event.get_sender_nickname();
    let sender = format!("[{}] {}", time, nickname);
    if let Some(message) = event.borrow_text() {
        if requests_no_reply(message) {
            println!("[INFO] 群聊明确要求不回复 (群组: {})", group_id);
            return;
        }
        update_group_profile(group_id, event.user_id, message, &nickname).await;
        learn_user_profile_from_message(event.user_id, message, &nickname, false).await;
        match message {
            "#系统信息" => {
                send_sys_info(Arc::clone(&bot), group_id).await;
            }

            "#重载配置文件" => match config::reload_config_from_file() {
                Ok(_) => bot.send_group_msg(group_id, "配置重载成功"),
                Err(e) => bot.send_group_msg(group_id, format!("配置重载失败: {}", e)),
            },

            "#重载全部配置" => match config::reload_config() {
                Ok(_) => bot.send_group_msg(group_id, "全部配置文件重载成功"),
                Err(e) => bot.send_group_msg(group_id, format!("重载失败： {}", e)),
            },

            "#启用自动重载" => {
                if config::is_auto_reload_enabled() {
                    bot.send_group_msg(group_id, "自动重载已经启用");
                } else {
                    config::enable_auto_reload(Duration::from_secs(5));
                    bot.send_group_msg(group_id, "自动重载已启用，每5秒检查一次");
                }
            }

            "#禁用自动重载" => {
                if config::is_auto_reload_enabled() {
                    config::disable_auto_reload();
                    bot.send_group_msg(group_id, "自动重载已禁用");
                } else {
                    bot.send_group_msg(group_id, "自动重载未启用");
                }
            }

            "#检查配置变化" => match config::check_and_reload() {
                Ok(true) => bot.send_group_msg(group_id, "检测到配置变化，已自动重载"),
                Ok(false) => bot.send_group_msg(group_id, "配置文件无变化"),
                Err(e) => bot.send_group_msg(group_id, format!("检查配置失败: {}", e)),
            },

            "#自动重载状态" => {
                let status = if config::is_auto_reload_enabled() {
                    "已启用"
                } else {
                    "已禁用"
                };
                bot.send_group_msg(group_id, format!("配置自动重载状态: {}", status));
            }

            "#健康检查" => {
                let mut health_checker = HealthChecker::new(Arc::clone(&MEMORY_MANAGER));
                let health_status = health_checker.check_health().await;

                let status_msg = if health_status.is_healthy && health_status.warnings.is_empty() {
                    format!(
                        "✅ 系统健康状态良好\n📊 记忆数量: {}\n👥 用户档案: {}\n🏢 群组档案: {}\n💾 记忆快照大小: {:.2}MB",
                        health_status.memory_usage.total_memories,
                        health_status.memory_usage.user_profiles,
                        health_status.memory_usage.group_profiles,
                        health_status.memory_usage.storage_size_bytes as f64 / 1024.0 / 1024.0
                    )
                } else if health_status.is_healthy {
                    format!(
                        "⚠️ 系统可以运行，但有警告\n{}\n📊 记忆数量: {}\n💾 记忆快照大小: {:.2}MB",
                        health_status.warnings.join("\n"),
                        health_status.memory_usage.total_memories,
                        health_status.memory_usage.storage_size_bytes as f64 / 1024.0 / 1024.0,
                    )
                } else {
                    format!(
                        "❌ 系统健康状态异常\n错误: {}\n警告: {}",
                        health_status.errors.join(", "),
                        health_status.warnings.join(", ")
                    )
                };

                bot.send_group_msg(group_id, &status_msg);
            }
            _ => {
                // 被点名时始终处理；未点名消息仅由本地节流器偶尔抽样，不逐条调用模型。
                if is_addressed_to_bot(&event, message) || matches!(message, "#禁言" | "#结束禁言")
                {
                    if silence(group_id, message, bot, sender).await {
                        activate_conversation_window(group_id).await;
                    }
                } else if should_continue_conversation(group_id, message).await {
                    println!("[INFO] 群聊接续对话 (群组: {})", group_id);
                    if silence(group_id, message, bot, sender).await {
                        activate_conversation_window(group_id).await;
                    } else {
                        close_conversation_window(group_id).await;
                    }
                } else if should_interject(group_id, message).await {
                    println!("[INFO] 群聊未点名接话 (群组: {})", group_id);
                    if silence(group_id, message, bot, sender).await {
                        activate_conversation_window(group_id).await;
                    }
                } else if let Err(error) = MEMORY_MANAGER
                    .add_conversation_memory(
                        group_id,
                        &format!("{}: {}", nickname, message),
                        "group_observation",
                    )
                    .await
                {
                    eprintln!(
                        "[ERROR] 群聊观察记忆记录失败 (群组: {}): {}",
                        group_id, error
                    );
                }
            }
        }
    }
}

/// 机器人成功回复后开启或续期窗口，使用户可以不重复叫名字而继续对话。
async fn activate_conversation_window(group_id: i64) {
    let duration = Duration::from_secs(
        config::get()
            .group_interjection()
            .conversation_window_secs(),
    );
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    states.entry(group_id).or_default().conversation_until = Some(Instant::now() + duration);
}

async fn close_conversation_window(group_id: i64) {
    if let Some(state) = GROUP_INTERJECTION_STATE.lock().await.get_mut(&group_id) {
        state.conversation_until = None;
    }
}

/// 在窗口内，仅对本地判断像在继续聊天的消息调用模型；无关消息仍不消耗 token。
async fn should_continue_conversation(group_id: i64, message: &str) -> bool {
    if !is_conversation_follow_up(message) {
        return false;
    }

    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return false;
    };
    if has_active_conversation_window(state.conversation_until, Instant::now()) {
        true
    } else {
        state.conversation_until = None;
        false
    }
}

fn has_active_conversation_window(until: Option<Instant>, now: Instant) -> bool {
    until.is_some_and(|deadline| deadline > now)
}

fn is_conversation_follow_up(message: &str) -> bool {
    let text = message.trim();
    if text.is_empty() || text.starts_with('#') {
        return false;
    }

    let follow_up_cues = [
        "？",
        "?",
        "你",
        "吗",
        "谢谢",
        "好呀",
        "好的",
        "对啊",
        "是吗",
        "真的",
        "然后",
        "继续",
        "为什么",
        "怎么",
        "能不能",
        "我也",
        "哈哈",
        "嗯",
        "对",
        "好",
        "行",
        "可以",
    ];
    follow_up_cues.iter().any(|cue| text.contains(cue))
}

/// 仅用本地关键词、计数、冷却和概率筛选未点名接话机会；这里不会请求模型。
async fn should_interject(group_id: i64, message: &str) -> bool {
    let config = config::get().group_interjection().clone();
    if !config.enabled() || !is_interjection_candidate(message, config.min_message_chars()) {
        return false;
    }

    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let state = states.entry(group_id).or_default();
    if state
        .last_interjection
        .is_some_and(|last| last.elapsed() < Duration::from_secs(config.cooldown_secs()))
    {
        return false;
    }

    state.eligible_messages_since_sample = state.eligible_messages_since_sample.saturating_add(1);
    if state.eligible_messages_since_sample < config.min_eligible_messages() {
        return false;
    }
    // 每积累一批候选消息才抽样一次；未抽中也重新累计，避免逐条消耗 token。
    state.eligible_messages_since_sample = 0;
    if !rand::rng().random_ratio(config.response_probability_percent().into(), 100) {
        return false;
    }

    state.last_interjection = Some(Instant::now());
    true
}

fn is_interjection_candidate(message: &str, min_message_chars: usize) -> bool {
    let text = message.trim();
    if text.chars().count() < min_message_chars || text.starts_with('#') {
        return false;
    }

    let conversational_cues = [
        "？",
        "?",
        "怎么",
        "为什么",
        "有没有",
        "推荐",
        "你们觉得",
        "如何",
        "要不要",
        "求助",
        "哈哈",
        "笑死",
        "开心",
        "难过",
        "担心",
        "加油",
    ];
    conversational_cues.iter().any(|cue| text.contains(cue))
        || !extract_topics_from_message(text).is_empty()
}

fn is_addressed_to_bot(event: &GroupMsgEvent, message: &str) -> bool {
    let self_id = event.self_id.to_string();
    let mentioned = event.message.iter().any(|segment| {
        if segment.type_ != "at" {
            return false;
        }
        let qq = segment.data.get("qq");
        qq.and_then(|value| value.as_str()) == Some(self_id.as_str())
            || qq.and_then(|value| value.as_i64()) == Some(event.self_id)
    });
    let text = message.trim_start();
    mentioned || text.starts_with("芸汐") || text.starts_with("云汐")
}

async fn update_group_profile(group_id: i64, user_id: i64, message: &str, _nickname: &str) {
    let mut profile = MEMORY_MANAGER
        .get_group_profile(group_id)
        .await
        .unwrap_or_else(|| GroupProfile {
            group_id,
            group_name: format!("群组_{}", group_id),
            active_members: Vec::new(),
            group_personality: "friendly".to_string(),
            conversation_topics: Vec::new(),
            last_activity: Local::now(),
            activity_level: 1,
        });

    // 更新活动信息
    profile.last_activity = Local::now();
    profile.activity_level = (profile.activity_level + 1).min(10);
    if !profile.active_members.contains(&user_id) {
        profile.active_members.push(user_id);
        if profile.active_members.len() > 100 {
            profile.active_members.remove(0);
        }
    }

    // 提取话题关键词
    let topics = extract_topics_from_message(message);
    for topic in topics {
        if !profile.conversation_topics.contains(&topic) {
            profile.conversation_topics.push(topic);
        }
    }

    // 限制话题数量
    if profile.conversation_topics.len() > 20 {
        profile
            .conversation_topics
            .drain(0..profile.conversation_topics.len() - 20);
    }

    profile.group_personality = infer_group_personality(message, &profile.group_personality);

    // 更新群组档案
    if let Err(e) = MEMORY_MANAGER.update_group_profile(group_id, profile).await {
        eprintln!("[ERROR] 更新群组档案失败 (群组: {}): {}", group_id, e);
    }
}

fn infer_group_personality(message: &str, current: &str) -> String {
    if ["哈哈", "笑死", "好玩", "开心"]
        .iter()
        .any(|keyword| message.contains(keyword))
    {
        "lively".to_string()
    } else if ["技术", "代码", "编程", "论文", "学习"]
        .iter()
        .any(|keyword| message.contains(keyword))
    {
        "knowledgeable".to_string()
    } else if ["难过", "担心", "安慰", "加油"]
        .iter()
        .any(|keyword| message.contains(keyword))
    {
        "supportive".to_string()
    } else {
        current.to_string()
    }
}

fn extract_topics_from_message(message: &str) -> Vec<String> {
    let mut topics = Vec::new();
    let message_lower = message.to_lowercase();

    let topic_keywords = [
        (
            "游戏",
            vec!["游戏", "打游戏", "玩", "lol", "王者", "吃鸡", "steam"],
        ),
        ("学习", vec!["学习", "考试", "课程", "知识", "作业", "论文"]),
        ("工作", vec!["工作", "上班", "加班", "项目", "会议", "同事"]),
        ("生活", vec!["生活", "日常", "今天", "昨天", "明天", "计划"]),
        ("娱乐", vec!["电影", "音乐", "看书", "听歌", "追剧", "综艺"]),
        ("美食", vec!["吃", "美食", "餐厅", "料理", "做饭", "外卖"]),
        (
            "旅行",
            vec!["旅行", "旅游", "出去玩", "度假", "景点", "攻略"],
        ),
        ("运动", vec!["运动", "跑步", "健身", "锻炼", "瑜伽", "游泳"]),
        ("科技", vec!["科技", "AI", "编程", "技术", "互联网", "手机"]),
        ("情感", vec!["情感", "心情", "开心", "难过", "生气", "担心"]),
    ];

    for (category, keywords) in &topic_keywords {
        for keyword in keywords {
            if message_lower.contains(keyword) {
                topics.push(category.to_string());
                break;
            }
        }
    }

    topics
}

#[cfg(test)]
mod tests {
    use super::{
        extract_topics_from_message, has_active_conversation_window, infer_group_personality,
        is_conversation_follow_up, is_interjection_candidate,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn group_topics_and_personality_are_learned() {
        let topics = extract_topics_from_message("最近在学习 Rust 编程和 AI 技术");
        assert!(topics.contains(&"学习".to_string()));
        assert!(topics.contains(&"科技".to_string()));
        assert_eq!(
            infer_group_personality("一起讨论代码和技术吧", "friendly"),
            "knowledgeable"
        );
    }

    #[test]
    fn only_meaningful_unaddressed_messages_become_interjection_candidates() {
        assert!(is_interjection_candidate("你们觉得 Rust 好学吗？", 5));
        assert!(is_interjection_candidate("我今天有点难过", 5));
        assert!(!is_interjection_candidate("嗯", 5));
        assert!(!is_interjection_candidate("#某个命令", 5));
    }

    #[test]
    fn conversation_window_only_accepts_likely_follow_ups() {
        assert!(is_conversation_follow_up("那你觉得呢？"));
        assert!(is_conversation_follow_up("好呀，谢谢你"));
        assert!(is_conversation_follow_up("嗯，对呀"));
        assert!(!is_conversation_follow_up("[图片]"));
        assert!(!is_conversation_follow_up("#系统信息"));
    }

    #[test]
    fn conversation_window_remains_open_for_three_minutes_then_expires() {
        let opened_at = Instant::now();
        let deadline = opened_at + Duration::from_secs(180);

        assert!(has_active_conversation_window(Some(deadline), opened_at));
        assert!(has_active_conversation_window(
            Some(deadline),
            opened_at + Duration::from_secs(179)
        ));
        assert!(!has_active_conversation_window(Some(deadline), deadline));
        assert!(!has_active_conversation_window(None, opened_at));
    }
}
