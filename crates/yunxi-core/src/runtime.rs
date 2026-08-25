use crate::arbiter::{ActionArbiter, ActionPort, ActionResult};
use crate::attention::{AttentionResult, AttentionSystem};
use crate::event::{EventPriority, EventScope, EventType, EventValidationError, WorldEvent};
use crate::identity::{ConversationId, EventId, PersonId};
use crate::memory::{MemoryQuery, MemoryScope};
use crate::open_loop::OpenLoopOwner;
use crate::planner::{
    DecisionPlan, Planner, PlannerError, PlannerInput, PlannerOutputValidationError,
    PlannerStateSnapshot, StateUpdateProposal,
};
use crate::ports::CoreServices;
use crate::working_state::{
    StateUpdate, WorkingState, WorkingStateConfig, WorkingStateConfigError, WorkingStateError,
};
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

const MAX_EVENT_QUEUE_CAPACITY: usize = 4_096;
pub const MAX_DATA_ERASURE_CONVERSATIONS: usize = 256;
pub const MAX_BLOCKED_DATA_ERASURE_PEOPLE: usize = 256;
pub const MAX_BLOCKED_DATA_ERASURE_CONVERSATIONS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub event_queue_capacity: usize,
    pub max_trace_depth: u8,
    pub working_state: WorkingStateConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_queue_capacity: 256,
            max_trace_depth: 8,
            working_state: WorkingStateConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RuntimeConfigError {
    #[error("event queue capacity must be greater than zero")]
    ZeroQueueCapacity,
    #[error(
        "event queue capacity {value} is above maximum {maximum}",
        maximum = MAX_EVENT_QUEUE_CAPACITY
    )]
    QueueCapacityTooLarge { value: usize },
    #[error(transparent)]
    WorkingState(#[from] WorkingStateConfigError),
}

#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    sender: mpsc::Sender<RuntimeCommand>,
    max_trace_depth: u8,
}

impl RuntimeHandle {
    pub async fn submit(&self, event: WorldEvent) -> Result<Admission, SubmitError> {
        if let Err(error) = event.validate(self.max_trace_depth) {
            return Err(SubmitError::InvalidEvent { event, error });
        }
        if event.priority().requires_backpressure() {
            self.sender
                .send(RuntimeCommand::Event(event))
                .await
                .map(|()| Admission::Accepted)
                .map_err(|error| match error.0 {
                    RuntimeCommand::Event(event) => SubmitError::RuntimeClosed(event),
                    RuntimeCommand::BeginDataErasure { .. }
                    | RuntimeCommand::EndDataErasure { .. } => {
                        unreachable!("submit only sends event commands")
                    }
                })
        } else {
            match self.sender.try_send(RuntimeCommand::Event(event)) {
                Ok(()) => Ok(Admission::Accepted),
                Err(mpsc::error::TrySendError::Full(_)) => Ok(Admission::DroppedAtCapacity),
                Err(mpsc::error::TrySendError::Closed(RuntimeCommand::Event(event))) => {
                    Err(SubmitError::RuntimeClosed(event))
                }
                Err(mpsc::error::TrySendError::Closed(
                    RuntimeCommand::BeginDataErasure { .. } | RuntimeCommand::EndDataErasure { .. },
                )) => unreachable!("submit only sends event commands"),
            }
        }
    }

    /// Establishes a FIFO data-erasure barrier for one canonical person.
    ///
    /// When the acknowledgement arrives, every earlier queued event and active
    /// planner/action turn has completed, the supplied direct-conversation
    /// snapshots have been removed, and later matching queued events will be
    /// discarded until [`Self::end_data_erasure`] is acknowledged. Hosts must
    /// still block their external identity ingress so a deleted identity cannot
    /// be resolved into a new Core identifier during the erasure window.
    pub async fn begin_data_erasure<I>(
        &self,
        person_id: PersonId,
        direct_conversation_ids: I,
    ) -> Result<usize, DataErasureError>
    where
        I: IntoIterator<Item = ConversationId>,
    {
        let conversation_ids = bounded_conversation_ids(direct_conversation_ids)?;
        let (acknowledge, acknowledged) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::BeginDataErasure {
                person_id,
                conversation_ids,
                acknowledge,
            })
            .await
            .map_err(|_| DataErasureError::RuntimeClosed)?;
        acknowledged
            .await
            .map_err(|_| DataErasureError::AcknowledgementDropped)?
    }

    /// Releases the scopes installed by [`Self::begin_data_erasure`]. The end
    /// command shares the event FIFO, so its acknowledgement also confirms that
    /// all events submitted during the blocked window have been discarded.
    pub async fn end_data_erasure(&self, person_id: PersonId) -> Result<bool, DataErasureError> {
        let (acknowledge, acknowledged) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::EndDataErasure {
                person_id,
                acknowledge,
            })
            .await
            .map_err(|_| DataErasureError::RuntimeClosed)?;
        acknowledged
            .await
            .map_err(|_| DataErasureError::AcknowledgementDropped)
    }
}

fn bounded_conversation_ids<I>(conversation_ids: I) -> Result<Vec<ConversationId>, DataErasureError>
where
    I: IntoIterator<Item = ConversationId>,
{
    let mut bounded = Vec::new();
    for conversation_id in conversation_ids {
        if bounded.contains(&conversation_id) {
            continue;
        }
        if bounded.len() >= MAX_DATA_ERASURE_CONVERSATIONS {
            return Err(DataErasureError::TooManyConversations {
                maximum: MAX_DATA_ERASURE_CONVERSATIONS,
            });
        }
        bounded.push(conversation_id);
    }
    Ok(bounded)
}

#[derive(Debug)]
enum RuntimeCommand {
    Event(WorldEvent),
    BeginDataErasure {
        person_id: PersonId,
        conversation_ids: Vec<ConversationId>,
        acknowledge: oneshot::Sender<Result<usize, DataErasureError>>,
    },
    EndDataErasure {
        person_id: PersonId,
        acknowledge: oneshot::Sender<bool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DataErasureError {
    #[error("data-erasure request exceeds the maximum of {maximum} direct conversations")]
    TooManyConversations { maximum: usize },
    #[error("data erasure is already active for person {person_id}")]
    AlreadyActive { person_id: PersonId },
    #[error("direct conversation {conversation_id} is already blocked by another erasure")]
    ConversationAlreadyBlocked { conversation_id: ConversationId },
    #[error("data-erasure blocked-person capacity is full (maximum {maximum})")]
    BlockedPeopleCapacity { maximum: usize },
    #[error("data-erasure blocked-conversation capacity is full (maximum {maximum})")]
    BlockedConversationsCapacity { maximum: usize },
    #[error("cognitive runtime is closed")]
    RuntimeClosed,
    #[error("cognitive runtime dropped the data-erasure acknowledgement")]
    AcknowledgementDropped,
    #[error(transparent)]
    WorkingState(#[from] WorkingStateError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    DroppedAtCapacity,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SubmitError {
    #[error("cognitive runtime rejected an invalid event: {error}")]
    InvalidEvent {
        event: WorldEvent,
        error: EventValidationError,
    },
    #[error("cognitive runtime is closed")]
    RuntimeClosed(WorldEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeObservation {
    pub event_id: EventId,
    pub event_type: EventType,
    pub scope: EventScope,
    pub priority: EventPriority,
    pub attention: AttentionResult,
    pub state: StateUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingOutcome {
    Observed(RuntimeObservation),
    RejectedEvent {
        event: WorldEvent,
        error: EventValidationError,
    },
    RejectedState {
        event: WorldEvent,
        error: WorkingStateError,
    },
}

/// Result of a runtime turn that may invoke the optional Core Planner.
/// Ignore and observe-only events carry a synthetic silent plan so hosts can
/// keep one outcome shape without spending a model call. Actual planning
/// remains declarative: intents still need ActionArbiter admission before a
/// host side effect occurs.
#[derive(Debug)]
pub enum PlannedProcessingOutcome {
    Planned {
        observation: RuntimeObservation,
        plan: DecisionPlan,
        actions: Vec<ActionResult>,
        feedback: Vec<RuntimeObservation>,
    },
    RejectedEvent {
        event: WorldEvent,
        error: EventValidationError,
    },
    RejectedState {
        event: WorldEvent,
        error: WorkingStateError,
    },
}

#[derive(Debug)]
pub struct CognitiveRuntime {
    receiver: mpsc::Receiver<RuntimeCommand>,
    state: WorkingState,
    data_erasure: DataErasureState,
    attention: AttentionSystem,
    max_trace_depth: u8,
    planner: Option<Planner>,
    services: Option<Arc<CoreServices>>,
}

#[derive(Debug, Default)]
struct DataErasureState {
    conversations_by_person: HashMap<PersonId, Vec<ConversationId>>,
    blocked_conversations: HashSet<ConversationId>,
}

impl DataErasureState {
    fn begin(
        &mut self,
        state: &mut WorkingState,
        person_id: PersonId,
        conversation_ids: Vec<ConversationId>,
    ) -> Result<usize, DataErasureError> {
        if self.conversations_by_person.contains_key(&person_id) {
            return Err(DataErasureError::AlreadyActive { person_id });
        }
        if self.conversations_by_person.len() >= MAX_BLOCKED_DATA_ERASURE_PEOPLE {
            return Err(DataErasureError::BlockedPeopleCapacity {
                maximum: MAX_BLOCKED_DATA_ERASURE_PEOPLE,
            });
        }
        if let Some(conversation_id) = conversation_ids
            .iter()
            .find(|conversation_id| self.blocked_conversations.contains(conversation_id))
        {
            return Err(DataErasureError::ConversationAlreadyBlocked {
                conversation_id: *conversation_id,
            });
        }
        if self.blocked_conversations.len() + conversation_ids.len()
            > MAX_BLOCKED_DATA_ERASURE_CONVERSATIONS
        {
            return Err(DataErasureError::BlockedConversationsCapacity {
                maximum: MAX_BLOCKED_DATA_ERASURE_CONVERSATIONS,
            });
        }

        let removed = state.purge_person_domain(person_id, &conversation_ids)?;
        self.blocked_conversations
            .extend(conversation_ids.iter().copied());
        self.conversations_by_person
            .insert(person_id, conversation_ids);
        Ok(removed)
    }

    fn end(&mut self, person_id: PersonId) -> bool {
        let Some(conversation_ids) = self.conversations_by_person.remove(&person_id) else {
            return false;
        };
        for conversation_id in conversation_ids {
            self.blocked_conversations.remove(&conversation_id);
        }
        true
    }

    fn blocks(&self, event: &WorldEvent) -> bool {
        match event.scope() {
            EventScope::Person { person_id }
                if self.conversations_by_person.contains_key(&person_id) =>
            {
                true
            }
            EventScope::Conversation { conversation_id }
                if self.blocked_conversations.contains(&conversation_id) =>
            {
                true
            }
            _ => event_person_id(event)
                .is_some_and(|person_id| self.conversations_by_person.contains_key(&person_id)),
        }
    }
}

impl CognitiveRuntime {
    pub fn new(config: RuntimeConfig) -> Result<(RuntimeHandle, Self), RuntimeConfigError> {
        if config.event_queue_capacity == 0 {
            return Err(RuntimeConfigError::ZeroQueueCapacity);
        }
        if config.event_queue_capacity > MAX_EVENT_QUEUE_CAPACITY {
            return Err(RuntimeConfigError::QueueCapacityTooLarge {
                value: config.event_queue_capacity,
            });
        }
        let state = WorkingState::new(config.working_state)?;
        let (sender, receiver) = mpsc::channel(config.event_queue_capacity);
        Ok((
            RuntimeHandle {
                sender,
                max_trace_depth: config.max_trace_depth,
            },
            Self {
                receiver,
                state,
                data_erasure: DataErasureState::default(),
                attention: AttentionSystem,
                max_trace_depth: config.max_trace_depth,
                planner: None,
                services: None,
            },
        ))
    }

    /// Creates a runtime with the model and optional Core service ports.  The
    /// original [`Self::new`] constructor remains observe-only for hosts that
    /// have not installed a model backend yet.
    pub fn new_with_services(
        config: RuntimeConfig,
        services: CoreServices,
    ) -> Result<(RuntimeHandle, Self), RuntimeConfigError> {
        let (handle, mut runtime) = Self::new(config)?;
        runtime.install_services(services);
        Ok((handle, runtime))
    }

    /// Installs a planner without changing the bounded event queue or state.
    #[must_use]
    pub fn with_planner(mut self, planner: Planner) -> Self {
        self.planner = Some(planner);
        self
    }

    /// Installs Core services and uses their model backend for planning.
    pub fn install_services(&mut self, services: CoreServices) {
        let planner =
            Planner::new(services.model.clone()).with_max_trace_depth(self.max_trace_depth);
        self.planner = Some(planner);
        self.services = Some(Arc::new(services));
    }

    #[must_use]
    pub fn planner(&self) -> Option<&Planner> {
        self.planner.as_ref()
    }

    #[must_use]
    pub fn services(&self) -> Option<&CoreServices> {
        self.services.as_deref()
    }

    pub async fn process_next(&mut self) -> Option<ProcessingOutcome> {
        let event = self.next_event().await?;
        Some(self.process_event(event))
    }

    async fn next_event(&mut self) -> Option<WorldEvent> {
        loop {
            match self.receiver.recv().await? {
                RuntimeCommand::Event(event) => {
                    if !self.data_erasure.blocks(&event) {
                        return Some(event);
                    }
                }
                RuntimeCommand::BeginDataErasure {
                    person_id,
                    conversation_ids,
                    acknowledge,
                } => {
                    let result =
                        self.data_erasure
                            .begin(&mut self.state, person_id, conversation_ids);
                    let _ = acknowledge.send(result);
                }
                RuntimeCommand::EndDataErasure {
                    person_id,
                    acknowledge,
                } => {
                    let _ = acknowledge.send(self.data_erasure.end(person_id));
                }
            }
        }
    }

    pub fn process_event(&mut self, event: WorldEvent) -> ProcessingOutcome {
        if let Err(error) = event.validate(self.max_trace_depth) {
            return ProcessingOutcome::RejectedEvent { event, error };
        }

        let attention = self.attention.evaluate(&event);
        let state = match self.state.observe(&event, attention) {
            Ok(state) => state,
            Err(error) => {
                return ProcessingOutcome::RejectedState { event, error };
            }
        };
        ProcessingOutcome::Observed(RuntimeObservation {
            event_id: event.id(),
            event_type: event.kind().event_type(),
            scope: event.scope(),
            priority: event.priority(),
            attention,
            state,
        })
    }

    /// Processes one event and invokes the installed planner only when the
    /// attention result is `Attend` or `MustHandle`.
    pub async fn process_event_with_planner(
        &mut self,
        event: WorldEvent,
    ) -> Result<PlannedProcessingOutcome, PlannerError> {
        let planner_event = event.clone();
        let observed = self.process_event(event);
        let ProcessingOutcome::Observed(observation) = observed else {
            return Ok(match observed {
                ProcessingOutcome::RejectedEvent { event, error } => {
                    PlannedProcessingOutcome::RejectedEvent { event, error }
                }
                ProcessingOutcome::RejectedState { event, error } => {
                    PlannedProcessingOutcome::RejectedState { event, error }
                }
                ProcessingOutcome::Observed(_) => unreachable!("observed outcome matched above"),
            });
        };
        if !observation.attention.should_invoke_planner() {
            return Ok(observed_without_planning(observation));
        }
        let planner = self
            .planner
            .as_ref()
            .ok_or(PlannerError::Model(crate::ModelBackendError::Unavailable))?;
        let input = self
            .planner_input_with_context(planner_event.clone())
            .await
            .with_capabilities(Vec::new());
        validate_due_open_loop_context(&input)?;
        let plan = planner.plan(&input).await?;
        validate_intent_targets(&planner_event, &plan)?;
        let deferred_due_resolution = deferred_due_open_loop_resolution(&planner_event, &plan);
        self.apply_state_updates(&input, &plan, deferred_due_resolution)
            .await?;
        Ok(PlannedProcessingOutcome::Planned {
            observation,
            plan,
            actions: Vec::new(),
            feedback: Vec::new(),
        })
    }

    /// Runs one complete Core decision turn. Every intent is converted to a
    /// proposed action, admitted by the arbiter, executed by the host port,
    /// and represented as a derived WorldEvent that is observed by this
    /// runtime. The method deliberately does not call the planner again for
    /// feedback events, preventing action-result loops.
    pub async fn process_event_with_planner_and_actions(
        &mut self,
        event: WorldEvent,
        arbiter: &ActionArbiter,
        port: &dyn ActionPort,
    ) -> Result<PlannedProcessingOutcome, PlannerError> {
        let planner_event = event.clone();
        let observed = self.process_event(event);
        let observation = match observed {
            ProcessingOutcome::Observed(observation) => observation,
            ProcessingOutcome::RejectedEvent { event, error } => {
                return Ok(PlannedProcessingOutcome::RejectedEvent { event, error });
            }
            ProcessingOutcome::RejectedState { event, error } => {
                return Ok(PlannedProcessingOutcome::RejectedState { event, error });
            }
        };
        if !observation.attention.should_invoke_planner() {
            return Ok(observed_without_planning(observation));
        }
        let planner = self
            .planner
            .as_ref()
            .ok_or(PlannerError::Model(crate::ModelBackendError::Unavailable))?;
        let capabilities = arbiter.config().capabilities.actions().to_vec();
        let input = self
            .planner_input_with_context(planner_event.clone())
            .await
            .with_capabilities(capabilities);
        validate_due_open_loop_context(&input)?;
        let plan = planner.plan(&input).await?;
        validate_intent_targets(&planner_event, &plan)?;
        let due_open_loop = due_open_loop_id(&planner_event);
        let deferred_due_resolution = deferred_due_open_loop_resolution(&planner_event, &plan);
        let applied_state_updates = self
            .apply_state_updates(&input, &plan, deferred_due_resolution)
            .await?;
        let mut all_due_deliveries_succeeded = true;
        let mut actions = Vec::with_capacity(plan.intents.len());
        let mut feedback = Vec::new();
        for (intent_index, intent) in plan.intents.iter().enumerate() {
            let mut proposed = intent.propose_action().map_err(|error| {
                PlannerError::InvalidOutput(PlannerOutputValidationError::InvalidIntent(error))
            })?;
            if let Some(open_loop_id) = due_open_loop {
                apply_due_action_idempotency(&mut proposed, open_loop_id, intent_index).map_err(
                    |error| {
                        PlannerError::InvalidOutput(PlannerOutputValidationError::InvalidIntent(
                            crate::IntentValidationError::Action(error),
                        ))
                    },
                )?;
            }
            let result = arbiter.dispatch(proposed.clone(), port).await;
            let replay_of_delivered_action = matches!(
                &result,
                ActionResult::Rejected(crate::ActionRejection::Duplicate {
                    idempotency_key,
                    original_action_id,
                    ..
                }) if arbiter.was_delivered(idempotency_key, *original_action_id)
            );
            if deferred_due_resolution.is_some()
                && !matches!(&proposed, crate::ProposedAction::Noop)
                && !(matches!(
                    &result,
                    ActionResult::Executed {
                        outcome: crate::ActionPortOutcome::Delivered { .. },
                        ..
                    }
                ) || replay_of_delivered_action)
            {
                all_due_deliveries_succeeded = false;
            }
            if let Some(feedback_event) =
                action_result_event(&planner_event, &proposed, &result, self.max_trace_depth)
                && let ProcessingOutcome::Observed(feedback_observation) =
                    self.process_event(feedback_event)
            {
                feedback.push(feedback_observation);
            }
            if let Some(sent_event) =
                message_sent_event(&planner_event, &proposed, &result, self.max_trace_depth)
                && let ProcessingOutcome::Observed(feedback_observation) =
                    self.process_event(sent_event)
            {
                feedback.push(feedback_observation);
            }
            actions.push(result);
        }
        if deferred_due_resolution.is_some() && all_due_deliveries_succeeded {
            self.resolve_due_open_loop(&input, applied_state_updates)
                .await?;
        }
        Ok(PlannedProcessingOutcome::Planned {
            observation,
            plan,
            actions,
            feedback,
        })
    }

    /// Queue variant of [`Self::process_event_with_planner`].
    pub async fn process_next_with_planner(
        &mut self,
    ) -> Option<Result<PlannedProcessingOutcome, PlannerError>> {
        let event = self.next_event().await?;
        Some(self.process_event_with_planner(event).await)
    }

    /// Queue variant of [`Self::process_event_with_planner_and_actions`].
    pub async fn process_next_with_planner_and_actions(
        &mut self,
        arbiter: &ActionArbiter,
        port: &dyn ActionPort,
    ) -> Option<Result<PlannedProcessingOutcome, PlannerError>> {
        let event = self.next_event().await?;
        Some(
            self.process_event_with_planner_and_actions(event, arbiter, port)
                .await,
        )
    }

    /// Builds a planner context from the current runtime state.  Hosts that
    /// have retrieved durable memories/open loops can extend the returned
    /// input before calling [`Planner::plan`].
    pub fn planner_input(&self, event: WorldEvent) -> PlannerInput {
        let conversation = event
            .scope()
            .conversation_id()
            .and_then(|conversation_id| self.state.conversation(conversation_id));
        PlannerInput::new(
            event,
            PlannerStateSnapshot::new(self.state.global_version(), conversation),
        )
    }

    /// Builds a planner input and opportunistically hydrates bounded durable
    /// context from the installed Core service ports. Storage failures are
    /// intentionally non-fatal during migration: a host can bring the new
    /// runtime online before every legacy store has been moved.
    pub async fn planner_input_with_context(&self, event: WorldEvent) -> PlannerInput {
        let Some(services) = self.services.as_ref() else {
            return self.planner_input(event);
        };

        let mut input = self.planner_input(event.clone());
        let conversation_id = event.scope().conversation_id();
        let person_id = event_person_id(&event).or_else(|| match event.scope() {
            EventScope::Person { person_id } => Some(person_id),
            _ => None,
        });

        let mut memories = Vec::new();
        let mut open_loops = Vec::new();

        if let Some(conversation_id) = conversation_id {
            let scope = MemoryScope::Conversation(conversation_id);
            if let Ok(query) = MemoryQuery::new(scope, "", 32)
                && let Ok(recalled) = services.memory.recall(&query).await
            {
                extend_unique_memories(&mut memories, recalled);
            }
            let owner = OpenLoopOwner::Conversation(conversation_id);
            if let Ok(listed) = services.open_loops.list(&owner, 32).await {
                extend_unique_open_loops(&mut open_loops, listed);
            }
        }
        if let Some(person_id) = person_id {
            let scope = MemoryScope::Person(person_id);
            if let Ok(query) = MemoryQuery::new(scope, "", 32)
                && let Ok(recalled) = services.memory.recall(&query).await
            {
                extend_unique_memories(&mut memories, recalled);
            }
            let owner = OpenLoopOwner::Person(person_id);
            if let Ok(listed) = services.open_loops.list(&owner, 32).await {
                extend_unique_open_loops(&mut open_loops, listed);
            }
        }

        input = input.with_memories(memories).with_open_loops(open_loops);

        if let Some(person_id) = person_id {
            if let Ok(relation) = services.relations.get(person_id).await {
                input = input.with_relation(relation);
            }
            if let Ok(affect) = services.affect.get(person_id).await {
                input = input.with_affect(affect);
            }
        }
        input
    }

    async fn apply_state_updates(
        &mut self,
        input: &PlannerInput,
        plan: &DecisionPlan,
        deferred_open_loop_resolution: Option<crate::OpenLoopId>,
    ) -> Result<usize, PlannerError> {
        let event = &input.event;
        let person_id = event_person_id(event).or_else(|| match event.scope() {
            EventScope::Person { person_id } => Some(person_id),
            _ => None,
        });

        // Validate every target before the first persistence call. A planner
        // may only mutate state that was durably hydrated into this turn;
        // otherwise one bad later proposal could leave earlier store updates
        // partially applied or cross a conversation/person boundary.
        for update in &plan.state_updates {
            match update {
                StateUpdateProposal::Affect(_) if person_id.is_none() => {
                    return Err(state_update_error(
                        "affect",
                        "the event has no person identity",
                        0,
                    ));
                }
                StateUpdateProposal::Relation(relation)
                    if person_id != Some(relation.person_id) =>
                {
                    return Err(state_update_error(
                        "relation",
                        "the update targets a person outside this turn",
                        0,
                    ));
                }
                StateUpdateProposal::SetTopic {
                    conversation_id, ..
                } if event.scope().conversation_id() != Some(*conversation_id) => {
                    return Err(state_update_error(
                        "set_topic",
                        "the update targets a conversation outside this turn",
                        0,
                    ));
                }
                StateUpdateProposal::ResolveOpenLoop { open_loop_id }
                    if !planner_input_contains_open_loop(input, *open_loop_id) =>
                {
                    return Err(state_update_error(
                        "resolve_open_loop",
                        "the update targets an open loop outside this turn",
                        0,
                    ));
                }
                _ => {}
            }
        }

        // Core service ports are intentionally independent. We therefore use
        // fail-fast commits and report how many earlier proposals were already
        // applied instead of pretending the batch is transactional.
        let mut applied_updates = 0;
        for update in &plan.state_updates {
            match update {
                StateUpdateProposal::Affect(affect) => {
                    let person_id = person_id.ok_or_else(|| {
                        state_update_error(
                            "affect",
                            "the event has no person identity",
                            applied_updates,
                        )
                    })?;
                    let services = self.services.clone().ok_or_else(|| {
                        state_update_error(
                            "affect",
                            "Core services are unavailable",
                            applied_updates,
                        )
                    })?;
                    services
                        .affect
                        .set(person_id, *affect)
                        .await
                        .map_err(|error| {
                            state_update_error("affect", error.to_string(), applied_updates)
                        })?;
                }
                StateUpdateProposal::Relation(relation) => {
                    let person_id = person_id.ok_or_else(|| {
                        state_update_error(
                            "relation",
                            "the event has no person identity",
                            applied_updates,
                        )
                    })?;
                    if relation.person_id != person_id {
                        return Err(state_update_error(
                            "relation",
                            "the update targets a different person",
                            applied_updates,
                        ));
                    }
                    let services = self.services.clone().ok_or_else(|| {
                        state_update_error(
                            "relation",
                            "Core services are unavailable",
                            applied_updates,
                        )
                    })?;
                    services.relations.set(*relation).await.map_err(|error| {
                        state_update_error("relation", error.to_string(), applied_updates)
                    })?;
                }
                StateUpdateProposal::SetTopic {
                    conversation_id,
                    topic,
                } => {
                    self.state
                        .set_current_topic(*conversation_id, topic.clone())
                        .map_err(|error| {
                            state_update_error("set_topic", error.to_string(), applied_updates)
                        })?;
                }
                StateUpdateProposal::ResolveOpenLoop { open_loop_id } => {
                    if deferred_open_loop_resolution == Some(*open_loop_id) {
                        continue;
                    }
                    let services = self.services.clone().ok_or_else(|| {
                        state_update_error(
                            "resolve_open_loop",
                            "Core services are unavailable",
                            applied_updates,
                        )
                    })?;
                    services
                        .open_loops
                        .resolve(*open_loop_id, Utc::now())
                        .await
                        .map_err(|error| {
                            state_update_error(
                                "resolve_open_loop",
                                error.to_string(),
                                applied_updates,
                            )
                        })?;
                    self.state
                        .resolve_open_loop_reference(*open_loop_id)
                        .map_err(|error| {
                            state_update_error(
                                "resolve_open_loop",
                                error.to_string(),
                                applied_updates,
                            )
                        })?;
                }
            }
            applied_updates += 1;
        }
        Ok(applied_updates)
    }

    async fn resolve_due_open_loop(
        &mut self,
        input: &PlannerInput,
        applied_before_failure: usize,
    ) -> Result<(), PlannerError> {
        let event = &input.event;
        let open_loop_id = due_open_loop_id(event).ok_or_else(|| {
            state_update_error(
                "resolve_open_loop",
                "the event is not an open-loop due event",
                applied_before_failure,
            )
        })?;
        if !planner_input_contains_open_loop(input, open_loop_id) {
            return Err(state_update_error(
                "resolve_open_loop",
                format!("open loop {open_loop_id} was not hydrated for the event owner"),
                applied_before_failure,
            ));
        }
        let services = self.services.as_ref().ok_or_else(|| {
            state_update_error(
                "resolve_open_loop",
                "Core services are unavailable",
                applied_before_failure,
            )
        })?;
        services
            .open_loops
            .resolve(open_loop_id, Utc::now())
            .await
            .map_err(|error| {
                state_update_error(
                    "resolve_open_loop",
                    error.to_string(),
                    applied_before_failure,
                )
            })?;
        self.state
            .resolve_open_loop_reference(open_loop_id)
            .map_err(|error| {
                state_update_error(
                    "resolve_open_loop",
                    error.to_string(),
                    applied_before_failure,
                )
            })?;
        Ok(())
    }

    #[must_use]
    pub const fn state(&self) -> &WorkingState {
        &self.state
    }
}

fn observed_without_planning(observation: RuntimeObservation) -> PlannedProcessingOutcome {
    PlannedProcessingOutcome::Planned {
        observation,
        plan: DecisionPlan::silent(),
        actions: Vec::new(),
        feedback: Vec::new(),
    }
}

fn event_person_id(event: &WorldEvent) -> Option<PersonId> {
    match event.kind() {
        crate::WorldEventKind::MessageReceived(message) => Some(message.sender),
        _ => None,
    }
}

fn due_open_loop_id(event: &WorldEvent) -> Option<crate::OpenLoopId> {
    match event.kind() {
        crate::WorldEventKind::ProspectiveMemoryDue(due) => Some(due.open_loop_id),
        _ => None,
    }
}

fn apply_due_action_idempotency(
    action: &mut crate::ProposedAction,
    open_loop_id: crate::OpenLoopId,
    intent_index: usize,
) -> Result<(), crate::ActionValidationError> {
    let metadata = match action {
        crate::ProposedAction::SendMessage(action) => &mut action.metadata,
        crate::ProposedAction::ReachOut(action) => &mut action.metadata,
        crate::ProposedAction::Noop => return Ok(()),
    };
    metadata.idempotency_key = format!("open-loop:{open_loop_id}:delivery:{intent_index}");
    metadata.validate()
}

fn validate_due_open_loop_context(input: &PlannerInput) -> Result<(), PlannerError> {
    let Some(open_loop_id) = due_open_loop_id(&input.event) else {
        return Ok(());
    };
    if planner_input_contains_open_loop(input, open_loop_id) {
        Ok(())
    } else {
        Err(state_update_error(
            "resolve_open_loop",
            format!("open loop {open_loop_id} was not hydrated for the event owner"),
            0,
        ))
    }
}

fn validate_intent_targets(event: &WorldEvent, plan: &DecisionPlan) -> Result<(), PlannerError> {
    for intent in &plan.intents {
        let allowed = match (event.scope(), intent) {
            (_, crate::CognitiveIntent::Noop) => true,
            (
                EventScope::Conversation { conversation_id },
                crate::CognitiveIntent::SendMessage {
                    conversation_id: target,
                    ..
                },
            ) => conversation_id == *target,
            (EventScope::Person { person_id }, crate::CognitiveIntent::ReachOut(reach_out)) => {
                person_id == reach_out.person_id()
            }
            _ => false,
        };
        if !allowed {
            let reason = match intent {
                crate::CognitiveIntent::SendMessage {
                    conversation_id, ..
                } => format!(
                    "send_message targets conversation {conversation_id} from {:?}",
                    event.scope()
                ),
                crate::CognitiveIntent::ReachOut(reach_out) => format!(
                    "reach_out targets person {} from {:?}",
                    reach_out.person_id(),
                    event.scope()
                ),
                crate::CognitiveIntent::Noop => unreachable!("noop intents are always allowed"),
            };
            return Err(PlannerError::InvalidOutput(
                PlannerOutputValidationError::IntentOutsideEventScope { reason },
            ));
        }
    }
    Ok(())
}

fn deferred_due_open_loop_resolution(
    event: &WorldEvent,
    plan: &DecisionPlan,
) -> Option<crate::OpenLoopId> {
    let open_loop_id = due_open_loop_id(event)?;
    let resolves_due = plan.state_updates.iter().any(|update| {
        matches!(
            update,
            StateUpdateProposal::ResolveOpenLoop { open_loop_id: candidate }
                if *candidate == open_loop_id
        )
    });
    let requires_delivery = plan
        .intents
        .iter()
        .any(|intent| !matches!(intent, crate::CognitiveIntent::Noop));
    (resolves_due && requires_delivery).then_some(open_loop_id)
}

fn planner_input_contains_open_loop(input: &PlannerInput, open_loop_id: crate::OpenLoopId) -> bool {
    input.open_loops.iter().any(|open_loop| {
        open_loop.id() == open_loop_id
            && open_loop_owner_is_visible(open_loop.owner(), &input.event)
    })
}

fn open_loop_owner_is_visible(owner: OpenLoopOwner, event: &WorldEvent) -> bool {
    match owner {
        OpenLoopOwner::Person(owner) => {
            event_person_id(event) == Some(owner)
                || matches!(event.scope(), EventScope::Person { person_id } if person_id == owner)
        }
        OpenLoopOwner::Conversation(owner) => event.scope().conversation_id() == Some(owner),
        OpenLoopOwner::Global => matches!(event.scope(), EventScope::Global),
    }
}

fn state_update_error(
    kind: &'static str,
    message: impl Into<String>,
    applied_before_failure: usize,
) -> PlannerError {
    PlannerError::StateUpdate {
        kind,
        message: message.into(),
        applied_before_failure,
    }
}

fn extend_unique_memories(target: &mut Vec<crate::Memory>, values: Vec<crate::Memory>) {
    for memory in values {
        if !target.iter().any(|existing| existing.id() == memory.id()) {
            target.push(memory);
        }
    }
}

fn extend_unique_open_loops(target: &mut Vec<crate::OpenLoop>, values: Vec<crate::OpenLoop>) {
    for open_loop in values {
        if !target
            .iter()
            .any(|existing| existing.id() == open_loop.id())
        {
            target.push(open_loop);
        }
    }
}

fn action_result_event(
    parent: &WorldEvent,
    action: &crate::ProposedAction,
    result: &ActionResult,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    let idempotency_key = action.idempotency_key()?.to_owned();
    let scope = match action.scope() {
        crate::ActionScope::Conversation(conversation_id) => {
            EventScope::Conversation { conversation_id }
        }
        crate::ActionScope::Person(person_id) => EventScope::Person { person_id },
        crate::ActionScope::Global => EventScope::Global,
    };
    let kind = match result {
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Delivered { .. },
            ..
        } => {
            crate::WorldEventKind::ActionSucceeded(crate::ActionSucceededEvent { idempotency_key })
        }
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Deferred { reason },
            ..
        } => crate::WorldEventKind::ActionFailed(crate::ActionFailedEvent {
            idempotency_key,
            error_category: format!("deferred:{reason}"),
        }),
        ActionResult::Failed { error, .. } => {
            crate::WorldEventKind::ActionFailed(crate::ActionFailedEvent {
                idempotency_key,
                error_category: error.category.clone(),
            })
        }
        ActionResult::Rejected(rejection) => {
            crate::WorldEventKind::ActionRejected(crate::ActionRejectedEvent {
                idempotency_key,
                reason: rejection.to_string(),
            })
        }
        ActionResult::Noop => return None,
    };
    WorldEvent::derived_from(
        parent,
        Utc::now(),
        scope,
        EventPriority::High,
        kind,
        max_trace_depth,
    )
    .ok()
}

fn message_sent_event(
    parent: &WorldEvent,
    action: &crate::ProposedAction,
    result: &ActionResult,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    let crate::ActionResult::Executed {
        outcome:
            crate::ActionPortOutcome::Delivered {
                message_id: Some(message_id),
                conversation_id: delivered_conversation_id,
                ..
            },
        ..
    } = result
    else {
        return None;
    };
    let conversation_id = match action {
        crate::ProposedAction::SendMessage(send) => {
            if delivered_conversation_id.is_some_and(|delivered| delivered != send.conversation_id)
            {
                return None;
            }
            send.conversation_id
        }
        crate::ProposedAction::ReachOut(_) => (*delivered_conversation_id)?,
        crate::ProposedAction::Noop => return None,
    };
    let delivered_at = Utc::now();
    WorldEvent::derived_from(
        parent,
        delivered_at,
        EventScope::Conversation { conversation_id },
        EventPriority::High,
        crate::WorldEventKind::MessageSent(crate::MessageSentEvent {
            message_id: *message_id,
            conversation_id,
            timestamp: delivered_at,
        }),
        max_trace_depth,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, CognitiveRuntime, DataErasureError, MAX_DATA_ERASURE_CONVERSATIONS,
        PlannedProcessingOutcome, ProcessingOutcome, RuntimeConfig, RuntimeConfigError,
        SubmitError, apply_due_action_idempotency, bounded_conversation_ids, message_sent_event,
        validate_intent_targets,
    };
    use crate::arbiter::{
        ActionArbiter, ActionArbiterConfig, ActionPort, ActionPortFuture, ActionPortOutcome,
        ActionReceipt, ActionRejection, ActionResult, EnvironmentCapabilities,
    };
    use crate::event::{
        EventPriority, EventScope, EventValidationError, MessageContent, MessageReceivedEvent,
        WorldEvent, WorldEventKind,
    };
    use crate::identity::{ConversationId, ConversationKind, MessageId, OpenLoopId, PersonId};
    use crate::memory::{Memory, MemoryDraft, MemoryKind, MemoryQuery, MemoryScope};
    use crate::open_loop::{OpenLoop, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStatus};
    use crate::planner::{
        AffectState, DecisionDisposition, DecisionPlan, ModelBackend, ModelBackendFuture,
        PlannerError, PlannerInput, RelationState, StateUpdateProposal,
    };
    use crate::ports::{
        AffectStore, AffectStoreFuture, CoreServices, MemoryStore, MemoryStoreFuture,
        OpenLoopStore, OpenLoopStoreFuture, RelationStore, RelationStoreFuture,
    };
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn event(priority: EventPriority) -> WorldEvent {
        WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            priority,
            WorldEventKind::IdleTick,
        )
    }

    fn direct_message(conversation_id: ConversationId, sender: PersonId) -> WorldEvent {
        message(conversation_id, sender, ConversationKind::Direct)
    }

    fn group_message(conversation_id: ConversationId, sender: PersonId) -> WorldEvent {
        message(conversation_id, sender, ConversationKind::Group)
    }

    fn message(
        conversation_id: ConversationId,
        sender: PersonId,
        conversation_kind: ConversationKind,
    ) -> WorldEvent {
        WorldEvent::message_received(
            EventPriority::High,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id,
                sender,
                content: MessageContent::text("hello"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind,
                addressed_to_agent: true,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: true,
            },
        )
    }

    fn due_event(conversation_id: ConversationId, open_loop_id: OpenLoopId) -> WorldEvent {
        WorldEvent::new(
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ProspectiveMemoryDue(crate::ProspectiveMemoryEvent { open_loop_id }),
        )
    }

    struct FakeModel;

    impl ModelBackend for FakeModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            Box::pin(async {
                Ok(DecisionPlan {
                    disposition: DecisionDisposition::Silent,
                    intents: vec![crate::CognitiveIntent::noop()],
                    state_updates: Vec::new(),
                })
            })
        }
    }

    struct CountingModel {
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for CountingModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(DecisionPlan::silent().with_intent(crate::CognitiveIntent::noop()))
            })
        }
    }

    struct ActionModel {
        conversation_id: ConversationId,
    }

    impl ModelBackend for ActionModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            Box::pin(async move {
                Ok(DecisionPlan {
                    disposition: DecisionDisposition::Reply,
                    intents: vec![crate::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text("planned reply"),
                    )],
                    state_updates: Vec::new(),
                })
            })
        }
    }

    struct DueActionModel {
        conversation_id: ConversationId,
        open_loop_id: OpenLoopId,
    }

    impl ModelBackend for DueActionModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            let open_loop_id = self.open_loop_id;
            Box::pin(async move {
                Ok(DecisionPlan::new(DecisionDisposition::Reply)
                    .with_intent(crate::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text("checking in"),
                    ))
                    .with_state_update(StateUpdateProposal::ResolveOpenLoop { open_loop_id }))
            })
        }
    }

    struct DueMultiActionModel {
        conversation_id: ConversationId,
        open_loop_id: OpenLoopId,
    }

    impl ModelBackend for DueMultiActionModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            let open_loop_id = self.open_loop_id;
            Box::pin(async move {
                Ok(DecisionPlan::new(DecisionDisposition::Reply)
                    .with_intent(crate::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text("first delivery"),
                    ))
                    .with_intent(crate::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text("second delivery"),
                    ))
                    .with_state_update(StateUpdateProposal::ResolveOpenLoop { open_loop_id }))
            })
        }
    }

    struct DueActionWithoutResolutionModel {
        conversation_id: ConversationId,
    }

    impl ModelBackend for DueActionWithoutResolutionModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            Box::pin(async move {
                Ok(DecisionPlan::new(DecisionDisposition::Reply).with_intent(
                    crate::CognitiveIntent::send_message(
                        conversation_id,
                        MessageContent::text("leave the loop open"),
                    ),
                ))
            })
        }
    }

    struct FakeActionPort;

    impl ActionPort for FakeActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            Box::pin(async {
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some("fake".to_owned()),
                    message_id: None,
                    conversation_id: None,
                })
            })
        }
    }

    struct FailingActionPort;

    impl ActionPort for FailingActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            Box::pin(async { Err(crate::ActionPortError::new("delivery_failed", true)) })
        }
    }

    struct CountingDeliveredActionPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for CountingDeliveredActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some("fake".to_owned()),
                    message_id: None,
                    conversation_id: None,
                })
            })
        }
    }

    struct FailOnceActionPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for FailOnceActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 0 {
                    Err(crate::ActionPortError::new(
                        "temporary_delivery_failure",
                        true,
                    ))
                } else {
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some("fake".to_owned()),
                        message_id: None,
                        conversation_id: None,
                    })
                }
            })
        }
    }

    struct FailSecondActionOncePort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for FailSecondActionOncePort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 1 {
                    Err(crate::ActionPortError::new(
                        "temporary_second_delivery_failure",
                        true,
                    ))
                } else {
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some("fake".to_owned()),
                        message_id: None,
                        conversation_id: None,
                    })
                }
            })
        }
    }

    struct MappedActionPort {
        message_id: MessageId,
    }

    impl ActionPort for MappedActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            let message_id = self.message_id;
            Box::pin(async move {
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some("qq-message:42".to_owned()),
                    message_id: Some(message_id),
                    conversation_id: None,
                })
            })
        }
    }

    #[derive(Default)]
    struct TestMemoryStore {
        recalled_scopes: Mutex<Vec<MemoryScope>>,
    }

    impl MemoryStore for TestMemoryStore {
        fn remember<'a>(&'a self, draft: &'a MemoryDraft) -> MemoryStoreFuture<'a, Memory> {
            Box::pin(async move {
                Memory::from_draft(crate::MemoryId::new(), draft, Utc::now()).map_err(|error| {
                    crate::MemoryStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                })
            })
        }

        fn recall<'a>(&'a self, query: &'a MemoryQuery) -> MemoryStoreFuture<'a, Vec<Memory>> {
            Box::pin(async move {
                self.recalled_scopes
                    .lock()
                    .expect("memory recorder lock")
                    .push(query.scope());
                let draft = MemoryDraft::new(
                    query.scope(),
                    MemoryKind::Fact,
                    format!("memory for {:?}", query.scope()),
                    Utc::now(),
                )
                .expect("valid test memory");
                Ok(vec![
                    Memory::from_draft(crate::MemoryId::new(), &draft, Utc::now())
                        .expect("valid test memory record"),
                ])
            })
        }

        fn forget(&self, _scope: MemoryScope, _id: crate::MemoryId) -> MemoryStoreFuture<'_, bool> {
            Box::pin(async { Ok(false) })
        }
    }

    #[derive(Default)]
    struct TestOpenLoopStore {
        listed_owners: Mutex<Vec<OpenLoopOwner>>,
        resolved: Mutex<Vec<OpenLoopId>>,
        visible: Option<(OpenLoopId, OpenLoopOwner)>,
        resolve_failures_remaining: AtomicUsize,
    }

    impl TestOpenLoopStore {
        fn open_loop(id: OpenLoopId, owner: OpenLoopOwner) -> OpenLoop {
            OpenLoop::new(id, owner, OpenLoopKind::FollowUp, "follow up", Utc::now())
                .expect("valid test open loop")
        }

        fn with_visible(id: OpenLoopId, owner: OpenLoopOwner) -> Self {
            Self {
                visible: Some((id, owner)),
                ..Self::default()
            }
        }

        fn with_resolve_failures(id: OpenLoopId, owner: OpenLoopOwner, failures: usize) -> Self {
            Self {
                visible: Some((id, owner)),
                resolve_failures_remaining: AtomicUsize::new(failures),
                ..Self::default()
            }
        }
    }

    impl OpenLoopStore for TestOpenLoopStore {
        fn create<'a>(&'a self, draft: &'a OpenLoopDraft) -> OpenLoopStoreFuture<'a, OpenLoop> {
            Box::pin(async move {
                OpenLoop::from_draft(OpenLoopId::new(), draft, Utc::now()).map_err(|error| {
                    crate::OpenLoopStoreError::InvalidRequest {
                        reason: error.to_string(),
                    }
                })
            })
        }

        fn get<'a>(&'a self, _id: OpenLoopId) -> OpenLoopStoreFuture<'a, Option<OpenLoop>> {
            Box::pin(async { Ok(None) })
        }

        fn list<'a>(
            &'a self,
            owner: &'a OpenLoopOwner,
            _limit: usize,
        ) -> OpenLoopStoreFuture<'a, Vec<OpenLoop>> {
            Box::pin(async move {
                self.listed_owners
                    .lock()
                    .expect("open-loop recorder lock")
                    .push(*owner);
                let id = self
                    .visible
                    .filter(|(_, visible_owner)| visible_owner == owner)
                    .map_or_else(OpenLoopId::new, |(id, _)| id);
                Ok(vec![Self::open_loop(id, *owner)])
            })
        }

        fn claim_due(
            &self,
            _now: chrono::DateTime<Utc>,
            _limit: usize,
        ) -> OpenLoopStoreFuture<'_, Vec<OpenLoop>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn defer(
            &self,
            id: OpenLoopId,
            _due_at: Option<chrono::DateTime<Utc>>,
            _now: chrono::DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move { Ok(Self::open_loop(id, OpenLoopOwner::Global)) })
        }

        fn resolve(
            &self,
            id: OpenLoopId,
            now: chrono::DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move {
                self.resolved
                    .lock()
                    .expect("open-loop recorder lock")
                    .push(id);
                if self.resolve_failures_remaining.load(Ordering::SeqCst) > 0 {
                    self.resolve_failures_remaining
                        .fetch_sub(1, Ordering::SeqCst);
                    return Err(crate::OpenLoopStoreError::storage(std::io::Error::other(
                        "temporary resolve failure",
                    )));
                }
                Self::open_loop(id, OpenLoopOwner::Global)
                    .transition(OpenLoopStatus::Resolved, now)
                    .map_err(|error| crate::OpenLoopStoreError::InvalidRequest {
                        reason: error.to_string(),
                    })
            })
        }

        fn cancel(
            &self,
            id: OpenLoopId,
            now: chrono::DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move {
                Self::open_loop(id, OpenLoopOwner::Global)
                    .transition(OpenLoopStatus::Cancelled, now)
                    .map_err(|error| crate::OpenLoopStoreError::InvalidRequest {
                        reason: error.to_string(),
                    })
            })
        }

        fn recover_stale_triggered(
            &self,
            _now: chrono::DateTime<Utc>,
            _limit: usize,
        ) -> OpenLoopStoreFuture<'_, usize> {
            Box::pin(async { Ok(0) })
        }
    }

    #[derive(Default)]
    struct TestAffectStore {
        updates: Mutex<Vec<(PersonId, AffectState)>>,
    }

    impl AffectStore for TestAffectStore {
        fn get<'a>(&'a self, _person_id: PersonId) -> AffectStoreFuture<'a, AffectState> {
            Box::pin(async { Ok(AffectState::default()) })
        }

        fn set<'a>(
            &'a self,
            person_id: PersonId,
            state: AffectState,
        ) -> AffectStoreFuture<'a, AffectState> {
            Box::pin(async move {
                self.updates
                    .lock()
                    .expect("affect recorder lock")
                    .push((person_id, state));
                Ok(state)
            })
        }
    }

    #[derive(Default)]
    struct TestRelationStore {
        updates: Mutex<Vec<RelationState>>,
    }

    impl RelationStore for TestRelationStore {
        fn get<'a>(
            &'a self,
            _person_id: PersonId,
        ) -> RelationStoreFuture<'a, Option<RelationState>> {
            Box::pin(async { Ok(None) })
        }

        fn set<'a>(&'a self, state: RelationState) -> RelationStoreFuture<'a, RelationState> {
            Box::pin(async move {
                self.updates
                    .lock()
                    .expect("relation recorder lock")
                    .push(state);
                Ok(state)
            })
        }
    }

    struct StateUpdatingModel {
        conversation_id: ConversationId,
        open_loop_id: OpenLoopId,
        affect: AffectState,
        relation: RelationState,
    }

    impl ModelBackend for StateUpdatingModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            let open_loop_id = self.open_loop_id;
            let affect = self.affect;
            let relation = self.relation;
            Box::pin(async move {
                Ok(DecisionPlan::silent()
                    .with_state_update(StateUpdateProposal::Affect(affect))
                    .with_state_update(StateUpdateProposal::Relation(relation))
                    .with_state_update(StateUpdateProposal::SetTopic {
                        conversation_id,
                        topic: "runtime closure".to_owned(),
                    })
                    .with_state_update(StateUpdateProposal::ResolveOpenLoop { open_loop_id }))
            })
        }
    }

    #[tokio::test]
    async fn ignore_and_observe_only_events_do_not_invoke_the_planner() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(CountingModel {
                calls: calls.clone(),
            }),
        )
        .expect("valid runtime");
        for background in [
            event(EventPriority::Normal),
            WorldEvent::new(
                Utc::now(),
                EventScope::Global,
                EventPriority::Normal,
                WorldEventKind::MaintenanceTick,
            ),
        ] {
            let output = runtime
                .process_event_with_planner(background)
                .await
                .expect("background observation should remain local");
            assert!(matches!(
                output,
                PlannedProcessingOutcome::Planned {
                    plan: DecisionPlan {
                        disposition: DecisionDisposition::Silent,
                        intents,
                        ..
                    },
                    ..
                } if intents.is_empty()
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        runtime
            .process_event_with_planner(WorldEvent::new(
                Utc::now(),
                EventScope::Global,
                EventPriority::High,
                WorldEventKind::HostStarted,
            ))
            .await
            .expect("attended event should plan");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.state().global_version(), 3);
    }

    #[tokio::test]
    async fn runtime_dispatches_planned_actions_and_observes_feedback_events() {
        let conversation_id = ConversationId::new();
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ActionModel { conversation_id }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let event = direct_message(conversation_id, PersonId::new());
        let output = runtime
            .process_event_with_planner_and_actions(event, &arbiter, &FakeActionPort)
            .await
            .expect("planner and action dispatch should succeed");
        assert!(matches!(
            output,
            PlannedProcessingOutcome::Planned {
                actions,
                feedback,
                ..
            } if actions.len() == 1 && feedback.len() == 1
        ));
        assert_eq!(runtime.state().global_version(), 2);
    }

    #[tokio::test]
    async fn planning_only_does_not_resolve_a_due_open_loop_before_delivery() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueActionModel {
                conversation_id,
                open_loop_id,
            })
            .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");

        let outcome = runtime
            .process_event_with_planner(due_event(conversation_id, open_loop_id))
            .await
            .expect("planning should succeed");

        assert!(matches!(
            outcome,
            PlannedProcessingOutcome::Planned { plan, .. }
                if plan.intents.len() == 1 && plan.state_updates.len() == 1
        ));
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty(),
            "planning without delivery must leave the due item open"
        );
    }

    #[tokio::test]
    async fn due_open_loop_owner_is_verified_before_planning_or_delivery() {
        let event_person = PersonId::new();
        let foreign_person = PersonId::new();
        let open_loop_id = OpenLoopId::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Person(foreign_person),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(CountingModel {
                calls: calls.clone(),
            })
            .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Person {
                person_id: event_person,
            },
            EventPriority::High,
            WorldEventKind::ProspectiveMemoryDue(crate::ProspectiveMemoryEvent { open_loop_id }),
        );

        assert!(matches!(
            runtime.process_event_with_planner(event).await,
            Err(PlannerError::StateUpdate {
                kind: "resolve_open_loop",
                applied_before_failure: 0,
                ..
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn due_open_loop_resolves_only_after_a_delivered_action() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueActionModel {
                conversation_id,
                open_loop_id,
            })
            .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );

        let failed = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &FailingActionPort,
            )
            .await
            .expect("a port failure is represented as an action result");
        assert!(matches!(
            failed,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(actions.as_slice(), [ActionResult::Failed { .. }])
        ));
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty(),
            "failed delivery must not resolve the due item"
        );

        runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &FakeActionPort,
            )
            .await
            .expect("delivered action should close the due item");
        assert_eq!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[open_loop_id]
        );
    }

    #[tokio::test]
    async fn delivered_due_replay_retries_resolution_without_redelivery() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_resolve_failures(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
            1,
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueActionModel {
                conversation_id,
                open_loop_id,
            })
            .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let port = CountingDeliveredActionPort {
            calls: calls.clone(),
        };

        assert!(matches!(
            runtime
                .process_event_with_planner_and_actions(
                    due_event(conversation_id, open_loop_id),
                    &arbiter,
                    &port,
                )
                .await,
            Err(PlannerError::StateUpdate {
                kind: "resolve_open_loop",
                ..
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let replay = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("a duplicate delivered action should retry resolution");
        assert!(matches!(
            replay,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [ActionResult::Rejected(ActionRejection::Duplicate { .. })]
                )
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[open_loop_id, open_loop_id]
        );
    }

    #[tokio::test]
    async fn failed_due_delivery_releases_the_key_for_a_real_retry() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueActionModel {
                conversation_id,
                open_loop_id,
            })
            .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let port = FailOnceActionPort {
            calls: calls.clone(),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("port failures remain structured action results");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(actions.as_slice(), [ActionResult::Failed { .. }])
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let second = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("a failed delivery must be dispatched again");
        assert!(matches!(
            second,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [ActionResult::Executed {
                        outcome: ActionPortOutcome::Delivered { .. },
                        ..
                    }]
                )
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[open_loop_id]
        );
    }

    #[tokio::test]
    async fn due_open_loop_waits_for_every_required_delivery() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueMultiActionModel {
                conversation_id,
                open_loop_id,
            })
            .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let port = FailSecondActionOncePort {
            calls: calls.clone(),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("delivery failures remain structured action results");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [
                        ActionResult::Executed {
                            outcome: ActionPortOutcome::Delivered { .. },
                            ..
                        },
                        ActionResult::Failed { .. }
                    ]
                )
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty(),
            "one successful intent must not hide a later failed delivery"
        );

        let replay = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("only the failed intent should be dispatched again");
        assert!(matches!(
            replay,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [
                        ActionResult::Rejected(ActionRejection::Duplicate { .. }),
                        ActionResult::Executed {
                            outcome: ActionPortOutcome::Delivered { .. },
                            ..
                        }
                    ]
                )
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[open_loop_id]
        );
    }

    #[tokio::test]
    async fn due_delivery_does_not_resolve_without_a_planner_state_update() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueActionWithoutResolutionModel { conversation_id })
                .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );

        runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &FakeActionPort,
            )
            .await
            .expect("the action can be delivered while the loop stays open");
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );
    }

    #[test]
    fn due_idempotency_keys_are_stable_and_distinguish_intent_positions() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let mut first =
            crate::ProposedAction::send_message(conversation_id, MessageContent::text("first"))
                .expect("valid action");
        let mut second =
            crate::ProposedAction::send_message(conversation_id, MessageContent::text("second"))
                .expect("valid action");

        apply_due_action_idempotency(&mut first, open_loop_id, 0).expect("valid metadata");
        apply_due_action_idempotency(&mut second, open_loop_id, 1).expect("valid metadata");

        assert_eq!(
            first.idempotency_key(),
            Some(format!("open-loop:{open_loop_id}:delivery:0").as_str())
        );
        assert_eq!(
            second.idempotency_key(),
            Some(format!("open-loop:{open_loop_id}:delivery:1").as_str())
        );
        assert_ne!(first.idempotency_key(), second.idempotency_key());
    }

    #[tokio::test]
    async fn direct_messages_load_both_conversation_and_person_context() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let memory = Arc::new(TestMemoryStore::default());
        let open_loops = Arc::new(TestOpenLoopStore::default());
        let (_handle, runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel)
                .with_memory(memory.clone())
                .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");

        let input = runtime
            .planner_input_with_context(direct_message(conversation_id, person_id))
            .await;

        assert_eq!(
            memory
                .recalled_scopes
                .lock()
                .expect("memory recorder lock")
                .as_slice(),
            &[
                MemoryScope::Conversation(conversation_id),
                MemoryScope::Person(person_id),
            ]
        );
        assert_eq!(
            open_loops
                .listed_owners
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[
                OpenLoopOwner::Conversation(conversation_id),
                OpenLoopOwner::Person(person_id),
            ]
        );
        assert_eq!(input.memories.len(), 2);
        assert!(
            input
                .memories
                .iter()
                .any(|memory| memory.scope() == MemoryScope::Conversation(conversation_id))
        );
        assert!(
            input
                .memories
                .iter()
                .any(|memory| memory.scope() == MemoryScope::Person(person_id))
        );
        assert_eq!(input.open_loops.len(), 2);
        assert!(
            input
                .open_loops
                .iter()
                .any(|open_loop| open_loop.owner() == OpenLoopOwner::Conversation(conversation_id))
        );
        assert!(
            input
                .open_loops
                .iter()
                .any(|open_loop| open_loop.owner() == OpenLoopOwner::Person(person_id))
        );
    }

    #[test]
    fn planner_intents_cannot_cross_the_event_scope() {
        let source_conversation = ConversationId::new();
        let foreign_conversation = ConversationId::new();
        let sender = PersonId::new();
        let conversation_event = direct_message(source_conversation, sender);
        let valid_send = DecisionPlan::new(DecisionDisposition::Reply).with_intent(
            crate::CognitiveIntent::send_message(
                source_conversation,
                MessageContent::text("same conversation"),
            ),
        );
        assert!(validate_intent_targets(&conversation_event, &valid_send).is_ok());

        let foreign_send = DecisionPlan::new(DecisionDisposition::Reply).with_intent(
            crate::CognitiveIntent::send_message(
                foreign_conversation,
                MessageContent::text("wrong conversation"),
            ),
        );
        assert!(matches!(
            validate_intent_targets(&conversation_event, &foreign_send),
            Err(PlannerError::InvalidOutput(
                crate::PlannerOutputValidationError::IntentOutsideEventScope { .. }
            ))
        ));

        let source_person = PersonId::new();
        let foreign_person = PersonId::new();
        let person_event = WorldEvent::new(
            Utc::now(),
            EventScope::Person {
                person_id: source_person,
            },
            EventPriority::High,
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: "background.task".to_owned(),
            }),
        );
        let reach_out = |person_id, text| {
            crate::ReachOutIntent::from_parts(
                person_id,
                MessageContent::text(text),
                crate::ProactiveMotive::FollowUp,
            )
            .map(crate::CognitiveIntent::reach_out)
            .expect("valid reach-out intent")
        };
        let valid_reach_out = DecisionPlan::new(DecisionDisposition::Reply)
            .with_intent(reach_out(source_person, "same person"));
        assert!(validate_intent_targets(&person_event, &valid_reach_out).is_ok());

        let foreign_reach_out = DecisionPlan::new(DecisionDisposition::Reply)
            .with_intent(reach_out(foreign_person, "wrong person"));
        assert!(matches!(
            validate_intent_targets(&person_event, &foreign_reach_out),
            Err(PlannerError::InvalidOutput(
                crate::PlannerOutputValidationError::IntentOutsideEventScope { .. }
            ))
        ));

        let cross_kind = DecisionPlan::new(DecisionDisposition::Reply)
            .with_intent(reach_out(sender, "conversation to person"));
        assert!(matches!(
            validate_intent_targets(&conversation_event, &cross_kind),
            Err(PlannerError::InvalidOutput(
                crate::PlannerOutputValidationError::IntentOutsideEventScope { .. }
            ))
        ));
        assert!(validate_intent_targets(&person_event, &DecisionPlan::silent()).is_ok());
    }

    #[tokio::test]
    async fn planner_state_updates_are_applied_to_ports_and_working_state() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let open_loop_id = OpenLoopId::new();
        let affect = AffectState {
            valence: 0.4,
            arousal: 0.3,
            social_energy: 0.8,
            curiosity: 0.9,
        };
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let affects = Arc::new(TestAffectStore::default());
        let relations = Arc::new(TestRelationStore::default());
        let relation = RelationState {
            person_id,
            familiarity: 0.2,
            affinity: 0.4,
            trust: 0.3,
            comfort: 0.5,
            tension: -0.1,
        };
        let services = CoreServices::with_model(StateUpdatingModel {
            conversation_id,
            open_loop_id,
            affect,
            relation,
        })
        .with_open_loops(open_loops.clone())
        .with_affect(affects.clone())
        .with_relations(relations.clone());
        let (_handle, mut runtime) =
            CognitiveRuntime::new_with_services(RuntimeConfig::default(), services)
                .expect("valid runtime");

        let due = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::Normal,
            WorldEventKind::ProspectiveMemoryDue(crate::ProspectiveMemoryEvent { open_loop_id }),
        );
        assert!(matches!(
            runtime.process_event(due),
            ProcessingOutcome::Observed(_)
        ));
        runtime
            .process_event_with_planner(direct_message(conversation_id, person_id))
            .await
            .expect("state updates should apply");

        let snapshot = runtime
            .state()
            .conversation(conversation_id)
            .expect("conversation state");
        assert_eq!(snapshot.current_topic.as_deref(), Some("runtime closure"));
        assert!(!snapshot.open_loops.contains(&open_loop_id));
        assert_eq!(
            affects
                .updates
                .lock()
                .expect("affect recorder lock")
                .as_slice(),
            &[(person_id, affect)]
        );
        assert_eq!(
            relations
                .updates
                .lock()
                .expect("relation recorder lock")
                .as_slice(),
            &[relation]
        );
        assert_eq!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[open_loop_id]
        );
    }

    #[tokio::test]
    async fn state_update_targets_are_preflighted_before_any_store_mutation() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let affects = Arc::new(TestAffectStore::default());
        let open_loops = Arc::new(TestOpenLoopStore::default());
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel)
                .with_affect(affects.clone())
                .with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let input = runtime.planner_input(direct_message(conversation_id, person_id));
        let plan = DecisionPlan::silent()
            .with_state_update(StateUpdateProposal::Affect(AffectState::default()))
            .with_state_update(StateUpdateProposal::SetTopic {
                conversation_id: ConversationId::new(),
                topic: "foreign conversation".to_owned(),
            });

        assert!(matches!(
            runtime.apply_state_updates(&input, &plan, None).await,
            Err(PlannerError::StateUpdate {
                kind: "set_topic",
                ..
            })
        ));
        assert!(
            affects
                .updates
                .lock()
                .expect("affect recorder lock")
                .is_empty(),
            "preflight must reject all targets before persisting the earlier affect update"
        );

        let unseen_open_loop = OpenLoopId::new();
        let plan = DecisionPlan::silent().with_state_update(StateUpdateProposal::ResolveOpenLoop {
            open_loop_id: unseen_open_loop,
        });
        assert!(matches!(
            runtime.apply_state_updates(&input, &plan, None).await,
            Err(PlannerError::StateUpdate {
                kind: "resolve_open_loop",
                ..
            })
        ));
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );

        let stale_reference = OpenLoopId::new();
        assert!(matches!(
            runtime.process_event(due_event(conversation_id, stale_reference)),
            ProcessingOutcome::Observed(_)
        ));
        let input_with_stale_reference =
            runtime.planner_input(direct_message(conversation_id, person_id));
        let plan = DecisionPlan::silent().with_state_update(StateUpdateProposal::ResolveOpenLoop {
            open_loop_id: stale_reference,
        });
        assert!(matches!(
            runtime
                .apply_state_updates(&input_with_stale_reference, &plan, None)
                .await,
            Err(PlannerError::StateUpdate {
                kind: "resolve_open_loop",
                applied_before_failure: 0,
                ..
            })
        ));

        let foreign_open_loop_id = OpenLoopId::new();
        let foreign_person = PersonId::new();
        let foreign_open_loop = TestOpenLoopStore::open_loop(
            foreign_open_loop_id,
            OpenLoopOwner::Person(foreign_person),
        );
        let foreign_input = runtime
            .planner_input(direct_message(conversation_id, person_id))
            .with_open_loops(vec![foreign_open_loop]);
        let plan = DecisionPlan::silent().with_state_update(StateUpdateProposal::ResolveOpenLoop {
            open_loop_id: foreign_open_loop_id,
        });
        assert!(matches!(
            runtime
                .apply_state_updates(&foreign_input, &plan, None)
                .await,
            Err(PlannerError::StateUpdate {
                kind: "resolve_open_loop",
                applied_before_failure: 0,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn state_update_failure_reports_prior_independent_commits() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let affects = Arc::new(TestAffectStore::default());
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel).with_affect(affects.clone()),
        )
        .expect("valid runtime");
        let affect = AffectState {
            valence: 0.25,
            arousal: 0.5,
            social_energy: 0.75,
            curiosity: 1.0,
        };
        let relation = RelationState::new(person_id);
        let input = runtime.planner_input(direct_message(conversation_id, person_id));
        let plan = DecisionPlan::silent()
            .with_state_update(StateUpdateProposal::Affect(affect))
            .with_state_update(StateUpdateProposal::Relation(relation));

        assert!(matches!(
            runtime.apply_state_updates(&input, &plan, None).await,
            Err(PlannerError::StateUpdate {
                kind: "relation",
                applied_before_failure: 1,
                ..
            })
        ));
        assert_eq!(
            affects
                .updates
                .lock()
                .expect("affect recorder lock")
                .as_slice(),
            &[(person_id, affect)],
            "fail-fast batches report, but cannot roll back, prior port commits"
        );
    }

    #[tokio::test]
    async fn delivered_message_feedback_is_valid_and_updates_working_state() {
        let conversation_id = ConversationId::new();
        let delivered_message_id = MessageId::new();
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ActionModel { conversation_id }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let output = runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, PersonId::new()),
                &arbiter,
                &MappedActionPort {
                    message_id: delivered_message_id,
                },
            )
            .await
            .expect("mapped delivery should produce valid feedback");
        assert!(matches!(
            output,
            PlannedProcessingOutcome::Planned { feedback, .. }
                if feedback.len() == 2
                    && feedback.iter().any(|item| item.event_type == crate::EventType::MessageSent)
        ));
        let snapshot = runtime
            .state()
            .conversation(conversation_id)
            .expect("conversation state");
        assert_eq!(snapshot.last_message_id, Some(delivered_message_id));
        assert!(snapshot.last_bot_action_at.is_some());
    }

    #[test]
    fn message_sent_feedback_uses_one_timestamp_and_a_derived_trace() {
        let conversation_id = ConversationId::new();
        let parent = direct_message(conversation_id, PersonId::new());
        let message_id = MessageId::new();
        let action =
            crate::ProposedAction::send_message(conversation_id, MessageContent::text("delivered"))
                .expect("valid action");
        let result = ActionResult::Executed {
            receipt: ActionReceipt {
                action_id: action.action_id(),
                idempotency_key: action.idempotency_key().map(ToOwned::to_owned),
                admitted_at: Utc::now(),
            },
            outcome: ActionPortOutcome::Delivered {
                external_reference: Some("qq-message:42".to_owned()),
                message_id: Some(message_id),
                conversation_id: Some(conversation_id),
            },
        };

        let feedback = message_sent_event(&parent, &action, &result, 8)
            .expect("mapped delivery should create feedback");
        feedback.validate(8).expect("feedback must be valid");
        assert_eq!(feedback.trace().parent_event_id(), Some(parent.id()));
        let WorldEventKind::MessageSent(sent) = feedback.kind() else {
            panic!("feedback must describe a sent message");
        };
        assert_eq!(sent.message_id, message_id);
        assert_eq!(sent.timestamp, feedback.occurred_at());
    }

    #[test]
    fn delivered_reach_out_uses_the_outcome_conversation_for_message_sent() {
        let source_conversation_id = ConversationId::new();
        let delivered_conversation_id = ConversationId::new();
        let parent = direct_message(source_conversation_id, PersonId::new());
        let message_id = MessageId::new();
        let action = crate::ProposedAction::reach_out(
            PersonId::new(),
            MessageContent::text("checking in"),
            crate::ProactiveMotive::FollowUp,
        )
        .expect("valid reach-out action");
        let result = ActionResult::Executed {
            receipt: ActionReceipt {
                action_id: action.action_id(),
                idempotency_key: action.idempotency_key().map(ToOwned::to_owned),
                admitted_at: Utc::now(),
            },
            outcome: ActionPortOutcome::Delivered {
                external_reference: Some("qq-message:43".to_owned()),
                message_id: Some(message_id),
                conversation_id: Some(delivered_conversation_id),
            },
        };

        let feedback = message_sent_event(&parent, &action, &result, 8)
            .expect("mapped reach-out should create feedback");
        feedback.validate(8).expect("feedback must be valid");
        assert_eq!(
            feedback.scope(),
            EventScope::Conversation {
                conversation_id: delivered_conversation_id
            }
        );
        let WorldEventKind::MessageSent(sent) = feedback.kind() else {
            panic!("feedback must describe a sent message");
        };
        assert_eq!(sent.message_id, message_id);
        assert_eq!(sent.conversation_id, delivered_conversation_id);
    }

    #[test]
    fn data_erasure_conversation_limit_counts_unique_ids() {
        let repeated = ConversationId::new();
        let deduplicated = bounded_conversation_ids(std::iter::repeat_n(
            repeated,
            MAX_DATA_ERASURE_CONVERSATIONS + 1,
        ))
        .expect("duplicates must not consume the unique-id budget");
        assert_eq!(deduplicated, vec![repeated]);

        let too_many = (0..=MAX_DATA_ERASURE_CONVERSATIONS)
            .map(|_| ConversationId::new())
            .collect::<Vec<_>>();
        assert_eq!(
            bounded_conversation_ids(too_many),
            Err(DataErasureError::TooManyConversations {
                maximum: MAX_DATA_ERASURE_CONVERSATIONS,
            })
        );
    }

    #[tokio::test]
    async fn data_erasure_commands_form_a_fifo_blocking_barrier() {
        let person_id = PersonId::new();
        let other_person = PersonId::new();
        let direct_conversation = ConversationId::new();
        let shared_conversation = ConversationId::new();
        let prior_direct = direct_message(direct_conversation, person_id);
        let prior_shared = group_message(shared_conversation, person_id);
        let prior_ids = [prior_direct.id(), prior_shared.id()];
        let (handle, mut runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        let (observed_sender, mut observed_receiver) = tokio::sync::mpsc::unbounded_channel();
        let driver = tokio::spawn(async move {
            while let Some(outcome) = runtime.process_next().await {
                if let ProcessingOutcome::Observed(observation) = outcome {
                    observed_sender
                        .send(observation.event_id)
                        .expect("observation receiver should remain open");
                }
            }
            runtime
        });

        handle.submit(prior_direct).await.expect("enqueue direct");
        handle.submit(prior_shared).await.expect("enqueue shared");
        assert_eq!(
            handle
                .begin_data_erasure(person_id, [direct_conversation])
                .await
                .expect("begin barrier"),
            1
        );
        for expected in prior_ids {
            assert_eq!(observed_receiver.recv().await, Some(expected));
        }

        handle
            .submit(direct_message(direct_conversation, person_id))
            .await
            .expect("enqueue blocked direct message");
        handle
            .submit(group_message(shared_conversation, person_id))
            .await
            .expect("enqueue blocked sender message");
        handle
            .submit(due_event(direct_conversation, OpenLoopId::new()))
            .await
            .expect("enqueue blocked direct due event");
        let unrelated = group_message(shared_conversation, other_person);
        let unrelated_id = unrelated.id();
        handle
            .submit(unrelated)
            .await
            .expect("enqueue unrelated event");
        assert!(
            handle
                .end_data_erasure(person_id)
                .await
                .expect("end barrier")
        );
        assert_eq!(observed_receiver.recv().await, Some(unrelated_id));
        assert!(observed_receiver.try_recv().is_err());

        let resumed = direct_message(direct_conversation, person_id);
        let resumed_id = resumed.id();
        handle.submit(resumed).await.expect("enqueue resumed event");
        assert_eq!(observed_receiver.recv().await, Some(resumed_id));

        drop(handle);
        let runtime = driver.await.expect("runtime driver should join");
        let direct = runtime
            .state()
            .conversation(direct_conversation)
            .expect("post-erasure direct state should be recreated");
        assert_eq!(direct.active_people, vec![person_id]);
        let shared = runtime
            .state()
            .conversation(shared_conversation)
            .expect("shared state should remain");
        assert_eq!(shared.active_people, vec![other_person]);
    }

    #[tokio::test]
    async fn data_erasure_commands_fail_cleanly_after_runtime_closes() {
        let (handle, runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        drop(runtime);
        assert_eq!(
            handle
                .begin_data_erasure(PersonId::new(), std::iter::empty())
                .await,
            Err(DataErasureError::RuntimeClosed)
        );
        assert_eq!(
            handle.end_data_erasure(PersonId::new()).await,
            Err(DataErasureError::RuntimeClosed)
        );
    }

    #[tokio::test]
    async fn low_priority_events_drop_when_the_bounded_queue_is_full() {
        let (handle, mut runtime) = CognitiveRuntime::new(RuntimeConfig {
            event_queue_capacity: 1,
            ..RuntimeConfig::default()
        })
        .expect("valid runtime");

        assert_eq!(
            handle.submit(event(EventPriority::Low)).await,
            Ok(Admission::Accepted)
        );
        assert_eq!(
            handle.submit(event(EventPriority::Low)).await,
            Ok(Admission::DroppedAtCapacity)
        );
        assert!(matches!(
            runtime.process_next().await,
            Some(ProcessingOutcome::Observed(_))
        ));
    }

    #[tokio::test]
    async fn critical_events_wait_for_capacity_instead_of_dropping() {
        let (handle, mut runtime) = CognitiveRuntime::new(RuntimeConfig {
            event_queue_capacity: 1,
            ..RuntimeConfig::default()
        })
        .expect("valid runtime");
        handle
            .submit(event(EventPriority::Low))
            .await
            .expect("first event should fit");

        let submitter = tokio::spawn({
            let handle = handle.clone();
            async move { handle.submit(event(EventPriority::Critical)).await }
        });
        tokio::task::yield_now().await;
        assert!(!submitter.is_finished());

        runtime.process_next().await.expect("first event");
        assert_eq!(
            submitter.await.expect("submission task should join"),
            Ok(Admission::Accepted)
        );
        runtime.process_next().await.expect("critical event");
        assert_eq!(runtime.state().global_version(), 2);
    }

    #[tokio::test]
    async fn admission_rejects_events_beyond_the_runtime_trace_limit() {
        let (handle, runtime) = CognitiveRuntime::new(RuntimeConfig {
            max_trace_depth: 1,
            ..RuntimeConfig::default()
        })
        .expect("valid runtime");
        let root = event(EventPriority::Normal);
        let child = WorldEvent::derived_from(
            &root,
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::IdleTick,
            2,
        )
        .expect("first child");
        let grandchild = WorldEvent::derived_from(
            &child,
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::IdleTick,
            2,
        )
        .expect("second child");
        let event_id = grandchild.id();

        assert!(matches!(
            handle.submit(grandchild).await,
            Err(SubmitError::InvalidEvent {
                event,
                error: EventValidationError::TraceDepthExceeded {
                    depth: 2,
                    maximum: 1,
                },
            }) if event.id() == event_id
        ));
        assert_eq!(runtime.state().global_version(), 0);
    }

    #[tokio::test]
    async fn closed_runtime_returns_the_reliable_event_to_the_host() {
        let (handle, runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        drop(runtime);
        let reliable = event(EventPriority::Critical);
        let event_id = reliable.id();

        assert!(matches!(
            handle.submit(reliable).await,
            Err(SubmitError::RuntimeClosed(event)) if event.id() == event_id
        ));
    }

    #[tokio::test]
    async fn accepted_reliable_event_is_returned_when_state_rejects_it() {
        let (handle, mut runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        let conversation_id = ConversationId::new();
        let message = |conversation_kind| {
            WorldEvent::message_received(
                EventPriority::Critical,
                MessageReceivedEvent {
                    message_id: MessageId::new(),
                    conversation_id,
                    sender: PersonId::new(),
                    content: MessageContent::text("hello"),
                    reply_to: None,
                    timestamp: Utc::now(),
                    conversation_kind,
                    addressed_to_agent: false,
                    replies_to_agent: false,
                    stop_requested: false,
                    explicit_request: false,
                },
            )
        };
        handle
            .submit(message(ConversationKind::Group))
            .await
            .expect("first event should be accepted");
        assert!(matches!(
            runtime.process_next().await,
            Some(ProcessingOutcome::Observed(_))
        ));
        let conflicting = message(ConversationKind::Direct);
        let event_id = conflicting.id();
        handle
            .submit(conflicting)
            .await
            .expect("validated event should be admitted");

        assert!(matches!(
            runtime.process_next().await,
            Some(ProcessingOutcome::RejectedState {
                event,
                error: crate::working_state::WorkingStateError::ConversationKindMismatch,
            }) if event.id() == event_id
        ));
    }

    #[test]
    fn runtime_rejects_zero_and_excessive_queue_capacities_without_panicking() {
        assert_eq!(
            CognitiveRuntime::new(RuntimeConfig {
                event_queue_capacity: 0,
                ..RuntimeConfig::default()
            })
            .expect_err("zero capacity must fail"),
            RuntimeConfigError::ZeroQueueCapacity
        );
        assert_eq!(
            CognitiveRuntime::new(RuntimeConfig {
                event_queue_capacity: 4_097,
                ..RuntimeConfig::default()
            })
            .expect_err("excessive capacity must fail"),
            RuntimeConfigError::QueueCapacityTooLarge { value: 4_097 }
        );
    }
}
