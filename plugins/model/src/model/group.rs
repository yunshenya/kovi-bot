use crate::config;
use crate::health_check::HealthChecker;
use crate::memory::{GroupProfile, MEMORY_MANAGER};
use crate::model::utils::{send_sys_info, silence};
use chrono::Local;
use kovi::RuntimeBot;
use kovi::event::GroupMsgEvent;
use std::sync::Arc;
use std::time::Duration;

pub async fn group_message_event(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    let group_id = event.group_id;
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let nickname = event.get_sender_nickname();
    let sender = format!("[{}] {}", time, nickname);
    if let Some(message) = event.borrow_text() {
        update_group_profile(group_id, event.user_id, message, &nickname).await;
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
                        "✅ 系统健康状态良好\n📊 记忆数量: {}\n👥 用户档案: {}\n🏢 群组档案: {}\n💾 记忆文件大小: {:.2}MB",
                        health_status.memory_usage.total_memories,
                        health_status.memory_usage.user_profiles,
                        health_status.memory_usage.group_profiles,
                        health_status.memory_usage.memory_file_size as f64 / 1024.0 / 1024.0
                    )
                } else if health_status.is_healthy {
                    format!(
                        "⚠️ 系统可以运行，但有警告\n{}\n📊 记忆数量: {}\n💾 记忆文件大小: {:.2}MB",
                        health_status.warnings.join("\n"),
                        health_status.memory_usage.total_memories,
                        health_status.memory_usage.memory_file_size as f64 / 1024.0 / 1024.0,
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
                // 群聊中只在被 @ 或明确叫名字时回复，普通消息仍用于群组档案与话题学习。
                if is_addressed_to_bot(&event, message) || matches!(message, "#禁言" | "#结束禁言")
                {
                    silence(group_id, message, bot, sender).await;
                }
            }
        }
    }
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

    // 更新群组档案
    if let Err(e) = MEMORY_MANAGER.update_group_profile(group_id, profile).await {
        eprintln!("[ERROR] 更新群组档案失败 (群组: {}): {}", group_id, e);
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
