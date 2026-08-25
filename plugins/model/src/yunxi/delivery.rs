//! QQ delivery adapter for platform-neutral actions.
//!
//! Core only exposes opaque people and conversation IDs. This module is the
//! only place where those IDs are translated to concrete QQ destinations or
//! Kovi API calls. Legacy proactive-chat delivery remains below as a small
//! compatibility helper; new actions use [`QqActionAdapter`].

use super::identity_store::PostgresIdentityStore;
use super::qq;
use crate::model::{
    MessageDestination, MessageTransport, OutgoingCommitRejection, OutgoingSource, OutgoingToken,
    ReplyScope, ToolExecutionContext, commit_outgoing_guard_with_context,
    contextual_outgoing_fingerprint, find_prepared_outgoing, finish, interrupt, mark_active,
    mark_outgoing_failed, outgoing_fingerprint, prepare_outgoing, record_standalone_bot_message,
    send_tracked_private_message, tool_registry,
};
use kovi::bot::message::Segment;
use kovi::serde_json::json;
use kovi::tokio::sync::Mutex;
use kovi::{Message, RuntimeBot};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;
use yunxi_core::{
    ActionPort, ActionPortError, ActionPortFuture, ActionPortOutcome, ActionScope, ConversationId,
    ConversationKind, ConversationMemberStore, DeliveryResolutionError, DeliveryResolver,
    DeliveryResolverFuture, DeliveryRoute, GoalState, GoalStore, IdentityStore, MessageContent,
    MessageId, OpenLoopStore, ProposedAction, ReachOutIntent, ToolAction,
    MAX_TOOL_ERROR_DETAIL_BYTES, MAX_TOOL_ERROR_DETAIL_CHARS, MAX_TOOL_RESULT_BYTES,
    MAX_TOOL_RESULT_CHARS,
};

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
    open_loop_store: Arc<dyn OpenLoopStore>,
    goal_store: Arc<dyn GoalStore>,
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
    ) -> Arc<Self> {
        Arc::new(Self {
            bot,
            identity_store,
            open_loop_store,
            goal_store,
            self_id: Arc::new(Mutex::new(None)),
        })
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
        let external_id = self
            .identity_store
            .qq_external_identity_for_delivery(person_id)
            .await
            .map_err(|error| {
                DeliveryResolutionError::failed(QqAdapterFailure::new(
                    "person identity lookup",
                    error.to_string(),
                ))
            })?;
        let Some(user_id) = external_id
            .as_deref()
            .and_then(|value| single_positive_qq_id(&[value.to_owned()]))
        else {
            return Err(DeliveryResolutionError::Unavailable { person_id });
        };

        let self_id = self
            .current_self_id()
            .await
            .map_err(DeliveryResolutionError::failed)?;
        let external = qq::direct(self_id, user_id).map_err(|error| {
            DeliveryResolutionError::failed(QqAdapterFailure::new(
                "direct conversation reference",
                error.to_string(),
            ))
        })?;
        let conversation_id = self
            .identity_store
            .resolve_external_conversation(&external)
            .await
            .map_err(|error| {
                DeliveryResolutionError::failed(QqAdapterFailure::new(
                    "direct conversation lookup",
                    error.to_string(),
                ))
            })?;
        Ok((
            DeliveryRoute::new(conversation_id, ConversationKind::Direct),
            QqDestination::Private(user_id),
        ))
    }

    async fn resolve_conversation_destination(
        &self,
        conversation_id: ConversationId,
    ) -> Result<QqDestination, ActionPortError> {
        let destination = self
            .resolve_conversation_destination_without_authorization(conversation_id)
            .await?;
        if let QqDestination::Group(group_id) = destination {
            let authorized = crate::group_access::is_authorized_group(group_id)
                .await
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

    async fn send_to_destination(
        &self,
        revalidation_target: DeliveryRevalidationTarget,
        expected_destination: QqDestination,
        content: &MessageContent,
        reply_to: Option<MessageId>,
        expected_conversation_id: ConversationId,
        idempotency_key: &str,
        outgoing: OutgoingToken,
    ) -> Result<ActionPortOutcome, ActionPortError> {
        // Resolve the optional quote before the final destination check. Quote
        // degradation is stylistic; route and authorization are security
        // boundaries and therefore must be the last awaited lookups before
        // the serialized commit.
        let reply_to = if let Some(reply_to) = reply_to {
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
        let route_guard = crate::yunxi::pin_delivery_routes().await;
        let authorization = match expected_destination {
            QqDestination::Group(group_id) => {
                match crate::group_access::authorize_group_send(group_id).await {
                    Ok(authorization) => Some(authorization),
                    Err(error) => {
                        mark_outgoing_failed(outgoing).await;
                        return Err(ActionPortError::new(
                            format!("group_not_authorized_before_commit:{error}"),
                            false,
                        ));
                    }
                }
            }
            QqDestination::Private(_) => None,
        };
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
                mark_outgoing_failed(outgoing).await;
                return Ok(ActionPortOutcome::Deferred {
                    reason: "delivery_route_changed_before_commit".to_string(),
                });
            }
            Err(error) => {
                mark_outgoing_failed(outgoing).await;
                return Err(error);
            }
        };
        let text = content.as_text();
        let message = if let Some(reply_to) = reply_to {
            Message::from(vec![
                Segment::new("reply", json!({"id": reply_to})),
                Segment::new("text", json!({"text": text})),
            ])
        } else {
            text.to_owned().into()
        };
        let fingerprint_content = serde_json::to_string(content)
            .unwrap_or_else(|_| content.as_text().to_owned());
        let fingerprint = contextual_outgoing_fingerprint(
            destination.reply_scope(),
            &fingerprint_content,
            reply_to,
            &[],
            Some(idempotency_key),
        );
        let committed = match commit_outgoing_guard_with_context(
            outgoing,
            fingerprint,
            Some(idempotency_key),
        )
        .await
        {
            Ok(committed) => committed,
            Err(OutgoingCommitRejection::Stale) => {
                return Ok(ActionPortOutcome::Deferred {
                    reason: "outgoing_superseded_before_commit".to_string(),
                });
            }
            Err(OutgoingCommitRejection::DuplicateIdempotency) => {
                return Ok(ActionPortOutcome::Deferred {
                    reason: "outgoing_duplicate_idempotency_key".to_string(),
                });
            }
        };
        drop(authorization);
        drop(route_guard);
        let send_result = MessageTransport::new(&self.bot)
            .send(destination.message_destination(), message)
            .await;
        let message_id = match send_result {
            Ok(message_id) => {
                committed.mark_sent().await;
                message_id
            }
            Err(error) => {
                committed.mark_failed().await;
                return Err(ActionPortError::new(
                    format!("qq_send_failed:{}", format_api_return(&error)),
                    true,
                ));
            }
        };
        record_standalone_bot_message(destination.reply_scope(), message_id, text).await;
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
        allow_proactive_fallback: bool,
    ) -> Option<OutgoingToken> {
        let fingerprint = outgoing_fingerprint(content.as_text());
        let prepared = find_prepared_outgoing(scope, fingerprint).await;
        let (outgoing, source) = if let Some(prepared) = prepared {
            prepared
        } else if allow_proactive_fallback {
            let ticket = interrupt(scope).await;
            if !mark_active(ticket).await {
                return None;
            }
            let outgoing = prepare_outgoing(ticket, fingerprint, OutgoingSource::Proactive).await;
            finish(ticket).await;
            (outgoing?, OutgoingSource::Proactive)
        } else {
            return None;
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
        let Some(registry) = tool_registry() else {
            return Ok(ActionPortOutcome::Deferred {
                reason: "tool_registry_unavailable".to_string(),
            });
        };
        let actor = action
            .actor()
            .ok_or_else(|| ActionPortError::new("tool_actor_required", false))?;
        let actor_user_id = self
            .identity_store
            .qq_external_identity_for_delivery(actor)
            .await
            .map_err(|error| {
                ActionPortError::new(format!("tool_actor_lookup_failed:{error}"), true)
            })?
            .and_then(|value| parse_positive_i64(&value))
            .ok_or_else(|| ActionPortError::new("tool_actor_route_unavailable", false))?;

        let destination = match action.scope {
            ActionScope::Conversation(conversation_id) => {
                if self
                    .identity_store
                    .get(conversation_id, actor)
                    .await
                    .map_err(|error| {
                        ActionPortError::new(format!("tool_scope_membership_failed:{error}"), true)
                    })?
                    .is_none()
                {
                    return Err(ActionPortError::new(
                        "tool_scope_membership_required",
                        false,
                    ));
                }
                self.resolve_conversation_destination(conversation_id).await?
            }
            ActionScope::Person(person_id) => {
                if person_id != actor {
                    return Err(ActionPortError::new(
                        "tool_person_scope_actor_mismatch",
                        false,
                    ));
                }
                let (_route, destination) = self
                    .resolve_person_destination(person_id)
                    .await
                    .map_err(|error| ActionPortError::new(error.to_string(), true))?;
                destination
            }
            ActionScope::Global => {
                return Ok(ActionPortOutcome::Deferred {
                    reason: "global_tool_scope_requires_host_context".to_string(),
                });
            }
        };

        let arguments = serde_json::from_str::<serde_json::Value>(&action.input)
            .map_err(|error| ActionPortError::new(format!("tool_input_invalid:{error}"), false))?;
        let Some(arguments) = arguments.as_object().cloned() else {
            return Err(ActionPortError::new(
                "tool_input_must_be_json_object",
                false,
            ));
        };
        let configured_owner = crate::config::get().identity().owner_person_id();
        let is_main_admin = configured_owner.is_some_and(|owner| owner == actor.into_uuid())
            || (configured_owner.is_none()
                && self
                    .bot
                    .get_main_admin()
                    .ok()
                    .is_some_and(|main_admin| main_admin == actor_user_id));
        // The Core action has no raw group-admin proof. Restrict admin tools
        // to the host's main administrator until a platform-neutral
        // capability token is available.
        let is_admin = is_main_admin;
        let group_paused = match destination {
            QqDestination::Group(group_id) => crate::model::utils::is_group_paused(group_id).await,
            QqDestination::Private(_) => false,
        };
        let reply_scope = destination.reply_scope();
        let ticket = interrupt(reply_scope).await;
        if !mark_active(ticket).await {
            return Ok(ActionPortOutcome::Deferred {
                reason: "tool_turn_superseded_before_start".to_string(),
            });
        }
        let context = ToolExecutionContext {
            subject_id: actor_user_id,
            actor_user_id,
            is_admin,
            is_main_admin,
            context: "yunxi_core_tool",
            destination: destination.message_destination(),
            source_message_id: None,
            scheduled: false,
            group_paused,
            runtime_bot: Some(Arc::clone(&self.bot)),
            sticker_teaching: None,
            requires_reminder_create: false,
            requires_agent_run_create: false,
            requires_group_message_send: false,
            requires_group_followup: false,
            requires_external_tool: false,
        };
        let result = registry
            .execute(&action.tool_name, arguments, context, ticket)
            .await;
        finish(ticket).await;
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

impl ActionPort for QqActionAdapter {
    fn execute<'a>(&'a self, action: &'a ProposedAction) -> ActionPortFuture<'a> {
        Box::pin(async move {
            match action {
                ProposedAction::SendMessage(send) => {
                    let destination = self
                        .resolve_conversation_destination(send.conversation_id)
                        .await?;
                    let reply_to = send.reply_to;
                    let Some(outgoing) = self
                        .prepared_outgoing(destination.reply_scope(), &send.content, false)
                        .await
                    else {
                        return Ok(ActionPortOutcome::Deferred {
                            reason: "outgoing_not_prepared".to_string(),
                        });
                    };
                    self.send_to_destination(
                        DeliveryRevalidationTarget::Conversation(send.conversation_id),
                        destination,
                        &send.content,
                        reply_to,
                        send.conversation_id,
                        send.idempotency_key(),
                        outgoing,
                    )
                    .await
                }
                ProposedAction::ReachOut(reach_out) => {
                    let (route, destination) = self
                        .resolve_person_destination(reach_out.person_id)
                        .await
                        .map_err(|error| ActionPortError::new(error.to_string(), true))?;
                    let Some(outgoing) = self
                        .prepared_outgoing(destination.reply_scope(), &reach_out.message, true)
                        .await
                    else {
                        return Ok(ActionPortOutcome::Deferred {
                            reason: "outgoing_not_prepared".to_string(),
                        });
                    };
                    self.send_to_destination(
                        DeliveryRevalidationTarget::Person(reach_out.person_id),
                        destination,
                        &reach_out.message,
                        None,
                        route.conversation_id,
                        reach_out.idempotency_key(),
                        outgoing,
                    )
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
}

fn store_action_error(error: impl std::fmt::Display) -> ActionPortError {
    ActionPortError::new(format!("core_store_failed:{error}"), true)
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

pub(crate) async fn send_reach_out(
    bot: &Arc<RuntimeBot>,
    identity_store: &PostgresIdentityStore,
    intent: &ReachOutIntent,
    expected_user_id: i64,
) -> bool {
    let Ok(Some(external_id)) = identity_store
        .qq_external_identity_for_delivery(intent.person_id())
        .await
    else {
        return false;
    };
    let Some(user_id) = single_positive_qq_id(&[external_id]) else {
        return false;
    };
    if user_id != expected_user_id {
        return false;
    }
    let content: &MessageContent = intent.message();
    send_tracked_private_message(bot, user_id, content.as_text().to_string()).await
}

#[cfg(test)]
mod tests {
    use super::{
        QqDestination, delivery_authorization_allows, delivery_route_is_unchanged,
        parse_qq_destination, single_positive_qq_id,
    };
    use yunxi_core::{ConversationId, ConversationKind};

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
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
}
