use crate::identity::{
    ConversationId, ConversationKind, EventId, GoalId, MessageId, OpenLoopId, PersonId,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_MESSAGE_CONTENT_BYTES: usize = 32 * 1_024;
pub const MAX_MESSAGE_CONTENT_CHARS: usize = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPriority {
    Critical,
    High,
    Normal,
    Low,
}

impl EventPriority {
    #[must_use]
    pub const fn requires_backpressure(self) -> bool {
        matches!(self, Self::Critical | Self::High)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventScope {
    Global,
    Conversation { conversation_id: ConversationId },
    Person { person_id: PersonId },
    Goal { goal_id: GoalId },
}

impl EventScope {
    #[must_use]
    pub const fn conversation_id(self) -> Option<ConversationId> {
        match self {
            Self::Conversation { conversation_id } => Some(conversation_id),
            Self::Global | Self::Person { .. } | Self::Goal { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    trace_id: EventId,
    root_event_id: EventId,
    parent_event_id: Option<EventId>,
    depth: u8,
}

impl TraceContext {
    #[must_use]
    pub const fn root(event_id: EventId) -> Self {
        Self {
            trace_id: event_id,
            root_event_id: event_id,
            parent_event_id: None,
            depth: 0,
        }
    }

    pub(crate) fn child(
        &self,
        parent_event_id: EventId,
        max_depth: u8,
    ) -> Result<Self, TraceError> {
        let attempted_depth = u16::from(self.depth) + 1;
        if attempted_depth > u16::from(max_depth) {
            return Err(TraceError::DepthExceeded {
                depth: attempted_depth,
                max_depth,
            });
        }
        let depth = attempted_depth as u8;
        Ok(Self {
            trace_id: self.trace_id,
            root_event_id: self.root_event_id,
            parent_event_id: Some(parent_event_id),
            depth,
        })
    }

    #[must_use]
    pub const fn trace_id(self) -> EventId {
        self.trace_id
    }

    #[must_use]
    pub const fn root_event_id(self) -> EventId {
        self.root_event_id
    }

    #[must_use]
    pub const fn parent_event_id(self) -> Option<EventId> {
        self.parent_event_id
    }

    #[must_use]
    pub const fn depth(self) -> u8 {
        self.depth
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TraceError {
    #[error("derived event depth {depth} exceeds configured maximum {max_depth}")]
    DepthExceeded { depth: u16, max_depth: u8 },
    #[error("cannot derive an event from an invalid trace context")]
    InvalidContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageContent {
    text: String,
}

impl MessageContent {
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self { text: value.into() }
    }

    #[must_use]
    pub fn as_text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReceivedEvent {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    pub sender: PersonId,
    pub content: MessageContent,
    pub reply_to: Option<MessageId>,
    pub timestamp: DateTime<Utc>,
    pub conversation_kind: ConversationKind,
    pub addressed_to_agent: bool,
    pub replies_to_agent: bool,
    pub stop_requested: bool,
    pub explicit_request: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSentEvent {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCompletedEvent {
    pub operation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailedEvent {
    pub operation: String,
    pub error_category: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderDueEvent {
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalUpdatedEvent {
    pub goal_id: GoalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCompletedEvent {
    pub goal_id: GoalId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProspectiveMemoryEvent {
    pub open_loop_id: OpenLoopId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSucceededEvent {
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionFailedEvent {
    pub idempotency_key: String,
    pub error_category: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WorldEventKind {
    MessageReceived(MessageReceivedEvent),
    MessageSent(MessageSentEvent),
    ToolCompleted(ToolCompletedEvent),
    ToolFailed(ToolFailedEvent),
    ReminderDue(ReminderDueEvent),
    GoalUpdated(GoalUpdatedEvent),
    GoalCompleted(GoalCompletedEvent),
    ProspectiveMemoryDue(ProspectiveMemoryEvent),
    ActionSucceeded(ActionSucceededEvent),
    ActionFailed(ActionFailedEvent),
    IdleTick,
    MaintenanceTick,
    HostStarted,
    HostStopping,
}

impl WorldEventKind {
    #[must_use]
    pub const fn event_type(&self) -> EventType {
        match self {
            Self::MessageReceived(_) => EventType::MessageReceived,
            Self::MessageSent(_) => EventType::MessageSent,
            Self::ToolCompleted(_) => EventType::ToolCompleted,
            Self::ToolFailed(_) => EventType::ToolFailed,
            Self::ReminderDue(_) => EventType::ReminderDue,
            Self::GoalUpdated(_) => EventType::GoalUpdated,
            Self::GoalCompleted(_) => EventType::GoalCompleted,
            Self::ProspectiveMemoryDue(_) => EventType::ProspectiveMemoryDue,
            Self::ActionSucceeded(_) => EventType::ActionSucceeded,
            Self::ActionFailed(_) => EventType::ActionFailed,
            Self::IdleTick => EventType::IdleTick,
            Self::MaintenanceTick => EventType::MaintenanceTick,
            Self::HostStarted => EventType::HostStarted,
            Self::HostStopping => EventType::HostStopping,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    MessageReceived,
    MessageSent,
    ToolCompleted,
    ToolFailed,
    ReminderDue,
    GoalUpdated,
    GoalCompleted,
    ProspectiveMemoryDue,
    ActionSucceeded,
    ActionFailed,
    IdleTick,
    MaintenanceTick,
    HostStarted,
    HostStopping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldEvent {
    id: EventId,
    occurred_at: DateTime<Utc>,
    scope: EventScope,
    priority: EventPriority,
    trace: TraceContext,
    kind: WorldEventKind,
}

impl WorldEvent {
    #[must_use]
    pub fn new(
        occurred_at: DateTime<Utc>,
        scope: EventScope,
        priority: EventPriority,
        kind: WorldEventKind,
    ) -> Self {
        let id = EventId::new();
        Self {
            id,
            occurred_at,
            scope,
            priority,
            trace: TraceContext::root(id),
            kind,
        }
    }

    #[must_use]
    pub fn message_received(priority: EventPriority, message: MessageReceivedEvent) -> Self {
        Self::new(
            message.timestamp,
            EventScope::Conversation {
                conversation_id: message.conversation_id,
            },
            priority,
            WorldEventKind::MessageReceived(message),
        )
    }

    pub fn derived_from(
        parent: &Self,
        occurred_at: DateTime<Utc>,
        scope: EventScope,
        priority: EventPriority,
        kind: WorldEventKind,
        max_depth: u8,
    ) -> Result<Self, TraceError> {
        parent
            .validate_trace(max_depth)
            .map_err(|_| TraceError::InvalidContext)?;
        let id = EventId::new();
        Ok(Self {
            id,
            occurred_at,
            scope,
            priority,
            trace: parent.trace.child(parent.id, max_depth)?,
            kind,
        })
    }

    #[must_use]
    pub const fn id(&self) -> EventId {
        self.id
    }

    #[must_use]
    pub const fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    #[must_use]
    pub const fn scope(&self) -> EventScope {
        self.scope
    }

    #[must_use]
    pub const fn priority(&self) -> EventPriority {
        self.priority
    }

    #[must_use]
    pub const fn trace(&self) -> TraceContext {
        self.trace
    }

    #[must_use]
    pub const fn kind(&self) -> &WorldEventKind {
        &self.kind
    }

    pub fn validate(&self, max_trace_depth: u8) -> Result<(), EventValidationError> {
        self.validate_trace(max_trace_depth)?;
        self.validate_scope()?;
        self.validate_payload()
    }

    fn validate_trace(&self, max_trace_depth: u8) -> Result<(), EventValidationError> {
        if self.trace.depth > max_trace_depth {
            return Err(EventValidationError::TraceDepthExceeded {
                depth: self.trace.depth,
                maximum: max_trace_depth,
            });
        }
        let valid = self.trace.trace_id == self.trace.root_event_id
            && match self.trace.depth {
                0 => self.trace.root_event_id == self.id && self.trace.parent_event_id.is_none(),
                1 => {
                    self.trace.root_event_id != self.id
                        && self.trace.parent_event_id == Some(self.trace.root_event_id)
                }
                _ => {
                    self.trace.root_event_id != self.id
                        && self.trace.parent_event_id.is_some_and(|parent| {
                            parent != self.id && parent != self.trace.root_event_id
                        })
                }
            };
        if !valid {
            return Err(EventValidationError::InvalidTraceContext);
        }
        Ok(())
    }

    pub(crate) fn validate_scope(&self) -> Result<(), EventValidationError> {
        let expected_conversation = match &self.kind {
            WorldEventKind::MessageReceived(message) => {
                if message.timestamp != self.occurred_at {
                    return Err(EventValidationError::TimestampMismatch);
                }
                Some(message.conversation_id)
            }
            WorldEventKind::MessageSent(message) => {
                if message.timestamp != self.occurred_at {
                    return Err(EventValidationError::TimestampMismatch);
                }
                Some(message.conversation_id)
            }
            _ => None,
        };
        if let Some(expected) = expected_conversation
            && self.scope.conversation_id() != Some(expected)
        {
            return Err(EventValidationError::ScopeMismatch);
        }

        let expected_goal = match &self.kind {
            WorldEventKind::GoalUpdated(goal) => Some(goal.goal_id),
            WorldEventKind::GoalCompleted(goal) => Some(goal.goal_id),
            _ => None,
        };
        if let Some(expected) = expected_goal
            && self.scope != (EventScope::Goal { goal_id: expected })
        {
            return Err(EventValidationError::ScopeMismatch);
        }
        if matches!(
            self.kind,
            WorldEventKind::IdleTick
                | WorldEventKind::MaintenanceTick
                | WorldEventKind::HostStarted
                | WorldEventKind::HostStopping
        ) && self.scope != EventScope::Global
        {
            return Err(EventValidationError::ScopeMismatch);
        }
        Ok(())
    }

    fn validate_payload(&self) -> Result<(), EventValidationError> {
        const MAX_OPERATION_BYTES: usize = 1_024;
        const MAX_REFERENCE_BYTES: usize = 1_024;
        const MAX_IDEMPOTENCY_KEY_BYTES: usize = 512;
        const MAX_ERROR_CATEGORY_BYTES: usize = 256;

        match &self.kind {
            WorldEventKind::MessageReceived(message) => {
                check_payload_size(
                    "message_content",
                    message.content.text.len(),
                    MAX_MESSAGE_CONTENT_BYTES,
                )?;
                check_payload_char_count(
                    "message_content",
                    message.content.text.chars().count(),
                    MAX_MESSAGE_CONTENT_CHARS,
                )
            }
            WorldEventKind::ToolCompleted(tool) => {
                check_payload_size("tool_operation", tool.operation.len(), MAX_OPERATION_BYTES)
            }
            WorldEventKind::ToolFailed(tool) => {
                check_payload_size("tool_operation", tool.operation.len(), MAX_OPERATION_BYTES)?;
                check_payload_size(
                    "error_category",
                    tool.error_category.len(),
                    MAX_ERROR_CATEGORY_BYTES,
                )
            }
            WorldEventKind::ReminderDue(reminder) => check_payload_size(
                "reminder_reference",
                reminder.reference.len(),
                MAX_REFERENCE_BYTES,
            ),
            WorldEventKind::ActionSucceeded(action) => check_payload_size(
                "idempotency_key",
                action.idempotency_key.len(),
                MAX_IDEMPOTENCY_KEY_BYTES,
            ),
            WorldEventKind::ActionFailed(action) => {
                check_payload_size(
                    "idempotency_key",
                    action.idempotency_key.len(),
                    MAX_IDEMPOTENCY_KEY_BYTES,
                )?;
                check_payload_size(
                    "error_category",
                    action.error_category.len(),
                    MAX_ERROR_CATEGORY_BYTES,
                )
            }
            _ => Ok(()),
        }
    }
}

fn check_payload_size(
    field: &'static str,
    length: usize,
    maximum: usize,
) -> Result<(), EventValidationError> {
    if length > maximum {
        return Err(EventValidationError::PayloadTooLarge {
            field,
            length,
            maximum,
        });
    }
    Ok(())
}

fn check_payload_char_count(
    field: &'static str,
    length: usize,
    maximum: usize,
) -> Result<(), EventValidationError> {
    if length > maximum {
        return Err(EventValidationError::PayloadTooLong {
            field,
            length,
            maximum,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EventValidationError {
    #[error("event trace depth {depth} exceeds runtime maximum {maximum}")]
    TraceDepthExceeded { depth: u8, maximum: u8 },
    #[error("event trace context is internally inconsistent")]
    InvalidTraceContext,
    #[error("event scope does not match its domain payload")]
    ScopeMismatch,
    #[error("event timestamp does not match its message payload")]
    TimestampMismatch,
    #[error("event payload `{field}` is {length} bytes, above maximum {maximum}")]
    PayloadTooLarge {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("event payload `{field}` is {length} characters, above maximum {maximum}")]
    PayloadTooLong {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        EventPriority, EventScope, EventValidationError, MessageContent, MessageReceivedEvent,
        TraceError, WorldEvent, WorldEventKind,
    };
    use crate::{ConversationId, ConversationKind, EventId, MessageId, PersonId};
    use chrono::Utc;

    #[test]
    fn derived_events_keep_trace_and_enforce_depth() {
        let root = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::HostStarted,
        );
        let child = WorldEvent::derived_from(
            &root,
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::IdleTick,
            1,
        )
        .expect("first derived event should fit");

        assert_eq!(child.trace().depth(), 1);
        assert_eq!(child.trace().trace_id(), root.trace().trace_id());
        assert_eq!(child.trace().root_event_id(), root.id());
        assert_eq!(child.trace().parent_event_id(), Some(root.id()));
        assert_eq!(
            WorldEvent::derived_from(
                &child,
                Utc::now(),
                EventScope::Global,
                EventPriority::Normal,
                WorldEventKind::IdleTick,
                1,
            ),
            Err(TraceError::DepthExceeded {
                depth: 2,
                max_depth: 1,
            })
        );
    }

    #[test]
    fn deserialized_invalid_trace_is_rejected() {
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::HostStarted,
        );
        let mut value = serde_json::to_value(&event).expect("event should serialize");
        value["trace"]["depth"] = serde_json::json!(1);
        let malformed: WorldEvent =
            serde_json::from_value(value).expect("wire shape remains parseable");

        assert_eq!(
            malformed.validate(8),
            Err(super::EventValidationError::InvalidTraceContext)
        );

        let mut value = serde_json::to_value(&event).expect("event should serialize");
        value["trace"]["depth"] = serde_json::json!(1);
        value["trace"]["parent_event_id"] = serde_json::json!(EventId::new().to_string());
        let forged_child: WorldEvent =
            serde_json::from_value(value).expect("wire shape remains parseable");
        assert_eq!(
            forged_child.validate(8),
            Err(super::EventValidationError::InvalidTraceContext)
        );
    }

    #[test]
    fn lifecycle_events_require_global_scope() {
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation {
                conversation_id: ConversationId::new(),
            },
            EventPriority::Normal,
            WorldEventKind::HostStarted,
        );

        assert_eq!(event.validate(8), Err(EventValidationError::ScopeMismatch));
    }

    #[test]
    fn event_round_trips_and_oversized_content_is_rejected() {
        let event = WorldEvent::message_received(
            EventPriority::Normal,
            MessageReceivedEvent {
                message_id: MessageId::new(),
                conversation_id: ConversationId::new(),
                sender: PersonId::new(),
                content: MessageContent::text("hello"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Direct,
                addressed_to_agent: false,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: false,
            },
        );
        let encoded = serde_json::to_string(&event).expect("event should serialize");
        let decoded: WorldEvent = serde_json::from_str(&encoded).expect("event should deserialize");
        assert_eq!(decoded, event);
        assert_eq!(decoded.validate(8), Ok(()));

        let oversized = match event.kind().clone() {
            WorldEventKind::MessageReceived(mut message) => {
                message.content = MessageContent::text("x".repeat(32 * 1_024 + 1));
                WorldEvent::message_received(EventPriority::Normal, message)
            }
            _ => unreachable!("fixture is a received message"),
        };
        assert_eq!(
            oversized.validate(8),
            Err(EventValidationError::PayloadTooLarge {
                field: "message_content",
                length: 32 * 1_024 + 1,
                maximum: 32 * 1_024,
            })
        );
    }
}
