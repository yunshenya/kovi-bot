//! 模型调用边界。

use super::interrupt::ReplyTicket;
use super::memory_query::{
    interruptible_model_call, interruptible_model_call_with_native_tools,
    interruptible_model_call_with_plain_style_context,
    interruptible_model_call_with_plain_style_context_allow_empty,
    interruptible_model_call_without_reply_guidance, params_model_with_tool_access,
};
use super::thinking::ThinkingReporter;
use super::tool_access::ToolExecutionContext;
use super::utils::{BotMemory, ModelPayload};
use crate::vision::VisionImage;
use serde_json::Value;
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
    #[allow(dead_code)]
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

    /// Complete a tightly scoped protocol repair without appending the style
    /// guidance used by normal chat replies.
    pub(crate) async fn complete_without_tools_or_reply_guidance(
        messages: &mut [BotMemory],
        reply_ticket: ReplyTicket,
        max_output_tokens: Option<u32>,
        vision_images: &[VisionImage],
        progress: Option<Arc<ThinkingReporter>>,
    ) -> Option<BotMemory> {
        interruptible_model_call_without_reply_guidance(
            messages,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
    }

    /// Complete a visible plain-text turn with host-owned persona/state
    /// context, without exposing the legacy reply/action envelope.
    pub(crate) async fn complete_without_tools_with_plain_style_context(
        messages: &mut [BotMemory],
        reply_ticket: ReplyTicket,
        max_output_tokens: Option<u32>,
        vision_images: &[VisionImage],
        progress: Option<Arc<ThinkingReporter>>,
    ) -> Option<BotMemory> {
        interruptible_model_call_with_plain_style_context(
            messages,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
    }

    pub(crate) async fn complete_without_tools_with_plain_style_context_allow_empty(
        messages: &mut [BotMemory],
        reply_ticket: ReplyTicket,
        max_output_tokens: Option<u32>,
        vision_images: &[VisionImage],
        progress: Option<Arc<ThinkingReporter>>,
    ) -> Option<BotMemory> {
        interruptible_model_call_with_plain_style_context_allow_empty(
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

    /// 原生 function-calling：一次可中断的模型请求，带工具声明与宿主累积的
    /// wire 历史，返回结构化载荷。模型通过 provider 的 tool_calls 通道提出
    /// 工具调用，而不是在正文里写文本协议。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_with_native_tools(
        messages: &mut [BotMemory],
        extra_wire: &[Value],
        tool_specs: &[Value],
        reply_ticket: ReplyTicket,
        max_output_tokens: Option<u32>,
        vision_images: &[VisionImage],
        progress: Option<Arc<ThinkingReporter>>,
    ) -> Option<ModelPayload> {
        interruptible_model_call_with_native_tools(
            messages,
            extra_wire,
            tool_specs,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
    }
}
