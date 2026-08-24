//! Platform-neutral planning boundary.
//!
//! A planner turns a bounded snapshot of the Core world into a validated
//! high-level decision.  It does not execute actions and it does not know
//! which host will eventually deliver an intent.

use crate::arbiter::ActionCapability;
use crate::arbiter::ActionDescriptor;
use crate::event::WorldEvent;
use crate::identity::{ConversationId, OpenLoopId, PersonId};
use crate::intent::{CognitiveIntent, IntentValidationError};
use crate::memory::Memory;
use crate::open_loop::OpenLoop;
use crate::working_state::ConversationSnapshot;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

pub const MAX_PLANNER_INTENTS: usize = 32;
pub const MAX_PLANNER_STATE_UPDATES: usize = 32;
pub const MAX_PLANNER_MEMORIES: usize = 128;
pub const MAX_PLANNER_OPEN_LOOPS: usize = 128;
pub const MAX_PLANNER_TOPIC_CHARS: usize = 512;
pub const MAX_PLANNER_TOPIC_BYTES: usize = 2 * 1_024;

/// Slow, platform-neutral affect values made available to a planner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AffectState {
    pub valence: f32,
    pub arousal: f32,
    pub social_energy: f32,
    pub curiosity: f32,
}

impl Default for AffectState {
    fn default() -> Self {
        Self {
            valence: 0.0,
            arousal: 0.0,
            social_energy: 1.0,
            curiosity: 0.5,
        }
    }
}

impl AffectState {
    pub fn validate(self) -> Result<(), PlannerInputValidationError> {
        if !self.valence.is_finite()
            || !self.arousal.is_finite()
            || !self.social_energy.is_finite()
            || !self.curiosity.is_finite()
        {
            return Err(PlannerInputValidationError::NonFiniteAffect);
        }
        if !(-1.0..=1.0).contains(&self.valence)
            || !(-1.0..=1.0).contains(&self.arousal)
            || !(0.0..=1.0).contains(&self.social_energy)
            || !(0.0..=1.0).contains(&self.curiosity)
        {
            return Err(PlannerInputValidationError::AffectOutOfRange);
        }
        Ok(())
    }
}

/// Relation context keyed by the Core person identity, never by a host ID.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RelationState {
    pub person_id: PersonId,
    pub familiarity: f32,
    pub affinity: f32,
    pub trust: f32,
    pub comfort: f32,
    pub tension: f32,
}

impl RelationState {
    pub fn new(person_id: PersonId) -> Self {
        Self {
            person_id,
            familiarity: 0.0,
            affinity: 0.0,
            trust: 0.0,
            comfort: 0.0,
            tension: 0.0,
        }
    }

    pub fn validate(self) -> Result<(), PlannerInputValidationError> {
        let values = [
            self.familiarity,
            self.affinity,
            self.trust,
            self.comfort,
            self.tension,
        ];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(PlannerInputValidationError::NonFiniteRelation);
        }
        if values.iter().any(|value| !(-1.0..=1.0).contains(value)) {
            return Err(PlannerInputValidationError::RelationOutOfRange);
        }
        Ok(())
    }
}

/// The bounded working-state view exposed to a model.  Keeping the complete
/// mutable [`WorkingState`](crate::WorkingState) out of this type prevents a
/// planner from retaining a lock or mutating runtime state after a call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerStateSnapshot {
    pub global_version: u64,
    pub conversation: Option<ConversationSnapshot>,
}

impl PlannerStateSnapshot {
    #[must_use]
    pub const fn new(global_version: u64, conversation: Option<ConversationSnapshot>) -> Self {
        Self {
            global_version,
            conversation,
        }
    }

    #[must_use]
    pub const fn empty() -> Self {
        Self {
            global_version: 0,
            conversation: None,
        }
    }

    #[must_use]
    pub fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation
            .as_ref()
            .map(ConversationSnapshot::conversation_id)
    }
}

impl Default for PlannerStateSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

/// All context needed for one planning turn.  The event is included in full,
/// while durable context is bounded by the stores and the caller's retrieval
/// policy before it reaches a model backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerInput {
    pub event: WorldEvent,
    pub state: PlannerStateSnapshot,
    pub memories: Vec<Memory>,
    pub open_loops: Vec<OpenLoop>,
    pub relation: Option<RelationState>,
    pub affect: AffectState,
    pub capabilities: Vec<ActionDescriptor>,
}

impl PlannerInput {
    #[must_use]
    pub fn new(event: WorldEvent, state: PlannerStateSnapshot) -> Self {
        Self {
            event,
            state,
            memories: Vec::new(),
            open_loops: Vec::new(),
            relation: None,
            affect: AffectState::default(),
            capabilities: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_memories(mut self, memories: Vec<Memory>) -> Self {
        self.memories = memories;
        self
    }

    #[must_use]
    pub fn with_open_loops(mut self, open_loops: Vec<OpenLoop>) -> Self {
        self.open_loops = open_loops;
        self
    }

    #[must_use]
    pub fn with_relation(mut self, relation: Option<RelationState>) -> Self {
        self.relation = relation;
        self
    }

    #[must_use]
    pub fn with_affect(mut self, affect: AffectState) -> Self {
        self.affect = affect;
        self
    }

    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<ActionDescriptor>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn validate(&self, max_trace_depth: u8) -> Result<(), PlannerInputValidationError> {
        self.event
            .validate(max_trace_depth)
            .map_err(PlannerInputValidationError::InvalidEvent)?;
        if self.memories.len() > MAX_PLANNER_MEMORIES {
            return Err(PlannerInputValidationError::TooManyMemories {
                length: self.memories.len(),
                maximum: MAX_PLANNER_MEMORIES,
            });
        }
        if self.open_loops.len() > MAX_PLANNER_OPEN_LOOPS {
            return Err(PlannerInputValidationError::TooManyOpenLoops {
                length: self.open_loops.len(),
                maximum: MAX_PLANNER_OPEN_LOOPS,
            });
        }
        self.affect.validate()?;
        if let Some(relation) = self.relation {
            relation.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn supports(&self, capability: ActionCapability) -> bool {
        let scope = self
            .state
            .conversation_id()
            .map(crate::ActionScope::Conversation)
            .unwrap_or(crate::ActionScope::Global);
        self.capabilities
            .iter()
            .any(|descriptor| descriptor.capability == capability && descriptor.allows(scope))
    }
}

/// The planner's disposition is intentionally less specific than an action.
/// A host only sees an action after an intent has passed the arbiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionDisposition {
    Reply,
    Silent,
    Defer,
    ReactOnly,
    AskQuestion,
    ChangeTopic,
    ResumeAgenda,
    SpecialAction,
}

impl Default for DecisionDisposition {
    fn default() -> Self {
        Self::Silent
    }
}

impl DecisionDisposition {
    /// Compatibility spelling for callers that use "respond" in their
    /// product language while the wire representation remains `reply`.
    #[allow(non_upper_case_globals)]
    pub const Respond: Self = Self::Reply;
    #[allow(non_upper_case_globals)]
    pub const Ignore: Self = Self::Silent;
}

/// A bounded, declarative update requested alongside a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum StateUpdateProposal {
    Affect(AffectState),
    SetTopic {
        conversation_id: ConversationId,
        topic: String,
    },
    ResolveOpenLoop {
        open_loop_id: OpenLoopId,
    },
}

impl StateUpdateProposal {
    pub fn validate(&self) -> Result<(), PlannerOutputValidationError> {
        match self {
            Self::Affect(affect) => affect
                .validate()
                .map_err(PlannerOutputValidationError::InvalidAffect),
            Self::SetTopic { topic, .. } => validate_topic(topic),
            Self::ResolveOpenLoop { .. } => Ok(()),
        }
    }
}

/// High-level planner output.  It contains no adapter call and no side
/// effect; conversion to [`ProposedAction`](crate::ProposedAction) is a later
/// boundary that remains under arbiter policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionPlan {
    pub disposition: DecisionDisposition,
    pub intents: Vec<CognitiveIntent>,
    pub state_updates: Vec<StateUpdateProposal>,
}

/// Alternate name used by integrations that speak in terms of input/output.
pub type PlannerOutput = DecisionPlan;

impl DecisionPlan {
    #[must_use]
    pub const fn new(disposition: DecisionDisposition) -> Self {
        Self {
            disposition,
            intents: Vec::new(),
            state_updates: Vec::new(),
        }
    }

    #[must_use]
    pub const fn silent() -> Self {
        Self {
            disposition: DecisionDisposition::Silent,
            intents: Vec::new(),
            state_updates: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), PlannerOutputValidationError> {
        if self.intents.len() > MAX_PLANNER_INTENTS {
            return Err(PlannerOutputValidationError::TooManyIntents {
                length: self.intents.len(),
                maximum: MAX_PLANNER_INTENTS,
            });
        }
        if self.state_updates.len() > MAX_PLANNER_STATE_UPDATES {
            return Err(PlannerOutputValidationError::TooManyStateUpdates {
                length: self.state_updates.len(),
                maximum: MAX_PLANNER_STATE_UPDATES,
            });
        }
        for intent in &self.intents {
            intent
                .validate()
                .map_err(PlannerOutputValidationError::InvalidIntent)?;
        }
        for update in &self.state_updates {
            update.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn with_intent(mut self, intent: CognitiveIntent) -> Self {
        self.intents.push(intent);
        self
    }

    #[must_use]
    pub fn with_state_update(mut self, update: StateUpdateProposal) -> Self {
        self.state_updates.push(update);
        self
    }
}

pub type ModelBackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<PlannerOutput, ModelBackendError>> + Send + 'a>>;

/// Platform-neutral model boundary.  An implementation may use a local
/// model, a remote service, or a deterministic policy engine.  It receives
/// typed Core data and returns a declarative plan only.
pub trait ModelBackend: Send + Sync {
    /// Produces one declarative plan.  Backends that use a different
    /// vocabulary can implement [`Self::complete`] instead; the default
    /// methods keep both names available without coupling Core to a vendor.
    fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        Box::pin(async { Err(ModelBackendError::Unavailable) })
    }

    fn complete<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        self.plan(input)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelBackendError {
    #[error("model backend is unavailable")]
    Unavailable,
    #[error("model backend returned an invalid plan: {reason}")]
    InvalidPlan { reason: String },
    #[error("model backend failed: {message}")]
    Failed { message: String, retryable: bool },
}

impl ModelBackendError {
    #[must_use]
    pub fn failed(message: impl Into<String>, retryable: bool) -> Self {
        Self::Failed {
            message: message.into(),
            retryable,
        }
    }
}

/// Planner orchestration and output validation.  The planner owns no runtime
/// state, so the same instance can safely be shared by multiple runtimes.
#[derive(Clone)]
pub struct Planner {
    model: Arc<dyn ModelBackend>,
    max_trace_depth: u8,
}

impl std::fmt::Debug for Planner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Planner")
            .field("max_trace_depth", &self.max_trace_depth)
            .finish_non_exhaustive()
    }
}

impl Planner {
    #[must_use]
    pub fn new(model: Arc<dyn ModelBackend>) -> Self {
        Self {
            model,
            max_trace_depth: u8::MAX,
        }
    }

    #[must_use]
    pub fn from_model<M>(model: M) -> Self
    where
        M: ModelBackend + 'static,
    {
        Self::new(Arc::new(model))
    }

    #[must_use]
    pub fn with_max_trace_depth(mut self, max_trace_depth: u8) -> Self {
        self.max_trace_depth = max_trace_depth;
        self
    }

    #[must_use]
    pub fn model(&self) -> &Arc<dyn ModelBackend> {
        &self.model
    }

    pub async fn plan(&self, input: &PlannerInput) -> Result<PlannerOutput, PlannerError> {
        input
            .validate(self.max_trace_depth)
            .map_err(PlannerError::InvalidInput)?;
        let output = self
            .model
            .complete(input)
            .await
            .map_err(PlannerError::Model)?;
        output.validate().map_err(PlannerError::InvalidOutput)?;
        Ok(output)
    }
}

#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("planner input is invalid: {0}")]
    InvalidInput(PlannerInputValidationError),
    #[error(transparent)]
    Model(#[from] ModelBackendError),
    #[error("planner output is invalid: {0}")]
    InvalidOutput(PlannerOutputValidationError),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlannerInputValidationError {
    #[error("world event is invalid: {0}")]
    InvalidEvent(crate::EventValidationError),
    #[error("planner received too many memories: {length}, maximum {maximum}")]
    TooManyMemories { length: usize, maximum: usize },
    #[error("planner received too many open loops: {length}, maximum {maximum}")]
    TooManyOpenLoops { length: usize, maximum: usize },
    #[error("affect contains a non-finite value")]
    NonFiniteAffect,
    #[error("affect value is outside its supported range")]
    AffectOutOfRange,
    #[error("relation contains a non-finite value")]
    NonFiniteRelation,
    #[error("relation value is outside its supported range")]
    RelationOutOfRange,
}

#[derive(Debug, Error)]
pub enum PlannerOutputValidationError {
    #[error("planner returned too many intents: {length}, maximum {maximum}")]
    TooManyIntents { length: usize, maximum: usize },
    #[error("planner returned too many state updates: {length}, maximum {maximum}")]
    TooManyStateUpdates { length: usize, maximum: usize },
    #[error("planner returned an invalid intent: {0}")]
    InvalidIntent(#[from] IntentValidationError),
    #[error("planner returned an invalid affect update: {0}")]
    InvalidAffect(PlannerInputValidationError),
    #[error("planner returned an invalid topic: {0}")]
    InvalidTopic(String),
}

fn validate_topic(topic: &str) -> Result<(), PlannerOutputValidationError> {
    if topic.trim().is_empty() {
        return Err(PlannerOutputValidationError::InvalidTopic(
            "topic must not be empty".to_owned(),
        ));
    }
    if topic.as_bytes().contains(&0) {
        return Err(PlannerOutputValidationError::InvalidTopic(
            "topic must not contain NUL".to_owned(),
        ));
    }
    if topic.len() > MAX_PLANNER_TOPIC_BYTES {
        return Err(PlannerOutputValidationError::InvalidTopic(format!(
            "topic is {} bytes, maximum {}",
            topic.len(),
            MAX_PLANNER_TOPIC_BYTES
        )));
    }
    if topic.chars().count() > MAX_PLANNER_TOPIC_CHARS {
        return Err(PlannerOutputValidationError::InvalidTopic(format!(
            "topic is too long, maximum {} characters",
            MAX_PLANNER_TOPIC_CHARS
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventPriority, EventScope, WorldEventKind};
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn input() -> PlannerInput {
        PlannerInput::new(
            WorldEvent::new(
                Utc::now(),
                EventScope::Global,
                EventPriority::High,
                WorldEventKind::IdleTick,
            ),
            PlannerStateSnapshot::empty(),
        )
    }

    struct FakeModel {
        calls: AtomicUsize,
        output: DecisionPlan,
    }

    impl ModelBackend for FakeModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let output = self.output.clone();
            Box::pin(async move { Ok(output) })
        }
    }

    #[tokio::test]
    async fn planner_calls_fake_model_and_validates_declarative_output() {
        let model = Arc::new(FakeModel {
            calls: AtomicUsize::new(0),
            output: DecisionPlan {
                disposition: DecisionDisposition::Silent,
                intents: vec![CognitiveIntent::noop()],
                state_updates: Vec::new(),
            },
        });
        let planner = Planner::new(model.clone());
        let output = planner.plan(&input()).await.expect("valid plan");
        assert_eq!(output.disposition, DecisionDisposition::Silent);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn planner_rejects_invalid_model_intent_before_host_execution() {
        let model = Arc::new(FakeModel {
            calls: AtomicUsize::new(0),
            output: DecisionPlan {
                disposition: DecisionDisposition::Reply,
                intents: vec![CognitiveIntent::send_message(
                    crate::ConversationId::new(),
                    crate::MessageContent::text(""),
                )],
                state_updates: Vec::new(),
            },
        });
        let error = Planner::new(model)
            .plan(&input())
            .await
            .expect_err("invalid intent must be rejected");
        assert!(matches!(error, PlannerError::InvalidOutput(_)));
    }

    #[test]
    fn affect_and_relation_ranges_are_checked() {
        assert!(
            AffectState {
                valence: 2.0,
                ..AffectState::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RelationState {
                person_id: PersonId::new(),
                tension: f32::NAN,
                ..RelationState::new(PersonId::new())
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn planner_input_serialization_keeps_host_capabilities() {
        let input =
            input().with_capabilities(vec![ActionDescriptor::new(ActionCapability::SendMessage)]);
        let encoded = serde_json::to_value(&input).expect("planner input should serialize");
        assert_eq!(
            encoded["capabilities"][0]["capability"],
            serde_json::json!("send_message")
        );
        let decoded: PlannerInput = serde_json::from_value(encoded).expect("round trip");
        assert_eq!(decoded.capabilities, input.capabilities);
    }
}
