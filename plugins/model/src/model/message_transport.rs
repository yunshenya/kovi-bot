//! 消息发送边界。

use super::message_actions::MessageDestination;
use kovi::bot::SendApi;
use kovi::bot::runtimebot::send_api_request_with_response;
use kovi::{ApiReturn, Message, RuntimeBot};
use serde_json::{Value, json};

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
    ) -> Result<i32, ApiReturn> {
        let human_text = message.to_human_string();
        let params = structured_send_params(destination, message);
        match destination {
            MessageDestination::Group(group_id) => {
                println!("[send] [to group {group_id}]: {human_text}");
            }
            MessageDestination::Private(user_id) => {
                println!("[send] [to private {user_id}]: {human_text}");
            }
        }

        let response =
            send_api_request_with_response(&self.bot.api_tx, SendApi::new("send_msg", params))
                .await;
        match response {
            Ok(value) => value
                .data
                .get("message_id")
                .and_then(|message_id| message_id.as_i64())
                .map(|message_id| message_id as i32)
                .ok_or(value),
            Err(value) => Err(value),
        }
    }
}

fn structured_send_params(destination: MessageDestination, message: Message) -> Value {
    match destination {
        MessageDestination::Group(group_id) => json!({
            "message_type": "group",
            "group_id": group_id,
            "message": message,
            "auto_escape": false,
        }),
        MessageDestination::Private(user_id) => json!({
            "message_type": "private",
            "user_id": user_id,
            "message": message,
            "auto_escape": false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{MessageDestination, structured_send_params};
    use kovi::Message;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;

    #[test]
    fn structured_messages_keep_at_segments_and_disable_auto_escape() {
        let message = Message::from(vec![
            Segment::new("at", json!({"qq": "42"})),
            Segment::new("text", json!({"text": "你好"})),
        ]);

        let params = structured_send_params(MessageDestination::Group(7), message);

        assert_eq!(params["auto_escape"], false);
        assert_eq!(params["message"][0]["type"], "at");
        assert_eq!(params["message"][0]["data"]["qq"], "42");
        assert_eq!(params["message"][1]["type"], "text");
    }
}
