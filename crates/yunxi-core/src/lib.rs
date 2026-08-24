//! Platform-neutral domain primitives and the observe-only Yunxi runtime.
//!
//! This crate describes what Yunxi observes and keeps in working state. Hosts
//! translate environment-specific events into these types and own all concrete
//! side effects and infrastructure.

pub mod attention;
pub mod event;
pub mod identity;
pub mod ports;
pub mod runtime;
pub mod working_state;

pub use attention::{AttentionDisposition, AttentionReason, AttentionResult, AttentionSystem};
pub use event::{
    ActionFailedEvent, ActionSucceededEvent, EventPriority, EventScope, EventType,
    EventValidationError, GoalCompletedEvent, GoalUpdatedEvent, MessageContent,
    MessageReceivedEvent, MessageSentEvent, ProspectiveMemoryEvent, ReminderDueEvent,
    ToolCompletedEvent, ToolFailedEvent, TraceContext, TraceError, WorldEvent, WorldEventKind,
};
pub use identity::{
    ConversationId, ConversationKind, EventId, GoalId, MessageId, OpenLoopId, PersonId,
};
pub use runtime::{
    Admission, CognitiveRuntime, ProcessingOutcome, RuntimeConfig, RuntimeConfigError,
    RuntimeHandle, RuntimeObservation, SubmitError,
};
pub use working_state::{
    CompactEvent, ConversationSnapshot, StateUpdate, WorkingState, WorkingStateConfig,
    WorkingStateConfigError, WorkingStateError,
};
