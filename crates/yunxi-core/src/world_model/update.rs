//! Update pipeline: proposal → validation → merge → store (v4 §84–88).
//!
//! Models never write the World Model directly. Every mutation flows through
//! a [`WorldUpdateProposal`] that Rust validates as a whole before applying.

use super::environment::EnvironmentUpdate;
use super::entity::EntityUpdateProposal;
use super::hypothesis::Hypothesis;
use super::observation::Observation;
use super::situation::{Situation, SituationTransitionProposal};
use super::social_scene::SocialSceneUpdate;
use super::{TimelineEntry, WorldUncertainty, WorldValidationError};
use crate::EventId;
use serde::{Deserialize, Serialize};

pub const MAX_UPDATES_PER_BATCH: usize = 4;
pub const MAX_REASON_TAGS_PER_BATCH: usize = 8;

/// Structured reason tags for explainability (v4 §153).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorldReasonTag {
    StateStale,
    StateUnknown,
    HypothesisLowConfidence,
    SituationTransition,
    SocialInterruptHigh,
    HostUnavailable,
    ToolDegraded,
    PredictionUncertain,
    CausalRuleMatch,
    SimulationSkipped,
    SimulationUsed,
    WorldVersionStale,
}

/// One validated mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorldUpdate {
    Observation(Observation),
    Entity(EntityUpdateProposal),
    SituationTransition(SituationTransitionProposal),
    SituationNew(Situation),
    Hypothesis(Hypothesis),
    SocialScene(SocialSceneUpdate),
    Environment(EnvironmentUpdate),
    Uncertainty(WorldUncertainty),
    Timeline(TimelineEntry),
}

impl WorldUpdate {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        match self {
            Self::Observation(observation) => observation.validate(),
            Self::Entity(proposal) => proposal.validate(),
            Self::SituationTransition(proposal) => proposal.validate(),
            Self::SituationNew(situation) => situation.validate(),
            Self::Hypothesis(hypothesis) => hypothesis.validate(),
            Self::SocialScene(update) => update.validate(),
            Self::Environment(update) => update.validate(),
            Self::Uncertainty(uncertainty) => uncertainty.validate(),
            Self::Timeline(entry) => entry.validate(),
        }
    }
}

/// A bounded, validated batch of world mutations from one source event
/// (v4 §84 pipeline). Empty or oversized batches are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldUpdateProposal {
    source_event_id: EventId,
    reason_tags: Vec<WorldReasonTag>,
    updates: Vec<WorldUpdate>,
}

impl WorldUpdateProposal {
    pub fn new(
        source_event_id: EventId,
        reason_tags: Vec<WorldReasonTag>,
        updates: Vec<WorldUpdate>,
    ) -> Result<Self, WorldValidationError> {
        let proposal = Self {
            source_event_id,
            reason_tags,
            updates,
        };
        proposal.validate()?;
        Ok(proposal)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.updates.is_empty() {
            return Err(WorldValidationError::InvalidState {
                reason: "world update batch is empty",
            });
        }
        if self.updates.len() > MAX_UPDATES_PER_BATCH {
            return Err(WorldValidationError::TooManyItems {
                field: "updates per batch",
                length: self.updates.len(),
                maximum: MAX_UPDATES_PER_BATCH,
            });
        }
        if self.reason_tags.len() > MAX_REASON_TAGS_PER_BATCH {
            return Err(WorldValidationError::TooManyItems {
                field: "reason tags per batch",
                length: self.reason_tags.len(),
                maximum: MAX_REASON_TAGS_PER_BATCH,
            });
        }
        let mut seen = Vec::new();
        for tag in &self.reason_tags {
            if seen.contains(tag) {
                return Err(WorldValidationError::DuplicateItem {
                    field: "reason tag",
                });
            }
            seen.push(*tag);
        }
        for update in &self.updates {
            update.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn source_event_id(&self) -> EventId {
        self.source_event_id
    }

    #[must_use]
    pub fn reason_tags(&self) -> &[WorldReasonTag] {
        &self.reason_tags
    }

    #[must_use]
    pub fn updates(&self) -> &[WorldUpdate] {
        &self.updates
    }
}

/// Result of applying a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldUpdateState {
    applied: usize,
    previous_version: u64,
    new_version: u64,
}

impl WorldUpdateState {
    pub fn new(applied: usize, previous_version: u64, new_version: u64) -> Self {
        Self {
            applied,
            previous_version,
            new_version,
        }
    }

    #[must_use]
    pub const fn applied(&self) -> usize {
        self.applied
    }

    #[must_use]
    pub const fn previous_version(&self) -> u64 {
        self.previous_version
    }

    #[must_use]
    pub const fn new_version(&self) -> u64 {
        self.new_version
    }
}
