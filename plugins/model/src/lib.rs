//! # Kovi Bot Model Plugin
//!
//! 这是一个基于Kovi框架的智能聊天机器人插件，具备以下核心功能：
//! - 长期记忆系统：智能存储和检索对话记忆
//! - 情绪化人格：根据对话内容动态调整情绪状态
//! - 主动聊天：基于情绪和社交信心主动发起对话
//! - 个性化体验：根据用户档案提供定制化回复
//! - 话题生成：智能生成相关话题促进互动
//! - 健康监控：实时监控系统状态和性能

use crate::model::{group_message_event, private_message_event, recall_notice_event};
use kovi::PluginBuilder;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// 配置管理模块
pub mod config;
// 核心模型处理模块
mod image_security;
mod model;
mod private_image_memory;
mod redis_store;
mod vision;
mod vision_router;
// 工具函数模块
mod utils;
// 记忆管理系统
pub mod memory;
// 话题生成器
pub mod topic_generator;
// 情绪系统
pub mod mood_system;
// 主动聊天功能
pub mod proactive_chat;
// 健康检查系统
pub mod health_check;
// PostgreSQL 表情包记忆库
pub(crate) mod sticker_memory;

#[cfg(feature = "integration-tests")]
#[doc(hidden)]
pub mod test_support {
    use crate::image_security::validate_remote_image_url;
    use crate::model::{ReplyScope, finish, interrupt, is_active, mark_active};
    use crate::redis_store;
    use std::time::Duration;

    pub fn accepts_public_image_url(raw_url: &str) -> bool {
        validate_remote_image_url(raw_url).is_ok()
    }

    pub async fn reply_ticket_generation_is_atomic() -> bool {
        let scope = ReplyScope::Private(9_900_001);
        let old = interrupt(scope).await;
        let _ = mark_active(old).await;
        let new = interrupt(scope).await;
        let _ = mark_active(new).await;
        finish(old).await;
        let new_reply_survived = is_active(scope).await;
        finish(new).await;
        new_reply_survived
    }

    pub async fn redis_runtime_round_trip() -> anyhow::Result<()> {
        let store = redis_store::get()
            .await
            .ok_or_else(|| anyhow::anyhow!("REDIS_URL 未指向可用 Redis"))?;
        let suffix = format!(
            "integration-black-box:{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        store
            .set_expiring_text(&suffix, "round-trip", Duration::from_secs(30))
            .await?;
        anyhow::ensure!(
            store.take_text(&suffix).await? == Some("round-trip".to_string()),
            "Redis 临时值往返失败"
        );
        anyhow::ensure!(
            store
                .increment_expiring(&suffix, Duration::from_secs(30))
                .await?
                == 1,
            "Redis 计数器第一次递增失败"
        );
        anyhow::ensure!(
            store
                .increment_expiring(&suffix, Duration::from_secs(30))
                .await?
                == 2,
            "Redis 计数器第二次递增失败"
        );
        store.delete(&suffix).await?;
        Ok(())
    }
}

/// 后台任务启动标志，确保只启动一次
static BACKGROUND_TASK_STARTED: AtomicBool = AtomicBool::new(false);
const DATABASE_INIT_MAX_ATTEMPTS: u32 = 8;

/// 插件主入口函数
///
/// 初始化所有必要的组件并注册消息处理函数：
/// - 注册群聊和私聊消息处理函数
/// - 启动记忆管理器
/// - 初始化情绪系统
/// - 启动后台定期任务（自然情绪变化）
///
/// 主动聊天功能在插件初始化时启动。
#[kovi::plugin]
async fn main() {
    clear_ready_marker();
    // 数据库必须先加载完成，避免第一条消息在旧记忆恢复前被处理。
    for attempt in 1..=DATABASE_INIT_MAX_ATTEMPTS {
        match memory::MEMORY_MANAGER.initialize_database().await {
            Ok(()) => break,
            Err(error) if attempt == DATABASE_INIT_MAX_ATTEMPTS => {
                panic!(
                    "PostgreSQL 记忆存储连续 {} 次初始化失败，终止进程交由服务管理器重启: {}",
                    DATABASE_INIT_MAX_ATTEMPTS, error
                );
            }
            Err(error) => {
                let delay_secs = (1_u64 << (attempt - 1).min(5)).min(30);
                eprintln!(
                    "[ERROR] PostgreSQL 记忆存储初始化失败，第 {}/{} 次，{} 秒后重试: {}",
                    attempt, DATABASE_INIT_MAX_ATTEMPTS, delay_secs, error
                );
                kovi::tokio::time::sleep(kovi::tokio::time::Duration::from_secs(delay_secs)).await;
            }
        }
    }

    if let Err(error) = sticker_memory::initialize_database().await {
        panic!("表情包记忆库初始化失败，拒绝写入 readiness: {error}");
    }

    // Redis 只承载可丢失的运行态；连接失败时各模块会继续使用本地兜底。
    redis_store::initialize().await;

    if let Err(error) = model::tool_access::initialize().await {
        eprintln!(
            "[ERROR] 模型工具初始化失败，工具调用功能暂不可用: {}",
            error
        );
    }

    if let Err(error) = redis_store::check_readiness().await {
        panic!("Redis readiness 检查失败，拒绝写入 readiness: {error}");
    }

    // 注册聊天功能宏，定义消息处理函数映射
    register_chat_function! {
        (group_message, group_message_event),
        (private_message, private_message_event)
    }

    // 注册群聊消息处理器
    PluginBuilder::on_group_msg(group_message);
    // 注册私聊消息处理器
    PluginBuilder::on_private_msg(private_message);

    // 插件启动即启动主动消息循环，不需要等待第一条群聊或私聊事件。
    let proactive_bot = PluginBuilder::get_runtime_bot();
    let recall_bot = Arc::clone(&proactive_bot);
    PluginBuilder::on_notice({
        move |event| {
            let bot = Arc::clone(&recall_bot);
            async move {
                recall_notice_event(event, bot).await;
            }
        }
    });
    if proactive_chat::startup::get_or_create_proactive_manager(proactive_bot)
        .await
        .is_some()
    {
        println!("[INFO] 随机主动消息管理器已启动");
    }

    // 确保后台任务只启动一次
    if BACKGROUND_TASK_STARTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        // 在后台异步任务中执行定期任务
        // 主动聊天循环由 startup 模块单独管理。
        kovi::tokio::spawn(async move {
            // 定期执行自然情绪变化
            loop {
                if let Err(e) = mood_system::MOOD_SYSTEM.natural_mood_drift().await {
                    eprintln!("[ERROR] 自然情绪变化失败: {}", e);
                }

                let check_interval = config::get().mood().natural_drift_check_secs();
                kovi::tokio::time::sleep(kovi::tokio::time::Duration::from_secs(check_interval))
                    .await;
            }
        });

        let health_memory_manager = Arc::clone(&memory::MEMORY_MANAGER);
        kovi::tokio::spawn(async move {
            let mut health_checker = health_check::HealthChecker::new(health_memory_manager);
            health_checker.start_health_monitoring().await;
        });

        let maintenance_memory_manager = Arc::clone(&memory::MEMORY_MANAGER);
        kovi::tokio::spawn(async move {
            loop {
                if let Err(error) = maintenance_memory_manager.compact_memories().await {
                    eprintln!("[ERROR] 定期记忆清理失败: {}", error);
                }
                match sticker_memory::compact_expired().await {
                    Ok(removed) if removed > 0 => {
                        println!("[INFO] 过期表情标签清理完成，移除 {} 条", removed);
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("[ERROR] 定期表情标签清理失败: {}", error),
                }
                let interval = config::get().memory().maintenance_interval_secs();
                kovi::tokio::time::sleep(kovi::tokio::time::Duration::from_secs(interval)).await;
            }
        });

        println!("[INFO] 后台任务已启动");
    }

    if let Err(error) = write_ready_marker() {
        panic!("插件初始化完成但无法写入 readiness 标记: {error}");
    }
    println!("[INFO] Kovi Bot 插件已就绪");
}

fn ready_file_path() -> Option<PathBuf> {
    std::env::var_os("KOVI_READY_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn clear_ready_marker() {
    let Some(path) = ready_file_path() else {
        return;
    };
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "[WARN] 清理旧 readiness 标记失败 ({}): {}",
            path.display(),
            error
        );
    }
}

fn write_ready_marker() -> std::io::Result<()> {
    let Some(path) = ready_file_path() else {
        return Ok(());
    };
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let revision = std::env::var("KOVI_DEPLOY_REVISION").unwrap_or_else(|_| "ready".to_string());
    let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&temp_path, format!("{}\n", revision.trim()))?;
    std::fs::rename(temp_path, path)
}

#[cfg(test)]
mod tests {
    use super::{clear_ready_marker, write_ready_marker};
    use std::fs;

    #[test]
    fn readiness_marker_contains_the_deployed_revision() {
        let path = std::env::temp_dir().join(format!(
            "kovi-ready-{}-{}.txt",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        unsafe {
            std::env::set_var("KOVI_READY_FILE", &path);
            std::env::set_var("KOVI_DEPLOY_REVISION", "test-revision");
        }
        clear_ready_marker();
        write_ready_marker().expect("应写入 readiness 标记");
        assert_eq!(
            fs::read_to_string(&path).expect("应读取 readiness 标记"),
            "test-revision\n"
        );
        clear_ready_marker();
        unsafe {
            std::env::remove_var("KOVI_READY_FILE");
            std::env::remove_var("KOVI_DEPLOY_REVISION");
        }
    }
}
