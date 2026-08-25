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
// 持久化角色目标与统一动作执行器
mod agent_runtime;
// 通用持久化 Agent Run Runtime
mod agent_runs;
// 持久化跨群问答闭环任务
mod agent_tasks;
// 核心模型处理模块
mod group_access;
mod image_security;
mod model;
mod private_image_memory;
mod redis_store;
pub(crate) mod reminders;
mod vision;
mod vision_router;
pub(crate) mod yunxi;
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
const CORE_PRIVATE_CUTOVER_ENV: &str = "YUNXI_CORE_PRIVATE_CUTOVER";

/// Exactly one runtime owns a private message. Core takeover stays opt-in
/// until the new path has parity with the mature private-message pipeline.
/// This makes rollback a process restart with the flag removed and, more
/// importantly, prevents an accepted shadow event from creating a second
/// reply beside the legacy handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateMessageOwner {
    Legacy,
    Core,
    Dropped,
}

fn core_private_cutover_enabled_from(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on"
        )
    })
}

fn core_private_cutover_enabled() -> bool {
    core_private_cutover_enabled_from(
        std::env::var_os(CORE_PRIVATE_CUTOVER_ENV)
            .as_deref()
            .and_then(std::ffi::OsStr::to_str),
    )
}

/// Keep host-specific private features on the mature path even if the bridge
/// later broadens its own event admission. The Core canary currently accepts
/// only non-command text and cannot preserve image/sticker semantics.
fn core_private_canary_payload_is_safe(
    message: &kovi::Message,
    text: Option<&str>,
    sender_is_admin: bool,
) -> bool {
    message.iter().all(|segment| segment.type_ == "text")
        && text.is_some_and(|text| {
            let text = text.trim();
            !text.is_empty()
                && !text.starts_with('#')
                && !private_text_requires_legacy(text, sender_is_admin)
        })
}

/// Requests backed by mature host tools stay on the legacy pipeline until
/// Core has equivalent declarative intents. Administrator conversations are
/// kept there wholesale because their natural-language control surface also
/// includes cross-group sends, health checks, and persistent task management.
fn private_text_requires_legacy(text: &str, sender_is_admin: bool) -> bool {
    if sender_is_admin
        || crate::reminders::looks_like_reminder_request(text)
        || crate::agent_runs::looks_like_agent_run_request(text)
    {
        return true;
    }

    // The creation detectors intentionally reject cancellation/status phrases,
    // because those must not force a create call. They still require the
    // legacy tool-capable path, though, so keep a conservative routing guard.
    if text.contains("提醒") || text.contains("定时") {
        return true;
    }

    let lower = text.to_ascii_lowercase();
    if lower.contains("agent run") || lower.contains("agent-run") {
        return true;
    }
    let agent_run_control = [
        "查看",
        "列出",
        "列表",
        "有哪些",
        "状态",
        "进度",
        "取消",
        "删除",
        "停止",
        "不用",
        "不要",
        "别",
    ]
    .iter()
    .any(|marker| text.contains(marker));
    let agent_run_target = ["监控", "监测", "轮询", "盯着", "盯一下"]
        .iter()
        .any(|marker| text.contains(marker));
    agent_run_control && agent_run_target
}

/// Select a single owner at the Kovi ingress boundary.
///
/// Unsupported events always stay with the legacy handler. Even during an
/// explicit Core canary, an ingress rejection falls back immediately instead
/// of silently losing the user's message. Once Core accepts the event, legacy
/// must not run as well because both paths can send a visible reply.
async fn select_private_message_owner<Interrupt, InterruptFuture>(
    core_cutover_enabled: bool,
    core_supports_event: bool,
    interrupt_core: Interrupt,
    enqueue_core: impl FnOnce() -> yunxi::bridge::EnqueueOutcome,
) -> PrivateMessageOwner
where
    Interrupt: FnOnce() -> InterruptFuture,
    InterruptFuture: std::future::Future<Output = ()>,
{
    if !core_cutover_enabled {
        // The backend is observe-only for direct MessageReceived events while
        // cutover is disabled, so shadowing cannot create a duplicate reply.
        if core_supports_event && enqueue_core() == yunxi::bridge::EnqueueOutcome::Blocked {
            return PrivateMessageOwner::Dropped;
        }
        return PrivateMessageOwner::Legacy;
    }
    if !core_supports_event {
        return PrivateMessageOwner::Legacy;
    }

    // Invalidate an in-flight legacy or Core model call before this message is
    // admitted. If ingress then rejects the message, the legacy fallback below
    // claims a fresh ticket through its normal pipeline.
    interrupt_core().await;
    match enqueue_core() {
        yunxi::bridge::EnqueueOutcome::Accepted => PrivateMessageOwner::Core,
        yunxi::bridge::EnqueueOutcome::DroppedAtCapacity
        | yunxi::bridge::EnqueueOutcome::SkippedInvalid => PrivateMessageOwner::Legacy,
        yunxi::bridge::EnqueueOutcome::Blocked => PrivateMessageOwner::Dropped,
    }
}

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

    if let Err(error) = yunxi::initialize_database().await {
        panic!("Yunxi identity mapping 表初始化失败，拒绝写入 readiness: {error}");
    }

    if let Err(error) = reminders::initialize_database().await {
        panic!("提醒任务表初始化失败，拒绝写入 readiness: {error}");
    }

    if let Err(error) = agent_runtime::initialize_database().await {
        panic!("角色目标表初始化失败，拒绝写入 readiness: {error}");
    }

    if let Err(error) = agent_runs::initialize_database().await {
        panic!("Agent Run 表初始化失败，拒绝写入 readiness: {error}");
    }

    if let Err(error) = agent_tasks::initialize_database().await {
        panic!("跨群问答任务表初始化失败，拒绝写入 readiness: {error}");
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
    let proactive_bot = PluginBuilder::get_runtime_bot();
    if let Err(error) = group_access::initialize(&proactive_bot).await {
        panic!("群聊白名单 PostgreSQL 初始化失败，拒绝写入 readiness: {error}");
    }

    let yunxi_bridge = yunxi::bridge::ShadowBridge::start_with_open_loops_and_actions(
        yunxi::identity_store().expect("Yunxi identity store must be initialized before handlers"),
        yunxi::open_loop_store()
            .expect("Yunxi open-loop store must be initialized before handlers"),
        Arc::clone(&proactive_bot),
    );
    yunxi::install_shadow_bridge(Arc::clone(&yunxi_bridge))
        .expect("Yunxi ShadowBridge must be installed exactly once");
    let group_bridge = Arc::clone(&yunxi_bridge);
    let private_bridge = Arc::clone(&yunxi_bridge);
    let group_bot = Arc::clone(&proactive_bot);
    let private_bot = Arc::clone(&proactive_bot);
    let core_private_cutover = core_private_cutover_enabled();
    println!(
        "[INFO] 私聊回复所有者: {} ({CORE_PRIVATE_CUTOVER_ENV}=1 可启用 Core canary)",
        if core_private_cutover {
            "Yunxi Core canary"
        } else {
            "legacy"
        }
    );
    let group_message = move |event: Arc<kovi::event::GroupMsgEvent>| {
        let bridge = Arc::clone(&group_bridge);
        let bot = Arc::clone(&group_bot);
        bridge.enqueue_group(&event);
        async move {
            group_message_event(event, bot).await;
        }
    };
    let private_message = move |event: Arc<kovi::event::PrivateMsgEvent>| {
        let bridge = Arc::clone(&private_bridge);
        let bot = Arc::clone(&private_bot);
        let user_id = event.user_id;
        let confirmed_data_erasure = event
            .borrow_text()
            .is_some_and(|text| text.trim() == "#删除我的数据 确认");
        let data_erasure_token = confirmed_data_erasure
            .then(|| bridge.capture_private_data_erasure(user_id))
            .flatten();
        let handler_token = (!confirmed_data_erasure)
            .then(|| bridge.capture_private_handler(user_id))
            .flatten();
        let sender_is_admin = crate::model::utils::is_bot_admin(&bot, event.user_id);
        let core_supports_event = core_private_canary_payload_is_safe(
            &event.message,
            event.borrow_text(),
            sender_is_admin,
        ) && bridge.handles_private(&event);
        async move {
            if confirmed_data_erasure {
                let Some(token) = data_erasure_token else {
                    println!("[INFO] 私聊数据删除已在等待或执行，丢弃重复确认 (用户: {user_id})");
                    return;
                };
                let Some(_permit) = token.enter().await else {
                    println!("[INFO] 私聊数据删除确认已过期，停止执行 (用户: {user_id})");
                    return;
                };
                private_message_event(event, bot).await;
                return;
            }
            let Some(token) = handler_token else {
                println!("[INFO] 私聊数据删除屏障期间丢弃入站 (用户: {user_id})");
                return;
            };
            let Some(_permit) = token.enter().await else {
                println!("[INFO] 私聊数据删除 epoch 已推进，丢弃旧入站 (用户: {user_id})");
                return;
            };
            if bridge.is_user_blocked(user_id) {
                println!("[INFO] 私聊数据删除屏障期间丢弃入站 (用户: {user_id})");
                return;
            }
            let owner = select_private_message_owner(
                core_private_cutover,
                core_supports_event,
                || async move {
                    let _ =
                        crate::model::interrupt(crate::model::ReplyScope::Private(user_id)).await;
                },
                || bridge.enqueue_private(&event),
            )
            .await;
            if owner == PrivateMessageOwner::Legacy && !bridge.is_user_blocked(user_id) {
                private_message_event(event, bot).await;
            }
        }
    };

    // 注册群聊消息处理器
    PluginBuilder::on_group_msg(group_message);
    // 注册私聊消息处理器
    PluginBuilder::on_private_msg(private_message);

    // 插件启动即启动主动消息循环，不需要等待第一条群聊或私聊事件。
    let recall_bot = Arc::clone(&proactive_bot);
    PluginBuilder::on_notice({
        move |event| {
            let bot = Arc::clone(&recall_bot);
            async move {
                recall_notice_event(event, bot).await;
            }
        }
    });
    if proactive_chat::startup::get_or_create_proactive_manager_with_bridge(
        Arc::clone(&proactive_bot),
        Some(Arc::clone(&yunxi_bridge)),
    )
    .await
    .is_some()
    {
        println!("[INFO] 主动消息管理器已启动");
    }

    // 确保后台任务只启动一次
    if BACKGROUND_TASK_STARTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        let reminder_bot = Arc::clone(&proactive_bot);
        kovi::tokio::spawn(async move {
            reminders::start_scheduler(reminder_bot).await;
        });

        let agent_task_bot = Arc::clone(&proactive_bot);
        kovi::tokio::spawn(async move {
            agent_tasks::start_scheduler(agent_task_bot).await;
        });

        let agent_run_bot = Arc::clone(&proactive_bot);
        kovi::tokio::spawn(async move {
            agent_runs::start_scheduler(agent_run_bot).await;
        });

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
                match agent_runtime::compact_expired().await {
                    Ok(removed) if removed > 0 => {
                        println!("[INFO] 过期角色目标清理完成，移除 {} 条", removed);
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("[ERROR] 定期角色目标清理失败: {}", error),
                }
                match agent_tasks::compact_expired().await {
                    Ok(removed) if removed > 0 => {
                        println!("[INFO] 过期跨群问答任务清理完成，移除 {} 条", removed);
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("[ERROR] 定期跨群问答任务清理失败: {}", error),
                }
                match agent_runs::compact_expired().await {
                    Ok(removed) if removed > 0 => {
                        println!("[INFO] 过期 Agent Run 清理完成，移除 {} 条", removed);
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("[ERROR] 定期 Agent Run 清理失败: {}", error),
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
    use super::{
        PrivateMessageOwner, clear_ready_marker, core_private_canary_payload_is_safe,
        core_private_cutover_enabled_from, select_private_message_owner, write_ready_marker,
    };
    use crate::yunxi::bridge::EnqueueOutcome;
    use kovi::bot::message::{Message, Segment};
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;

    #[test]
    fn private_core_cutover_requires_an_explicit_truthy_value() {
        assert!(!core_private_cutover_enabled_from(None));
        assert!(!core_private_cutover_enabled_from(Some("")));
        assert!(!core_private_cutover_enabled_from(Some("0")));
        assert!(!core_private_cutover_enabled_from(Some("false")));
        assert!(core_private_cutover_enabled_from(Some("1")));
        assert!(core_private_cutover_enabled_from(Some(" TRUE ")));
        assert!(core_private_cutover_enabled_from(Some("on")));
    }

    #[test]
    fn private_messages_default_to_legacy_while_shadowing_core_once() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        let interrupt_calls = Cell::new(0);
        let enqueue_calls = Cell::new(0);
        let owner = runtime.block_on(select_private_message_owner(
            false,
            true,
            || async {
                interrupt_calls.set(interrupt_calls.get() + 1);
            },
            || {
                enqueue_calls.set(enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));

        assert_eq!(owner, PrivateMessageOwner::Legacy);
        assert_eq!(interrupt_calls.get(), 0);
        assert_eq!(enqueue_calls.get(), 1);

        let unsupported_interrupt_calls = Cell::new(0);
        let unsupported_enqueue_calls = Cell::new(0);
        let owner = runtime.block_on(select_private_message_owner(
            false,
            false,
            || async {
                unsupported_interrupt_calls.set(unsupported_interrupt_calls.get() + 1);
            },
            || {
                unsupported_enqueue_calls.set(unsupported_enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));
        assert_eq!(owner, PrivateMessageOwner::Legacy);
        assert_eq!(unsupported_interrupt_calls.get(), 0);
        assert_eq!(unsupported_enqueue_calls.get(), 0);
    }

    #[test]
    fn commands_and_non_text_private_events_stay_on_legacy() {
        let command = Message::from("#删除我的数据");
        assert!(!core_private_canary_payload_is_safe(
            &command,
            Some("#删除我的数据"),
            false,
        ));

        let non_text = Message::from(vec![
            Segment::new("text", json!({"text": "看看这张图"})),
            Segment::new("image", json!({"url": "https://example.test/image.png"})),
        ]);
        assert!(!core_private_canary_payload_is_safe(
            &non_text,
            Some("看看这张图"),
            false,
        ));

        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        for unsupported_event in ["command", "non-text", "bridge-rejected"] {
            let interrupt_calls = Cell::new(0);
            let enqueue_calls = Cell::new(0);
            let owner = runtime.block_on(select_private_message_owner(
                true,
                false,
                || async {
                    interrupt_calls.set(interrupt_calls.get() + 1);
                },
                || {
                    enqueue_calls.set(enqueue_calls.get() + 1);
                    EnqueueOutcome::Accepted
                },
            ));

            assert_eq!(owner, PrivateMessageOwner::Legacy, "{unsupported_event}");
            assert_eq!(interrupt_calls.get(), 0, "{unsupported_event}");
            assert_eq!(enqueue_calls.get(), 0, "{unsupported_event}");
        }
    }

    #[test]
    fn private_host_features_and_admins_stay_on_legacy() {
        for text in [
            "明天早上提醒我吃饭",
            "取消定时任务 3",
            "查看我的提醒列表",
            "每隔30秒请求一下 https://example.com/health，直到返回 ready 之后告诉我",
            "查看接口监控任务状态",
            "停止监控这个链接",
        ] {
            let message = Message::from(text);
            assert!(
                !core_private_canary_payload_is_safe(&message, Some(text), false),
                "{text}"
            );
        }

        let ordinary_admin_message = Message::from("今天过得怎么样");
        assert!(!core_private_canary_payload_is_safe(
            &ordinary_admin_message,
            Some("今天过得怎么样"),
            true,
        ));

        let ordinary_non_admin_message = Message::from("今天过得怎么样");
        assert!(core_private_canary_payload_is_safe(
            &ordinary_non_admin_message,
            Some("今天过得怎么样"),
            false,
        ));
    }

    #[test]
    fn core_ingress_failure_falls_back_to_legacy() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        for outcome in [
            EnqueueOutcome::DroppedAtCapacity,
            EnqueueOutcome::SkippedInvalid,
        ] {
            let order = Cell::new(0);
            let owner = runtime.block_on(select_private_message_owner(
                true,
                true,
                || async {
                    assert_eq!(order.get(), 0);
                    order.set(1);
                },
                || {
                    assert_eq!(order.get(), 1);
                    order.set(2);
                    outcome
                },
            ));
            assert_eq!(owner, PrivateMessageOwner::Legacy, "{outcome:?}");
            assert_eq!(order.get(), 2, "{outcome:?}");
        }
    }

    #[test]
    fn data_erasure_block_never_falls_back_to_legacy_private_handling() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        for core_cutover_enabled in [false, true] {
            let interrupt_calls = Cell::new(0);
            let enqueue_calls = Cell::new(0);
            let owner = runtime.block_on(select_private_message_owner(
                core_cutover_enabled,
                true,
                || async {
                    interrupt_calls.set(interrupt_calls.get() + 1);
                },
                || {
                    enqueue_calls.set(enqueue_calls.get() + 1);
                    EnqueueOutcome::Blocked
                },
            ));

            assert_eq!(owner, PrivateMessageOwner::Dropped);
            assert_eq!(enqueue_calls.get(), 1);
            assert_eq!(interrupt_calls.get(), usize::from(core_cutover_enabled));
        }
    }

    #[test]
    fn accepted_core_canary_has_exactly_one_reply_owner() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        let order = Cell::new(0);
        let enqueue_calls = Cell::new(0);
        let owner = runtime.block_on(select_private_message_owner(
            true,
            true,
            || async {
                assert_eq!(order.get(), 0);
                order.set(1);
            },
            || {
                assert_eq!(order.get(), 1);
                order.set(2);
                enqueue_calls.set(enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));

        assert_eq!(owner, PrivateMessageOwner::Core);
        assert_eq!(order.get(), 2);
        assert_eq!(enqueue_calls.get(), 1);
        assert_ne!(owner, PrivateMessageOwner::Legacy);
    }

    #[test]
    fn core_takeover_invalidates_the_existing_private_reply_generation() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        runtime.block_on(async {
            let scope = crate::model::ReplyScope::Private(9_200_001);
            let previous = crate::model::interrupt(scope).await;
            assert!(crate::model::is_current(previous).await);

            let owner = select_private_message_owner(
                true,
                true,
                || async {
                    let _ = crate::model::interrupt(scope).await;
                },
                || EnqueueOutcome::Accepted,
            )
            .await;

            assert_eq!(owner, PrivateMessageOwner::Core);
            assert!(!crate::model::is_current(previous).await);
        });
    }

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
