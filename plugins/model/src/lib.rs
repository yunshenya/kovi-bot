use crate::model::{group_message_event, private_message_event};
use kovi::PluginBuilder;

mod config;
mod model;
mod utils;

#[kovi::plugin]
async fn main() {
    let bot_shore = PluginBuilder::get_runtime_bot();
    let group_message = {
        let bot = bot_shore.clone();
        move |event| {
            let bot = bot.clone();
            async move {
                group_message_event(event, bot).await;
            }
        }
    };

    let private_message = {
        let bot = bot_shore.clone();
        move |event| {
            let bot = bot.clone();
            async move {
                private_message_event(event, bot).await;
            }
        }
    };
    PluginBuilder::on_group_msg(group_message);
    PluginBuilder::on_private_msg(private_message);
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    #[tokio::test]
    async fn test() {}
}
