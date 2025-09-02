use crate::model::{group_message_event, private_message_event};
use kovi::PluginBuilder;

pub mod config;
mod model;
mod utils;

#[kovi::plugin]
async fn main() {
    register_chat_function! {
        (group_message,group_message_event),
        (private_message, private_message_event)
    }
    PluginBuilder::on_group_msg(group_message);
    PluginBuilder::on_private_msg(private_message);
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    #[tokio::test]
    async fn test() {}
}
