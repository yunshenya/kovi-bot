//! # Kovi Bot Model Plugin
//!
//! 这是一个基于Kovi框架的智能聊天机器人插件，具备以下核心功能：
//! - 长期记忆系统：智能存储和检索对话记忆
//! - 情绪化人格：根据对话内容动态调整情绪状态
//! - 主动聊天：基于情绪和社交信心主动发起对话
//! - 个性化体验：根据用户档案提供定制化回复
//! - 话题生成：智能生成相关话题促进互动
//! - 健康监控：实时监控系统状态和性能

use crate::model::{
    ConversationCoordinator, group_message_event_after_ingress,
    private_message_event_after_ingress, recall_notice_event, record_group_message_observation,
    should_suppress_core_group_message,
};
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
#[cfg(test)]
pub(crate) mod database_test_support {
    use std::future::Future;
    use std::sync::OnceLock;

    // PostgreSQL integration tests share the process-global MemoryManager. A
    // long-lived runtime keeps its pool's sockets and maintenance tasks from
    // outliving the short runtime created by an individual test.
    static RUNTIME: OnceLock<kovi::tokio::runtime::Runtime> = OnceLock::new();

    pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
        RUNTIME
            .get_or_init(|| kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时"))
            .block_on(future)
    }
}
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

/// Entry point for the explicit offline Memory v2 migration binary.
#[doc(hidden)]
pub async fn run_memory_v2_migration_cli(args: Vec<String>) -> anyhow::Result<String> {
    yunxi::memory_migration::run_cli(args).await
}

/// Export canonical Yunxi person data without exposing the PostgreSQL host.
#[doc(hidden)]
pub async fn export_yunxi_person(person_id: uuid::Uuid) -> anyhow::Result<String> {
    yunxi::export_person_json(person_id).await
}

/// Import a previously exported canonical Yunxi person snapshot.
#[doc(hidden)]
pub async fn import_yunxi_person(payload: &str) -> anyhow::Result<uuid::Uuid> {
    yunxi::import_person_json(payload).await
}

/// Unlink an external identity while retaining the canonical person domain.
#[doc(hidden)]
pub async fn unlink_yunxi_identity(platform: &str, external_id: &str) -> anyhow::Result<bool> {
    yunxi::unlink_external_identity(platform, external_id).await
}
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

/// Exactly one runtime owns a message that may produce a visible reply. Core
/// observation-only group chatter is classified before this selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageOwner {
    Host,
    Core,
    Dropped,
}

/// Select a single owner at the Kovi ingress boundary.
///
/// Unsupported events stay with the specialized Host handler. Events that may
/// reply first obtain a shared admission: a stale prepared output is resolved
/// by the executive policy, while an already-generating reply stays on its
/// ticket until semantic refinement decides whether it still matters. Core
/// observation-only chatter never enters this function and cannot disturb an
/// active reply. Once Core is selected, the Host handler must not run as well
/// because both paths can send a visible reply.
#[cfg_attr(not(test), allow(dead_code))]
async fn select_message_owner<Interrupt, InterruptFuture, Admission>(
    core_supports_event: bool,
    interrupt_core: Interrupt,
    enqueue_core: impl FnOnce(&Admission) -> yunxi::bridge::EnqueueOutcome,
) -> (MessageOwner, Admission)
where
    Interrupt: FnOnce() -> InterruptFuture,
    InterruptFuture: std::future::Future<Output = Admission>,
{
    // This is the earliest common linearization point for supported and
    // Host-owned events. The exact admission is carried into the owner path;
    // later semantic refinement cannot revive a generation that was superseded.
    let admission = interrupt_core().await;

    if !core_supports_event {
        return (MessageOwner::Host, admission);
    }

    let owner = match enqueue_core(&admission) {
        yunxi::bridge::EnqueueOutcome::Accepted => MessageOwner::Core,
        yunxi::bridge::EnqueueOutcome::DroppedAtCapacity
        | yunxi::bridge::EnqueueOutcome::Blocked
        | yunxi::bridge::EnqueueOutcome::SkippedInvalid => MessageOwner::Dropped,
    };
    (owner, admission)
}

/// Select an owner when the admission may require Host-side serialization.
/// An already-generating reply cannot safely share its ticket with a Core
/// planner turn; Host's existing semantic queue is the single-owner path for
/// that case. Core still receives an observation from the Host branch.
#[cfg_attr(not(test), allow(dead_code))]
async fn select_message_owner_with_admission_policy<
    Interrupt,
    InterruptFuture,
    Admission,
    HostPolicy,
>(
    core_supports_event: bool,
    interrupt_core: Interrupt,
    host_policy: HostPolicy,
    enqueue_core: impl FnOnce(&Admission) -> yunxi::bridge::EnqueueOutcome,
) -> (MessageOwner, Admission)
where
    Interrupt: FnOnce() -> InterruptFuture,
    InterruptFuture: std::future::Future<Output = Admission>,
    HostPolicy: FnOnce(&Admission) -> bool,
{
    let admission = interrupt_core().await;
    if !core_supports_event || host_policy(&admission) {
        return (MessageOwner::Host, admission);
    }

    let owner = match enqueue_core(&admission) {
        yunxi::bridge::EnqueueOutcome::Accepted => MessageOwner::Core,
        yunxi::bridge::EnqueueOutcome::DroppedAtCapacity
        | yunxi::bridge::EnqueueOutcome::Blocked
        | yunxi::bridge::EnqueueOutcome::SkippedInvalid => MessageOwner::Dropped,
    };
    (owner, admission)
}

/// Select an owner for a visible event whose Core ingress may wait for
/// capacity. Observation-only traffic deliberately keeps the synchronous
/// selector above; a message that is expected to receive a reply must not be
/// discarded merely because the bounded bridge queue is temporarily full.
async fn select_message_owner_with_async_admission_policy<
    Interrupt,
    InterruptFuture,
    HostPolicy,
    Enqueue,
    EnqueueFuture,
>(
    core_supports_event: bool,
    interrupt_core: Interrupt,
    host_policy: HostPolicy,
    enqueue_core: Enqueue,
) -> (MessageOwner, crate::model::IncomingAdmission)
where
    Interrupt: FnOnce() -> InterruptFuture,
    InterruptFuture: std::future::Future<Output = crate::model::IncomingAdmission>,
    HostPolicy: FnOnce(&crate::model::IncomingAdmission) -> bool,
    Enqueue: FnOnce(&crate::model::IncomingAdmission) -> EnqueueFuture,
    EnqueueFuture: std::future::Future<Output = yunxi::bridge::EnqueueOutcome>,
{
    let mut admission_guard = IncomingAdmissionGuard::new(interrupt_core().await);
    if !core_supports_event || host_policy(&admission_guard.admission()) {
        return (MessageOwner::Host, admission_guard.take());
    }

    let owner = match enqueue_core(&admission_guard.admission()).await {
        yunxi::bridge::EnqueueOutcome::Accepted => MessageOwner::Core,
        yunxi::bridge::EnqueueOutcome::DroppedAtCapacity
        | yunxi::bridge::EnqueueOutcome::Blocked
        | yunxi::bridge::EnqueueOutcome::SkippedInvalid => MessageOwner::Dropped,
    };
    let admission = admission_guard.take();
    (owner, admission)
}

/// An async queue send can be cancelled by the host during shutdown. Keep the
/// conversation reservation recoverable in that case instead of leaking it
/// behind a permanently active reply ticket.
struct IncomingAdmissionGuard {
    admission: Option<crate::model::IncomingAdmission>,
}

impl IncomingAdmissionGuard {
    fn new(admission: crate::model::IncomingAdmission) -> Self {
        Self {
            admission: Some(admission),
        }
    }

    fn admission(&self) -> crate::model::IncomingAdmission {
        self.admission
            .expect("an armed incoming admission guard must carry its admission")
    }

    fn take(&mut self) -> crate::model::IncomingAdmission {
        self.admission
            .take()
            .expect("an armed incoming admission guard must carry its admission")
    }
}

impl Drop for IncomingAdmissionGuard {
    fn drop(&mut self) {
        let Some(admission) = self.admission.take() else {
            return;
        };
        if let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                ConversationCoordinator::abandon_incoming(admission).await;
            });
        }
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

    let yunxi_bridge = yunxi::bridge::CoreBridge::start_with_open_loops_and_actions(
        yunxi::identity_store().expect("Yunxi identity store must be initialized before handlers"),
        yunxi::open_loop_store()
            .expect("Yunxi open-loop store must be initialized before handlers"),
        Arc::clone(&proactive_bot),
    );
    yunxi::install_core_bridge(Arc::clone(&yunxi_bridge))
        .expect("Yunxi CoreBridge must be installed exactly once");
    let group_bridge = Arc::clone(&yunxi_bridge);
    let private_bridge = Arc::clone(&yunxi_bridge);
    let group_bot = Arc::clone(&proactive_bot);
    let private_bot = Arc::clone(&proactive_bot);
    let group_message = move |event: Arc<kovi::event::GroupMsgEvent>| {
        let bridge = Arc::clone(&group_bridge);
        let bot = Arc::clone(&group_bot);
        let group_id = event.group_id;
        let ingress_event = Arc::clone(&event);
        let confirmed_data_erasure = event
            .borrow_text()
            .is_some_and(|text| text.trim() == "#删除本群数据 确认")
            && crate::model::utils::is_bot_admin(&bot, event.user_id);
        let data_erasure_token = confirmed_data_erasure
            .then(|| bridge.capture_group_data_erasure(group_id))
            .flatten();
        let handler_token = (!confirmed_data_erasure)
            .then(|| bridge.capture_group_handler(group_id))
            .flatten();
        async move {
            if event.user_id == event.self_id {
                println!(
                    "[INFO] 忽略群聊自发消息回流 (群组: {}, 消息: {})",
                    group_id, event.message_id
                );
                return;
            }
            let (_handler_permit, _data_erasure_permit) = if confirmed_data_erasure {
                let Some(token) = data_erasure_token else {
                    println!("[INFO] 群数据删除已在等待或执行，丢弃重复确认 (群组: {group_id})");
                    return;
                };
                let Some(permit) = token.enter().await else {
                    println!("[INFO] 群数据删除确认 epoch 已推进，停止执行 (群组: {group_id})");
                    return;
                };
                (None, Some(permit))
            } else {
                let Some(token) = handler_token else {
                    println!("[INFO] 群数据删除屏障期间丢弃入站 (群组: {group_id})");
                    return;
                };
                let Some(permit) = token.enter().await else {
                    println!("[INFO] 群数据删除 epoch 已推进，丢弃旧入站 (群组: {group_id})");
                    return;
                };
                (Some(permit), None)
            };
            if bridge.is_group_blocked(group_id) {
                println!("[INFO] 群数据删除屏障期间丢弃入站 (群组: {group_id})");
                return;
            }
            let core_supported = bridge.supports_group(&event);
            if core_supported && !bridge.is_user_blocked(event.user_id) {
                // Preserve the Host-era Agent Task observation contract even
                // when Core traffic is throttled or its queue is full.
                record_group_message_observation(&event).await;
            }
            if core_supported && should_suppress_core_group_message(&event, &bot).await {
                println!(
                    "[INFO] Core 群聊入站流量已抑制 (群组: {}, 用户: {})",
                    group_id, event.user_id
                );
                return;
            }
            let group_decision = bridge.classify_group(&event).await;
            if group_decision.handling == yunxi::bridge::GroupCoreHandling::Observe {
                if !bridge.is_user_blocked(event.user_id) {
                    // No reply admission exists for this background turn.
                    let _ = bridge.enqueue_group_observation(&ingress_event);
                }
                return;
            }
            let core_supports_event =
                group_decision.handling == yunxi::bridge::GroupCoreHandling::Decide;
            let (owner, admission) = select_message_owner_with_async_admission_policy(
                core_supports_event,
                || async move {
                    ConversationCoordinator::begin_incoming(crate::model::ReplyScope::Group(
                        group_id,
                    ))
                    .await
                },
                |admission| admission.active_reply_preserved,
                |admission| {
                    bridge.enqueue_group_reliably(
                        &ingress_event,
                        *admission,
                        group_decision.replies_to_agent,
                    )
                },
            )
            .await;
            if owner == MessageOwner::Host {
                if !bridge.is_user_blocked(event.user_id) {
                    // Core observes unsupported events while the Host keeps the
                    // only visible reply permission for them.
                    let _ = bridge.enqueue_group_observation(&ingress_event);
                    if let Err(error) = bridge.flush_group_collisions(&ingress_event).await {
                        kovi::log::warn!(
                            "Yunxi group collision flush failed before Host handling: {error}"
                        );
                    }
                    group_message_event_after_ingress(event, bot, admission).await;
                }
                // The erasure barrier can close after owner selection. Host
                // still owns the admission in that race and must release it.
                ConversationCoordinator::abandon_incoming(admission).await;
            } else if owner == MessageOwner::Dropped {
                kovi::log::error!(
                    "Yunxi Core visible group event was not admitted: group_id={} user_id={} message_id={} action=drop",
                    group_id,
                    event.user_id,
                    event.message_id,
                );
                ConversationCoordinator::abandon_incoming(admission).await;
            }
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
        let core_supports_event = bridge.handles_private(&event);
        async move {
            if event.user_id == event.self_id {
                println!(
                    "[INFO] 忽略私聊自发消息回流 (用户: {}, 消息: {})",
                    user_id, event.message_id
                );
                return;
            }
            if confirmed_data_erasure {
                let admission = ConversationCoordinator::begin_incoming(
                    crate::model::ReplyScope::Private(user_id),
                )
                .await;
                let Some(token) = data_erasure_token else {
                    println!("[INFO] 私聊数据删除已在等待或执行，丢弃重复确认 (用户: {user_id})");
                    ConversationCoordinator::abandon_incoming(admission).await;
                    return;
                };
                let Some(_permit) = token.enter().await else {
                    println!("[INFO] 私聊数据删除确认已过期，停止执行 (用户: {user_id})");
                    ConversationCoordinator::abandon_incoming(admission).await;
                    return;
                };
                private_message_event_after_ingress(event, bot, admission).await;
                ConversationCoordinator::abandon_incoming(admission).await;
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
            let (owner, admission) = select_message_owner_with_async_admission_policy(
                core_supports_event,
                || async move {
                    ConversationCoordinator::begin_incoming(crate::model::ReplyScope::Private(
                        user_id,
                    ))
                    .await
                },
                |admission| admission.active_reply_preserved,
                |admission| bridge.enqueue_private_reliably(&event, *admission),
            )
            .await;
            if owner == MessageOwner::Host {
                if !bridge.is_user_blocked(user_id) {
                    let _ = bridge.enqueue_private_observation(&event);
                    if let Err(error) = bridge.flush_private_collisions(&event).await {
                        kovi::log::warn!(
                            "Yunxi private collision flush failed before Host handling: {error}"
                        );
                    }
                    private_message_event_after_ingress(event, bot, admission).await;
                }
                // A concurrently-started erasure may block the Host handler after
                // owner selection; the reservation cannot be left to expire.
                ConversationCoordinator::abandon_incoming(admission).await;
            } else if owner == MessageOwner::Dropped {
                kovi::log::error!(
                    "Yunxi Core visible private event was not admitted: user_id={} message_id={} action=drop",
                    user_id,
                    event.message_id,
                );
                ConversationCoordinator::abandon_incoming(admission).await;
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
                if let Some(store) = yunxi::mind_store() {
                    match store.cleanup(chrono::Utc::now()).await {
                        Ok(removed) if removed > 0 => {
                            println!("[INFO] Yunxi Mind 过期状态清理完成，移除 {} 条", removed);
                        }
                        Ok(_) => {}
                        Err(error) => eprintln!("[ERROR] Yunxi Mind 定期清理失败: {}", error),
                    }
                }
                yunxi::observe_mind_maintenance_tick();
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
        MessageOwner, clear_ready_marker, select_message_owner,
        select_message_owner_with_admission_policy, write_ready_marker,
    };
    use crate::yunxi::bridge::EnqueueOutcome;
    use std::cell::Cell;
    use std::fs;

    #[test]
    fn unsupported_messages_stay_on_the_host_path() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        let interrupt_calls = Cell::new(0);
        let enqueue_calls = Cell::new(0);
        let (owner, _) = runtime.block_on(select_message_owner(
            false,
            || async {
                interrupt_calls.set(interrupt_calls.get() + 1);
            },
            |_| {
                enqueue_calls.set(enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));

        assert_eq!(owner, MessageOwner::Host);
        assert_eq!(interrupt_calls.get(), 1);
        assert_eq!(enqueue_calls.get(), 0);

        let unsupported_interrupt_calls = Cell::new(0);
        let unsupported_enqueue_calls = Cell::new(0);
        let (owner, _) = runtime.block_on(select_message_owner(
            false,
            || async {
                unsupported_interrupt_calls.set(unsupported_interrupt_calls.get() + 1);
            },
            |_| {
                unsupported_enqueue_calls.set(unsupported_enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));
        assert_eq!(owner, MessageOwner::Host);
        assert_eq!(unsupported_interrupt_calls.get(), 1);
        assert_eq!(unsupported_enqueue_calls.get(), 0);
    }

    #[test]
    fn bridge_rejected_private_events_stay_on_the_host() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        let interrupt_calls = Cell::new(0);
        let enqueue_calls = Cell::new(0);
        let (owner, _) = runtime.block_on(select_message_owner(
            false,
            || async {
                interrupt_calls.set(interrupt_calls.get() + 1);
            },
            |_| {
                enqueue_calls.set(enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));

        assert_eq!(owner, MessageOwner::Host);
        assert_eq!(interrupt_calls.get(), 1);
        assert_eq!(enqueue_calls.get(), 0);
    }

    #[test]
    fn core_ingress_failure_does_not_fall_back_to_the_host() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        for outcome in [
            EnqueueOutcome::DroppedAtCapacity,
            EnqueueOutcome::SkippedInvalid,
        ] {
            let order = Cell::new(0);
            let (owner, _) = runtime.block_on(select_message_owner(
                true,
                || async {
                    assert_eq!(order.get(), 0);
                    order.set(1);
                },
                |_| {
                    assert_eq!(order.get(), 1);
                    order.set(2);
                    outcome
                },
            ));
            assert_eq!(owner, MessageOwner::Dropped, "{outcome:?}");
            assert_eq!(order.get(), 2, "{outcome:?}");
        }
    }

    #[test]
    fn data_erasure_block_drops_core_private_handling() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        let interrupt_calls = Cell::new(0);
        let enqueue_calls = Cell::new(0);
        let (owner, _) = runtime.block_on(select_message_owner(
            true,
            || async {
                interrupt_calls.set(interrupt_calls.get() + 1);
            },
            |_| {
                enqueue_calls.set(enqueue_calls.get() + 1);
                EnqueueOutcome::Blocked
            },
        ));

        assert_eq!(owner, MessageOwner::Dropped);
        assert_eq!(enqueue_calls.get(), 1);
        assert_eq!(interrupt_calls.get(), 1);
    }

    #[test]
    fn accepted_core_message_has_exactly_one_reply_owner() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        let order = Cell::new(0);
        let enqueue_calls = Cell::new(0);
        let (owner, admission) = runtime.block_on(select_message_owner(
            true,
            || async {
                assert_eq!(order.get(), 0);
                order.set(1);
                42_u8
            },
            |admission| {
                assert_eq!(order.get(), 1);
                assert_eq!(*admission, 42);
                order.set(2);
                enqueue_calls.set(enqueue_calls.get() + 1);
                EnqueueOutcome::Accepted
            },
        ));

        assert_eq!(owner, MessageOwner::Core);
        assert_eq!(admission, 42);
        assert_eq!(order.get(), 2);
        assert_eq!(enqueue_calls.get(), 1);
        assert_ne!(owner, MessageOwner::Host);
    }

    #[test]
    fn active_admission_routes_core_capable_follow_up_to_host_queue() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        runtime.block_on(async {
            let scope = crate::model::ReplyScope::Group(9_200_004);
            let active = crate::model::interrupt(scope).await;
            assert!(crate::model::mark_active(active).await);
            let enqueue_calls = Cell::new(0);
            let (owner, admission) = select_message_owner_with_admission_policy(
                true,
                || async { crate::model::ConversationCoordinator::begin_incoming(scope).await },
                |admission| admission.active_reply_preserved,
                |_| {
                    enqueue_calls.set(enqueue_calls.get() + 1);
                    EnqueueOutcome::Accepted
                },
            )
            .await;

            assert_eq!(owner, MessageOwner::Host);
            assert_eq!(enqueue_calls.get(), 0);
            assert!(admission.active_reply_preserved);
            assert!(crate::model::ConversationCoordinator::abandon_incoming(admission).await);
            crate::model::finish(active).await;
        });
    }

    #[test]
    fn core_owner_invalidates_the_existing_private_reply_generation() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        runtime.block_on(async {
            let scope = crate::model::ReplyScope::Private(9_200_001);
            let previous = crate::model::interrupt(scope).await;
            assert!(crate::model::is_current(previous).await);

            let (owner, _) = select_message_owner(
                true,
                || async {
                    let _ = crate::model::interrupt(scope).await;
                },
                |_| EnqueueOutcome::Accepted,
            )
            .await;

            assert_eq!(owner, MessageOwner::Core);
            assert!(!crate::model::is_current(previous).await);
        });
    }

    #[test]
    fn host_owned_ingress_freezes_then_supersedes_prepared_output() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        runtime.block_on(async {
            let scope = crate::model::ReplyScope::Private(9_200_002);
            let previous = crate::model::interrupt(scope).await;
            assert!(crate::model::mark_active(previous).await);
            let outgoing = crate::model::prepare_outgoing(
                previous,
                crate::model::outgoing_fingerprint("stale reply"),
                crate::model::OutgoingSource::Reply,
            )
            .await
            .expect("current reply should prepare");
            crate::model::finish(previous).await;

            let (owner, admission) = select_message_owner(
                false,
                || async { crate::model::ConversationCoordinator::begin_incoming(scope).await },
                |_| EnqueueOutcome::Accepted,
            )
            .await;

            assert_eq!(owner, MessageOwner::Host);
            assert_eq!(
                admission.decision,
                crate::model::OutgoingExecutiveDecision::Rewrite
            );
            assert!(admission.frozen_prepared);
            assert!(crate::model::is_current(previous).await);
            let refined = crate::model::ConversationCoordinator::refine_current_incoming(
                admission,
                crate::model::OutgoingExecutiveContext::default(),
            )
            .await
            .expect("Host semantic refinement should remain current");
            assert_eq!(
                refined.decision,
                crate::model::OutgoingExecutiveDecision::Rewrite
            );
            assert!(!crate::model::is_current(previous).await);
            assert!(!crate::model::commit_outgoing(outgoing).await);
            assert_eq!(
                crate::model::test_outgoing_state(outgoing).await,
                Some(crate::model::OutgoingState::Superseded)
            );
        });
    }

    #[test]
    fn owner_selection_returns_the_production_defer_admission() {
        let runtime = kovi::tokio::runtime::Runtime::new().expect("应创建测试运行时");
        runtime.block_on(async {
            let scope = crate::model::ReplyScope::Private(9_200_003);
            let previous = crate::model::interrupt(scope).await;
            assert!(crate::model::mark_active(previous).await);
            let outgoing = crate::model::prepare_outgoing(
                previous,
                crate::model::outgoing_fingerprint("prepared proactive"),
                crate::model::OutgoingSource::Proactive,
            )
            .await
            .expect("proactive output should prepare");
            crate::model::finish(previous).await;

            let (owner, admission) = select_message_owner(
                false,
                || async { crate::model::ConversationCoordinator::begin_incoming(scope).await },
                |_| EnqueueOutcome::Accepted,
            )
            .await;

            assert_eq!(owner, MessageOwner::Host);
            assert_eq!(
                admission.decision,
                crate::model::OutgoingExecutiveDecision::Defer
            );
            assert!(admission.frozen_prepared);
            let refined = crate::model::ConversationCoordinator::refine_current_incoming(
                admission,
                crate::model::OutgoingExecutiveContext::default(),
            )
            .await
            .expect("Host semantic refinement should remain current");
            assert_eq!(
                refined.decision,
                crate::model::OutgoingExecutiveDecision::Defer
            );
            assert!(!crate::model::commit_outgoing(outgoing).await);
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
