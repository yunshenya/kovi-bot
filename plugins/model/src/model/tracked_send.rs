//! Unified lifecycle for visible host-side sends that are not model reply bubbles.

use super::interrupt::{
    CommittedOutgoing, OutgoingCommitRejection, OutgoingSource, OutgoingToken, ReplyScope,
    ReplyTicket, active_ticket_locked, begin_outgoing_commit, claim_active_locked,
    contextual_outgoing_fingerprint, finish, interrupt_locked, mark_outgoing_failed,
    prepare_outgoing_with_semantic_preview,
    prepare_proactive_outgoing_if_idle_with_semantic_preview, scope_mutex,
};
use super::message_actions::MessageDestination;
use super::message_transport::MessageTransport;
use super::recall::record_standalone_bot_message;
use crate::group_access;
use kovi::{Message, RuntimeBot};
use std::future::Future;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackedSendError {
    InvalidTarget,
    ConversationBusy,
    RevalidationRejected,
    Unauthorized,
    Stale,
    DuplicateIdempotency,
    Transport(String),
    TransportIndeterminate(String),
}

impl std::fmt::Display for TrackedSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTarget => formatter.write_str("invalid delivery target"),
            Self::ConversationBusy => {
                formatter.write_str("conversation already has an active reply")
            }
            Self::RevalidationRejected => {
                formatter.write_str("pre-commit revalidation rejected the send")
            }
            Self::Unauthorized => formatter.write_str("delivery target is no longer authorized"),
            Self::Stale => formatter.write_str("prepared send became stale before commit"),
            Self::DuplicateIdempotency => formatter.write_str("duplicate idempotency key"),
            Self::Transport(detail) => write!(formatter, "transport failed: {detail}"),
            Self::TransportIndeterminate(detail) => {
                write!(formatter, "transport outcome is indeterminate: {detail}")
            }
        }
    }
}

impl std::error::Error for TrackedSendError {}

struct PreparedTrackedSend {
    destination: MessageDestination,
    message: Message,
    audit_content: String,
    fingerprint: u64,
    idempotency_key: Option<String>,
    ticket: Option<ReplyTicket>,
    outgoing: Option<OutgoingToken>,
}

impl PreparedTrackedSend {
    async fn cancel(mut self) {
        self.cleanup().await;
    }

    async fn commit(
        mut self,
        precommit: super::interrupt::PreparedOutgoingCommit,
    ) -> Result<CommittedTrackedSend, TrackedSendError> {
        if self.outgoing.is_none() {
            return Err(TrackedSendError::Stale);
        }
        let committed = precommit
            .commit(self.fingerprint, self.idempotency_key.as_deref())
            .await
            .map_err(|rejection| match rejection {
                OutgoingCommitRejection::Stale => TrackedSendError::Stale,
                OutgoingCommitRejection::DuplicateIdempotency => {
                    TrackedSendError::DuplicateIdempotency
                }
            })?;
        self.outgoing = None;
        Ok(CommittedTrackedSend {
            destination: self.destination,
            message: std::mem::take(&mut self.message),
            audit_content: std::mem::take(&mut self.audit_content),
            ticket: self.ticket.take(),
            committed: Some(committed),
        })
    }

    async fn cleanup(&mut self) {
        if let Some(outgoing) = self.outgoing.take() {
            mark_outgoing_failed(outgoing).await;
        }
        if let Some(ticket) = self.ticket.take() {
            finish(ticket).await;
        }
    }
}

impl Drop for PreparedTrackedSend {
    fn drop(&mut self) {
        let outgoing = self.outgoing.take();
        let ticket = self.ticket.take();
        if outgoing.is_none() && ticket.is_none() {
            return;
        }
        if let Ok(runtime) = kovi::tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(outgoing) = outgoing {
                    mark_outgoing_failed(outgoing).await;
                }
                if let Some(ticket) = ticket {
                    finish(ticket).await;
                }
            });
        }
    }
}

struct CommittedTrackedSend {
    destination: MessageDestination,
    message: Message,
    audit_content: String,
    ticket: Option<ReplyTicket>,
    committed: Option<CommittedOutgoing>,
}

impl CommittedTrackedSend {
    async fn mark_sent(mut self, message_id: i32, record_delivery: bool) -> i32 {
        if let Some(committed) = self.committed.take() {
            committed.mark_sent().await;
        }
        if record_delivery {
            let scope = destination_scope(self.destination);
            if let Some(ticket) = self.ticket {
                record_standalone_bot_message(scope, ticket, message_id, &self.audit_content).await;
            }
        }
        self.finish().await;
        message_id
    }

    async fn mark_failed(mut self) {
        if let Some(committed) = self.committed.take() {
            committed.mark_failed().await;
        }
        self.finish().await;
    }

    async fn finish(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            finish(ticket).await;
        }
    }
}

impl Drop for CommittedTrackedSend {
    fn drop(&mut self) {
        let ticket = self.ticket.take();
        if let Some(ticket) = ticket
            && let Ok(runtime) = kovi::tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                finish(ticket).await;
            });
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TrackedSendOptions {
    record_delivery: bool,
    precommit_before_revalidation: bool,
}

/// Send a visible host-side message through the same coordinator used by Core
/// and legacy model replies. The caller's revalidation runs after preparation
/// and immediately before the adapter authorization check and commit.
pub(crate) async fn send_tracked_message_with_revalidation<Validate, ValidateFuture>(
    bot: &RuntimeBot,
    destination: MessageDestination,
    message: Message,
    source: OutgoingSource,
    idempotency_key: Option<&str>,
    revalidate: Validate,
) -> Result<i32, TrackedSendError>
where
    Validate: FnOnce() -> ValidateFuture,
    ValidateFuture: Future<Output = bool>,
{
    send_tracked_message_inner(
        bot,
        destination,
        message,
        source,
        idempotency_key,
        move || async move { revalidate().await.then_some(()) },
        TrackedSendOptions {
            record_delivery: true,
            precommit_before_revalidation: false,
        },
    )
    .await
}

/// Variant whose validator returns a guard that stays alive through commit.
/// Route and actor checks use it to close their final lookup-to-commit race
/// without holding security locks over the platform request.
pub(crate) async fn send_tracked_message_with_revalidation_guard<
    Validate,
    ValidateFuture,
    RevalidationGuard,
>(
    bot: &RuntimeBot,
    destination: MessageDestination,
    message: Message,
    source: OutgoingSource,
    idempotency_key: Option<&str>,
    revalidate: Validate,
) -> Result<i32, TrackedSendError>
where
    Validate: FnOnce() -> ValidateFuture,
    ValidateFuture: Future<Output = Option<RevalidationGuard>>,
{
    send_tracked_message_inner(
        bot,
        destination,
        message,
        source,
        idempotency_key,
        revalidate,
        TrackedSendOptions {
            record_delivery: true,
            precommit_before_revalidation: true,
        },
    )
    .await
}

async fn send_tracked_message_inner<Validate, ValidateFuture, RevalidationGuard>(
    bot: &RuntimeBot,
    destination: MessageDestination,
    message: Message,
    source: OutgoingSource,
    idempotency_key: Option<&str>,
    revalidate: Validate,
    options: TrackedSendOptions,
) -> Result<i32, TrackedSendError>
where
    Validate: FnOnce() -> ValidateFuture,
    ValidateFuture: Future<Output = Option<RevalidationGuard>>,
{
    let prepared = prepare_tracked_message(
        destination,
        message,
        source,
        idempotency_key.map(ToOwned::to_owned),
    )
    .await?;
    let outgoing = prepared.outgoing.ok_or(TrackedSendError::Stale)?;
    let mut precommit = if options.precommit_before_revalidation {
        Some(
            begin_outgoing_commit(outgoing)
                .await
                .map_err(map_commit_rejection)?,
        )
    } else {
        None
    };

    let Some(revalidation_guard) = revalidate().await else {
        prepared.cancel().await;
        return Err(TrackedSendError::RevalidationRejected);
    };
    if precommit.is_none() {
        precommit = Some(
            begin_outgoing_commit(outgoing)
                .await
                .map_err(map_commit_rejection)?,
        );
    }
    let authorization = match destination {
        MessageDestination::Group(group_id) => {
            match group_access::authorize_group_send(group_id).await {
                Ok(authorization) => Some(authorization),
                Err(_) => {
                    drop(revalidation_guard);
                    prepared.cancel().await;
                    return Err(TrackedSendError::Unauthorized);
                }
            }
        }
        MessageDestination::Private(_) => None,
    };

    let committed = prepared
        .commit(precommit.expect("precommit must be acquired before authorization"))
        .await?;
    drop(authorization);
    drop(revalidation_guard);
    let transport = MessageTransport::new(bot);
    let send_result = if options.record_delivery {
        transport
            .send(committed.destination, committed.message.clone())
            .await
    } else {
        transport
            .send_redacted(committed.destination, committed.message.clone())
            .await
    };
    match send_result {
        Ok(message_id) => Ok(committed
            .mark_sent(message_id, options.record_delivery)
            .await),
        Err(error) => {
            let detail = error.to_string();
            if error.is_indeterminate() {
                drop(committed);
                Err(TrackedSendError::TransportIndeterminate(detail))
            } else {
                committed.mark_failed().await;
                Err(TrackedSendError::Transport(detail))
            }
        }
    }
}

/// Tracked send for a data-erasure receipt. It participates in conversation
/// concurrency but deliberately does not recreate persisted message history
/// immediately after the user's data has been deleted.
pub(crate) async fn send_tracked_unrecorded_plain_text(
    bot: &RuntimeBot,
    destination: MessageDestination,
    content: String,
) -> Result<i32, TrackedSendError> {
    send_tracked_message_inner(
        bot,
        destination,
        Message::from(content),
        OutgoingSource::Reply,
        None,
        || async { Some(()) },
        TrackedSendOptions {
            record_delivery: false,
            precommit_before_revalidation: true,
        },
    )
    .await
}

fn map_commit_rejection(rejection: OutgoingCommitRejection) -> TrackedSendError {
    match rejection {
        OutgoingCommitRejection::Stale => TrackedSendError::Stale,
        OutgoingCommitRejection::DuplicateIdempotency => TrackedSendError::DuplicateIdempotency,
    }
}

pub(crate) async fn send_tracked_plain_text(
    bot: &RuntimeBot,
    destination: MessageDestination,
    content: String,
) -> Result<i32, TrackedSendError> {
    send_tracked_message_with_revalidation(
        bot,
        destination,
        Message::from(content),
        OutgoingSource::Reply,
        None,
        || async { true },
    )
    .await
}

async fn prepare_tracked_message(
    destination: MessageDestination,
    message: Message,
    source: OutgoingSource,
    idempotency_key: Option<String>,
) -> Result<PreparedTrackedSend, TrackedSendError> {
    if destination_id(destination) <= 0 {
        return Err(TrackedSendError::InvalidTarget);
    }
    let scope = destination_scope(destination);
    let audit_content = message.to_human_string();
    let fingerprint =
        tracked_message_fingerprint(destination, &message, idempotency_key.as_deref());
    let (ticket, outgoing) = if source == OutgoingSource::Proactive {
        let outgoing = prepare_proactive_outgoing_if_idle_with_semantic_preview(
            scope,
            fingerprint,
            Some(&audit_content),
        )
        .await
        .ok_or(TrackedSendError::ConversationBusy)?;
        (outgoing.ticket(), outgoing)
    } else {
        let ticket = {
            let lock = scope_mutex(scope);
            let _scope_guard = lock.lock().await;
            if active_ticket_locked(scope).await.is_some() {
                return Err(TrackedSendError::ConversationBusy);
            }
            let ticket = interrupt_locked(scope).await;
            if !claim_active_locked(ticket).await {
                return Err(TrackedSendError::Stale);
            }
            ticket
        };
        let Some(outgoing) = prepare_outgoing_with_semantic_preview(
            ticket,
            fingerprint,
            source,
            Some(&audit_content),
        )
        .await
        else {
            finish(ticket).await;
            return Err(TrackedSendError::Stale);
        };
        (ticket, outgoing)
    };
    Ok(PreparedTrackedSend {
        destination,
        message,
        audit_content,
        fingerprint,
        idempotency_key,
        ticket: Some(ticket),
        outgoing: Some(outgoing),
    })
}

fn tracked_message_fingerprint(
    destination: MessageDestination,
    message: &Message,
    idempotency_key: Option<&str>,
) -> u64 {
    let serialized =
        kovi::serde_json::to_string(message).unwrap_or_else(|_| message.to_human_string());
    contextual_outgoing_fingerprint(
        destination_scope(destination),
        &serialized,
        None,
        &[],
        idempotency_key,
    )
}

const fn destination_scope(destination: MessageDestination) -> ReplyScope {
    match destination {
        MessageDestination::Group(group_id) => ReplyScope::Group(group_id),
        MessageDestination::Private(user_id) => ReplyScope::Private(user_id),
    }
}

const fn destination_id(destination: MessageDestination) -> i64 {
    match destination {
        MessageDestination::Group(group_id) => group_id,
        MessageDestination::Private(user_id) => user_id,
    }
}

#[cfg(test)]
mod tests {
    use super::{TrackedSendError, prepare_tracked_message, tracked_message_fingerprint};
    use crate::model::ConversationCoordinator;
    use crate::model::interrupt::{
        OutgoingSource, ReplyScope, begin_outgoing_commit, finish, interrupt, is_active,
        is_current, mark_active,
    };
    use crate::model::message_actions::MessageDestination;
    use kovi::Message;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;

    #[test]
    fn structured_reply_and_mention_segments_change_the_envelope_fingerprint() {
        let destination = MessageDestination::Group(9_110_000);
        let first = Message::from(vec![
            Segment::new("reply", json!({"id": 41})),
            Segment::new("at", json!({"qq": 51})),
            Segment::new("text", json!({"text": "相同正文"})),
        ]);
        let different_reply = Message::from(vec![
            Segment::new("reply", json!({"id": 42})),
            Segment::new("at", json!({"qq": 51})),
            Segment::new("text", json!({"text": "相同正文"})),
        ]);
        let different_mention = Message::from(vec![
            Segment::new("reply", json!({"id": 41})),
            Segment::new("at", json!({"qq": 52})),
            Segment::new("text", json!({"text": "相同正文"})),
        ]);

        let first = tracked_message_fingerprint(destination, &first, None);
        assert_ne!(
            first,
            tracked_message_fingerprint(destination, &different_reply, None)
        );
        assert_ne!(
            first,
            tracked_message_fingerprint(destination, &different_mention, None)
        );
    }

    #[test]
    fn host_side_send_never_borrows_an_unrelated_active_ticket() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_110_001);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);

                let result = prepare_tracked_message(
                    MessageDestination::Group(9_110_001),
                    Message::from("确认消息"),
                    OutgoingSource::Reply,
                    None,
                )
                .await;
                assert!(matches!(result, Err(TrackedSendError::ConversationBusy)));

                assert!(is_current(ticket).await);
                assert!(is_active(scope).await);
                finish(ticket).await;
            });
    }

    #[test]
    fn proactive_send_yields_to_an_active_reactive_ticket() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Private(9_110_002);
                let ticket = interrupt(scope).await;
                assert!(mark_active(ticket).await);

                let result = prepare_tracked_message(
                    MessageDestination::Private(9_110_002),
                    Message::from("主动消息"),
                    OutgoingSource::Proactive,
                    None,
                )
                .await;
                assert!(matches!(result, Err(TrackedSendError::ConversationBusy)));
                assert!(is_current(ticket).await);

                finish(ticket).await;
            });
    }

    #[test]
    fn proactive_send_yields_to_an_admitted_but_not_yet_active_inbound() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_110_005);
                let admission = ConversationCoordinator::begin_incoming(scope).await;

                let result = prepare_tracked_message(
                    MessageDestination::Private(9_110_005),
                    Message::from("proactive must wait"),
                    OutgoingSource::Proactive,
                    None,
                )
                .await;

                assert!(matches!(result, Err(TrackedSendError::ConversationBusy)));
                assert!(is_current(admission.ticket).await);
                assert!(ConversationCoordinator::abandon_incoming(admission).await);

                let prepared = prepare_tracked_message(
                    MessageDestination::Private(9_110_005),
                    Message::from("proactive after release"),
                    OutgoingSource::Proactive,
                    None,
                )
                .await
                .expect("released reservation must not block the scope permanently");
                prepared.cancel().await;
            });
    }

    #[test]
    fn inbound_turn_supersedes_a_prepared_proactive_send() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_110_003);
                let prepared = prepare_tracked_message(
                    MessageDestination::Group(9_110_003),
                    Message::from("主动消息"),
                    OutgoingSource::Proactive,
                    None,
                )
                .await
                .expect("空闲会话应允许准备主动消息");

                let inbound = interrupt(scope).await;
                let outgoing = prepared
                    .outgoing
                    .expect("prepared send must retain its token");
                assert!(matches!(
                    begin_outgoing_commit(outgoing).await,
                    Err(crate::model::OutgoingCommitRejection::Stale)
                ));
                assert!(is_current(inbound).await);
            });
    }

    #[test]
    fn cancelling_an_owned_prepared_send_keeps_the_scope_idle() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_110_004);
                let prepared = prepare_tracked_message(
                    MessageDestination::Group(9_110_004),
                    Message::from("主动消息"),
                    OutgoingSource::Proactive,
                    None,
                )
                .await
                .expect("空闲会话应允许准备主动消息");
                assert!(!is_active(scope).await);

                prepared.cancel().await;
                assert!(!is_active(scope).await);
            });
    }
}
