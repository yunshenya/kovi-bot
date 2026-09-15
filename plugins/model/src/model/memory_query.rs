//! 由模型自主发起、由程序严格约束的工具调用循环。

use super::interrupt::{ReplyTicket, is_current};
use super::reply::{
    REPLY_ACTION_TOOL_NAME, ReplyActionOutcome, ReplyTurn, reply_action_from_tool_calls,
    reply_action_tool_spec,
};
use super::thinking::ThinkingReporter;
use super::tool_access::{ToolExecutionContext, ToolExecutionResult, tool_registry};
use super::utils::{
    BotMemory, ModelPayload, NativeToolStyle, Roles, assistant_tool_calls_wire,
    is_model_error_response, likely_requires_tool_protocol, params_model_with_native_tools,
    params_model_with_native_tools_and_plain_style,
    params_model_with_native_tools_and_reply_guidance, params_model_with_plain_style_context,
    params_model_with_plain_style_context_allow_empty,
    params_model_with_token_limit_and_progress_for_reply, params_model_without_reply_guidance,
    plain_assistant_wire, system_wire, tool_result_wire, vision_failure_detail,
};
use crate::config;
use crate::vision::VisionImage;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// 一轮里最多执行多少个原生工具调用。
///
/// `tools.max_rounds` 只约束"来回几轮"，不约束单轮里上游一次返回多少个调用：
/// 上游异常或被注入诱导时可能一次返回上百个 `group.message.send`，而它们会**全部**
/// 真的执行（本轮里每个调用都发一条群消息）。这里给单轮设界，超出的部分回一条
/// "没执行"，既保住 wire 里 tool_call 与 tool 结果的一一配对，也让模型自己拆分。
const MAX_TOOL_CALLS_PER_ROUND: usize = 8;

/// 本轮不执行的调用，以及为什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallRefusal {
    /// 单轮条数超限：剩下的拆到后续轮次。
    OverRoundBudget,
    /// 本轮的长期记忆查询次数已经用完。
    MemoryRoundsExhausted,
}

impl ToolCallRefusal {
    fn message(self) -> String {
        match self {
            Self::OverRoundBudget => format!(
                "本轮最多执行 {MAX_TOOL_CALLS_PER_ROUND} 个工具调用，这一条没有执行；请把剩下的调用拆到后续轮次。"
            ),
            Self::MemoryRoundsExhausted => {
                "本轮长期记忆查询次数已用完，请使用已有资料回答。".to_string()
            }
        }
    }
}

/// 这一轮里的第 `call_index` 个调用要不要真的执行。
///
/// 单轮上限先判：即使记忆配额还剩，也不该在一个响应里把上百个调用全放出去。
fn refuse_tool_call(
    call_index: usize,
    tool_name: &str,
    memory_rounds: u8,
    max_memory_rounds: u8,
) -> Option<ToolCallRefusal> {
    if call_index >= MAX_TOOL_CALLS_PER_ROUND {
        return Some(ToolCallRefusal::OverRoundBudget);
    }
    (tool_name == "memory.search" && memory_rounds >= max_memory_rounds)
        .then_some(ToolCallRefusal::MemoryRoundsExhausted)
}

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

/// 这一轮要不要**只**为了表情包把工具带上。
///
/// 独立成一个函数是为了能单测：判定的两端都会出错——写宽了每个普通回合都多背一轮工具
/// 循环的风险（模型可能在不必要的回合发起调用），写窄了就回到"提示词让她调 `sticker_list`、
/// 她手里却没有"的老毛病。
///
/// 判据：只在**不是**工具轮时补（工具轮本来就带全套工具），且素材库确实有货——后者同时是
/// 宿主回复协议里那段"先调 sticker_list 拿标签"的下发条件（`reply.rs` 用同一个
/// `sticker_library::is_available`）。两者绑在同一个判据上，"提示词点名了工具"与"工具在
/// 请求里"才不会各说各话。
fn offers_sticker_tool_alone(tool_turn: bool, sticker_available: bool) -> bool {
    !tool_turn && sticker_available
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextPromptMode {
    /// 本轮挂了 `reply_action`：结构化动作走工具，正文仍是自然语言。
    ReplyAction,
    PlainText,
}

/// 这一轮该带哪份语气参考（原本还兼管"要不要走带回复引导的那条路"）。
///
/// 注意：**普通文本快速路径不可能落在 `ReplyAction` 上**。快速路径的守卫是
/// "没有 `reply_action` 工具"，而工具正是由 `allow_reply_actions` 决定的，所以那条路上
/// `allow_reply_actions` 必为 false（见 [`params_model_with_tool_access`] 里的守卫）。
/// `ReplyAction` 这一档只在工具循环里用来选 `NativeToolStyle`。
fn context_prompt_mode(tool_context: &ToolExecutionContext) -> ContextPromptMode {
    if tool_context.allow_reply_actions {
        ContextPromptMode::ReplyAction
    } else {
        ContextPromptMode::PlainText
    }
}

/// 普通回复只调用一次模型；只有模型明确请求工具时才进入有限工具循环。
///
/// 返回值是 [`ReplyTurn`]：正文之外还带着模型通过 `reply_action` 工具提交的结构化动作。
/// 动作只能从工具参数里来——调用方不再（也不能）从正文里解析任何东西。
pub(crate) async fn params_model_with_tool_access(
    messages: &mut [BotMemory],
    tool_context: ToolExecutionContext,
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> ReplyTurn {
    // "工具轮"：这一轮本来就该带工具（群被暂停 / 语义层判定要查 / 关键词命中）。
    let tool_turn = tool_context.group_paused
        || tool_context.requires_structured_tool_turn()
        || latest_user_message(messages).is_some_and(likely_requires_tool_protocol);
    // 素材库有货时，外面挂的动作候选提示里已经点名了 `sticker_list`（见 `reply.rs` 的
    // `REPLY_ACTION_STICKER_FIELD`，判据与这里同一个 `is_available`）。工具只在工具轮下发的话，
    // 提示词让她去调、她手里却没有这个工具——只能凭印象编一个标签，或者答应发一张相册里
    // 没有的图（线上 2026-09-15 02:15 的"猫猫歪头"）。Core 那条路修的是同一个坑
    // （`7ee0b95`），这里补上，两条链路才一致。
    let sticker_only_turn =
        offers_sticker_tool_alone(tool_turn, crate::sticker_library::is_available());
    // 结构化回复动作的工具声明。只在这轮真的该有结构动作用途时下发；
    // 两个可选能力（语音 / 表情包）按当下真的可用决定字段是否进 schema。
    let reply_action_tool = tool_context.allow_reply_actions.then(|| {
        reply_action_tool_spec(
            crate::config::qq_voice_enabled(),
            crate::sticker_library::is_available(),
        )
    });
    if !tool_turn && !sticker_only_turn && reply_action_tool.is_none() {
        // 走到这里的这一轮既没有工具、也没有 `reply_action`（等价于
        // `!allow_reply_actions`），就是一条普通可见回复：一次模型调用、带 plain 语气参考。
        return interruptible_model_call_with_plain_style_context(
            messages,
            reply_ticket,
            max_output_tokens,
            vision_images,
            None,
        )
        .await
        .map(ReplyTurn::from)
        .unwrap_or_else(interrupted_turn);
    }
    // 挂了 `reply_action` 的回合即使不是工具轮也要进循环：那里统一负责"这一轮挂了哪些
    // 工具、模型怎么用它们"。它的工具清单可以只有 `reply_action`（+ sticker.list），
    // 与"整套工具每轮几百个 token"是两回事。
    let native_tool_style = native_tool_style(&tool_context, tool_turn, sticker_only_turn);
    let Some(registry) = tool_registry() else {
        if tool_context.group_paused {
            return ReplyTurn::silent();
        }
        if tool_context.requires_external_tool {
            eprintln!(
                "[WARN] 定时任务请求未执行：外部查询工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            return BotMemory {
                role: Roles::Assistant,
                content: crate::reminders::SCHEDULED_EXTERNAL_TOOL_FAILURE.to_string(),
            }
            .into();
        }
        if tool_context.requires_reminder_create {
            eprintln!(
                "[WARN] 定时任务请求未执行：模型工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            return BotMemory {
                role: Roles::Assistant,
                content: "我暂时无法创建这个定时任务，请稍后再试一次。".to_string(),
            }
            .into();
        }
        if tool_context.requires_agent_run_create {
            eprintln!(
                "[WARN] 持续任务请求未执行：模型工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            return required_agent_run_failure(false).into();
        }
        if tool_context.requires_group_message_send {
            eprintln!(
                "[WARN] 跨群发送请求未执行：模型工具注册表不可用 (范围: {}:{})",
                tool_context.context, tool_context.subject_id
            );
            if tool_context.requires_group_followup {
                return required_group_followup_failure(false, false).into();
            }
            return required_group_message_failure(false, false).into();
        }
        // 工具注册表不可用挡的是注册表里的工具，`reply_action` 不经过它：
        // 结构化动作仍然照常下发，否则"按昵称 @/引用/撤回"这类明确要求会毫无动静地
        // 退化成一次普通文字回复。
        if let Some(reply_action_tool) = reply_action_tool {
            return interruptible_reply_action_turn(
                messages,
                &[],
                std::slice::from_ref(&reply_action_tool),
                native_tool_style,
                reply_ticket,
                max_output_tokens,
                vision_images,
                progress,
            )
            .await
            .unwrap_or_else(ReplyTurn::silent);
        }
        // 既没有注册表、也没有 `reply_action`（等价于 `!allow_reply_actions`）：普通可见回复。
        return interruptible_model_call_with_plain_style_context(
            messages,
            reply_ticket,
            max_output_tokens,
            vision_images,
            None,
        )
        .await
        .map(ReplyTurn::from)
        .unwrap_or_else(interrupted_turn);
    };

    let mut tool_context = tool_context;
    let mut request = messages.to_vec();
    // 这段"怎么用工具"的长指令只发给真正的工具轮。只因为"她可能想发图"才带上
    // `sticker.list` 的普通回合同样不带它：那一段四百多字、每轮都付，而这一轮需要的
    // 全部信息已经在工具自己的 description 里（AGENTS.md 第 6 条：能写进工具 description
    // 的就放那里，不要抄进提示词）。Core 那条路对 sticker-only 回合也是这么做的。
    if tool_turn {
        request.push(BotMemory {
            role: Roles::System,
            content: registry.instruction_for_native(&tool_context, false),
        });
    }
    // reminder.create / agent.run.create 的强制指令由
    // `instruction_for_native`（上面那一行）按 `requires_*` 统一追加，
    // 这里不再重复一份，免得两条链路各说各话。
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
    // 一旦某一轮的结果里混进了外部（可被注入）内容，从**下一轮**开始就只给它挂只读
    // 工具：网页/搜索结果是被注入的载体，模型据此发起的发送、提醒、改群状态都不该带
    // 真实副作用。没有外部内容时保持全量——`group.message.targets` → `group.message.send`
    // 那条两轮流程靠的是宿主自己给的数据，不受影响。
    //
    // 与兄弟路径一致：core_model 的跟进轮用 `native_tool_specs(ctx, tool_follow_up)`，
    // delivery 在执行前再查一次 `available_read_only_for_context`，qq_call 直接走
    // `execute_read_only`。这里原来两样都没做。
    let mut untrusted_tool_output = false;
    // 这一轮到底有没有真的调过 `sticker.list`。"她可能想发图"才带上工具的那些普通回合，
    // 查过一次就够了——清单已经在上下文里，再带一次只是白花一轮模型调用（Core 那条路
    // 用 `just_completed_sticker_list` 守同一件事）。
    let mut sticker_list_queried = false;
    // 工具循环的历史增量（assistant tool_calls / assistant 文本 / tool 结果 /
    // 修复 system 提示）统一以 wire 形式维护，保证与 API 历史严格同序：
    // 模型永远通过 provider 的 tool_calls 通道发起调用，不再依赖文本协议。
    let mut extra_wire: Vec<Value> = Vec::new();

    for round in 0..max_tool_rounds {
        // 原生 function-calling 清单：只包含本轮上下文可用的工具；上一轮吃到外部内容
        // 就收窄成只读。
        //
        // 只因为"她可能想发图"才带上工具的普通回合只给 `sticker.list` 一个：整套工具是
        // 每轮几百个 token，还会让她在闲聊里发起不相干的调用。她已经查过就不再带——清单
        // 就在上一条工具结果里。
        let mut tool_specs = if sticker_only_turn && !sticker_list_queried {
            registry
                .sticker_tool_spec(&tool_context)
                .into_iter()
                .collect::<Vec<_>>()
        } else if sticker_only_turn {
            Vec::new()
        } else {
            registry.native_tool_specs(&tool_context, untrusted_tool_output)
        };
        // `reply_action` 与注册表工具并列下发。它不在注册表里（执行者是宿主自己），
        // 所以这里单独追加；已经查过 `sticker.list` 的回合清单为空，但那一轮恰恰最需要
        // 它——标签拿到手之后，只有通过 `reply_action` 的 sticker 字段才发得出去。
        if let Some(reply_action_tool) = reply_action_tool.as_ref() {
            tool_specs.push(reply_action_tool.clone());
        }
        // 普通可见回合（`PlainText`）本来就带语气参考（`generate_plain_style_context`：
        // 此刻心情 / 精力 / 主动性），而工具循环的**最后一轮就是那条可见正文**，所以加了
        // 工具也不该把它丢掉。宿主这条循环原来一律用不带语气上下文的版本，于是"她只是
        // 可能想发图"才进循环的普通回合会静默少掉一段提示词。生成它只是读一次 personality
        // 再拼字符串，每轮重算是便宜的。
        let payload = interruptible_native_tool_call(
            &mut request,
            &extra_wire,
            &tool_specs,
            native_tool_style,
            reply_ticket,
            max_output_tokens,
            vision_images,
            progress.clone(),
        )
        .await;
        let Some(payload) = payload else {
            return interrupted_turn();
        };
        if vision_failure_detail(&payload.content).is_some() {
            if group_message_send_succeeded {
                return completed_group_message_response().into();
            }
            if agent_run_create_succeeded {
                return completed_agent_run_response().into();
            }
            return payload.as_bot_memory().into();
        }
        if !payload.tool_calls.is_empty() {
            // 结构化回复动作是**终止轮**：她提交动作就是在给这一轮下结论，不再有后续
            // 往返（动作已经交出去了，也没有对应的工具结果要回灌）。与注册表工具同轮
            // 提交则动作作废——她还没拿到工具结果就先宣布了怎么回。
            let classified =
                classify_turn_tool_calls(&payload.tool_calls, payload.finish_reason.as_deref());
            if classified.registry.is_empty() {
                // 暂停（`#禁言`）期间不说话这条约束不能被"她提交了动作"绕过：
                // 与下面没有工具调用的那条路同一个判据。`group.resume` 成功执行会把
                // `group_paused` 清掉，所以"解禁并回复"不受影响。
                if tool_context.group_paused {
                    return ReplyTurn::silent();
                }
                return finish_reply_action_turn(payload.content, classified.action);
            }
            let registry_calls = classified.registry;
            // ===== 原生 function-calling 轮：执行全部调用，结果回灌后让
            // 模型继续推理（ReAct），直到它认为资料足够并输出最终正文。 =====
            println!(
                "[INFO] 模型原生工具调用请求: 数量={}, 范围={}:{}, 轮次={}",
                registry_calls.len(),
                tool_context.context,
                tool_context.subject_id,
                round + 1
            );
            let mut executed: Vec<(String, ToolExecutionResult)> =
                Vec::with_capacity(registry_calls.len());
            for (call_index, call) in registry_calls.iter().enumerate() {
                // Provider 返回的 wire 名（点号已转下划线）先反查回注册名；
                // 未知名字原样交给执行层，让它以“未知工具”失败反馈给模型。
                let tool_name = registry.resolve_wire_tool_name(&call.name);
                if tool_name == crate::sticker_library::TOOL_NAME {
                    sticker_list_queried = true;
                }
                // 每个 tool_call 都必须有一条配对的 tool 结果（否则下一次请求会因为
                // "tool_calls 没有全部跟结果"被上游拒绝），所以超限的那些也要回一条
                // 结果——回"没执行，请拆到下一轮"，而不是静默丢掉或照单全收。
                let result = if let Some(refusal) =
                    refuse_tool_call(call_index, &tool_name, memory_rounds, max_memory_rounds)
                {
                    ToolExecutionResult {
                        succeeded: false,
                        content: refusal.message(),
                        reminder_failure_kind: None,
                    }
                } else {
                    if tool_name == "memory.search" {
                        memory_rounds += 1;
                    }
                    // 执行边界也要守：只靠清单收窄不够，模型仍可能报出上一轮见过的
                    // 写工具名。sticker-only 的普通回合同样按只读执行——那一轮只该查清单，
                    // 不该有任何副作用（与 Core 的 `read_only_only = sticker_only_turn ||
                    // follow_up` 一个口径）。
                    if untrusted_tool_output || sticker_only_turn {
                        registry
                            .execute_read_only(
                                &tool_name,
                                call.arguments.clone(),
                                tool_context.clone(),
                                reply_ticket,
                            )
                            .await
                    } else {
                        registry
                            .execute(
                                &tool_name,
                                call.arguments.clone(),
                                tool_context.clone(),
                                reply_ticket,
                            )
                            .await
                    }
                };
                if result.succeeded && is_external_tool_name(&tool_name) {
                    external_tool_succeeded = true;
                    untrusted_tool_output = true;
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
            extra_wire.push(assistant_tool_calls_wire(&payload.content, &registry_calls));
            for (index, result) in executed.iter().enumerate() {
                let call_id = registry_calls
                    .get(index)
                    .map(|call| call.id.clone())
                    .unwrap_or_default();
                extra_wire.push(tool_result_wire(&call_id, &result.1.content));
            }
            // 这条提示必须排在工具结果**之后**：wire 里 assistant 的 `tool_calls` 与随后的
            // `role: "tool"` 结果要保持相邻配对，中间插一条 system 会被上游判成
            // "tool_calls 没有全部跟结果"。
            if classified.action_dropped {
                extra_wire.push(system_wire(
                    "reply_action 必须单独调用：不要在发起其它工具调用的同一轮里提交它。先看完这一轮的工具结果，再用一次独立的 reply_action 提交本轮动作。",
                ));
            }
            continue;
        }
        // Provider 未返回工具调用：把本轮正文当作普通助手响应，沿用既有
        // required 工具约束（旧模型/网关混用期兼容）。
        let response = payload.as_bot_memory();
        // Provider 未返回工具调用：本轮正文就是最终回复。仍先校验 required
        // 工具约束（提醒/持续任务/跨群发送等），未完成时要求补齐或拒绝
        // 可能伪造成功的模型文本。
        if tool_context.group_paused {
            return ReplyTurn::silent();
        }
        if group_message_send_succeeded && is_model_error_response(&response.content) {
            return if group_followup_succeeded {
                completed_group_followup_response()
            } else {
                completed_group_message_response()
            }
            .into();
        }
        if agent_run_create_succeeded && is_model_error_response(&response.content) {
            return completed_agent_run_response().into();
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
                }
                .into();
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
            return required_agent_run_failure(agent_run_create_attempted).into();
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
            )
            .into();
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
            )
            .into();
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
        return response.into();
    }

    if tool_context.requires_reminder_create && !reminder_tool_succeeded {
        log_reminder_failure(
            reminder_failure,
            reminder_failure_detail.as_deref(),
            tool_context,
        );
        return reminder_failure_response(reminder_failure, reminder_failure_detail.as_deref())
            .into();
    }

    if tool_context.requires_agent_run_create && !agent_run_create_succeeded {
        return required_agent_run_failure(agent_run_create_attempted).into();
    }

    if tool_context.requires_external_tool && !external_tool_succeeded {
        eprintln!(
            "[WARN] 定时任务工具调用轮次耗尽，未获得成功的外部资料 (范围: {}:{})",
            tool_context.context, tool_context.subject_id
        );
        return BotMemory {
            role: Roles::Assistant,
            content: crate::reminders::SCHEDULED_EXTERNAL_TOOL_FAILURE.to_string(),
        }
        .into();
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
        }
        .into();
    }

    if tool_context.requires_group_followup && !group_followup_succeeded {
        return required_group_followup_failure(
            group_target_lookup_succeeded,
            group_message_send_attempted,
        )
        .into();
    }

    extra_wire.push(system_wire(
        "本轮工具调用次数已用完。请使用已有结果直接回答，不要再发起工具调用。",
    ));
    // 这一轮同样按"有没有吃到外部内容"决定清单，否则收窄会被这最后一次调用绕过。
    let mut final_tool_specs = registry.native_tool_specs(&tool_context, untrusted_tool_output);
    if let Some(reply_action_tool) = reply_action_tool.as_ref() {
        final_tool_specs.push(reply_action_tool.clone());
    }
    let Some(payload) = interruptible_native_tool_call(
        &mut request,
        &extra_wire,
        &final_tool_specs,
        native_tool_style,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
    )
    .await
    else {
        return interrupted_turn();
    };
    // 收尾轮同样是终止轮：动作在这一轮提交，不再有后续往返。
    let turn = finish_reply_action_turn(
        payload.content,
        reply_action_from_tool_calls(&payload.tool_calls, payload.finish_reason.as_deref()),
    );
    // 轮次耗尽后的收尾：允许模型使用已有结果给出最终回复，但 required
    // 工具（跨群发送/持续任务）失败时仍不得伪造成功。
    if group_message_send_succeeded && is_model_error_response(&turn.content) {
        if group_followup_succeeded {
            completed_group_followup_response().into()
        } else {
            completed_group_message_response().into()
        }
    } else if agent_run_create_succeeded && is_model_error_response(&turn.content) {
        completed_agent_run_response().into()
    } else if tool_context.group_paused {
        // 与前面几处的确定性静默一致：暂停状态在轮次耗尽后依然生效，模型
        // 输出的可见正文不能绕过“禁言期间不说话”的约束；只有本轮成功
        // 执行了 group.resume（此处 group_paused 已被清掉）才允许可见回复。
        ReplyTurn::silent()
    } else {
        turn
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
        // 与 Core 链路共用同一句话：两条链路对"提醒没建成"的说法必须一致，
        // 也不能把"模型/工具"这类内部细节讲给用户听。
        ReminderCreateFailure::NotCalled => crate::reminders::REMINDER_NOT_CREATED,
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

/// 原生工具 + 宿主结构化回复回合的语气上下文，可被新消息打断。
///
/// 与 [`interruptible_model_call_with_native_tools`] 的唯一区别是附上
/// `generate_reply_guidance`——结构化回复回合本来就有它，加了 `reply_action` 工具
/// 也不该丢。
pub(crate) async fn interruptible_model_call_with_native_tools_and_reply_guidance(
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
        response = params_model_with_native_tools_and_reply_guidance(
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

/// 原生工具 + 普通可见回合的语气上下文，可被新消息打断。
///
/// 与 [`interruptible_model_call_with_native_tools`] 的唯一区别是保留
/// `params_model_with_native_tools_and_plain_style` 附上的语气参考——普通可见回合
/// 本来就有它，加了工具也不该丢。
pub(crate) async fn interruptible_model_call_with_native_tools_and_plain_style(
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
        response = params_model_with_native_tools_and_plain_style(
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

/// 被新消息打断：这一轮什么都不发，交给接管的那一轮。
fn interrupted_turn() -> ReplyTurn {
    ReplyTurn::silent()
}

/// 按各条路原本的口径挑语气上下文。
///
/// 迁移只把动作通道从正文标记换成 `reply_action` 工具，不顺手改语气：普通可见回合带
/// plain style；原先走 legacy 文本协议的结构化回复回合在**非工具轮**上带
/// `generate_reply_guidance`，进了循环的（工具轮、sticker-only 轮）历来不带，这里照旧。
fn native_tool_style(
    tool_context: &ToolExecutionContext,
    tool_turn: bool,
    sticker_only_turn: bool,
) -> NativeToolStyle {
    match context_prompt_mode(tool_context) {
        ContextPromptMode::PlainText => NativeToolStyle::PlainStyle,
        ContextPromptMode::ReplyAction if !tool_turn && !sticker_only_turn => {
            NativeToolStyle::ReplyGuidance
        }
        ContextPromptMode::ReplyAction => NativeToolStyle::None,
    }
}

/// 按语气口径选一个原生工具调用的可中断包装。
#[allow(clippy::too_many_arguments)]
async fn interruptible_native_tool_call(
    messages: &mut [BotMemory],
    extra_wire: &[Value],
    tool_specs: &[Value],
    style: NativeToolStyle,
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<ModelPayload> {
    match style {
        NativeToolStyle::PlainStyle => {
            interruptible_model_call_with_native_tools_and_plain_style(
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
        NativeToolStyle::ReplyGuidance => {
            interruptible_model_call_with_native_tools_and_reply_guidance(
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
        NativeToolStyle::None => {
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
}

/// 一轮原生工具调用的分流结论。
#[derive(Debug)]
struct TurnToolCalls {
    /// 交给注册表执行的调用；`reply_action` 不在其中（它的执行者是宿主自己）。
    registry: Vec<crate::model::utils::NativeToolCall>,
    /// 本轮提交的结构化动作。
    action: ReplyActionOutcome,
    /// 动作是否因为"没有单独调用"而被丢弃。
    action_dropped: bool,
}

/// 把一轮 provider 工具调用拆成"结构化动作"与"注册表工具"两组。
///
/// `reply_action` 必须单独调用：与注册表工具同轮时，她还没拿到工具结果就先宣布了怎么回，
/// 动作建立在一个还不存在的前提上。工具照常执行（不静默丢调用），动作作废并由调用方在
/// 工具结果之后要求她重新提交。
fn classify_turn_tool_calls(
    calls: &[crate::model::utils::NativeToolCall],
    finish_reason: Option<&str>,
) -> TurnToolCalls {
    let registry = calls
        .iter()
        .filter(|call| call.name != REPLY_ACTION_TOOL_NAME)
        .cloned()
        .collect::<Vec<_>>();
    let outcome = reply_action_from_tool_calls(calls, finish_reason);
    if registry.is_empty() {
        return TurnToolCalls {
            registry,
            action: outcome,
            action_dropped: false,
        };
    }
    let action_dropped = matches!(outcome, ReplyActionOutcome::Submitted(_));
    TurnToolCalls {
        registry,
        action: ReplyActionOutcome::Absent,
        action_dropped,
    }
}

/// 把"这一轮的正文 + `reply_action` 的提交结果"落成 [`ReplyTurn`]。
///
/// `Invalid` 只记日志、降级成"这一轮没有结构化动作"：畸形参数绝不能变成一次静默、
/// 一次撤回或一次 @。正文该发照发；正文为空时由空回复修复那条路接管。
fn finish_reply_action_turn(content: String, outcome: ReplyActionOutcome) -> ReplyTurn {
    let action = match outcome {
        ReplyActionOutcome::Absent => None,
        ReplyActionOutcome::Submitted(action) => Some(action),
        ReplyActionOutcome::Invalid(reason) => {
            eprintln!("[WARN] reply_action 参数不可用，本轮按没有结构化动作处理 (原因: {reason})");
            None
        }
    };
    ReplyTurn { content, action }
}

/// 一次挂了 `reply_action` 的原生调用，直接返回可执行的一轮回复。
///
/// 用在没有工具循环的地方（注册表不可用时的兜底、空回复修复）：那里只挂了
/// `reply_action`，所以它必须是终止轮；万一模型报了别的工具名，这里也**不会**静默吞掉，
/// 而是明确记一条日志。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn interruptible_reply_action_turn(
    messages: &mut [BotMemory],
    extra_wire: &[Value],
    tool_specs: &[Value],
    style: NativeToolStyle,
    reply_ticket: ReplyTicket,
    max_output_tokens: Option<u32>,
    vision_images: &[VisionImage],
    progress: Option<Arc<ThinkingReporter>>,
) -> Option<ReplyTurn> {
    let payload = interruptible_native_tool_call(
        messages,
        extra_wire,
        tool_specs,
        style,
        reply_ticket,
        max_output_tokens,
        vision_images,
        progress,
    )
    .await?;
    if let Some(other) = payload
        .tool_calls
        .iter()
        .find(|call| call.name != REPLY_ACTION_TOOL_NAME)
    {
        eprintln!(
            "[WARN] 这一轮只挂载了 reply_action，未声明也没执行的工具调用被忽略 (工具: {})",
            other.name
        );
    }
    let outcome =
        reply_action_from_tool_calls(&payload.tool_calls, payload.finish_reason.as_deref());
    Some(finish_reply_action_turn(payload.content, outcome))
}

#[cfg(test)]
mod tests {
    use super::{
        ContextPromptMode, MAX_TOOL_CALLS_PER_ROUND, ReminderCreateFailure, ToolCallRefusal,
        classify_turn_tool_calls, completed_group_followup_response,
        completed_group_message_response, context_prompt_mode, interrupted_turn,
        likely_requires_tool_protocol, merge_group_send_result, offers_sticker_tool_alone,
        refuse_tool_call, reminder_failure_response, required_group_followup_failure,
        required_group_message_failure, tool_result_has_task_status, tool_round_limit,
    };
    use crate::model::MessageDestination;
    use crate::model::reply::REPLY_ACTION_TOOL_NAME;
    use crate::model::reply::ReplyActionOutcome;
    use crate::model::tool_access::{ToolExecutionContext, ToolExecutionResult};
    use crate::model::{BotMemory, ReplyScope, Roles};

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

    /// 提示词里点名了 `sticker_list`，那一轮就必须真的把工具下发。
    ///
    /// 宿主链路的回复协议在素材库有货时会写"想发一张就填 `"sticker":"标签"`（先调
    /// sticker_list 拿标签）"（`reply.rs` 的 `REPLY_PROTOCOL_STICKER`，下发判据同样是
    /// `sticker_library::is_available`）。而工具原本只在"工具轮"下发，工具轮的关键词
    /// （搜索 / 提醒 / 查一下）里没有任何与图或表情相关的词——于是"芸汐看看你的照片"
    /// 这类回合里，她被告知去调一个**不在请求里**的工具，只能凭印象编一个标签。
    /// Core 那条路修的是同一个坑（`7ee0b95` 接线、`a4e3fe4` 补边界），这里是宿主链路。
    #[test]
    fn plain_host_turns_get_the_sticker_tool_alone() {
        // 普通回合 + 素材库有货：必须补，否则提示词让她调、她调不到。
        assert!(offers_sticker_tool_alone(false, true));
        // 普通回合 + 素材库空着：不给。这种情况下回复协议本身也不提表情包。
        assert!(!offers_sticker_tool_alone(false, false));
        // 工具轮：不重复补（返回 false 不会让她少一个工具——工具轮本来就带全套）。
        assert!(!offers_sticker_tool_alone(true, true));
        assert!(!offers_sticker_tool_alone(true, false));

        // 上面那条"她调不到"的判断依据：这几种真实问法都命中不了工具轮判据。
        for content in [
            "芸汐看看你的照片",
            "发张表情包",
            "你的照片给我看看",
            "来个猫猫的表情",
        ] {
            assert!(
                !likely_requires_tool_protocol(content),
                "这句话不该被判成工具轮（否则这条补丁的前提就变了）: {content}"
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
            ContextPromptMode::ReplyAction
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

    /// 端到端接缝：这一轮真的把 `reply_action` 挂上了，provider 出的工具调用真的变成了
    /// 这一轮的动作。
    ///
    /// 单元测试测不到这一段——它跨过模型调用；而这次迁移要保证的正是"动作只从工具参数
    /// 里来"。替身返回的就是 provider 会返回的那份结构化载荷。
    #[test]
    fn reply_action_tool_call_becomes_the_turn_action() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            use crate::model::llm_mock::{MockPayload, with_mock_payload_model};
            use crate::model::utils::NativeToolCall;
            use serde_json::json;

            let scope = ReplyScope::Private(9_555_001);
            let ticket = crate::model::interrupt::interrupt(scope).await;
            let mut messages = vec![BotMemory {
                role: Roles::User,
                content: "把刚才那句话撤回".to_string(),
            }];
            let context = ToolExecutionContext {
                subject_id: 9_555_001,
                actor_user_id: 9_555_001,
                is_admin: false,
                is_main_admin: false,
                context: "private_chat",
                destination: MessageDestination::Private(9_555_001),
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
                allow_reply_actions: true,
            };
            let turn = with_mock_payload_model(
                "reply-action-tool-call",
                |request| {
                    let offered = request["tools"]
                        .as_array()
                        .expect("这一轮必须下发工具声明")
                        .iter()
                        .map(|tool| tool["function"]["name"].as_str().unwrap_or_default())
                        .collect::<Vec<_>>();
                    assert!(
                        offered.contains(&REPLY_ACTION_TOOL_NAME),
                        "结构化回复回合必须拿得到 reply_action: {offered:?}"
                    );
                    MockPayload {
                        content: "好，撤回了。".to_string(),
                        tool_calls: vec![NativeToolCall {
                            id: "call_1".to_string(),
                            name: REPLY_ACTION_TOOL_NAME.to_string(),
                            arguments: json!({"recall_message_ids": [77]})
                                .as_object()
                                .expect("对象")
                                .clone(),
                            raw_arguments: r#"{"recall_message_ids":[77]}"#.to_string(),
                        }],
                    }
                },
                async {
                    crate::model::ModelGateway::complete(
                        &mut messages,
                        context,
                        ticket,
                        Some(64),
                        &[],
                        None,
                    )
                    .await
                },
            )
            .await;

            assert_eq!(turn.content, "好，撤回了。");
            let action = turn.action.expect("工具调用必须落到这一轮的动作上");
            assert_eq!(action.action.recall_message_ids, vec![77]);
        });
    }

    /// 反向接缝：正文里复述旧标记不再产生任何动作——协议确实只剩工具一条通道。
    #[test]
    fn a_text_marker_in_the_body_no_longer_becomes_an_action() {
        let executor = kovi::tokio::runtime::Runtime::new().expect("test runtime");
        executor.block_on(async {
            use crate::model::llm_mock::{MockPayload, with_mock_payload_model};

            let scope = ReplyScope::Private(9_555_002);
            let ticket = crate::model::interrupt::interrupt(scope).await;
            let mut messages = vec![BotMemory {
                role: Roles::User,
                content: "把刚才那句话撤回".to_string(),
            }];
            let context = ToolExecutionContext {
                subject_id: 9_555_002,
                actor_user_id: 9_555_002,
                is_admin: false,
                is_main_admin: false,
                context: "private_chat",
                destination: MessageDestination::Private(9_555_002),
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
                allow_reply_actions: true,
            };
            let turn = with_mock_payload_model(
                "reply-action-text-marker",
                |_| MockPayload {
                    content: r#"[[REPLY_ACTION]]{"disposition":"silent"}[[/REPLY_ACTION]]"#
                        .to_string(),
                    tool_calls: Vec::new(),
                },
                async {
                    crate::model::ModelGateway::complete(
                        &mut messages,
                        context,
                        ticket,
                        Some(64),
                        &[],
                        None,
                    )
                    .await
                },
            )
            .await;

            assert!(
                turn.action.is_none(),
                "正文里的标记不能被解释成动作: {:?}",
                turn.action
            );
            let parsed = crate::model::reply::parse_reply_output(&turn.content, None);
            assert!(!parsed.disposition.is_silent());
            assert!(parsed.content.is_empty(), "标记本身也不能留在可见正文里");
        });
    }

    /// `reply_action` 必须单独调用：与注册表工具同轮时动作作废、工具照常执行。
    #[test]
    fn a_tool_call_voids_a_reply_action_submitted_in_the_same_round() {
        use crate::model::utils::NativeToolCall;
        use serde_json::json;

        let action = NativeToolCall {
            id: "call_1".to_string(),
            name: REPLY_ACTION_TOOL_NAME.to_string(),
            arguments: json!({"at_current_sender": true})
                .as_object()
                .expect("对象")
                .clone(),
            raw_arguments: "{}".to_string(),
        };
        let search = NativeToolCall {
            id: "call_2".to_string(),
            name: "memory.search".to_string(),
            arguments: serde_json::Map::new(),
            raw_arguments: "{}".to_string(),
        };

        // 只有动作：终止轮，动作照收。
        let alone = classify_turn_tool_calls(std::slice::from_ref(&action), None);
        assert!(alone.registry.is_empty());
        assert!(matches!(alone.action, ReplyActionOutcome::Submitted(_)));
        assert!(!alone.action_dropped);

        // 动作 + 注册表工具：动作作废，工具照常进执行队列。
        let mixed = classify_turn_tool_calls(&[action, search.clone()], None);
        assert_eq!(mixed.registry.len(), 1);
        assert_eq!(mixed.registry[0].name, "memory.search");
        assert!(matches!(mixed.action, ReplyActionOutcome::Absent));
        assert!(mixed.action_dropped);

        // 只有注册表工具：跟迁移前一样。
        let tools_only = classify_turn_tool_calls(&[search], None);
        assert_eq!(tools_only.registry.len(), 1);
        assert!(matches!(tools_only.action, ReplyActionOutcome::Absent));
        assert!(!tools_only.action_dropped);
    }

    #[test]
    fn interrupted_tool_loop_returns_structured_silence() {
        // 被打断的一轮是宿主自己判定的静默：结构化的，不经过任何正文标记。
        let turn = interrupted_turn();
        assert!(turn.is_silent());
        assert!(turn.content.is_empty());
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
        // "没调工具"这一格的文案与 Core 链路共用，断言直接对齐那个常量。
        assert_eq!(
            reminder_failure_response(ReminderCreateFailure::NotCalled, None).content,
            crate::reminders::REMINDER_NOT_CREATED
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

    #[test]
    fn a_single_round_cannot_run_unbounded_tool_calls() {
        // 上游异常或被注入诱导时，一次响应可能带回上百个调用——其中
        // `group.message.send` 这类副作用工具会**全部**真的执行。单轮必须设界。
        for index in 0..MAX_TOOL_CALLS_PER_ROUND {
            assert_eq!(
                refuse_tool_call(index, "group.message.send", 0, 2),
                None,
                "第 {index} 个调用仍在上限内"
            );
        }
        assert_eq!(
            refuse_tool_call(MAX_TOOL_CALLS_PER_ROUND, "group.message.send", 0, 2),
            Some(ToolCallRefusal::OverRoundBudget),
        );
        assert_eq!(
            refuse_tool_call(999, "memory.search", 0, 2),
            Some(ToolCallRefusal::OverRoundBudget),
            "超限优先于记忆配额"
        );
        // 记忆查询配额只在 memory.search 上生效，且用完之后仍给得出提示。
        assert_eq!(
            refuse_tool_call(0, "memory.search", 2, 2),
            Some(ToolCallRefusal::MemoryRoundsExhausted)
        );
        assert_eq!(refuse_tool_call(0, "memory.search", 1, 2), None);
        assert_eq!(
            refuse_tool_call(0, "time.now", 2, 2),
            None,
            "配额只约束记忆查询"
        );
        let message = ToolCallRefusal::OverRoundBudget.message();
        assert!(message.contains(&MAX_TOOL_CALLS_PER_ROUND.to_string()));
        assert!(
            ToolCallRefusal::MemoryRoundsExhausted
                .message()
                .contains("记忆查询")
        );
    }
}
