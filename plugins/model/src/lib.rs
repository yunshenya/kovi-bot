//! # Kovi Bot Model Plugin
//!
//! 这是一个基于Kovi框架的智能聊天机器人插件，具备以下核心功能：
//! - 长期记忆系统：智能存储和检索对话记忆
//! - 情绪化人格：根据对话内容动态调整情绪状态
//! - 主动聊天：基于情绪和社交信心主动发起对话
//! - 个性化体验：根据用户档案提供定制化回复
//! - 话题生成：智能生成相关话题促进互动
//! - 健康监控：实时监控系统状态和性能

use crate::model::{group_message_event, private_message_event};
use kovi::PluginBuilder;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// 配置管理模块
pub mod config;
// 核心模型处理模块
mod model;
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

/// 后台任务启动标志，确保只启动一次
static BACKGROUND_TASK_STARTED: AtomicBool = AtomicBool::new(false);

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
        // 获取全局记忆管理器实例
        let memory_manager = Arc::clone(&memory::MEMORY_MANAGER);

        // 在后台异步任务中执行定期任务
        // 主动聊天循环由 startup 模块单独管理。
        kovi::tokio::spawn(async move {
            // 创建单一的情绪系统实例，避免重复创建
            let mood_system = mood_system::MoodSystem::new(memory_manager);

            // 定期执行自然情绪变化
            loop {
                if let Err(e) = mood_system.natural_mood_drift().await {
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
                let interval = config::get().memory().maintenance_interval_secs();
                kovi::tokio::time::sleep(kovi::tokio::time::Duration::from_secs(interval)).await;
            }
        });

        println!("[INFO] 后台任务已启动");
    }
}
