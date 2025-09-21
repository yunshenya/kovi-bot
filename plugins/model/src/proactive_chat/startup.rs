use crate::proactive_chat::ProactiveChatManager;
use kovi::RuntimeBot;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

// 全局主动聊天管理器
static PROACTIVE_MANAGERS: LazyLock<Mutex<HashMap<String, Arc<ProactiveChatManager>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// 启动状态标记
static IS_STARTED: AtomicBool = AtomicBool::new(false);

pub async fn get_or_create_proactive_manager(bot: Arc<RuntimeBot>) -> Option<Arc<ProactiveChatManager>> {
    // 检查是否已经启动过
    if IS_STARTED.load(Ordering::Relaxed) {
        return None;
    }
    
    let bot_id = format!("bot_{}", std::ptr::addr_of!(bot) as usize);
    
    {
        let managers = PROACTIVE_MANAGERS.lock().unwrap();
        if let Some(manager) = managers.get(&bot_id) {
            return Some(Arc::clone(manager));
        }
    }
    
    // 创建新的管理器
    let memory_manager = Arc::clone(&crate::memory::MEMORY_MANAGER);
    let manager = Arc::new(ProactiveChatManager::new(memory_manager, bot));
    
    {
        let mut managers = PROACTIVE_MANAGERS.lock().unwrap();
        managers.insert(bot_id, Arc::clone(&manager));
    }
    
    // 标记为已启动
    IS_STARTED.store(true, Ordering::Relaxed);
    
    // 启动主动聊天循环
    let manager_clone = Arc::clone(&manager);
    kovi::tokio::spawn(async move {
        manager_clone.start_proactive_chat_loop().await;
    });
    
    Some(manager)
}
