//! 消息发送边界。

use super::message_actions::MessageDestination;
use kovi::bot::SendApi;
use kovi::tokio::sync::{mpsc, oneshot};
use kovi::types::ApiAndOneshot;
use kovi::{ApiReturn, Message, RuntimeBot};
use serde_json::{Value, json};
use std::time::Duration;

/// A request that cannot enter Kovi's API queue is safe to fail as unavailable:
/// dropping the timed-out send future guarantees that it was not accepted.
const API_REQUEST_ENQUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// Once a `send_msg` request enters Kovi's API queue, a missing response does
/// not prove that OneBot rejected it. Bound the wait so a stuck websocket
/// cannot pin a reply worker forever, while retaining the indeterminate
/// delivery semantics that prevent an automatic duplicate send.
const API_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct MessageTransport<'a> {
    bot: &'a RuntimeBot,
}

#[derive(Debug)]
pub(crate) enum MessageTransportError {
    Unavailable,
    Rejected(ApiReturn),
    Indeterminate(ApiReturn),
    IndeterminateNoResponse,
    IndeterminateTimeout,
}

impl MessageTransportError {
    pub(crate) const fn is_indeterminate(&self) -> bool {
        matches!(
            self,
            Self::Indeterminate(_) | Self::IndeterminateNoResponse | Self::IndeterminateTimeout
        )
    }
}

impl std::fmt::Display for MessageTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("Kovi API request queue is unavailable"),
            Self::IndeterminateNoResponse => {
                formatter.write_str("Kovi API response channel closed after request enqueue")
            }
            Self::IndeterminateTimeout => formatter.write_str(
                "Kovi API response timed out after request enqueue; delivery outcome is indeterminate",
            ),
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
    request_api_response_with_timeouts(
        api_tx,
        request,
        API_REQUEST_ENQUEUE_TIMEOUT,
        API_RESPONSE_TIMEOUT,
    )
    .await
}

async fn request_api_response_with_timeouts(
    api_tx: &mpsc::Sender<ApiAndOneshot>,
    request: SendApi,
    enqueue_timeout: Duration,
    response_timeout: Duration,
) -> Result<ApiReturn, MessageTransportError> {
    let (response_tx, response_rx) = oneshot::channel();
    // Tokio's bounded `send` is cancellation-safe: when the timeout wins, the
    // request is guaranteed not to have entered the queue. Unlike
    // `Permit::send`, it also reports a receiver that closes at the capacity
    // boundary instead of silently dropping the request.
    kovi::tokio::time::timeout(enqueue_timeout, api_tx.send((request, Some(response_tx))))
        .await
        .map_err(|_| MessageTransportError::Unavailable)?
        .map_err(|_| MessageTransportError::Unavailable)?;
    kovi::tokio::time::timeout(response_timeout, response_rx)
        .await
        .map_err(|_| MessageTransportError::IndeterminateTimeout)?
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
        MessageDestination, MessageTransportError, request_api_response,
        request_api_response_with_timeouts, structured_send_params,
    };
    use kovi::Message;
    use kovi::bot::SendApi;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;
    use kovi::tokio::sync::mpsc;
    use kovi::types::ApiAndOneshot;
    use std::future::pending;
    use std::time::Duration;

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

    #[test]
    fn missing_response_times_out_as_indeterminate_without_retrying() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let (api_tx, mut api_rx) = mpsc::channel::<ApiAndOneshot>(1);
                let consumer = kovi::tokio::spawn(async move {
                    let (_, response_tx) = api_rx.recv().await.expect("请求必须已入队");
                    // Keep the response channel open to distinguish a timeout
                    // from the already-covered receiver-closed case.
                    let _response_tx = response_tx;
                    pending::<()>().await;
                });

                let error = request_api_response_with_timeouts(
                    &api_tx,
                    SendApi::new("send_msg", json!({"message": "test"})),
                    Duration::from_millis(10),
                    Duration::from_millis(10),
                )
                .await
                .expect_err("没有 API 响应时必须在有界时间内返回");

                assert!(matches!(error, MessageTransportError::IndeterminateTimeout));
                assert!(error.is_indeterminate());
                assert!(error.to_string().contains("timed out"));

                consumer.abort();
                let _ = consumer.await;
            });
    }

    #[test]
    fn full_request_queue_times_out_as_unavailable_before_enqueue() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let (api_tx, mut api_rx) = mpsc::channel::<ApiAndOneshot>(1);
                api_tx
                    .send((SendApi::new("occupied", json!({})), None))
                    .await
                    .expect("应填满测试队列");

                let error = request_api_response_with_timeouts(
                    &api_tx,
                    SendApi::new("send_msg", json!({"message": "test"})),
                    Duration::from_millis(10),
                    Duration::from_millis(10),
                )
                .await
                .expect_err("API 队列持续满载时必须在有界时间内返回");

                assert!(matches!(error, MessageTransportError::Unavailable));
                assert!(!error.is_indeterminate());
                assert_eq!(
                    api_rx.recv().await.expect("原有请求应保留").0.action,
                    "occupied"
                );
                assert!(api_rx.try_recv().is_err(), "超时请求不得在之后悄悄入队");
            });
    }
}
