//! 群聊和私聊共享的会话生命周期与排队策略。

use super::interrupt::{
    OutgoingSource, OutgoingState, OutgoingToken, ReplyScope, ReplyTicket,
    active_incoming_reservation_matches_locked, active_ticket_locked, cancel_if_current_locked,
    cancel_prepared_proactive_locked, claim_follow_up_locked, has_other_pending_incoming_locked,
    incoming_reservation_matches_locked, interrupt_locked as supersede_locked, is_active_locked,
    is_current_locked, pending_incoming_for_ticket_locked,
    prepared_outgoing_source_for_token_locked, prepared_outgoing_source_locked,
    prepared_semantic_preview_for_token_locked, release_active_incoming_by_id_locked,
    release_incoming_locked, reserve_active_incoming_locked, reserve_incoming_locked, scope_mutex,
    supersede_active_incoming_locked, try_freeze_prepared_for_incoming_locked,
    wait_for_active_incoming_clear, wait_for_active_incoming_turn, wait_for_pending_incoming,
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
    /// `true` only when semantic refinement selected Keep for an outgoing
    /// envelope that is still Prepared. The caller must not prepare a second
    /// visible reply on the same ticket in that case.
    pub(crate) preserved_prepared: bool,
    /// The ingress linearization point retained an existing Prepared envelope
    /// until semantic refinement. Only the exact admission may resolve it.
    pub(crate) frozen_prepared: bool,
    /// The ingress observed a reply that is still being generated. Keep the
    /// current ticket until the semantic pass decides whether that reply is
    /// still meaningful; this is intentionally separate from a Prepared token.
    pub(crate) active_reply_preserved: bool,
    reservation_id: u64,
    frozen_token: Option<OutgoingToken>,
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
            IncomingTurnImpact::Unknown if prepared_source == Some(OutgoingSource::Proactive) => {
                OutgoingExecutiveDecision::Defer
            }
            IncomingTurnImpact::Unknown | IncomingTurnImpact::InvalidatesPendingContent => {
                OutgoingExecutiveDecision::Rewrite
            }
            IncomingTurnImpact::ExtendsPendingTopic => OutgoingExecutiveDecision::Merge,
            IncomingTurnImpact::Unrelated
                if context.direct_reply_expected
                    && prepared_source == Some(OutgoingSource::Proactive) =>
            {
                OutgoingExecutiveDecision::Defer
            }
            // There is no second follow-up queue for an independent direct
            // turn. Keeping an older reactive reply here would consume the new
            // plan and leave that turn unanswered, so regenerate one reply.
            IncomingTurnImpact::Unrelated if context.direct_reply_expected => {
                OutgoingExecutiveDecision::Rewrite
            }
            IncomingTurnImpact::None | IncomingTurnImpact::Unrelated => {
                OutgoingExecutiveDecision::Keep
            }
        }
    }

    /// Decide how a message affects a reply that is still inside model
    /// generation and therefore has no trustworthy outgoing preview yet.
    /// Independent or observational traffic must not erase that reply before
    /// the current turn has a chance to finish. An unknown result also keeps
    /// the in-flight reply: without a trustworthy classification there is no
    /// evidence that it became meaningless.
    fn decide_active_reply(context: OutgoingExecutiveContext) -> OutgoingExecutiveDecision {
        match context.incoming_impact {
            IncomingTurnImpact::ExtendsPendingTopic => OutgoingExecutiveDecision::Merge,
            IncomingTurnImpact::InvalidatesPendingContent => OutgoingExecutiveDecision::Rewrite,
            IncomingTurnImpact::None
            | IncomingTurnImpact::Unrelated
            | IncomingTurnImpact::Unknown => OutgoingExecutiveDecision::Keep,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn interrupt(scope: ReplyScope) -> ReplyTicket {
        Self::begin_incoming(scope).await.ticket
    }

    /// Earliest fail-closed ingress admission. Unlike `interrupt`, this keeps
    /// the Executive decision so the production handler can preserve an
    /// initial proactive Defer, or an already-generating reply, through its
    /// later semantic refinement.
    pub(crate) async fn begin_incoming(scope: ReplyScope) -> IncomingAdmission {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        Self::begin_incoming_locked(scope).await
    }

    pub(crate) async fn interrupt_locked(scope: ReplyScope) -> ReplyTicket {
        supersede_locked(scope).await
    }

    pub(crate) async fn begin_incoming_locked(scope: ReplyScope) -> IncomingAdmission {
        // An active turn takes precedence over a Prepared envelope it may have
        // produced moments ago. Letting the envelope freeze first could route
        // a second owner onto the same active ticket.
        if let Some(ticket) = active_ticket_locked(scope).await
            && let Some(reservation_id) = reserve_active_incoming_locked(ticket).await
        {
            return IncomingAdmission {
                decision: OutgoingExecutiveDecision::Keep,
                ticket,
                preserved_prepared: false,
                frozen_prepared: false,
                active_reply_preserved: true,
                reservation_id,
                frozen_token: None,
            };
        }
        // A marker may have expired between the observation and the
        // reservation attempt. Continue through the ordinary admission path
        // against the now-current state instead of blindly invalidating an
        // in-flight reply.
        if let Some((token, source, reservation_id)) =
            try_freeze_prepared_for_incoming_locked(scope).await
        {
            return IncomingAdmission {
                decision: Self::decide_prepared_outgoing(
                    Some(source),
                    OutgoingExecutiveContext::default(),
                ),
                ticket: token.ticket(),
                preserved_prepared: false,
                frozen_prepared: true,
                active_reply_preserved: false,
                reservation_id,
                frozen_token: Some(token),
            };
        }
        let (decision, ticket) =
            Self::apply_incoming_locked(scope, OutgoingExecutiveContext::default()).await;
        let ticket = ticket.expect("the fail-closed ingress policy must advance the generation");
        let reservation_id = reserve_incoming_locked(ticket)
            .await
            .expect("the new ingress generation must accept its reservation");
        IncomingAdmission {
            decision,
            ticket,
            preserved_prepared: false,
            frozen_prepared: false,
            active_reply_preserved: false,
            reservation_id,
            frozen_token: None,
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

    /// Refine the earliest admission with the existing semantic pass. If that
    /// first pass already deferred and cancelled a proactive envelope, Defer
    /// remains authoritative: the payload is intentionally not resurrected.
    /// With no newly prepared envelope, other semantic decisions reuse the
    /// ingress generation instead of advancing it redundantly. An active
    /// reply is advanced only when the semantic result says it is invalidated
    /// or must be merged into a replacement turn.
    pub(crate) async fn refine_current_incoming(
        mut initial: IncomingAdmission,
        context: OutgoingExecutiveContext,
    ) -> Option<IncomingAdmission> {
        let scope = initial.ticket.scope();
        // Active reservations are refined in ingress order. A later semantic
        // task waits for earlier reservations to resolve; the wait also
        // returns the reservation's rebound successor ticket when an earlier
        // turn replaced the in-flight reply.
        if initial.active_reply_preserved && initial.reservation_id != 0 {
            initial.ticket = wait_for_active_incoming_turn(scope, initial.reservation_id).await?;
        }
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        if !is_current_locked(initial.ticket).await {
            return None;
        }
        if initial.active_reply_preserved {
            if !active_incoming_reservation_matches_locked(initial.ticket, initial.reservation_id)
                .await
                && initial.reservation_id != 0
            {
                return None;
            }
            let decision = Self::decide_active_reply(context);
            if decision == OutgoingExecutiveDecision::Keep {
                release_active_incoming_by_id_locked(scope, initial.reservation_id).await?;
                return Some(IncomingAdmission {
                    decision,
                    ticket: initial.ticket,
                    preserved_prepared: false,
                    frozen_prepared: false,
                    active_reply_preserved: false,
                    // The active marker has already been released. Keeping a
                    // second normal reservation here would block the FIFO
                    // drainer even though this message is intentionally
                    // allowed to wait behind the current reply.
                    reservation_id: 0,
                    frozen_token: None,
                });
            }
            // The active reply has no safe semantic preview yet. Once the
            // classifier says it must be replaced, advance exactly once. Keep
            // later active reservations attached to the successor so their
            // semantic tasks cannot become stale or disappear.
            let next_ticket = supersede_active_incoming_locked(
                scope,
                initial.reservation_id,
                OutgoingState::Superseded,
            )
            .await?;
            // The replacing turn owns the successor generation until its
            // handler claims it. Later ingress is attached behind it rather
            // than making this admission stale in the hand-off window.
            let reservation_id = reserve_incoming_locked(next_ticket).await.unwrap_or(0);
            return Some(IncomingAdmission {
                decision,
                ticket: next_ticket,
                preserved_prepared: false,
                frozen_prepared: false,
                active_reply_preserved: false,
                reservation_id,
                frozen_token: None,
            });
        }
        if initial.frozen_prepared {
            if !incoming_reservation_matches_locked(
                initial.ticket,
                initial.reservation_id,
                initial.frozen_token,
            )
            .await
            {
                return None;
            }
            let frozen_token = initial.frozen_token?;
            let Some(prepared_source) =
                prepared_outgoing_source_for_token_locked(frozen_token).await
            else {
                release_incoming_locked(
                    initial.ticket,
                    initial.reservation_id,
                    initial.frozen_token,
                    true,
                )
                .await;
                return None;
            };
            let decision = Self::decide_prepared_outgoing(Some(prepared_source), context);
            if decision == OutgoingExecutiveDecision::Keep {
                if !release_incoming_locked(
                    initial.ticket,
                    initial.reservation_id,
                    initial.frozen_token,
                    false,
                )
                .await
                {
                    return None;
                }
                return Some(IncomingAdmission {
                    decision,
                    ticket: initial.ticket,
                    preserved_prepared: true,
                    frozen_prepared: false,
                    active_reply_preserved: false,
                    reservation_id: initial.reservation_id,
                    frozen_token: None,
                });
            }
            let (applied, next_ticket) = Self::apply_incoming_locked(scope, context).await;
            return Some(IncomingAdmission {
                decision: applied,
                ticket: next_ticket.unwrap_or(initial.ticket),
                preserved_prepared: false,
                frozen_prepared: false,
                active_reply_preserved: false,
                reservation_id: initial.reservation_id,
                frozen_token: None,
            });
        }
        if prepared_outgoing_source_locked(scope).await.is_none() {
            if initial.decision == OutgoingExecutiveDecision::Defer {
                return Some(initial);
            }
            return Some(IncomingAdmission {
                decision: Self::decide_prepared_outgoing(None, context),
                ticket: initial.ticket,
                preserved_prepared: false,
                frozen_prepared: false,
                active_reply_preserved: false,
                reservation_id: initial.reservation_id,
                frozen_token: None,
            });
        }
        let (decision, next_ticket) = Self::apply_incoming_locked(scope, context).await;
        Some(IncomingAdmission {
            decision,
            ticket: next_ticket.unwrap_or(initial.ticket),
            preserved_prepared: decision == OutgoingExecutiveDecision::Keep,
            frozen_prepared: false,
            active_reply_preserved: false,
            reservation_id: initial.reservation_id,
            frozen_token: None,
        })
    }

    /// A control command has an explicit user-visible response and cannot be
    /// silently dropped just because an older model reply is still running.
    /// Treat that command as an intentional replacement, then let the normal
    /// tracked sender acquire a fresh generation. Ordinary conversational
    /// turns never use this escape hatch; they go through semantic refinement.
    pub(crate) async fn resolve_active_reply_for_direct_response(
        admission: IncomingAdmission,
    ) -> bool {
        if !admission.active_reply_preserved {
            return true;
        }
        let scope = admission.ticket.scope();
        let Some(resolved) = Self::refine_current_incoming(
            admission,
            OutgoingExecutiveContext {
                incoming_impact: IncomingTurnImpact::InvalidatesPendingContent,
                direct_reply_expected: true,
            },
        )
        .await
        else {
            return false;
        };
        let replacing = resolved.decision == OutgoingExecutiveDecision::Rewrite;
        if replacing {
            // The direct sender will establish its own tracked generation.
            // Release the temporary successor reservation first, otherwise a
            // subsequent `interrupt` would mistake it for another owner and
            // leave the command response waiting behind itself.
            Self::abandon_incoming(resolved).await;
        }
        replacing && wait_for_active_incoming_clear(scope).await
    }

    /// Release an admission that cannot reach semantic refinement. Frozen
    /// content is resolved with the admission's conservative Unknown policy;
    /// an ordinary queued reservation is simply relinquished.
    pub(crate) async fn abandon_incoming(admission: IncomingAdmission) -> bool {
        let scope_lock = scope_mutex(admission.ticket.scope());
        let _scope_guard = scope_lock.lock().await;
        Self::abandon_incoming_locked(admission).await
    }

    /// Release an admission while the caller already holds the scope lock.
    /// Queue hand-off paths use this to avoid leaving their own reservation in
    /// the coordinator after moving the payload into the adapter FIFO.
    pub(crate) async fn abandon_incoming_locked(admission: IncomingAdmission) -> bool {
        if admission.active_reply_preserved {
            return release_active_incoming_by_id_locked(
                admission.ticket.scope(),
                admission.reservation_id,
            )
            .await
            .is_some();
        }
        release_incoming_locked(
            admission.ticket,
            admission.reservation_id,
            admission.frozen_token,
            true,
        )
        .await
    }

    /// Read the bounded body of the exact Prepared envelope frozen by this
    /// admission. A stale admission or a replaced envelope yields no context.
    pub(crate) async fn frozen_prepared_semantic_preview(
        admission: IncomingAdmission,
    ) -> Option<String> {
        let frozen_token = admission.frozen_token?;
        let scope_lock = scope_mutex(admission.ticket.scope());
        let _scope_guard = scope_lock.lock().await;
        if !incoming_reservation_matches_locked(
            admission.ticket,
            admission.reservation_id,
            admission.frozen_token,
        )
        .await
        {
            return None;
        }
        prepared_semantic_preview_for_token_locked(frozen_token).await
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
    #[cfg_attr(not(test), allow(dead_code))]
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

    pub(crate) async fn has_other_pending_incoming_locked(admission: IncomingAdmission) -> bool {
        has_other_pending_incoming_locked(admission.ticket, admission.reservation_id).await
    }

    pub(crate) async fn current_ticket(scope: ReplyScope) -> Option<ReplyTicket> {
        super::interrupt::current_ticket(scope).await
    }

    pub(crate) async fn current_ticket_locked(scope: ReplyScope) -> Option<ReplyTicket> {
        super::interrupt::current_ticket_locked(scope).await
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

    pub(crate) async fn pending_incoming_for_ticket_locked(ticket: ReplyTicket) -> bool {
        pending_incoming_for_ticket_locked(ticket).await
    }

    pub(crate) async fn wait_for_pending_incoming(ticket: ReplyTicket) -> bool {
        wait_for_pending_incoming(ticket).await
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
            // A front active admission may have replaced the completed reply
            // while its drainer was waiting. Adopt that successor only when
            // no newer handler is already active; this keeps the old drainer
            // alive for the FIFO queue without letting it steal a live turn.
            if !is_current_locked(*completed).await
                && !is_active_locked(scope).await
                && let Some(current) = Self::current_ticket_locked(scope).await
                && current.scope_epoch() == completed.scope_epoch()
            {
                *completed = current;
            }
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
        prepare_outgoing_with_semantic_preview, prepare_proactive_outgoing_if_idle,
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
    fn unrelated_direct_turn_rewrites_a_prepared_reactive_reply() {
        assert_eq!(
            decide(OutgoingSource::Reply, IncomingTurnImpact::Unrelated, true,),
            OutgoingExecutiveDecision::Rewrite
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
    fn active_reply_survives_unrelated_ingress_until_semantic_refinement() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_030);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);

                let admission = ConversationCoordinator::begin_incoming(scope).await;
                assert_eq!(admission.ticket, ticket);
                assert!(admission.active_reply_preserved);
                assert!(is_current(ticket).await);

                let refined = ConversationCoordinator::refine_current_incoming(
                    admission,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::Unrelated,
                        direct_reply_expected: true,
                    },
                )
                .await
                .expect("active admission should remain current");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(!refined.preserved_prepared);
                assert!(is_current(ticket).await);
                finish(ticket).await;
            });
    }

    #[test]
    fn active_reply_survives_when_semantic_classification_is_unavailable() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_034);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let admission = ConversationCoordinator::begin_incoming(scope).await;

                let refined = ConversationCoordinator::refine_current_incoming(
                    admission,
                    OutgoingExecutiveContext::default(),
                )
                .await
                .expect("unknown classification should keep the active turn current");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(is_current(ticket).await);
                finish(ticket).await;
            });
    }

    #[test]
    fn pending_admission_is_not_superseded_by_later_ingress() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_041);
                let first = ConversationCoordinator::begin_incoming(scope).await;
                let second = ConversationCoordinator::begin_incoming(scope).await;

                assert!(!first.active_reply_preserved);
                assert!(second.active_reply_preserved);
                assert_eq!(first.ticket, second.ticket);
                assert!(is_current(first.ticket).await);

                let first = ConversationCoordinator::refine_current_incoming(
                    first,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("the first admission must remain claimable");
                assert!(mark_active(first.ticket).await);

                let second = ConversationCoordinator::refine_current_incoming(
                    second,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("the later admission must remain current");
                assert_eq!(second.decision, OutgoingExecutiveDecision::Keep);
                finish(first.ticket).await;
            });
    }

    #[test]
    fn multiple_active_admissions_release_independently() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_035);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let first = ConversationCoordinator::begin_incoming(scope).await;
                let second = ConversationCoordinator::begin_incoming(scope).await;
                assert!(first.active_reply_preserved);
                assert!(second.active_reply_preserved);
                assert_ne!(first.reservation_id, second.reservation_id);

                let first_refined = ConversationCoordinator::refine_current_incoming(
                    first,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("first active admission should remain current");
                assert_eq!(first_refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(
                    prepare_proactive_outgoing_if_idle(
                        scope,
                        outgoing_fingerprint("still blocked")
                    )
                    .await
                    .is_none(),
                    "the second admission must keep proactive work blocked"
                );

                let second_refined = ConversationCoordinator::refine_current_incoming(
                    second,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("second active admission should remain current");
                assert_eq!(second_refined.decision, OutgoingExecutiveDecision::Keep);
                finish(ticket).await;
                let proactive = prepare_proactive_outgoing_if_idle(
                    scope,
                    outgoing_fingerprint("allowed after both admissions"),
                )
                .await
                .expect("proactive work should resume after both admissions");
                mark_outgoing_failed(proactive).await;
            });
    }

    #[test]
    fn active_reply_waits_for_semantic_refinement_before_commit() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_031);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let admission = ConversationCoordinator::begin_incoming(scope).await;
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("reply generated before the new message"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("active reply should prepare");

                let commit = kovi::tokio::spawn(async move { commit_outgoing(outgoing).await });
                kovi::tokio::task::yield_now().await;
                assert!(
                    !commit.is_finished(),
                    "an active reply must wait while semantic refinement is pending"
                );

                let refined = ConversationCoordinator::refine_current_incoming(
                    admission,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("active admission should remain current");
                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(
                    commit.await.expect("commit task should finish"),
                    "the preserved active reply should commit after Keep"
                );
                mark_outgoing_failed(outgoing).await;
                finish(ticket).await;
            });
    }

    #[test]
    fn active_reply_precedes_a_prepared_envelope_at_ingress() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_036);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("active reply is already at the send boundary"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("active reply should prepare");

                let admission = ConversationCoordinator::begin_incoming(scope).await;
                assert!(admission.active_reply_preserved);
                assert!(!admission.frozen_prepared);
                assert_eq!(admission.ticket, ticket);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Prepared)
                );

                let refined = ConversationCoordinator::refine_current_incoming(
                    admission,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("active admission should remain current");
                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(commit_outgoing(outgoing).await);
                mark_outgoing_failed(outgoing).await;
                finish(ticket).await;
            });
    }

    #[test]
    fn direct_control_response_replaces_an_active_reply() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_038);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let admission = ConversationCoordinator::begin_incoming(scope).await;

                assert!(
                    ConversationCoordinator::resolve_active_reply_for_direct_response(admission)
                        .await
                );
                assert!(!is_current(ticket).await);

                let command_ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(command_ticket).await);
                let command_reply = prepare_outgoing(
                    command_ticket,
                    outgoing_fingerprint("command response"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("the command response should acquire a fresh generation");
                assert!(commit_outgoing(command_reply).await);
                mark_outgoing_failed(command_reply).await;
                finish(command_ticket).await;
            });
    }

    #[test]
    fn active_reply_is_replaced_only_for_semantic_invalidation() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_032);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let admission = ConversationCoordinator::begin_incoming(scope).await;

                let refined = ConversationCoordinator::refine_current_incoming(
                    admission,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::InvalidatesPendingContent,
                        direct_reply_expected: true,
                    },
                )
                .await
                .expect("invalidating admission should advance to a new ticket");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Rewrite);
                assert_ne!(refined.ticket, ticket);
                assert!(!is_current(ticket).await);
                assert!(is_current(refined.ticket).await);
                finish(refined.ticket).await;
            });
    }

    #[test]
    fn active_admission_blocks_proactive_work_until_it_is_resolved() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_033);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let admission = ConversationCoordinator::begin_incoming(scope).await;
                finish(ticket).await;

                assert!(
                    prepare_proactive_outgoing_if_idle(scope, outgoing_fingerprint("blocked"))
                        .await
                        .is_none(),
                    "pending semantic admission must keep proactive work out"
                );
                let refined = ConversationCoordinator::refine_current_incoming(
                    admission,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("active admission should remain current");
                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);

                let proactive = prepare_proactive_outgoing_if_idle(
                    scope,
                    outgoing_fingerprint("allowed after refinement"),
                )
                .await
                .expect("proactive work should resume after refinement");
                mark_outgoing_failed(proactive).await;
            });
    }

    #[test]
    fn active_admission_survives_the_original_reply_finishing_first() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Group(9_300_037);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let first = ConversationCoordinator::begin_incoming(scope).await;
                finish(ticket).await;

                let second = ConversationCoordinator::begin_incoming(scope).await;
                assert!(first.active_reply_preserved);
                assert!(second.active_reply_preserved);
                assert_eq!(second.ticket, ticket);
                assert_ne!(first.reservation_id, second.reservation_id);

                let first = ConversationCoordinator::refine_current_incoming(
                    first,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("first active admission should remain current");
                assert_eq!(first.decision, OutgoingExecutiveDecision::Keep);
                let second = ConversationCoordinator::refine_current_incoming(
                    second,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("second active admission should remain current");
                assert_eq!(second.decision, OutgoingExecutiveDecision::Keep);
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
    fn production_ingress_freezes_then_defers_prepared_proactive_content() {
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
                finish(proactive_ticket).await;

                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert_eq!(initial.decision, OutgoingExecutiveDecision::Defer);
                assert!(initial.frozen_prepared);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Prepared)
                );
                let commit = kovi::tokio::spawn(async move { commit_outgoing(outgoing).await });
                kovi::tokio::task::yield_now().await;
                assert!(
                    !commit.is_finished(),
                    "frozen Prepared content must not cross commit before semantic refinement"
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
                assert_ne!(refined.ticket, initial.ticket);
                assert!(!commit.await.expect("commit task should complete"));
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
                finish(reply_ticket).await;

                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert_eq!(initial.decision, OutgoingExecutiveDecision::Rewrite);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Prepared)
                );

                // Coalescing can admit another fragment before the semantic
                // Stop result for the batch is ready.
                let latest = ConversationCoordinator::begin_incoming(scope).await;
                let stopped = ConversationCoordinator::cancel_current_incoming(latest.ticket)
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

    #[test]
    fn production_ingress_keep_releases_the_existing_prepared_token() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_024);
                let reply_ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(reply_ticket).await);
                let outgoing = prepare_outgoing(
                    reply_ticket,
                    outgoing_fingerprint("the prepared answer already covers this"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply output should prepare");
                finish(reply_ticket).await;

                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert!(initial.frozen_prepared);
                assert_eq!(initial.ticket, reply_ticket);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Prepared)
                );
                let commit = kovi::tokio::spawn(async move { commit_outgoing(outgoing).await });
                kovi::tokio::task::yield_now().await;
                assert!(
                    !commit.is_finished(),
                    "frozen Prepared content must not cross commit before semantic refinement"
                );

                let refined = ConversationCoordinator::refine_current_incoming(
                    initial,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("the frozen admission should remain current");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(refined.preserved_prepared);
                assert!(commit.await.expect("commit task should complete"));
                mark_outgoing_failed(outgoing).await;
            });
    }

    #[test]
    fn frozen_admission_exposes_only_its_bounded_prepared_semantic_preview() {
        kovi::tokio::runtime::Runtime::new()
            .expect("test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_360_008);
                let previous = super::ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(previous).await);
                let body = "x".repeat(5_000);
                let prepared = prepare_outgoing_with_semantic_preview(
                    previous,
                    outgoing_fingerprint(&body),
                    OutgoingSource::Reply,
                    Some(&body),
                )
                .await
                .expect("reply should prepare");
                finish(previous).await;

                let admission = ConversationCoordinator::begin_incoming(scope).await;
                let preview = ConversationCoordinator::frozen_prepared_semantic_preview(admission)
                    .await
                    .expect("the exact frozen token should expose its semantic preview");
                assert_eq!(
                    preview
                        .chars()
                        .filter(|character| *character == 'x')
                        .count(),
                    4_096
                );
                assert!(preview.ends_with("[truncated]"));

                assert!(ConversationCoordinator::abandon_incoming(admission).await);
                assert!(!commit_outgoing(prepared).await);
            });
    }

    #[test]
    fn late_cleanup_cannot_release_a_new_reservation_on_the_same_ticket() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_025);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("still prepared across two observations"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply output should prepare");
                finish(ticket).await;

                let first = ConversationCoordinator::begin_incoming(scope).await;
                let first_refined = ConversationCoordinator::refine_current_incoming(
                    first,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("first admission should keep the token");
                assert!(first_refined.preserved_prepared);

                let second = ConversationCoordinator::begin_incoming(scope).await;
                assert_eq!(second.ticket, first.ticket);
                assert_ne!(second.reservation_id, first.reservation_id);
                assert!(!ConversationCoordinator::abandon_incoming(first).await);
                let second_refined = ConversationCoordinator::refine_current_incoming(
                    second,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::None,
                        direct_reply_expected: false,
                    },
                )
                .await
                .expect("late first cleanup must not consume the second reservation");
                assert!(second_refined.preserved_prepared);
                assert!(commit_outgoing(outgoing).await);
                mark_outgoing_failed(outgoing).await;
            });
    }

    #[test]
    fn stale_semantic_refinement_cannot_touch_a_newer_prepared_output() {
        kovi::tokio::runtime::Runtime::new()
            .expect("should create test runtime")
            .block_on(async {
                let scope = ReplyScope::Private(9_300_023);
                let ticket = ConversationCoordinator::interrupt(scope).await;
                assert!(mark_active(ticket).await);
                let old_outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("old prepared reply"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("old reply should prepare");
                finish(ticket).await;

                let stale = ConversationCoordinator::begin_incoming(scope).await;
                assert!(stale.frozen_prepared);
                mark_outgoing_failed(old_outgoing).await;

                assert!(mark_active(ticket).await);
                let newer_outgoing = prepare_outgoing(
                    ticket,
                    outgoing_fingerprint("replacement on the same ticket"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("replacement reply should prepare");
                finish(ticket).await;

                let refinement = ConversationCoordinator::refine_current_incoming(
                    stale,
                    OutgoingExecutiveContext {
                        incoming_impact: IncomingTurnImpact::InvalidatesPendingContent,
                        direct_reply_expected: true,
                    },
                )
                .await;

                assert!(refinement.is_none());
                assert_eq!(
                    test_outgoing_state(newer_outgoing).await,
                    Some(OutgoingState::Prepared)
                );
                assert!(commit_outgoing(newer_outgoing).await);
                mark_outgoing_failed(newer_outgoing).await;
            });
    }
}
