//! Deterministic revalidation of an outgoing envelope before it is committed.
//!
//! The host still owns the concrete reply ticket and delivery operation. This
//! module only describes the bounded decision that can be made while an
//! outgoing value is mutable; it never sends a message or invokes a model.

use super::ConflictId;
use crate::{ConversationId, EventId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_OUTGOING_REWRITES: u8 = 2;
pub const MAX_OUTGOING_REASON_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingSource {
    Reply,
    Proactive,
    Reminder,
    TaskDelivery,
    System,
}

impl OutgoingSource {
    #[must_use]
    pub const fn is_must_execute(self) -> bool {
        matches!(self, Self::Reminder | Self::TaskDelivery | Self::System)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOutgoing {
    pub event_id: EventId,
    pub conversation_id: Option<ConversationId>,
    pub recipient_id: Option<PersonId>,
    pub source: OutgoingSource,
    pub generation: u64,
    pub committed: bool,
    #[serde(default)]
    pub rewrite_count: u8,
    #[serde(default)]
    pub merge_count: u8,
    #[serde(default = "default_max_rewrites")]
    pub max_rewrites: u8,
}

impl PendingOutgoing {
    #[must_use]
    pub fn new(event_id: EventId, source: OutgoingSource) -> Self {
        Self {
            event_id,
            conversation_id: None,
            recipient_id: None,
            source,
            generation: 1,
            committed: false,
            rewrite_count: 0,
            merge_count: 0,
            max_rewrites: MAX_OUTGOING_REWRITES,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.generation == 0 {
            return Err("pending outgoing generation must be non-zero");
        }
        if self.rewrite_count > self.max_rewrites || self.max_rewrites > MAX_OUTGOING_REWRITES {
            return Err("pending outgoing rewrite count is out of bounds");
        }
        Ok(())
    }

    #[must_use]
    pub const fn must_execute(&self) -> bool {
        self.source.is_must_execute()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncomingOutgoingChange {
    None,
    ExtendsPendingTopic,
    InvalidatesPendingContent,
    UserAlreadyAnswered,
    Unrelated,
    StopRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingRevalidationContext {
    pub pending: PendingOutgoing,
    pub incoming_event_id: Option<EventId>,
    pub incoming_change: IncomingOutgoingChange,
    pub direct_reply_expected: bool,
    pub conversation_changed: bool,
    pub semantic_ambiguity: bool,
    pub target_valid: bool,
    pub permission_valid: bool,
    pub capability_valid: bool,
    pub exact_duplicate: bool,
    pub open_loop_resolved: bool,
    #[serde(default)]
    pub related_conflicts: Vec<ConflictId>,
}

impl OutgoingRevalidationContext {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.pending.validate()?;
        if self.related_conflicts.len() > 8 {
            return Err("outgoing revalidation has too many conflicts");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteRequest {
    pub reason: String,
    pub base_generation: u64,
}

impl RewriteRequest {
    fn new(reason: &str, base_generation: u64) -> Self {
        Self {
            reason: bounded_reason(reason),
            base_generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub reason: String,
    pub base_generation: u64,
}

impl MergeRequest {
    fn new(reason: &str, base_generation: u64) -> Self {
        Self {
            reason: bounded_reason(reason),
            base_generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferUntil {
    pub until: Option<DateTime<Utc>>,
    pub reason: String,
}

impl DeferUntil {
    fn new(until: Option<DateTime<Utc>>, reason: &str) -> Self {
        Self {
            until,
            reason: bounded_reason(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutgoingRevalidation {
    CommitAsIs,
    Cancel,
    Supersede,
    Rewrite(RewriteRequest),
    Merge(MergeRequest),
    Defer(DeferUntil),
}

impl OutgoingRevalidation {
    #[must_use]
    pub const fn is_sendable_without_rewrite(&self) -> bool {
        matches!(self, Self::CommitAsIs)
    }
}

/// Fast-path policy for the existing host-side outgoing fence. Ambiguous
/// semantic cases are represented as a bounded rewrite/merge request; the
/// caller may then choose whether an optional evaluator is worth invoking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutgoingRevalidator {
    pub max_rewrites: u8,
}

impl Default for OutgoingRevalidator {
    fn default() -> Self {
        Self {
            max_rewrites: MAX_OUTGOING_REWRITES,
        }
    }
}

impl OutgoingRevalidator {
    #[must_use]
    pub const fn new(max_rewrites: u8) -> Self {
        Self {
            max_rewrites: if max_rewrites > MAX_OUTGOING_REWRITES {
                MAX_OUTGOING_REWRITES
            } else {
                max_rewrites
            },
        }
    }

    pub fn evaluate(
        &self,
        context: &OutgoingRevalidationContext,
    ) -> Result<OutgoingRevalidation, &'static str> {
        context.validate()?;
        let pending = &context.pending;

        // A committed envelope and must-execute envelope cannot be silently
        // withdrawn by a soft semantic decision. Validity failures are still
        // terminal because sending an invalid target is never safe.
        if !context.target_valid || !context.permission_valid || !context.capability_valid {
            return Ok(OutgoingRevalidation::Cancel);
        }
        if pending.committed {
            return Ok(OutgoingRevalidation::CommitAsIs);
        }
        if context.exact_duplicate || context.open_loop_resolved {
            return Ok(if pending.must_execute() {
                OutgoingRevalidation::Defer(DeferUntil::new(
                    None,
                    "duplicate or already-resolved work requires host recovery",
                ))
            } else {
                OutgoingRevalidation::Cancel
            });
        }
        if matches!(
            context.incoming_change,
            IncomingOutgoingChange::StopRequested
        ) {
            return Ok(OutgoingRevalidation::Cancel);
        }

        let max_rewrites = self.max_rewrites.min(pending.max_rewrites);
        if pending.rewrite_count >= max_rewrites
            && matches!(
                context.incoming_change,
                IncomingOutgoingChange::ExtendsPendingTopic
                    | IncomingOutgoingChange::InvalidatesPendingContent
                    | IncomingOutgoingChange::UserAlreadyAnswered
            )
        {
            return Ok(if pending.must_execute() {
                OutgoingRevalidation::Defer(DeferUntil::new(
                    None,
                    "outgoing rewrite budget exhausted",
                ))
            } else {
                OutgoingRevalidation::Supersede
            });
        }

        // A direct user request takes precedence over unrelated proactive
        // content, while a committed/task envelope remains host-owned.
        if context.direct_reply_expected
            && matches!(pending.source, OutgoingSource::Proactive)
            && matches!(
                context.incoming_change,
                IncomingOutgoingChange::Unrelated
                    | IncomingOutgoingChange::InvalidatesPendingContent
                    | IncomingOutgoingChange::UserAlreadyAnswered
            )
        {
            return Ok(OutgoingRevalidation::Defer(DeferUntil::new(
                None,
                "direct reply preempts proactive content",
            )));
        }

        let decision = match context.incoming_change {
            IncomingOutgoingChange::None => OutgoingRevalidation::CommitAsIs,
            IncomingOutgoingChange::StopRequested => OutgoingRevalidation::Cancel,
            IncomingOutgoingChange::UserAlreadyAnswered => {
                OutgoingRevalidation::Rewrite(RewriteRequest::new(
                    "the user already supplied the expected answer",
                    pending.generation,
                ))
            }
            IncomingOutgoingChange::InvalidatesPendingContent => {
                OutgoingRevalidation::Rewrite(RewriteRequest::new(
                    "new content invalidates the pending semantic context",
                    pending.generation,
                ))
            }
            IncomingOutgoingChange::ExtendsPendingTopic => OutgoingRevalidation::Merge(
                MergeRequest::new("new content extends the pending topic", pending.generation),
            ),
            IncomingOutgoingChange::Unrelated if context.direct_reply_expected => {
                OutgoingRevalidation::Rewrite(RewriteRequest::new(
                    "new direct request is an unrelated topic",
                    pending.generation,
                ))
            }
            IncomingOutgoingChange::Unrelated => OutgoingRevalidation::CommitAsIs,
        };

        if context.conversation_changed
            && context.semantic_ambiguity
            && matches!(decision, OutgoingRevalidation::CommitAsIs)
        {
            return Ok(OutgoingRevalidation::Defer(DeferUntil::new(
                None,
                "conversation changed in an ambiguous way",
            )));
        }
        Ok(decision)
    }
}

fn default_max_rewrites() -> u8 {
    MAX_OUTGOING_REWRITES
}

fn bounded_reason(reason: &str) -> String {
    let mut output = reason
        .chars()
        .take(MAX_OUTGOING_REASON_BYTES)
        .collect::<String>();
    if output.is_empty() {
        output.push_str("unspecified");
    }
    output
}
