//! Snapshot: bounded, relevant view of the World Model for one decision
//! (v4 §63–67, §214). Never hand the planner the whole store.

use super::entity::{EntityKind, EntityState, StateProperty};
use super::environment::{ServiceHealth, ToolHealth};
use super::hypothesis::{Hypothesis, HypothesisStatus};
use super::situation::Situation;
use super::social_scene::SocialSceneState;
use super::temporal::{Freshness, TimeInterval, WorldRef};
use super::{WorldModel, WorldScope, WorldUncertainty, WorldValidationError, limits};
use crate::{ConversationId, GoalId, OpenLoopId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_CONTEXT_PERSONS: usize = 8;
pub const MAX_CONTEXT_GOALS: usize = 8;
pub const MAX_CONTEXT_OPEN_LOOPS: usize = 8;

/// Configurable caps for one snapshot (v4 §65).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshotLimits {
    entities: usize,
    situations: usize,
    hypotheses: usize,
    causal: usize,
    temporal: usize,
    uncertainties: usize,
}

impl Default for WorldSnapshotLimits {
    fn default() -> Self {
        Self {
            entities: limits::MAX_ENTITIES_PER_SNAPSHOT,
            situations: limits::MAX_SITUATIONS_PER_SNAPSHOT,
            hypotheses: limits::MAX_HYPOTHESES_PER_SNAPSHOT,
            causal: limits::MAX_CAUSAL_PER_SNAPSHOT,
            temporal: limits::MAX_TEMPORAL_PER_SNAPSHOT,
            uncertainties: limits::MAX_UNCERTAINTIES_PER_SNAPSHOT,
        }
    }
}

impl WorldSnapshotLimits {
    pub fn new(
        entities: usize,
        situations: usize,
        hypotheses: usize,
        causal: usize,
        temporal: usize,
        uncertainties: usize,
    ) -> Result<Self, WorldValidationError> {
        let limits = Self {
            entities,
            situations,
            hypotheses,
            causal,
            temporal,
            uncertainties,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        for (field, value, maximum) in [
            (
                "snapshot entities",
                self.entities,
                limits::MAX_ENTITIES_PER_SNAPSHOT,
            ),
            (
                "snapshot situations",
                self.situations,
                limits::MAX_SITUATIONS_PER_SNAPSHOT,
            ),
            (
                "snapshot hypotheses",
                self.hypotheses,
                limits::MAX_HYPOTHESES_PER_SNAPSHOT,
            ),
            (
                "snapshot causal",
                self.causal,
                limits::MAX_CAUSAL_PER_SNAPSHOT,
            ),
            (
                "snapshot temporal",
                self.temporal,
                limits::MAX_TEMPORAL_PER_SNAPSHOT,
            ),
            (
                "snapshot uncertainties",
                self.uncertainties,
                limits::MAX_UNCERTAINTIES_PER_SNAPSHOT,
            ),
        ] {
            if value > maximum {
                return Err(WorldValidationError::TooManyItems {
                    field,
                    length: value,
                    maximum,
                });
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn entities(&self) -> usize {
        self.entities
    }

    #[must_use]
    pub const fn situations(&self) -> usize {
        self.situations
    }

    #[must_use]
    pub const fn hypotheses(&self) -> usize {
        self.hypotheses
    }

    #[must_use]
    pub const fn causal(&self) -> usize {
        self.causal
    }

    #[must_use]
    pub const fn temporal(&self) -> usize {
        self.temporal
    }

    #[must_use]
    pub const fn uncertainties(&self) -> usize {
        self.uncertainties
    }
}

/// Retrieval context: what the current decision is about (v4 §64).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSnapshotContext {
    conversation_id: Option<ConversationId>,
    person_ids: Vec<PersonId>,
    goal_ids: Vec<GoalId>,
    open_loop_ids: Vec<OpenLoopId>,
    now: DateTime<Utc>,
    limits: Option<WorldSnapshotLimits>,
}

impl WorldSnapshotContext {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            conversation_id: None,
            person_ids: Vec::new(),
            goal_ids: Vec::new(),
            open_loop_ids: Vec::new(),
            now,
            limits: None,
        }
    }

    pub fn with_conversation(mut self, conversation_id: ConversationId) -> Self {
        self.conversation_id = Some(conversation_id);
        self
    }

    pub fn with_person(mut self, person_id: PersonId) -> Self {
        if !self.person_ids.contains(&person_id) {
            self.person_ids.push(person_id);
        }
        self
    }

    pub fn with_goal(mut self, goal_id: GoalId) -> Self {
        if !self.goal_ids.contains(&goal_id) {
            self.goal_ids.push(goal_id);
        }
        self
    }

    pub fn with_open_loop(mut self, open_loop_id: OpenLoopId) -> Self {
        if !self.open_loop_ids.contains(&open_loop_id) {
            self.open_loop_ids.push(open_loop_id);
        }
        self
    }

    pub fn with_limits(mut self, limits: WorldSnapshotLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        if self.person_ids.len() > MAX_CONTEXT_PERSONS {
            return Err(WorldValidationError::TooManyItems {
                field: "context persons",
                length: self.person_ids.len(),
                maximum: MAX_CONTEXT_PERSONS,
            });
        }
        if self.goal_ids.len() > MAX_CONTEXT_GOALS {
            return Err(WorldValidationError::TooManyItems {
                field: "context goals",
                length: self.goal_ids.len(),
                maximum: MAX_CONTEXT_GOALS,
            });
        }
        if self.open_loop_ids.len() > MAX_CONTEXT_OPEN_LOOPS {
            return Err(WorldValidationError::TooManyItems {
                field: "context open loops",
                length: self.open_loop_ids.len(),
                maximum: MAX_CONTEXT_OPEN_LOOPS,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation_id
    }

    #[must_use]
    pub fn person_ids(&self) -> &[PersonId] {
        &self.person_ids
    }

    #[must_use]
    pub fn goal_ids(&self) -> &[GoalId] {
        &self.goal_ids
    }

    #[must_use]
    pub fn open_loop_ids(&self) -> &[OpenLoopId] {
        &self.open_loop_ids
    }

    #[must_use]
    pub const fn now(&self) -> DateTime<Utc> {
        self.now
    }

    fn limits(&self) -> WorldSnapshotLimits {
        self.limits.unwrap_or_default()
    }

    fn matches_scope(&self, scope: WorldScope) -> bool {
        match scope {
            WorldScope::Global => true,
            WorldScope::Person { person_id } => self.person_ids.contains(&person_id),
            WorldScope::Conversation { conversation_id } => {
                self.conversation_id == Some(conversation_id)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatePropertySnapshot {
    key: String,
    value: String,
    confidence: f32,
    freshness: Freshness,
}

impl StatePropertySnapshot {
    fn from_property(property: &StateProperty, now: DateTime<Utc>) -> Self {
        Self {
            key: property.key().to_owned(),
            value: property.value().to_owned(),
            confidence: property.confidence(),
            freshness: property.freshness_at(now),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.confidence, "snapshot property confidence")?;
        Ok(())
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn freshness(&self) -> Freshness {
        self.freshness
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityStateSnapshot {
    id: super::EntityId,
    kind: EntityKind,
    linked_person: Option<PersonId>,
    linked_conversation: Option<ConversationId>,
    properties: Vec<StatePropertySnapshot>,
    confidence: f32,
    last_observed_at: DateTime<Utc>,
    version: u64,
}

impl EntityStateSnapshot {
    fn from_entity(entity: &EntityState, now: DateTime<Utc>) -> Self {
        Self {
            id: entity.id(),
            kind: entity.kind(),
            linked_person: entity.linked_person(),
            linked_conversation: entity.linked_conversation(),
            properties: entity
                .properties()
                .iter()
                .map(|property| StatePropertySnapshot::from_property(property, now))
                .collect(),
            confidence: entity.confidence(),
            last_observed_at: entity.last_observed_at(),
            version: entity.version(),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.confidence, "snapshot entity confidence")?;
        if self.properties.len() > super::entity::MAX_PROPERTIES_PER_ENTITY {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot entity properties",
                length: self.properties.len(),
                maximum: super::entity::MAX_PROPERTIES_PER_ENTITY,
            });
        }
        for property in &self.properties {
            property.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::EntityId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    #[must_use]
    pub const fn linked_person(&self) -> Option<PersonId> {
        self.linked_person
    }

    #[must_use]
    pub const fn linked_conversation(&self) -> Option<ConversationId> {
        self.linked_conversation
    }

    #[must_use]
    pub fn properties(&self) -> &[StatePropertySnapshot] {
        &self.properties
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn last_observed_at(&self) -> DateTime<Utc> {
        self.last_observed_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SituationSnapshot {
    id: super::SituationId,
    kind: super::situation::SituationKind,
    state: super::situation::SituationState,
    status: super::situation::SituationStatus,
    detail: Option<String>,
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

impl SituationSnapshot {
    fn from_situation(situation: &Situation) -> Self {
        Self {
            id: situation.id(),
            kind: situation.kind(),
            state: situation.state(),
            status: situation.status(),
            detail: situation.detail().map(str::to_owned),
            persons: situation.persons().to_vec(),
            conversation_id: situation.conversation_id(),
            related_goals: situation.related_goals().to_vec(),
            related_open_loops: situation.related_open_loops().to_vec(),
            confidence: situation.confidence(),
            started_at: situation.started_at(),
            updated_at: situation.updated_at(),
            ended_at: situation.ended_at(),
            version: situation.version(),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.confidence, "snapshot situation confidence")?;
        if self.related_goals.len() > super::common::MAX_RELATED_IDS {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot situation goals",
                length: self.related_goals.len(),
                maximum: super::common::MAX_RELATED_IDS,
            });
        }
        if self.related_open_loops.len() > super::common::MAX_RELATED_IDS {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot situation open loops",
                length: self.related_open_loops.len(),
                maximum: super::common::MAX_RELATED_IDS,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::SituationId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> super::situation::SituationKind {
        self.kind
    }

    #[must_use]
    pub const fn state(&self) -> super::situation::SituationState {
        self.state
    }

    #[must_use]
    pub const fn status(&self) -> super::situation::SituationStatus {
        self.status
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HypothesisSnapshot {
    id: super::HypothesisId,
    proposition: String,
    scope: WorldScope,
    confidence: f32,
    status: HypothesisStatus,
    evidence_for_count: usize,
    evidence_against_count: usize,
    updated_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    freshness: Freshness,
}

impl HypothesisSnapshot {
    fn from_hypothesis(hypothesis: &Hypothesis, now: DateTime<Utc>) -> Self {
        Self {
            id: hypothesis.id(),
            proposition: hypothesis.proposition().text().to_owned(),
            scope: hypothesis.scope(),
            confidence: hypothesis.confidence(),
            status: hypothesis.status(),
            evidence_for_count: hypothesis.evidence_for().len(),
            evidence_against_count: hypothesis.evidence_against().len(),
            updated_at: hypothesis.updated_at(),
            expires_at: hypothesis.expires_at(),
            freshness: hypothesis.freshness_at(now),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.confidence, "snapshot hypothesis confidence")?;
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::HypothesisId {
        self.id
    }

    #[must_use]
    pub fn proposition(&self) -> &str {
        &self.proposition
    }

    #[must_use]
    pub const fn scope(&self) -> WorldScope {
        self.scope
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub const fn status(&self) -> HypothesisStatus {
        self.status
    }

    #[must_use]
    pub const fn evidence_for_count(&self) -> usize {
        self.evidence_for_count
    }

    #[must_use]
    pub const fn evidence_against_count(&self) -> usize {
        self.evidence_against_count
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn freshness(&self) -> Freshness {
        self.freshness
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocialSceneSnapshot {
    conversation_id: ConversationId,
    scene_kind: super::social_scene::SocialSceneKind,
    activity_level: f32,
    interruption_cost: f32,
    bot_addressed: bool,
    current_floor: Vec<PersonId>,
    active_participants_count: usize,
    conversation_version: u64,
}

impl SocialSceneSnapshot {
    fn from_scene(scene: &SocialSceneState) -> Self {
        Self {
            conversation_id: scene.conversation_id(),
            scene_kind: scene.scene_kind(),
            activity_level: scene.activity_level(),
            interruption_cost: scene.interruption_cost(),
            bot_addressed: scene.bot_addressed(),
            current_floor: scene.current_floor().to_vec(),
            active_participants_count: scene.active_participants().len(),
            conversation_version: scene.conversation_version(),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.activity_level, "snapshot scene activity")?;
        super::common::validate_unit(self.interruption_cost, "snapshot scene interruption")?;
        if self.current_floor.len() > super::social_scene::MAX_SCENE_CURRENT_FLOOR {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot scene floor",
                length: self.current_floor.len(),
                maximum: super::social_scene::MAX_SCENE_CURRENT_FLOOR,
            });
        }
        Ok(())
    }

    #[must_use]
    pub const fn conversation_id(&self) -> ConversationId {
        self.conversation_id
    }

    #[must_use]
    pub const fn scene_kind(&self) -> super::social_scene::SocialSceneKind {
        self.scene_kind
    }

    #[must_use]
    pub const fn activity_level(&self) -> f32 {
        self.activity_level
    }

    #[must_use]
    pub const fn interruption_cost(&self) -> f32 {
        self.interruption_cost
    }

    #[must_use]
    pub const fn bot_addressed(&self) -> bool {
        self.bot_addressed
    }

    #[must_use]
    pub fn current_floor(&self) -> &[PersonId] {
        &self.current_floor
    }

    #[must_use]
    pub const fn active_participants_count(&self) -> usize {
        self.active_participants_count
    }

    #[must_use]
    pub const fn conversation_version(&self) -> u64 {
        self.conversation_version
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSnapshot {
    host: super::environment::HostId,
    health: ServiceHealth,
}

impl HostSnapshot {
    #[must_use]
    pub fn host(&self) -> &super::environment::HostId {
        &self.host
    }

    #[must_use]
    pub const fn health(&self) -> ServiceHealth {
        self.health
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSnapshot {
    tool_name: String,
    health: ServiceHealth,
    detail: Option<String>,
}

impl ToolSnapshot {
    fn from_tool(tool: &ToolHealth, now: DateTime<Utc>) -> Self {
        Self {
            tool_name: tool.tool_name().to_owned(),
            health: tool.effective_health_at(now),
            detail: tool.detail().map(str::to_owned),
        }
    }

    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    #[must_use]
    pub const fn health(&self) -> ServiceHealth {
        self.health
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    hosts: Vec<HostSnapshot>,
    tools: Vec<ToolSnapshot>,
    model_health: ServiceHealth,
    availability_fraction: f32,
    version: u64,
}

impl EnvironmentSnapshot {
    fn from_environment(
        environment: &super::environment::EnvironmentState,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            hosts: environment
                .hosts()
                .iter()
                .map(|host| HostSnapshot {
                    host: host.host().clone(),
                    health: host.effective_health_at(now),
                })
                .collect(),
            tools: environment
                .tools()
                .iter()
                .map(|tool| ToolSnapshot::from_tool(tool, now))
                .collect(),
            model_health: environment.model_health(),
            availability_fraction: environment.load().availability_fraction(),
            version: environment.version(),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.availability_fraction, "snapshot availability")?;
        if self.hosts.len() > super::environment::MAX_ENVIRONMENT_HOSTS {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot hosts",
                length: self.hosts.len(),
                maximum: super::environment::MAX_ENVIRONMENT_HOSTS,
            });
        }
        if self.tools.len() > super::environment::MAX_ENVIRONMENT_TOOLS {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot tools",
                length: self.tools.len(),
                maximum: super::environment::MAX_ENVIRONMENT_TOOLS,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn hosts(&self) -> &[HostSnapshot] {
        &self.hosts
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolSnapshot] {
        &self.tools
    }

    #[must_use]
    pub const fn model_health(&self) -> ServiceHealth {
        self.model_health
    }

    #[must_use]
    pub const fn availability_fraction(&self) -> f32 {
        self.availability_fraction
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalSnapshotEntry {
    subject: WorldRef,
    interval: TimeInterval,
}

impl TemporalSnapshotEntry {
    fn from_entry(entry: &super::temporal::TimelineEntry) -> Self {
        Self {
            subject: entry.subject(),
            interval: *entry.interval(),
        }
    }

    #[must_use]
    pub const fn subject(&self) -> WorldRef {
        self.subject
    }

    #[must_use]
    pub const fn interval(&self) -> &TimeInterval {
        &self.interval
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldUncertaintySnapshot {
    uncertainty_type: super::UncertaintyType,
    scope: WorldScope,
    note: String,
    freshness: Freshness,
    expires_at: Option<DateTime<Utc>>,
}

impl WorldUncertaintySnapshot {
    fn from_uncertainty(uncertainty: &WorldUncertainty, now: DateTime<Utc>) -> Self {
        Self {
            uncertainty_type: uncertainty.uncertainty_type(),
            scope: uncertainty.scope(),
            note: uncertainty.note().to_owned(),
            freshness: uncertainty.freshness_at(now),
            expires_at: uncertainty.expires_at(),
        }
    }

    #[must_use]
    pub const fn uncertainty_type(&self) -> super::UncertaintyType {
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
    pub const fn freshness(&self) -> Freshness {
        self.freshness
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }
}

/// Bounded causal relation view for a decision (v4 §64–§65).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CausalRelationSnapshot {
    id: super::CausalRelationId,
    cause_kind: super::causal::PatternKind,
    cause_label: String,
    effect_kind: super::causal::PatternKind,
    effect_label: String,
    strength: f32,
    confidence: f32,
    scope: super::causal::CausalScope,
}

impl CausalRelationSnapshot {
    fn from_relation(relation: &super::causal::CausalRelation) -> Self {
        Self {
            id: relation.id(),
            cause_kind: relation.cause().kind(),
            cause_label: relation.cause().label().to_owned(),
            effect_kind: relation.effect().kind(),
            effect_label: relation.effect().label().to_owned(),
            strength: relation.strength(),
            confidence: relation.confidence(),
            scope: relation.scope(),
        }
    }

    pub fn validate(&self) -> Result<(), WorldValidationError> {
        super::common::validate_unit(self.strength, "snapshot causal strength")?;
        super::common::validate_unit(self.confidence, "snapshot causal confidence")?;
        Ok(())
    }

    #[must_use]
    pub const fn id(&self) -> super::CausalRelationId {
        self.id
    }

    #[must_use]
    pub const fn cause_kind(&self) -> super::causal::PatternKind {
        self.cause_kind
    }

    #[must_use]
    pub fn cause_label(&self) -> &str {
        &self.cause_label
    }

    #[must_use]
    pub const fn effect_kind(&self) -> super::causal::PatternKind {
        self.effect_kind
    }

    #[must_use]
    pub fn effect_label(&self) -> &str {
        &self.effect_label
    }

    #[must_use]
    pub const fn strength(&self) -> f32 {
        self.strength
    }

    #[must_use]
    pub const fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub fn scope(&self) -> super::causal::CausalScope {
        self.scope.clone()
    }
}

/// The bounded relevance-filtered view handed to the planner/executive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldModelSnapshot {
    entities: Vec<EntityStateSnapshot>,
    situations: Vec<SituationSnapshot>,
    hypotheses: Vec<HypothesisSnapshot>,
    causal: Vec<CausalRelationSnapshot>,
    social_scene: Option<SocialSceneSnapshot>,
    environment: EnvironmentSnapshot,
    temporal: Vec<TemporalSnapshotEntry>,
    uncertainties: Vec<WorldUncertaintySnapshot>,
    version: u64,
}

impl WorldModelSnapshot {
    pub fn validate(&self) -> Result<(), WorldValidationError> {
        let limits = WorldSnapshotLimits::default();
        if self.entities.len() > limits.entities() {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot entities",
                length: self.entities.len(),
                maximum: limits.entities(),
            });
        }
        if self.situations.len() > limits.situations() {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot situations",
                length: self.situations.len(),
                maximum: limits.situations(),
            });
        }
        if self.hypotheses.len() > limits.hypotheses() {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot hypotheses",
                length: self.hypotheses.len(),
                maximum: limits.hypotheses(),
            });
        }
        if self.causal.len() > limits.causal() {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot causal",
                length: self.causal.len(),
                maximum: limits.causal(),
            });
        }
        if self.temporal.len() > limits.temporal() {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot temporal",
                length: self.temporal.len(),
                maximum: limits.temporal(),
            });
        }
        if self.uncertainties.len() > limits.uncertainties() {
            return Err(WorldValidationError::TooManyItems {
                field: "snapshot uncertainties",
                length: self.uncertainties.len(),
                maximum: limits.uncertainties(),
            });
        }
        for entity in &self.entities {
            entity.validate()?;
        }
        for situation in &self.situations {
            situation.validate()?;
        }
        for hypothesis in &self.hypotheses {
            hypothesis.validate()?;
        }
        for causal in &self.causal {
            causal.validate()?;
        }
        if let Some(scene) = &self.social_scene {
            scene.validate()?;
        }
        self.environment.validate()?;
        if self.version == 0 {
            return Err(WorldValidationError::ZeroVersion);
        }
        Ok(())
    }

    #[must_use]
    pub fn entities(&self) -> &[EntityStateSnapshot] {
        &self.entities
    }

    #[must_use]
    pub fn situations(&self) -> &[SituationSnapshot] {
        &self.situations
    }

    #[must_use]
    pub fn hypotheses(&self) -> &[HypothesisSnapshot] {
        &self.hypotheses
    }

    #[must_use]
    pub fn causal(&self) -> &[CausalRelationSnapshot] {
        &self.causal
    }

    #[must_use]
    pub fn social_scene(&self) -> Option<&SocialSceneSnapshot> {
        self.social_scene.as_ref()
    }

    #[must_use]
    pub fn environment(&self) -> &EnvironmentSnapshot {
        &self.environment
    }

    #[must_use]
    pub fn temporal(&self) -> &[TemporalSnapshotEntry] {
        &self.temporal
    }

    #[must_use]
    pub fn uncertainties(&self) -> &[WorldUncertaintySnapshot] {
        &self.uncertainties
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Does the snapshot carry any "usable" state? (may be empty for a
    /// fresh world; callers must decide, not assume a non-empty world.)
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
            && self.situations.is_empty()
            && self.hypotheses.is_empty()
            && self.social_scene.is_none()
            && self.temporal.is_empty()
            && self.uncertainties.is_empty()
            && self.environment.hosts().is_empty()
            && self.environment.tools().is_empty()
    }
}

pub(super) fn build_snapshot(
    world: &WorldModel,
    context: &WorldSnapshotContext,
) -> Result<WorldModelSnapshot, WorldValidationError> {
    context.validate()?;
    let now = context.now();
    let limits = context.limits();

    // Entities: scope match (global Host/Tool state lives in the
    // environment snapshot instead), most recently observed first.
    let mut entities: Vec<_> = world
        .entities()
        .iter()
        .filter(|entity| {
            let is_environment_kind = matches!(entity.kind(), EntityKind::Host | EntityKind::Tool);
            context.matches_scope(entity.scope())
                && !(is_environment_kind && matches!(entity.scope(), WorldScope::Global))
        })
        .collect();
    entities.sort_by_key(|entity| std::cmp::Reverse(entity.last_observed_at()));
    let entity_ids: Vec<_> = entities
        .iter()
        .take(limits.entities())
        .map(|entity| entity.id())
        .collect();
    let entity_snapshots = entities
        .into_iter()
        .take(limits.entities())
        .map(|entity| EntityStateSnapshot::from_entity(entity, now))
        .collect();

    // Situations: active only, matched via conversation/person/goal/open-loop.
    let mut situations: Vec<_> = world
        .situations()
        .iter()
        .filter(|situation| {
            if !situation.is_active() {
                return false;
            }
            let matched_conversation =
                match (situation.conversation_id(), context.conversation_id()) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
            matched_conversation
                || situation
                    .persons()
                    .iter()
                    .any(|person| context.person_ids().contains(person))
                || situation
                    .related_goals()
                    .iter()
                    .any(|goal| context.goal_ids().contains(goal))
                || situation
                    .related_open_loops()
                    .iter()
                    .any(|loop_id| context.open_loop_ids().contains(loop_id))
        })
        .collect();
    situations.sort_by_key(|situation| std::cmp::Reverse(situation.updated_at()));
    let situation_ids: Vec<_> = situations
        .iter()
        .take(limits.situations())
        .map(|situation| situation.id())
        .collect();
    let situation_snapshots = situations
        .into_iter()
        .take(limits.situations())
        .map(SituationSnapshot::from_situation)
        .collect();

    // Hypotheses: active/supported, scope matched, not yet expired.
    let mut hypotheses: Vec<_> = world
        .hypotheses()
        .iter()
        .filter(|hypothesis| {
            matches!(
                hypothesis.status(),
                HypothesisStatus::Active | HypothesisStatus::Supported
            ) && hypothesis.freshness_at(now) != Freshness::Expired
                && context.matches_scope(hypothesis.scope())
        })
        .collect();
    hypotheses.sort_by(|a, b| b.confidence().total_cmp(&a.confidence()));
    let hypothesis_snapshots = hypotheses
        .into_iter()
        .take(limits.hypotheses())
        .map(|hypothesis| HypothesisSnapshot::from_hypothesis(hypothesis, now))
        .collect();

    // Social scene for the current conversation only.
    let social_scene = context
        .conversation_id()
        .and_then(|conversation_id| {
            world
                .social_scenes()
                .iter()
                .find(|scene| scene.conversation_id() == conversation_id)
        })
        .map(SocialSceneSnapshot::from_scene);

    // Environment snapshot (always presented, bounded).
    let environment = EnvironmentSnapshot::from_environment(world.environment(), now);

    // Timeline: entries pointing at selected entities/situations only.
    let mut temporal: Vec<_> = world
        .timeline()
        .iter()
        .filter(|entry| match entry.subject() {
            WorldRef::Entity(entity_id) => entity_ids.contains(&entity_id),
            WorldRef::Situation(situation_id) => situation_ids.contains(&situation_id),
        })
        .collect();
    temporal.sort_by_key(|entry| {
        std::cmp::Reverse(entry.interval().start().unwrap_or(DateTime::<Utc>::MIN_UTC))
    });
    let temporal_snapshots = temporal
        .into_iter()
        .take(limits.temporal())
        .map(TemporalSnapshotEntry::from_entry)
        .collect();

    // Uncertainties (not expired) matching scope.
    let mut uncertainties: Vec<_> = world
        .uncertainties()
        .iter()
        .filter(|uncertainty| {
            uncertainty.freshness_at(now) != Freshness::Expired
                && context.matches_scope(uncertainty.scope())
        })
        .collect();
    uncertainties.sort_by_key(|uncertainty| std::cmp::Reverse(uncertainty.observed_at()));
    let uncertainty_snapshots = uncertainties
        .into_iter()
        .take(limits.uncertainties())
        .map(|uncertainty| WorldUncertaintySnapshot::from_uncertainty(uncertainty, now))
        .collect();

    // Relevant causal knowledge: high-confidence relations matching scope,
    // confidence-descending (v4 §64, §136).
    let person_id = context.person_ids().first().copied();
    let causal = world
        .causal()
        .relevant(person_id, context.conversation_id(), limits.causal())
        .into_iter()
        .map(CausalRelationSnapshot::from_relation)
        .collect();

    let snapshot = WorldModelSnapshot {
        entities: entity_snapshots,
        situations: situation_snapshots,
        hypotheses: hypothesis_snapshots,
        causal,
        social_scene,
        environment,
        temporal: temporal_snapshots,
        uncertainties: uncertainty_snapshots,
        version: world.version(),
    };
    snapshot.validate()?;
    Ok(snapshot)
}
