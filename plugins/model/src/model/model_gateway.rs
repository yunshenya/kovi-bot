//! 模型调用边界。

use super::interrupt::ReplyTicket;
use super::memory_query::params_model_with_tool_access;
use super::thinking::ThinkingReporter;
use super::tool_access::ToolExecutionContext;
use super::utils::BotMemory;
use crate::vision::VisionImage;
use std::sync::Arc;

/// 所有需要工具访问的主回复都经过这个网关，便于统一超时、计量和审计。
pub(crate) struct ModelGateway;

impl ModelGateway {
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
