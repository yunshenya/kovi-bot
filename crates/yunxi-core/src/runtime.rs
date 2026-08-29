use crate::arbiter::{ActionArbiter, ActionPort, ActionResult};
use crate::attention::{AttentionResult, AttentionSystem};
use crate::event::{
    EventPriority, EventScope, EventType, EventValidationError, MAX_TOOL_RESULT_BYTES,
    MAX_TOOL_RESULT_CHARS, WorldEvent, WorldEventKind,
};
use crate::executive::{
    DecisionActionKind, DecisionRecord, ExecutiveController, ExecutiveReasonTag, ExecutiveScope,
};
use crate::goal::GoalOwner;
use crate::identity::{ConversationId, EventId, PersonId};
use crate::memory::{MemoryQuery, MemoryScope};
use crate::mind::{
    MindInfluenceMode, MindSnapshotLimits, MindSnapshotProvider, MindSnapshotRequest,
};
use crate::open_loop::OpenLoopOwner;
use crate::planner::{
    DecisionPlan, MAX_PLANNER_GOALS, MAX_PLANNER_INTENTS, Planner, PlannerError, PlannerInput,
    PlannerOutputValidationError, PlannerStateSnapshot, StateUpdateProposal,
};
use crate::ports::CoreServices;
use crate::working_state::{
    StateUpdate, WorkingState, WorkingStateConfig, WorkingStateConfigError, WorkingStateError,
};
use chrono::Utc;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const MAX_EVENT_QUEUE_CAPACITY: usize = 4_096;
const MAX_GOALS_PER_CONTEXT_OWNER: usize = 32;
const MAX_PENDING_TOOL_FOLLOW_UPS: usize = 128;
/// Maximum number of tool actions allowed across one causal trace.
///
/// This intentionally matches the per-plan intent bound. A model can still
/// use multiple tools, but a recursive chain cannot grow without limit.
pub const MAX_TOOL_ACTIONS_PER_TRACE: usize = MAX_PLANNER_INTENTS;
/// Number of root traces retained by the cumulative tool-budget ledger.
/// Entries contain only event IDs and counters, never user content.
const MAX_TOOL_TRACE_BUDGET_ENTRIES: usize = 1_024;
/// Recently completed roots are kept as tombstones so replaying the same
/// depth-zero event cannot create a fresh cumulative budget entry. The set is
/// bounded because event IDs are opaque and contain no user content.
const MAX_CLOSED_TOOL_TRACE_TOMBSTONES: usize = 4_096;
const MAX_TOOL_BATCH_OPERATION_CHARS: usize = 128;
const MAX_TOOL_OPERATION_BYTES: usize = 1_024;
const MAX_TOOL_ERROR_CATEGORY_BYTES: usize = 256;
const INITIAL_TOOL_BATCH_FIELD_CHARS: usize = 1_024;
const DEFAULT_MIND_SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(75);
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
                    | RuntimeCommand::EndDataErasure { .. }
                    | RuntimeCommand::BeginConversationDataErasure { .. }
                    | RuntimeCommand::EndConversationDataErasure { .. } => {
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
                    RuntimeCommand::BeginDataErasure { .. }
                    | RuntimeCommand::EndDataErasure { .. }
                    | RuntimeCommand::BeginConversationDataErasure { .. }
                    | RuntimeCommand::EndConversationDataErasure { .. },
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

    /// Establishes a FIFO erasure barrier for one canonical conversation.
    ///
    /// The acknowledgement means all earlier turns have drained and the
    /// retained conversation snapshot has been purged. Later events for the
    /// same conversation are discarded until
    /// [`Self::end_conversation_data_erasure`] is acknowledged.
    pub async fn begin_conversation_data_erasure(
        &self,
        conversation_id: ConversationId,
    ) -> Result<bool, DataErasureError> {
        self.begin_conversation_data_erasures([conversation_id])
            .await
            .map(|removed| removed != 0)
    }

    /// Atomically establishes FIFO erasure barriers for a bounded set of
    /// canonical conversations.
    pub async fn begin_conversation_data_erasures<I>(
        &self,
        conversation_ids: I,
    ) -> Result<usize, DataErasureError>
    where
        I: IntoIterator<Item = ConversationId>,
    {
        let conversation_ids = bounded_conversation_ids(conversation_ids)?;
        if conversation_ids.is_empty() {
            return Ok(0);
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::BeginConversationDataErasure {
                conversation_ids,
                acknowledge,
            })
            .await
            .map_err(|_| DataErasureError::RuntimeClosed)?;
        acknowledged
            .await
            .map_err(|_| DataErasureError::AcknowledgementDropped)?
    }

    /// Releases a conversation barrier through the same FIFO. Its
    /// acknowledgement confirms that events submitted during the blocked
    /// window have already been discarded.
    pub async fn end_conversation_data_erasure(
        &self,
        conversation_id: ConversationId,
    ) -> Result<bool, DataErasureError> {
        self.end_conversation_data_erasures([conversation_id])
            .await
            .map(|resumed| resumed == 1)
    }

    /// Releases a bounded set of conversation barriers through one FIFO
    /// command. The returned count lets a host fail closed if any expected
    /// barrier was missing.
    pub async fn end_conversation_data_erasures<I>(
        &self,
        conversation_ids: I,
    ) -> Result<usize, DataErasureError>
    where
        I: IntoIterator<Item = ConversationId>,
    {
        let conversation_ids = bounded_conversation_ids(conversation_ids)?;
        if conversation_ids.is_empty() {
            return Ok(0);
        }
        let (acknowledge, acknowledged) = oneshot::channel();
        self.sender
            .send(RuntimeCommand::EndConversationDataErasure {
                conversation_ids,
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
#[allow(clippy::large_enum_variant)]
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
    BeginConversationDataErasure {
        conversation_ids: Vec<ConversationId>,
        acknowledge: oneshot::Sender<Result<usize, DataErasureError>>,
    },
    EndConversationDataErasure {
        conversation_ids: Vec<ConversationId>,
        acknowledge: oneshot::Sender<usize>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DataErasureError {
    #[error("data-erasure request exceeds the maximum of {maximum} direct conversations")]
    TooManyConversations { maximum: usize },
    #[error("data erasure is already active for person {person_id}")]
    AlreadyActive { person_id: PersonId },
    #[error("conversation {conversation_id} is already blocked by another erasure")]
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
    pending_tool_follow_ups: VecDeque<WorldEvent>,
    tool_action_budget_by_trace: HashMap<EventId, usize>,
    tool_action_budget_order: VecDeque<EventId>,
    closed_tool_budget_roots: HashSet<EventId>,
    closed_tool_budget_order: VecDeque<EventId>,
    state: WorkingState,
    data_erasure: DataErasureState,
    attention: AttentionSystem,
    max_trace_depth: u8,
    planner: Option<Planner>,
    services: Option<Arc<CoreServices>>,
    mind: Option<InstalledMindProvider>,
    executive: ExecutiveController,
}

#[derive(Clone)]
struct InstalledMindProvider {
    provider: Arc<dyn MindSnapshotProvider>,
    limits: MindSnapshotLimits,
    influence_mode: MindInfluenceMode,
    timeout: std::time::Duration,
}

impl std::fmt::Debug for InstalledMindProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstalledMindProvider")
            .field("limits", &self.limits)
            .field("influence_mode", &self.influence_mode)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct DataErasureState {
    conversations_by_person: HashMap<PersonId, Vec<ConversationId>>,
    standalone_conversations: HashSet<ConversationId>,
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

    fn begin_conversations(
        &mut self,
        state: &mut WorkingState,
        conversation_ids: Vec<ConversationId>,
    ) -> Result<usize, DataErasureError> {
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

        let removed = state.purge_conversation_domains(&conversation_ids)?;
        self.blocked_conversations
            .extend(conversation_ids.iter().copied());
        self.standalone_conversations.extend(conversation_ids);
        Ok(removed)
    }

    fn end_conversations(&mut self, conversation_ids: Vec<ConversationId>) -> usize {
        let mut resumed = 0;
        for conversation_id in conversation_ids {
            if self.standalone_conversations.remove(&conversation_id) {
                self.blocked_conversations.remove(&conversation_id);
                resumed += 1;
            }
        }
        resumed
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
                pending_tool_follow_ups: VecDeque::new(),
                tool_action_budget_by_trace: HashMap::new(),
                tool_action_budget_order: VecDeque::new(),
                closed_tool_budget_roots: HashSet::new(),
                closed_tool_budget_order: VecDeque::new(),
                state,
                data_erasure: DataErasureState::default(),
                attention: AttentionSystem,
                max_trace_depth: config.max_trace_depth,
                planner: None,
                services: None,
                mind: None,
                executive: ExecutiveController::default(),
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

    /// Installs the bounded Executive state holder used to enrich planner
    /// inputs. The controller owns policy and metadata only; it never runs a
    /// model or holds a lock across an await.
    #[must_use]
    pub fn with_executive(mut self, executive: ExecutiveController) -> Self {
        self.executive = executive;
        self
    }

    pub fn set_executive(&mut self, executive: ExecutiveController) {
        self.executive = executive;
    }

    #[must_use]
    pub fn executive(&self) -> &ExecutiveController {
        &self.executive
    }

    /// Installs Core services and uses their model backend for planning.
    pub fn install_services(&mut self, services: CoreServices) {
        let planner =
            Planner::new(services.model.clone()).with_max_trace_depth(self.max_trace_depth);
        self.planner = Some(planner);
        self.services = Some(Arc::new(services));
    }

    /// Installs the optional Mind v2 retrieval boundary. Retrieval is
    /// fail-soft and starts in shadow mode so V1-visible behavior is unchanged.
    #[must_use]
    pub fn with_mind_snapshot_provider(mut self, provider: Arc<dyn MindSnapshotProvider>) -> Self {
        self.install_mind_snapshot_provider(provider);
        self
    }

    pub fn install_mind_snapshot_provider(&mut self, provider: Arc<dyn MindSnapshotProvider>) {
        self.mind = Some(InstalledMindProvider {
            provider,
            limits: MindSnapshotLimits::default(),
            influence_mode: MindInfluenceMode::Shadow,
            timeout: DEFAULT_MIND_SNAPSHOT_TIMEOUT,
        });
    }

    /// Configures whether retrieved Mind state is disabled, shadow-only, or
    /// active. This has no effect until a provider is installed.
    #[must_use]
    pub fn with_mind_influence_mode(mut self, mode: MindInfluenceMode) -> Self {
        if let Some(mind) = self.mind.as_mut() {
            mind.influence_mode = mode;
        }
        self
    }

    pub fn set_mind_influence_mode(&mut self, mode: MindInfluenceMode) {
        if let Some(mind) = self.mind.as_mut() {
            mind.influence_mode = mode;
        }
    }

    pub fn set_mind_snapshot_limits(
        &mut self,
        limits: MindSnapshotLimits,
    ) -> Result<(), crate::MindValidationError> {
        limits.validate()?;
        if let Some(mind) = self.mind.as_mut() {
            mind.limits = limits;
        }
        Ok(())
    }

    pub fn set_mind_snapshot_timeout(&mut self, timeout: std::time::Duration) {
        if let Some(mind) = self.mind.as_mut() {
            mind.timeout = timeout.max(std::time::Duration::from_millis(1));
        }
    }

    #[must_use]
    pub fn planner(&self) -> Option<&Planner> {
        self.planner.as_ref()
    }

    #[must_use]
    pub fn services(&self) -> Option<&CoreServices> {
        self.services.as_deref()
    }

    #[cfg(test)]
    fn tool_actions_used(&self, event: &WorldEvent) -> usize {
        self.effective_tool_actions_used(event)
    }

    fn effective_tool_actions_used(&self, event: &WorldEvent) -> usize {
        let root = event.trace().root_event_id();
        if let Some(used) = self.tool_action_budget_by_trace.get(&root).copied() {
            return used;
        }
        if self.closed_tool_budget_roots.contains(&root) {
            return MAX_TOOL_ACTIONS_PER_TRACE;
        }
        // A derived event with no ledger entry may be a delayed child of a
        // completed trace. Treating it as a fresh budget would reset the
        // cumulative limit, so unknown descendants fail closed.
        if event.trace().depth() > 0
            || self.tool_action_budget_by_trace.len() >= MAX_TOOL_TRACE_BUDGET_ENTRIES
        {
            MAX_TOOL_ACTIONS_PER_TRACE
        } else {
            0
        }
    }

    fn tool_actions_remaining(&self, event: &WorldEvent) -> usize {
        MAX_TOOL_ACTIONS_PER_TRACE.saturating_sub(self.effective_tool_actions_used(event))
    }

    fn root_has_pending_tool_follow_up(&self, root: EventId) -> bool {
        self.pending_tool_follow_ups
            .iter()
            .any(|event| event.trace().root_event_id() == root)
    }

    fn enqueue_tool_follow_up(
        &mut self,
        event: WorldEvent,
        feedback: &mut Vec<RuntimeObservation>,
    ) {
        if self.pending_tool_follow_ups.len() < MAX_PENDING_TOOL_FOLLOW_UPS {
            self.pending_tool_follow_ups.push_back(event);
        } else if let ProcessingOutcome::Observed(feedback_observation) = self.process_event(event)
        {
            feedback.push(feedback_observation);
        }
    }

    /// Releases a completed root after its final action turn. Pending
    /// follow-ups pin the entry so a delayed child can never observe a reset
    /// budget. The ledger remains bounded while allowing sequential roots to
    /// reuse capacity.
    fn release_tool_budget_root_if_terminal(&mut self, root: EventId) {
        if self.root_has_pending_tool_follow_up(root) {
            return;
        }
        if self.tool_action_budget_by_trace.remove(&root).is_some() {
            self.tool_action_budget_order
                .retain(|candidate| *candidate != root);
            self.closed_tool_budget_roots.insert(root);
            self.closed_tool_budget_order
                .retain(|candidate| *candidate != root);
            self.closed_tool_budget_order.push_back(root);
            while self.closed_tool_budget_order.len() > MAX_CLOSED_TOOL_TRACE_TOMBSTONES {
                let Some(expired) = self.closed_tool_budget_order.pop_front() else {
                    break;
                };
                self.closed_tool_budget_roots.remove(&expired);
            }
        }
    }

    fn touch_tool_budget_root(&mut self, root: EventId) -> bool {
        if self.closed_tool_budget_roots.contains(&root) {
            return false;
        }
        if self.tool_action_budget_by_trace.contains_key(&root) {
            if let Some(position) = self
                .tool_action_budget_order
                .iter()
                .position(|existing| *existing == root)
            {
                self.tool_action_budget_order.remove(position);
            }
        } else {
            if self.tool_action_budget_by_trace.len() >= MAX_TOOL_TRACE_BUDGET_ENTRIES {
                // Existing roots are never evicted: doing so could reset the
                // cumulative budget of a delayed child trace. A new root is
                // rejected once the bounded ledger is full.
                return false;
            }
            self.tool_action_budget_by_trace.insert(root, 0);
        }
        self.tool_action_budget_order.push_back(root);
        true
    }

    fn validate_tool_action_budget(
        &self,
        event: &WorldEvent,
        plan: &DecisionPlan,
    ) -> Result<(), PlannerError> {
        let requested = tool_intent_count(plan);
        let used = self.effective_tool_actions_used(event);
        if requested > MAX_TOOL_ACTIONS_PER_TRACE.saturating_sub(used) {
            return Err(PlannerError::InvalidOutput(
                PlannerOutputValidationError::ToolActionBudgetExceeded {
                    used,
                    requested,
                    maximum: MAX_TOOL_ACTIONS_PER_TRACE,
                },
            ));
        }
        Ok(())
    }

    /// Reserves one or more tool-action budget units immediately before
    /// dispatch. The caller must run the whole-plan preflight first so a
    /// malformed or over-budget plan cannot partially execute.
    fn reserve_tool_actions(
        &mut self,
        event: &WorldEvent,
        requested: usize,
    ) -> Result<(), PlannerError> {
        if requested == 0 {
            return Ok(());
        }
        let root = event.trace().root_event_id();
        let used = self.effective_tool_actions_used(event);
        if requested > MAX_TOOL_ACTIONS_PER_TRACE.saturating_sub(used) {
            return Err(PlannerError::InvalidOutput(
                PlannerOutputValidationError::ToolActionBudgetExceeded {
                    used,
                    requested,
                    maximum: MAX_TOOL_ACTIONS_PER_TRACE,
                },
            ));
        }
        if !self.touch_tool_budget_root(root) {
            return Err(PlannerError::InvalidOutput(
                PlannerOutputValidationError::ToolActionBudgetExceeded {
                    used: MAX_TOOL_ACTIONS_PER_TRACE,
                    requested,
                    maximum: MAX_TOOL_ACTIONS_PER_TRACE,
                },
            ));
        }
        let entry = self
            .tool_action_budget_by_trace
            .get_mut(&root)
            .expect("touched tool budget root must exist");
        *entry += requested;
        Ok(())
    }

    pub async fn process_next(&mut self) -> Option<ProcessingOutcome> {
        let event = self.next_event().await?;
        let root = event.trace().root_event_id();
        let outcome = self.process_event(event);
        // This observe-only queue API consumes the event without handing it to
        // a planner, so an otherwise terminal tool root can be released here.
        self.release_tool_budget_root_if_terminal(root);
        Some(outcome)
    }

    async fn next_event(&mut self) -> Option<WorldEvent> {
        loop {
            if let Some(event) = self.pending_tool_follow_ups.pop_front() {
                if !self.data_erasure.blocks(&event) {
                    return Some(event);
                }
                self.release_tool_budget_root_if_terminal(event.trace().root_event_id());
                continue;
            }
            match self.receiver.recv().await? {
                RuntimeCommand::Event(event) => {
                    if !self.data_erasure.blocks(&event) {
                        return Some(event);
                    }
                    self.release_tool_budget_root_if_terminal(event.trace().root_event_id());
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
                RuntimeCommand::BeginConversationDataErasure {
                    conversation_ids,
                    acknowledge,
                } => {
                    let result = self
                        .data_erasure
                        .begin_conversations(&mut self.state, conversation_ids);
                    let _ = acknowledge.send(result);
                }
                RuntimeCommand::EndConversationDataErasure {
                    conversation_ids,
                    acknowledge,
                } => {
                    let _ = acknowledge.send(self.data_erasure.end_conversations(conversation_ids));
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
        self.executive.observe_expectations(&event);
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
        let root = event.trace().root_event_id();
        let result = self.process_event_with_planner_inner(event).await;
        // This API does not dispatch actions itself. A plan that still
        // contains tools remains live for its caller; every other outcome,
        // including planner errors, is terminal once no child is pending.
        let keeps_root_live = matches!(
            &result,
            Ok(PlannedProcessingOutcome::Planned { plan, .. })
                if tool_intent_count(plan) > 0
        );
        if !keeps_root_live {
            self.release_tool_budget_root_if_terminal(root);
        }
        result
    }

    async fn process_event_with_planner_inner(
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
        self.validate_tool_action_budget(&planner_event, &plan)?;
        validate_intent_targets(&planner_event, &plan)?;
        let deferred_due_resolution = deferred_due_open_loop_resolution(&planner_event, &plan);
        self.apply_state_updates(&input, &plan, deferred_due_resolution)
            .await?;
        record_planner_decision(&self.executive, &input, &plan, None);
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
    /// runtime. Tool results may enqueue bounded follow-up planner turns;
    /// queue and trace limits remain the loop-safety boundary.
    pub async fn process_event_with_planner_and_actions(
        &mut self,
        event: WorldEvent,
        arbiter: &ActionArbiter,
        port: &dyn ActionPort,
    ) -> Result<PlannedProcessingOutcome, PlannerError> {
        let root = event.trace().root_event_id();
        let result = self
            .process_event_with_planner_and_actions_inner(event, arbiter, port)
            .await;
        // Any completed or failed turn is terminal once no child is pending;
        // retaining errored roots indefinitely would let malformed/backend
        // failure traffic exhaust the bounded ledger and deny new requests.
        self.release_tool_budget_root_if_terminal(root);
        result
    }

    async fn process_event_with_planner_and_actions_inner(
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
        let tool_actions_remaining = self.tool_actions_remaining(&planner_event);
        let capabilities = arbiter
            .config()
            .capabilities
            .actions()
            .iter()
            .filter(|descriptor| {
                descriptor.capability != crate::ActionCapability::UseTool
                    || tool_actions_remaining > 0
            })
            .cloned()
            .collect();
        let input = self
            .planner_input_with_context(planner_event.clone())
            .await
            .with_capabilities(capabilities);
        validate_due_open_loop_context(&input)?;
        let plan = planner.plan(&input).await?;
        if let Err(error) = self.validate_tool_action_budget(&planner_event, &plan) {
            release_unexecuted_tool_intents(&planner_event, &plan, port).await;
            return Err(error);
        }
        if let Err(error) = validate_intent_targets(&planner_event, &plan) {
            release_unexecuted_tool_intents(&planner_event, &plan, port).await;
            return Err(error);
        }
        let due_open_loop = due_open_loop_id(&planner_event);
        let deferred_due_resolution = deferred_due_open_loop_resolution(&planner_event, &plan);
        let applied_state_updates = match self
            .apply_state_updates(&input, &plan, deferred_due_resolution)
            .await
        {
            Ok(applied) => applied,
            Err(error) => {
                release_unexecuted_tool_intents(&planner_event, &plan, port).await;
                return Err(error);
            }
        };
        let mut all_due_deliveries_succeeded = true;
        let mut due_terminal_non_success = false;
        let mut actions = Vec::with_capacity(plan.intents.len());
        let mut feedback = Vec::new();
        let mut tool_follow_up_events = Vec::new();
        let mut selected_action = None;
        for (intent_index, intent) in plan.intents.iter().enumerate() {
            let tool_notification_policy = intent.tool_notification_policy().unwrap_or_default();
            let mut proposed = match intent.propose_action() {
                Ok(proposed) => proposed,
                Err(error) => {
                    release_unexecuted_tool_intents_from(&planner_event, &plan, intent_index, port)
                        .await;
                    return Err(PlannerError::InvalidOutput(
                        PlannerOutputValidationError::InvalidIntent(error),
                    ));
                }
            };
            if proposed.actor().is_none()
                && let Some(actor) = trusted_action_actor(&planner_event)
            {
                proposed = proposed.with_actor(actor);
            }
            if let Err(error) =
                apply_planned_action_idempotency(&mut proposed, &planner_event, intent_index)
            {
                release_unexecuted_tool_intents_from(&planner_event, &plan, intent_index, port)
                    .await;
                return Err(PlannerError::InvalidOutput(
                    PlannerOutputValidationError::InvalidIntent(
                        crate::IntentValidationError::Action(error),
                    ),
                ));
            }
            if matches!(&proposed, crate::ProposedAction::UseTool(_))
                && let Err(error) = self.reserve_tool_actions(&planner_event, 1)
            {
                release_unexecuted_tool_intents_from(&planner_event, &plan, intent_index, port)
                    .await;
                return Err(error);
            }
            if selected_action.is_none() && !matches!(&proposed, crate::ProposedAction::Noop) {
                selected_action = Some(proposed.clone());
            }
            let result = arbiter.dispatch(proposed.clone(), port).await;
            if matches!(
                &result,
                ActionResult::Rejected(rejection)
                    if !matches!(rejection, crate::ActionRejection::Duplicate { .. })
            ) {
                // The arbiter rejected the action before calling the port, so
                // any host-side capability registered for this action is still
                // live and must be released explicitly. A duplicate may refer
                // to another dispatch that has already crossed this boundary,
                // so its capability must be left alone.
                port.release_unexecuted(&proposed).await;
            }
            let replay_terminal = match &result {
                ActionResult::Rejected(crate::ActionRejection::Duplicate {
                    idempotency_key,
                    original_action_id,
                    ..
                }) => arbiter.terminal_outcome(idempotency_key, *original_action_id),
                _ => None,
            };
            let delivery_succeeded = matches!(
                &result,
                ActionResult::Executed {
                    outcome: crate::ActionPortOutcome::Delivered { .. },
                    ..
                } | ActionResult::Executed {
                    outcome: crate::ActionPortOutcome::ToolCompleted { .. },
                    ..
                }
            ) || replay_terminal
                == Some(crate::arbiter::AdmittedTerminal::Succeeded);
            let delivery_indeterminate = matches!(
                &result,
                ActionResult::Executed {
                    outcome: crate::ActionPortOutcome::DeliveryIndeterminate { .. },
                    ..
                }
            ) || replay_terminal
                == Some(crate::arbiter::AdmittedTerminal::Indeterminate);
            let terminal_non_success = delivery_indeterminate
                || matches!(
                    &result,
                    ActionResult::Executed {
                        outcome: crate::ActionPortOutcome::ToolFailed { .. },
                        ..
                    } | ActionResult::Rejected(
                        crate::ActionRejection::TargetUnavailable { .. }
                            | crate::ActionRejection::DeliveryResolutionFailed { .. },
                    ) | ActionResult::Failed {
                        error: crate::ActionPortError {
                            retryable: false,
                            ..
                        },
                        ..
                    }
                )
                || replay_terminal == Some(crate::arbiter::AdmittedTerminal::Failed);
            if due_open_loop.is_some() && !matches!(&proposed, crate::ProposedAction::Noop) {
                if terminal_non_success {
                    due_terminal_non_success = true;
                    all_due_deliveries_succeeded = false;
                } else if !delivery_succeeded {
                    all_due_deliveries_succeeded = false;
                }
            }
            if matches!(&proposed, crate::ProposedAction::UseTool(_)) {
                // A successful duplicate already produced its result during
                // the original turn. Replaying it as a rejection would turn a
                // known success into a false ToolFailed follow-up.
                if !duplicate_action_already_succeeded(&result, replay_terminal)
                    && let Some(tool_event) = tool_follow_up_event(
                        &planner_event,
                        &proposed,
                        &result,
                        self.max_trace_depth,
                    )
                {
                    tool_follow_up_events
                        .push(tool_event.with_tool_notification_policy(tool_notification_policy));
                }
            } else if let Some(feedback_event) =
                action_result_event(&planner_event, &proposed, &result, self.max_trace_depth)
            {
                // Non-tool action feedback retains its historical behavior,
                // including the defensive case where a host returns a tool
                // outcome for a different proposed action.
                if tool_event_requires_follow_up(&feedback_event) {
                    self.enqueue_tool_follow_up(feedback_event, &mut feedback);
                } else if let ProcessingOutcome::Observed(feedback_observation) =
                    self.process_event(feedback_event)
                {
                    feedback.push(feedback_observation);
                }
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
        let mut final_tool_follow_ups = Vec::new();
        for tool_event in tool_follow_up_events {
            match tool_event.tool_notification_policy().unwrap_or_default() {
                crate::ToolNotificationPolicy::Final => final_tool_follow_ups.push(tool_event),
                crate::ToolNotificationPolicy::Each => {
                    if let Some(tool_follow_up) = bounded_single_tool_follow_up_event(
                        &planner_event,
                        tool_event,
                        self.max_trace_depth,
                    ) {
                        self.enqueue_tool_follow_up(tool_follow_up, &mut feedback);
                    }
                }
                crate::ToolNotificationPolicy::EachAndFinal => {
                    if let Some(tool_follow_up) = bounded_single_tool_follow_up_event(
                        &planner_event,
                        tool_event.clone(),
                        self.max_trace_depth,
                    ) {
                        self.enqueue_tool_follow_up(tool_follow_up, &mut feedback);
                    }
                    final_tool_follow_ups.push(tool_event);
                }
            }
        }
        if let Some(tool_follow_up) = aggregate_tool_follow_up_events(
            &planner_event,
            final_tool_follow_ups,
            self.max_trace_depth,
        ) {
            self.enqueue_tool_follow_up(tool_follow_up, &mut feedback);
        }
        if due_open_loop.is_some() && due_terminal_non_success {
            self.defer_due_open_loop_without_schedule(&input, applied_state_updates)
                .await?;
        } else if deferred_due_resolution.is_some() && all_due_deliveries_succeeded {
            self.resolve_due_open_loop(&input, applied_state_updates)
                .await?;
        }
        record_planner_decision(&self.executive, &input, &plan, selected_action.as_ref());
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
    /// have retrieved durable memories, open loops, or goals can extend the
    /// returned input before calling [`Planner::plan`].
    pub fn planner_input(&self, event: WorldEvent) -> PlannerInput {
        let conversation = event
            .scope()
            .conversation_id()
            .and_then(|conversation_id| self.state.conversation(conversation_id));
        let executive_scope = executive_scope_for_event(&event);
        PlannerInput::new(
            event,
            PlannerStateSnapshot::new(self.state.global_version(), conversation),
        )
        .with_executive(self.executive.snapshot_for_scope(&executive_scope))
    }

    /// Builds a planner input and opportunistically hydrates bounded durable
    /// context from the installed Core service ports. Storage failures are
    /// intentionally non-fatal during migration: a host can bring the new
    /// runtime online before every legacy store has been moved.
    pub async fn planner_input_with_context(&self, event: WorldEvent) -> PlannerInput {
        let mut input = self.planner_input(event.clone());
        let mind = self.retrieve_mind_snapshot(&input).await;
        input = input.with_mind(mind);

        let Some(services) = self.services.as_ref() else {
            return input;
        };
        let conversation_id = event.scope().conversation_id();
        let person_id = event_person_id(&event).or_else(|| match event.scope() {
            EventScope::Person { person_id } => Some(person_id),
            _ => None,
        });

        let mut memories = Vec::new();
        let mut open_loops = Vec::new();
        let mut goals = Vec::new();

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
            hydrate_goal_owner(
                services.goals.as_ref(),
                GoalOwner::Conversation(conversation_id),
                &mut goals,
            )
            .await;
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
            hydrate_goal_owner(
                services.goals.as_ref(),
                GoalOwner::Person(person_id),
                &mut goals,
            )
            .await;
        }

        match event.scope() {
            EventScope::Global => {
                hydrate_goal_owner(services.goals.as_ref(), GoalOwner::Global, &mut goals).await;
            }
            EventScope::Goal { goal_id } => {
                if let Ok(Some(goal)) = services.goals.get(goal_id).await
                    && goal.id() == goal_id
                {
                    let owner = goal.owner();
                    extend_unique_goals(&mut goals, [goal]);
                    hydrate_goal_owner(services.goals.as_ref(), owner, &mut goals).await;
                }
            }
            EventScope::Conversation { .. } | EventScope::Person { .. } => {}
        }

        input = input
            .with_memories(memories)
            .with_open_loops(open_loops)
            .with_goals(goals);

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

    async fn retrieve_mind_snapshot(&self, input: &PlannerInput) -> crate::MindSnapshot {
        let Some(mind) = self.mind.as_ref() else {
            return crate::MindSnapshot::empty();
        };
        if mind.influence_mode == MindInfluenceMode::Disabled {
            return crate::MindSnapshot::empty();
        }
        let topic = input
            .state
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.current_topic.as_deref());
        let Ok(request) =
            MindSnapshotRequest::for_event(&input.event, topic, mind.limits, mind.influence_mode)
        else {
            return crate::MindSnapshot::empty();
        };
        match timeout(mind.timeout, mind.provider.snapshot(&request)).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(_)) | Err(_) => crate::MindSnapshot::empty(),
        }
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
                StateUpdateProposal::ConversationDirective {
                    conversation_id, ..
                } if event.scope().conversation_id() != Some(*conversation_id) => {
                    return Err(state_update_error(
                        "conversation_directive",
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
                StateUpdateProposal::DeferOpenLoop { open_loop_id, .. }
                    if !planner_input_contains_open_loop(input, *open_loop_id) =>
                {
                    return Err(state_update_error(
                        "defer_open_loop",
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
                StateUpdateProposal::ConversationDirective { .. } => {}
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
                StateUpdateProposal::DeferOpenLoop {
                    open_loop_id,
                    due_at,
                } => {
                    let services = self.services.clone().ok_or_else(|| {
                        state_update_error(
                            "defer_open_loop",
                            "Core services are unavailable",
                            applied_updates,
                        )
                    })?;
                    services
                        .open_loops
                        .defer(*open_loop_id, *due_at, Utc::now())
                        .await
                        .map_err(|error| {
                            state_update_error(
                                "defer_open_loop",
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

    async fn defer_due_open_loop_without_schedule(
        &mut self,
        input: &PlannerInput,
        applied_before_failure: usize,
    ) -> Result<(), PlannerError> {
        let event = &input.event;
        let open_loop_id = due_open_loop_id(event).ok_or_else(|| {
            state_update_error(
                "defer_open_loop",
                "the event is not an open-loop due event",
                applied_before_failure,
            )
        })?;
        if !planner_input_contains_open_loop(input, open_loop_id) {
            return Err(state_update_error(
                "defer_open_loop",
                format!("open loop {open_loop_id} was not hydrated for the event owner"),
                applied_before_failure,
            ));
        }
        let services = self.services.as_ref().ok_or_else(|| {
            state_update_error(
                "defer_open_loop",
                "Core services are unavailable",
                applied_before_failure,
            )
        })?;
        services
            .open_loops
            .defer(open_loop_id, None, Utc::now())
            .await
            .map_err(|error| {
                state_update_error("defer_open_loop", error.to_string(), applied_before_failure)
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

/// Append only bounded metadata that is useful for future arbitration.
/// PlannerInput deliberately has no model-confidence field, so zero means
/// "unknown" rather than inventing confidence from event salience or output
/// quality. A failed record write is telemetry loss, not a runtime failure.
fn record_planner_decision(
    executive: &ExecutiveController,
    input: &PlannerInput,
    plan: &DecisionPlan,
    materialized_action: Option<&crate::ProposedAction>,
) {
    let capability = &input.executive.cognitive_capability;
    let selected_cognitive_tier = capability.current_tier;
    let fallback_used = selected_cognitive_tier == crate::CognitiveTier::Intrinsic
        && capability.preferred_tier.is_strong();
    let mut record = DecisionRecord::new(input.event.id(), plan.disposition, Utc::now());
    record.selected_action = materialized_action
        .and_then(decision_action_kind_from_proposed_action)
        .or_else(|| {
            plan.intents
                .iter()
                .find_map(decision_action_kind_from_intent)
        });
    record.selected_action_id = materialized_action.and_then(crate::ProposedAction::action_id);
    record.relevant_goals = input
        .executive
        .prioritized_goals
        .iter()
        .take(crate::MAX_SNAPSHOT_ITEMS)
        .map(|goal| goal.goal_id)
        .collect();
    record.relevant_agenda_items = input
        .mind
        .agenda()
        .iter()
        .take(crate::MAX_SNAPSHOT_ITEMS)
        .map(|item| item.id)
        .collect();
    record.relevant_conflicts = input
        .executive
        .active_conflicts
        .iter()
        .take(crate::MAX_SNAPSHOT_ITEMS)
        .map(|conflict| conflict.id)
        .collect();
    record.selected_cognitive_tier = selected_cognitive_tier;
    record.fallback_used = fallback_used;
    record.intrinsic_model_version =
        if selected_cognitive_tier == crate::CognitiveTier::Intrinsic || fallback_used {
            capability.intrinsic_version.clone()
        } else {
            None
        };
    record.reason_tags = decision_reason_tags(input, selected_cognitive_tier, fallback_used);

    let _ = executive.record_decision_for_scope(executive_scope_for_event(&input.event), record);
}

fn decision_action_kind_from_intent(intent: &crate::CognitiveIntent) -> Option<DecisionActionKind> {
    Some(match intent {
        crate::CognitiveIntent::SendMessage { .. } => DecisionActionKind::SendMessage,
        crate::CognitiveIntent::ReachOut(_) => DecisionActionKind::ReachOut,
        crate::CognitiveIntent::UseTool { .. } => DecisionActionKind::UseTool,
        crate::CognitiveIntent::CreateOpenLoop(_) => DecisionActionKind::CreateOpenLoop,
        crate::CognitiveIntent::ResolveOpenLoop { .. } => DecisionActionKind::ResolveOpenLoop,
        crate::CognitiveIntent::StartGoal(_) => DecisionActionKind::StartGoal,
        crate::CognitiveIntent::CancelGoal { .. } => DecisionActionKind::CancelGoal,
        crate::CognitiveIntent::Noop => return None,
    })
}

fn decision_action_kind_from_proposed_action(
    action: &crate::ProposedAction,
) -> Option<DecisionActionKind> {
    Some(match action {
        crate::ProposedAction::SendMessage(_) => DecisionActionKind::SendMessage,
        crate::ProposedAction::ReachOut(_) => DecisionActionKind::ReachOut,
        crate::ProposedAction::UseTool(_) => DecisionActionKind::UseTool,
        crate::ProposedAction::CreateOpenLoop(_) => DecisionActionKind::CreateOpenLoop,
        crate::ProposedAction::ResolveOpenLoop(_) => DecisionActionKind::ResolveOpenLoop,
        crate::ProposedAction::StartGoal(_) => DecisionActionKind::StartGoal,
        crate::ProposedAction::CancelGoal(_) => DecisionActionKind::CancelGoal,
        crate::ProposedAction::Noop => return None,
    })
}

fn decision_reason_tags(
    input: &PlannerInput,
    selected_cognitive_tier: crate::CognitiveTier,
    fallback_used: bool,
) -> Vec<ExecutiveReasonTag> {
    let capability = &input.executive.cognitive_capability;
    let mut tags = Vec::new();
    let mut add = |tag| {
        if !tags.contains(&tag) && tags.len() < crate::MAX_REASON_TAGS {
            tags.push(tag);
        }
    };

    if !capability.strong_available {
        add(ExecutiveReasonTag::StrongModelUnavailable);
    }
    if !capability.intrinsic_health.can_serve() {
        add(ExecutiveReasonTag::IntrinsicModelUnavailable);
    }
    if selected_cognitive_tier == crate::CognitiveTier::Intrinsic {
        add(ExecutiveReasonTag::CognitiveTierIntrinsic);
    }
    if selected_cognitive_tier == crate::CognitiveTier::Reflex {
        add(ExecutiveReasonTag::ReflexOnly);
    }
    if selected_cognitive_tier < capability.preferred_tier {
        add(ExecutiveReasonTag::CognitiveTierDowngraded);
    }
    if fallback_used {
        add(ExecutiveReasonTag::IntrinsicFallbackUsed);
    }
    if !input.executive.active_conflicts.is_empty() {
        add(ExecutiveReasonTag::ConflictHigh);
    }
    if !input.executive.pending_expectations.is_empty() {
        add(ExecutiveReasonTag::ExpectationPending);
    }
    if input
        .executive
        .active_plan
        .as_ref()
        .is_some_and(|plan| plan.stale_reason.is_some())
    {
        add(ExecutiveReasonTag::PlanStale);
    }
    tags
}

fn event_person_id(event: &WorldEvent) -> Option<PersonId> {
    match event.kind() {
        crate::WorldEventKind::MessageReceived(message) => Some(message.sender),
        crate::WorldEventKind::InteractionCuesObserved(cues) => Some(cues.person_id),
        _ => None,
    }
}

fn executive_scope_for_event(event: &WorldEvent) -> ExecutiveScope {
    match event.scope() {
        EventScope::Global => ExecutiveScope::Global,
        EventScope::Conversation { conversation_id } => {
            ExecutiveScope::Conversation { conversation_id }
        }
        EventScope::Person { person_id } => ExecutiveScope::Person { person_id },
        EventScope::Goal { goal_id } => ExecutiveScope::Goal { goal_id },
    }
}

fn due_open_loop_id(event: &WorldEvent) -> Option<crate::OpenLoopId> {
    match event.kind() {
        crate::WorldEventKind::ProspectiveMemoryDue(due) => Some(due.open_loop_id),
        _ => None,
    }
}

/// Derive the exact action key used by both capability-aware planners and the
/// runtime's final `ProposedAction`.
#[must_use]
pub fn planned_action_idempotency_key(event: &WorldEvent, intent_index: usize) -> String {
    due_open_loop_id(event).map_or_else(
        || crate::event_action_idempotency_key(event.id(), intent_index),
        |open_loop_id| format!("open-loop:{open_loop_id}:delivery:{intent_index}"),
    )
}

fn apply_action_idempotency_key(
    action: &mut crate::ProposedAction,
    idempotency_key: String,
) -> Result<(), crate::ActionValidationError> {
    let metadata = match action {
        crate::ProposedAction::SendMessage(action) => &mut action.metadata,
        crate::ProposedAction::ReachOut(action) => &mut action.metadata,
        crate::ProposedAction::UseTool(action) => &mut action.metadata,
        crate::ProposedAction::CreateOpenLoop(action) => &mut action.metadata,
        crate::ProposedAction::ResolveOpenLoop(action) => &mut action.metadata,
        crate::ProposedAction::StartGoal(action) => &mut action.metadata,
        crate::ProposedAction::CancelGoal(action) => &mut action.metadata,
        crate::ProposedAction::Noop => return Ok(()),
    };
    metadata.idempotency_key = idempotency_key;
    metadata.validate()
}

fn apply_planned_action_idempotency(
    action: &mut crate::ProposedAction,
    event: &WorldEvent,
    intent_index: usize,
) -> Result<(), crate::ActionValidationError> {
    apply_action_idempotency_key(action, planned_action_idempotency_key(event, intent_index))
}

#[cfg(test)]
fn apply_due_action_idempotency(
    action: &mut crate::ProposedAction,
    open_loop_id: crate::OpenLoopId,
    intent_index: usize,
) -> Result<(), crate::ActionValidationError> {
    apply_action_idempotency_key(
        action,
        format!("open-loop:{open_loop_id}:delivery:{intent_index}"),
    )
}

#[cfg(test)]
fn apply_event_action_idempotency(
    action: &mut crate::ProposedAction,
    event_id: crate::EventId,
    intent_index: usize,
) -> Result<(), crate::ActionValidationError> {
    apply_action_idempotency_key(
        action,
        crate::event_action_idempotency_key(event_id, intent_index),
    )
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

fn tool_intent_count(plan: &DecisionPlan) -> usize {
    plan.intents
        .iter()
        .filter(|intent| matches!(intent, crate::CognitiveIntent::UseTool { .. }))
        .count()
}

/// Release host reservations for tool intents that were materialized by a
/// planner but never entered the arbiter/port execution boundary. This is a
/// best-effort cleanup hook: a validated planner intent should always convert
/// to an action, while a malformed custom backend is simply ignored here.
async fn release_unexecuted_tool_intents(
    event: &WorldEvent,
    plan: &DecisionPlan,
    port: &dyn ActionPort,
) {
    release_unexecuted_tool_intents_from(event, plan, 0, port).await;
}

async fn release_unexecuted_tool_intents_from(
    event: &WorldEvent,
    plan: &DecisionPlan,
    start_index: usize,
    port: &dyn ActionPort,
) {
    for (intent_index, intent) in plan.intents.iter().enumerate().skip(start_index) {
        if !matches!(intent, crate::CognitiveIntent::UseTool { .. }) {
            continue;
        }
        let Ok(mut action) = intent.propose_action() else {
            continue;
        };
        if action.actor().is_none()
            && let Some(actor) = trusted_action_actor(event)
        {
            action = action.with_actor(actor);
        }
        if apply_planned_action_idempotency(&mut action, event, intent_index).is_ok() {
            port.release_unexecuted(&action).await;
        }
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
            (scope, crate::CognitiveIntent::UseTool { scope: target, .. }) => {
                scope_matches_action_scope(scope, *target)
            }
            (scope, crate::CognitiveIntent::CreateOpenLoop(draft)) => scope_matches_action_scope(
                scope,
                crate::ActionScope::for_open_loop_owner(draft.owner()),
            ),
            (scope, crate::CognitiveIntent::ResolveOpenLoop { owner, .. }) => {
                scope_matches_action_scope(scope, crate::ActionScope::for_open_loop_owner(*owner))
            }
            (scope, crate::CognitiveIntent::StartGoal(draft)) => {
                scope_matches_action_scope(scope, crate::ActionScope::for_goal_owner(draft.owner()))
            }
            (scope, crate::CognitiveIntent::CancelGoal { owner, .. }) => {
                scope_matches_action_scope(scope, crate::ActionScope::for_goal_owner(*owner))
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
                crate::CognitiveIntent::UseTool { tool_name, .. } => format!(
                    "use_tool `{tool_name}` targets {:?} from {:?}",
                    intent.action_scope(),
                    event.scope()
                ),
                _ => format!(
                    "intent targets {:?} from {:?}",
                    intent.action_scope(),
                    event.scope()
                ),
            };
            return Err(PlannerError::InvalidOutput(
                PlannerOutputValidationError::IntentOutsideEventScope { reason },
            ));
        }
    }
    Ok(())
}

fn scope_matches_action_scope(event_scope: EventScope, action_scope: crate::ActionScope) -> bool {
    match (event_scope, action_scope) {
        (EventScope::Global, crate::ActionScope::Global) => true,
        (
            EventScope::Conversation { conversation_id },
            crate::ActionScope::Conversation(target),
        ) => conversation_id == target,
        (EventScope::Person { person_id }, crate::ActionScope::Person(target)) => {
            person_id == target
        }
        (EventScope::Goal { .. }, crate::ActionScope::Global) => false,
        _ => false,
    }
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

async fn hydrate_goal_owner(
    store: &dyn crate::GoalStore,
    owner: GoalOwner,
    target: &mut Vec<crate::Goal>,
) {
    let remaining = MAX_PLANNER_GOALS.saturating_sub(target.len());
    let limit = remaining.min(MAX_GOALS_PER_CONTEXT_OWNER);
    if limit == 0 {
        return;
    }
    if let Ok(listed) = store.list(&owner, limit).await {
        extend_unique_goals(
            target,
            listed
                .into_iter()
                .filter(|goal| goal.owner() == owner)
                .take(limit),
        );
    }
}

fn extend_unique_goals(
    target: &mut Vec<crate::Goal>,
    values: impl IntoIterator<Item = crate::Goal>,
) {
    for goal in values {
        if target.len() >= MAX_PLANNER_GOALS {
            break;
        }
        if !target.iter().any(|existing| existing.id() == goal.id()) {
            target.push(goal);
        }
    }
}

fn trusted_action_actor(event: &WorldEvent) -> Option<crate::PersonId> {
    match event.kind() {
        crate::WorldEventKind::MessageReceived(message) => Some(message.sender),
        crate::WorldEventKind::ToolCompleted(_) | crate::WorldEventKind::ToolFailed(_) => {
            event.actor()
        }
        _ => None,
    }
}

fn duplicate_action_already_succeeded(
    result: &ActionResult,
    terminal: Option<crate::arbiter::AdmittedTerminal>,
) -> bool {
    matches!(
        result,
        ActionResult::Rejected(crate::ActionRejection::Duplicate { .. })
    ) && terminal == Some(crate::arbiter::AdmittedTerminal::Succeeded)
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
            outcome: crate::ActionPortOutcome::ToolCompleted { operation, output },
            ..
        } => crate::WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
            operation: bounded_tool_text(
                operation,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_OPERATION_BYTES,
            ),
            output: bounded_tool_text(output, MAX_TOOL_RESULT_CHARS, MAX_TOOL_RESULT_BYTES),
            requires_follow_up: true,
        }),
        ActionResult::Executed {
            outcome:
                crate::ActionPortOutcome::ToolFailed {
                    operation,
                    error_category,
                    detail,
                },
            ..
        } => crate::WorldEventKind::ToolFailed(crate::ToolFailedEvent {
            operation: bounded_tool_text(
                operation,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_OPERATION_BYTES,
            ),
            error_category: bounded_tool_text(
                error_category,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_ERROR_CATEGORY_BYTES,
            ),
            detail: bounded_tool_text(
                detail,
                crate::MAX_TOOL_ERROR_DETAIL_CHARS,
                crate::MAX_TOOL_ERROR_DETAIL_BYTES,
            ),
            requires_follow_up: true,
        }),
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Delivered { .. },
            ..
        } => {
            crate::WorldEventKind::ActionSucceeded(crate::ActionSucceededEvent { idempotency_key })
        }
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::DeliveryIndeterminate { reason, .. },
            ..
        } => crate::WorldEventKind::ActionFailed(crate::ActionFailedEvent {
            idempotency_key,
            error_category: format!("delivery_indeterminate:{reason}"),
        }),
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
    let event = WorldEvent::derived_from(
        parent,
        Utc::now(),
        scope,
        EventPriority::High,
        kind,
        max_trace_depth,
    )
    .ok()?;
    Some(match action.actor() {
        Some(actor) => event.with_actor(actor),
        None => event,
    })
}

/// Converts every tool action result into one follow-up event. Normal adapter
/// completions retain their richer result kind; failures that happen before a
/// `ToolCompleted`/`ToolFailed` outcome are normalized to `ToolFailed` so a
/// sibling batch cannot hide a requested operation.
fn tool_follow_up_event(
    parent: &WorldEvent,
    action: &crate::ProposedAction,
    result: &ActionResult,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    let event = action_result_event(parent, action, result, max_trace_depth);
    if event.as_ref().is_some_and(|event| {
        matches!(
            event.kind(),
            WorldEventKind::ToolCompleted(_) | WorldEventKind::ToolFailed(_)
        )
    }) {
        return event;
    }
    tool_failure_follow_up_event(parent, action, result, max_trace_depth)
}

/// Normalizes failures that happen outside the tool adapter into the same
/// follow-up envelope as an ordinary `ToolFailed` result. Without this, a
/// mixed batch could hide rejected/deferred operations from the model.
fn tool_failure_follow_up_event(
    parent: &WorldEvent,
    action: &crate::ProposedAction,
    result: &ActionResult,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    let crate::ProposedAction::UseTool(tool) = action else {
        return None;
    };
    let (category, detail) = match result {
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Deferred { reason },
            ..
        } => ("deferred".to_owned(), reason.clone()),
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::DeliveryIndeterminate { reason, .. },
            ..
        } => ("delivery_indeterminate".to_owned(), reason.clone()),
        ActionResult::Failed { error, .. } => (error.category.clone(), error.to_string()),
        ActionResult::Rejected(rejection) => ("rejected".to_owned(), rejection.to_string()),
        ActionResult::Executed {
            outcome: crate::ActionPortOutcome::Delivered { .. },
            ..
        } => (
            "unexpected_tool_outcome".to_owned(),
            "tool action returned a message delivery outcome".to_owned(),
        ),
        ActionResult::Noop => (
            "noop".to_owned(),
            "tool action produced no result".to_owned(),
        ),
        ActionResult::Executed {
            outcome:
                crate::ActionPortOutcome::ToolCompleted { .. }
                | crate::ActionPortOutcome::ToolFailed { .. },
            ..
        } => return None,
    };
    let scope = match tool.scope {
        crate::ActionScope::Conversation(conversation_id) => {
            EventScope::Conversation { conversation_id }
        }
        crate::ActionScope::Person(person_id) => EventScope::Person { person_id },
        crate::ActionScope::Global => EventScope::Global,
    };
    let event = WorldEvent::derived_from(
        parent,
        Utc::now(),
        scope,
        EventPriority::High,
        WorldEventKind::ToolFailed(crate::ToolFailedEvent {
            operation: bounded_tool_text(
                &tool.tool_name,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_OPERATION_BYTES,
            ),
            error_category: bounded_tool_text(
                &category,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_ERROR_CATEGORY_BYTES,
            ),
            detail: bounded_tool_text(
                &detail,
                crate::MAX_TOOL_ERROR_DETAIL_CHARS,
                crate::MAX_TOOL_ERROR_DETAIL_BYTES,
            ),
            requires_follow_up: true,
        }),
        max_trace_depth,
    )
    .ok()?;
    Some(match action.actor() {
        Some(actor) => event.with_actor(actor),
        None => event,
    })
}

/// Coalesces sibling tool results from one planner turn into one follow-up
/// event. Keeping the aggregate as a normal `ToolCompleted` event preserves
/// the existing attention/state path while giving the model one coherent
/// result set to summarize.
fn aggregate_tool_follow_up_events(
    parent: &WorldEvent,
    mut events: Vec<WorldEvent>,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    match events.len() {
        0 => return None,
        1 => {
            return bounded_single_tool_follow_up_event(parent, events.pop()?, max_trace_depth);
        }
        _ => {}
    }

    let mut operation_limit = MAX_TOOL_BATCH_OPERATION_CHARS;
    let mut field_limit = INITIAL_TOOL_BATCH_FIELD_CHARS;
    let output = loop {
        let encoded = serialize_tool_batch(&events, operation_limit, field_limit)?;
        if encoded.len() <= MAX_TOOL_RESULT_BYTES
            && encoded.chars().count() <= MAX_TOOL_RESULT_CHARS
        {
            break encoded;
        }

        if field_limit > 0 {
            field_limit = field_limit.saturating_sub(field_limit.div_ceil(2));
        } else if operation_limit > 1 {
            operation_limit = operation_limit.saturating_sub(operation_limit.div_ceil(2));
        } else {
            // The fixed envelope itself should fit comfortably under the
            // event bound. Returning None is safer than emitting an invalid
            // event if a future schema change makes that assumption false.
            return None;
        }
    };

    let actor = events
        .first()
        .and_then(WorldEvent::actor)
        .filter(|actor| events.iter().all(|event| event.actor() == Some(*actor)));
    let notification_policy = if events.iter().any(|event| {
        event.tool_notification_policy() == Some(crate::ToolNotificationPolicy::EachAndFinal)
    }) {
        crate::ToolNotificationPolicy::EachAndFinal
    } else {
        crate::ToolNotificationPolicy::Final
    };
    let event = WorldEvent::derived_from(
        parent,
        Utc::now(),
        parent.scope(),
        EventPriority::High,
        WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
            operation: "core.tool_batch".to_owned(),
            output,
            requires_follow_up: true,
        }),
        max_trace_depth,
    )
    .ok()?
    .with_tool_notification_policy(notification_policy);
    Some(match actor {
        Some(actor) => event.with_actor(actor),
        None => event,
    })
}

fn serialize_tool_batch(
    events: &[WorldEvent],
    operation_limit: usize,
    field_limit: usize,
) -> Option<String> {
    let results = events
        .iter()
        .map(|event| {
            let mut result = serde_json::Map::new();
            match event.kind() {
                WorldEventKind::ToolCompleted(tool) => {
                    result.insert(
                        "operation".to_owned(),
                        serde_json::Value::String(bounded_tool_batch_text(
                            &tool.operation,
                            operation_limit,
                        )),
                    );
                    result.insert(
                        "status".to_owned(),
                        serde_json::Value::String("completed".to_owned()),
                    );
                    result.insert(
                        "output".to_owned(),
                        serde_json::Value::String(bounded_tool_batch_text(
                            &tool.output,
                            field_limit,
                        )),
                    );
                    result.insert("error".to_owned(), serde_json::Value::Null);
                }
                WorldEventKind::ToolFailed(tool) => {
                    result.insert(
                        "operation".to_owned(),
                        serde_json::Value::String(bounded_tool_batch_text(
                            &tool.operation,
                            operation_limit,
                        )),
                    );
                    result.insert(
                        "status".to_owned(),
                        serde_json::Value::String("failed".to_owned()),
                    );
                    result.insert(
                        "output".to_owned(),
                        serde_json::Value::String(String::new()),
                    );
                    let mut error = serde_json::Map::new();
                    error.insert(
                        "category".to_owned(),
                        serde_json::Value::String(bounded_tool_batch_text(
                            &tool.error_category,
                            field_limit,
                        )),
                    );
                    error.insert(
                        "detail".to_owned(),
                        serde_json::Value::String(bounded_tool_batch_text(
                            &tool.detail,
                            field_limit,
                        )),
                    );
                    result.insert("error".to_owned(), serde_json::Value::Object(error));
                }
                _ => return None,
            }
            Some(serde_json::Value::Object(result))
        })
        .collect::<Option<Vec<_>>>()?;
    serde_json::to_string(&serde_json::json!({ "results": results })).ok()
}

fn bounded_tool_batch_text(value: &str, max_chars: usize) -> String {
    bounded_tool_text(value, max_chars, MAX_TOOL_RESULT_BYTES)
}

fn bounded_tool_text(value: &str, max_chars: usize, max_bytes: usize) -> String {
    let mut bounded = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars().take(max_chars) {
        if bounded.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        bounded.push(character);
    }
    bounded
}

fn bounded_single_tool_follow_up_event(
    parent: &WorldEvent,
    event: WorldEvent,
    max_trace_depth: u8,
) -> Option<WorldEvent> {
    if event.validate(max_trace_depth).is_ok() {
        return Some(event);
    }

    let kind = match event.kind() {
        WorldEventKind::ToolCompleted(tool) => {
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: bounded_tool_text(
                    &tool.operation,
                    MAX_TOOL_BATCH_OPERATION_CHARS,
                    MAX_TOOL_OPERATION_BYTES,
                ),
                output: bounded_tool_text(
                    &tool.output,
                    MAX_TOOL_RESULT_CHARS,
                    MAX_TOOL_RESULT_BYTES,
                ),
                requires_follow_up: tool.requires_follow_up,
            })
        }
        WorldEventKind::ToolFailed(tool) => WorldEventKind::ToolFailed(crate::ToolFailedEvent {
            operation: bounded_tool_text(
                &tool.operation,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_OPERATION_BYTES,
            ),
            error_category: bounded_tool_text(
                &tool.error_category,
                MAX_TOOL_BATCH_OPERATION_CHARS,
                MAX_TOOL_ERROR_CATEGORY_BYTES,
            ),
            detail: bounded_tool_text(
                &tool.detail,
                crate::MAX_TOOL_ERROR_DETAIL_CHARS,
                crate::MAX_TOOL_ERROR_DETAIL_BYTES,
            ),
            requires_follow_up: tool.requires_follow_up,
        }),
        _ => return None,
    };
    let actor = event.actor();
    let notification_policy = event.tool_notification_policy();
    let bounded = WorldEvent::derived_from(
        parent,
        event.occurred_at(),
        event.scope(),
        event.priority(),
        kind,
        max_trace_depth,
    )
    .ok()?;
    bounded.validate(max_trace_depth).ok()?;
    let bounded = match notification_policy {
        Some(policy) => bounded.with_tool_notification_policy(policy),
        None => bounded,
    };
    Some(match actor {
        Some(actor) => bounded.with_actor(actor),
        None => bounded,
    })
}

fn tool_event_requires_follow_up(event: &WorldEvent) -> bool {
    match event.kind() {
        WorldEventKind::ToolCompleted(tool) => tool.requires_follow_up,
        WorldEventKind::ToolFailed(tool) => tool.requires_follow_up,
        _ => false,
    }
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
    let (conversation_id, content) = match action {
        crate::ProposedAction::SendMessage(send) => {
            if delivered_conversation_id.is_some_and(|delivered| delivered != send.conversation_id)
            {
                return None;
            }
            (send.conversation_id, send.content.clone())
        }
        crate::ProposedAction::ReachOut(reach_out) => {
            ((*delivered_conversation_id)?, reach_out.message.clone())
        }
        crate::ProposedAction::UseTool(_)
        | crate::ProposedAction::CreateOpenLoop(_)
        | crate::ProposedAction::ResolveOpenLoop(_)
        | crate::ProposedAction::StartGoal(_)
        | crate::ProposedAction::CancelGoal(_) => return None,
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
            content: Some(content),
        }),
        max_trace_depth,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, CognitiveRuntime, DataErasureError, MAX_DATA_ERASURE_CONVERSATIONS,
        MAX_GOALS_PER_CONTEXT_OWNER, PlannedProcessingOutcome, ProcessingOutcome, RuntimeConfig,
        RuntimeConfigError, SubmitError, aggregate_tool_follow_up_events,
        apply_due_action_idempotency, apply_event_action_idempotency, bounded_conversation_ids,
        duplicate_action_already_succeeded, message_sent_event, tool_follow_up_event,
        validate_intent_targets,
    };
    use crate::arbiter::{
        ActionArbiter, ActionArbiterConfig, ActionPort, ActionPortFuture, ActionPortOutcome,
        ActionPortReleaseFuture, ActionReceipt, ActionRejection, ActionResult,
        EnvironmentCapabilities,
    };
    use crate::event::{
        EventPriority, EventScope, EventValidationError, GoalUpdatedEvent,
        InteractionCuesObservedEvent, MessageContent, MessageReceivedEvent, WorldEvent,
        WorldEventKind,
    };
    use crate::goal::{Goal, GoalDraft, GoalKind, GoalOwner};
    use crate::identity::{
        ConversationId, ConversationKind, EventId, GoalId, MessageId, OpenLoopId, PersonId,
    };
    use crate::memory::{Memory, MemoryDraft, MemoryKind, MemoryQuery, MemoryScope};
    use crate::mind::{
        MindInfluenceMode, MindSnapshot, MindSnapshotFuture, MindSnapshotProvider,
        MindSnapshotRequest, SelfModel, SelfModelSnapshot,
    };
    use crate::open_loop::{OpenLoop, OpenLoopDraft, OpenLoopKind, OpenLoopOwner, OpenLoopStatus};
    use crate::planner::{
        AffectState, DecisionDisposition, DecisionPlan, InteractionCues, ModelBackend,
        ModelBackendError, ModelBackendFuture, PlannerError, PlannerInput, RelationState,
        StateUpdateProposal,
    };
    use crate::ports::{
        AffectStore, AffectStoreFuture, CoreServices, GoalStore, GoalStoreFuture, MemoryStore,
        MemoryStoreFuture, OpenLoopStore, OpenLoopStoreFuture, RelationStore, RelationStoreFuture,
    };
    use crate::{DecisionActionKind, ExecutiveScope, ToolNotificationPolicy};
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct StaticMindProvider {
        snapshot: MindSnapshot,
    }

    impl MindSnapshotProvider for StaticMindProvider {
        fn snapshot<'a>(&'a self, _request: &'a MindSnapshotRequest) -> MindSnapshotFuture<'a> {
            let snapshot = self.snapshot.clone();
            Box::pin(async move { Ok(snapshot) })
        }
    }

    struct SlowMindProvider;

    impl MindSnapshotProvider for SlowMindProvider {
        fn snapshot<'a>(&'a self, _request: &'a MindSnapshotRequest) -> MindSnapshotFuture<'a> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(MindSnapshot::empty())
            })
        }
    }

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
                visible_reply_allowed: true,
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

    struct ToolModel {
        conversation_id: ConversationId,
    }

    impl ModelBackend for ToolModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            Box::pin(async move {
                Ok(DecisionPlan::new(DecisionDisposition::Reply).with_intent(
                    crate::CognitiveIntent::use_tool(
                        "calculator",
                        r#"{"expression":"1+1"}"#,
                        crate::ActionScope::Conversation(conversation_id),
                    ),
                ))
            })
        }
    }

    struct ToolWithInvalidStateModel {
        conversation_id: ConversationId,
        foreign_conversation_id: ConversationId,
    }

    impl ModelBackend for ToolWithInvalidStateModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            let foreign_conversation_id = self.foreign_conversation_id;
            Box::pin(async move {
                Ok(DecisionPlan::new(DecisionDisposition::SpecialAction)
                    .with_intent(crate::CognitiveIntent::use_tool(
                        "calculator",
                        r#"{"expression":"1+1"}"#,
                        crate::ActionScope::Conversation(conversation_id),
                    ))
                    // This target is deliberately outside the incoming
                    // conversation. Runtime preflight must reject it before
                    // dispatching the tool, then release the capability.
                    .with_state_update(StateUpdateProposal::SetTopic {
                        conversation_id: foreign_conversation_id,
                        topic: "invalid target".to_owned(),
                    }))
            })
        }
    }

    struct BudgetProbeModel {
        conversation_id: ConversationId,
        saw_tool_capability: Arc<Mutex<Option<bool>>>,
    }

    impl ModelBackend for BudgetProbeModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            *self.saw_tool_capability.lock().expect("budget probe lock") =
                Some(input.supports(crate::ActionCapability::UseTool));
            let conversation_id = self.conversation_id;
            Box::pin(async move {
                Ok(
                    DecisionPlan::new(DecisionDisposition::SpecialAction).with_intent(
                        crate::CognitiveIntent::use_tool(
                            "calculator",
                            r#"{"expression":"1+1"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                        ),
                    ),
                )
            })
        }
    }

    struct MultiToolModel {
        conversation_id: ConversationId,
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for MultiToolModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let call_number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let conversation_id = self.conversation_id;
            let plan = match input.event.kind() {
                WorldEventKind::MessageReceived(_) => {
                    DecisionPlan::new(DecisionDisposition::SpecialAction)
                        .with_intent(crate::CognitiveIntent::use_tool(
                            "weather.current",
                            r#"{"location":"成都"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                        ))
                        .with_intent(crate::CognitiveIntent::use_tool(
                            "web.search",
                            r#"{"query":"猫眼星云"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                        ))
                }
                WorldEventKind::ToolCompleted(tool) => {
                    assert_eq!(call_number, 2);
                    assert_eq!(tool.operation, "core.tool_batch");
                    let payload: serde_json::Value =
                        serde_json::from_str(&tool.output).expect("tool batch JSON");
                    let results = payload
                        .get("results")
                        .and_then(serde_json::Value::as_array)
                        .expect("tool batch results");
                    assert_eq!(results.len(), 2);
                    let operations = results
                        .iter()
                        .filter_map(|result| {
                            result.get("operation").and_then(serde_json::Value::as_str)
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(operations, ["weather.current", "web.search"]);
                    assert!(results.iter().all(|result| {
                        result.get("status").is_some()
                            && result.get("output").is_some()
                            && result.get("error").is_some()
                    }));
                    DecisionPlan::new(DecisionDisposition::Reply).with_intent(
                        crate::CognitiveIntent::send_message(
                            conversation_id,
                            MessageContent::text("已整理查询结果"),
                        ),
                    )
                }
                WorldEventKind::ToolFailed(_) => {
                    panic!("multi-tool test should aggregate failures too")
                }
                _ => DecisionPlan::silent(),
            };
            Box::pin(async move { Ok(plan) })
        }
    }

    struct NotificationPolicyModel {
        conversation_id: ConversationId,
        policy: ToolNotificationPolicy,
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for NotificationPolicyModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let conversation_id = self.conversation_id;
            let policy = self.policy;
            let plan = match input.event.kind() {
                WorldEventKind::MessageReceived(_) => {
                    DecisionPlan::new(DecisionDisposition::SpecialAction)
                        .with_intent(crate::CognitiveIntent::use_tool_with_notification_policy(
                            "weather.current",
                            r#"{"location":"成都"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                            policy,
                        ))
                        .with_intent(crate::CognitiveIntent::use_tool_with_notification_policy(
                            "web.search",
                            r#"{"query":"猫眼星云"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                            policy,
                        ))
                }
                WorldEventKind::ToolCompleted(_) | WorldEventKind::ToolFailed(_) => {
                    DecisionPlan::new(DecisionDisposition::Reply).with_intent(
                        crate::CognitiveIntent::send_message(
                            conversation_id,
                            MessageContent::text("已完成一项查询"),
                        ),
                    )
                }
                _ => DecisionPlan::silent(),
            };
            Box::pin(async move { Ok(plan) })
        }
    }

    struct MixedOutcomeToolModel {
        conversation_id: ConversationId,
        expected_statuses: Vec<&'static str>,
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for MixedOutcomeToolModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let conversation_id = self.conversation_id;
            let expected_statuses = self.expected_statuses.clone();
            let plan = match input.event.kind() {
                WorldEventKind::MessageReceived(_) => {
                    DecisionPlan::new(DecisionDisposition::SpecialAction)
                        .with_intent(crate::CognitiveIntent::use_tool(
                            "weather.current",
                            r#"{"location":"成都"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                        ))
                        .with_intent(crate::CognitiveIntent::use_tool(
                            "web.search",
                            r#"{"query":"猫眼星云"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                        ))
                }
                WorldEventKind::ToolCompleted(tool) => {
                    assert_eq!(tool.operation, "core.tool_batch");
                    let payload: serde_json::Value =
                        serde_json::from_str(&tool.output).expect("tool batch JSON");
                    let statuses = payload
                        .get("results")
                        .and_then(serde_json::Value::as_array)
                        .expect("tool batch results")
                        .iter()
                        .filter_map(|result| {
                            result.get("status").and_then(serde_json::Value::as_str)
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(statuses, expected_statuses);
                    DecisionPlan::new(DecisionDisposition::Reply).with_intent(
                        crate::CognitiveIntent::send_message(
                            conversation_id,
                            MessageContent::text("已整理查询结果"),
                        ),
                    )
                }
                _ => DecisionPlan::silent(),
            };
            Box::pin(async move { Ok(plan) })
        }
    }

    struct ToolFollowUpModel {
        conversation_id: ConversationId,
        calls: Arc<AtomicUsize>,
        recurse_on_result: bool,
    }

    impl ModelBackend for ToolFollowUpModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let call_number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let conversation_id = self.conversation_id;
            let plan = match input.event.kind() {
                WorldEventKind::MessageReceived(_) => {
                    assert!(input.supports(crate::ActionCapability::UseTool));
                    DecisionPlan::new(DecisionDisposition::SpecialAction).with_intent(
                        crate::CognitiveIntent::use_tool(
                            "calculator",
                            r#"{"expression":"1+1"}"#,
                            crate::ActionScope::Conversation(conversation_id),
                        ),
                    )
                }
                WorldEventKind::ToolCompleted(tool) => {
                    assert!(tool.requires_follow_up);
                    assert_eq!(tool.output, "2");
                    assert!(input.supports(crate::ActionCapability::UseTool));
                    // Exercise one bounded tool chain, then finish with a
                    // visible reply so the test does not rely on trace-limit
                    // exhaustion to terminate.
                    if self.recurse_on_result && call_number == 2 {
                        DecisionPlan::new(DecisionDisposition::SpecialAction).with_intent(
                            crate::CognitiveIntent::use_tool(
                                "calculator",
                                r#"{"expression":"2+2"}"#,
                                crate::ActionScope::Conversation(conversation_id),
                            ),
                        )
                    } else {
                        DecisionPlan::new(DecisionDisposition::Reply).with_intent(
                            crate::CognitiveIntent::send_message(
                                conversation_id,
                                MessageContent::text("结果是 2"),
                            ),
                        )
                    }
                }
                _ => DecisionPlan::silent(),
            };
            Box::pin(async move { Ok(plan) })
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

    struct DueReachOutModel {
        person_id: PersonId,
        open_loop_id: OpenLoopId,
    }

    impl ModelBackend for DueReachOutModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let person_id = self.person_id;
            let open_loop_id = self.open_loop_id;
            Box::pin(async move {
                let reach_out = crate::ReachOutIntent::from_parts(
                    person_id,
                    MessageContent::text("checking in"),
                    crate::ProactiveMotive::FollowUp,
                )
                .map(crate::CognitiveIntent::reach_out)
                .expect("valid due reach-out");
                Ok(DecisionPlan::new(DecisionDisposition::Reply)
                    .with_intent(reach_out)
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

    struct DueToolActionModel {
        conversation_id: ConversationId,
        open_loop_id: OpenLoopId,
    }

    impl ModelBackend for DueToolActionModel {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            let conversation_id = self.conversation_id;
            let open_loop_id = self.open_loop_id;
            Box::pin(async move {
                Ok(DecisionPlan::new(DecisionDisposition::SpecialAction)
                    .with_intent(crate::CognitiveIntent::use_tool(
                        "calculator",
                        r#"{"expression":"1+1"}"#,
                        crate::ActionScope::Conversation(conversation_id),
                    ))
                    .with_state_update(StateUpdateProposal::ResolveOpenLoop { open_loop_id }))
            })
        }
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

    struct CountingPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for CountingPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ActionPortOutcome::ToolCompleted {
                    operation: "calculator".to_owned(),
                    output: "2".to_owned(),
                })
            })
        }
    }

    struct ReleaseRecordingPort {
        calls: Arc<AtomicUsize>,
        released_keys: Arc<Mutex<Vec<String>>>,
    }

    impl ActionPort for ReleaseRecordingPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ActionPortOutcome::ToolCompleted {
                    operation: "calculator".to_owned(),
                    output: "2".to_owned(),
                })
            })
        }

        fn release_unexecuted<'a>(
            &'a self,
            action: &'a crate::ProposedAction,
        ) -> ActionPortReleaseFuture<'a> {
            Box::pin(async move {
                if let Some(key) = action.idempotency_key() {
                    self.released_keys
                        .lock()
                        .expect("released key recorder lock")
                        .push(key.to_owned());
                }
            })
        }
    }

    struct ToolThenDeliveryPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for ToolThenDeliveryPort {
        fn execute<'a>(&'a self, action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = match action {
                crate::ProposedAction::UseTool(tool) => ActionPortOutcome::ToolCompleted {
                    operation: tool.tool_name.clone(),
                    output: "2".to_string(),
                },
                crate::ProposedAction::SendMessage(send) => ActionPortOutcome::Delivered {
                    external_reference: Some("fake-message".to_string()),
                    message_id: Some(MessageId::new()),
                    conversation_id: Some(send.conversation_id),
                },
                _ => ActionPortOutcome::Deferred {
                    reason: "unexpected_action".to_string(),
                },
            };
            Box::pin(async move { Ok(outcome) })
        }
    }

    struct MixedToolPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for MixedToolPort {
        fn execute<'a>(&'a self, action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = match action {
                crate::ProposedAction::UseTool(tool) if tool.tool_name == "weather.current" => {
                    ActionPortOutcome::ToolCompleted {
                        operation: tool.tool_name.clone(),
                        output: "晴".to_owned(),
                    }
                }
                crate::ProposedAction::UseTool(_) => ActionPortOutcome::Deferred {
                    reason: "search temporarily unavailable".to_owned(),
                },
                crate::ProposedAction::SendMessage(send) => ActionPortOutcome::Delivered {
                    external_reference: Some("fake-message".to_owned()),
                    message_id: Some(MessageId::new()),
                    conversation_id: Some(send.conversation_id),
                },
                _ => ActionPortOutcome::Deferred {
                    reason: "unexpected_action".to_owned(),
                },
            };
            Box::pin(async move { Ok(outcome) })
        }
    }

    struct AllFailedToolPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for AllFailedToolPort {
        fn execute<'a>(&'a self, action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(action, crate::ProposedAction::UseTool(_)) {
                Box::pin(async { Err(crate::ActionPortError::new("upstream_unavailable", true)) })
            } else {
                Box::pin(async {
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some("fake-message".to_owned()),
                        message_id: Some(MessageId::new()),
                        conversation_id: None,
                    })
                })
            }
        }
    }

    struct ActorRecordingPort {
        actor: Arc<Mutex<Option<PersonId>>>,
    }

    impl ActionPort for ActorRecordingPort {
        fn execute<'a>(&'a self, action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            let actor = Arc::clone(&self.actor);
            let action_actor = action.actor();
            Box::pin(async move {
                *actor.lock().expect("actor recording lock") = action_actor;
                Ok(ActionPortOutcome::Delivered {
                    external_reference: Some("tool".to_owned()),
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

    struct TerminalFailingActionPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for TerminalFailingActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(crate::ActionPortError::new("delivery_rejected", false)) })
        }
    }

    struct ToolFailedActionPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for ToolFailedActionPort {
        fn execute<'a>(&'a self, _action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ActionPortOutcome::ToolFailed {
                    operation: "calculator".to_owned(),
                    error_category: "invalid_expression".to_owned(),
                    detail: "expression rejected".to_owned(),
                })
            })
        }
    }

    struct IndeterminateActionPort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for IndeterminateActionPort {
        fn execute<'a>(&'a self, action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let conversation_id = match action.scope() {
                crate::ActionScope::Conversation(conversation_id) => Some(conversation_id),
                crate::ActionScope::Person(_) | crate::ActionScope::Global => None,
            };
            Box::pin(async move {
                Ok(ActionPortOutcome::DeliveryIndeterminate {
                    reason: "durable replay barrier".to_owned(),
                    conversation_id,
                })
            })
        }
    }

    struct DeliverThenIndeterminatePort {
        calls: Arc<AtomicUsize>,
    }

    impl ActionPort for DeliverThenIndeterminatePort {
        fn execute<'a>(&'a self, action: &'a crate::ProposedAction) -> ActionPortFuture<'a> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            let conversation_id = match action.scope() {
                crate::ActionScope::Conversation(conversation_id) => Some(conversation_id),
                crate::ActionScope::Person(_) | crate::ActionScope::Global => None,
            };
            Box::pin(async move {
                if attempt == 0 {
                    Ok(ActionPortOutcome::Delivered {
                        external_reference: Some("fake".to_owned()),
                        message_id: None,
                        conversation_id,
                    })
                } else {
                    Ok(ActionPortOutcome::DeliveryIndeterminate {
                        reason: "second delivery outcome unknown".to_owned(),
                        conversation_id,
                    })
                }
            })
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

    struct RejectingDeliveryResolver {
        resolution_failed: bool,
    }

    impl crate::DeliveryResolver for RejectingDeliveryResolver {
        fn resolve<'a>(&'a self, person_id: PersonId) -> crate::DeliveryResolverFuture<'a> {
            let resolution_failed = self.resolution_failed;
            Box::pin(async move {
                if resolution_failed {
                    Err(crate::DeliveryResolutionError::failed(
                        std::io::Error::other("route store unavailable"),
                    ))
                } else {
                    Err(crate::DeliveryResolutionError::Unavailable { person_id })
                }
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
    struct TestGoalStore {
        goals: Vec<Goal>,
        fetched_ids: Mutex<Vec<GoalId>>,
        listed_owners: Mutex<Vec<(GoalOwner, usize)>>,
    }

    impl TestGoalStore {
        fn with_goals(goals: Vec<Goal>) -> Self {
            Self {
                goals,
                ..Self::default()
            }
        }

        fn goal(id: GoalId, owner: GoalOwner, title: impl Into<String>) -> Goal {
            let draft =
                GoalDraft::new(owner, GoalKind::Project, title).expect("valid test goal draft");
            Goal::from_draft(id, &draft, Utc::now()).expect("valid test goal")
        }
    }

    impl GoalStore for TestGoalStore {
        fn get(&self, id: GoalId) -> GoalStoreFuture<'_, Option<Goal>> {
            Box::pin(async move {
                self.fetched_ids
                    .lock()
                    .expect("goal fetch recorder lock")
                    .push(id);
                Ok(self.goals.iter().find(|goal| goal.id() == id).cloned())
            })
        }

        fn list<'a>(
            &'a self,
            owner: &'a GoalOwner,
            limit: usize,
        ) -> GoalStoreFuture<'a, Vec<Goal>> {
            Box::pin(async move {
                self.listed_owners
                    .lock()
                    .expect("goal list recorder lock")
                    .push((*owner, limit));
                // Deliberately ignore owner and limit; the runtime owns its context boundary.
                Ok(self.goals.clone())
            })
        }
    }

    #[derive(Default)]
    struct TestOpenLoopStore {
        listed_owners: Mutex<Vec<OpenLoopOwner>>,
        resolved: Mutex<Vec<OpenLoopId>>,
        deferred: Mutex<Vec<(OpenLoopId, Option<chrono::DateTime<Utc>>)>>,
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
            due_at: Option<chrono::DateTime<Utc>>,
            _now: chrono::DateTime<Utc>,
        ) -> OpenLoopStoreFuture<'_, OpenLoop> {
            Box::pin(async move {
                self.deferred
                    .lock()
                    .expect("open-loop recorder lock")
                    .push((id, due_at));
                Ok(Self::open_loop(id, OpenLoopOwner::Global))
            })
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
        reads: Mutex<Vec<PersonId>>,
        updates: Mutex<Vec<(PersonId, AffectState)>>,
    }

    impl AffectStore for TestAffectStore {
        fn get<'a>(&'a self, person_id: PersonId) -> AffectStoreFuture<'a, AffectState> {
            Box::pin(async move {
                self.reads
                    .lock()
                    .expect("affect recorder lock")
                    .push(person_id);
                Ok(AffectState::default())
            })
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
        reads: Mutex<Vec<PersonId>>,
        updates: Mutex<Vec<RelationState>>,
    }

    impl RelationStore for TestRelationStore {
        fn get<'a>(
            &'a self,
            person_id: PersonId,
        ) -> RelationStoreFuture<'a, Option<RelationState>> {
            Box::pin(async move {
                self.reads
                    .lock()
                    .expect("relation recorder lock")
                    .push(person_id);
                Ok(None)
            })
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

    struct InteractionCueModel;

    impl ModelBackend for InteractionCueModel {
        fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            Box::pin(async move {
                let WorldEventKind::InteractionCuesObserved(observed) = input.event.kind() else {
                    return Err(ModelBackendError::InvalidPlan {
                        reason: "expected interaction cue event".to_owned(),
                    });
                };
                let evolved = crate::apply_interaction_cues(
                    observed.person_id,
                    input.relation,
                    input.affect,
                    observed.cues(),
                )
                .map_err(|error| ModelBackendError::InvalidPlan {
                    reason: error.to_string(),
                })?;
                Ok(DecisionPlan::silent()
                    .with_state_update(StateUpdateProposal::Affect(evolved.affect))
                    .with_state_update(StateUpdateProposal::Relation(evolved.relation)))
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

        let planned_event = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::HostStarted,
        );
        let planned_event_id = planned_event.id();
        runtime
            .process_event_with_planner(planned_event)
            .await
            .expect("attended event should plan");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.state().global_version(), 3);
        let records = runtime.executive().snapshot().recent_decisions;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_id, planned_event_id);
        assert_eq!(records[0].disposition, DecisionDisposition::Silent);
        assert_eq!(records[0].selected_action, None);
        assert_eq!(records[0].selected_action_id, None);
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
        let action_id = match &output {
            PlannedProcessingOutcome::Planned { actions, .. } => match actions.first() {
                Some(ActionResult::Executed { receipt, .. }) => receipt.action_id,
                _ => None,
            },
            _ => None,
        };
        assert!(matches!(
            output,
            PlannedProcessingOutcome::Planned {
                actions,
                feedback,
                ..
            } if actions.len() == 1 && feedback.len() == 1
        ));
        assert_eq!(runtime.state().global_version(), 2);
        let records = runtime
            .executive()
            .snapshot_for_scope(&ExecutiveScope::Conversation { conversation_id })
            .recent_decisions;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].disposition, DecisionDisposition::Reply);
        assert_eq!(
            records[0].selected_action,
            Some(DecisionActionKind::SendMessage)
        );
        assert_eq!(records[0].selected_action_id, action_id);
    }

    #[tokio::test]
    async fn runtime_stamps_message_sender_on_model_tool_actions() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ToolModel { conversation_id }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let actor = Arc::new(Mutex::new(None));
        let port = ActorRecordingPort {
            actor: Arc::clone(&actor),
        };
        let output = runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, sender),
                &arbiter,
                &port,
            )
            .await
            .expect("tool action should be dispatched");
        assert!(
            matches!(output, PlannedProcessingOutcome::Planned { actions, .. } if actions.len() == 1)
        );
        assert_eq!(*actor.lock().expect("actor recording lock"), Some(sender));
    }

    #[tokio::test]
    async fn tool_output_gets_one_causal_follow_up_turn_and_visible_delivery() {
        let conversation_id = ConversationId::new();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ToolFollowUpModel {
                conversation_id,
                calls: Arc::clone(&model_calls),
                recurse_on_result: false,
            }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port_calls = Arc::new(AtomicUsize::new(0));
        let port = ToolThenDeliveryPort {
            calls: Arc::clone(&port_calls),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, PersonId::new()),
                &arbiter,
                &port,
            )
            .await
            .expect("tool request should execute");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [ActionResult::Executed {
                        outcome: ActionPortOutcome::ToolCompleted { output, .. },
                        ..
                    }] if output == "2"
                )
        ));
        assert_eq!(runtime.pending_tool_follow_ups.len(), 1);

        let second = runtime
            .process_next_with_planner_and_actions(&arbiter, &port)
            .await
            .expect("tool result should be queued")
            .expect("tool follow-up planning should succeed");
        assert!(matches!(
            second,
            PlannedProcessingOutcome::Planned {
                observation,
                actions,
                ..
            } if observation.event_type == crate::EventType::ToolCompleted
                && matches!(
                    actions.as_slice(),
                    [ActionResult::Executed {
                        outcome: ActionPortOutcome::Delivered { .. },
                        ..
                    }]
                )
        ));
        assert!(runtime.pending_tool_follow_ups.is_empty());
        assert_eq!(model_calls.load(Ordering::SeqCst), 2);
        assert_eq!(port_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn one_core_turn_dispatches_multiple_tool_intents() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(MultiToolModel {
                conversation_id,
                calls: Arc::clone(&model_calls),
            }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port_calls = Arc::new(AtomicUsize::new(0));
        let port = ToolThenDeliveryPort {
            calls: Arc::clone(&port_calls),
        };

        let outcome = runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, sender),
                &arbiter,
                &port,
            )
            .await
            .expect("multiple tool intents should dispatch");
        let keys = match &outcome {
            PlannedProcessingOutcome::Planned { actions, .. } => actions
                .iter()
                .filter_map(|action| match action {
                    ActionResult::Executed { receipt, .. } => receipt.idempotency_key.clone(),
                    ActionResult::Rejected(_)
                    | ActionResult::Failed { .. }
                    | ActionResult::Noop => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert!(matches!(
            outcome,
            PlannedProcessingOutcome::Planned { actions, .. } if actions.len() == 2
        ));
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
        assert_eq!(runtime.pending_tool_follow_ups.len(), 1);
        assert_eq!(model_calls.load(Ordering::SeqCst), 1);
        assert_eq!(port_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            runtime
                .pending_tool_follow_ups
                .front()
                .and_then(WorldEvent::actor),
            Some(sender)
        );
        let follow_up = runtime
            .process_next_with_planner_and_actions(&arbiter, &port)
            .await
            .expect("aggregated tool result should be queued")
            .expect("aggregated follow-up planning should succeed");
        assert!(matches!(
            follow_up,
            PlannedProcessingOutcome::Planned {
                actions,
                ..
            } if matches!(
                actions.as_slice(),
                [ActionResult::Executed {
                    outcome: ActionPortOutcome::Delivered { .. },
                    ..
                }]
            )
        ));
        assert!(runtime.pending_tool_follow_ups.is_empty());
        assert_eq!(model_calls.load(Ordering::SeqCst), 2);
        // Two tool executions and exactly one final visible delivery.
        assert_eq!(port_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn tool_notification_policy_controls_follow_up_message_cadence() {
        for (policy, expected_operations) in [
            (
                ToolNotificationPolicy::Each,
                vec!["weather.current", "web.search"],
            ),
            (
                ToolNotificationPolicy::EachAndFinal,
                vec!["weather.current", "web.search", "core.tool_batch"],
            ),
        ] {
            let conversation_id = ConversationId::new();
            let model_calls = Arc::new(AtomicUsize::new(0));
            let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
                RuntimeConfig::default(),
                CoreServices::with_model(NotificationPolicyModel {
                    conversation_id,
                    policy,
                    calls: Arc::clone(&model_calls),
                }),
            )
            .expect("valid runtime");
            let arbiter = ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            );
            let port_calls = Arc::new(AtomicUsize::new(0));
            let port = ToolThenDeliveryPort {
                calls: Arc::clone(&port_calls),
            };

            runtime
                .process_event_with_planner_and_actions(
                    direct_message(conversation_id, PersonId::new()),
                    &arbiter,
                    &port,
                )
                .await
                .expect("tool request should execute");

            let queued_operations = runtime
                .pending_tool_follow_ups
                .iter()
                .map(|event| match event.kind() {
                    WorldEventKind::ToolCompleted(tool) => tool.operation.as_str(),
                    WorldEventKind::ToolFailed(tool) => tool.operation.as_str(),
                    _ => panic!("only tool follow-ups should be queued"),
                })
                .collect::<Vec<_>>();
            assert_eq!(queued_operations, expected_operations);
            assert!(
                runtime
                    .pending_tool_follow_ups
                    .iter()
                    .all(|event| { event.tool_notification_policy() == Some(policy) })
            );

            let expected_replies = expected_operations.len();
            for _ in 0..expected_replies {
                let outcome = runtime
                    .process_next_with_planner_and_actions(&arbiter, &port)
                    .await
                    .expect("tool follow-up should be queued")
                    .expect("tool follow-up should plan");
                assert!(matches!(
                    outcome,
                    PlannedProcessingOutcome::Planned { actions, .. }
                        if matches!(
                            actions.as_slice(),
                            [ActionResult::Executed {
                                outcome: ActionPortOutcome::Delivered { .. },
                                ..
                            }]
                        )
                ));
            }
            assert!(runtime.pending_tool_follow_ups.is_empty());
            assert_eq!(model_calls.load(Ordering::SeqCst), expected_replies + 1);
            assert_eq!(port_calls.load(Ordering::SeqCst), expected_replies + 2);
        }
    }

    #[tokio::test]
    async fn mixed_tool_success_and_deferred_results_share_one_follow_up() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(MixedOutcomeToolModel {
                conversation_id,
                expected_statuses: vec!["completed", "failed"],
                calls: Arc::clone(&model_calls),
            }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port_calls = Arc::new(AtomicUsize::new(0));
        let port = MixedToolPort {
            calls: Arc::clone(&port_calls),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, sender),
                &arbiter,
                &port,
            )
            .await
            .expect("mixed tool turn should execute");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if actions.len() == 2
                    && matches!(
                        actions[0],
                        ActionResult::Executed {
                            outcome: ActionPortOutcome::ToolCompleted { .. },
                            ..
                        }
                    )
                    && matches!(
                        actions[1],
                        ActionResult::Executed {
                            outcome: ActionPortOutcome::Deferred { .. },
                            ..
                        }
                    )
        ));
        assert_eq!(runtime.pending_tool_follow_ups.len(), 1);

        let second = runtime
            .process_next_with_planner_and_actions(&arbiter, &port)
            .await
            .expect("mixed result should be queued")
            .expect("mixed follow-up should plan");
        assert!(matches!(
            second,
            PlannedProcessingOutcome::Planned {
                actions,
                observation,
                ..
            } if observation.event_type == crate::EventType::ToolCompleted
                && matches!(
                    actions.as_slice(),
                    [ActionResult::Executed {
                        outcome: ActionPortOutcome::Delivered { .. },
                        ..
                    }]
                )
        ));
        assert!(runtime.pending_tool_follow_ups.is_empty());
        assert_eq!(model_calls.load(Ordering::SeqCst), 2);
        assert_eq!(port_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn all_failed_tool_results_still_share_one_follow_up() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(MixedOutcomeToolModel {
                conversation_id,
                expected_statuses: vec!["failed", "failed"],
                calls: Arc::clone(&model_calls),
            }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port_calls = Arc::new(AtomicUsize::new(0));
        let port = AllFailedToolPort {
            calls: Arc::clone(&port_calls),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, sender),
                &arbiter,
                &port,
            )
            .await
            .expect("all-failed tool turn should return actions");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if actions.len() == 2
                    && actions
                        .iter()
                        .all(|action| matches!(action, ActionResult::Failed { .. }))
        ));
        assert_eq!(runtime.pending_tool_follow_ups.len(), 1);

        let second = runtime
            .process_next_with_planner_and_actions(&arbiter, &port)
            .await
            .expect("all-failed result should be queued")
            .expect("all-failed follow-up should plan");
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
        assert!(runtime.pending_tool_follow_ups.is_empty());
        assert_eq!(model_calls.load(Ordering::SeqCst), 2);
        assert_eq!(port_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn aggregated_tool_follow_up_is_structured_and_bounded() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let parent = direct_message(conversation_id, sender);
        let completed = WorldEvent::derived_from(
            &parent,
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: "weather.current".to_owned(),
                output: "晴".repeat(crate::MAX_TOOL_RESULT_CHARS),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("completed event");
        let failed = WorldEvent::derived_from(
            &parent,
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ToolFailed(crate::ToolFailedEvent {
                operation: "web.search".to_owned(),
                error_category: "upstream_timeout".to_owned(),
                detail: "e".repeat(crate::MAX_TOOL_ERROR_DETAIL_CHARS),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("failed event");
        let aggregate = aggregate_tool_follow_up_events(
            &parent,
            vec![completed.with_actor(sender), failed.with_actor(sender)],
            8,
        )
        .expect("aggregate event");
        assert_eq!(aggregate.actor(), Some(sender));
        assert!(aggregate.validate(8).is_ok());
        let WorldEventKind::ToolCompleted(tool) = aggregate.kind() else {
            panic!("aggregate must be a completed tool event");
        };
        assert_eq!(tool.operation, "core.tool_batch");
        assert!(tool.output.len() <= crate::MAX_TOOL_RESULT_BYTES);
        assert!(tool.output.chars().count() <= crate::MAX_TOOL_RESULT_CHARS);
        let payload: serde_json::Value =
            serde_json::from_str(&tool.output).expect("aggregate output JSON");
        let results = payload
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("aggregate results");
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0]
                .get("operation")
                .and_then(serde_json::Value::as_str),
            Some("weather.current")
        );
        assert_eq!(
            results[0].get("status").and_then(serde_json::Value::as_str),
            Some("completed")
        );
        assert!(results[0].get("output").is_some());
        assert!(results[0].get("error").is_some());
        assert_eq!(
            results[1]
                .get("operation")
                .and_then(serde_json::Value::as_str),
            Some("web.search")
        );
        assert_eq!(
            results[1].get("status").and_then(serde_json::Value::as_str),
            Some("failed")
        );
        assert!(results[1].get("output").is_some());
        assert!(results[1].get("error").is_some());
    }

    #[test]
    fn single_tool_follow_up_is_normalized_to_utf8_byte_limits() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let parent = direct_message(conversation_id, sender);
        let valid = WorldEvent::derived_from(
            &parent,
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: "weather.current".to_owned(),
                output: "晴".to_owned(),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("valid tool event")
        .with_actor(sender);
        let valid_id = valid.id();
        let preserved = aggregate_tool_follow_up_events(&parent, vec![valid], 8)
            .expect("valid single completion should be preserved");
        assert_eq!(preserved.id(), valid_id);

        let oversized_completed = WorldEvent::derived_from(
            &parent,
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: "x".repeat(2_000),
                output: "🌟".repeat(crate::MAX_TOOL_RESULT_CHARS + 1),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("trace should derive even before payload normalization")
        .with_actor(sender);
        assert!(oversized_completed.validate(8).is_err());

        let completed = aggregate_tool_follow_up_events(&parent, vec![oversized_completed], 8)
            .expect("single completion should be normalized");
        assert_eq!(completed.actor(), Some(sender));
        completed
            .validate(8)
            .expect("normalized completion must validate");
        let WorldEventKind::ToolCompleted(tool) = completed.kind() else {
            panic!("completion kind must be preserved");
        };
        assert!(tool.operation.len() <= super::MAX_TOOL_OPERATION_BYTES);
        assert!(tool.output.len() <= crate::MAX_TOOL_RESULT_BYTES);
        assert!(tool.output.chars().count() <= crate::MAX_TOOL_RESULT_CHARS);

        let oversized_failed = WorldEvent::derived_from(
            &parent,
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ToolFailed(crate::ToolFailedEvent {
                operation: "web.search".to_owned(),
                error_category: "🔥".repeat(128),
                detail: "🌟".repeat(crate::MAX_TOOL_ERROR_DETAIL_CHARS + 1),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("trace should derive even before payload normalization");
        assert!(oversized_failed.validate(8).is_err());

        let failed = aggregate_tool_follow_up_events(&parent, vec![oversized_failed], 8)
            .expect("single failure should be normalized");
        failed
            .validate(8)
            .expect("normalized failure must validate");
        let WorldEventKind::ToolFailed(tool) = failed.kind() else {
            panic!("failure kind must be preserved");
        };
        assert!(tool.error_category.len() <= super::MAX_TOOL_ERROR_CATEGORY_BYTES);
        assert!(tool.detail.len() <= crate::MAX_TOOL_ERROR_DETAIL_BYTES);
        assert!(tool.detail.chars().count() <= crate::MAX_TOOL_ERROR_DETAIL_CHARS);
    }

    #[test]
    fn successful_duplicate_tool_action_does_not_become_a_false_failure() {
        let result = ActionResult::Rejected(ActionRejection::Duplicate {
            action_id: None,
            idempotency_key: "duplicate-tool".to_owned(),
            original_action_id: crate::ActionId::new(),
        });
        assert!(duplicate_action_already_succeeded(
            &result,
            Some(crate::arbiter::AdmittedTerminal::Succeeded)
        ));
        assert!(!duplicate_action_already_succeeded(
            &result,
            Some(crate::arbiter::AdmittedTerminal::Failed)
        ));
    }

    #[tokio::test]
    async fn tool_budget_is_exposed_and_enforced_per_root_trace() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let saw_tool_capability = Arc::new(Mutex::new(None));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(BudgetProbeModel {
                conversation_id,
                saw_tool_capability: Arc::clone(&saw_tool_capability),
            }),
        )
        .expect("valid runtime");
        let event = direct_message(conversation_id, sender);
        runtime
            .reserve_tool_actions(&event, super::MAX_TOOL_ACTIONS_PER_TRACE)
            .expect("preload the root trace budget");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port_calls = Arc::new(AtomicUsize::new(0));
        let port = CountingPort {
            calls: Arc::clone(&port_calls),
        };

        let result = runtime
            .process_event_with_planner_and_actions(event.clone(), &arbiter, &port)
            .await;
        assert!(matches!(
            result,
            Err(PlannerError::InvalidOutput(
                crate::PlannerOutputValidationError::ToolActionBudgetExceeded {
                    used,
                    requested: 1,
                    maximum,
                }
            )) if used == super::MAX_TOOL_ACTIONS_PER_TRACE
                && maximum == super::MAX_TOOL_ACTIONS_PER_TRACE
        ));
        assert_eq!(
            *saw_tool_capability.lock().expect("budget probe lock"),
            Some(false)
        );
        assert_eq!(port_calls.load(Ordering::SeqCst), 0);

        assert_eq!(
            runtime.tool_actions_used(&event),
            super::MAX_TOOL_ACTIONS_PER_TRACE
        );
    }

    #[tokio::test]
    async fn rejected_preflight_releases_materialized_tool_capabilities() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let event = direct_message(conversation_id, sender);
        let expected_key = crate::event_action_idempotency_key(event.id(), 0);
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ToolWithInvalidStateModel {
                conversation_id,
                foreign_conversation_id: ConversationId::new(),
            }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let released_keys = Arc::new(Mutex::new(Vec::new()));
        let port = ReleaseRecordingPort {
            calls: Arc::clone(&calls),
            released_keys: Arc::clone(&released_keys),
        };

        let result = runtime
            .process_event_with_planner_and_actions(event, &arbiter, &port)
            .await;

        assert!(matches!(
            result,
            Err(PlannerError::StateUpdate {
                kind: "set_topic",
                applied_before_failure: 0,
                ..
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            released_keys
                .lock()
                .expect("released key recorder lock")
                .as_slice(),
            &[expected_key]
        );
    }

    #[test]
    fn every_noncompletion_tool_result_becomes_a_failed_batch_record() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let parent = direct_message(conversation_id, sender);
        let action = crate::ProposedAction::use_tool(
            "web.search",
            r#"{"query":"猫眼星云"}"#,
            crate::ActionScope::Conversation(conversation_id),
        )
        .expect("valid tool action")
        .with_actor(sender);
        let receipt = ActionReceipt {
            action_id: action.action_id(),
            idempotency_key: action.idempotency_key().map(ToOwned::to_owned),
            admitted_at: Utc::now(),
        };
        let results = [
            ActionResult::Executed {
                receipt: receipt.clone(),
                outcome: ActionPortOutcome::Deferred {
                    reason: "tool_turn_capability_missing".to_owned(),
                },
            },
            ActionResult::Failed {
                receipt,
                error: crate::ActionPortError::new("upstream_unavailable", true),
            },
            ActionResult::Rejected(ActionRejection::CapabilityUnavailable {
                action_id: action.action_id(),
                capability: crate::ActionCapability::UseTool,
            }),
            ActionResult::Noop,
        ];
        let failure_events = results
            .iter()
            .map(|result| {
                let event = tool_follow_up_event(&parent, &action, result, 8)
                    .expect("tool result should become a follow-up event");
                event.validate(8).expect("failure event must be valid");
                assert_eq!(event.actor(), Some(sender));
                let WorldEventKind::ToolFailed(tool) = event.kind() else {
                    panic!("noncompletion tool result must become ToolFailed");
                };
                assert_eq!(tool.operation, "web.search");
                assert!(tool.requires_follow_up);
                event
            })
            .collect::<Vec<_>>();

        let batch = aggregate_tool_follow_up_events(&parent, failure_events, 8)
            .expect("failed tool outcomes should aggregate");
        let WorldEventKind::ToolCompleted(tool) = batch.kind() else {
            panic!("a failed tool batch must use a completed envelope");
        };
        let payload: serde_json::Value =
            serde_json::from_str(&tool.output).expect("batch output must be JSON");
        let results = payload
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("batch results");
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|result| result["status"] == "failed"));
        assert!(
            results
                .iter()
                .all(|result| result["operation"] == "web.search")
        );
    }

    #[test]
    fn full_budget_ledger_never_resets_an_existing_trace() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let mut runtime = CognitiveRuntime::new(RuntimeConfig::default())
            .expect("valid runtime")
            .1;
        let retained_root = direct_message(conversation_id, sender);
        runtime
            .reserve_tool_actions(&retained_root, 1)
            .expect("root budget should reserve");
        let pending = WorldEvent::derived_from(
            &retained_root,
            Utc::now(),
            retained_root.scope(),
            EventPriority::High,
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: "web.search".to_owned(),
                output: "结果".to_owned(),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("pending event should be valid");
        runtime.pending_tool_follow_ups.push_back(pending);

        for _ in 0..super::MAX_TOOL_TRACE_BUDGET_ENTRIES.saturating_sub(1) {
            let root = direct_message(conversation_id, sender);
            runtime
                .reserve_tool_actions(&root, 1)
                .expect("fresh root budget should reserve");
        }
        assert_eq!(
            runtime.tool_action_budget_by_trace.len(),
            super::MAX_TOOL_TRACE_BUDGET_ENTRIES
        );
        for _ in 0..8 {
            let new_root = direct_message(conversation_id, sender);
            assert_eq!(runtime.tool_actions_remaining(&new_root), 0);
            assert!(matches!(
                runtime.reserve_tool_actions(&new_root, 1),
                Err(PlannerError::InvalidOutput(
                    crate::PlannerOutputValidationError::ToolActionBudgetExceeded {
                        used,
                        requested: 1,
                        maximum,
                    }
                )) if used == super::MAX_TOOL_ACTIONS_PER_TRACE
                    && maximum == super::MAX_TOOL_ACTIONS_PER_TRACE
            ));
        }

        // Overflow attempts neither evict nor reset the retained root. It can
        // still consume exactly the remainder of its original allowance.
        assert_eq!(runtime.tool_actions_used(&retained_root), 1);
        runtime
            .reserve_tool_actions(
                &retained_root,
                super::MAX_TOOL_ACTIONS_PER_TRACE.saturating_sub(1),
            )
            .expect("tracked root should retain its remaining budget");
        assert_eq!(
            runtime.tool_actions_used(&retained_root),
            super::MAX_TOOL_ACTIONS_PER_TRACE
        );
        assert!(
            runtime
                .tool_action_budget_by_trace
                .contains_key(&retained_root.trace().root_event_id())
        );
        assert_eq!(
            runtime.tool_action_budget_by_trace.len(),
            super::MAX_TOOL_TRACE_BUDGET_ENTRIES
        );
        assert_eq!(
            runtime.tool_action_budget_by_trace.len(),
            runtime.tool_action_budget_order.len()
        );
    }

    #[test]
    fn completed_roots_release_budget_capacity_without_resetting_late_children() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let mut runtime = CognitiveRuntime::new(RuntimeConfig::default())
            .expect("valid runtime")
            .1;

        // Sequentially completed traces can reuse the bounded ledger instead
        // of permanently exhausting it after 1024 independent requests.
        for _ in 0..(super::MAX_TOOL_TRACE_BUDGET_ENTRIES + 32) {
            let root = direct_message(conversation_id, sender);
            runtime
                .reserve_tool_actions(&root, 1)
                .expect("fresh root should reserve");
            runtime.release_tool_budget_root_if_terminal(root.trace().root_event_id());
            assert!(runtime.tool_action_budget_by_trace.is_empty());
            assert!(runtime.tool_action_budget_order.is_empty());
        }

        // A queued descendant pins the root until it is consumed.
        let active_root = direct_message(conversation_id, sender);
        runtime
            .reserve_tool_actions(&active_root, 1)
            .expect("active root should reserve");
        let late_child = WorldEvent::derived_from(
            &active_root,
            Utc::now(),
            active_root.scope(),
            EventPriority::High,
            WorldEventKind::ToolCompleted(crate::ToolCompletedEvent {
                operation: "calculator".to_owned(),
                output: "2".to_owned(),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("child event should be valid");
        runtime
            .pending_tool_follow_ups
            .push_back(late_child.clone());
        runtime.release_tool_budget_root_if_terminal(active_root.trace().root_event_id());
        assert!(
            runtime
                .tool_action_budget_by_trace
                .contains_key(&active_root.trace().root_event_id())
        );

        runtime.pending_tool_follow_ups.pop_front();
        runtime.release_tool_budget_root_if_terminal(active_root.trace().root_event_id());
        assert!(
            !runtime
                .tool_action_budget_by_trace
                .contains_key(&active_root.trace().root_event_id())
        );

        // Neither a root replay nor a delayed child can recreate the released
        // cumulative budget.
        assert_eq!(runtime.tool_actions_remaining(&active_root), 0);
        assert!(matches!(
            runtime.reserve_tool_actions(&active_root, 1),
            Err(PlannerError::InvalidOutput(
                crate::PlannerOutputValidationError::ToolActionBudgetExceeded { .. }
            ))
        ));
        assert_eq!(runtime.tool_actions_remaining(&late_child), 0);
        assert!(matches!(
            runtime.reserve_tool_actions(&late_child, 1),
            Err(PlannerError::InvalidOutput(
                crate::PlannerOutputValidationError::ToolActionBudgetExceeded { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn tool_follow_up_can_invoke_another_tool_with_actor_context() {
        let conversation_id = ConversationId::new();
        let sender = PersonId::new();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(ToolFollowUpModel {
                conversation_id,
                calls: Arc::clone(&model_calls),
                recurse_on_result: true,
            }),
        )
        .expect("valid runtime");
        let arbiter = ActionArbiter::new(
            ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
        );
        let port_calls = Arc::new(AtomicUsize::new(0));
        let port = ToolThenDeliveryPort {
            calls: Arc::clone(&port_calls),
        };

        runtime
            .process_event_with_planner_and_actions(
                direct_message(conversation_id, sender),
                &arbiter,
                &port,
            )
            .await
            .expect("first tool request should execute");
        assert!(matches!(
            runtime
                .pending_tool_follow_ups
                .front()
                .map(WorldEvent::kind),
            Some(WorldEventKind::ToolCompleted(_))
        ));
        assert_eq!(
            runtime
                .pending_tool_follow_ups
                .front()
                .and_then(WorldEvent::actor),
            Some(sender)
        );
        let follow_up = runtime
            .process_next_with_planner_and_actions(&arbiter, &port)
            .await
            .expect("tool result should be queued");
        assert!(matches!(
            follow_up,
            Ok(PlannedProcessingOutcome::Planned {
                actions,
                ..
            }) if matches!(
                actions.as_slice(),
                [ActionResult::Executed {
                    outcome: ActionPortOutcome::ToolCompleted { .. },
                    ..
                }]
            )
        ));
        let final_follow_up = runtime
            .process_next_with_planner_and_actions(&arbiter, &port)
            .await
            .expect("second tool result should be queued")
            .expect("final reply planning should succeed");
        assert!(matches!(
            final_follow_up,
            PlannedProcessingOutcome::Planned {
                actions,
                ..
            } if matches!(
                actions.as_slice(),
                [ActionResult::Executed {
                    outcome: ActionPortOutcome::Delivered { .. },
                    ..
                }]
            )
        ));
        assert!(runtime.pending_tool_follow_ups.is_empty());
        assert_eq!(model_calls.load(Ordering::SeqCst), 3);
        assert_eq!(port_calls.load(Ordering::SeqCst), 3);
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
    async fn indeterminate_due_delivery_is_unscheduled_without_claiming_success() {
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
        let port = IndeterminateActionPort {
            calls: calls.clone(),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("indeterminate delivery should remain a structured terminal outcome");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned {
                ref actions,
                ref feedback,
                ..
            } if matches!(
                actions.as_slice(),
                [ActionResult::Executed {
                    outcome: ActionPortOutcome::DeliveryIndeterminate { .. },
                    ..
                }]
            ) && feedback.iter().all(|item| {
                item.event_type != crate::EventType::ActionSucceeded
                    && item.event_type != crate::EventType::MessageSent
            }) && feedback
                .iter()
                .any(|item| item.event_type == crate::EventType::ActionFailed)
        ));
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );
        assert_eq!(
            open_loops
                .deferred
                .lock()
                .expect("open-loop defer recorder lock")
                .as_slice(),
            &[(open_loop_id, None)]
        );

        let replay = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("an indeterminate duplicate should retry only the unschedule transition");
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
                .deferred
                .lock()
                .expect("open-loop defer recorder lock")
                .as_slice(),
            &[(open_loop_id, None), (open_loop_id, None)]
        );
    }

    #[tokio::test]
    async fn indeterminate_due_delivery_is_unscheduled_without_a_resolve_proposal() {
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
        let calls = Arc::new(AtomicUsize::new(0));

        let outcome = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &IndeterminateActionPort {
                    calls: calls.clone(),
                },
            )
            .await
            .expect("terminal delivery must close the scheduler lease independently of the plan");
        assert!(matches!(
            outcome,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [ActionResult::Executed {
                        outcome: ActionPortOutcome::DeliveryIndeterminate { .. },
                        ..
                    }]
                )
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            open_loops
                .deferred
                .lock()
                .expect("open-loop defer recorder lock")
                .as_slice(),
            &[(open_loop_id, None)]
        );
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn non_retryable_due_port_failure_is_unscheduled_and_not_dispatched_again() {
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
        let port = TerminalFailingActionPort {
            calls: calls.clone(),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("non-retryable adapter failure should remain structured");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [ActionResult::Failed {
                        error: crate::ActionPortError {
                            retryable: false,
                            ..
                        },
                        ..
                    }]
                )
        ));

        let replay = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("terminal duplicate should only repeat the unschedule transition");
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
                .deferred
                .lock()
                .expect("open-loop defer recorder lock")
                .as_slice(),
            &[(open_loop_id, None), (open_loop_id, None)]
        );
    }

    #[tokio::test]
    async fn due_reach_out_route_rejections_are_terminal_and_unscheduled() {
        for resolution_failed in [false, true] {
            let person_id = PersonId::new();
            let open_loop_id = OpenLoopId::new();
            let open_loops = Arc::new(TestOpenLoopStore::with_visible(
                open_loop_id,
                OpenLoopOwner::Person(person_id),
            ));
            let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
                RuntimeConfig::default(),
                CoreServices::with_model(DueReachOutModel {
                    person_id,
                    open_loop_id,
                })
                .with_open_loops(open_loops.clone()),
            )
            .expect("valid runtime");
            let arbiter = ActionArbiter::new(
                ActionArbiterConfig::default().with_capabilities(EnvironmentCapabilities::all()),
            )
            .with_delivery_resolver(Arc::new(RejectingDeliveryResolver { resolution_failed }));
            let calls = Arc::new(AtomicUsize::new(0));
            let event = WorldEvent::new(
                Utc::now(),
                EventScope::Person { person_id },
                EventPriority::High,
                WorldEventKind::ProspectiveMemoryDue(crate::ProspectiveMemoryEvent {
                    open_loop_id,
                }),
            );

            let outcome = runtime
                .process_event_with_planner_and_actions(
                    event,
                    &arbiter,
                    &CountingDeliveredActionPort {
                        calls: calls.clone(),
                    },
                )
                .await
                .expect("route rejection should remain a structured terminal outcome");

            assert!(matches!(
                outcome,
                PlannedProcessingOutcome::Planned { actions, .. }
                    if if resolution_failed {
                        matches!(
                            actions.as_slice(),
                            [ActionResult::Rejected(
                                ActionRejection::DeliveryResolutionFailed { .. }
                            )]
                        )
                    } else {
                        matches!(
                            actions.as_slice(),
                            [ActionResult::Rejected(ActionRejection::TargetUnavailable { .. })]
                        )
                    }
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                open_loops
                    .deferred
                    .lock()
                    .expect("open-loop defer recorder lock")
                    .as_slice(),
                &[(open_loop_id, None)]
            );
            assert!(
                open_loops
                    .resolved
                    .lock()
                    .expect("open-loop recorder lock")
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn due_tool_failure_is_unscheduled_and_not_dispatched_again() {
        let conversation_id = ConversationId::new();
        let open_loop_id = OpenLoopId::new();
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(
            open_loop_id,
            OpenLoopOwner::Conversation(conversation_id),
        ));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(DueToolActionModel {
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
        let port = ToolFailedActionPort {
            calls: calls.clone(),
        };

        let first = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("tool failure should remain a structured terminal outcome");
        assert!(matches!(
            first,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [ActionResult::Executed {
                        outcome: ActionPortOutcome::ToolFailed { .. },
                        ..
                    }]
                )
        ));

        let replay = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &port,
            )
            .await
            .expect("terminal tool duplicate should only repeat the unschedule transition");
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
                .deferred
                .lock()
                .expect("open-loop defer recorder lock")
                .as_slice(),
            &[(open_loop_id, None), (open_loop_id, None)]
        );
    }

    #[tokio::test]
    async fn one_indeterminate_action_unschedules_a_multi_action_due_loop() {
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

        let outcome = runtime
            .process_event_with_planner_and_actions(
                due_event(conversation_id, open_loop_id),
                &arbiter,
                &DeliverThenIndeterminatePort {
                    calls: calls.clone(),
                },
            )
            .await
            .expect("mixed terminal delivery outcomes should be represented");
        assert!(matches!(
            outcome,
            PlannedProcessingOutcome::Planned { actions, .. }
                if matches!(
                    actions.as_slice(),
                    [
                        ActionResult::Executed {
                            outcome: ActionPortOutcome::Delivered { .. },
                            ..
                        },
                        ActionResult::Executed {
                            outcome: ActionPortOutcome::DeliveryIndeterminate { .. },
                            ..
                        }
                    ]
                )
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );
        assert_eq!(
            open_loops
                .deferred
                .lock()
                .expect("open-loop defer recorder lock")
                .as_slice(),
            &[(open_loop_id, None)]
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

    #[test]
    fn ordinary_action_key_matches_the_public_planner_helper() {
        let conversation_id = ConversationId::new();
        let event_id = EventId::new();
        let mut action =
            crate::ProposedAction::send_message(conversation_id, MessageContent::text("reply"))
                .expect("valid action");

        apply_event_action_idempotency(&mut action, event_id, 0).expect("valid metadata");

        assert_eq!(
            action.idempotency_key(),
            Some(crate::event_action_idempotency_key(event_id, 0).as_str())
        );
    }

    #[tokio::test]
    async fn direct_messages_load_both_conversation_and_person_context() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let memory = Arc::new(TestMemoryStore::default());
        let open_loops = Arc::new(TestOpenLoopStore::default());
        let conversation_goal = TestGoalStore::goal(
            GoalId::new(),
            GoalOwner::Conversation(conversation_id),
            "conversation goal",
        );
        let person_goal =
            TestGoalStore::goal(GoalId::new(), GoalOwner::Person(person_id), "person goal");
        let goals = Arc::new(TestGoalStore::with_goals(vec![
            conversation_goal.clone(),
            person_goal.clone(),
        ]));
        let (_handle, runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel)
                .with_memory(memory.clone())
                .with_open_loops(open_loops.clone())
                .with_goals(goals.clone()),
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
        assert_eq!(input.goals, vec![conversation_goal, person_goal]);
        assert_eq!(
            goals
                .listed_owners
                .lock()
                .expect("goal list recorder lock")
                .as_slice(),
            &[
                (
                    GoalOwner::Conversation(conversation_id),
                    MAX_GOALS_PER_CONTEXT_OWNER,
                ),
                (GoalOwner::Person(person_id), MAX_GOALS_PER_CONTEXT_OWNER),
            ]
        );
    }

    #[tokio::test]
    async fn goal_events_load_the_exact_goal_and_its_owner_context() {
        let person_id = PersonId::new();
        let owner = GoalOwner::Person(person_id);
        let goal_id = GoalId::new();
        let goal = TestGoalStore::goal(goal_id, owner, "changed goal");
        let sibling = TestGoalStore::goal(GoalId::new(), owner, "related goal");
        let goals = Arc::new(TestGoalStore::with_goals(vec![
            goal.clone(),
            sibling.clone(),
        ]));
        let (_handle, runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel).with_goals(goals.clone()),
        )
        .expect("valid runtime");

        let input = runtime
            .planner_input_with_context(WorldEvent::new(
                Utc::now(),
                EventScope::Goal { goal_id },
                EventPriority::High,
                WorldEventKind::GoalUpdated(GoalUpdatedEvent { goal_id }),
            ))
            .await;

        assert_eq!(
            goals
                .fetched_ids
                .lock()
                .expect("goal fetch recorder lock")
                .as_slice(),
            &[goal_id]
        );
        assert_eq!(
            goals
                .listed_owners
                .lock()
                .expect("goal list recorder lock")
                .as_slice(),
            &[(owner, MAX_GOALS_PER_CONTEXT_OWNER)]
        );
        assert_eq!(input.goals, vec![goal, sibling]);
    }

    #[tokio::test]
    async fn goal_hydration_enforces_the_per_owner_bound_locally() {
        let visible = (0..MAX_GOALS_PER_CONTEXT_OWNER + 5)
            .map(|index| {
                TestGoalStore::goal(
                    GoalId::new(),
                    GoalOwner::Global,
                    format!("global goal {index}"),
                )
            })
            .collect();
        let goals = Arc::new(TestGoalStore::with_goals(visible));
        let (_handle, runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel).with_goals(goals.clone()),
        )
        .expect("valid runtime");

        let input = runtime
            .planner_input_with_context(event(EventPriority::High))
            .await;

        assert_eq!(input.goals.len(), MAX_GOALS_PER_CONTEXT_OWNER);
        assert_eq!(
            goals
                .listed_owners
                .lock()
                .expect("goal list recorder lock")
                .as_slice(),
            &[(GoalOwner::Global, MAX_GOALS_PER_CONTEXT_OWNER)]
        );
        input.validate(8).expect("hydrated input remains bounded");
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
                output: String::new(),
                requires_follow_up: false,
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
    async fn interaction_cue_events_hydrate_and_persist_their_person_state() {
        let person_id = PersonId::new();
        let cues = InteractionCues {
            sentiment_valence: 0.65,
            sentiment_arousal: 0.35,
            sentiment_confidence: 0.9,
            gratitude_strength: 0.8,
        };
        let observed = InteractionCuesObservedEvent::new(person_id, cues).expect("bounded cues");
        let expected =
            crate::apply_interaction_cues(person_id, None, AffectState::default(), observed.cues())
                .expect("fixed-point cues stay bounded");
        let affects = Arc::new(TestAffectStore::default());
        let relations = Arc::new(TestRelationStore::default());
        let services = CoreServices::with_model(InteractionCueModel)
            .with_affect(affects.clone())
            .with_relations(relations.clone());
        let (_handle, mut runtime) =
            CognitiveRuntime::new_with_services(RuntimeConfig::default(), services)
                .expect("valid runtime");
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Person { person_id },
            EventPriority::Normal,
            WorldEventKind::InteractionCuesObserved(observed),
        );

        let outcome = runtime
            .process_event_with_planner(event)
            .await
            .expect("cue state should persist");
        assert!(matches!(
            outcome,
            PlannedProcessingOutcome::Planned { plan, .. }
                if plan.intents.is_empty() && plan.state_updates.len() == 2
        ));
        assert_eq!(
            affects
                .reads
                .lock()
                .expect("affect recorder lock")
                .as_slice(),
            &[person_id]
        );
        assert_eq!(
            relations
                .reads
                .lock()
                .expect("relation recorder lock")
                .as_slice(),
            &[person_id]
        );
        assert_eq!(
            affects
                .updates
                .lock()
                .expect("affect recorder lock")
                .as_slice(),
            &[(person_id, expected.affect)]
        );
        assert_eq!(
            relations
                .updates
                .lock()
                .expect("relation recorder lock")
                .as_slice(),
            &[expected.relation]
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
    async fn defer_open_loop_update_reopens_the_visible_item_without_resolving_it() {
        let conversation_id = ConversationId::new();
        let person_id = PersonId::new();
        let open_loop_id = OpenLoopId::new();
        let owner = OpenLoopOwner::Conversation(conversation_id);
        let open_loops = Arc::new(TestOpenLoopStore::with_visible(open_loop_id, owner));
        let (_handle, mut runtime) = CognitiveRuntime::new_with_services(
            RuntimeConfig::default(),
            CoreServices::with_model(FakeModel).with_open_loops(open_loops.clone()),
        )
        .expect("valid runtime");
        let input = runtime
            .planner_input(direct_message(conversation_id, person_id))
            .with_open_loops(vec![TestOpenLoopStore::open_loop(open_loop_id, owner)]);
        let plan = DecisionPlan::silent().with_state_update(StateUpdateProposal::DeferOpenLoop {
            open_loop_id,
            due_at: None,
        });

        assert_eq!(
            runtime
                .apply_state_updates(&input, &plan, None)
                .await
                .expect("visible open loop should be deferred"),
            1
        );
        assert_eq!(
            open_loops
                .deferred
                .lock()
                .expect("open-loop recorder lock")
                .as_slice(),
            &[(open_loop_id, None)]
        );
        assert!(
            open_loops
                .resolved
                .lock()
                .expect("open-loop recorder lock")
                .is_empty()
        );
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
        assert_eq!(
            sent.content.as_ref().map(MessageContent::as_text),
            Some("delivered")
        );
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
        assert_eq!(
            sent.content.as_ref().map(MessageContent::as_text),
            Some("checking in")
        );
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
    async fn conversation_data_erasure_purges_state_and_discards_late_feedback_in_fifo() {
        let conversation_id = ConversationId::new();
        let unrelated_conversation = ConversationId::new();
        let person_id = PersonId::new();
        let prior = group_message(conversation_id, person_id);
        let prior_id = prior.id();
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

        handle.submit(prior).await.expect("enqueue prior event");
        assert!(
            handle
                .begin_conversation_data_erasure(conversation_id)
                .await
                .expect("begin conversation barrier")
        );
        assert_eq!(observed_receiver.recv().await, Some(prior_id));

        let sent_at = Utc::now();
        handle
            .submit(WorldEvent::new(
                sent_at,
                EventScope::Conversation { conversation_id },
                EventPriority::High,
                WorldEventKind::ActionSucceeded(crate::ActionSucceededEvent {
                    idempotency_key: "late-action".to_string(),
                }),
            ))
            .await
            .expect("enqueue late action receipt");
        let message_sent_at = Utc::now();
        handle
            .submit(WorldEvent::new(
                message_sent_at,
                EventScope::Conversation { conversation_id },
                EventPriority::High,
                WorldEventKind::MessageSent(crate::MessageSentEvent {
                    message_id: MessageId::new(),
                    conversation_id,
                    timestamp: message_sent_at,
                    content: None,
                }),
            ))
            .await
            .expect("enqueue late message receipt");
        let unrelated = group_message(unrelated_conversation, person_id);
        let unrelated_id = unrelated.id();
        handle
            .submit(unrelated)
            .await
            .expect("enqueue unrelated event");
        assert!(
            handle
                .end_conversation_data_erasure(conversation_id)
                .await
                .expect("end conversation barrier")
        );
        assert_eq!(observed_receiver.recv().await, Some(unrelated_id));
        assert!(observed_receiver.try_recv().is_err());

        drop(handle);
        let runtime = driver.await.expect("runtime driver should join");
        assert!(runtime.state().conversation(conversation_id).is_none());
        assert!(
            runtime
                .state()
                .conversation(unrelated_conversation)
                .is_some()
        );
    }

    #[tokio::test]
    async fn conversation_data_erasure_batch_covers_every_stale_canonical_id_atomically() {
        let first_conversation = ConversationId::new();
        let stale_remap = ConversationId::new();
        let person_id = PersonId::new();
        let (handle, mut runtime) =
            CognitiveRuntime::new(RuntimeConfig::default()).expect("valid runtime");
        let driver = tokio::spawn(async move {
            while runtime.process_next().await.is_some() {}
            runtime
        });

        handle
            .submit(group_message(first_conversation, person_id))
            .await
            .expect("enqueue first conversation");
        handle
            .submit(group_message(stale_remap, person_id))
            .await
            .expect("enqueue stale remap");
        assert_eq!(
            handle
                .begin_conversation_data_erasures([first_conversation, stale_remap])
                .await
                .expect("begin conversation barriers"),
            2
        );

        handle
            .submit(group_message(first_conversation, person_id))
            .await
            .expect("enqueue blocked first conversation");
        handle
            .submit(group_message(stale_remap, person_id))
            .await
            .expect("enqueue blocked stale remap");
        assert_eq!(
            handle
                .end_conversation_data_erasures([first_conversation, stale_remap])
                .await
                .expect("end conversation barriers"),
            2
        );

        drop(handle);
        let runtime = driver.await.expect("runtime driver should join");
        assert!(runtime.state().conversation(first_conversation).is_none());
        assert!(runtime.state().conversation(stale_remap).is_none());
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
        assert_eq!(
            handle
                .begin_conversation_data_erasure(ConversationId::new())
                .await,
            Err(DataErasureError::RuntimeClosed)
        );
        assert_eq!(
            handle
                .end_conversation_data_erasure(ConversationId::new())
                .await,
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
                    visible_reply_allowed: true,
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

    #[tokio::test]
    async fn optional_mind_provider_hydrates_replayable_planner_input() {
        let timestamp = Utc::now();
        let self_model = SelfModel::seed_yunxi(timestamp);
        let snapshot = MindSnapshot::new(
            Some(SelfModelSnapshot::from_model(&self_model).expect("self snapshot")),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            MindInfluenceMode::Shadow,
            7,
            timestamp,
        )
        .expect("valid mind snapshot");
        let (_, runtime) = CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
        let runtime =
            runtime.with_mind_snapshot_provider(Arc::new(StaticMindProvider { snapshot }));
        let input = runtime
            .planner_input_with_context(direct_message(ConversationId::new(), PersonId::new()))
            .await;

        assert_eq!(input.mind.version(), 7);
        assert_eq!(input.mind.influence_mode(), MindInfluenceMode::Shadow);
        assert_eq!(
            input
                .mind
                .self_model()
                .expect("self model")
                .identity()
                .name(),
            "芸汐"
        );
    }

    #[tokio::test]
    async fn mind_timeout_fails_soft_to_v1_empty_snapshot() {
        let (_, mut runtime) = CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
        runtime.install_mind_snapshot_provider(Arc::new(SlowMindProvider));
        runtime.set_mind_snapshot_timeout(std::time::Duration::from_millis(1));
        let started = std::time::Instant::now();
        let input = runtime
            .planner_input_with_context(direct_message(ConversationId::new(), PersonId::new()))
            .await;

        assert!(input.mind.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(25));
    }

    #[tokio::test]
    async fn runtime_without_mind_provider_preserves_v1_empty_snapshot() {
        let (_, runtime) = CognitiveRuntime::new(RuntimeConfig::default()).expect("runtime");
        let input = runtime
            .planner_input_with_context(direct_message(ConversationId::new(), PersonId::new()))
            .await;

        assert!(input.mind.is_empty());
        assert_eq!(input.mind.influence_mode(), MindInfluenceMode::Disabled);
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
