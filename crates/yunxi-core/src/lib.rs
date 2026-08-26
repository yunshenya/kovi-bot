//! Platform-neutral domain primitives and the observe-only Yunxi runtime.
//!
//! This crate describes what Yunxi observes and keeps in working state. Hosts
//! translate environment-specific events into these types and own all concrete
//! side effects and infrastructure.

pub mod action;
pub mod arbiter;
pub mod attention;
pub mod delivery;
pub mod event;
pub mod goal;
pub mod identity;
pub mod intent;
pub mod memory;
pub mod mind;
pub mod open_loop;
pub mod planner;
pub mod ports;
pub mod proactive;
pub mod runtime;
pub mod working_state;

pub use action::{
    ActionId, ActionMetadata, ActionScope, ActionValidationError, CancelGoalAction,
    CreateOpenLoopAction, MAX_ACTION_IDEMPOTENCY_KEY_BYTES, MAX_ACTION_IDEMPOTENCY_KEY_CHARS,
    MAX_TOOL_INPUT_BYTES, MAX_TOOL_INPUT_CHARS, MAX_TOOL_NAME_BYTES, MAX_TOOL_NAME_CHARS,
    ProposedAction, ReachOutAction, ResolveOpenLoopAction, SendMessageAction, StartGoalAction,
    ToolAction, event_action_idempotency_key,
};
pub use arbiter::{
    ActionArbiter, ActionArbiterConfig, ActionCapability, ActionDescriptor, ActionPort,
    ActionPortError, ActionPortFuture, ActionPortOutcome, ActionReceipt, ActionRejection,
    ActionResult, AuthorizationPolicy, EnvironmentCapabilities, MAX_RATE_LIMIT_WINDOW_ENTRIES,
    MAX_TRACKED_ACTION_KEYS, MAX_TRACKED_ACTION_SCOPES, RateLimit, StaleReason,
};
pub use attention::{AttentionDisposition, AttentionReason, AttentionResult, AttentionSystem};
pub use delivery::{
    DeliveryResolutionError, DeliveryResolver, DeliveryResolverFuture, DeliveryRoute,
};
pub use event::{
    ActionFailedEvent, ActionRejectedEvent, ActionSucceededEvent, Attachment, AttachmentKind,
    EventPriority, EventScope, EventType, EventValidationError, GoalCompletedEvent,
    GoalUpdatedEvent, InteractionCuesObservedEvent, MAX_TOOL_ERROR_DETAIL_BYTES,
    MAX_TOOL_ERROR_DETAIL_CHARS, MAX_TOOL_RESULT_BYTES, MAX_TOOL_RESULT_CHARS, Message,
    MessageCollisionDetectedEvent, MessageContent, MessageReceivedEvent, MessageSentEvent,
    MessageValidationError, ProspectiveMemoryEvent, ReminderDueEvent, ToolCompletedEvent,
    ToolFailedEvent, TraceContext, TraceError, WorldEvent, WorldEventKind,
};
pub use goal::{
    Goal, GoalDraft, GoalKind, GoalOwner, GoalState, GoalValidationError, MAX_GOAL_DETAILS_BYTES,
    MAX_GOAL_DETAILS_CHARS, MAX_GOAL_TITLE_BYTES, MAX_GOAL_TITLE_CHARS,
};
pub use identity::{
    ConversationId, ConversationKind, ConversationMember, ConversationMemberValidationError,
    EventId, ExternalConversation, ExternalIdentity, ExternalReferenceError, GoalId,
    MAX_CONVERSATION_MEMBER_ROLE_BYTES, MAX_CONVERSATION_MEMBER_ROLE_CHARS, MAX_EXTERNAL_ID_BYTES,
    MAX_PLATFORM_ID_BYTES, MemoryId, MessageId, OpenLoopId, PersonId, PlatformId,
};
pub use intent::{CognitiveIntent, IntentValidationError};
pub use memory::{
    MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_CONTENT_CHARS, MAX_MEMORY_QUERY_BYTES,
    MAX_MEMORY_TAG_BYTES, MAX_MEMORY_TAGS, Memory, MemoryDraft, MemoryKind, MemoryQuery,
    MemoryScope, MemoryValidationError,
};
pub use mind::*;
pub use open_loop::{
    MAX_OPEN_LOOP_DEDUPE_KEY_BYTES, MAX_OPEN_LOOP_SALIENCE, MAX_OPEN_LOOP_SUMMARY_BYTES,
    MAX_OPEN_LOOP_SUMMARY_CHARS, OpenLoop, OpenLoopDraft, OpenLoopKind, OpenLoopOwner,
    OpenLoopStatus, OpenLoopValidationError,
};
pub use planner::{
    AffectState, DecisionDisposition, DecisionPlan, InteractionCueValidationError, InteractionCues,
    InteractionStateEvolution, MAX_PLANNER_GOALS, MAX_PLANNER_INTENTS, MAX_PLANNER_MEMORIES,
    MAX_PLANNER_OPEN_LOOPS, MAX_PLANNER_STATE_UPDATES, MAX_PLANNER_TOPIC_BYTES,
    MAX_PLANNER_TOPIC_CHARS, ModelBackend, ModelBackendError, ModelBackendFuture, Planner,
    PlannerError, PlannerInput, PlannerInputValidationError, PlannerOutput,
    PlannerOutputValidationError, PlannerStateSnapshot, RelationState, StateUpdateProposal,
    apply_interaction_cues, drift_affect_state, drift_relation_state, evolve_interaction_state,
    evolve_interaction_state_with_cues,
};
pub use ports::{
    AffectStore, AffectStoreError, AffectStoreFuture, Clock, ConversationMemberStore,
    ConversationMemberStoreError, ConversationMemberStoreFuture, CoreServices, GoalStore,
    GoalStoreError, GoalStoreFuture, IdentityStore, IdentityStoreError, IdentityStoreFuture,
    MemoryStore, MemoryStoreError, MemoryStoreFuture, OpenLoopStore, OpenLoopStoreError,
    OpenLoopStoreFuture, RelationStore, RelationStoreError, RelationStoreFuture, SystemClock,
};
pub use proactive::{
    MAX_PROACTIVE_CANDIDATES, MAX_REACH_OUT_MESSAGE_BYTES, MAX_REACH_OUT_MESSAGE_CHARS,
    ProactiveCandidate, ProactiveContext, ProactiveDecision, ProactiveMotive, ProactiveOpportunity,
    ProactiveSilenceReason, ProactiveSystem, ProactiveValidationError, ProspectiveSignal,
    ReachOutIntent,
};
pub use runtime::{
    Admission, CognitiveRuntime, DataErasureError, MAX_BLOCKED_DATA_ERASURE_CONVERSATIONS,
    MAX_BLOCKED_DATA_ERASURE_PEOPLE, MAX_DATA_ERASURE_CONVERSATIONS, PlannedProcessingOutcome,
    ProcessingOutcome, RuntimeConfig, RuntimeConfigError, RuntimeHandle, RuntimeObservation,
    SubmitError, planned_action_idempotency_key,
};
pub use working_state::{
    CompactEvent, ConversationSnapshot, StateUpdate, WorkingState, WorkingStateConfig,
    WorkingStateConfigError, WorkingStateError,
};
