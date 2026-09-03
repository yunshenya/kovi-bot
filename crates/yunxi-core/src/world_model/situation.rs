//! Situation: a named, durable "what is happening" with a life cycle and
//! validated transitions (v4 §21–26, §180).
//!
//! A situation is not a fact: it is an estimate with confidence, participants,
//! related goals/open loops, and a deterministic transition table. Only
//! observations plus a validated transition may move it.

use super::observation::ObservationSource;
use super::{
    EntityId, WorldValidationError,
    common::{
        MAX_RELATED_IDS, MAX_WORLD_TEXT_BYTES, MAX_WORLD_TEXT_CHARS, clamp_unit, dedupe,
        validate_text, validate_unit,
    },
};
use crate::{ConversationId, GoalId, OpenLoopId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_SITUATION_PARTICIPANTS: usize = 12;
pub const MAX_SITUATION_DETAIL_BYTES: usize = MAX_WORLD_TEXT_BYTES;
pub const MAX_SITUATION_DETAIL_CHARS: usize = MAX_WORLD_TEXT_CHARS;
pub const MAX_ACTIVE_SITUATIONS_PER_SCOPE: usize = 8;

/// What kind of situation this is (v4 §180 — real business first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SituationKind {
    FutureEvent,
    ToolTask,
    AgentTask,
    BuildTask,
    ConversationState,
    Unknown,
}

/// Semantic state of a situation. This is a small deterministic lattice so
/// transitions can be validated in Rust (v4 §25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SituationState {
    Planned,
    InProgress,
    OutcomeUnknown,
    Completed,
    Failed,
    Expired,
    Unknown,
}

impl SituationState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Expired)
    }

    #[must_use]
    pub const fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown | Self::OutcomeUnknown)
    }
}

/// Lifecycle status, derived (not stored) from state + end time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SituationStatus {
    Active,
    Dormant,
    Resolved,
    Failed,
    Expired,
    Unknown,
}

/// Deterministic transition table (v4 §25). Everything else is rejected.
#[must_use]
pub fn can_transition(from: SituationState, to: SituationState) -> bool {
    use SituationState::*;
    if from == to {
        return false;
    }
    matches!(
        (from, to),
        (Planned, InProgress)
            | (Planned, OutcomeUnknown)
            | (Planned, Failed)
            | (Planned, Expired)
            | (InProgress, OutcomeUnknown)
            | (InProgress, Completed)
            | (InProgress, Failed)
            | (OutcomeUnknown, Completed)
            | (OutcomeUnknown, Failed)
            | (OutcomeUnknown, Expired)
            | (Unknown, Planned)
            | (Unknown, OutcomeUnknown)
    )
}

/// A named situation with life cycle (v4 §23).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Situation {
    id: super::SituationId,
    kind: SituationKind,
    state: SituationState,
    detail: Option<String>,
    participants: Vec<EntityId>,
    persons: Vec<PersonId>,
    conversation_id: Option<ConversationId>,
    related_goals: Vec<GoalId>,
    related_open_loops: Vec<OpenLoopId>,
    confidence: f32,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    version: u64,
}

impl Situation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: super::SituationId,
        kind: SituationKind,
        state: SituationState,
        detail: Option<String>,
        participants: Vec<EntityId>,
        persons: Vec<PersonId>,
        conversation_id: Option<ConversationId>,
        related_goals: Vec<GoalId>,
        related_open_loops: Vec<OpenLoopId>,
        confidence: f32,
        started_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let situation = Self {
            id,
            kind,
            state,
            detail: match detail {
                Some(detail) => Some(validate_text(detail, "situation detail")?),
                None => None,
            },
            participants: dedupe(participants, "situation participants", true)?,
            persons: dedupe(persons, "situation persons", true)?,
            conversation_id,
            related_goals: dedupe(related_goals, "situation goals", true)?,
            related_open_loops: dedupe(related_open_loops, "situation open loops", true)?,
            confidence: clamp_unit(confidence),
            started_at,
            updated_at: started_at,
            ended_at: None,
            version: 1,
        };
        situation.validate()?;
        Ok(situation)
    }

    /// Restore a persisted situation (adapter use): validates the whole
    /// record including terminal-state consistency.
    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        id: super::SituationId,
        kind: SituationKind,
        state: SituationState,
        detail: Option<String>,
        participants: Vec<EntityId>,
        persons: Vec<PersonId>,
        conversation_id: Option<ConversationId>,
        related_goals: Vec<GoalId>,
        related_open_loops: Vec<OpenLoopId>,
        confidence: f32,
        started_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        ended_at: Option<DateTime<Utc>>,
        version: u64,
    ) -> Result<Self, WorldValidationError> {
        let mut situation = Self::new(
            id,
            kind,
            state,
            detail,
            participants,
            persons,
            conversation_id,
            related_goals,
            related_open_loops,
            confidence,
            started_at,
        )?;
        situation.updated_at = updated_at;
        situation.ended_at = ended_at;
        situation.version = version;
        situation.validate()?;
        Ok(situation)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "situation confidence")?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if self.participants.len() > MAX_SITUATION_PARTICIPANTS {
            return Err(WorldValidationError::TooManyItems {
                field: "situation participants",
                length: self.participants.len(),
                maximum: MAX_SITUATION_PARTICIPANTS,
            });
        }
        if self.related_goals.len() > MAX_RELATED_IDS {
            return Err(WorldValidationError::TooManyItems {
                field: "situation goals",
                length: self.related_goals.len(),
                maximum: MAX_RELATED_IDS,
            });
        }
        if self.related_open_loops.len() > MAX_RELATED_IDS {
            return Err(WorldValidationError::TooManyItems {
                field: "situation open loops",
                length: self.related_open_loops.len(),
                maximum: MAX_RELATED_IDS,
            });
        }
        if let Some(detail) = &self.detail {
            validate_text(detail.clone(), "situation detail")?;
        }
        if let Some(ended_at) = self.ended_at
            && ended_at < self.started_at
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "situation ends before it starts",
            });
        }
        if self.updated_at < self.started_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "situation update predates its start",
            });
        }
        if self.state.is_terminal() && self.ended_at.is_none() {
            return Err(WorldValidationError::InvalidState {
                reason: "terminal situation has no end time",
            });
        }
        if !self.state.is_terminal() && self.ended_at.is_some() {
            return Err(WorldValidationError::InvalidState {
                reason: "non-terminal situation has an end time",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::SituationId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> SituationKind {
        self.kind
    }

    #[must_use]
    pub const fn state(&self) -> SituationState {
        self.state
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    #[must_use]
    pub fn participants(&self) -> &[EntityId] {
        &self.participants
    }

    #[must_use]
    pub fn persons(&self) -> &[PersonId] {
        &self.persons
    }

    #[must_use]
    pub const fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation_id
    }

    #[must_use]
    pub fn related_goals(&self) -> &[GoalId] {
        &self.related_goals
    }

    #[must_use]
    pub fn related_open_loops(&self) -> &[OpenLoopId] {
        &self.related_open_loops
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn ended_at(&self) -> Option<DateTime<Utc>> {
        self.ended_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Derived lifecycle status (v4 §24).
    #[must_use]
    pub fn status(&self) -> SituationStatus {
        match self.state {
            SituationState::Completed => SituationStatus::Resolved,
            SituationState::Failed => SituationStatus::Failed,
            SituationState::Expired => SituationStatus::Expired,
            SituationState::Unknown => SituationStatus::Unknown,
            // Active until it stops being updated: no stored dormancy yet.
            SituationState::Planned
            | SituationState::InProgress
            | SituationState::OutcomeUnknown => SituationStatus::Active,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.state.is_terminal() && self.ended_at.is_none()
    }

    #[must_use]
    pub fn involves_person(&self, person_id: PersonId) -> bool {
        self.persons.contains(&person_id)
    }

    /// Apply a validated transition (v4 §26).
    pub fn apply_transition(
        &mut self,
        proposal: &SituationTransitionProposal,
    ) -> Result<(), WorldValidationError> {
        proposal.validate()?;
        if self.id != proposal.situation_id() {
            return Err(WorldValidationError::InvalidState {
                reason: "transition targets a different situation",
            });
        }
        if self.state != proposal.expected_state() {
            return Err(WorldValidationError::InvalidState {
                reason: "situation state does not match the transition's expected state",
            });
        }
        if proposal.contradiction() {
            return Err(WorldValidationError::InvalidState {
                reason: "contradictory transition rejected",
            });
        }
        if !can_transition(self.state, proposal.target_state()) {
            return Err(WorldValidationError::InvalidTransition {
                from: self.state_label(),
                to: proposal.target_state_label(),
            });
        }
        let now = proposal.observed_at();
        self.state = proposal.target_state();
        self.updated_at = now;
        self.confidence = proposal.confidence();
        if self.state.is_terminal() && self.ended_at.is_none() {
            self.ended_at = Some(now);
        }
        self.version = self.version.saturating_add(1);
        self.validate()?;
        Ok(())
    }

    /// Expire in place (Planned/Unknown → Expired), e.g. after the event
    /// window ended; terminal states are untouched.
    pub fn expire(&mut self, now: DateTime<Utc>) -> Result<(), WorldValidationError> {
        if !matches!(self.state, SituationState::Planned | SituationState::Unknown) {
            return Err(WorldValidationError::InvalidTransition {
                from: self.state_label(),
                to: SituationState::Expired.serde_label(),
            });
        }
        if now < self.updated_at {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "situation expiry predates its last update",
            });
        }
        self.state = SituationState::Expired;
        self.ended_at = Some(now);
        self.updated_at = now;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    fn state_label(&self) -> &'static str {
        self.state.serde_label()
    }
}

impl SituationState {
    const fn serde_label(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::InProgress => "in_progress",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Unknown => "unknown",
        }
    }
}

/// A validated situation transition proposal (v4 §26).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SituationTransitionProposal {
    situation_id: super::SituationId,
    current_version: u64,
    expected_state: SituationState,
    target_state: SituationState,
    confidence: f32,
    source: ObservationSource,
    contradiction: bool,
    expected_until: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
}

impl SituationTransitionProposal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        situation_id: super::SituationId,
        current_version: u64,
        expected_state: SituationState,
        target_state: SituationState,
        confidence: f32,
        source: ObservationSource,
        contradiction: bool,
        expected_until: Option<DateTime<Utc>>,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, WorldValidationError> {
        let proposal = Self {
            situation_id,
            current_version,
            expected_state,
            target_state,
            confidence: clamp_unit(confidence),
            source,
            contradiction,
            expected_until,
            observed_at,
        };
        proposal.validate()?;
        Ok(proposal)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        validate_unit(self.confidence, "transition confidence")?;
        if self.current_version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if self.expected_state == self.target_state {
            return Err(WorldValidationError::InvalidTransition {
                from: self.expected_state.serde_label(),
                to: self.target_state.serde_label(),
            });
        }
        if !can_transition(self.expected_state, self.target_state) {
            return Err(WorldValidationError::InvalidTransition {
                from: self.expected_state.serde_label(),
                to: self.target_state.serde_label(),
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn situation_id(&self) -> super::SituationId {
        self.situation_id
    }

    #[must_use]
    pub const fn current_version(&self) -> u64 {
        self.current_version
    }

    #[must_use]
    pub const fn expected_state(&self) -> SituationState {
        self.expected_state
    }

    #[must_use]
    pub const fn target_state(&self) -> SituationState {
        self.target_state
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn source(&self) -> ObservationSource {
        self.source
    }

    #[must_use]
    pub const fn contradiction(&self) -> bool {
        self.contradiction
    }

    #[must_use]
    pub const fn expected_until(&self) -> Option<DateTime<Utc>> {
        self.expected_until
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    fn target_state_label(&self) -> &'static str {
        self.target_state.serde_label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GoalId, OpenLoopId};

    fn sample_situation(now: DateTime<Utc>) -> Situation {
        Situation::new(
            super::super::SituationId::new(),
            SituationKind::FutureEvent,
            SituationState::Planned,
            Some("面试".into()),
            vec![],
            vec![PersonId::new()],
            None,
            vec![GoalId::new()],
            vec![OpenLoopId::new()],
            0.6,
            now,
        )
        .expect("situation")
    }

    #[test]
    fn transition_table_is_deterministic() {
        assert!(can_transition(SituationState::Planned, SituationState::InProgress));
        assert!(can_transition(SituationState::InProgress, SituationState::OutcomeUnknown));
        assert!(can_transition(SituationState::OutcomeUnknown, SituationState::Completed));
        assert!(can_transition(SituationState::OutcomeUnknown, SituationState::Failed));
        assert!(can_transition(SituationState::Planned, SituationState::Expired));
        assert!(!can_transition(SituationState::Completed, SituationState::InProgress));
        assert!(!can_transition(SituationState::Failed, SituationState::Completed));
        assert!(!can_transition(SituationState::Planned, SituationState::Completed));
    }

    #[test]
    fn invalid_transition_is_rejected_at_proposal_boundary() {
        let now = Utc::now();
        let situation = sample_situation(now);
        // Planned → Completed is not in the table, so the proposal fails.
        assert!(SituationTransitionProposal::new(
            situation.id(),
            situation.version(),
            SituationState::Planned,
            SituationState::Completed,
            0.8,
            ObservationSource::DirectUserStatement,
            false,
            None,
            now,
        )
        .is_err());
        assert!(SituationTransitionProposal::new(
            situation.id(),
            situation.version(),
            SituationState::Planned,
            SituationState::Failed,
            0.8,
            ObservationSource::DirectUserStatement,
            false,
            None,
            now,
        )
        .is_ok());
    }

    #[test]
    fn applying_valid_transition_updates_state_and_version() {
        let now = Utc::now();
        let mut situation = sample_situation(now);
        let approved = SituationTransitionProposal::new(
            situation.id(),
            situation.version(),
            SituationState::Planned,
            SituationState::InProgress,
            0.9,
            ObservationSource::DirectUserStatement,
            false,
            None,
            now,
        )
        .expect("proposal");
        situation.apply_transition(&approved).expect("transition");
        assert_eq!(situation.state(), SituationState::InProgress);
        assert_eq!(situation.version(), 2);
        assert_eq!(situation.status(), SituationStatus::Active);

        // Complete the interview:
        let done = SituationTransitionProposal::new(
            situation.id(),
            situation.version(),
            SituationState::InProgress,
            SituationState::OutcomeUnknown,
            0.8,
            ObservationSource::DirectUserStatement,
            false,
            None,
            now,
        )
        .expect("proposal");
        situation.apply_transition(&done).expect("transition");
        assert_eq!(situation.status(), SituationStatus::Active);
        let passed = SituationTransitionProposal::new(
            situation.id(),
            situation.version(),
            SituationState::OutcomeUnknown,
            SituationState::Completed,
            0.95,
            ObservationSource::DirectUserStatement,
            false,
            None,
            now,
        )
        .expect("proposal");
        situation.apply_transition(&passed).expect("transition");
        assert_eq!(situation.status(), SituationStatus::Resolved);
        assert_eq!(situation.ended_at(), Some(now));
    }

    #[test]
    fn expiry_only_from_nested_or_unknown() {
        let now = Utc::now();
        let mut situation = sample_situation(now);
        situation.expire(now).expect("expire planned");
        assert_eq!(situation.state(), SituationState::Expired);
        assert_eq!(situation.ended_at(), Some(now));
        // Terminal states cannot expire again.
        assert!(situation.expire(now).is_err());
    }

    #[test]
    fn contradiction_in_transition_is_rejected() {
        let now = Utc::now();
        let mut situation = sample_situation(now);
        let contradicting = SituationTransitionProposal::new(
            situation.id(),
            situation.version(),
            SituationState::Planned,
            SituationState::InProgress,
            0.5,
            ObservationSource::DerivedObservation,
            true,
            None,
            now,
        )
        .expect("proposal");
        assert!(situation.apply_transition(&contradicting).is_err());
    }
}
