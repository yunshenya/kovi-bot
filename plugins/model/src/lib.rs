use crate::model::{group_message_event, private_message_event};
use kovi::PluginBuilder;

pub mod config;
mod model;
mod utils;
pub mod memory;
pub mod topic_generator;
pub mod mood_system;
pub mod proactive_chat;

#[kovi::plugin]
async fn main() {
    register_chat_function! {
        (group_message,group_message_event),
        (private_message, private_message_event)
    }
    PluginBuilder::on_group_msg(group_message);
    PluginBuilder::on_private_msg(private_message);
    
    // 启动主动聊天系统
    // 注意：这里需要根据实际的kovi API来调整
    // tokio::spawn(async {
    //     let memory_manager = std::sync::Arc::new(crate::memory::MemoryManager::new("bot_memory.json"));
    //     // 主动聊天系统将在后续版本中实现
    // });
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    #[tokio::test]
    async fn test() {}
}
