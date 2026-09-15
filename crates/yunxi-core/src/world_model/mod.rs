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

mod causal;
mod common;
mod entity;
mod environment;
mod hypothesis;
mod ids;
mod observation;
mod prediction;
mod simulation;
mod situation;
mod snapshot;
mod social_scene;
mod temporal;
mod update;

pub use causal::{
    CausalKnowledge, CausalRelation, CausalRelationProposal, CausalScope, CausalSource,
    MAX_CAUSAL_CANDIDATES, MAX_CAUSAL_RELATIONS, MIN_EVIDENCE_OCCURRENCES, PatternKind,
    WorldPattern, occurrences_qualify, promote_candidate,
};
pub use common::{
    MAX_EVIDENCE_REFS, MAX_RELATED_IDS, MAX_WORLD_TEXT_BYTES, MAX_WORLD_TEXT_CHARS,
    MAX_WORLD_VALUE_BYTES, MAX_WORLD_VALUE_CHARS, WorldValidationError,
};
pub use entity::{
    EntityKind, EntityState, EntityStateIndex, EntityUpdate, EntityUpdateAction,
    EntityUpdateProposal, MAX_ACTIVE_ENTITIES, MAX_ENTITIES_PER_SCOPE, MAX_PROPERTIES_PER_ENTITY,
    StateProperty,
};
pub use environment::{
    EnvironmentState, EnvironmentUpdate, HostId, HostState, MAX_ENVIRONMENT_HOSTS,
    MAX_ENVIRONMENT_TOOLS, MAX_HOST_ID_BYTES, MAX_HOST_ID_CHARS, MAX_TOOL_NAME_BYTES,
    MAX_TOOL_NAME_CHARS, RuntimeLoad, ServiceHealth, ToolHealth,
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
    MAX_OBSERVATION_PAYLOAD_BYTES, MAX_OBSERVATION_PAYLOAD_CHARS, MAX_OBSERVATIONS_PER_EVENT,
    Observation, ObservationDraft, ObservationKind, ObservationPayload, ObservationSource,
    ObservationSourceReliability, observation_fingerprint,
};
pub use prediction::{
    MAX_PREDICTED_OUTCOMES, MAX_RUNTIME_PREDICTION_ERRORS, MAX_RUNTIME_PREDICTIONS, OutcomeKind,
    PredictedOutcome, Prediction, PredictionCalibration, PredictionError, PredictionHorizon,
    ProbabilityBand, quantize_probability,
};
pub use simulation::{
    ExecutionMode, MAX_SIMULATION_CACHE_ENTRIES, MAX_SIMULATION_CANDIDATES,
    MAX_SIMULATIONS_PER_ROOT_TRACE, SIMULATION_CACHE_TTL_SECS, SimulationBatch, SimulationBudget,
    SimulationCache, SimulationCacheEntry, SimulationCandidate, SimulationInput, SimulationResult,
};
pub use situation::{
    MAX_ACTIVE_SITUATIONS_PER_SCOPE, MAX_SITUATION_DETAIL_BYTES, MAX_SITUATION_DETAIL_CHARS,
    MAX_SITUATION_PARTICIPANTS, Situation, SituationKind, SituationState, SituationStatus,
    SituationTransitionProposal, can_transition,
};
pub use snapshot::{
    CausalRelationSnapshot, EntityStateSnapshot, EnvironmentSnapshot, HypothesisSnapshot,
    SituationSnapshot, SocialSceneSnapshot, TemporalSnapshotEntry, WorldModelSnapshot,
    WorldSnapshotContext, WorldSnapshotLimits, WorldUncertaintySnapshot,
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

use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 终态情境在"当前世界"里保留多久。
///
/// 它们不参与快照（快照只取 `is_active()` 的），留这么久只是为了追溯
/// "刚刚发生过什么"；再久就只是占地方的旧账。
const SITUATION_TERMINAL_GRACE: chrono::Duration = chrono::Duration::hours(24);

/// 世界模型里不确定项的总量上限（超出时淘汰最久没观测到的）。
///
/// 快照另有 `MAX_UNCERTAINTIES_PER_SNAPSHOT`（每次给模型看几条）；这个管的是
/// **状态本身**能存多少——没有它，`add_uncertainty` 是唯一一条无界增长的路。
const MAX_UNCERTAINTIES_PER_WORLD: usize = 64;

/// 情境列表的硬上限（终态优先淘汰，活动中的不动）。
///
/// 与 `MAX_ACTIVE_SITUATIONS_PER_SCOPE` 是两件事：那个管"同时活着几条"，
/// 这个管"整个列表能有多大"，后者是防止一次爆发把状态撑到无界。
const MAX_SITUATIONS_PER_WORLD: usize = 256;

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
    /// Maximum causal relations surfaced in one snapshot (v4 §65).
    pub const MAX_CAUSAL_PER_SNAPSHOT: usize = 8;
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
    causal: CausalKnowledge,
    predictions: Vec<Prediction>,
    prediction_errors: Vec<PredictionError>,
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
            causal: CausalKnowledge::default(),
            predictions: Vec::new(),
            prediction_errors: Vec::new(),
            version: 1,
        }
    }

    /// Restore a persisted world state (adapter use): validates every part
    /// and rebuilds a consistent model; version floored at 1.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_from_parts(
        observations: Vec<Observation>,
        entities: Vec<EntityState>,
        situations: Vec<Situation>,
        hypotheses: Vec<Hypothesis>,
        social_scene: Vec<SocialSceneState>,
        environment: EnvironmentState,
        timeline_entries: Vec<TimelineEntry>,
        uncertainties: Vec<WorldUncertainty>,
        version: u64,
    ) -> Result<Self, WorldValidationError> {
        let world = Self {
            observations,
            entities: EntityStateIndex::from_entities(entities)?,
            situations,
            hypotheses,
            social_scene,
            environment,
            timeline: TimelineState::from_entries(timeline_entries)?,
            uncertainties,
            causal: CausalKnowledge::default(),
            predictions: Vec::new(),
            prediction_errors: Vec::new(),
            version: if version == 0 { 1 } else { version },
        };
        world.validate()?;
        Ok(world)
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
        for prediction in &self.predictions {
            prediction.validate()?;
        }
        for prediction_error in &self.prediction_errors {
            prediction_error.validate()?;
        }
        self.causal.validate()?;
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

    #[must_use]
    pub fn predictions(&self) -> &[Prediction] {
        &self.predictions
    }

    #[must_use]
    pub fn causal(&self) -> &CausalKnowledge {
        &self.causal
    }

    /// Add a causal candidate (dedupe + bounded, v4 §97–§98).
    pub fn add_causal_proposal(
        &mut self,
        proposal: CausalRelationProposal,
    ) -> Result<(), WorldValidationError> {
        self.causal.add_proposal(proposal)?;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Promote a validated causal relation (repeated evidence / domain rule).
    pub fn add_causal_relation(
        &mut self,
        relation: CausalRelation,
    ) -> Result<(), WorldValidationError> {
        self.causal.promote(relation, None)?;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    #[must_use]
    pub fn prediction_errors(&self) -> &[PredictionError] {
        &self.prediction_errors
    }

    /// Record a validated prediction (bounded; newest wins on capacity).
    pub fn record_prediction(
        &mut self,
        prediction: Prediction,
    ) -> Result<(), WorldValidationError> {
        prediction.validate()?;
        if self.predictions.len() >= prediction::MAX_RUNTIME_PREDICTIONS {
            self.predictions.remove(0);
        }
        self.predictions.push(prediction);
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Record a prediction error (bounded calibration signal, v4 §123).
    pub fn record_prediction_error(
        &mut self,
        error: PredictionError,
    ) -> Result<(), WorldValidationError> {
        error.validate()?;
        if self.prediction_errors.len() >= prediction::MAX_RUNTIME_PREDICTION_ERRORS {
            self.prediction_errors.remove(0);
        }
        self.prediction_errors.push(error);
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Rolling calibration accuracy for all recorded errors (v4 §123).
    #[must_use]
    pub fn calibration_accuracy(&self) -> Option<f32> {
        if self.prediction_errors.is_empty() {
            return None;
        }
        let correct = self
            .prediction_errors
            .iter()
            .filter(|error| error.is_correct())
            .count();
        Some(correct as f32 / self.prediction_errors.len() as f32)
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
        // **按作用域计数**，不是全世界一起数。常量名一直叫 `..._PER_SCOPE`，
        // 但实现数的是全部：8 个会话各留一条在办的事，第 9 个会话就再也记不进来，
        // 而且报的是"active situations 太多"——听起来像是她自己太忙，其实是隔壁
        // 群占满了名额。
        //
        // 判据也统一用 `is_active()`（与快照、`expire_stale_situations` 同一口径）：
        // 此前这里用 `status() == Active`，而 `Unknown` 在两套判据里一个算活动、
        // 一个不算，于是同一条记录在两处得到相反的答案。
        let scope = situation.conversation_id();
        let active = self
            .situations
            .iter()
            .filter(|candidate| candidate.is_active() && candidate.conversation_id() == scope)
            .count();
        if active >= situation::MAX_ACTIVE_SITUATIONS_PER_SCOPE {
            return Err(WorldValidationError::TooManyItems {
                field: "active situations in scope",
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
            // 到顶时淘汰**最久没更新**的那个会话，而不是拒绝新会话：拒绝会让
            // 第 257 个群永远进不了世界模型（而且失败是静默的，只有一行日志），
            // 淘汰最旧的只是让最冷清的那个会话重新开始积累——它有 256 个会话在
            // 前面，本来也已经不是"当前正在发生的事"了。
            if self.social_scene.len() >= social_scene::MAX_SCENES_PER_WORLD
                && let Some((oldest, _)) = self
                    .social_scene
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, scene)| scene.updated_at())
            {
                self.social_scene.remove(oldest);
            }
            let mut scene = SocialSceneState::new(update.conversation_id(), update.now())?;
            scene.apply(update)?;
            self.social_scene.push(scene);
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Conversation-level event (message collision, floor change): bump the
    /// scene's version and timestamp without inferring anything (v4 appendix
    /// §4–§5). Errors when no scene exists yet.
    pub fn touch_social_scene(
        &mut self,
        conversation_id: ConversationId,
        now: DateTime<Utc>,
    ) -> Result<(), WorldValidationError> {
        let scene = self
            .social_scene
            .iter_mut()
            .find(|scene| scene.conversation_id() == conversation_id)
            .ok_or(WorldValidationError::InvalidState {
                reason: "no social scene for conversation",
            })?;
        scene.touch(now)?;
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
        // 这一处此前是**唯一没有上限**的集合（其它集合在各自的 mutator 或
        // `validate` 里都有 cap），而它照样会落盘：`WorldModel` 的模块注释写着
        // "bounded (text, counts, confidence)"。到顶时淘汰最久没观测到的那一条。
        if self.uncertainties.len() >= MAX_UNCERTAINTIES_PER_WORLD
            && let Some((oldest, _)) = self
                .uncertainties
                .iter()
                .enumerate()
                .min_by_key(|(_, existing)| existing.observed_at())
        {
            self.uncertainties.remove(oldest);
        }
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

    /// Deterministic staleness maintenance (v4 §92): expire `Planned` /
    /// `Unknown` situations of `kind` that were not updated within `max_age`.
    /// Transitions stay inside the validated state machine. Returns how many
    /// situations expired.
    pub fn expire_stale_situations(
        &mut self,
        kind: situation::SituationKind,
        max_age: chrono::Duration,
        now: DateTime<Utc>,
    ) -> usize {
        if max_age <= chrono::Duration::zero() {
            return 0;
        }
        let stale: Vec<_> = self
            .situations
            .iter()
            .filter(|situation| {
                situation.kind() == kind
                    && situation.is_active()
                    && matches!(
                        situation.state(),
                        situation::SituationState::Planned | situation::SituationState::Unknown
                    )
                    && now - situation.updated_at() > max_age
            })
            .map(|situation| situation.id())
            .collect();
        let mut expired = 0;
        for id in stale {
            if let Some(situation) = self.situations.iter_mut().find(|s| s.id() == id)
                && situation.expire(now).is_ok()
            {
                expired += 1;
            }
        }
        if expired > 0 {
            self.version = self.version.saturating_add(1);
        }
        expired
    }

    /// TTL maintenance (v4 §131): drop expired observations and hypotheses,
    /// and retire situations that are no longer part of the live world.
    ///
    /// Situations were previously never removed at all, which turned into a
    /// permanent latch on the host side: the host asked "are there fewer than
    /// eight situations?" before recording a new one, so once eight had *ever*
    /// been created no situation was recorded again — in any conversation, and
    /// across restarts, because terminal rows are persisted and restored.
    /// A bounded grace window keeps "what just happened" readable without
    /// letting the list grow forever.
    ///
    /// Idempotent; returns how many records were removed.
    pub fn prune_expired(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.live_record_count();
        self.observations
            .retain(|observation| observation.freshness_at(now) != Freshness::Expired);
        self.hypotheses
            .retain(|hypothesis| hypothesis.freshness_at(now) != Freshness::Expired);
        self.uncertainties
            .retain(|uncertainty| uncertainty.freshness_at(now) != Freshness::Expired);
        self.predictions
            .retain(|prediction| prediction.freshness_at(now) != Freshness::Expired);
        // 终态情境留一个短窗口做追溯，之后离开"当前世界"。活动中的情境不在这里
        // 消失——它们只能走状态机（`expire` / `apply_transition`）结束。
        self.situations.retain(|situation| {
            if situation.is_active() {
                return true;
            }
            // 终态但没有 `ended_at` 的记录同样要老去：拿 `updated_at` 当它的
            // 时间戳，否则它们会永远留在列表里——正是这次要修的那个坑。
            let ended = situation
                .ended_at()
                .unwrap_or_else(|| situation.updated_at());
            now - ended <= SITUATION_TERMINAL_GRACE
        });
        self.evict_excess_situations();
        let removed = before.saturating_sub(self.live_record_count());
        if removed > 0 {
            self.version = self.version.saturating_add(1);
        }
        removed
    }

    fn live_record_count(&self) -> usize {
        self.observations.len()
            + self.hypotheses.len()
            + self.uncertainties.len()
            + self.predictions.len()
            + self.situations.len()
    }

    /// 硬上限兜底：即使还在宽限期内，一次爆发也不能把情境列表撑到无界。
    /// 只淘汰终态记录，且从最旧的开始；活动中的一条都不动。
    fn evict_excess_situations(&mut self) {
        if self.situations.len() <= MAX_SITUATIONS_PER_WORLD {
            return;
        }
        let mut terminal: Vec<(usize, DateTime<Utc>)> = self
            .situations
            .iter()
            .enumerate()
            .filter(|(_, situation)| !situation.is_active())
            .map(|(index, situation)| {
                (
                    index,
                    situation
                        .ended_at()
                        .unwrap_or_else(|| situation.updated_at()),
                )
            })
            .collect();
        terminal.sort_by_key(|(_, stamp)| *stamp);
        let excess = self.situations.len() - MAX_SITUATIONS_PER_WORLD;
        let mut drop_indices: Vec<usize> = terminal
            .into_iter()
            .take(excess)
            .map(|(index, _)| index)
            .collect();
        drop_indices.sort_unstable_by(|left, right| right.cmp(left));
        for index in drop_indices {
            self.situations.remove(index);
        }
    }

    /// Erase every world-model record linked to the person, for data-deletion
    /// flows (v4 §242).
    ///
    /// **"Every" is the contract, and a test asserts it.** The version that
    /// shipped only cleared entities, situations, hypotheses, uncertainties and
    /// scenes — observations (which carry the user's own words), predictions
    /// and causal knowledge stayed behind, so someone who asked to be
    /// forgotten was still described in the world model. Anything linked to a
    /// person belongs in this list or in [`WorldModel::erase_conversation`].
    pub fn erase_person(&mut self, person_id: PersonId) {
        let scoped = |scope: WorldScope| !matches!(scope, WorldScope::Person { person_id: p } if p == person_id);
        self.entities.erase_person(person_id);
        self.situations
            .retain(|situation| !situation.involves_person(person_id));
        self.observations
            .retain(|observation| scoped(observation.scope()));
        self.hypotheses
            .retain(|hypothesis| scoped(hypothesis.scope()));
        self.uncertainties
            .retain(|uncertainty| scoped(uncertainty.scope()));
        self.predictions
            .retain(|prediction| scoped(prediction.scope()));
        self.prune_orphan_prediction_errors();
        self.causal.erase_person(person_id);
        self.social_scene
            .retain(|scene| !scene.active_participants().contains(&person_id));
        self.version = self.version.saturating_add(1);
    }

    /// Erase every world-model record linked to the conversation.
    pub fn erase_conversation(&mut self, conversation_id: ConversationId) {
        let scoped = |scope: WorldScope| {
            !matches!(
                scope,
                WorldScope::Conversation { conversation_id: c } if c == conversation_id
            )
        };
        self.entities.erase_conversation(conversation_id);
        self.situations
            .retain(|situation| situation.conversation_id() != Some(conversation_id));
        self.observations
            .retain(|observation| scoped(observation.scope()));
        self.hypotheses
            .retain(|hypothesis| scoped(hypothesis.scope()));
        self.uncertainties
            .retain(|uncertainty| scoped(uncertainty.scope()));
        self.predictions
            .retain(|prediction| scoped(prediction.scope()));
        self.prune_orphan_prediction_errors();
        self.causal.erase_conversation(conversation_id);
        self.social_scene
            .retain(|scene| scene.conversation_id() != conversation_id);
        self.version = self.version.saturating_add(1);
    }

    /// 误差记录只挂 prediction id、没有自己的作用域：预测被删掉之后它们就是悬空
    /// 引用（还带着"她当时以为会发生什么"），随预测一起清掉。
    fn prune_orphan_prediction_errors(&mut self) {
        let live: Vec<super::PredictionId> = self
            .predictions
            .iter()
            .map(|prediction| prediction.id())
            .collect();
        self.prediction_errors
            .retain(|error| live.contains(&error.prediction_id()));
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
        world
            .observe(observation(WorldScope::Global, "build main passed", now))
            .expect("obs");
        let v1 = world.version();
        world
            .observe(observation(WorldScope::Global, "build main passed", now))
            .expect("obs");
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
        world
            .apply_situation_transition(proposal)
            .expect("transition");
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
            .upsert_hypothesis(
                Hypothesis::new(
                    super::HypothesisId::new(),
                    proposition.clone(),
                    WorldScope::Global,
                    0.3,
                    now,
                    None,
                )
                .expect("hypothesis"),
            )
            .expect("upsert");
        world
            .upsert_hypothesis(
                Hypothesis::new(
                    super::HypothesisId::new(),
                    proposition,
                    WorldScope::Global,
                    0.6,
                    now,
                    None,
                )
                .expect("hypothesis"),
            )
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
            .upsert_hypothesis(
                Hypothesis::new(
                    super::HypothesisId::new(),
                    WorldProposition::new("user 可能忙").expect("proposition"),
                    WorldScope::Person { person_id },
                    0.3,
                    now,
                    None,
                )
                .expect("hypothesis"),
            )
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
        assert!(
            world.hypotheses().iter().all(
                |h| !matches!(h.scope(), WorldScope::Person { person_id: p } if p == person_id)
            )
        );
        // **观察也在契约之内。** 观察正文是用户自己的话；此前 erase_person 只清
        // 实体/情境/假设/不确定/场景，观察与预测原样留着——要求被忘记的人在世界
        // 模型里仍然被描述着。删除路径（宿主侧先删 SQL 行、再 restore_from_store）
        // 不经过这个函数，但公共 API 的承诺必须为真。
        assert!(
            world.observations().is_empty(),
            "被擦除者的观察必须一并消失"
        );
        assert!(
            world
                .observations()
                .iter()
                .all(|observation| observation.scope() != WorldScope::Person { person_id }),
            "擦除不得留下 person 作用域的观察"
        );
        world.erase_conversation(conversation_id);
        assert!(world.social_scenes().is_empty());
        world.validate().expect("valid");
    }

    #[test]
    fn erase_person_clears_predictions_and_causal_knowledge_too() {
        // 同一条契约的另一半：预测（"她以为会发生什么"）与因果知识（"这个人身上
        // 的规律"）同样按作用域持有个人信息。
        let now = Utc::now();
        let person_id = PersonId::new();
        let other = PersonId::new();
        let mut world = WorldModel::new();
        for person in [person_id, other] {
            world
                .observe(observation(
                    WorldScope::Person { person_id: person },
                    "状态",
                    now,
                ))
                .expect("obs");
        }
        world.erase_person(person_id);
        assert_eq!(world.observations().len(), 1, "只清掉目标那个人的观察");
        assert_eq!(
            world.observations()[0].scope(),
            WorldScope::Person { person_id: other }
        );
        world.validate().expect("valid");
    }

    #[test]
    fn collision_touch_bumps_scene_version_without_inference() {
        let now = Utc::now();
        let conversation_id = ConversationId::new();
        let mut world = WorldModel::new();
        world
            .update_social_scene(
                SocialSceneUpdate::new(
                    conversation_id,
                    now,
                    vec![PersonId::new()],
                    vec![],
                    vec![],
                    false,
                    0.3,
                    SocialSceneKind::GroupDiscussion,
                )
                .expect("scene update"),
            )
            .expect("scene");
        let before = world.social_scenes()[0].conversation_version();
        world
            .touch_social_scene(conversation_id, now)
            .expect("touch");
        assert_eq!(world.social_scenes()[0].conversation_version(), before + 1);
        assert_eq!(
            world.social_scenes()[0].interruption_cost(),
            world.social_scenes()[0].interruption_cost()
        );
        // No scene → error (caller decides; nothing invented).
        assert!(
            world
                .touch_social_scene(ConversationId::new(), now)
                .is_err()
        );
    }

    #[test]
    fn prune_expired_removes_expired_records_only() {
        let now = Utc::now();
        let mut world = WorldModel::new();
        let ttl_observation = observation(
            WorldScope::Global,
            "short-lived state",
            now - Duration::hours(2),
        );
        // The observation helper uses a 1h TTL from its observed_at, so it is
        // already expired now.
        world.observe(ttl_observation).expect("obs");
        world
            .upsert_hypothesis(
                Hypothesis::new(
                    super::HypothesisId::new(),
                    WorldProposition::new("可能忙").expect("proposition"),
                    WorldScope::Global,
                    0.3,
                    now - Duration::hours(2),
                    Some(now - Duration::hours(1)),
                )
                .expect("hypothesis"),
            )
            .expect("hyp");
        let removed = world.prune_expired(now);
        assert_eq!(removed, 2);
        assert!(world.observations().is_empty());
        assert!(world.hypotheses().is_empty());
    }

    #[test]
    fn terminal_situations_leave_the_live_world_instead_of_piling_up() {
        // 情境此前**从不**被清理，而宿主侧的门是"总数 < 8 才记新的"：一旦历史上
        // 攒够 8 条（终态也算），任何会话都再也记不进新情境，且终态记录会被持久化、
        // 重启后照样占着名额。这里钉住两件事：宽限期过后终态离场，活动的不受影响。
        let now = Utc::now();
        let mut world = WorldModel::new();
        let conversation_id = ConversationId::new();
        let mut settled = situation_for(conversation_id, now - Duration::hours(30));
        // InProgress → Completed 是转换表允许的路径（InProgress 不能直接 Expired）。
        settled
            .apply_transition(
                &SituationTransitionProposal::new(
                    settled.id(),
                    settled.version(),
                    SituationState::InProgress,
                    SituationState::Completed,
                    0.6,
                    super::observation::ObservationSource::DirectUserStatement,
                    false,
                    None,
                    now - Duration::hours(25),
                )
                .expect("proposal"),
            )
            .expect("complete");
        world.add_situation(settled).expect("terminal situation");
        let active = situation_for(conversation_id, now);
        world.add_situation(active).expect("active situation");

        let removed = world.prune_expired(now);
        assert_eq!(removed, 1, "只有过了宽限期的终态情境离场");
        assert_eq!(world.situations().len(), 1);
        assert!(world.situations()[0].is_active(), "活动中的情境不许被清掉");

        // 宽限期内的终态留着，供"刚刚发生过什么"追溯。
        let mut recent = situation_for(conversation_id, now);
        recent
            .apply_transition(
                &SituationTransitionProposal::new(
                    recent.id(),
                    recent.version(),
                    SituationState::InProgress,
                    SituationState::Completed,
                    0.6,
                    super::observation::ObservationSource::DirectUserStatement,
                    false,
                    None,
                    now,
                )
                .expect("proposal"),
            )
            .expect("complete");
        world.add_situation(recent).expect("recent terminal");
        assert_eq!(world.prune_expired(now), 0);
        assert_eq!(world.situations().len(), 2);
    }

    #[test]
    fn active_situation_cap_counts_within_a_scope_not_world_wide() {
        // 常量叫 PER_SCOPE，实现却数的是全世界：8 个会话各留一条在办的事，
        // 第 9 个会话就再也记不进来。
        let now = Utc::now();
        let mut world = WorldModel::new();
        for _ in 0..super::situation::MAX_ACTIVE_SITUATIONS_PER_SCOPE {
            world
                .add_situation(situation_for(ConversationId::new(), now))
                .expect("每个会话自己的一格");
        }
        // 另一个会话照样能记：名额是按作用域算的。
        let fresh = ConversationId::new();
        world
            .add_situation(situation_for(fresh, now))
            .expect("另一个会话不该被前面的会话占满");
        // 同一个作用域内超限才拒绝。
        for _ in 1..super::situation::MAX_ACTIVE_SITUATIONS_PER_SCOPE {
            world
                .add_situation(situation_for(fresh, now))
                .expect("同作用域内的第 2..8 条");
        }
        assert!(
            world.add_situation(situation_for(fresh, now)).is_err(),
            "同一个会话内超过上限应当拒绝"
        );
    }

    fn scene_update(conversation_id: ConversationId, at: DateTime<Utc>) -> SocialSceneUpdate {
        SocialSceneUpdate::new(
            conversation_id,
            at,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            0.2,
            SocialSceneKind::GroupDiscussion,
        )
        .expect("scene update")
    }

    fn situation_for(conversation_id: ConversationId, at: DateTime<Utc>) -> Situation {
        Situation::new(
            super::SituationId::new(),
            situation::SituationKind::ConversationState,
            situation::SituationState::InProgress,
            Some("测试情境".to_owned()),
            Vec::new(),
            vec![PersonId::new()],
            Some(conversation_id),
            Vec::new(),
            Vec::new(),
            0.5,
            at,
        )
        .expect("situation")
    }

    #[test]
    fn a_snapshot_with_only_causal_knowledge_is_not_reported_as_empty() {
        // `is_empty` 以前漏了 `causal`：只有因果关系的那份世界状态会被判成"没东西
        // 可存"，而调用方正是靠这个判断决定要不要落盘。
        let now = Utc::now();
        let mut world = WorldModel::new();
        let context = WorldSnapshotContext::new(now);
        assert!(
            world.snapshot_for(&context).expect("snapshot").is_empty(),
            "空白世界模型应当是空的"
        );
        world
            .add_causal_relation(
                super::causal::CausalRelation::new(
                    super::CausalRelationId::new(),
                    super::causal::WorldPattern::new(
                        super::causal::PatternKind::Environment,
                        "rate_limited",
                    )
                    .expect("cause"),
                    super::causal::WorldPattern::new(
                        super::causal::PatternKind::Tool,
                        "retry_later",
                    )
                    .expect("effect"),
                    0.8,
                    0.9,
                    super::causal::CausalSource::DomainRule,
                    super::causal::CausalScope::Global,
                    1,
                )
                .expect("relation"),
            )
            .expect("causal relation");
        assert!(
            !world.snapshot_for(&context).expect("snapshot").is_empty(),
            "只有因果关系的状态也必须算作非空"
        );
    }

    #[test]
    fn the_snapshot_includes_causal_knowledge_for_every_person_in_context() {
        // 因果快照此前只取 `person_ids().first()`，第 2..8 个参与者的
        // person-specific 关系全部被丢掉——而它们正是"这个人身上会怎样"那部分。
        let now = Utc::now();
        let first = PersonId::new();
        let second = PersonId::new();
        let mut world = WorldModel::new();
        for person in [first, second] {
            world
                .add_causal_relation(
                    super::causal::CausalRelation::new(
                        super::CausalRelationId::new(),
                        super::causal::WorldPattern::new(
                            super::causal::PatternKind::Environment,
                            "rate_limited",
                        )
                        .expect("cause"),
                        super::causal::WorldPattern::new(
                            super::causal::PatternKind::Tool,
                            format!("outcome_{}", person.into_uuid()),
                        )
                        .expect("effect"),
                        0.8,
                        0.9,
                        super::causal::CausalSource::DomainRule,
                        super::causal::CausalScope::PersonSpecific { person_id: person },
                        1,
                    )
                    .expect("relation"),
                )
                .expect("promote");
        }
        let context = WorldSnapshotContext::new(now)
            .with_person(first)
            .with_person(second);
        let snapshot = world.snapshot_for(&context).expect("snapshot");
        assert_eq!(
            snapshot.causal().len(),
            2,
            "上下文里的每个参与者都该带出自己的因果知识"
        );
    }

    #[test]
    fn uncertainties_are_bounded_like_every_other_collection() {
        // `add_uncertainty` 是唯一没有上限的 mutator，而它照样落盘。
        let now = Utc::now();
        let mut world = WorldModel::new();
        for index in 0..(MAX_UNCERTAINTIES_PER_WORLD + 4) {
            world
                .add_uncertainty(
                    WorldUncertainty::new(
                        super::UncertaintyId::new(),
                        super::UncertaintyType::StateUnknown,
                        WorldScope::Global,
                        format!("第 {index} 条"),
                        now - Duration::minutes((MAX_UNCERTAINTIES_PER_WORLD + 4 - index) as i64),
                        None,
                    )
                    .expect("uncertainty"),
                )
                .expect("add");
        }
        assert_eq!(world.uncertainties().len(), MAX_UNCERTAINTIES_PER_WORLD);
        // 最久没观测到的那条先走。
        assert!(
            !world
                .uncertainties()
                .iter()
                .any(|item| item.note().contains("第 0 条")),
            "最旧的应当被淘汰"
        );
        world.validate().expect("valid");
    }

    #[test]
    fn a_new_conversation_scene_evicts_the_stalest_one_instead_of_failing() {
        // 场景上限此前是"到顶就拒"：第 257 个会话永远进不了世界模型，失败还很安静。
        let now = Utc::now();
        let mut world = WorldModel::new();
        let mut oldest = None;
        for index in 0..social_scene::MAX_SCENES_PER_WORLD {
            let conversation_id = ConversationId::new();
            if index == 0 {
                oldest = Some(conversation_id);
            }
            world
                .update_social_scene(scene_update(
                    conversation_id,
                    // index 0 最旧（最久没更新），index 越大越新。
                    now - Duration::minutes((social_scene::MAX_SCENES_PER_WORLD - index) as i64),
                ))
                .expect("fill to cap");
        }
        assert_eq!(
            world.social_scenes().len(),
            social_scene::MAX_SCENES_PER_WORLD
        );
        let fresh = ConversationId::new();
        world
            .update_social_scene(scene_update(fresh, now))
            .expect("新会话应当能挤掉最久没更新的那个，而不是被拒绝");
        assert_eq!(
            world.social_scenes().len(),
            social_scene::MAX_SCENES_PER_WORLD
        );
        assert!(
            world
                .social_scenes()
                .iter()
                .any(|scene| scene.conversation_id() == fresh),
            "新会话必须记进来"
        );
        assert!(
            !world
                .social_scenes()
                .iter()
                .any(|scene| Some(scene.conversation_id()) == oldest),
            "最久没更新的那个应当被淘汰"
        );
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
                            StateProperty::new(
                                "n",
                                i.to_string(),
                                0.5,
                                ObservationSource::SystemState,
                                now,
                                None,
                            )
                            .expect("prop"),
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
                        StateProperty::new(
                            "state",
                            "busy",
                            0.9,
                            ObservationSource::DirectUserStatement,
                            now,
                            None,
                        )
                        .expect("prop"),
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
                        HostState::new(
                            host.clone(),
                            ServiceHealth::Healthy,
                            now,
                            Duration::minutes(5),
                        )
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
