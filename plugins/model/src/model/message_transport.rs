//! 消息发送边界。

use super::message_actions::MessageDestination;
use kovi::bot::SendApi;
use kovi::tokio::sync::{mpsc, oneshot};
use kovi::types::ApiAndOneshot;
use kovi::{ApiReturn, Message, RuntimeBot};
use serde_json::{Value, json};

pub(crate) struct MessageTransport<'a> {
    bot: &'a RuntimeBot,
}

#[derive(Debug)]
pub(crate) enum MessageTransportError {
    Unavailable,
    Rejected(ApiReturn),
    Indeterminate(ApiReturn),
    IndeterminateNoResponse,
}

impl MessageTransportError {
    pub(crate) const fn is_indeterminate(&self) -> bool {
        matches!(self, Self::Indeterminate(_) | Self::IndeterminateNoResponse)
    }
}

impl std::fmt::Display for MessageTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("Kovi API request queue is unavailable"),
            Self::IndeterminateNoResponse => {
                formatter.write_str("Kovi API response channel closed after request enqueue")
            }
            Self::Rejected(response) | Self::Indeterminate(response) => write!(
                formatter,
                "status={} retcode={} data={} echo={}",
                response.status, response.retcode, response.data, response.echo
            ),
        }
    }
}

async fn request_api_response(
    api_tx: &mpsc::Sender<ApiAndOneshot>,
    request: SendApi,
) -> Result<ApiReturn, MessageTransportError> {
    let (response_tx, response_rx) = oneshot::channel();
    api_tx
        .send((request, Some(response_tx)))
        .await
        .map_err(|_| MessageTransportError::Unavailable)?;
    response_rx
        .await
        .map_err(|_| MessageTransportError::IndeterminateNoResponse)?
        .map_err(MessageTransportError::Rejected)
}

impl<'a> MessageTransport<'a> {
    pub(crate) const fn new(bot: &'a RuntimeBot) -> Self {
        Self { bot }
    }

    pub(crate) async fn send(
        &self,
        destination: MessageDestination,
        message: Message,
    ) -> Result<i32, MessageTransportError> {
        self.send_with_audit(destination, message, true).await
    }

    pub(crate) async fn send_redacted(
        &self,
        destination: MessageDestination,
        message: Message,
    ) -> Result<i32, MessageTransportError> {
        self.send_with_audit(destination, message, false).await
    }

    async fn send_with_audit(
        &self,
        destination: MessageDestination,
        message: Message,
        include_payload: bool,
    ) -> Result<i32, MessageTransportError> {
        if include_payload {
            let human_text = message.to_human_string();
            match destination {
                MessageDestination::Group(group_id) => {
                    println!("[send] [to group {group_id}]: {human_text}");
                }
                MessageDestination::Private(user_id) => {
                    println!("[send] [to private {user_id}]: {human_text}");
                }
            }
        } else {
            println!("[send] [redacted delivery]");
        }
        let params = structured_send_params(destination, message);

        let response =
            request_api_response(&self.bot.api_tx, SendApi::new("send_msg", params)).await?;
        response
            .data
            .get("message_id")
            .and_then(|message_id| message_id.as_i64())
            .and_then(|message_id| i32::try_from(message_id).ok())
            .filter(|message_id| *message_id > 0)
            .ok_or(MessageTransportError::Indeterminate(response))
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
    use super::{
        MessageDestination, MessageTransportError, request_api_response, structured_send_params,
    };
    use kovi::Message;
    use kovi::bot::SendApi;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;
    use kovi::tokio::sync::mpsc;
    use kovi::types::ApiAndOneshot;

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

    #[test]
    fn closed_api_queue_is_a_definite_transport_failure() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let (api_tx, api_rx) = mpsc::channel::<ApiAndOneshot>(1);
                drop(api_rx);

                let error = request_api_response(
                    &api_tx,
                    SendApi::new("send_msg", json!({"message": "test"})),
                )
                .await
                .expect_err("关闭的请求队列必须返回错误");

                assert!(matches!(error, MessageTransportError::Unavailable));
                assert!(!error.is_indeterminate());
            });
    }

    #[test]
    fn cancelled_response_channel_is_indeterminate_without_panicking() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let (api_tx, mut api_rx) = mpsc::channel::<ApiAndOneshot>(1);
                let consumer = kovi::tokio::spawn(async move {
                    let (_, response_tx) = api_rx.recv().await.expect("请求必须已入队");
                    drop(response_tx);
                });

                let error = request_api_response(
                    &api_tx,
                    SendApi::new("send_msg", json!({"message": "test"})),
                )
                .await
                .expect_err("响应取消后结果必须不确定");
                consumer.await.expect("消费任务不应 panic");

                assert!(matches!(
                    error,
                    MessageTransportError::IndeterminateNoResponse
                ));
                assert!(error.is_indeterminate());
            });
    }
}
