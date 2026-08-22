//! 消息发送边界。

use super::message_actions::MessageDestination;
use kovi::{Message, RuntimeBot};

pub(crate) struct MessageTransport<'a> {
    bot: &'a RuntimeBot,
}

impl<'a> MessageTransport<'a> {
    pub(crate) const fn new(bot: &'a RuntimeBot) -> Self {
        Self { bot }
    }

    pub(crate) async fn send(
        &self,
        destination: MessageDestination,
        message: Message,
    ) -> Result<i32, kovi::ApiReturn> {
        match destination {
            MessageDestination::Group(group_id) => {
                self.bot.send_group_msg_return(group_id, message).await
            }
            MessageDestination::Private(user_id) => {
                self.bot.send_private_msg_return(user_id, message).await
            }
        }
    }
}
