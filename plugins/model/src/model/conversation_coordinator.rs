//! 群聊和私聊共享的会话生命周期与排队策略。

use super::interrupt::{
    OutgoingSource, ReplyScope, ReplyTicket, cancel_if_current_locked,
    cancel_prepared_proactive_locked, claim_follow_up_locked,
    interrupt_locked as supersede_locked, is_active_locked, is_current_locked,
    prepared_outgoing_source_locked, scope_mutex,
};
use super::recall::begin_reply_locked;
use super::semantic::MessageUnderstanding;
use crate::config;
use crate::vision::VisionImage;
use kovi::Message;
use std::collections::VecDeque;

/// 一个不可拆分的待处理 turn；正文、发送者、附件和消息 ID 总是一起入队。
#[derive(Debug, Clone)]
pub(crate) struct PendingTurn {
    pub(crate) user_id: i64,
    pub(crate) sender: String,
    pub(crate) message: String,
    pub(crate) reply_expected: bool,
    pub(crate) vision_images: Vec<VisionImage>,
    pub(crate) message_ids: Vec<i32>,
    pub(crate) understanding: MessageUnderstanding,
    pub(crate) sticker_teaching_message: Option<Message>,
}

/// Executive's semantic decision for an otherwise valid prepared outgoing.
///
/// Hard failures such as a stale ticket, stop intent, invalid route, denied
/// authorization, or duplicate idempotency key are handled by the coordinator
/// before this policy runs. Consequently, cancellation is intentionally not a
/// semantic outcome here.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutgoingExecutiveDecision {
    Keep,
    Rewrite,
    Merge,
    Defer,
}

/// The semantic effect of the newest inbound turn on prepared content.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum IncomingTurnImpact {
    /// No caller-owned semantic classification is available. Fail closed by
    /// superseding the prepared output, matching the pre-policy behavior.
    #[default]
    Unknown,
    /// The turn carries no information that changes the prepared content.
    None,
    /// The turn adds compatible context that should be reflected in one reply.
    ExtendsPendingTopic,
    /// The turn answers the pending question or invalidates its premise.
    InvalidatesPendingContent,
    /// The turn starts an independent topic.
    Unrelated,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutgoingExecutiveContext {
    pub(crate) incoming_impact: IncomingTurnImpact,
    /// Whether the newest turn itself requires a direct response.
    pub(crate) direct_reply_expected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IncomingAdmission {
    pub(crate) decision: OutgoingExecutiveDecision,
    pub(crate) ticket: ReplyTicket,
}

impl Default for OutgoingExecutiveContext {
    fn default() -> Self {
        Self {
            incoming_impact: IncomingTurnImpact::Unknown,
            direct_reply_expected: true,
        }
    }
}

pub(crate) struct ConversationCoordinator;

impl ConversationCoordinator {
    /// Resolve semantic contention after the coordinator's hard validity
    /// checks and before the irreversible commit point.
    ///
    /// Precedence is deliberate: content invalidation and same-topic context
    /// are resolved before an unrelated direct turn can defer proactive work.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn decide_prepared_outgoing(
        prepared_source: Option<OutgoingSource>,
        context: OutgoingExecutiveContext,
    ) -> OutgoingExecutiveDecision {
        match context.incoming_impact {
            // Unknown is the earliest fail-closed pass. A proactive envelope
            // is cancelled as a deferral; every other source is superseded.
            IncomingTurnImpact::Unknown
                if prepared_source == Some(OutgoingSource::Proactive) =>
            {
                OutgoingExecutiveDecision::Defer
            }
            IncomingTurnImpact::Unknown | IncomingTurnImpact::InvalidatesPendingContent => {
                OutgoingExecutiveDecision::Rewrite
            }
            IncomingTurnImpact::ExtendsPendingTopic => OutgoingExecutiveDecision::Merge,
            IncomingTurnImpact::Unrelated
                if context.direct_reply_expected && prepared_source.is_none() =>
            {
                OutgoingExecutiveDecision::Rewrite
            }
            IncomingTurnImpact::Unrelated
                if context.direct_reply_expected
                    && prepared_source == Some(OutgoingSource::Proactive) =>
            {
                OutgoingExecutiveDecision::Defer
            }
            IncomingTurnImpact::None | IncomingTurnImpact::Unrelated => {
                OutgoingExecutiveDecision::Keep
            }
        }
    }

    pub(crate) async fn interrupt(scope: ReplyScope) -> ReplyTicket {
        Self::begin_incoming(scope).await.ticket
    }

    /// Earliest fail-closed ingress admission. Unlike `interrupt`, this keeps
    /// the Executive decision so the production handler can preserve an
    /// initial proactive Defer through its later semantic refinement.
    pub(crate) async fn begin_incoming(scope: ReplyScope) -> IncomingAdmission {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        Self::begin_incoming_locked(scope).await
    }

    pub(crate) async fn interrupt_locked(scope: ReplyScope) -> ReplyTicket {
        Self::begin_incoming_locked(scope).await.ticket
    }

    pub(crate) async fn begin_incoming_locked(scope: ReplyScope) -> IncomingAdmission {
        let (decision, ticket) =
            Self::apply_incoming_locked(scope, OutgoingExecutiveContext::default()).await;
        IncomingAdmission {
            decision,
            ticket: ticket.expect("the fail-closed ingress policy must advance the generation"),
        }
    }

    /// Apply a caller-owned semantic classification at the inbound
    /// linearization point. This method never holds the scope lock across a
    /// model call or platform operation.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn admit_incoming(
        scope: ReplyScope,
        context: OutgoingExecutiveContext,
    ) -> OutgoingExecutiveDecision {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        Self::admit_incoming_locked(scope, context).await
    }

    /// Locked form for group/private batching paths that already serialize on
    /// the conversation scope.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn admit_incoming_locked(
        scope: ReplyScope,
        context: OutgoingExecutiveContext,
    ) -> OutgoingExecutiveDecision {
        Self::apply_incoming_locked(scope, context).await.0
    }

    /// Apply a completed semantic decision only while its ingress ticket is
    /// still authoritative. This prevents a slow understanding pass from
    /// rewriting or merging a newer conversation generation.
    pub(crate) async fn admit_current_incoming(
        ingress: ReplyTicket,
        context: OutgoingExecutiveContext,
    ) -> Option<IncomingAdmission> {
        let scope = ingress.scope();
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        if !is_current_locked(ingress).await {
            return None;
        }
        let (decision, next_ticket) = Self::apply_incoming_locked(scope, context).await;
        Some(IncomingAdmission {
            decision,
            ticket: next_ticket.unwrap_or(ingress),
        })
    }

    /// Refine the earliest admission with the existing semantic pass. If that
    /// first pass already deferred and cancelled a proactive envelope, Defer
    /// remains authoritative: the payload is intentionally not resurrected.
    /// With no newly prepared envelope, other semantic decisions reuse the
    /// ingress generation instead of advancing it redundantly.
    pub(crate) async fn refine_current_incoming(
        initial: IncomingAdmission,
        context: OutgoingExecutiveContext,
    ) -> Option<IncomingAdmission> {
        let scope = initial.ticket.scope();
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        if !is_current_locked(initial.ticket).await {
            return None;
        }
        if prepared_outgoing_source_locked(scope).await.is_none() {
            if initial.decision == OutgoingExecutiveDecision::Defer {
                return Some(initial);
            }
            return Some(IncomingAdmission {
                decision: Self::decide_prepared_outgoing(None, context),
                ticket: initial.ticket,
            });
        }
        let (decision, next_ticket) = Self::apply_incoming_locked(scope, context).await;
        Some(IncomingAdmission {
            decision,
            ticket: next_ticket.unwrap_or(initial.ticket),
        })
    }

    /// Reuse the existing one-pass semantic result. No additional model call
    /// is introduced solely for concurrent outgoing arbitration.
    pub(crate) fn context_for_understood_turn(
        understanding: &MessageUnderstanding,
        direct_reply_expected: bool,
    ) -> OutgoingExecutiveContext {
        let incoming_impact = if understanding.conversation_relevant {
            IncomingTurnImpact::ExtendsPendingTopic
        } else if direct_reply_expected {
            IncomingTurnImpact::Unrelated
        } else {
            IncomingTurnImpact::None
        };
        OutgoingExecutiveContext {
            incoming_impact,
            direct_reply_expected,
        }
    }

    /// Stop is stronger than semantic supersession and remains conditional on
    /// the same ingress ticket, so delayed understanding cannot cancel newer
    /// work.
    pub(crate) async fn cancel_current_incoming(ingress: ReplyTicket) -> Option<ReplyTicket> {
        let scope = ingress.scope();
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        Self::cancel_current_incoming_locked(ingress).await
    }

    pub(crate) async fn cancel_current_incoming_locked(
        ingress: ReplyTicket,
    ) -> Option<ReplyTicket> {
        cancel_if_current_locked(ingress).await
    }

    async fn apply_incoming_locked(
        scope: ReplyScope,
        context: OutgoingExecutiveContext,
    ) -> (OutgoingExecutiveDecision, Option<ReplyTicket>) {
        let prepared_source = prepared_outgoing_source_locked(scope).await;
        let decision = Self::decide_prepared_outgoing(prepared_source, context);
        let ticket = match decision {
            // Preserving the whole ticket is required: advancing only the
            // conversation version would still make the prepared token stale.
            OutgoingExecutiveDecision::Keep => None,
            OutgoingExecutiveDecision::Rewrite | OutgoingExecutiveDecision::Merge => {
                Some(supersede_locked(scope).await)
            }
            OutgoingExecutiveDecision::Defer => {
                if cancel_prepared_proactive_locked(scope).await {
                    // The direct turn cannot share a ticket with the deferred
                    // proactive task. Preserve its Cancelled terminal state,
                    // then advance ownership for the incoming reply.
                    Some(supersede_locked(scope).await)
                } else {
                    // State should be stable under the scope lock. Still fail
                    // closed if the authoritative proactive disappeared.
                    Some(supersede_locked(scope).await)
                }
            }
        };
        (decision, ticket)
    }

    pub(crate) async fn is_active_locked(scope: ReplyScope) -> bool {
        is_active_locked(scope).await
    }

    pub(crate) async fn begin_reply_locked(
        scope: ReplyScope,
        ticket: ReplyTicket,
        source_message_ids: Vec<i32>,
    ) -> bool {
        begin_reply_locked(scope, ticket, source_message_ids).await
    }

    pub(crate) async fn claim_follow_up_locked(completed: ReplyTicket) -> Option<ReplyTicket> {
        claim_follow_up_locked(completed).await
    }

    /// 统一队列上限，保证两个入口不会各自演化出不同的丢弃策略。
    pub(crate) fn enqueue(
        queue: &mut VecDeque<PendingTurn>,
        turn: PendingTurn,
        scope_label: &str,
        scope_id: i64,
    ) {
        let max_pending = config::get().traffic().max_pending_turns();
        if queue.len() >= max_pending {
            queue.pop_front();
            eprintln!(
                "[WARN] {}待处理队列已满，丢弃最旧 turn (范围: {}, 上限: {})",
                scope_label, scope_id, max_pending
            );
        }
        queue.push_back(turn);
    }

    /// 领取排队 turn 时必须持有同一会话锁，避免旧 drainer 抢走新消息的代数。
    pub(crate) async fn claim_next_locked(
        scope: ReplyScope,
        completed: &mut ReplyTicket,
        queue: &mut VecDeque<PendingTurn>,
    ) -> Option<(PendingTurn, ReplyTicket)> {
        loop {
            let pending = queue.pop_front()?;
            let Some(ticket) = Self::claim_follow_up_locked(*completed).await else {
                queue.push_front(pending);
                return None;
            };
            if Self::begin_reply_locked(scope, ticket, pending.message_ids.clone()).await {
                return Some((pending, ticket));
            }
            *completed = ticket;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationCoordinator, IncomingTurnImpact, OutgoingExecutiveContext,
        OutgoingExecutiveDecision,
    };
    use crate::model::interrupt::{
        OutgoingSource, OutgoingState, ReplyScope, commit_outgoing, finish, is_current,
        mark_active, mark_outgoing_failed, outgoing_fingerprint, prepare_outgoing,
        test_outgoing_state,
    };
    use crate::model::semantic::MessageUnderstanding;

    fn decide(
        source: OutgoingSource,
        incoming_impact: IncomingTurnImpact,
        direct_reply_expected: bool,
    ) -> OutgoingExecutiveDecision {
        ConversationCoordinator::decide_prepared_outgoing(
            Some(source),
            OutgoingExecutiveContext {
                incoming_impact,
                direct_reply_expected,
            },
        )
    }

    #[test]
    fn keeps_content_when_the_new_turn_has_no_material_effect() {
        assert_eq!(
            decide(OutgoingSource::Reply, IncomingTurnImpact::None, true),
            OutgoingExecutiveDecision::Keep
        );
        assert_eq!(
            decide(OutgoingSource::Proactive, IncomingTurnImpact::None, false),
            OutgoingExecutiveDecision::Keep
        );
    }

    #[test]
    fn unclassified_input_is_fail_closed_and_defers_known_proactive_work() {
        assert_eq!(
            decide(
                OutgoingSource::Proactive,
                IncomingTurnImpact::default(),
                false,
            ),
            OutgoingExecutiveDecision::Defer
        );
        assert_eq!(
            ConversationCoordinator::decide_prepared_outgoing(
                Some(OutgoingSource::Reply),
                OutgoingExecutiveContext::default(),
            ),
            OutgoingExecutiveDecision::Rewrite
        );
    }

    #[test]
    fn rewrites_when_the_new_turn_answers_or_invalidates_pending_content() {
        for source in [OutgoingSource::Reply, OutgoingSource::Proactive] {
            assert_eq!(
                decide(source, IncomingTurnImpact::InvalidatesPendingContent, true,),
                OutgoingExecutiveDecision::Rewrite
            );
        }
    }

    #[test]
    fn merges_compatible_same_topic_context_into_one_outgoing() {
        for source in [OutgoingSource::Reply, OutgoingSource::Proactive] {
            assert_eq!(
                decide(source, IncomingTurnImpact::ExtendsPendingTopic, true),
                OutgoingExecutiveDecision::Merge
            );
        }
    }

    #[test]
    fn unrelated_direct_reply_defers_prepared_proactive_content() {
        assert_eq!(
            decide(
                OutgoingSource::Proactive,
                IncomingTurnImpact::Unrelated,
                true,
            ),
            OutgoingExecutiveDecision::Defer
        );
    }

    #[test]
    fn unrelated_non_reply_turn_does_not_suppress_proactive_content() {
        assert_eq!(
            decide(
                OutgoingSource::Proactive,
                IncomingTurnImpact::Unrelated,
                false,
            ),
            OutgoingExecutiveDecision::Keep
        );
    }

    #[test]
    fn unrelated_turn_does_not_semantically_preempt_a_direct_reply() {
        assert_eq!(
            decide(OutgoingSource::Reply, IncomingTurnImpact::Unrelated, true,),
            OutgoingExecutiveDecision::Keep
        );
    }

    #[test]
    fn semantic_change_takes_precedence_over_proactive_deferral() {
        assert_eq!(
            decide(
                OutgoingSource::Proactive,
                IncomingTurnImpact::InvalidatesPendingContent,
                true,
            ),
            OutgoingExecutiveDecision::Rewrite
        );
        assert_eq!(
            decide(
                OutgoingSource::Proactive,
                IncomingTurnImpact::ExtendsPendingTopic,
                true,
            ),
            OutgoingExecutiveDecision::Merge
        );
    }

    #[test]
    fn keep_preserves_the_prepared_ticket_and_allows_commit() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_001);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("still relevant"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("current reply should prepare");

                let decision = ConversationCoordinator::admit_incoming(
                    scope,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await;

                assert_eq!(decision, OutgoingExecutiveDecision::Keep);
                assert!(is_current(ticket).await);
                assert!(commit_outgoing(outgoing).await);
                mark_outgoing_failed(outgoing).await;
                finish(ticket).await;
            });
    }

    #[test]
    fn rewrite_and_merge_both_supersede_prepared_content() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                for (index, impact, expected) in [
                    (
                        0,
                        IncomingTurnImpact::InvalidatesPendingContent,
                        OutgoingExecutiveDecision::Rewrite,
                    ),
                    (
                        1,
                        IncomingTurnImpact::ExtendsPendingTopic,
                        OutgoingExecutiveDecision::Merge,
                    ),
                ] {
                    let scope = ReplyScope::Private(9_300_010 + index);
                    let ticket = ConversationCoordinator::interrupt(scope).await;
                    assert!(mark_active(ticket).await);
                    let outgoing = prepare_outgoing(
                        ticket,
                        outgoing_fingerprint("content needs another generation"),
                        OutgoingSource::Reply,
                    )
                    .await
                    .expect("current reply should prepare");

                    let decision = ConversationCoordinator::admit_incoming(
                        scope,
                        OutgoingExecutiveContext {
                            incoming_impact: impact,
                            direct_reply_expected: true,
                        },
                    )
                    .await;

                    assert_eq!(decision, expected);
                    assert!(!is_current(ticket).await);
                    assert!(!commit_outgoing(outgoing).await);
                }
            });
    }

    #[test]
    fn defer_cancels_an_explicitly_identified_prepared_proactive() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_020);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("prepared check-in"),
                    OutgoingSource::Proactive,
                )
                .await
                .expect("current proactive should prepare");

                let decision = ConversationCoordinator::admit_incoming(
                    scope,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::Unrelated,
                        direct_reply_expected: true,
                    },
                )
                .await;

                assert_eq!(decision, OutgoingExecutiveDecision::Defer);
                assert!(!is_current(ticket).await);
                assert!(!commit_outgoing(outgoing).await);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Cancelled)
                );
                finish(ticket).await;
            });
    }

    #[test]
    fn production_ingress_preserves_initial_defer_during_semantic_refinement() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_021);
                let proactive_ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(proactive_ticket).await);
                let outgoing = prepare_outgoing(
                    proactive_ticket,
                    outgoing_fingerprint("prepared proactive"),
                    OutgoingSource::Proactive,
                )
                .await
                .expect("proactive output should prepare");

                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert_eq!(initial.decision, OutgoingExecutiveDecision::Defer);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Cancelled)
                );

                let refined = ConversationCoordinator::refine_current_incoming(
                    initial,
                    ConversationCoordinator::context_for_understood_turn(
                        &MessageUnderstanding::default(),
                        true,
                    ),
                )
                .await
                .expect("the ingress should remain current");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Defer);
                assert_eq!(refined.ticket, initial.ticket);
                assert!(!commit_outgoing(outgoing).await);
            });
    }

    #[test]
    fn semantic_stop_reclassifies_the_ingress_supersession_as_cancelled() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_022);
                let reply_ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(reply_ticket).await);
                let outgoing = prepare_outgoing(
                    reply_ticket,
                    outgoing_fingerprint("stop this reply"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply output should prepare");

                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert_eq!(initial.decision, OutgoingExecutiveDecision::Rewrite);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Superseded)
                );

                let stopped = ConversationCoordinator::cancel_current_incoming(initial.ticket)
                    .await
                    .expect("current stop should cancel");

                assert!(is_current(stopped).await);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Cancelled)
                );
                assert!(!commit_outgoing(outgoing).await);
            });
    }
}
