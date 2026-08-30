//! Persistent, platform-neutral conversation lifecycle state.
//!
//! This is deliberately separate from message transport. A host reports
//! normalized inbound/outbound events and asks the lifecycle whether an idle
//! autonomous turn is due. QQ, WeChat, and non-chat hosts can therefore share
//! the same continuation semantics.

use crate::{ConversationId, ConversationKind, ConversationTurnDirective, PersonId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use thiserror::Error;

pub const MAX_LIFECYCLE_PARTICIPANTS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPhase {
    New,
    Active,
    Waiting,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutonomyPolicy {
    pub direct_idle: Duration,
    pub group_idle: Duration,
    pub direct_cooldown: Duration,
    pub group_cooldown: Duration,
}

impl Default for AutonomyPolicy {
    fn default() -> Self {
        Self {
            direct_idle: Duration::seconds(90),
            group_idle: Duration::seconds(180),
            direct_cooldown: Duration::seconds(15),
            group_cooldown: Duration::seconds(30),
        }
    }
}

impl AutonomyPolicy {
    pub fn validate(self) -> Result<Self, ConversationLifecycleError> {
        if [
            self.direct_idle,
            self.group_idle,
            self.direct_cooldown,
            self.group_cooldown,
        ]
        .iter()
        .any(|duration| *duration <= Duration::zero())
        {
            return Err(ConversationLifecycleError::InvalidPolicy);
        }
        Ok(self)
    }

    fn idle_for(self, kind: ConversationKind) -> Option<Duration> {
        match kind {
            ConversationKind::Direct => Some(self.direct_idle),
            ConversationKind::Group => Some(self.group_idle),
            ConversationKind::System => None,
        }
    }

    fn cooldown_for(self, kind: ConversationKind) -> Option<Duration> {
        match kind {
            ConversationKind::Direct => Some(self.direct_cooldown),
            ConversationKind::Group => Some(self.group_cooldown),
            ConversationKind::System => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConversationLifecycleError {
    #[error("conversation kind conflicts with existing lifecycle state")]
    ConversationKindMismatch,
    #[error("autonomy policy durations must be positive")]
    InvalidPolicy,
    #[error("conversation lifecycle version counter is exhausted")]
    VersionExhausted,
}

/// Serializable conversation state shared by every host adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationLifecycle {
    conversation_id: ConversationId,
    kind: ConversationKind,
    phase: ConversationPhase,
    current_topic: Option<String>,
    active_people: VecDeque<PersonId>,
    last_inbound_at: Option<DateTime<Utc>>,
    last_outbound_at: Option<DateTime<Utc>>,
    last_autonomous_at: Option<DateTime<Utc>>,
    directive: ConversationTurnDirective,
    continuation_decided: bool,
    next_wake_at: Option<DateTime<Utc>>,
    in_flight: bool,
    autonomous_turns: u64,
    version: u64,
}

impl ConversationLifecycle {
    pub fn new(
        conversation_id: ConversationId,
        kind: ConversationKind,
    ) -> Result<Self, ConversationLifecycleError> {
        Self {
            conversation_id,
            kind,
            phase: ConversationPhase::New,
            current_topic: None,
            active_people: VecDeque::new(),
            last_inbound_at: None,
            last_outbound_at: None,
            last_autonomous_at: None,
            directive: ConversationTurnDirective::Wait,
            continuation_decided: false,
            next_wake_at: None,
            in_flight: false,
            autonomous_turns: 0,
            version: 0,
        }
        .with_validated_policy(AutonomyPolicy::default())
    }

    fn with_validated_policy(
        self,
        policy: AutonomyPolicy,
    ) -> Result<Self, ConversationLifecycleError> {
        policy.validate().map(|_| self)
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn kind(&self) -> ConversationKind {
        self.kind
    }

    #[must_use]
    pub const fn phase(&self) -> ConversationPhase {
        self.phase
    }

    #[must_use]
    pub fn current_topic(&self) -> Option<&str> {
        self.current_topic.as_deref()
    }

    #[must_use]
    pub fn active_people(&self) -> impl ExactSizeIterator<Item = PersonId> + '_ {
        self.active_people.iter().copied()
    }

    #[must_use]
    pub const fn last_inbound_at(&self) -> Option<DateTime<Utc>> {
        self.last_inbound_at
    }

    #[must_use]
    pub const fn last_outbound_at(&self) -> Option<DateTime<Utc>> {
        self.last_outbound_at
    }

    #[must_use]
    pub const fn last_autonomous_at(&self) -> Option<DateTime<Utc>> {
        self.last_autonomous_at
    }

    #[must_use]
    pub const fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    #[must_use]
    pub const fn directive(&self) -> ConversationTurnDirective {
        self.directive
    }

    #[must_use]
    pub const fn next_wake_at(&self) -> Option<DateTime<Utc>> {
        self.next_wake_at
    }

    #[must_use]
    pub const fn autonomous_turns(&self) -> u64 {
        self.autonomous_turns
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    pub fn set_topic(
        &mut self,
        topic: impl Into<String>,
    ) -> Result<(), ConversationLifecycleError> {
        self.current_topic = Some(topic.into());
        self.bump_version()
    }

    /// Record a participant-aware inbound turn. Any new inbound activity
    /// supersedes a pending autonomous continuation and reopens the session.
    pub fn observe_inbound(
        &mut self,
        kind: ConversationKind,
        person_id: PersonId,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), ConversationLifecycleError> {
        if self.kind != kind {
            return Err(ConversationLifecycleError::ConversationKindMismatch);
        }
        push_person(&mut self.active_people, person_id);
        self.last_inbound_at = Some(
            self.last_inbound_at
                .map_or(occurred_at, |current| current.max(occurred_at)),
        );
        self.phase = ConversationPhase::Active;
        self.directive = ConversationTurnDirective::Wait;
        self.continuation_decided = false;
        self.next_wake_at = None;
        self.in_flight = false;
        self.bump_version()
    }

    /// Variant for hosts that only know the conversation-level activity at
    /// the autonomy boundary. Participant identity can be supplied later by
    /// the normalized message event without changing lifecycle semantics.
    pub fn observe_inbound_activity(
        &mut self,
        kind: ConversationKind,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), ConversationLifecycleError> {
        if self.kind != kind {
            return Err(ConversationLifecycleError::ConversationKindMismatch);
        }
        self.last_inbound_at = Some(
            self.last_inbound_at
                .map_or(occurred_at, |current| current.max(occurred_at)),
        );
        self.phase = ConversationPhase::Active;
        self.directive = ConversationTurnDirective::Wait;
        self.continuation_decided = false;
        self.next_wake_at = None;
        self.in_flight = false;
        self.bump_version()
    }

    /// Ambient group traffic updates activity while leaving the session
    /// dormant. It cancels a stale continuation without creating one.
    pub fn observe_ambient_group(
        &mut self,
        occurred_at: DateTime<Utc>,
    ) -> Result<(), ConversationLifecycleError> {
        if self.kind != ConversationKind::Group {
            return Ok(());
        }
        if self.last_inbound_at.is_some_and(|last| occurred_at <= last) {
            return Ok(());
        }
        self.last_inbound_at = Some(occurred_at);
        self.phase = ConversationPhase::Active;
        self.directive = ConversationTurnDirective::Wait;
        self.continuation_decided = false;
        self.next_wake_at = None;
        self.in_flight = false;
        self.bump_version()
    }

    /// Record a delivered or prepared model turn and its semantic lifecycle
    /// decision. No message-count budget is consulted here.
    pub fn record_outbound(
        &mut self,
        occurred_at: DateTime<Utc>,
        directive: Option<ConversationTurnDirective>,
        policy: AutonomyPolicy,
    ) -> Result<(), ConversationLifecycleError> {
        policy.validate()?;
        self.last_outbound_at = Some(
            self.last_outbound_at
                .map_or(occurred_at, |current| current.max(occurred_at)),
        );
        self.in_flight = false;
        if let Some(directive) = directive {
            self.apply_directive(occurred_at, directive, policy)?;
        } else {
            self.phase = ConversationPhase::Waiting;
            self.continuation_decided = false;
            self.next_wake_at = None;
            self.bump_version()?;
        }
        Ok(())
    }

    /// Atomically claim an eligible autonomous turn. A caller must release or
    /// finish the claim even when the host cannot enqueue the event.
    pub fn claim_autonomous(
        &mut self,
        now: DateTime<Utc>,
        policy: AutonomyPolicy,
    ) -> Result<bool, ConversationLifecycleError> {
        let due = self.autonomous_due(now, policy)?;
        if due {
            self.in_flight = true;
            self.bump_version()?;
        }
        Ok(due)
    }

    /// Check whether an autonomous turn is eligible without mutating state.
    /// Hosts use this to choose a candidate; `claim_autonomous` performs the
    /// atomic state transition after the candidate has been selected.
    pub fn autonomous_due(
        &self,
        now: DateTime<Utc>,
        policy: AutonomyPolicy,
    ) -> Result<bool, ConversationLifecycleError> {
        policy.validate()?;
        if self.in_flight || self.kind == ConversationKind::System {
            return Ok(false);
        }
        let Some(last_inbound) = self.last_inbound_at else {
            return Ok(false);
        };
        let Some(last_outbound) = self.last_outbound_at else {
            return Ok(false);
        };
        if last_outbound < last_inbound {
            return Ok(false);
        }
        Ok(if self.continuation_decided {
            self.directive == ConversationTurnDirective::Continue
                && self.next_wake_at.is_some_and(|wake| now >= wake)
        } else {
            now.signed_duration_since(last_inbound) >= policy.idle_for(self.kind).unwrap()
        })
    }

    pub fn release_autonomous_claim(&mut self) -> Result<(), ConversationLifecycleError> {
        if self.in_flight {
            self.in_flight = false;
            self.bump_version()?;
        }
        Ok(())
    }

    pub fn finish_autonomous_claim(
        &mut self,
        occurred_at: DateTime<Utc>,
        delivered: bool,
        directive: ConversationTurnDirective,
        policy: AutonomyPolicy,
    ) -> Result<(), ConversationLifecycleError> {
        policy.validate()?;
        if !self.in_flight {
            return Ok(());
        }
        self.in_flight = false;
        self.autonomous_turns = self.autonomous_turns.saturating_add(1);
        self.last_autonomous_at = Some(
            self.last_autonomous_at
                .map_or(occurred_at, |current| current.max(occurred_at)),
        );
        if delivered {
            self.last_outbound_at = Some(
                self.last_outbound_at
                    .map_or(occurred_at, |current| current.max(occurred_at)),
            );
            self.apply_directive(occurred_at, directive, policy)?;
        } else {
            self.apply_directive(occurred_at, ConversationTurnDirective::Wait, policy)?;
        }
        Ok(())
    }

    fn apply_directive(
        &mut self,
        occurred_at: DateTime<Utc>,
        directive: ConversationTurnDirective,
        policy: AutonomyPolicy,
    ) -> Result<(), ConversationLifecycleError> {
        self.directive = directive;
        self.continuation_decided = true;
        self.phase = match directive {
            ConversationTurnDirective::Continue | ConversationTurnDirective::Wait => {
                ConversationPhase::Waiting
            }
            ConversationTurnDirective::End => ConversationPhase::Ended,
        };
        self.next_wake_at = match directive {
            ConversationTurnDirective::Continue => {
                Some(occurred_at + policy.cooldown_for(self.kind).unwrap())
            }
            ConversationTurnDirective::Wait | ConversationTurnDirective::End => None,
        };
        self.bump_version()
    }

    fn bump_version(&mut self) -> Result<(), ConversationLifecycleError> {
        self.version = self
            .version
            .checked_add(1)
            .ok_or(ConversationLifecycleError::VersionExhausted)?;
        Ok(())
    }
}

fn push_person(people: &mut VecDeque<PersonId>, person_id: PersonId) {
    people.retain(|candidate| *candidate != person_id);
    people.push_back(person_id);
    while people.len() > MAX_LIFECYCLE_PARTICIPANTS {
        people.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_is_semantic_and_has_no_turn_budget() {
        let id = ConversationId::new();
        let person = PersonId::new();
        let mut lifecycle = ConversationLifecycle::new(id, ConversationKind::Direct).unwrap();
        let policy = AutonomyPolicy {
            direct_idle: Duration::seconds(1),
            direct_cooldown: Duration::seconds(1),
            ..AutonomyPolicy::default()
        };
        let start = Utc::now();
        lifecycle
            .observe_inbound(ConversationKind::Direct, person, start)
            .unwrap();
        lifecycle
            .record_outbound(start, Some(ConversationTurnDirective::Continue), policy)
            .unwrap();
        for index in 0..100u64 {
            let at = start + Duration::seconds(2 + (index as i64) * 2);
            assert!(lifecycle.claim_autonomous(at, policy).unwrap());
            lifecycle
                .finish_autonomous_claim(at, true, ConversationTurnDirective::Continue, policy)
                .unwrap();
        }
        assert_eq!(lifecycle.autonomous_turns(), 100);
        assert_eq!(lifecycle.directive(), ConversationTurnDirective::Continue);
        assert_eq!(
            lifecycle.last_autonomous_at(),
            Some(start + Duration::seconds(200))
        );
        assert!(!lifecycle.is_in_flight());
    }

    #[test]
    fn group_activity_cancels_pending_claim_and_preserves_members() {
        let id = ConversationId::new();
        let first = PersonId::new();
        let second = PersonId::new();
        let mut lifecycle = ConversationLifecycle::new(id, ConversationKind::Group).unwrap();
        let policy = AutonomyPolicy {
            group_idle: Duration::seconds(1),
            group_cooldown: Duration::seconds(1),
            ..AutonomyPolicy::default()
        };
        let start = Utc::now();
        lifecycle
            .observe_inbound(ConversationKind::Group, first, start)
            .unwrap();
        lifecycle
            .record_outbound(start, Some(ConversationTurnDirective::Continue), policy)
            .unwrap();
        assert!(
            lifecycle
                .claim_autonomous(start + Duration::seconds(2), policy)
                .unwrap()
        );
        lifecycle
            .observe_inbound(
                ConversationKind::Group,
                second,
                start + Duration::seconds(3),
            )
            .unwrap();
        assert!(!lifecycle.release_autonomous_claim().is_err());
        assert_eq!(
            lifecycle.active_people().collect::<Vec<_>>(),
            vec![first, second]
        );
        assert!(
            !lifecycle
                .claim_autonomous(start + Duration::seconds(5), policy)
                .unwrap()
        );
    }

    #[test]
    fn lifecycle_round_trips_without_platform_fields() {
        let lifecycle =
            ConversationLifecycle::new(ConversationId::new(), ConversationKind::Direct).unwrap();
        let encoded = serde_json::to_value(&lifecycle).unwrap();
        assert!(encoded.get("qq").is_none());
        let decoded: ConversationLifecycle = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.kind(), ConversationKind::Direct);
    }
}
