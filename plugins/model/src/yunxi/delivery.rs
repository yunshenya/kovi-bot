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
        let precommit = match begin_outgoing_commit(outgoing).await {
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
        let text = content.as_text();
        let message = if let Some(reply_to) = external_reply_to {
            Message::from(vec![
                Segment::new("reply", json!({"id": reply_to})),
                Segment::new("text", json!({"text": text})),
            ])
        } else {
            text.to_owned().into()
        };
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
        let send_result = MessageTransport::new(&self.bot)
            .send(destination.message_destination(), message)
            .await;
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
            let read_only_only = claim.read_only_only;
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
            };
            if read_only_only
                && !registry.available_read_only_for_context(&action.tool_name, &context)
            {
                drop(mind_delivery_permit);
                drop(group_authorization);
                drop(route_guard);
                return Ok(ActionPortOutcome::Deferred {
                    reason: "tool_follow_up_requires_read_only_tool".to_string(),
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
                    read_only_only,
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
        EnvironmentCapabilities::new([
            ActionDescriptor::new(ActionCapability::SendMessage),
            ActionDescriptor::new(ActionCapability::ReachOut),
            ActionDescriptor::new(ActionCapability::UseTool),
            ActionDescriptor::new(ActionCapability::CreateOpenLoop),
            ActionDescriptor::new(ActionCapability::ResolveOpenLoop),
            ActionDescriptor::new(ActionCapability::StartGoal),
            ActionDescriptor::new(ActionCapability::CancelGoal),
        ])
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
                    self.send_to_destination(QqSendContext {
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
                    self.send_to_destination(QqSendContext {
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
    format!(
        "legacy-reach-out:{}:{}:{:x}",
        intent.person_id(),
        chrono::Utc::now().format("%Y%m%d"),
        hasher.finalize()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        QqDestination, ReachOutDeliveryOutcome, compatibility_reach_out_outcome,
        delivery_authorization_allows, delivery_route_is_unchanged, durable_commit_error,
        parse_qq_destination, recorded_delivery_outcome, single_positive_qq_id,
    };
    use crate::model::TrackedSendError;
    use crate::yunxi::delivery_ledger::{DeliveryCommitError, DeliveryStatus};
    use yunxi_core::{ActionPortOutcome, ConversationId, ConversationKind};

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
