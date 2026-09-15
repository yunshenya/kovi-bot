//! QQ delivery adapter for platform-neutral actions.
//!
//! Core only exposes opaque people and conversation IDs. This module is the
//! only place where those IDs are translated to concrete QQ destinations or
//! Kovi API calls. Legacy proactive-chat delivery remains below as a small
//! compatibility helper; new actions use [`QqActionAdapter`].

use super::core_model::HostToolTurnRegistry;
use super::delivery_ledger::{
    DeliveryActionKind, DeliveryAttempt, DeliveryCommitError, DeliveryCommitOutcome,
    DeliveryDestinationKind, DeliveryStatus, DeliveryTarget, PostgresDeliveryLedger,
};
use super::identity_store::PostgresIdentityStore;
use crate::model::tool_access::{
    ToolEffectRevalidationFuture, ToolEffectRevalidator, ToolRegistry,
};
use crate::model::{
    MessageDestination, MessageTransport, OutgoingCommitRejection, OutgoingSource, OutgoingToken,
    ReplyScope, ToolExecutionContext, action_outgoing_fingerprint, begin_outgoing_commit,
    contextual_outgoing_fingerprint, find_prepared_outgoing, find_prepared_outgoing_by_fingerprint,
    finish, is_current, mark_active, mark_outgoing_failed, outgoing_fingerprint,
    prepare_proactive_outgoing_if_idle_with_semantic_preview, record_standalone_bot_message,
    send_tracked_message_with_revalidation_guard, tool_registry,
};
use kovi::bot::message::Segment;
use kovi::serde_json::json;
use kovi::tokio::sync::Mutex;
use kovi::{Message, RuntimeBot};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use yunxi_core::{
    ActionCapability, ActionDescriptor, ActionPort, ActionPortError, ActionPortFuture,
    ActionPortOutcome, ActionPortReleaseFuture, ActionScope, ChannelAdapter, ConversationId,
    ConversationKind, ConversationMemberStore, DeliveryResolutionError, DeliveryResolver,
    DeliveryResolverFuture, DeliveryRoute, EnvironmentCapabilities, GoalState, GoalStore,
    MAX_TOOL_ERROR_DETAIL_BYTES, MAX_TOOL_ERROR_DETAIL_CHARS, MAX_TOOL_RESULT_BYTES,
    MAX_TOOL_RESULT_CHARS, MessageContent, MessageId, OpenLoopStore, PlatformId, ProposedAction,
    ReachOutIntent, ToolAction,
};
/// 判断一次发送失败是否像"群被禁言/被平台拒绝"(QQ sendMsg 的
/// status=failed retcode=1200 / EventChecker Failed 一类)。只有确定拒绝
/// (非 indeterminate)才进入退避,网络/超时类保持原重试语义。
fn qq_rejection_looks_muted(error: &crate::model::MessageTransportError) -> bool {
    let text = error.to_string();
    text.contains("retcode=1200")
        || text.contains("EventChecker Failed")
        || (text.contains("sendMsg") && text.contains("status=failed"))
}

/// Concrete QQ destination after a canonical Core conversation has been
/// resolved. The enum is intentionally private so platform identifiers do not
/// leak through the Core-facing traits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QqDestination {
    Group(i64),
    Private(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryRevalidationTarget {
    Conversation(ConversationId),
    Person(yunxi_core::PersonId),
}

impl DeliveryRevalidationTarget {
    const fn ledger_action_kind(self) -> DeliveryActionKind {
        match self {
            Self::Conversation(_) => DeliveryActionKind::SendMessage,
            Self::Person(_) => DeliveryActionKind::ReachOut,
        }
    }

    const fn ledger_target(self) -> DeliveryTarget {
        match self {
            Self::Conversation(conversation_id) => DeliveryTarget::Conversation(conversation_id),
            Self::Person(person_id) => DeliveryTarget::Person(person_id),
        }
    }
}

impl QqDestination {
    fn message_destination(self) -> MessageDestination {
        match self {
            Self::Group(group_id) => MessageDestination::Group(group_id),
            Self::Private(user_id) => MessageDestination::Private(user_id),
        }
    }

    fn reply_scope(self) -> crate::model::ReplyScope {
        match self {
            Self::Group(group_id) => crate::model::ReplyScope::Group(group_id),
            Self::Private(user_id) => crate::model::ReplyScope::Private(user_id),
        }
    }

    const fn ledger_kind(self) -> DeliveryDestinationKind {
        match self {
            Self::Group(_) => DeliveryDestinationKind::Group,
            Self::Private(_) => DeliveryDestinationKind::Private,
        }
    }

    const fn external_id(self) -> i64 {
        match self {
            Self::Group(group_id) => group_id,
            Self::Private(user_id) => user_id,
        }
    }
}

/// 发送链路各级的预算（毫秒）。
///
/// 为什么要有：线上 2026-09-14 21:16 一次群聊发送卡了整整 30 秒，最后只留下
/// `action execution timed out after 30000ms`——连 `[send]` 都没打，查不出卡在
/// 哪一级。发送前有两次查库（会话路由、群白名单）与一次内存仲裁，任何一级撞上
/// 连接池饥饿都会把整轮回复拖死（当时池是 5 条连接、acquire 超时 30 秒，和卡住
/// 的时长完全一致）。各级单独设预算：超时能指名道姓，而且**在跨过不可逆边界
/// 之前**超时可以安全重试。
const SEND_STAGE_RESOLVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);
const SEND_STAGE_AUTHORIZE_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);
/// 预提交这一级的特殊之处：它**可能要等同一条票据上更早的语义准入 resolve**
/// （`interrupt.rs` 的 `begin_outgoing_commit` 会一直等到那条准入 resolve 或到期，
/// 准入的预留租期是 180 秒）。别级是"内存里几步操作该多快"，这一级是"前一条回合
/// 还要跑多久"，两者不是一个量纲。
///
/// 原先沿用 3 秒，于是"前一条回合还在跑"就等于必然超时，代价是**把已经生成好的整条
/// 回复丢掉**（线上 2026-09-15 18:13 与 19:27 各一次，都是这个阶段）。这里给到一次
/// 模型调用同量级（`server.request_timeout_secs` 默认 60 秒），既覆盖常见的"等前一条
/// 说完"，又不至于让一条卡死的准入把回合无限拖住。
const SEND_STAGE_COMMIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
/// 真正调 QQ 接口那一级：超过它说明平台侧没有及时回应，此时**不能**断定没发出去，
/// 所以这一级超时按"结果未知"处理，而不是可重试。
const SEND_STAGE_TRANSPORT_BUDGET: std::time::Duration = std::time::Duration::from_secs(12);

/// "确定没发出去"的失败最多再试几次（每次都是一条新的完整尝试）。
///
/// 线上 2026-09-15 18:13 与 19:27 两次丢回复：回复已经生成好，收尾阶段超时，
/// 失败标记明明是 `retryable: true`——但那个标记只释放幂等预留，pipeline 里没有
/// 任何路径真的重发，于是整条回复就这么没了。这里把"真的重发一次"补上。
const SEND_PIPELINE_RETRIES: usize = 1;

/// 这个失败能不能断定"一条消息都没发出去"，因而可以安全重试。
///
/// 发送链路在真正调 QQ 之前有两级查库（会话路由、群白名单）与一次内存预提交，它们
/// 超时都意味着**没跨过不可逆边界**（`send_stage_timeout_error` 的注释）；真正调 QQ
/// 那一级（`transport_send`）超时是"结果未知"，走 `DeliveryIndeterminate`，压根不会
/// 变成这里能看到的错误类别——重发它会重复发消息。所以判据只认前两级加预提交。
fn send_failure_is_definitely_not_sent(error: &ActionPortError) -> bool {
    let Some(stage) = error.category.strip_prefix(SEND_STAGE_TIMEOUT_PREFIX) else {
        return false;
    };
    matches!(
        stage,
        SEND_STAGE_RESOLVE | SEND_STAGE_AUTHORIZE | SEND_STAGE_COMMIT
    )
}

/// "确定没发出去就再试一次"的循环。
///
/// 抽成独立函数是为了能直接测它：真正的发送链路要机器人运行时，测不动；而"重试几次、
/// 什么情况下不重试"恰恰是最容易写错、代价又最大的地方（少重试一次＝丢一条回复，
/// 多重试一次＝重复发消息）。
async fn retry_definitely_not_sent<T, F, Fut>(mut attempt_once: F) -> Result<T, ActionPortError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ActionPortError>>,
{
    let mut attempt = 0_usize;
    loop {
        match attempt_once().await {
            Err(error)
                if attempt < SEND_PIPELINE_RETRIES
                    && send_failure_is_definitely_not_sent(&error) =>
            {
                attempt += 1;
                kovi::log::warn!(
                    "Yunxi send retry after a definitely-not-sent failure: attempt={attempt}/{SEND_PIPELINE_RETRIES} category={}",
                    error.category,
                );
            }
            other => return other,
        }
    }
}

/// 给发送链路的一个阶段套预算，超时时留下**带阶段名**的一行。
async fn with_send_stage_budget<T>(
    stage: &'static str,
    budget: std::time::Duration,
    conversation_id: ConversationId,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    match kovi::tokio::time::timeout(budget, future).await {
        Ok(value) => Some(value),
        Err(_) => {
            kovi::log::warn!(
                "Yunxi send stage timed out: stage={stage} budget_ms={} conversation_id={conversation_id} action=abort_before_send",
                budget.as_millis(),
            );
            None
        }
    }
}

/// 发送链路各阶段的名字与超时错误类别前缀。
///
/// 阶段名在三个地方用到（打预算时、造错误时、判断"能不能安全重试"时），所以只在这里
/// 写一次：靠字面量在三处各写一遍，改一处就会让重试判据悄悄失效。
const SEND_STAGE_RESOLVE: &str = "resolve_destination";
const SEND_STAGE_AUTHORIZE: &str = "authorize_group";
const SEND_STAGE_COMMIT: &str = "begin_outgoing_commit";
const SEND_STAGE_TIMEOUT_PREFIX: &str = "send_stage_timeout:";

/// 发送前阶段超时的统一文案：没跨过不可逆边界，所以可重试。
fn send_stage_timeout_error(stage: &str) -> ActionPortError {
    ActionPortError::new(format!("{SEND_STAGE_TIMEOUT_PREFIX}{stage}"), true)
}

#[derive(Clone, Copy)]
struct QqSendContext<'a> {
    revalidation_target: DeliveryRevalidationTarget,
    expected_destination: QqDestination,
    content: &'a MessageContent,
    reply_to: Option<MessageId>,
    expected_conversation_id: ConversationId,
    idempotency_key: &'a str,
    outgoing: OutgoingToken,
}

#[derive(Clone)]
struct CoreToolEffectRevalidator {
    adapter: QqActionAdapter,
    registry: Arc<ToolRegistry>,
    action: ToolAction,
    ticket: crate::model::ReplyTicket,
    source_message_id: Option<i32>,
    expected_actor_user_id: i64,
    expected_conversation_id: ConversationId,
    expected_destination: QqDestination,
}

impl ToolEffectRevalidator for CoreToolEffectRevalidator {
    fn revalidate(&self) -> ToolEffectRevalidationFuture<'_> {
        Box::pin(async move { self.adapter.revalidate_tool_effect(self).await })
    }
}

#[derive(Debug, Error)]
#[error("QQ delivery adapter {operation} failed: {detail}")]
struct QqAdapterFailure {
    operation: &'static str,
    detail: String,
}

impl QqAdapterFailure {
    fn new(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            detail: detail.into(),
        }
    }
}

/// Kovi host implementation of both Core delivery resolution and action
/// execution. A single adapter keeps the login identity and mapping policy
/// consistent between `ReachOut` resolution and actual sends.
#[derive(Clone)]
pub(crate) struct QqActionAdapter {
    bot: Arc<RuntimeBot>,
    identity_store: Arc<PostgresIdentityStore>,
    delivery_ledger: Arc<PostgresDeliveryLedger>,
    open_loop_store: Arc<dyn OpenLoopStore>,
    goal_store: Arc<dyn GoalStore>,
    tool_turns: Arc<HostToolTurnRegistry>,
    /// Login info is stable for a running Kovi bot. Cache it after the first
    /// successful lookup, but leave it unset when the API is temporarily down
    /// so a later action can retry.
    self_id: Arc<Mutex<Option<i64>>>,
}

impl fmt::Debug for QqActionAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QqActionAdapter")
            .field(
                "self_id_cached",
                &self.self_id.try_lock().ok().and_then(|id| *id),
            )
            .finish_non_exhaustive()
    }
}

impl QqActionAdapter {
    pub(crate) fn new(
        bot: Arc<RuntimeBot>,
        identity_store: Arc<PostgresIdentityStore>,
        open_loop_store: Arc<dyn OpenLoopStore>,
        goal_store: Arc<dyn GoalStore>,
        tool_turns: Arc<HostToolTurnRegistry>,
    ) -> Arc<Self> {
        let delivery_ledger = super::delivery_ledger()
            .expect("Yunxi delivery ledger must be initialized before the action adapter");
        Arc::new(Self {
            bot,
            identity_store,
            delivery_ledger,
            open_loop_store,
            goal_store,
            tool_turns,
            self_id: Arc::new(Mutex::new(None)),
        })
    }

    async fn resolve_tool_actor_user_id(
        &self,
        actor: yunxi_core::PersonId,
    ) -> Result<i64, ActionPortError> {
        self.identity_store
            .qq_external_identity_for_delivery(actor)
            .await
            .map_err(|error| {
                ActionPortError::new(format!("tool_actor_lookup_failed:{error}"), true)
            })?
            .and_then(|value| parse_positive_i64(&value))
            .ok_or_else(|| ActionPortError::new("tool_actor_route_unavailable", false))
    }

    async fn revalidate_tool_effect(
        &self,
        binding: &CoreToolEffectRevalidator,
    ) -> Result<ToolExecutionContext, String> {
        let actor = binding
            .action
            .actor()
            .ok_or_else(|| "tool_actor_required".to_string())?;
        let route_guard = crate::yunxi::pin_delivery_routes().await;
        let actor_user_id = self
            .resolve_tool_actor_user_id(actor)
            .await
            .map_err(|error| error.to_string())?;
        if actor_user_id != binding.expected_actor_user_id {
            return Err("tool_actor_route_changed_at_effect_boundary".to_string());
        }
        if let ActionScope::Conversation(conversation_id) = binding.action.scope {
            match self.identity_store.get(conversation_id, actor).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err("tool_scope_membership_revoked_at_effect_boundary".to_string());
                }
                Err(error) => {
                    return Err(format!(
                        "tool_scope_membership_effect_revalidation_failed:{error}"
                    ));
                }
            }
        }
        let current_route = match binding.action.scope {
            ActionScope::Conversation(conversation_id) => self
                .resolve_conversation_destination_without_authorization(conversation_id)
                .await
                .map(|destination| (conversation_id, destination))
                .map_err(|error| error.to_string())?,
            ActionScope::Person(person_id) => self
                .resolve_person_destination(person_id)
                .await
                .map(|(route, destination)| (route.conversation_id, destination))
                .map_err(|error| error.to_string())?,
            ActionScope::Global => return Err("global_tool_scope_rejected".to_string()),
        };
        if !delivery_route_is_unchanged(
            binding.expected_conversation_id,
            binding.expected_destination,
            Some(current_route),
        ) {
            return Err("tool_route_changed_at_effect_boundary".to_string());
        }
        let destination = current_route.1;
        if binding.ticket.scope() != destination.reply_scope() {
            return Err("tool_ticket_route_mismatch_at_effect_boundary".to_string());
        }
        let group_authorization = match destination {
            QqDestination::Group(group_id) => Some(
                crate::group_access::authorize_group_send(group_id)
                    .await
                    .map_err(|error| format!("tool_group_authorization_revoked:{error}"))?,
            ),
            QqDestination::Private(_) => None,
        };
        let configured_owner = crate::config::get().identity().owner_person_id();
        let is_main_admin = configured_owner.is_some_and(|owner| owner == actor.into_uuid())
            || (configured_owner.is_none()
                && self
                    .bot
                    .get_main_admin()
                    .ok()
                    .is_some_and(|main_admin| main_admin == actor_user_id));
        let group_paused = match destination {
            QqDestination::Group(group_id) => crate::model::utils::is_group_paused(group_id).await,
            QqDestination::Private(_) => false,
        };
        if !is_current(binding.ticket).await {
            return Err("tool_turn_stale_at_effect_boundary".to_string());
        }
        let context = ToolExecutionContext {
            subject_id: actor_user_id,
            actor_user_id,
            is_admin: is_main_admin,
            is_main_admin,
            context: "yunxi_core_tool",
            destination: destination.message_destination(),
            source_message_id: binding.source_message_id,
            scheduled: false,
            group_paused,
            runtime_bot: Some(Arc::clone(&self.bot)),
            sticker_teaching: None,
            requires_reminder_create: false,
            requires_agent_run_create: false,
            requires_group_message_send: false,
            requires_group_followup: false,
            requires_external_tool: false,
            allow_reply_actions: false,
        };
        if !binding
            .registry
            .available_for_context(&binding.action.tool_name, &context)
        {
            return Err("tool_unavailable_in_revalidated_context".to_string());
        }
        drop(group_authorization);
        drop(route_guard);
        Ok(context)
    }

    async fn current_self_id(&self) -> Result<i64, QqAdapterFailure> {
        if let Some(self_id) = *self.self_id.lock().await {
            return Ok(self_id);
        }

        let response =
            self.bot.get_login_info().await.map_err(|error| {
                QqAdapterFailure::new("login lookup", format_api_return(&error))
            })?;
        let self_id = response
            .data
            .get("user_id")
            .and_then(value_as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                QqAdapterFailure::new(
                    "login lookup",
                    "Kovi returned no positive user_id in get_login_info",
                )
            })?;
        *self.self_id.lock().await = Some(self_id);
        Ok(self_id)
    }

    async fn resolve_person_destination(
        &self,
        person_id: yunxi_core::PersonId,
    ) -> Result<(DeliveryRoute, QqDestination), DeliveryResolutionError> {
        let self_id = self
            .current_self_id()
            .await
            .map_err(DeliveryResolutionError::failed)?;
        let route = self
            .identity_store
            .resolve_qq_direct_for_person_delivery(person_id, self_id)
            .await
            .map_err(|error| {
                DeliveryResolutionError::failed(QqAdapterFailure::new(
                    "person delivery route lookup",
                    error.to_string(),
                ))
            })?;
        let Some((conversation_id, user_id)) = route else {
            return Err(DeliveryResolutionError::Unavailable { person_id });
        };
        Ok((
            DeliveryRoute::new(conversation_id, ConversationKind::Direct),
            QqDestination::Private(user_id),
        ))
    }

    async fn resolve_conversation_destination(
        &self,
        conversation_id: ConversationId,
    ) -> Result<QqDestination, ActionPortError> {
        // 发送链路第一级：查会话路由（数据库）。撞上连接池饥饿时在这里就失败，
        // 而不是拖到 30 秒后连阶段名都没有。
        let destination = with_send_stage_budget(
            SEND_STAGE_RESOLVE,
            SEND_STAGE_RESOLVE_BUDGET,
            conversation_id,
            self.resolve_conversation_destination_without_authorization(conversation_id),
        )
        .await
        .ok_or_else(|| send_stage_timeout_error(SEND_STAGE_RESOLVE))??;
        if let QqDestination::Group(group_id) = destination {
            let authorized = with_send_stage_budget(
                SEND_STAGE_AUTHORIZE,
                SEND_STAGE_AUTHORIZE_BUDGET,
                conversation_id,
                crate::group_access::is_authorized_group(group_id),
            )
            .await
            .ok_or_else(|| send_stage_timeout_error(SEND_STAGE_AUTHORIZE))?
            .map_err(|error| {
                ActionPortError::new(format!("group_authorization_unavailable:{error}"), true)
            })?;
            if !delivery_authorization_allows(destination, Some(authorized)) {
                return Err(ActionPortError::new("group_not_authorized", false));
            }
        }
        Ok(destination)
    }

    async fn resolve_conversation_destination_without_authorization(
        &self,
        conversation_id: ConversationId,
    ) -> Result<QqDestination, ActionPortError> {
        let mappings = self
            .identity_store
            .qq_external_conversations_for_id(conversation_id)
            .await
            .map_err(|error| {
                ActionPortError::new(format!("delivery_lookup_failed:{error}"), true)
            })?;
        let [(external_id, kind)] = mappings.as_slice() else {
            return Err(ActionPortError::new(
                if mappings.is_empty() {
                    "delivery_route_unavailable"
                } else {
                    "delivery_route_ambiguous"
                },
                true,
            ));
        };
        let self_id = if *kind == ConversationKind::Direct {
            Some(
                self.current_self_id()
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))?,
            )
        } else {
            None
        };
        parse_qq_destination(external_id, *kind, self_id)
            .ok_or_else(|| ActionPortError::new("delivery_route_invalid", false))
    }

    /// 发送链路的入口：`send_to_destination` + 一次"确定没发出去"的重试。
    ///
    /// 只重试 [`send_failure_is_definitely_not_sent`] 认下的失败：那些阶段都在真正调
    /// QQ 之前，重试不会重复发消息；而重试的收益很直接——回复已经生成好了，重试一次
    /// 就发得出去，不重试用户什么都收不到。预提交动作在失败路径上**不会**被释放
    /// （释放只发生在解析失败那一支），所以第二次尝试拿到的还是同一个 token。
    async fn send_to_destination_with_retry(
        &self,
        context: QqSendContext<'_>,
    ) -> Result<ActionPortOutcome, ActionPortError> {
        retry_definitely_not_sent(|| self.send_to_destination(context)).await
    }

    async fn send_to_destination(
        &self,
        context: QqSendContext<'_>,
    ) -> Result<ActionPortOutcome, ActionPortError> {
        let QqSendContext {
            revalidation_target,
            expected_destination,
            content,
            reply_to: core_reply_to,
            expected_conversation_id,
            idempotency_key,
            outgoing,
        } = context;
        // 源消息刚被撤回就别发：Host 链路在生成前后各拦一次
        // （`has_recalled_messages` + `begin_reply` 里的撤回判据），而 Core 这一轮
        // 由 yunxi-core 驱动，投递端口是唯一能拦住的地方——发出去就收不回来了
        // （QQ 只允许撤自己 2 分钟内的消息）。这里放在最前面：连语音合成都不必做。
        //
        // 判据是入站时登记的"这一轮在答哪几条"（`remember_core_turn_sources`）；
        // 没有登记（主动消息、刚重启）时一律放行，宁可发出去也不要吞掉正常回复。
        if crate::model::core_turn_blocked_by_recall(expected_destination.reply_scope()).await {
            kovi::log::info!(
                "Yunxi Core input recalled: conversation_id={expected_conversation_id} action=discard"
            );
            return Ok(ActionPortOutcome::Deferred {
                reason: "input_recalled_before_delivery".to_string(),
            });
        }
        // Core 标记了语音/唱歌就交给本机合成一条 QQ 语音；配置、合成或落盘任何
        // 一步不成立都退回文字——表达方式不该把这条回复弄丢。
        //
        // **必须在 `begin_outgoing_commit` 之前**：那一步会武装 30 秒的 precommit
        // 租约，而合成是网络调用（qq_sing 默认超时 45 秒、可配到 180 秒；退回 TTS
        // 也有 20 秒）。放在租约内的话，一次"慢但成功"的合成会让 commit 拿到
        // `Stale`，整条已经渲染好的回复被丢弃且不重试——用户什么都收不到。
        // 音频只取决于 content 与配置，不依赖路由与授权，所以提前不影响下面那条
        // "路由与授权必须是 commit 前最后两个 await" 的不变量。
        let speech_message = {
            let config = crate::config::get();
            speech_message_for(content, config.qq_voice(), config.qq_sing()).await
        };
        if (content.is_sing() || content.is_voice()) && speech_message.is_none() {
            kovi::log::warn!(
                "语音消息不可用，本轮已回退成文字: conversation_id={expected_conversation_id} singing={}",
                content.is_sing()
            );
        }
        // 表情包同样在 commit 之前解析：它只取决于 content 与素材库，不依赖路由与
        // 授权，提前不破坏上面那条不变量；读一张本地图也不是慢操作，不会拖垮 30 秒
        // 的 precommit 租约。放在这里还让"什么都没得发"的轮次在提交前就被挡住。
        let sticker_message =
            sticker_segment_for(content, speech_message.is_some(), expected_conversation_id);
        // 她只想发一张表情、那张却取不到时，**不能整轮沉默**：群里看到的会是
        // "她掉线了"（线上 2026-09-15 02:16:10 就是这么被丢掉的）。宿主替她说一句
        // 最短的实话，投递照常走完；她本来有正文的话，正文一个字都不动。
        let mut text_override = None;
        if speech_message.is_none() && content.as_text().trim().is_empty() {
            match missing_sticker_fallback(content, sticker_message.is_some()) {
                Some(fallback) => {
                    kovi::log::warn!(
                        "只发表情的这一轮表情取不到，改用兜底文字投递: conversation_id={expected_conversation_id} label={:?}",
                        content.sticker_label()
                    );
                    text_override = Some(fallback);
                }
                None if sticker_message.is_none() => {
                    kovi::log::warn!(
                        "这一轮没有任何可发送内容，投递已放弃: conversation_id={expected_conversation_id} sticker={:?}",
                        content.sticker_label()
                    );
                    return Ok(ActionPortOutcome::Deferred {
                        reason: "empty_visible_delivery".to_string(),
                    });
                }
                None => {}
            }
        }
        let precommit = match with_send_stage_budget(
            SEND_STAGE_COMMIT,
            SEND_STAGE_COMMIT_BUDGET,
            expected_conversation_id,
            begin_outgoing_commit(outgoing),
        )
        .await
        .ok_or_else(|| send_stage_timeout_error(SEND_STAGE_COMMIT))?
        {
            Ok(precommit) => precommit,
            Err(OutgoingCommitRejection::Stale) => {
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "outgoing_superseded_before_revalidation".to_string(),
                });
            }
            Err(OutgoingCommitRejection::DuplicateIdempotency) => {
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Ok(ActionPortOutcome::DeliveryIndeterminate {
                    reason: "outgoing_duplicate_idempotency_key".to_string(),
                    conversation_id: Some(expected_conversation_id),
                });
            }
        };
        // Resolve the optional quote before the final destination check. Quote
        // degradation is stylistic; route and authorization are security
        // boundaries and therefore must be the last awaited lookups before
        // the serialized commit.
        let external_reply_to = if let Some(reply_to) = core_reply_to {
            match self
                .identity_store
                .qq_message_id_for_core(reply_to, expected_conversation_id)
                .await
            {
                Ok(Some(external_id)) => match i32::try_from(external_id) {
                    Ok(external_id) => Some(i64::from(external_id)),
                    Err(_) => {
                        kovi::log::warn!(
                            "Yunxi reply target was outside the OneBot message-id range; sending without a reply segment"
                        );
                        None
                    }
                },
                Ok(None) => {
                    kovi::log::warn!(
                        "Yunxi reply target had no QQ mapping in the expected conversation; sending without a reply segment"
                    );
                    None
                }
                Err(error) => {
                    kovi::log::warn!(
                        "Yunxi reply mapping lookup failed before route revalidation; sending without a reply segment: {error}"
                    );
                    None
                }
            }
        } else {
            None
        };
        // 路由与授权是最后两个 await，各自前面再续一次租。
        if !precommit.renew().await {
            return Ok(ActionPortOutcome::Deferred {
                reason: "outgoing_superseded_before_route_revalidation".to_string(),
            });
        }
        let route_guard = crate::yunxi::pin_delivery_routes().await;
        let revalidated = match revalidation_target {
            DeliveryRevalidationTarget::Conversation(conversation_id) => self
                .resolve_conversation_destination_without_authorization(conversation_id)
                .await
                .map(|destination| (conversation_id, destination)),
            DeliveryRevalidationTarget::Person(person_id) => self
                .resolve_person_destination(person_id)
                .await
                .map_err(|error| ActionPortError::new(error.to_string(), true))
                .map(|(route, destination)| (route.conversation_id, destination)),
        };
        let (conversation_id, destination) = match revalidated {
            Ok(current)
                if delivery_route_is_unchanged(
                    expected_conversation_id,
                    expected_destination,
                    Some(current),
                ) =>
            {
                current
            }
            Ok(_) => {
                drop(route_guard);
                mark_outgoing_failed(outgoing).await;
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "delivery_route_changed_before_commit".to_string(),
                });
            }
            Err(error) => {
                drop(route_guard);
                mark_outgoing_failed(outgoing).await;
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Err(error);
            }
        };
        let authorization = match destination {
            QqDestination::Group(group_id) => {
                match crate::group_access::authorize_group_send(group_id).await {
                    Ok(authorization) => Some(authorization),
                    Err(error) => {
                        drop(route_guard);
                        mark_outgoing_failed(outgoing).await;
                        crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                        return Err(ActionPortError::new(
                            format!("group_not_authorized_before_commit:{error}"),
                            false,
                        ));
                    }
                }
            }
            QqDestination::Private(_) => None,
        };
        // 兜底文字只在"本来没有正文"时出现，所以这里不会覆盖她写的话。
        let text = text_override
            .as_deref()
            .unwrap_or_else(|| content.as_text());
        let message = outbound_message(text, external_reply_to, speech_message, sticker_message);
        let fingerprint_content =
            serde_json::to_string(content).unwrap_or_else(|_| content.as_text().to_owned());
        let fingerprint = contextual_outgoing_fingerprint(
            destination.reply_scope(),
            &fingerprint_content,
            external_reply_to,
            &[],
            Some(idempotency_key),
        );
        let durable_attempt = match DeliveryAttempt::new(
            revalidation_target.ledger_action_kind(),
            revalidation_target.ledger_target(),
            conversation_id,
            destination.ledger_kind(),
            destination.external_id(),
            content,
            core_reply_to,
            external_reply_to,
        ) {
            Ok(attempt) => attempt,
            Err(error) => {
                drop(authorization);
                drop(route_guard);
                mark_outgoing_failed(outgoing).await;
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Err(ActionPortError::new(
                    format!("durable_delivery_envelope_invalid:{error}"),
                    false,
                ));
            }
        };
        let Some(mind_delivery_permit) =
            crate::yunxi::pin_mind_outgoing_fence(idempotency_key).await
        else {
            drop(authorization);
            drop(route_guard);
            drop(precommit);
            crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
            return Ok(ActionPortOutcome::Deferred {
                reason: "mind_snapshot_changed_before_commit".to_string(),
            });
        };
        let delivery_ticket = outgoing.ticket();
        let commit_result = precommit.commit(fingerprint, Some(idempotency_key)).await;
        let committed = match commit_result {
            Ok(committed) => committed,
            Err(OutgoingCommitRejection::Stale) => {
                drop(authorization);
                drop(route_guard);
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "outgoing_superseded_before_commit".to_string(),
                });
            }
            Err(OutgoingCommitRejection::DuplicateIdempotency) => {
                drop(authorization);
                drop(route_guard);
                crate::yunxi::discard_mind_outgoing_fence(idempotency_key);
                return Ok(ActionPortOutcome::DeliveryIndeterminate {
                    reason: "outgoing_duplicate_idempotency_key".to_string(),
                    conversation_id: Some(conversation_id),
                });
            }
        };
        // Mind proposals are tied to the same outgoing action key. Releasing
        // them only after the serialized commit prevents a superseded reply
        // from writing state inferred by a turn that never won the race.
        crate::yunxi::commit_mind_candidates(idempotency_key);
        let durable_commit = self
            .delivery_ledger
            .commit_attempt(idempotency_key, &durable_attempt)
            .await;
        drop(authorization);
        drop(route_guard);
        let durable_committed = match durable_commit {
            Ok(DeliveryCommitOutcome::Acquired(committed_delivery)) => committed_delivery,
            Ok(DeliveryCommitOutcome::AlreadyRecorded {
                status,
                external_message_id,
            }) => {
                let outcome =
                    recorded_delivery_outcome(status, external_message_id, conversation_id)?;
                if matches!(outcome, ActionPortOutcome::Delivered { .. }) {
                    committed.mark_sent().await;
                } else {
                    drop(committed);
                }
                return Ok(outcome);
            }
            Ok(DeliveryCommitOutcome::EnvelopeConflict) => {
                committed.mark_failed().await;
                return Err(ActionPortError::new(
                    "durable_delivery_key_envelope_conflict",
                    false,
                ));
            }
            Err(error) => {
                committed.mark_failed().await;
                return Err(durable_commit_error(error));
            }
        };
        // 跨过不可逆边界的那一级：这里超时**不能**当作"没发出去"（请求可能已经
        // 到达平台），所以单独归为 DeliveryIndeterminate，交给上层按"结果未知"
        // 处理——这正是它与前面几级的区别。
        let Some(send_result) = with_send_stage_budget(
            "transport_send",
            SEND_STAGE_TRANSPORT_BUDGET,
            expected_conversation_id,
            MessageTransport::new(&self.bot).send(destination.message_destination(), message),
        )
        .await
        else {
            if let Err(ledger_error) = durable_committed.mark_unknown().await {
                kovi::log::warn!("transport timeout could not be marked Unknown: {ledger_error}");
            }
            drop(committed);
            return Ok(ActionPortOutcome::DeliveryIndeterminate {
                reason: "qq_transport_timeout".to_string(),
                conversation_id: Some(expected_conversation_id),
            });
        };
        drop(mind_delivery_permit);
        let message_id = match send_result {
            Ok(message_id) => {
                if let Err(error) = durable_committed.mark_sent(i64::from(message_id)).await {
                    // The network side effect is already irreversible. The
                    // guard records Unknown when possible, while Committed is
                    // itself a durable replay barrier if PostgreSQL is down.
                    kovi::log::warn!(
                        "QQ delivery succeeded but durable Sent persistence failed: {error}"
                    );
                }
                committed.mark_sent().await;
                message_id
            }
            Err(error) => {
                let indeterminate = error.is_indeterminate();
                if !indeterminate && error.is_group_send_denied() {
                    // 本地退避期间的跳过:发送方已记账,终态失败(不可重试)。
                    return Err(ActionPortError::new(
                        format!("qq_send_denied_backoff:{error}"),
                        false,
                    ));
                }
                if !indeterminate && qq_rejection_looks_muted(&error) {
                    // QQ 侧禁言/风控:记录退避并标记不可重试,避免在禁言
                    // 期间无限重试同一批回复。
                    if let MessageDestination::Group(group_id) = destination.message_destination() {
                        crate::model::send_guard::record_rejection(group_id).await;
                    }
                    return Err(ActionPortError::new(
                        format!("qq_send_denied:{error}"),
                        false,
                    ));
                }
                if indeterminate {
                    if let Err(ledger_error) = durable_committed.mark_unknown().await {
                        kovi::log::warn!(
                            "indeterminate QQ delivery could not be marked Unknown: {ledger_error}"
                        );
                    }
                    drop(committed);
                    return Ok(ActionPortOutcome::DeliveryIndeterminate {
                        reason: "qq_send_indeterminate".to_owned(),
                        conversation_id: Some(conversation_id),
                    });
                } else {
                    if let Err(ledger_error) =
                        durable_committed.mark_failed("qq_transport_rejected").await
                    {
                        kovi::log::warn!(
                            "rejected QQ delivery could not be marked Failed: {ledger_error}"
                        );
                    }
                    committed.mark_failed().await;
                }
                return Err(ActionPortError::new(
                    format!("qq_send_failed:{error}"),
                    true,
                ));
            }
        };
        record_standalone_bot_message(destination.reply_scope(), delivery_ticket, message_id, text)
            .await;
        let core_message_id = MessageId::new();
        if let Err(error) = self
            .identity_store
            .record_qq_message_mapping(
                core_message_id,
                conversation_id,
                i64::from(message_id),
                "outbound",
            )
            .await
        {
            // The platform send is already irreversible. Reporting the whole
            // action as failed would make reliable schedulers retry and send a
            // duplicate message, so retain successful delivery and surface the
            // degraded reply-mapping state through diagnostics instead.
            kovi::log::warn!(
                "Yunxi outbound message mapping could not be persisted after QQ delivery: {error}"
            );
        }
        Ok(ActionPortOutcome::Delivered {
            external_reference: Some(format!("qq-message:{message_id}")),
            message_id: Some(core_message_id),
            conversation_id: Some(conversation_id),
        })
    }

    async fn prepared_outgoing(
        &self,
        scope: ReplyScope,
        content: &MessageContent,
        idempotency_key: &str,
        allow_proactive_fallback: bool,
    ) -> Option<OutgoingToken> {
        let fingerprint = outgoing_fingerprint(content.as_text());
        // Core batches are prepared before the runtime adds platform-specific
        // envelope fields, so bind lookup to the durable action key. Retain
        // the legacy content-only lookup for old/proactive callers.
        let action_fingerprint = action_outgoing_fingerprint(content.as_text(), idempotency_key);
        let prepared = find_prepared_outgoing(scope, action_fingerprint).await;
        let prepared = match prepared {
            Some(prepared) => Some(prepared),
            None => find_prepared_outgoing(scope, fingerprint).await,
        };
        let (outgoing, source) = match prepared {
            // ReachOut is always proactive. Even an exact content collision
            // must not let it consume a reactive user's prepared reply.
            Some((_, OutgoingSource::Reply)) if allow_proactive_fallback => return None,
            Some(prepared) => prepared,
            None if allow_proactive_fallback => (
                prepare_proactive_outgoing_if_idle_with_semantic_preview(
                    scope,
                    fingerprint,
                    Some(content.as_text()),
                )
                .await?,
                OutgoingSource::Proactive,
            ),
            None => return None,
        };
        if source == OutgoingSource::Proactive {
            let grace_ms = crate::config::get().proactive().prepared_grace_ms();
            if grace_ms > 0 {
                kovi::tokio::time::sleep(std::time::Duration::from_millis(grace_ms)).await;
            }
        }
        Some(outgoing)
    }

    /// Execute a Core tool action only when the action carries enough
    /// canonical context to reconstruct one concrete QQ turn. Core actions
    /// intentionally do not carry raw QQ ids, so an anonymous/global tool
    /// request is rejected instead of being guessed into a host operation.
    async fn execute_tool(
        &self,
        action: &ToolAction,
    ) -> Result<ActionPortOutcome, ActionPortError> {
        let Some(claim) = self
            .tool_turns
            .claim_with_context(
                action.idempotency_key(),
                action.scope,
                &action.tool_name,
                &action.input,
            )
            .await
        else {
            return Ok(ActionPortOutcome::Deferred {
                reason: "tool_turn_capability_missing".to_string(),
            });
        };
        let ticket = claim.ticket;
        let result = async {
            let source_message_id = claim.source_message_id;
            let allowance = claim.allowance;
            let Some(registry) = tool_registry() else {
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_registry_unavailable".to_string(),
                });
            };
            let actor = action
                .actor()
                .ok_or_else(|| ActionPortError::new("tool_actor_required", false))?;
            let actor_user_id = self.resolve_tool_actor_user_id(actor).await?;

            let (expected_conversation_id, expected_destination) = match action.scope {
                ActionScope::Conversation(conversation_id) => {
                    if self
                        .identity_store
                        .get(conversation_id, actor)
                        .await
                        .map_err(|error| {
                            ActionPortError::new(
                                format!("tool_scope_membership_failed:{error}"),
                                true,
                            )
                        })?
                        .is_none()
                    {
                        return Err(ActionPortError::new(
                            "tool_scope_membership_required",
                            false,
                        ));
                    }
                    let destination = self
                        .resolve_conversation_destination_without_authorization(conversation_id)
                        .await?;
                    (conversation_id, destination)
                }
                ActionScope::Person(person_id) => {
                    if person_id != actor {
                        return Err(ActionPortError::new(
                            "tool_person_scope_actor_mismatch",
                            false,
                        ));
                    }
                    let (route, destination) = self
                        .resolve_person_destination(person_id)
                        .await
                        .map_err(|error| ActionPortError::new(error.to_string(), true))?;
                    (route.conversation_id, destination)
                }
                ActionScope::Global => {
                    return Ok(ActionPortOutcome::Deferred {
                        reason: "global_tool_scope_requires_host_context".to_string(),
                    });
                }
            };
            if ticket.scope() != expected_destination.reply_scope() {
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_turn_capability_scope_mismatch".to_string(),
                });
            }

            let arguments =
                serde_json::from_str::<serde_json::Value>(&action.input).map_err(|error| {
                    ActionPortError::new(format!("tool_input_invalid:{error}"), false)
                })?;
            let Some(arguments) = arguments.as_object().cloned() else {
                return Err(ActionPortError::new(
                    "tool_input_must_be_json_object",
                    false,
                ));
            };
            // Route deletion and authorization revocation take the corresponding
            // write locks. Pin both snapshots through the Host commit point, but
            // never across ToolRegistry execution or external I/O.
            let route_guard = crate::yunxi::pin_delivery_routes().await;
            let current_actor_user_id = match self.resolve_tool_actor_user_id(actor).await {
                Ok(user_id) if user_id == actor_user_id => user_id,
                Ok(_) => {
                    drop(route_guard);
                    return Ok(ActionPortOutcome::Deferred {
                        reason: "tool_actor_route_changed_before_commit".to_string(),
                    });
                }
                Err(error) => {
                    drop(route_guard);
                    return Err(error);
                }
            };
            if let ActionScope::Conversation(conversation_id) = action.scope {
                match self.identity_store.get(conversation_id, actor).await {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        drop(route_guard);
                        return Err(ActionPortError::new(
                            "tool_scope_membership_revoked_before_commit",
                            false,
                        ));
                    }
                    Err(error) => {
                        drop(route_guard);
                        return Err(ActionPortError::new(
                            format!("tool_scope_membership_revalidation_failed:{error}"),
                            true,
                        ));
                    }
                }
            }
            let current_route = match action.scope {
                ActionScope::Conversation(conversation_id) => self
                    .resolve_conversation_destination_without_authorization(conversation_id)
                    .await
                    .map(|destination| (conversation_id, destination)),
                ActionScope::Person(person_id) => self
                    .resolve_person_destination(person_id)
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))
                    .map(|(route, destination)| (route.conversation_id, destination)),
                ActionScope::Global => {
                    unreachable!("global tool scopes return before revalidation")
                }
            };
            let (_, destination) = match current_route {
                Ok(current)
                    if delivery_route_is_unchanged(
                        expected_conversation_id,
                        expected_destination,
                        Some(current),
                    ) =>
                {
                    current
                }
                Ok(_) => {
                    drop(route_guard);
                    return Ok(ActionPortOutcome::Deferred {
                        reason: "tool_route_changed_before_commit".to_string(),
                    });
                }
                Err(error) => {
                    drop(route_guard);
                    return Err(error);
                }
            };
            if ticket.scope() != destination.reply_scope() {
                drop(route_guard);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_turn_capability_route_mismatch".to_string(),
                });
            }
            let group_authorization = match destination {
                QqDestination::Group(group_id) => {
                    match crate::group_access::authorize_group_send(group_id).await {
                        Ok(authorization) => Some(authorization),
                        Err(error) => {
                            drop(route_guard);
                            return Err(ActionPortError::new(
                                format!("tool_group_authorization_revoked:{error}"),
                                false,
                            ));
                        }
                    }
                }
                QqDestination::Private(_) => None,
            };
            let configured_owner = crate::config::get().identity().owner_person_id();
            let is_main_admin = configured_owner.is_some_and(|owner| owner == actor.into_uuid())
                || (configured_owner.is_none()
                    && self
                        .bot
                        .get_main_admin()
                        .ok()
                        .is_some_and(|main_admin| main_admin == current_actor_user_id));
            // The Core action has no raw group-admin proof. Restrict admin tools
            // to the Host's main administrator, re-evaluated at commit time.
            let is_admin = is_main_admin;
            let group_paused = match destination {
                QqDestination::Group(group_id) => {
                    crate::model::utils::is_group_paused(group_id).await
                }
                QqDestination::Private(_) => false,
            };
            let Some(mind_delivery_permit) =
                crate::yunxi::pin_mind_outgoing_fence(action.idempotency_key()).await
            else {
                drop(group_authorization);
                drop(route_guard);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "mind_snapshot_changed_before_tool_effect".to_string(),
                });
            };
            if !mark_active(ticket).await {
                drop(group_authorization);
                drop(route_guard);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_turn_capability_stale_before_commit".to_string(),
                });
            }
            if !is_current(ticket).await {
                drop(group_authorization);
                drop(route_guard);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_turn_capability_stale_at_effect_boundary".to_string(),
                });
            }
            let context = ToolExecutionContext {
                subject_id: current_actor_user_id,
                actor_user_id: current_actor_user_id,
                is_admin,
                is_main_admin,
                context: "yunxi_core_tool",
                destination: destination.message_destination(),
                source_message_id,
                scheduled: false,
                group_paused,
                runtime_bot: Some(Arc::clone(&self.bot)),
                sticker_teaching: None,
                requires_reminder_create: false,
                requires_agent_run_create: false,
                requires_group_message_send: false,
                requires_group_followup: false,
                requires_external_tool: false,
                allow_reply_actions: false,
            };
            // 执行边的最后一道硬拦：档位与可用性都按**此刻**的事实重算。清单收窄只是
            // "不告诉她有这些工具"，被注入或幻觉出来的工具名必须在这里被拦掉；顺带也把
            // 注册之后才变化的状态（群被暂停、管理员身份被撤、素材库没了）重新算一遍。
            if !registry.available_for_allowance(&action.tool_name, &context, allowance) {
                drop(mind_delivery_permit);
                drop(group_authorization);
                drop(route_guard);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_allowance_rejected_at_effect_boundary".to_string(),
                });
            }
            drop(group_authorization);
            drop(route_guard);
            crate::yunxi::commit_mind_candidates(action.idempotency_key());
            let revalidator: Arc<dyn ToolEffectRevalidator> = Arc::new(CoreToolEffectRevalidator {
                adapter: self.clone(),
                registry: Arc::clone(&registry),
                action: action.clone(),
                ticket,
                source_message_id,
                expected_actor_user_id: actor_user_id,
                expected_conversation_id,
                expected_destination,
            });
            let result = registry
                .execute_with_revalidation(
                    &action.tool_name,
                    arguments,
                    context,
                    ticket,
                    revalidator,
                    allowance,
                )
                .await;
            drop(mind_delivery_permit);
            if result.succeeded {
                return Ok(ActionPortOutcome::ToolCompleted {
                    operation: action.tool_name.clone(),
                    output: bounded_core_tool_text(
                        &result.content,
                        MAX_TOOL_RESULT_CHARS,
                        MAX_TOOL_RESULT_BYTES,
                    ),
                });
            }
            Ok(ActionPortOutcome::ToolFailed {
                operation: action.tool_name.clone(),
                error_category: "tool_execution_failed".to_string(),
                detail: bounded_core_tool_text(
                    &result.content,
                    MAX_TOOL_ERROR_DETAIL_CHARS,
                    MAX_TOOL_ERROR_DETAIL_BYTES,
                ),
            })
        }
        .await;
        // Capability claims carry the active incoming ticket. Every path
        // after a successful claim, including validation and authorization
        // failures above, must release that ticket before returning.
        finish(ticket).await;
        result
    }
}

fn durable_commit_error(error: DeliveryCommitError) -> ActionPortError {
    match error {
        DeliveryCommitError::OwnerMissing { .. } => {
            ActionPortError::new(format!("durable_delivery_owner_missing:{error}"), false)
        }
        DeliveryCommitError::Ledger(_) => {
            ActionPortError::new(format!("durable_delivery_ledger_unavailable:{error}"), true)
        }
    }
}

impl DeliveryResolver for QqActionAdapter {
    fn resolve<'a>(&'a self, person_id: yunxi_core::PersonId) -> DeliveryResolverFuture<'a> {
        Box::pin(async move {
            self.resolve_person_destination(person_id)
                .await
                .map(|(route, _)| route)
        })
    }
}

impl ChannelAdapter for QqActionAdapter {
    fn platform_id(&self) -> PlatformId {
        PlatformId::new("qq").expect("qq is a valid Core platform id")
    }

    fn capabilities(&self) -> EnvironmentCapabilities {
        let mut capabilities = EnvironmentCapabilities::new([
            ActionDescriptor::new(ActionCapability::SendMessage),
            ActionDescriptor::new(ActionCapability::ReachOut),
            ActionDescriptor::new(ActionCapability::UseTool),
            ActionDescriptor::new(ActionCapability::CreateOpenLoop),
            ActionDescriptor::new(ActionCapability::ResolveOpenLoop),
            ActionDescriptor::new(ActionCapability::StartGoal),
            ActionDescriptor::new(ActionCapability::CancelGoal),
        ]);
        // Core refuses a tool the host has not declared, so the declarations
        // have to travel with the capability. Without them every tool call
        // would be rejected — including her own.
        let Some(registry) = crate::model::tool_registry() else {
            return capabilities;
        };
        capabilities.actions.extend(
            registry
                .declared_effects()
                .into_iter()
                .map(|(name, effect)| ActionDescriptor::tool(name, effect)),
        );
        capabilities
    }
}

impl ActionPort for QqActionAdapter {
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
        Box::pin(async move {
            match action {
                ProposedAction::SendMessage(send) => {
                    let destination = match self
                        .resolve_conversation_destination(send.conversation_id)
                        .await
                    {
                        Ok(destination) => destination,
                        Err(error) => {
                            release_prepared_action(&send.content, send.idempotency_key()).await;
                            return Err(error);
                        }
                    };
                    let reply_to = send.reply_to;
                    let Some(outgoing) = self
                        .prepared_outgoing(
                            destination.reply_scope(),
                            &send.content,
                            send.idempotency_key(),
                            false,
                        )
                        .await
                    else {
                        return Ok(ActionPortOutcome::Deferred {
                            reason: "outgoing_not_prepared".to_string(),
                        });
                    };
                    self.send_to_destination_with_retry(QqSendContext {
                        revalidation_target: DeliveryRevalidationTarget::Conversation(
                            send.conversation_id,
                        ),
                        expected_destination: destination,
                        content: &send.content,
                        reply_to,
                        expected_conversation_id: send.conversation_id,
                        idempotency_key: send.idempotency_key(),
                        outgoing,
                    })
                    .await
                }
                ProposedAction::ReachOut(reach_out) => {
                    let (route, destination) =
                        match self.resolve_person_destination(reach_out.person_id).await {
                            Ok(resolved) => resolved,
                            Err(error) => {
                                release_prepared_action(
                                    &reach_out.message,
                                    reach_out.idempotency_key(),
                                )
                                .await;
                                return Err(ActionPortError::new(error.to_string(), true));
                            }
                        };
                    let Some(outgoing) = self
                        .prepared_outgoing(
                            destination.reply_scope(),
                            &reach_out.message,
                            reach_out.idempotency_key(),
                            true,
                        )
                        .await
                    else {
                        return Ok(ActionPortOutcome::Deferred {
                            reason: "outgoing_not_prepared".to_string(),
                        });
                    };
                    self.send_to_destination_with_retry(QqSendContext {
                        revalidation_target: DeliveryRevalidationTarget::Person(
                            reach_out.person_id,
                        ),
                        expected_destination: destination,
                        content: &reach_out.message,
                        reply_to: None,
                        expected_conversation_id: route.conversation_id,
                        idempotency_key: reach_out.idempotency_key(),
                        outgoing,
                    })
                    .await
                }
                ProposedAction::UseTool(action) => self.execute_tool(action).await,
                ProposedAction::CreateOpenLoop(action) => {
                    let open_loop = self
                        .open_loop_store
                        .create(&action.draft)
                        .await
                        .map_err(store_action_error)?;
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some(format!("yunxi-open-loop:{}", open_loop.id())),
                        message_id: None,
                        conversation_id: open_loop.owner().conversation_id(),
                    })
                }
                ProposedAction::ResolveOpenLoop(action) => {
                    let open_loop = self
                        .open_loop_store
                        .get(action.open_loop_id)
                        .await
                        .map_err(store_action_error)?
                        .ok_or_else(|| ActionPortError::new("open_loop_not_found", false))?;
                    if open_loop.owner() != action.owner {
                        return Err(ActionPortError::new("open_loop_owner_mismatch", false));
                    }
                    let resolved = self
                        .open_loop_store
                        .resolve(action.open_loop_id, chrono::Utc::now())
                        .await
                        .map_err(store_action_error)?;
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some(format!(
                            "yunxi-open-loop-resolved:{}",
                            resolved.id()
                        )),
                        message_id: None,
                        conversation_id: resolved.owner().conversation_id(),
                    })
                }
                ProposedAction::StartGoal(action) => {
                    let goal = self
                        .goal_store
                        .create(&action.draft)
                        .await
                        .map_err(store_action_error)?;
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some(format!("yunxi-goal:{}", goal.id())),
                        message_id: None,
                        conversation_id: goal.owner().conversation_id(),
                    })
                }
                ProposedAction::CancelGoal(action) => {
                    let mut goal = self
                        .goal_store
                        .get(action.goal_id)
                        .await
                        .map_err(store_action_error)?
                        .ok_or_else(|| ActionPortError::new("goal_not_found", false))?;
                    if goal.owner() != action.owner {
                        return Err(ActionPortError::new("goal_owner_mismatch", false));
                    }
                    goal.transition(GoalState::Cancelled, chrono::Utc::now())
                        .map_err(|error| ActionPortError::new(error.to_string(), false))?;
                    let cancelled = self
                        .goal_store
                        .update(&goal)
                        .await
                        .map_err(store_action_error)?;
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some(format!(
                            "yunxi-goal-cancelled:{}",
                            cancelled.id()
                        )),
                        message_id: None,
                        conversation_id: cancelled.owner().conversation_id(),
                    })
                }
                ProposedAction::Noop => Ok(ActionPortOutcome::Delivered {
                    external_reference: None,
                    message_id: None,
                    conversation_id: None,
                }),
            }
        })
    }

    fn release_unexecuted<'a>(&'a self, action: &'a ProposedAction) -> ActionPortReleaseFuture<'a> {
        Box::pin(async move {
            match action {
                ProposedAction::UseTool(tool) => {
                    self.tool_turns.revoke(tool.idempotency_key()).await;
                    crate::yunxi::discard_mind_outgoing_fence(tool.idempotency_key());
                }
                ProposedAction::SendMessage(send) => {
                    release_prepared_action(&send.content, send.idempotency_key()).await;
                }
                ProposedAction::ReachOut(reach_out) => {
                    release_prepared_action(&reach_out.message, reach_out.idempotency_key()).await;
                }
                _ => {}
            }
        })
    }
}

async fn release_prepared_action(content: &MessageContent, key: &str) {
    let action_fingerprint = action_outgoing_fingerprint(content.as_text(), key);
    if let Some((token, _)) = find_prepared_outgoing_by_fingerprint(action_fingerprint).await {
        mark_outgoing_failed(token).await;
    }
    crate::yunxi::discard_mind_outgoing_fence(key);
}

/// Core 把这一轮标记成语音时，尝试合成一条可直接发送的 QQ 语音消息。
///
/// 返回 `None` 表示这一轮没有可发的语音（没标记、配置关闭、合成或落盘失败），
/// 调用方应当按文字发送——语音只是表达方式，不该把回复弄丢。
async fn voice_message_for(
    content: &MessageContent,
    voice_config: &crate::config::QqVoiceConfig,
) -> Option<Message> {
    if !content.is_voice() {
        return None;
    }
    crate::voice_reply::build_voice_message(voice_config, content.as_text()).await
}

/// 需要"说出来"的内容：唱歌优先（带旋律），唱不出来就退成念歌词；都失败返回
/// `None`，调用方按文字发送。唱歌服务不可用时不会把一条该唱的消息变成哑巴。
async fn speech_message_for(
    content: &MessageContent,
    voice_config: &crate::config::QqVoiceConfig,
    sing_config: &crate::config::QqSingConfig,
) -> Option<Message> {
    if let Some(template) = content.sing_template() {
        if let Some(sung) = crate::sing_reply::build_sing_message(
            voice_config,
            sing_config,
            template,
            content.as_text(),
        )
        .await
        {
            return Some(sung);
        }
        kovi::log::warn!("唱歌失败，本轮改用语音念出来: template={template}");
        return crate::voice_reply::build_voice_message(voice_config, content.as_text()).await;
    }
    voice_message_for(content, voice_config).await
}

/// 组装要发给 QQ 的那条消息：语音优先，合成成功时整条消息只有 `record` 段
/// （语音消息承载不了引用）；否则退回文字，引用照旧挂在第一条上，表情包贴在
/// 文字后面（QQ 允许文字与图片同处一条消息）。
fn outbound_message(
    text: &str,
    reply_to: Option<i64>,
    voice: Option<Message>,
    sticker: Option<Segment>,
) -> Message {
    if let Some(voice) = voice {
        return voice;
    }
    let mut message = Message::new();
    if let Some(reply_to) = reply_to {
        message.push(Segment::new("reply", json!({ "id": reply_to })));
    }
    if !text.is_empty() {
        message.push_text(text);
    }
    if let Some(sticker) = sticker {
        message.push(sticker);
    }
    message
}

/// 只发一张表情、那张又取不到时，宿主补的一句文字。
///
/// 返回 `None` 表示"照旧按空内容处理"（她本来有正文，或者压根没要求发表情）。
/// 有正文时不动正文：表情发不出去不该把已经写好的话也换掉。
fn missing_sticker_fallback(content: &MessageContent, sticker_resolved: bool) -> Option<String> {
    if sticker_resolved || !content.as_text().trim().is_empty() {
        return None;
    }
    let label = content.sticker_label()?;
    Some(crate::sticker_library::unavailable_sticker_reply(label))
}

/// 解析这一轮要附带的表情包素材段。
///
/// 语音/歌声整条替换消息（`record` 段），承载不了图片，所以发声轮次不再附带表情；
/// 素材库关掉、标签不在库里、文件读不出来、内容不像图片时都返回 `None`，投递退回
/// 纯文字——一张图发不出去不该把整条回复带走。
fn sticker_segment_for(
    content: &MessageContent,
    voiced: bool,
    conversation_id: ConversationId,
) -> Option<Segment> {
    let label = content.sticker_label()?;
    if voiced {
        kovi::log::warn!(
            "语音/歌声与表情包同时标记，本轮只发声: conversation_id={conversation_id} label={label}"
        );
        return None;
    }
    match crate::sticker_library::build_sticker_segment(label) {
        Some(segment) => Some(segment),
        None => {
            kovi::log::warn!(
                "表情包素材不可用，本轮只发文字: conversation_id={conversation_id} label={label}"
            );
            None
        }
    }
}

fn store_action_error(error: impl std::fmt::Display) -> ActionPortError {
    ActionPortError::new(format!("core_store_failed:{error}"), true)
}

fn recorded_delivery_outcome(
    status: DeliveryStatus,
    external_message_id: Option<i64>,
    conversation_id: ConversationId,
) -> Result<ActionPortOutcome, ActionPortError> {
    match status {
        DeliveryStatus::Sent => Ok(ActionPortOutcome::Delivered {
            external_reference: external_message_id
                .map(|message_id| format!("qq-message:{message_id}")),
            message_id: None,
            conversation_id: Some(conversation_id),
        }),
        DeliveryStatus::Prepared | DeliveryStatus::Committed | DeliveryStatus::Unknown => {
            Ok(ActionPortOutcome::DeliveryIndeterminate {
                reason: format!("durable_delivery_already_{status}"),
                conversation_id: Some(conversation_id),
            })
        }
        DeliveryStatus::Failed => Err(ActionPortError::new(
            "durable_delivery_failed_row_was_not_reacquired",
            false,
        )),
    }
}

fn bounded_core_tool_text(value: &str, max_chars: usize, max_bytes: usize) -> String {
    let mut bounded = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars().take(max_chars) {
        if bounded.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        bounded.push(character);
    }
    bounded
}

fn delivery_route_is_unchanged(
    expected_conversation_id: ConversationId,
    expected_destination: QqDestination,
    current: Option<(ConversationId, QqDestination)>,
) -> bool {
    current == Some((expected_conversation_id, expected_destination))
}

fn delivery_authorization_allows(
    destination: QqDestination,
    group_authorized: Option<bool>,
) -> bool {
    match destination {
        QqDestination::Group(_) => group_authorized == Some(true),
        QqDestination::Private(_) => true,
    }
}

fn parse_qq_destination(
    external_id: &str,
    kind: ConversationKind,
    current_self_id: Option<i64>,
) -> Option<QqDestination> {
    match kind {
        ConversationKind::Group => external_id
            .strip_prefix("group:")
            .and_then(parse_positive_i64)
            .map(QqDestination::Group),
        ConversationKind::Direct => {
            let mut parts = external_id.split(':');
            if parts.next() != Some("direct") {
                return None;
            }
            let self_id = parts.next()?;
            let peer_user_id = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let self_id = parse_positive_i64(self_id)?;
            let peer_user_id = parse_positive_i64(peer_user_id)?;
            if current_self_id != Some(self_id) {
                return None;
            }
            Some(QqDestination::Private(peer_user_id))
        }
        ConversationKind::System => None,
    }
}

fn parse_positive_i64(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|value| *value > 0)
}

fn value_as_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse::<i64>().ok()))
}

fn format_api_return(value: &kovi::ApiReturn) -> String {
    format!(
        "status={} retcode={} data={} echo={}",
        value.status, value.retcode, value.data, value.echo
    )
}

/// Parse a delivery lookup result conservatively. A person must have exactly
/// one positive numeric QQ identity; zero, malformed, and ambiguous mappings
/// are all unavailable until a delivery policy exists.
#[must_use]
pub(crate) fn single_positive_qq_id(external_ids: &[String]) -> Option<i64> {
    let [external_id] = external_ids else {
        return None;
    };
    let user_id = external_id.parse::<i64>().ok()?;
    (user_id > 0).then_some(user_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReachOutDeliveryOutcome {
    Delivered,
    Indeterminate,
    Failed,
}

impl ReachOutDeliveryOutcome {
    pub(crate) const fn is_terminal_attempt(self) -> bool {
        matches!(self, Self::Delivered | Self::Indeterminate)
    }

    pub(crate) const fn confirms_delivery(self) -> bool {
        matches!(self, Self::Delivered)
    }
}

fn compatibility_reach_out_outcome(
    result: Result<i32, crate::model::TrackedSendError>,
) -> ReachOutDeliveryOutcome {
    match result {
        Ok(_) => ReachOutDeliveryOutcome::Delivered,
        Err(
            crate::model::TrackedSendError::TransportIndeterminate(_)
            | crate::model::TrackedSendError::DuplicateIdempotency,
        ) => ReachOutDeliveryOutcome::Indeterminate,
        Err(_) => ReachOutDeliveryOutcome::Failed,
    }
}

pub(crate) async fn send_reach_out(
    bot: &Arc<RuntimeBot>,
    identity_store: &PostgresIdentityStore,
    intent: &ReachOutIntent,
    expected_user_id: i64,
) -> ReachOutDeliveryOutcome {
    // Shadow-mode World Model: simulate "send now / defer" for the host
    // before a high-value proactive send (v4 appendix §8). Pure values +
    // Simulated outcomes only; never affects the send itself.
    if crate::config::get().world_model().enabled()
        && let Ok(host) = yunxi_core::HostId::new("qq")
        && let Some(batch) = crate::yunxi::world_model::simulate_delivery(&host)
    {
        kovi::log::debug!(
            "[YUNXI_WORLD] delivery_simulate results={}",
            batch.results().len()
        );
    }
    let person_id = intent.person_id();
    let content: &MessageContent = intent.message();
    let delivery_key = compatibility_reach_out_key(intent);
    compatibility_reach_out_outcome(
        send_tracked_message_with_revalidation_guard(
            bot,
            MessageDestination::Private(expected_user_id),
            Message::from(content.as_text().to_string()),
            OutgoingSource::Proactive,
            Some(&delivery_key),
            || async {
                let route_guard = crate::yunxi::pin_delivery_routes().await;
                let Ok(Some(external_id)) = identity_store
                    .qq_external_identity_for_delivery(person_id)
                    .await
                else {
                    return None;
                };
                (single_positive_qq_id(&[external_id]) == Some(expected_user_id))
                    .then_some(route_guard)
            },
        )
        .await,
    )
}

fn compatibility_reach_out_key(intent: &ReachOutIntent) -> String {
    let mut hasher = Sha256::new();
    match serde_json::to_vec(intent) {
        Ok(encoded) => hasher.update(encoded),
        Err(_) => hasher.update(intent.message().as_text().as_bytes()),
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "legacy-reach-out:{}:{}:{}",
        intent.person_id(),
        chrono::Utc::now().format("%Y%m%d"),
        digest
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ActionPortError, SEND_PIPELINE_RETRIES, SEND_STAGE_AUTHORIZE, SEND_STAGE_COMMIT,
        SEND_STAGE_RESOLVE, missing_sticker_fallback, retry_definitely_not_sent,
        send_failure_is_definitely_not_sent, send_stage_timeout_error,
    };

    /// 只发一张表情、那张取不到时必须有兜底文字；她本来有正文时一个字都不动。
    ///
    /// 线上 2026-09-15 02:16:10 就是因为没有这条兜底，整轮被丢弃、群里彻底沉默。
    #[test]
    fn a_missing_sticker_only_turn_falls_back_to_text() {
        let sticker_only = MessageContent::sticker("", "猫猫歪头");
        let fallback =
            missing_sticker_fallback(&sticker_only, false).expect("只发表情又取不到时必须有兜底");
        assert!(fallback.contains("猫猫歪头"));

        // 表情正常发得出去：不代替她说话。
        assert_eq!(missing_sticker_fallback(&sticker_only, true), None);
        // 有正文：正文照发，兜底不插嘴。
        let with_text = MessageContent::sticker("我在的呀。", "猫猫歪头");
        assert_eq!(missing_sticker_fallback(&with_text, false), None);
        // 压根没要求发表情：走原来的"空内容"判定。
        assert_eq!(
            missing_sticker_fallback(&MessageContent::text(""), false),
            None
        );
    }

    /// 只有"确定没发出去"的失败才允许重试。
    ///
    /// 这条判据守的是两件相反的事：漏判会让已经生成好的回复被整条丢掉（线上
    /// 2026-09-15 18:13、19:27 各一次，都是预提交阶段超时）；误判会重复发消息。
    /// 所以真正调 QQ 那一级（`transport_send`）**必须**落在外面——它超时走的是
    /// `DeliveryIndeterminate`，这里顺带把"假如它变成一个错误"也钉死。
    #[test]
    fn only_pre_transport_stage_timeouts_are_definitely_not_sent() {
        for stage in [SEND_STAGE_RESOLVE, SEND_STAGE_AUTHORIZE, SEND_STAGE_COMMIT] {
            assert!(
                send_failure_is_definitely_not_sent(&send_stage_timeout_error(stage)),
                "{stage} 在真正调 QQ 之前超时，重试是安全的"
            );
        }
        // 真正调 QQ 那一级：请求可能已经到了平台，重发就是重复发消息。
        assert!(!send_failure_is_definitely_not_sent(
            &send_stage_timeout_error("transport_send")
        ));
        // 别的失败各有各的语义（授权不通过、QQ 侧拒绝、没有可发送内容……），
        // 不能因为标着 retryable 就一律重发。
        for category in [
            "group_not_authorized",
            "qq_send_failed:muted",
            "durable_commit_failed",
        ] {
            assert!(
                !send_failure_is_definitely_not_sent(&ActionPortError::new(category, true)),
                "{category} 不该被当成“确定没发出去”"
            );
        }
    }

    /// 重试循环：确定没发出去时再试一次，其余失败原样返回（不重发）。
    #[test]
    fn the_send_pipeline_retries_exactly_once_and_only_before_the_irreversible_boundary() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                use std::sync::atomic::{AtomicUsize, Ordering};

                // 第一次预提交超时、第二次成功：这正是线上丢回复的形态。
                let calls = AtomicUsize::new(0);
                let outcome: Result<&str, ActionPortError> = retry_definitely_not_sent(|| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            Err(send_stage_timeout_error(SEND_STAGE_COMMIT))
                        } else {
                            Ok("sent")
                        }
                    }
                })
                .await;
                assert_eq!(outcome.expect("第二次应当成功"), "sent");
                assert_eq!(calls.load(Ordering::SeqCst), 2);

                // 一直超时：只重试 SEND_PIPELINE_RETRIES 次就放弃，不能无限重发。
                let calls = AtomicUsize::new(0);
                let outcome: Result<&str, ActionPortError> = retry_definitely_not_sent(|| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err(send_stage_timeout_error(SEND_STAGE_COMMIT)) }
                })
                .await;
                assert!(outcome.is_err());
                assert_eq!(calls.load(Ordering::SeqCst), SEND_PIPELINE_RETRIES + 1);

                // 跨过不可逆边界之后（transport），或别的语义失败：一次都不重试。
                for category in ["transport_send", "group_not_authorized"] {
                    let calls = AtomicUsize::new(0);
                    let outcome: Result<&str, ActionPortError> = retry_definitely_not_sent(|| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if category == "transport_send" {
                                Err(send_stage_timeout_error("transport_send"))
                            } else {
                                Err(ActionPortError::new("group_not_authorized", true))
                            }
                        }
                    })
                    .await;
                    assert!(outcome.is_err(), "{category} 应当原样返回失败");
                    assert_eq!(calls.load(Ordering::SeqCst), 1, "{category} 不该被重试");
                }
            });
    }

    /// 阶段预算的行为：正常完成原样返回；超时返回 None，并且**不**把结果当成
    /// 成功——调用方据此决定"未跨边界可重试"还是"结果未知"。
    #[test]
    fn send_stage_budget_returns_none_only_on_timeout() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let conversation_id = yunxi_core::ConversationId::new();
                let fast = with_send_stage_budget(
                    "test_fast",
                    std::time::Duration::from_secs(1),
                    conversation_id,
                    async { 7_u8 },
                )
                .await;
                assert_eq!(fast, Some(7));

                let slow = with_send_stage_budget(
                    "test_slow",
                    std::time::Duration::from_millis(20),
                    conversation_id,
                    async {
                        kovi::tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        7_u8
                    },
                )
                .await;
                assert_eq!(slow, None);
            });
    }

    use super::{
        QqDestination, ReachOutDeliveryOutcome, compatibility_reach_out_outcome,
        delivery_authorization_allows, delivery_route_is_unchanged, durable_commit_error,
        outbound_message, parse_qq_destination, recorded_delivery_outcome, single_positive_qq_id,
        speech_message_for, voice_message_for, with_send_stage_budget,
    };
    use crate::model::TrackedSendError;
    use crate::yunxi::delivery_ledger::{DeliveryCommitError, DeliveryStatus};
    use kovi::Message;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;
    use yunxi_core::{ActionPortOutcome, ConversationId, ConversationKind, MessageContent};

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    /// 一份可用的 `[qq_voice]`：暂存目录指向本测试独占的临时目录。
    fn voice_config(
        staging_dir: &str,
        tts_url: &str,
        enabled: bool,
    ) -> crate::config::QqVoiceConfig {
        serde_json::from_value(serde_json::json!({
            "enabled": enabled,
            "tts_url": tts_url,
            "tts_timeout_secs": 5,
            "sample_rate": 16_000,
            "max_chars": 80,
            "staging_dir": staging_dir,
            "napcat_staging_dir": "/app/qq-call/voice",
            "keep_files": 8,
        }))
        .expect("qq_voice test config")
    }

    fn staging_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kovi-voice-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 只回一句固定 PCM 的假 TTS：验证的是"合成结果怎么变成 QQ 消息"，
    /// 不是模型推理本身。
    async fn spawn_stub_tts(pcm: Vec<u8>) -> String {
        let listener = kovi::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub tts");
        let addr = listener.local_addr().expect("stub tts address");
        kovi::tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use kovi::tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0_u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let mut response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/L16\r\nX-Sample-Rate: 16000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    pcm.len()
                )
                .into_bytes();
                response.extend_from_slice(&pcm);
                let _ = socket.write_all(&response).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://127.0.0.1:{}/v1/tts", addr.port())
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        kovi::tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(future)
    }

    /// 假 TTS 与调用方必须在同一个 runtime 里：runtime 一丢，后台 accept 任务
    /// 也会跟着消失。
    /// 假的歌声合成服务：只回一段固定字节，验证的是"唱歌结果怎么变成 QQ 消息"。
    async fn spawn_stub_sing(payload: Vec<u8>, status: u16) -> String {
        use kovi::tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = kovi::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub sing");
        let port = listener.local_addr().expect("stub sing address").port();
        kovi::tokio::spawn(async move {
            // 同一轮里会被问两次：先取模板清单，再发合成请求。
            for _ in 0..4 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0_u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let (content_type, body) = if request.contains("/v1/templates") {
                    (
                        "application/json",
                        r#"{"templates":[{"id":"xiaoxingxing","name":"小星星","mood":"童谣","syllables":14}]}"#
                            .as_bytes()
                            .to_vec(),
                    )
                } else {
                    ("audio/wav", payload.clone())
                };
                let code = if status == 200 { 200 } else { status };
                let head = format!(
                    "HTTP/1.1 {code} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// 歌声服务与语音服务跑在同一个 runtime 里，两个 URL 一起交给被测代码。
    fn block_on_with_stubs<F, Fut>(
        sing_payload: Vec<u8>,
        sing_status: u16,
        pcm: Vec<u8>,
        body: F,
    ) -> Fut::Output
    where
        F: FnOnce(String, String) -> Fut,
        Fut: std::future::Future,
    {
        kovi::tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(async move {
                let sing = spawn_stub_sing(sing_payload, sing_status).await;
                let tts = spawn_stub_tts(pcm).await;
                body(sing, tts).await
            })
    }

    fn block_on_with_stub_tts<F, Fut>(pcm: Vec<u8>, body: F) -> Fut::Output
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future,
    {
        kovi::tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(async move { body(spawn_stub_tts(pcm).await).await })
    }

    #[test]
    fn voice_delivery_replaces_text_and_the_reply_segment() {
        let voice = Message::from(vec![Segment::new(
            "record",
            json!({"file": "file:///app/qq-call/voice/voice-1.wav"}),
        )]);
        // 合成成功：整条消息只有 record 段，引用被丢掉（语音承载不了引用）。
        let spoken = outbound_message("我在的呀。", Some(42), Some(voice.clone()), None);
        assert_eq!(spoken, voice);
        assert_eq!(spoken.to_human_string().matches("[record]").count(), 1);

        // 没有语音可用：文字与引用都保持原样。
        let quoted = outbound_message("我在的呀。", Some(42), None, None);
        assert!(quoted.to_human_string().contains("[reply]"));
        assert!(quoted.to_human_string().contains("我在的呀。"));
        let plain = outbound_message("我在的呀。", None, None, None);
        assert_eq!(plain.to_human_string(), "我在的呀。");
    }

    /// 表情包是**贴**在消息里的：文字照发，图跟在文字后面；只发一张表情时
    /// 引用仍然挂在前面。它与语音互斥（record 段装不下图）。
    #[test]
    fn sticker_rides_along_with_the_text_in_one_message() {
        let sticker = Segment::new("image", json!({"file": "base64://AAAA"}));

        let with_text = outbound_message("在的呀。", None, None, Some(sticker.clone()));
        assert_eq!(with_text.to_human_string(), "在的呀。[image]");
        assert_eq!(
            with_text
                .get_from_index(0)
                .map(|segment| segment.type_.as_str()),
            Some("text")
        );

        let sticker_only = outbound_message("", Some(42), None, Some(sticker.clone()));
        assert_eq!(sticker_only.to_human_string(), "[reply][image]");

        // 语音那一轮不带表情：record 段无法承载图片。
        let voice = Message::from(vec![Segment::new(
            "record",
            json!({"file": "file:///app/qq-call/voice/voice-1.wav"}),
        )]);
        let spoken = outbound_message("在的呀。", None, Some(voice.clone()), None);
        assert_eq!(spoken, voice);
    }

    #[test]
    fn voice_is_only_attempted_for_content_that_asked_for_it() {
        let dir = staging_dir("hint");
        let config = voice_config(
            &dir.display().to_string(),
            "http://127.0.0.1:1/v1/tts",
            true,
        );
        // 没标记语音的回复连合成请求都不会发（地址是死端口，发了就会超时）。
        let typed = MessageContent::text("普通文字。");
        assert!(block_on(voice_message_for(&typed, &config)).is_none());
    }

    /// 唱歌配置：模板服务指向假服务，暂存目录用测试独占目录。
    fn sing_config_for(base_url: &str) -> crate::config::QqSingConfig {
        serde_json::from_value(serde_json::json!({
            "enabled": true,
            "base_url": base_url,
            "timeout_secs": 5,
            "default_template": "zichang-qingkuai",
        }))
        .expect("qq_sing test config")
    }

    #[test]
    fn singing_goes_through_the_sing_channel() {
        let dir = staging_dir("sing");
        let staged = dir.display().to_string();
        let wav = b"RIFF....WAVEfmt ".to_vec();

        let message =
            block_on_with_stubs(wav.clone(), 200, vec![9_u8; 640], |sing_url, tts_url| {
                let staged = staged.clone();
                async move {
                    let voice = voice_config(&staged, &tts_url, true);
                    let sing = sing_config_for(&sing_url);
                    speech_message_for(
                        &MessageContent::sing("一闪一闪亮晶晶", "xiaoxingxing"),
                        &voice,
                        &sing,
                    )
                    .await
                    .expect("stub sing service should produce a voice message")
                }
            });
        let segments = message.iter().collect::<Vec<_>>();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].type_, "record");
        let file = segments[0].data["file"].as_str().expect("record file");
        assert!(
            file.starts_with("file:///app/qq-call/voice/sing-") && file.ends_with(".wav"),
            "唱歌的音频也该走同一条 NapCat 路径: {file}"
        );
        let written = std::fs::read_dir(&dir)
            .expect("staging dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(
            written.iter().any(|name| name.starts_with("sing-")),
            "expect a sing-*.wav in {written:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_sing_service_falls_back_to_speech() {
        let dir = staging_dir("sing-fallback");
        let staged = dir.display().to_string();
        let pcm = vec![7_u8; 3_200];

        let message =
            block_on_with_stubs(b"sing exploded".to_vec(), 500, pcm, |sing_url, tts_url| {
                let staged = staged.clone();
                async move {
                    let voice = voice_config(&staged, &tts_url, true);
                    let sing = sing_config_for(&sing_url);
                    speech_message_for(
                        &MessageContent::sing("一闪一闪亮晶晶", "xiaoxingxing"),
                        &voice,
                        &sing,
                    )
                    .await
                    .expect("唱不出来时要退成念歌词，而不是丢消息")
                }
            });
        let segments = message.iter().collect::<Vec<_>>();
        assert_eq!(segments[0].type_, "record");
        let file = segments[0].data["file"].as_str().expect("record file");
        assert!(
            file.contains("voice-"),
            "失败时应当退回语音合成的文件: {file}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_or_failing_tts_falls_back_to_text() {
        let dir = staging_dir("fallback");
        let staged = dir.display().to_string();
        let spoken = MessageContent::voice("我在的呀。");

        // 配置关闭：不发请求，调用方按文字发送。
        let disabled = voice_config(&staged, "http://127.0.0.1:1/v1/tts", false);
        assert!(block_on(voice_message_for(&spoken, &disabled)).is_none());

        // 语音服务不可达：同样回退，而且不留下半成品消息。
        let unreachable = voice_config(&staged, "http://127.0.0.1:1/v1/tts", true);
        assert!(block_on(voice_message_for(&spoken, &unreachable)).is_none());
    }

    #[test]
    fn synthesized_voice_becomes_a_record_segment_on_the_napcat_path() {
        let dir = staging_dir("synth");
        let staged = dir.display().to_string();
        let pcm = vec![7_u8; 3_200];

        let message = block_on_with_stub_tts(pcm.clone(), |url| {
            let staged = staged.clone();
            async move {
                let config = voice_config(&staged, &url, true);
                voice_message_for(&MessageContent::voice("我在的呀。"), &config)
                    .await
                    .expect("stub TTS should produce a voice message")
            }
        });
        let human = message.to_human_string();
        assert!(human.contains("[record]"), "unexpected message: {human}");
        let segments = message.iter().collect::<Vec<_>>();
        assert_eq!(segments.len(), 1, "语音消息不该带引用或文字段");
        assert_eq!(segments[0].type_, "record");
        let file = segments[0].data["file"]
            .as_str()
            .expect("record segment carries a file");
        // NapCat 侧路径来自 napcat_staging_dir，而不是机器人本机路径。
        assert!(
            file.starts_with("file:///app/qq-call/voice/voice-") && file.ends_with(".wav"),
            "unexpected record file: {file}"
        );
        assert!(
            !file.contains(&staged),
            "napcat path must not leak the host path"
        );

        // 音频真的落盘了，而且是可读的 WAV（RIFF 头 + 我们给的 PCM）。
        let written = std::fs::read_dir(&dir)
            .expect("staging dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(written.len(), 1, "one turn writes one voice file");
        let bytes = std::fs::read(&written[0]).expect("staged wav");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert!(bytes.ends_with(&pcm), "pcm payload should be preserved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delivery_requires_one_positive_numeric_identity() {
        assert_eq!(single_positive_qq_id(&ids(&["123456"])), Some(123456));
        assert_eq!(single_positive_qq_id(&[]), None);
        assert_eq!(single_positive_qq_id(&ids(&["0"])), None);
        assert_eq!(single_positive_qq_id(&ids(&["-1"])), None);
        assert_eq!(single_positive_qq_id(&ids(&["not-a-qq"])), None);
        assert_eq!(single_positive_qq_id(&ids(&["123", "456"])), None);
    }

    #[test]
    fn canonical_group_and_direct_routes_are_strictly_parsed() {
        assert_eq!(
            parse_qq_destination("group:123", ConversationKind::Group, None),
            Some(QqDestination::Group(123))
        );
        assert_eq!(
            parse_qq_destination("direct:456:123", ConversationKind::Direct, Some(456)),
            Some(QqDestination::Private(123))
        );
        assert_eq!(
            parse_qq_destination("direct:456:123", ConversationKind::Direct, Some(789)),
            None
        );
    }

    #[test]
    fn malformed_or_cross_kind_routes_fail_closed() {
        for (external_id, kind, self_id) in [
            ("group:0", ConversationKind::Group, None),
            ("group:123:456", ConversationKind::Group, None),
            ("direct:456", ConversationKind::Direct, Some(456)),
            ("direct:456:0", ConversationKind::Direct, Some(456)),
            ("direct:456:123:789", ConversationKind::Direct, Some(456)),
            ("group:123", ConversationKind::Direct, Some(456)),
            ("direct:456:123", ConversationKind::Group, None),
        ] {
            assert_eq!(
                parse_qq_destination(external_id, kind, self_id),
                None,
                "route should be rejected: {external_id}"
            );
        }
    }

    #[test]
    fn precommit_route_revalidation_rejects_deletion_or_retargeting() {
        let conversation_id = ConversationId::new();
        let expected = QqDestination::Group(123);
        assert!(delivery_route_is_unchanged(
            conversation_id,
            expected,
            Some((conversation_id, expected))
        ));
        assert!(!delivery_route_is_unchanged(
            conversation_id,
            expected,
            None,
        ));
        assert!(!delivery_route_is_unchanged(
            conversation_id,
            expected,
            Some((conversation_id, QqDestination::Group(456)))
        ));
        assert!(!delivery_route_is_unchanged(
            conversation_id,
            expected,
            Some((ConversationId::new(), expected))
        ));
    }

    #[test]
    fn precommit_authorization_rejects_a_revoked_group() {
        let group = QqDestination::Group(123);
        assert!(delivery_authorization_allows(group, Some(true)));
        assert!(!delivery_authorization_allows(group, Some(false)));
        assert!(!delivery_authorization_allows(group, None));
        assert!(delivery_authorization_allows(
            QqDestination::Private(456),
            None
        ));
    }

    #[test]
    fn missing_durable_owner_is_terminal_but_ledger_outage_is_retryable() {
        let missing = durable_commit_error(DeliveryCommitError::OwnerMissing {
            owner_kind: "person",
        });
        assert!(!missing.retryable);
        assert!(
            missing
                .category
                .starts_with("durable_delivery_owner_missing:")
        );

        let unavailable = durable_commit_error(DeliveryCommitError::Ledger(anyhow::anyhow!(
            "database unavailable"
        )));
        assert!(unavailable.retryable);
        assert!(
            unavailable
                .category
                .starts_with("durable_delivery_ledger_unavailable:")
        );
    }

    #[test]
    fn durable_replay_barriers_are_terminal_without_claiming_delivery() {
        let conversation_id = ConversationId::new();
        for status in [
            DeliveryStatus::Prepared,
            DeliveryStatus::Committed,
            DeliveryStatus::Unknown,
        ] {
            assert!(matches!(
                recorded_delivery_outcome(status, None, conversation_id),
                Ok(ActionPortOutcome::DeliveryIndeterminate {
                    conversation_id: Some(actual),
                    ..
                }) if actual == conversation_id
            ));
        }
        assert!(matches!(
            recorded_delivery_outcome(DeliveryStatus::Sent, Some(42), conversation_id),
            Ok(ActionPortOutcome::Delivered {
                external_reference: Some(reference),
                conversation_id: Some(actual),
                ..
            }) if reference == "qq-message:42" && actual == conversation_id
        ));
        assert!(recorded_delivery_outcome(DeliveryStatus::Failed, None, conversation_id).is_err());
    }

    #[test]
    fn compatibility_reach_out_preserves_indeterminate_delivery() {
        assert_eq!(
            compatibility_reach_out_outcome(Ok(42)),
            ReachOutDeliveryOutcome::Delivered
        );
        assert_eq!(
            compatibility_reach_out_outcome(Err(TrackedSendError::TransportIndeterminate(
                "response cancelled".to_owned()
            ))),
            ReachOutDeliveryOutcome::Indeterminate
        );
        assert_eq!(
            compatibility_reach_out_outcome(Err(TrackedSendError::DuplicateIdempotency)),
            ReachOutDeliveryOutcome::Indeterminate
        );
        assert_eq!(
            compatibility_reach_out_outcome(Err(TrackedSendError::Transport(
                "request rejected".to_owned()
            ))),
            ReachOutDeliveryOutcome::Failed
        );
    }
}
