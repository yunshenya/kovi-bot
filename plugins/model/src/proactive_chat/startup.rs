use crate::proactive_chat::ProactiveChatManager;
use kovi::RuntimeBot;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// 启动状态标记
static IS_STARTED: AtomicBool = AtomicBool::new(false);

pub async fn get_or_create_proactive_manager(
    bot: Arc<RuntimeBot>,
) -> Option<Arc<ProactiveChatManager>> {
    // 原子抢占启动权，避免群聊和私聊事件并发时创建两个循环。
    if IS_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None;
    }

    // 创建新的管理器
    let memory_manager = Arc::clone(&crate::memory::MEMORY_MANAGER);
    let manager = Arc::new(ProactiveChatManager::new(memory_manager, bot));

    // 启动主动聊天循环
    let manager_clone = Arc::clone(&manager);
    kovi::tokio::spawn(async move {
        manager_clone.start_proactive_chat_loop().await;
    });

    Some(manager)
}
