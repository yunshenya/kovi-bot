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
pub mod identity;
pub mod intent;
pub mod memory;
pub mod open_loop;
pub mod ports;
pub mod proactive;
pub mod runtime;
pub mod working_state;

pub use action::{
    ActionId, ActionMetadata, ActionScope, ActionValidationError, MAX_ACTION_IDEMPOTENCY_KEY_BYTES,
    MAX_ACTION_IDEMPOTENCY_KEY_CHARS, ProposedAction, ReachOutAction, SendMessageAction,
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
    ActionFailedEvent, ActionRejectedEvent, ActionSucceededEvent, EventPriority, EventScope,
    EventType, EventValidationError, GoalCompletedEvent, GoalUpdatedEvent, MessageContent,
    MessageReceivedEvent, MessageSentEvent, ProspectiveMemoryEvent, ReminderDueEvent,
    ToolCompletedEvent, ToolFailedEvent, TraceContext, TraceError, WorldEvent, WorldEventKind,
};
pub use identity::{
    ConversationId, ConversationKind, EventId, ExternalConversation, ExternalIdentity,
    ExternalReferenceError, GoalId, MAX_EXTERNAL_ID_BYTES, MAX_PLATFORM_ID_BYTES, MemoryId,
    MessageId, OpenLoopId, PersonId, PlatformId,
};
pub use intent::{CognitiveIntent, IntentValidationError};
pub use memory::{
    MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_CONTENT_CHARS, MAX_MEMORY_QUERY_BYTES,
    MAX_MEMORY_TAG_BYTES, MAX_MEMORY_TAGS, Memory, MemoryDraft, MemoryKind, MemoryQuery,
    MemoryScope, MemoryValidationError,
};
pub use open_loop::{
    MAX_OPEN_LOOP_DEDUPE_KEY_BYTES, MAX_OPEN_LOOP_SALIENCE, MAX_OPEN_LOOP_SUMMARY_BYTES,
    MAX_OPEN_LOOP_SUMMARY_CHARS, OpenLoop, OpenLoopDraft, OpenLoopKind, OpenLoopOwner,
    OpenLoopStatus, OpenLoopValidationError,
};
pub use ports::{
    Clock, IdentityStore, IdentityStoreError, IdentityStoreFuture, MemoryStore, MemoryStoreError,
    MemoryStoreFuture, OpenLoopStore, OpenLoopStoreError, OpenLoopStoreFuture, SystemClock,
};
pub use proactive::{
    MAX_PROACTIVE_CANDIDATES, MAX_REACH_OUT_MESSAGE_BYTES, MAX_REACH_OUT_MESSAGE_CHARS,
    ProactiveCandidate, ProactiveContext, ProactiveDecision, ProactiveMotive, ProactiveOpportunity,
    ProactiveSilenceReason, ProactiveSystem, ProactiveValidationError, ProspectiveSignal,
    ReachOutIntent,
};
pub use runtime::{
    Admission, CognitiveRuntime, ProcessingOutcome, RuntimeConfig, RuntimeConfigError,
    RuntimeHandle, RuntimeObservation, SubmitError,
};
pub use working_state::{
    CompactEvent, ConversationSnapshot, StateUpdate, WorkingState, WorkingStateConfig,
    WorkingStateConfigError, WorkingStateError,
};
