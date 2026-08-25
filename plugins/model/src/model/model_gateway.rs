//! 模型调用边界。

use super::interrupt::ReplyTicket;
use super::memory_query::{interruptible_model_call, params_model_with_tool_access};
use super::thinking::ThinkingReporter;
use super::tool_access::ToolExecutionContext;
use super::utils::BotMemory;
use crate::vision::VisionImage;
use std::sync::Arc;

/// 所有需要工具访问的主回复都经过这个网关，便于统一超时、计量和审计。
pub(crate) struct ModelGateway;

impl ModelGateway {
    /// Complete one interruptible model request without exposing or executing
    /// any legacy tools.
    ///
    /// Core planning must remain declarative: side effects are proposed as
    /// intents and only happen later through the arbiter and action port. A
    /// cancelled request therefore stays distinguishable from a model-produced
    /// silent reply by returning `None`.
    pub(crate) async fn complete_without_tools(
        messages: &mut [BotMemory],
        reply_ticket: ReplyTicket,
        max_output_tokens: Option<u32>,
        vision_images: &[VisionImage],
        progress: Option<Arc<ThinkingReporter>>,
    ) -> Option<BotMemory> {
        interruptible_model_call(
            messages,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
    }

    pub(crate) async fn complete(
        messages: &mut [BotMemory],
        tool_context: ToolExecutionContext,
        reply_ticket: ReplyTicket,
        max_output_tokens: Option<u32>,
        vision_images: &[VisionImage],
        progress: Option<Arc<ThinkingReporter>>,
    ) -> BotMemory {
        params_model_with_tool_access(
            messages,
            tool_context,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
    }
}
