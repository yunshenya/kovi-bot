//! Platform-neutral external World Model v4 domain state.
//!
//! The World Model answers "what might the external world look like right
//! now". It is deliberately separate from Mind (internal state), Executive
//! (control), and Memory (what happened in the past). This module is
//! platform-neutral: it never depends on QQ, Kovi, OneBot, SQLx, or any GUI.
//!
//! Every type here is:
//! - bounded (text, counts, confidence), see [`limits`];
//! - validated at construction (no invalid state can be deserialized);
//! - confidence-aware (0..=1, never silently upgraded to fact);
//! - freshness-aware (TTL / expiry / stale / unknown are first-class).
//!
//! Models never write state directly. They emit proposals ([`update`] module)
//! which Rust validates and merges through [`WorldModel::apply`].

macro_rules! world_id {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(uuid::Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            #[must_use]
            pub const fn from_uuid(value: uuid::Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> uuid::Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<uuid::Uuid> for $name {
            fn from(value: uuid::Uuid) -> Self {
                Self::from_uuid(value)
            }
        }

        impl From<$name> for uuid::Uuid {
            fn from(value: $name) -> Self {
                value.into_uuid()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                uuid::Uuid::parse_str(value).map(Self::from_uuid)
            }
        }
    };
}

mod common;
mod entity;
mod environment;
mod hypothesis;
mod ids;
mod observation;
mod situation;
mod snapshot;
mod social_scene;
mod temporal;
mod update;

pub use common::{
    MAX_EVIDENCE_REFS, MAX_RELATED_IDS, MAX_WORLD_TEXT_BYTES, MAX_WORLD_TEXT_CHARS,
    MAX_WORLD_VALUE_BYTES, MAX_WORLD_VALUE_CHARS, WorldValidationError,
};
pub use entity::{
    EntityKind, EntityState, EntityStateIndex, EntityUpdate, EntityUpdateAction,
    EntityUpdateProposal, MAX_ACTIVE_ENTITIES, MAX_ENTITIES_PER_SCOPE,
    MAX_PROPERTIES_PER_ENTITY, StateProperty,
};
pub use environment::{
    EnvironmentState, EnvironmentUpdate, HostId, HostState, RuntimeLoad, ServiceHealth,
    ToolHealth, MAX_ENVIRONMENT_HOSTS, MAX_ENVIRONMENT_TOOLS, MAX_HOST_ID_BYTES,
    MAX_HOST_ID_CHARS, MAX_TOOL_NAME_BYTES, MAX_TOOL_NAME_CHARS,
};
pub use hypothesis::{
    Hypothesis, HypothesisStatus, MAX_ACTIVE_HYPOTHESES_PER_CONVERSATION,
    MAX_ACTIVE_HYPOTHESES_PER_PERSON, MAX_HYPOTHESIS_TEXT_BYTES, MAX_HYPOTHESIS_TEXT_CHARS,
    MIN_HYPOTHESIS_CREATE_CONFIDENCE, WorldProposition, normalized_proposition,
};
pub use ids::{
    CausalRelationId, EntityId, HypothesisId, ObservationId, PredictionId, SituationId,
    UncertaintyId,
};
pub use observation::{
    Observation, ObservationDraft, ObservationKind, ObservationPayload, ObservationSource,
    ObservationSourceReliability, MAX_OBSERVATIONS_PER_EVENT,
    MAX_OBSERVATION_PAYLOAD_BYTES, MAX_OBSERVATION_PAYLOAD_CHARS, observation_fingerprint,
};
pub use situation::{
    Situation, SituationKind, SituationState, SituationStatus, SituationTransitionProposal,
    MAX_ACTIVE_SITUATIONS_PER_SCOPE, MAX_SITUATION_DETAIL_BYTES, MAX_SITUATION_DETAIL_CHARS,
    MAX_SITUATION_PARTICIPANTS, can_transition,
};
pub use social_scene::{
    MAX_SCENE_ACTIVITY_PARTICIPANTS, MAX_SCENE_CURRENT_FLOOR, MAX_SCENE_RECENT_SPEAKERS,
    MAX_SCENES_PER_WORLD, SocialSceneKind, SocialSceneState, SocialSceneUpdate,
    floor_interruption_cost,
};
pub use temporal::{
    Freshness, TemporalRelation, TimeInterval, TimelineEntry, TimelineState, WorldRef,
    freshness_at, relation_between,
};
pub use update::{
    MAX_REASON_TAGS_PER_BATCH, MAX_UPDATES_PER_BATCH, WorldReasonTag, WorldUpdate,
    WorldUpdateProposal, WorldUpdateState,
};
pub use snapshot::{
    EntityStateSnapshot, EnvironmentSnapshot, HypothesisSnapshot, SituationSnapshot,
    SocialSceneSnapshot, TemporalSnapshotEntry, WorldModelSnapshot, WorldSnapshotContext,
    WorldSnapshotLimits, WorldUncertaintySnapshot,
};

use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How strongly a World Model capability may influence agent behavior.
///
/// Every high-risk capability (transition, prediction, simulation, stale
/// marking) starts in [`WorldInfluenceMode::Shadow`] and only moves to
/// [`WorldInfluenceMode::Active`] after calibration evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorldInfluenceMode {
    #[default]
    Disabled,
    Shadow,
    Active,
}

/// The scope an observation/state/hypothesis belongs to.
///
/// The World Model never invents a new identity system: it reuses
/// [`PersonId`] / [`ConversationId`] from Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorldScope {
    Global,
    Person { person_id: PersonId },
    Conversation { conversation_id: ConversationId },
}

impl WorldScope {
    #[must_use]
    pub const fn person_id(self) -> Option<PersonId> {
        match self {
            Self::Person { person_id } => Some(person_id),
            Self::Global | Self::Conversation { .. } => None,
        }
    }

    #[must_use]
    pub const fn conversation_id(self) -> Option<ConversationId> {
        match self {
            Self::Conversation { conversation_id } => Some(conversation_id),
            Self::Global | Self::Person { .. } => None,
        }
    }
}

/// Kinds of "we do not know" the World Model can name explicitly.
///
/// Unknown is a legal, first-class state. The model must never be forced to
/// fill the gap with a low-quality hypothesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyType {
    StateUnknown,
    TemporalUnknown,
    SourceConflict,
    StaleState,
    InsufficientEvidence,
    PredictionUncertain,
}

/// A named, bounded record of one specific uncertainty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldUncertainty {
    id: UncertaintyId,
    uncertainty_type: UncertaintyType,
    scope: WorldScope,
    note: String,
    observed_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    version: u64,
}

impl WorldUncertainty {
    pub fn new(
        id: UncertaintyId,
        uncertainty_type: UncertaintyType,
        scope: WorldScope,
        note: impl Into<String>,
        observed_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Self, WorldValidationError> {
        let note = common::validate_text(note, "uncertainty note")?;
        if let Some(expires_at) = expires_at
            && expires_at < observed_at
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "uncertainty expires before it was observed",
            });
        }
        Ok(Self {
            id,
            uncertainty_type,
            scope,
            note,
            observed_at,
            expires_at,
            version: 1,
        })
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        common::validate_text(self.note.clone(), "uncertainty note")?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        if let Some(expires_at) = self.expires_at
            && expires_at < self.observed_at
        {
            return Err(WorldValidationError::InvalidTimestamp {
                reason: "uncertainty expires before it was observed",
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> UncertaintyId {
        self.id
    }

    #[must_use]
    pub const fn uncertainty_type(&self) -> UncertaintyType {
        self.uncertainty_type
    }

    #[must_use]
    pub const fn scope(&self) -> WorldScope {
        self.scope
    }

    #[must_use]
    pub fn note(&self) -> &str {
        &self.note
    }

    #[must_use]
    pub const fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Live freshness of this uncertainty at `now` (TTL-aware).
    #[must_use]
    pub fn freshness_at(&self, now: DateTime<Utc>) -> Freshness {
        temporal::freshness_at(self.observed_at, self.expires_at, now)
    }
}

/// Bounds shared by the World Model runtime indexes.
pub mod limits {
    /// Maximum entities surfaced in one snapshot (v4 §65).
    pub const MAX_ENTITIES_PER_SNAPSHOT: usize = 16;
    /// Maximum situations surfaced in one snapshot (v4 §65).
    pub const MAX_SITUATIONS_PER_SNAPSHOT: usize = 8;
    /// Maximum hypotheses surfaced in one snapshot (v4 §65).
    pub const MAX_HYPOTHESES_PER_SNAPSHOT: usize = 8;
    /// Maximum timeline entries surfaced in one snapshot (v4 §65).
    pub const MAX_TEMPORAL_PER_SNAPSHOT: usize = 12;
    /// Maximum uncertainties surfaced in one snapshot.
    pub const MAX_UNCERTAINTIES_PER_SNAPSHOT: usize = 8;
}

/// The runtime World Model: bounded in-memory state plus a version counter.
///
/// Persistence is an infrastructure concern (ports/adapters); this struct is
/// the platform-neutral core that everything else is computed from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldModel {
    observations: Vec<Observation>,
    entities: EntityStateIndex,
    situations: Vec<Situation>,
    hypotheses: Vec<Hypothesis>,
    social_scene: Vec<SocialSceneState>,
    environment: EnvironmentState,
    timeline: TimelineState,
    uncertainties: Vec<WorldUncertainty>,
    version: u64,
}

impl Default for WorldModel {
    fn default() -> Self {
        Self::new()
    }
}

impl WorldModel {
    #[must_use]
    pub fn new() -> Self {
        Self {
            observations: Vec::new(),
            entities: EntityStateIndex::default(),
            situations: Vec::new(),
            hypotheses: Vec::new(),
            social_scene: Vec::new(),
            environment: EnvironmentState::default(),
            timeline: TimelineState::default(),
            uncertainties: Vec::new(),
            version: 1,
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        for observation in &self.observations {
            observation.validate()?;
        }
        self.entities.validate()?;
        for situation in &self.situations {
            situation.validate()?;
        }
        for hypothesis in &self.hypotheses {
            hypothesis.validate()?;
        }
        for scene in &self.social_scene {
            scene.validate()?;
        }
        self.environment.validate()?;
        self.timeline.validate()?;
        for uncertainty in &self.uncertainties {
            uncertainty.validate()?;
        }
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn observations(&self) -> &[Observation] {
        &self.observations
    }

    #[must_use]
    pub fn entities(&self) -> &EntityStateIndex {
        &self.entities
    }

    #[must_use]
    pub fn situations(&self) -> &[Situation] {
        &self.situations
    }

    #[must_use]
    pub fn hypotheses(&self) -> &[Hypothesis] {
        &self.hypotheses
    }

    #[must_use]
    pub fn social_scenes(&self) -> &[SocialSceneState] {
        &self.social_scene
    }

    #[must_use]
    pub fn environment(&self) -> &EnvironmentState {
        &self.environment
    }

    #[must_use]
    pub fn timeline(&self) -> &TimelineState {
        &self.timeline
    }

    #[must_use]
    pub fn uncertainties(&self) -> &[WorldUncertainty] {
        &self.uncertainties
    }

    /// Record one observation (dedupe by fingerprint, TTL-aware).
    pub fn observe(&mut self, observation: Observation) -> Result<(), WorldValidationError> {
        observation.validate()?;
        let fingerprint = observation.fingerprint();
        if let Some(existing) = self
            .observations
            .iter_mut()
            .find(|candidate| candidate.fingerprint() == fingerprint)
        {
            existing.replace_with(observation)?;
        } else {
            if self.observations.len() >= observation::MAX_RUNTIME_OBSERVATIONS {
                return Err(WorldValidationError::TooManyItems {
                    field: "observations",
                    length: self.observations.len(),
                    maximum: observation::MAX_RUNTIME_OBSERVATIONS,
                });
            }
            self.observations.push(observation);
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn apply_entity_update(
        &mut self,
        proposal: EntityUpdateProposal,
    ) -> Result<EntityId, WorldValidationError> {
        let id = self.entities.apply(proposal)?;
        self.version = self.version.saturating_add(1);
        Ok(id)
    }

    pub fn apply_situation_transition(
        &mut self,
        proposal: SituationTransitionProposal,
    ) -> Result<(), WorldValidationError> {
        proposal.validate()?;
        let situation = self
            .situations
            .iter_mut()
            .find(|situation| situation.id() == proposal.situation_id())
            .ok_or(WorldValidationError::InvalidState {
                reason: "situation does not exist",
            })?;
        if situation.version() != proposal.current_version() {
            return Err(WorldValidationError::StaleProposal {
                expected: proposal.current_version(),
                actual: situation.version(),
            });
        }
        situation.apply_transition(&proposal)?;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn add_situation(&mut self, situation: Situation) -> Result<(), WorldValidationError> {
        situation.validate()?;
        if self
            .situations
            .iter()
            .any(|candidate| candidate.id() == situation.id())
        {
            return Err(WorldValidationError::DuplicateItem {
                field: "situation id",
            });
        }
        let active = self
            .situations
            .iter()
            .filter(|candidate| candidate.status() == SituationStatus::Active)
            .count();
        if active >= situation::MAX_ACTIVE_SITUATIONS_PER_SCOPE {
            return Err(WorldValidationError::TooManyItems {
                field: "active situations",
                length: active,
                maximum: situation::MAX_ACTIVE_SITUATIONS_PER_SCOPE,
            });
        }
        self.situations.push(situation);
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn upsert_hypothesis(
        &mut self,
        hypothesis: Hypothesis,
    ) -> Result<(), WorldValidationError> {
        hypothesis.validate()?;
        // Dedupe: a hypothesis with the same proposition key merges evidence
        // rather than adding a duplicate (v4 §148).
        if let Some(existing) = self
            .hypotheses
            .iter_mut()
            .find(|existing| existing.same_proposition(&hypothesis))
        {
            existing.merge(hypothesis)?;
        } else {
            self.hypotheses.push(hypothesis);
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn update_social_scene(
        &mut self,
        update: SocialSceneUpdate,
    ) -> Result<(), WorldValidationError> {
        update.validate()?;
        if let Some(scene) = self
            .social_scene
            .iter_mut()
            .find(|scene| scene.conversation_id() == update.conversation_id())
        {
            scene.apply(update)?;
        } else {
            if self.social_scene.len() >= social_scene::MAX_SCENES_PER_WORLD {
                return Err(WorldValidationError::TooManyItems {
                    field: "social scenes",
                    length: self.social_scene.len(),
                    maximum: social_scene::MAX_SCENES_PER_WORLD,
                });
            }
            let mut scene = SocialSceneState::new(update.conversation_id(), update.now())?;
            scene.apply(update)?;
            self.social_scene.push(scene);
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn update_environment(
        &mut self,
        update: EnvironmentUpdate,
    ) -> Result<(), WorldValidationError> {
        let environment = self.environment.apply(update)?;
        self.environment = environment;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn add_uncertainty(
        &mut self,
        uncertainty: WorldUncertainty,
    ) -> Result<(), WorldValidationError> {
        uncertainty.validate()?;
        self.uncertainties.push(uncertainty);
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    pub fn push_timeline_entry(
        &mut self,
        entry: TimelineEntry,
    ) -> Result<(), WorldValidationError> {
        entry.validate()?;
        self.timeline.push(entry)?;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Apply a validated update batch (validate everything first, then
    /// mutate; version increments once per successful sub-apply).
    pub fn apply(
        &mut self,
        proposal: WorldUpdateProposal,
    ) -> Result<WorldUpdateState, WorldValidationError> {
        proposal.validate()?;
        // Validate all updates without mutating first.
        for update in proposal.updates() {
            update.validate()?;
        }
        let previous_version = self.version;
        for update in proposal.updates() {
            match update {
                WorldUpdate::Observation(observation) => {
                    self.observe(observation.clone())?;
                }
                WorldUpdate::Entity(update) => {
                    self.apply_entity_update(update.clone())?;
                }
                WorldUpdate::SituationTransition(update) => {
                    self.apply_situation_transition(update.clone())?;
                }
                WorldUpdate::SituationNew(situation) => {
                    self.add_situation(situation.clone())?;
                }
                WorldUpdate::Hypothesis(hypothesis) => {
                    self.upsert_hypothesis(hypothesis.clone())?;
                }
                WorldUpdate::SocialScene(update) => {
                    self.update_social_scene(update.clone())?;
                }
                WorldUpdate::Environment(update) => {
                    self.update_environment(update.clone())?;
                }
                WorldUpdate::Uncertainty(uncertainty) => {
                    self.add_uncertainty(uncertainty.clone())?;
                }
                WorldUpdate::Timeline(entry) => {
                    self.push_timeline_entry(entry.clone())?;
                }
            }
        }
        Ok(WorldUpdateState::new(
            proposal.updates().len(),
            previous_version,
            self.version,
        ))
    }

    /// Produce a bounded, relevant snapshot for a decision (v4 §63–§65).
    pub fn snapshot_for(
        &self,
        context: &WorldSnapshotContext,
    ) -> Result<WorldModelSnapshot, WorldValidationError> {
        snapshot::build_snapshot(self, context)
    }

    /// Erase every world-model record linked to the person. Used by data
    /// deletion flows (v4 §242).
    pub fn erase_person(&mut self, person_id: PersonId) {
        self.entities.erase_person(person_id);
        self.situations
            .retain(|situation| !situation.involves_person(person_id));
        self.hypotheses.retain(|hypothesis| {
            !matches!(hypothesis.scope(), WorldScope::Person { person_id: p } if p == person_id)
        });
        self.uncertainties.retain(|uncertainty| {
            !matches!(uncertainty.scope(), WorldScope::Person { person_id: p } if p == person_id)
        });
        self.social_scene
            .retain(|scene| !scene.active_participants().contains(&person_id));
        self.version = self.version.saturating_add(1);
    }

    /// Erase every world-model record linked to the conversation.
    pub fn erase_conversation(&mut self, conversation_id: ConversationId) {
        self.entities.erase_conversation(conversation_id);
        self.situations
            .retain(|situation| situation.conversation_id() != Some(conversation_id));
        self.hypotheses.retain(|hypothesis| {
            !matches!(
                hypothesis.scope(),
                WorldScope::Conversation { conversation_id: c } if c == conversation_id
            )
        });
        self.uncertainties.retain(|uncertainty| {
            !matches!(
                uncertainty.scope(),
                WorldScope::Conversation { conversation_id: c } if c == conversation_id
            )
        });
        self.social_scene
            .retain(|scene| scene.conversation_id() != conversation_id);
        self.version = self.version.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventId;
    use chrono::Duration;

    fn observation(scope: WorldScope, content: &str, now: DateTime<Utc>) -> Observation {
        let draft = ObservationDraft::new(
            scope,
            ObservationKind::SystemState,
            ObservationSource::SystemState,
            ObservationPayload::new(content, None::<&str>).expect("payload"),
            0.8,
            Some(3600),
        )
        .expect("draft");
        draft
            .build(super::ObservationId::new(), EventId::new(), now)
            .expect("observation")
    }

    #[test]
    fn observe_dedupes_by_fingerprint_and_bumps_version() {
        let now = Utc::now();
        let mut world = WorldModel::new();
        world.observe(observation(WorldScope::Global, "build main passed", now)).expect("obs");
        let v1 = world.version();
        world.observe(observation(WorldScope::Global, "build main passed", now)).expect("obs");
        assert_eq!(world.observations().len(), 1);
        assert_eq!(world.version(), v1 + 1);
        world.validate().expect("world valid");
    }

    #[test]
    fn transition_requires_current_version() {
        let now = Utc::now();
        let mut world = WorldModel::new();
        let situation = Situation::new(
            super::SituationId::new(),
            SituationKind::FutureEvent,
            SituationState::Planned,
            None,
            vec![],
            vec![],
            None,
            vec![],
            vec![],
            0.6,
            now,
        )
        .expect("situation");
        world.add_situation(situation.clone()).expect("added");
        // Correct version works.
        let proposal = SituationTransitionProposal::new(
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
        world.apply_situation_transition(proposal).expect("transition");
        // Stale version is rejected.
        let stale = SituationTransitionProposal::new(
            situation.id(),
            1,
            SituationState::Planned,
            SituationState::InProgress,
            0.9,
            ObservationSource::DirectUserStatement,
            false,
            None,
            now,
        )
        .expect("proposal");
        assert!(world.apply_situation_transition(stale).is_err());
    }

    #[test]
    fn hypothesis_dedupe_merges_in_world_model() {
        let now = Utc::now();
        let mut world = WorldModel::new();
        let proposition = WorldProposition::new("tool A 可能恢复").expect("proposition");
        world
            .upsert_hypothesis(Hypothesis::new(
                super::HypothesisId::new(),
                proposition.clone(),
                WorldScope::Global,
                0.3,
                now,
                None,
            )
            .expect("hypothesis"))
            .expect("upsert");
        world
            .upsert_hypothesis(Hypothesis::new(
                super::HypothesisId::new(),
                proposition,
                WorldScope::Global,
                0.6,
                now,
                None,
            )
            .expect("hypothesis"))
            .expect("upsert");
        assert_eq!(world.hypotheses().len(), 1);
        assert_eq!(world.hypotheses()[0].confidence(), 0.6);
    }

    #[test]
    fn apply_batch_validates_before_mutating() {
        let now = Utc::now();
        let mut world = WorldModel::new();
        // Empty batch is rejected before anything happens.
        let empty = WorldUpdateProposal::new(EventId::new(), vec![], vec![]);
        assert!(empty.is_err());
        // Batch with an invalid transition is rejected wholesale.
        let situation = Situation::new(
            super::SituationId::new(),
            SituationKind::FutureEvent,
            SituationState::Planned,
            None,
            vec![],
            vec![],
            None,
            vec![],
            vec![],
            0.6,
            now,
        )
        .expect("situation");
        world.add_situation(situation.clone()).expect("added");
        let invalid = WorldUpdateProposal::new(
            EventId::new(),
            vec![WorldReasonTag::SituationTransition],
            vec![WorldUpdate::SituationTransition(
                SituationTransitionProposal::new(
                    situation.id(),
                    situation.version(),
                    SituationState::InProgress,
                    SituationState::Completed,
                    0.9,
                    ObservationSource::DirectUserStatement,
                    false,
                    None,
                    now,
                )
                .expect("table allows in_progress→completed"),
            )],
        )
        .expect("batch");
        // expected_state mismatch → rejected in the validation pass.
        assert!(world.apply(invalid).is_err());
        situation.validate().expect("source situation untouched");
    }

    #[test]
    fn erase_person_and_conversation_clean_world_state() {
        let now = Utc::now();
        let person_id = PersonId::new();
        let other = PersonId::new();
        let conversation_id = ConversationId::new();
        let mut world = WorldModel::new();
        world
            .observe(observation(
                WorldScope::Person { person_id },
                "user busy today",
                now,
            ))
            .expect("obs");
        world
            .upsert_hypothesis(Hypothesis::new(
                super::HypothesisId::new(),
                WorldProposition::new("user 可能忙").expect("proposition"),
                WorldScope::Person { person_id },
                0.3,
                now,
                None,
            )
            .expect("hypothesis"))
            .expect("hyp");
        world
            .update_social_scene(
                SocialSceneUpdate::new(
                    conversation_id,
                    now,
                    vec![person_id, other],
                    vec![person_id],
                    vec![person_id],
                    false,
                    0.2,
                    SocialSceneKind::GroupDiscussion,
                )
                .expect("scene update"),
            )
            .expect("scene");
        world.erase_person(person_id);
        assert!(world
            .hypotheses()
            .iter()
            .all(|h| !matches!(h.scope(), WorldScope::Person { person_id: p } if p == person_id)));
        world.erase_conversation(conversation_id);
        assert!(world.social_scenes().is_empty());
        world.validate().expect("valid");
    }

    #[test]
    fn snapshot_context_bounds_and_relevance() {
        let now = Utc::now();
        let person_id = PersonId::new();
        let conversation_id = ConversationId::new();
        let mut world = WorldModel::new();
        // 20 unrelated entities (person-scoped) + 1 relevant entity.
        for i in 0..20 {
            world
                .apply_entity_update(
                    EntityUpdateProposal::new(
                        None,
                        EntityKind::Person,
                        Some(PersonId::new()),
                        None,
                        0.5,
                        vec![EntityUpdateAction::Set(
                            StateProperty::new("n", i.to_string(), 0.5, ObservationSource::SystemState, now, None).expect("prop"),
                        )],
                        now,
                    )
                    .expect("proposal"),
                )
                .expect("entity");
        }
        world
            .apply_entity_update(
                EntityUpdateProposal::new(
                    None,
                    EntityKind::Person,
                    Some(person_id),
                    None,
                    0.8,
                    vec![EntityUpdateAction::Set(
                        StateProperty::new("state", "busy", 0.9, ObservationSource::DirectUserStatement, now, None).expect("prop"),
                    )],
                    now,
                )
                .expect("proposal"),
            )
            .expect("entity");
        world
            .add_situation(
                Situation::new(
                    super::SituationId::new(),
                    SituationKind::ConversationState,
                    SituationState::OutcomeUnknown,
                    Some("会面结果未知".into()),
                    vec![],
                    vec![person_id],
                    Some(conversation_id),
                    vec![],
                    vec![],
                    0.6,
                    now,
                )
                .expect("situation"),
            )
            .expect("situation");
        let context = WorldSnapshotContext::new(now)
            .with_conversation(conversation_id)
            .with_person(person_id);
        let snapshot = world.snapshot_for(&context).expect("snapshot");
        snapshot.validate().expect("snapshot valid");
        // Only the relevant person entity is included (unrelated filtered).
        assert_eq!(snapshot.entities().len(), 1);
        assert_eq!(
            snapshot.entities()[0]
                .properties()
                .iter()
                .find(|p| p.key() == "state")
                .expect("prop")
                .value(),
            "busy"
        );
        assert_eq!(snapshot.situations().len(), 1);
        assert_eq!(snapshot.version(), world.version());
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn stale_state_is_unknown_in_snapshot_not_persistent() {
        let now = Utc::now();
        let mut world = WorldModel::new();
        let host = environment::HostId::new("qq").expect("host id");
        world
            .update_environment(
                EnvironmentUpdate::new(
                    vec![
                        HostState::new(host.clone(), ServiceHealth::Healthy, now, Duration::minutes(5))
                            .expect("host"),
                    ],
                    vec![],
                    ServiceHealth::Healthy,
                    RuntimeLoad::new(1, None, 1, 1, now).expect("load"),
                )
                .expect("update"),
            )
            .expect("env");
        let context = WorldSnapshotContext::new(now + Duration::minutes(6));
        let snapshot = world.snapshot_for(&context).expect("snapshot");
        let snapshot_host = snapshot
            .environment()
            .hosts()
            .iter()
            .find(|host_snapshot| host_snapshot.host().as_str() == "qq")
            .expect("host in snapshot");
        assert_eq!(snapshot_host.health(), ServiceHealth::Unknown);
    }
}
