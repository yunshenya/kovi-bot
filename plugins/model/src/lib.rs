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
}

