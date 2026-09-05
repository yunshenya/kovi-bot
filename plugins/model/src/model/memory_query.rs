//! 由模型自主发起、由程序严格约束的工具调用循环。

use super::interrupt::{ReplyTicket, is_current};
use super::reply_disposition::SILENT_REPLY_OUTPUT;
use super::thinking::ThinkingReporter;
use super::tool_access::{ToolExecutionContext, ToolExecutionResult, tool_registry};
use super::utils::{
    BotMemory, ModelPayload, Roles, assistant_tool_calls_wire, is_model_error_response,
    likely_requires_tool_protocol, params_model_with_native_tools,
    params_model_with_plain_style_context, params_model_with_plain_style_context_allow_empty,
    params_model_with_token_limit_and_progress_for_reply, params_model_without_reply_guidance,
    plain_assistant_wire, system_wire, tool_result_wire, vision_failure_detail,
};
use crate::config;
use crate::vision::VisionImage;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReminderCreateFailure {
    NotCalled,
    InvalidArguments,
    Rejected,
    Database,
    Execution,
}

impl ReminderCreateFailure {
    fn label(self) -> &'static str {
        match self {
            Self::NotCalled => "模型未发起 reminder.create",
            Self::InvalidArguments => "reminder.create 参数校验失败",
            Self::Rejected => "reminder.create 被业务限制拒绝",
            Self::Database => "提醒数据库写入失败",
            Self::Execution => "reminder.create 执行失败",
        }
    }

    fn log_prefix(self) -> &'static str {
        match self {
            Self::Database => "[ERROR]",
            _ => "[WARN]",
        }
    }
}

fn latest_user_message(messages: &[BotMemory]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| matches!(message.role, Roles::User))
        .map(|message| message.content.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextPromptMode {
    LegacyReplyActions,
    PlainText,
}

fn context_prompt_mode(tool_context: &ToolExecutionContext) -> ContextPromptMode {
    if tool_context.allow_reply_actions {
        ContextPromptMode::LegacyReplyActions
    } else {
        ContextPromptMode::PlainText
    }
}

async fn interruptible_model_call_for_context(
    messages: &mut [BotMemory],
    tool_context: &ToolExecutionContext,
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<BotMemory> {
    match context_prompt_mode(tool_context) {
        ContextPromptMode::LegacyReplyActions => {
            interruptible_model_call(
                messages,
                reply_ticket,
                max_output_tokens,
                vision_images,
                progress,
            )
            .await
        }
        ContextPromptMode::PlainText => {
            interruptible_model_call_with_plain_style_context(
                messages,
                reply_ticket,
                max_output_tokens,
                vision_images,
                None,
            )
            .await
        }
    }
}

/// 普通回复只调用一次模型；只有模型明确请求工具时才进入有限工具循环。
pub(crate) async fn params_model_with_tool_access(
    messages: &mut [BotMemory],
    tool_context: ToolExecutionContext,
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> BotMemory {
    let expose_tools = tool_context.group_paused
        || tool_context.requires_structured_tool_turn()
        || latest_user_message(messages).is_some_and(likely_requires_tool_protocol);
    if !expose_tools {
        return interruptible_model_call_for_context(
            messages,
            &tool_context,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
        .unwrap_or_else(interrupted_response);
    }
    let Some(registry) = tool_registry() else {
        if tool_context.group_paused {
            return BotMemory {
                role: Roles::Assistant,
                content: SILENT_REPLY_OUTPUT.to_string(),
            };
        }
        if tool_context.requires_external_tool {
            eprintln!(
                "[WARN] 定时任务请求未执行：外部查询工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            return BotMemory {
                role: Roles::Assistant,
                content: crate::reminders::SCHEDULED_EXTERNAL_TOOL_FAILURE.to_string(),
            };
        }
        if tool_context.requires_reminder_create {
            eprintln!(
                "[WARN] 定时任务请求未执行：模型工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            return BotMemory {
                role: Roles::Assistant,
                content: "我暂时无法创建这个定时任务，请稍后再试一次。".to_string(),
            };
        }
        if tool_context.requires_agent_run_create {
            eprintln!(
                "[WARN] 持续任务请求未执行：模型工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            return required_agent_run_failure(false);
        }
        if tool_context.requires_group_message_send {
            eprintln!(
                "[WARN] 跨群发送请求未执行：模型工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            if tool_context.requires_group_followup {
                return required_group_followup_failure(false, false);
            }
            return required_group_message_failure(false, false);
        }
        return interruptible_model_call_for_context(
            messages,
            &tool_context,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress,
        )
        .await
        .unwrap_or_else(interrupted_response);
    };

    let mut tool_context = tool_context;
    let mut request = messages.to_vec();
    request.push(BotMemory {
        role: Roles::System,
        content: registry.instruction_for_native(&tool_context, false),
    });
    if tool_context.requires_reminder_create {
        request.push(BotMemory {
            role: Roles::System,
            content: "用户明确提出了定时任务请求。本轮不能只回复‘好的’、‘记住了’或其他确认话术；必须先严格调用 reminder.create，并且只有工具返回成功创建结果后才能向用户确认。若无法确定时间或参数，调用工具会返回错误，此时必须如实说明失败，不得声称任务已创建。".to_string(),
        });
    }
    if tool_context.requires_agent_run_create {
        request.push(BotMemory {
            role: Roles::System,
            content: "语义层确认用户明确要求持续监测公开 URL。本轮不能只口头答应，也不能创建普通提醒；必须调用 agent.run.create。把间隔、停止条件、截止时间、最大次数和命中后的私聊正文转换为结构化参数。只有工具成功返回 Run 编号后才能确认已经开始；参数不清楚或工具失败时必须如实说明没有创建。".to_string(),
        });
    }
    if tool_context.requires_group_message_send {
        request.push(BotMemory {
            role: Roles::System,
            content: if tool_context.requires_group_followup {
                "语义理解层确认用户明确要求跨群问答闭环。本轮不能只口头答应；必须调用 group.message.send，并填写 collect_replies_minutes（省略时由程序使用默认等待时长）。只有工具返回 task_status=collecting 或 already_completed 后才能说问题已发出并会汇总；工具返回 task_id 后可以自然告诉主管理员可用 #群问答状态 任务编号查询，必要时用 #取消群问答 任务编号取消。不能执行或目标不唯一时不得声称已发送，结果不确定时说明无法确认且不要重试。".to_string()
            } else {
                "语义理解层确认用户明确要求立即跨群发送。本轮不能只口头答应；必须调用 group.message.send。群名目标先调用 group.message.targets。只有 group.message.send 成功后才能确认已发送；不能执行或目标不唯一时不得声称已发送，结果不确定时说明无法确认且不要重试。".to_string()
            },
        });
    }
    let model_config = config::get();
    let max_tool_rounds = tool_round_limit(model_config.tools().max_rounds(), &tool_context);
    let max_memory_rounds = model_config.memory().autonomous_query_max_rounds();
    let mut memory_rounds = 0;
    let mut external_tool_succeeded = !tool_context.requires_external_tool;
    let mut reminder_tool_succeeded = false;
    let mut reminder_failure = ReminderCreateFailure::NotCalled;
    let mut reminder_failure_detail = None;
    let mut agent_run_create_attempted = false;
    let mut agent_run_create_succeeded = false;
    let mut group_target_lookup_succeeded = false;
    let mut group_message_send_attempted = false;
    let mut group_message_send_succeeded = false;
    let mut group_followup_succeeded = false;
    // 原生 function-calling 清单：只包含本轮上下文可用的工具。
    let tool_specs = registry.native_tool_specs(&tool_context, false);
    // 工具循环的历史增量（assistant tool_calls / assistant 文本 / tool 结果 /
    // 修复 system 提示）统一以 wire 形式维护，保证与 API 历史严格同序：
    // 模型永远通过 provider 的 tool_calls 通道发起调用，不再依赖文本协议。
    let mut extra_wire: Vec<Value> = Vec::new();

    for round in 0..max_tool_rounds {
        let Some(payload) = interruptible_model_call_with_native_tools(
            &mut request,
            &extra_wire,
            &tool_specs,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress.clone(),
        )
        .await
        else {
            return interrupted_response();
        };
        if vision_failure_detail(&payload.content).is_some() {
            if group_message_send_succeeded {
                return completed_group_message_response();
            }
            if agent_run_create_succeeded {
                return completed_agent_run_response();
            }
            return payload.as_bot_memory();
        }
        if !payload.tool_calls.is_empty() {
            // ===== 原生 function-calling 轮：执行全部调用，结果回灌后让
            // 模型继续推理（ReAct），直到它认为资料足够并输出最终正文。 =====
            println!(
                "[INFO] 模型原生工具调用请求: 数量={}, 范围={}:{}, 轮次={}",
                payload.tool_calls.len(),
                tool_context.context,
                tool_context.subject_id,
                round + 1
            );
            let mut executed: Vec<(String, ToolExecutionResult)> =
                Vec::with_capacity(payload.tool_calls.len());
            for call in &payload.tool_calls {
                // Provider 返回的 wire 名（点号已转下划线）先反查回注册名；
                // 未知名字原样交给执行层，让它以“未知工具”失败反馈给模型。
                let tool_name = registry.resolve_wire_tool_name(&call.name);
                let result = if tool_name == "memory.search" && memory_rounds >= max_memory_rounds {
                    ToolExecutionResult {
                        succeeded: false,
                        content: "本轮长期记忆查询次数已用完，请使用已有资料回答。".to_string(),
                        reminder_failure_kind: None,
                    }
                } else {
                    if tool_name == "memory.search" {
                        memory_rounds += 1;
                    }
                    registry
                        .execute(
                            &tool_name,
                            call.arguments.clone(),
                            tool_context.clone(),
                            reply_ticket,
                        )
                        .await
                };
                if result.succeeded && is_external_tool_name(&tool_name) {
                    external_tool_succeeded = true;
                }
                if result.succeeded && matches!(tool_name.as_str(), "group.pause" | "group.resume")
                {
                    tool_context.group_paused = false;
                }
                if tool_name == "reminder.create" {
                    reminder_tool_succeeded = result.succeeded;
                    if reminder_tool_succeeded {
                        reminder_failure_detail = None;
                        println!(
                            "[INFO] reminder.create 执行成功 (范围: {}:{}, 轮次: {})",
                            tool_context.context,
                            tool_context.subject_id,
                            round + 1
                        );
                    } else {
                        reminder_failure = match result.reminder_failure_kind {
                            Some(crate::reminders::ReminderToolFailureKind::Validation) => {
                                ReminderCreateFailure::InvalidArguments
                            }
                            Some(crate::reminders::ReminderToolFailureKind::Rejected) => {
                                ReminderCreateFailure::Rejected
                            }
                            Some(crate::reminders::ReminderToolFailureKind::Database) => {
                                ReminderCreateFailure::Database
                            }
                            None => ReminderCreateFailure::Execution,
                        };
                        reminder_failure_detail = Some(result.content.clone());
                        eprintln!(
                            "{} {} (范围: {}:{}, 轮次: {}, 详情: {})",
                            reminder_failure.log_prefix(),
                            reminder_failure.label(),
                            tool_context.context,
                            tool_context.subject_id,
                            round + 1,
                            compact_log_text(&result.content)
                        );
                    }
                }
                if tool_name == "agent.run.create" {
                    agent_run_create_attempted = true;
                    if result.succeeded {
                        agent_run_create_succeeded = true;
                        println!(
                            "[INFO] agent.run.create 执行成功 (范围: {}:{}, 轮次: {})",
                            tool_context.context,
                            tool_context.subject_id,
                            round + 1
                        );
                    }
                }
                if tool_name == "group.message.targets" && result.succeeded {
                    group_target_lookup_succeeded = true;
                }
                if tool_name == "group.message.send" {
                    group_message_send_attempted = true;
                    merge_group_send_result(
                        &result,
                        &mut group_message_send_succeeded,
                        &mut group_followup_succeeded,
                    );
                }
                println!(
                    "[INFO] 模型原生工具调用完成 (工具: {}, 范围: {}:{}, 轮次: {})",
                    tool_name,
                    tool_context.context,
                    tool_context.subject_id,
                    round + 1
                );
                executed.push((tool_name, result));
            }
            extra_wire.push(assistant_tool_calls_wire(
                &payload.content,
                &payload.tool_calls,
            ));
            for (index, result) in executed.iter().enumerate() {
                let call_id = payload
                    .tool_calls
                    .get(index)
                    .map(|call| call.id.clone())
                    .unwrap_or_default();
                extra_wire.push(tool_result_wire(&call_id, &result.1.content));
            }
            continue;
        }
        // Provider 未返回工具调用：把本轮正文当作普通助手响应，沿用既有
        // required 工具约束与文本协议兜底（旧模型/网关混用期兼容）。
        let response = payload.as_bot_memory();
        // Provider 未返回工具调用：本轮正文就是最终回复。仍先校验 required
        // 工具约束（提醒/持续任务/跨群发送等），未完成时要求补齐或拒绝
        // 可能伪造成功的模型文本。
        if tool_context.group_paused {
            return BotMemory {
                role: Roles::Assistant,
                content: SILENT_REPLY_OUTPUT.to_string(),
            };
        }
        if group_message_send_succeeded && is_model_error_response(&response.content) {
            return if group_followup_succeeded {
                completed_group_followup_response()
            } else {
                completed_group_message_response()
            };
        }
        if agent_run_create_succeeded && is_model_error_response(&response.content) {
            return completed_agent_run_response();
        }
        if tool_context.requires_external_tool && !external_tool_succeeded {
            if is_model_error_response(&response.content) || round + 1 >= max_tool_rounds {
                eprintln!(
                    "[WARN] 定时任务未成功执行外部查询工具，拒绝发送未经核实的结果 (范围: {}:{}, 轮次: {})",
                    tool_context.context,
                    tool_context.subject_id,
                    round + 1
                );
                return BotMemory {
                    role: Roles::Assistant,
                    content: crate::reminders::SCHEDULED_EXTERNAL_TOOL_FAILURE.to_string(),
                };
            }
            eprintln!(
                "[WARN] 定时任务模型未发起外部查询，要求协议重试 (范围: {}:{}, 轮次: {})",
                tool_context.context,
                tool_context.subject_id,
                round + 1
            );
            extra_wire.push(plain_assistant_wire("外部查询尚未执行。"));
            extra_wire.push(system_wire(
                "这个定时任务依赖最新外部资料。请不要直接回答；直接通过系统工具接口再次调用 web.search（function-calling），不要输出代码块、解释文字或重复标记。",
            ));
            continue;
        }
        if should_retry_reminder_create(
            tool_context.requires_reminder_create,
            reminder_tool_succeeded,
            &response.content,
        ) {
            extra_wire.push(plain_assistant_wire(&response.content));
            extra_wire.push(system_wire(
                "你刚才只输出了确认文本，但 reminder.create 尚未执行。不要把确认当成成功；通过系统工具接口调用 time.now（如需要）并调用 reminder.create。参数要完整包含 mode、时间和用户要求的动作；只有工具成功后才能生成最终回复。",
            ));
            println!(
                "[WARN] 定时任务模型返回普通确认，要求补充 reminder.create (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            continue;
        }
        if tool_context.requires_agent_run_create && !agent_run_create_succeeded {
            if !agent_run_create_attempted
                && !is_model_error_response(&response.content)
                && round + 1 < max_tool_rounds
            {
                extra_wire.push(plain_assistant_wire(&response.content));
                extra_wire.push(system_wire(
                    "你刚才只输出了文字，但持续任务尚未创建。下一条直接通过系统工具接口调用 agent.run.create；不要用确认话术或 reminder.create 代替。",
                ));
                println!(
                    "[WARN] 持续任务模型返回普通文本，要求补充 agent.run.create (范围: {}:{})",
                    tool_context.context, tool_context.subject_id
                );
                continue;
            }
            return required_agent_run_failure(agent_run_create_attempted);
        }
        if tool_context.requires_group_message_send && !group_message_send_succeeded {
            if !group_message_send_attempted
                && !group_target_lookup_succeeded
                && !is_model_error_response(&response.content)
                && round + 1 < max_tool_rounds
            {
                extra_wire.push(plain_assistant_wire(&response.content));
                extra_wire.push(system_wire(
                    "你刚才只输出了文字，但跨群消息尚未发送。下一条直接通过系统工具接口调用 group.message.send；如果目标是群名，先调用 group.message.targets。不要用确认话术代替工具执行。",
                ));
                println!(
                    "[WARN] 跨群发送模型返回普通文本，要求补充真实动作 (范围: {}:{})",
                    tool_context.context, tool_context.subject_id
                );
                continue;
            }
            eprintln!(
                "[WARN] 跨群发送未完成，拒绝返回可能伪造成功的模型文本 (范围: {}:{}, 轮次: {})",
                tool_context.context,
                tool_context.subject_id,
                round + 1
            );
            return required_group_message_failure(
                group_target_lookup_succeeded,
                group_message_send_attempted,
            );
        }
        if tool_context.requires_group_followup && !group_followup_succeeded {
            eprintln!(
                "[WARN] 跨群问答任务未创建，拒绝把普通发送结果当成闭环完成 (范围: {}:{}, 轮次: {})",
                tool_context.context,
                tool_context.subject_id,
                round + 1
            );
            return required_group_followup_failure(
                group_target_lookup_succeeded,
                group_message_send_attempted,
            );
        }
        if tool_context.requires_reminder_create && !reminder_tool_succeeded {
            eprintln!(
                "[WARN] {}：模型返回了不可重试的普通回复 (范围: {}:{}, 轮次: {})",
                reminder_failure.label(),
                tool_context.context,
                tool_context.subject_id,
                round + 1
            );
        }
        return response;
    }

    if tool_context.requires_reminder_create && !reminder_tool_succeeded {
        log_reminder_failure(
            reminder_failure,
            reminder_failure_detail.as_deref(),
            tool_context,
        );
        return reminder_failure_response(reminder_failure, reminder_failure_detail.as_deref());
    }

    if tool_context.requires_agent_run_create && !agent_run_create_succeeded {
        return required_agent_run_failure(agent_run_create_attempted);
    }

    if tool_context.requires_external_tool && !external_tool_succeeded {
        eprintln!(
            "[WARN] 定时任务工具调用轮次耗尽，未获得成功的外部资料 (范围: {}:{})",
            tool_context.context, tool_context.subject_id
        );
        return BotMemory {
            role: Roles::Assistant,
            content: crate::reminders::SCHEDULED_EXTERNAL_TOOL_FAILURE.to_string(),
        };
    }

    if tool_context.requires_group_message_send && !group_message_send_succeeded {
        eprintln!(
            "[WARN] 跨群发送工具轮次耗尽且动作未完成 (范围: {}:{})",
            tool_context.context, tool_context.subject_id
        );
        return if tool_context.requires_group_followup {
            required_group_followup_failure(
                group_target_lookup_succeeded,
                group_message_send_attempted,
            )
        } else {
            required_group_message_failure(
                group_target_lookup_succeeded,
                group_message_send_attempted,
            )
        };
    }

    if tool_context.requires_group_followup && !group_followup_succeeded {
        return required_group_followup_failure(
            group_target_lookup_succeeded,
            group_message_send_attempted,
        );
    }

    extra_wire.push(system_wire(
        "本轮工具调用次数已用完。请使用已有结果直接回答，不要再发起工具调用。",
    ));
    let Some(response) = interruptible_model_call_with_native_tools(
        &mut request,
        &extra_wire,
        &tool_specs,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
    )
    .await
    .map(|payload| payload.as_bot_memory()) else {
        return interrupted_response();
    };
    // 轮次耗尽后的收尾：允许模型使用已有结果给出最终回复，但 required
    // 工具（跨群发送/持续任务）失败时仍不得伪造成功。
    if group_message_send_succeeded && is_model_error_response(&response.content) {
        if group_followup_succeeded {
            completed_group_followup_response()
        } else {
            completed_group_message_response()
        }
    } else if agent_run_create_succeeded && is_model_error_response(&response.content) {
        completed_agent_run_response()
    } else {
        response
    }
}

fn tool_round_limit(configured: u8, tool_context: &ToolExecutionContext) -> u8 {
    if tool_context.requires_reminder_create
        || tool_context.requires_agent_run_create
        || tool_context.requires_external_tool
    {
        configured.max(3)
    } else if tool_context.is_main_admin
        && matches!(
            tool_context.destination,
            super::MessageDestination::Private(_)
        )
        && !tool_context.scheduled
    {
        // Resolving a group name can require targets followed by send.
        configured.max(2)
    } else {
        configured
    }
}

fn required_agent_run_failure(attempted: bool) -> BotMemory {
    BotMemory {
        role: Roles::Assistant,
        content: if attempted {
            "这个持续任务没有创建成功，我不会假装已经在后台执行。请检查 URL、间隔和停止条件后再试一次。"
        } else {
            "我没有成功创建这个持续任务，所以现在并没有在后台监测。请重新说一次，并明确 URL、检查间隔和停止条件。"
        }
        .to_string(),
    }
}

fn completed_agent_run_response() -> BotMemory {
    BotMemory {
        role: Roles::Assistant,
        content: "持续监测已经开始了，满足条件、到达截止时间或提前停止时我会在私聊里告诉你。"
            .to_string(),
    }
}

fn required_group_message_failure(targets_queried: bool, send_attempted: bool) -> BotMemory {
    let content = if send_attempted {
        "我没能确认这次发送是否成功，为避免重复没有自动重试。请先到目标群确认；需要重发时请重新给我一条指令。"
    } else if targets_queried {
        "我查过可用群，但还不能唯一确认目标，消息没有发送。请给我更准确的群名或群号。"
    } else {
        "我没有成功执行这次跨群发送，消息还没发出去。请重新说一次，并明确目标群和正文。"
    };
    BotMemory {
        role: Roles::Assistant,
        content: content.to_string(),
    }
}

fn required_group_followup_failure(targets_queried: bool, send_attempted: bool) -> BotMemory {
    let content = if send_attempted {
        "我没能确认这次群内收集任务是否建立，为避免重复提问没有自动重试。请先到目标群确认；需要重新询问时请重新给我一条指令。"
    } else if targets_queried {
        "我查过可用群，但还不能唯一确认目标，问题没有发送。请给我更准确的群名或群号。"
    } else {
        "我没有成功建立这次群内收集任务，问题还没发出去。请重新说一次，并明确目标群和等待时长。"
    };
    BotMemory {
        role: Roles::Assistant,
        content: content.to_string(),
    }
}

fn completed_group_message_response() -> BotMemory {
    BotMemory {
        role: Roles::Assistant,
        content: "已经发出去了。".to_string(),
    }
}

fn completed_group_followup_response() -> BotMemory {
    BotMemory {
        role: Roles::Assistant,
        content: "已经去群里问了，我等一会儿把大家的回复整理好再告诉你。".to_string(),
    }
}

fn tool_result_has_task_status(result: &str, expected: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|value| {
            value
                .get("task_status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|status| status == expected)
}

fn merge_group_send_result(
    result: &ToolExecutionResult,
    group_message_send_succeeded: &mut bool,
    group_followup_succeeded: &mut bool,
) {
    // 一旦外部动作成功，后续重复调用失败不能覆盖已发生的副作用。
    // 这也避免模型在第二次调用出错时向用户错误地报告“没有发送”。
    if result.succeeded {
        *group_message_send_succeeded = true;
        if tool_result_has_task_status(&result.content, "collecting") {
            *group_followup_succeeded = true;
        }
    }
}

fn is_external_tool_name(name: &str) -> bool {
    matches!(
        name,
        "web.search" | "web.fetch" | "news.search" | "weather.current" | "weather.forecast"
    ) || name.starts_with("mcp.")
}

fn should_retry_reminder_create(
    requires_reminder_create: bool,
    reminder_tool_succeeded: bool,
    response: &str,
) -> bool {
    requires_reminder_create && !reminder_tool_succeeded && !is_model_error_response(response)
}

fn reminder_failure_response(failure: ReminderCreateFailure, detail: Option<&str>) -> BotMemory {
    let content = match failure {
        ReminderCreateFailure::NotCalled => {
            "我理解了你的提醒请求，但模型没有成功调用提醒工具，任务未创建。请再试一次，并把时间和提醒内容说得更明确。"
        }
        ReminderCreateFailure::InvalidArguments => match detail
            .and_then(compact_user_detail)
            .as_deref()
        {
            Some(detail) => {
                return BotMemory {
                    role: Roles::Assistant,
                    content: format!(
                        "这个提醒的参数不完整或不合法（{}），任务未创建。请补充明确的时间和提醒内容后再试。",
                        detail
                    ),
                };
            }
            None => "这个提醒的参数不完整或不合法，任务未创建。请提供明确的时间和提醒内容后再试。",
        },
        ReminderCreateFailure::Rejected => {
            "这个提醒暂时无法创建（可能已达到未完成提醒数量上限），任务未创建。请先取消旧提醒或稍后再试。"
        }
        ReminderCreateFailure::Database => "提醒服务暂时不可用，任务未创建，请稍后再试。",
        ReminderCreateFailure::Execution => "提醒工具执行失败，任务未创建，请稍后再试。",
    };
    BotMemory {
        role: Roles::Assistant,
        content: content.to_string(),
    }
}

fn compact_user_detail(value: &str) -> Option<String> {
    let value = value
        .trim()
        .strip_prefix("工具执行失败：")
        .unwrap_or(value)
        .trim();
    if value.is_empty() {
        return None;
    }
    let mut compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 120 {
        compact = compact.chars().take(119).collect::<String>();
        compact.push('…');
    }
    Some(compact)
}

fn log_reminder_failure(
    failure: ReminderCreateFailure,
    detail: Option<&str>,
    tool_context: ToolExecutionContext,
) {
    let detail = detail.map(compact_log_text).unwrap_or_default();
    if detail.is_empty() {
        eprintln!(
            "{} {} (范围: {}:{})",
            failure.log_prefix(),
            failure.label(),
            tool_context.context,
            tool_context.subject_id
        );
    } else {
        eprintln!(
            "{} {} (范围: {}:{}, 详情: {})",
            failure.log_prefix(),
            failure.label(),
            tool_context.context,
            tool_context.subject_id,
            detail
        );
    }
}

fn compact_log_text(value: &str) -> String {
    let mut compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 240 {
        compact = compact.chars().take(239).collect::<String>();
        compact.push('…');
    }
    compact
}

/// 模型请求期间轮询会话代数；一旦有新消息，立即丢弃网络 future 并让下一轮接管。
pub(crate) async fn interruptible_model_call(
    messages: &mut [BotMemory],
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<BotMemory> {
    interruptible_model_call_mode(
        messages,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
        ModelPromptMode::LegacyReplyGuidance,
    )
    .await
}

/// Run a plain-text completion with host-owned persona/state context while
/// keeping legacy reply/action guidance out of the request.
pub(crate) async fn interruptible_model_call_with_plain_style_context(
    messages: &mut [BotMemory],
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<BotMemory> {
    interruptible_model_call_mode(
        messages,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
        ModelPromptMode::PlainStyleContext,
    )
    .await
}

/// Run a plain-text completion where an empty successful response means that
/// the host should remain quiet. Provider/network failures remain observable
/// as the normal model-error response.
pub(crate) async fn interruptible_model_call_with_plain_style_context_allow_empty(
    messages: &mut [BotMemory],
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<BotMemory> {
    interruptible_model_call_mode(
        messages,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
        ModelPromptMode::PlainStyleContextAllowEmpty,
    )
    .await
}

pub(crate) async fn interruptible_model_call_without_reply_guidance(
    messages: &mut [BotMemory],
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<BotMemory> {
    interruptible_model_call_mode(
        messages,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
        ModelPromptMode::None,
    )
    .await
}

/// 带原生 function-calling 的模型调用：请求带有工具声明，返回结构化载荷
/// （正文 + provider 工具调用）。中断语义与其它可中断模型调用一致。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn interruptible_model_call_with_native_tools(
    messages: &mut [BotMemory],
    extra_wire: &[Value],
    tool_specs: &[Value],
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<ModelPayload> {
    if !is_current(reply_ticket).await {
        return None;
    }
    kovi::tokio::select! {
        response = params_model_with_native_tools(
            messages,
            extra_wire,
            tool_specs,
            max_output_tokens,
            vision_images,
            progress,
            Some(reply_ticket),
        ) => {
            is_current(reply_ticket).await.then_some(response)
        }
        () = wait_until_interrupted(reply_ticket) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelPromptMode {
    LegacyReplyGuidance,
    PlainStyleContext,
    PlainStyleContextAllowEmpty,
    None,
}

async fn interruptible_model_call_mode(
    messages: &mut [BotMemory],
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
    prompt_mode: ModelPromptMode,
) -> Option<BotMemory> {
    if !is_current(reply_ticket).await {
        return None;
    }
    kovi::tokio::select! {
        response = async {
            match prompt_mode {
                ModelPromptMode::LegacyReplyGuidance => {
                    params_model_with_token_limit_and_progress_for_reply(
                        messages,
                        max_output_tokens,
                        vision_images,
                        progress,
                        Some(reply_ticket),
                    ).await
                }
                ModelPromptMode::PlainStyleContext => {
                    params_model_with_plain_style_context(
                        messages,
                        max_output_tokens,
                        vision_images,
                        progress,
                        Some(reply_ticket),
                    ).await
                }
                ModelPromptMode::PlainStyleContextAllowEmpty => {
                    params_model_with_plain_style_context_allow_empty(
                        messages,
                        max_output_tokens,
                        vision_images,
                        progress,
                        Some(reply_ticket),
                    ).await
                }
                ModelPromptMode::None => {
                    params_model_without_reply_guidance(
                        messages,
                        max_output_tokens,
                        vision_images,
                        progress,
                        Some(reply_ticket),
                    ).await
                }
            }
        } => {
            is_current(reply_ticket).await.then_some(response)
        }
        () = wait_until_interrupted(reply_ticket) => None,
    }
}

async fn wait_until_interrupted(reply_ticket: ReplyTicket) {
    while is_current(reply_ticket).await {
        kovi::tokio::time::sleep(Duration::from_millis(75)).await;
    }
}

fn interrupted_response() -> BotMemory {
    BotMemory {
        role: Roles::Assistant,
        content: SILENT_REPLY_OUTPUT.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ContextPromptMode, ReminderCreateFailure, completed_group_followup_response,
        completed_group_message_response, context_prompt_mode, interrupted_response,
        likely_requires_tool_protocol, merge_group_send_result, reminder_failure_response,
        required_group_followup_failure, required_group_message_failure,
        tool_result_has_task_status, tool_round_limit,
    };
    use crate::model::MessageDestination;
    use crate::model::reply::parse_reply_output;
    use crate::model::tool_access::{ToolExecutionContext, ToolExecutionResult};

    #[test]
    fn message_action_words_do_not_expose_the_tool_registry() {
        // @/quote/recall are handled by the separate, explicitly authorized
        // reply-action path. They must not make an ordinary turn receive the
        // tool registry, especially when the user is discussing the syntax.
        for content in [
            "这个 @ 符号在群里是什么意思？",
            "引用这条消息是什么意思？",
            "请解释一下怎么撤回消息",
            "艾特和提及有什么区别？",
            "删除消息这个功能怎么用？",
            "搜索功能怎么用？",
            "为什么要查询天气？",
        ] {
            assert!(
                !likely_requires_tool_protocol(content),
                "message-action discussion must stay out of tool mode: {content}"
            );
        }
        for content in ["搜索 Rust 最新版本", "提醒我明天开会", "查一下现在天气"]
        {
            assert!(
                likely_requires_tool_protocol(content),
                "external-tool intent should still expose tools: {content}"
            );
        }
    }

    #[test]
    fn tool_registry_turn_does_not_receive_plain_reply_guidance() {
        let context = ToolExecutionContext {
            subject_id: 42,
            actor_user_id: 42,
            is_admin: false,
            is_main_admin: false,
            context: "tool_prompt_test",
            destination: MessageDestination::Private(42),
            source_message_id: None,
            scheduled: false,
            group_paused: false,
            runtime_bot: None,
            sticker_teaching: None,
            requires_reminder_create: false,
            requires_agent_run_create: false,
            requires_group_message_send: false,
            requires_group_followup: false,
            requires_external_tool: false,
            allow_reply_actions: false,
        };
        assert_eq!(
            context_prompt_mode(&ToolExecutionContext {
                scheduled: true,
                ..context.clone()
            }),
            ContextPromptMode::PlainText
        );
        assert_eq!(
            context_prompt_mode(&ToolExecutionContext {
                allow_reply_actions: true,
                ..context.clone()
            },),
            ContextPromptMode::LegacyReplyActions
        );
        assert_eq!(context_prompt_mode(&context), ContextPromptMode::PlainText);
    }

    #[test]
    fn main_admin_private_actions_have_two_tool_rounds() {
        let context = ToolExecutionContext {
            subject_id: 42,
            actor_user_id: 42,
            is_admin: true,
            is_main_admin: true,
            context: "private_chat",
            destination: MessageDestination::Private(42),
            source_message_id: Some(7),
            scheduled: false,
            group_paused: false,
            runtime_bot: None,
            sticker_teaching: None,
            requires_reminder_create: false,
            requires_agent_run_create: false,
            requires_group_message_send: false,
            requires_group_followup: false,
            requires_external_tool: false,
            allow_reply_actions: false,
        };
        assert_eq!(tool_round_limit(1, &context), 2);
        assert_eq!(tool_round_limit(3, &context), 3);

        let ordinary_private = ToolExecutionContext {
            is_main_admin: false,
            ..context.clone()
        };
        assert_eq!(tool_round_limit(1, &ordinary_private), 1);

        let scheduled = ToolExecutionContext {
            scheduled: true,
            ..context
        };
        assert_eq!(tool_round_limit(1, &scheduled), 1);
    }

    #[test]
    fn required_group_action_failures_never_claim_a_send_succeeded() {
        for response in [
            required_group_message_failure(false, false),
            required_group_message_failure(true, false),
            required_group_message_failure(true, true),
        ] {
            assert!(!response.content.contains("已发送"));
            assert!(!response.content.contains("发出去了"));
        }
        assert_eq!(completed_group_message_response().content, "已经发出去了。");
    }

    #[test]
    fn followup_action_failures_never_claim_that_collection_started() {
        for response in [
            required_group_followup_failure(false, false),
            required_group_followup_failure(true, false),
            required_group_followup_failure(true, true),
        ] {
            assert!(!response.content.contains("等一会儿"));
            assert!(!response.content.contains("整理好"));
        }
        assert!(
            completed_group_followup_response()
                .content
                .contains("等一会儿")
        );
        assert!(tool_result_has_task_status(
            r#"{"status":"completed","task_status":"collecting"}"#,
            "collecting"
        ));
        assert!(!tool_result_has_task_status(
            r#"{"status":"completed"}"#,
            "collecting"
        ));
    }

    #[test]
    fn later_failed_send_cannot_erase_an_already_successful_send() {
        let mut send_succeeded = false;
        let mut followup_succeeded = false;
        merge_group_send_result(
            &ToolExecutionResult {
                succeeded: true,
                content: r#"{"status":"completed","task_status":"collecting"}"#.to_string(),
                reminder_failure_kind: None,
            },
            &mut send_succeeded,
            &mut followup_succeeded,
        );
        merge_group_send_result(
            &ToolExecutionResult {
                succeeded: false,
                content: "发送状态不确定".to_string(),
                reminder_failure_kind: None,
            },
            &mut send_succeeded,
            &mut followup_succeeded,
        );
        assert!(send_succeeded);
        assert!(followup_succeeded);
    }

    #[test]
    fn interrupted_tool_loop_returns_structured_silence() {
        let parsed = parse_reply_output(&interrupted_response().content);
        assert!(parsed.disposition.is_silent());
        assert!(parsed.content.is_empty());
    }

    #[test]
    fn plain_confirmation_cannot_finish_a_reminder_request() {
        assert!(super::should_retry_reminder_create(
            true,
            false,
            "好的，三分钟后提醒你"
        ));
        assert!(!super::should_retry_reminder_create(
            true,
            true,
            "好的，三分钟后提醒你"
        ));
        assert!(!super::should_retry_reminder_create(
            false,
            false,
            "好的，三分钟后提醒你"
        ));
        assert!(!super::should_retry_reminder_create(
            true,
            false,
            "抱歉，模型服务暂时不可用（上游超时）。"
        ));
    }

    #[test]
    fn reminder_failure_responses_explain_which_stage_failed() {
        assert!(
            reminder_failure_response(ReminderCreateFailure::NotCalled, None)
                .content
                .contains("没有成功调用提醒工具")
        );
        assert!(
            reminder_failure_response(ReminderCreateFailure::InvalidArguments, None)
                .content
                .contains("参数不完整或不合法")
        );
        assert!(
            reminder_failure_response(ReminderCreateFailure::Database, None)
                .content
                .contains("提醒服务暂时不可用")
        );
    }
}
