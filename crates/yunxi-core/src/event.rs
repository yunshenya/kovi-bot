use crate::identity::{
    ConversationId, ConversationKind, EventId, GoalId, MessageId, OpenLoopId, PersonId,
};
use crate::intent::ToolNotificationPolicy;
use crate::planner::{InteractionCueValidationError, InteractionCues};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const MAX_MESSAGE_CONTENT_BYTES: usize = 32 * 1_024;
pub const MAX_MESSAGE_CONTENT_CHARS: usize = 8_192;
pub const MAX_MESSAGE_ATTACHMENTS: usize = 16;
pub const MAX_ATTACHMENT_REFERENCE_BYTES: usize = 4 * 1_024;
pub const MAX_ATTACHMENT_REFERENCE_CHARS: usize = 2 * 1_024;
pub const MAX_ATTACHMENT_MEDIA_TYPE_BYTES: usize = 256;
pub const MAX_ATTACHMENT_FILE_NAME_BYTES: usize = 1_024;
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1_024;
pub const MAX_TOOL_RESULT_CHARS: usize = 4_096;
pub const MAX_TOOL_ERROR_DETAIL_BYTES: usize = 4 * 1_024;
pub const MAX_TOOL_ERROR_DETAIL_CHARS: usize = 1_024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Audio,
    Video,
    File,
}

/// A platform-neutral reference to message media.
///
/// Core preserves the reference as an opaque value. A host may use a URL,
/// content-addressed key, or another adapter-owned locator without exposing a
/// platform message segment in the domain model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    kind: AttachmentKind,
    reference: String,
    media_type: Option<String>,
    file_name: Option<String>,
}

impl Attachment {
    pub fn new(
        kind: AttachmentKind,
        reference: impl Into<String>,
    ) -> Result<Self, MessageValidationError> {
        let attachment = Self {
            kind,
            reference: reference.into(),
            media_type: None,
            file_name: None,
        };
        attachment.validate()?;
        Ok(attachment)
    }

    pub fn with_media_type(
        mut self,
        media_type: Option<String>,
    ) -> Result<Self, MessageValidationError> {
        self.media_type = media_type;
        self.validate()?;
        Ok(self)
    }

    pub fn with_file_name(
        mut self,
        file_name: Option<String>,
    ) -> Result<Self, MessageValidationError> {
        self.file_name = file_name;
        self.validate()?;
        Ok(self)
    }

    #[must_use]
    pub const fn kind(&self) -> AttachmentKind {
        self.kind
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }

    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.file_name.as_deref()
    }

    pub fn validate(&self) -> Result<(), MessageValidationError> {
        validate_required_attachment_field(
            "reference",
            &self.reference,
            MAX_ATTACHMENT_REFERENCE_BYTES,
            Some(MAX_ATTACHMENT_REFERENCE_CHARS),
        )?;
        if let Some(media_type) = &self.media_type {
            validate_optional_attachment_field(
                "media_type",
                media_type,
                MAX_ATTACHMENT_MEDIA_TYPE_BYTES,
            )?;
        }
        if let Some(file_name) = &self.file_name {
            validate_optional_attachment_field(
                "file_name",
                file_name,
                MAX_ATTACHMENT_FILE_NAME_BYTES,
            )?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Attachment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: AttachmentKind,
            reference: String,
            media_type: Option<String>,
            file_name: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Attachment::new(wire.kind, wire.reference)
            .and_then(|attachment| attachment.with_media_type(wire.media_type))
            .and_then(|attachment| attachment.with_file_name(wire.file_name))
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageContent {
    text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<Attachment>,
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::text("")
    }
}

impl MessageContent {
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            text: value.into(),
            attachments: Vec::new(),
        }
    }

    #[must_use]
    pub fn as_text(&self) -> &str {
        &self.text
    }

    pub fn with_attachments(
        mut self,
        attachments: Vec<Attachment>,
    ) -> Result<Self, MessageValidationError> {
        self.attachments = attachments;
        self.validate()?;
        Ok(self)
    }

    #[must_use]
    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty()
    }

    pub fn validate(&self) -> Result<(), MessageValidationError> {
        if self.text.contains('\0') {
            return Err(MessageValidationError::TextContainsNul);
        }
        if self.text.len() > MAX_MESSAGE_CONTENT_BYTES {
            return Err(MessageValidationError::TextTooLong {
                length: self.text.len(),
                maximum: MAX_MESSAGE_CONTENT_BYTES,
            });
        }
        let chars = self.text.chars().count();
        if chars > MAX_MESSAGE_CONTENT_CHARS {
            return Err(MessageValidationError::TextTooManyCharacters {
                length: chars,
                maximum: MAX_MESSAGE_CONTENT_CHARS,
            });
        }
        if self.attachments.len() > MAX_MESSAGE_ATTACHMENTS {
            return Err(MessageValidationError::TooManyAttachments {
                length: self.attachments.len(),
                maximum: MAX_MESSAGE_ATTACHMENTS,
            });
        }
        for attachment in &self.attachments {
            attachment.validate()?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            text: String,
            #[serde(default)]
            attachments: Vec<Attachment>,
        }

        let wire = Wire::deserialize(deserializer)?;
        MessageContent::text(wire.text)
            .with_attachments(wire.attachments)
            .map_err(serde::de::Error::custom)
    }
}

/// A complete message after a host has normalized platform input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub sender: PersonId,
    pub content: MessageContent,
    pub timestamp: DateTime<Utc>,
    pub reply_to: Option<MessageId>,
}

impl Message {
    pub fn new(
        id: MessageId,
        conversation_id: ConversationId,
        sender: PersonId,
        content: MessageContent,
        timestamp: DateTime<Utc>,
    ) -> Result<Self, MessageValidationError> {
        let message = Self {
            id,
            conversation_id,
            sender,
            content,
            timestamp,
            reply_to: None,
        };
        message.validate()?;
        Ok(message)
    }

    #[must_use]
    pub fn with_reply_to(mut self, reply_to: Option<MessageId>) -> Self {
        self.reply_to = reply_to;
        self
    }

    pub fn validate(&self) -> Result<(), MessageValidationError> {
        self.content.validate()
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: MessageId,
            conversation_id: ConversationId,
            sender: PersonId,
            content: MessageContent,
            timestamp: DateTime<Utc>,
            reply_to: Option<MessageId>,
        }

        let wire = Wire::deserialize(deserializer)?;
        Message::new(
            wire.id,
            wire.conversation_id,
            wire.sender,
            wire.content,
            wire.timestamp,
        )
        .map(|message| message.with_reply_to(wire.reply_to))
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MessageValidationError {
    #[error("message text must not contain NUL")]
    TextContainsNul,
    #[error("message text is {length} bytes, above maximum {maximum}")]
    TextTooLong { length: usize, maximum: usize },
    #[error("message text is {length} characters, above maximum {maximum}")]
    TextTooManyCharacters { length: usize, maximum: usize },
    #[error("message has {length} attachments, above maximum {maximum}")]
    TooManyAttachments { length: usize, maximum: usize },
    #[error("attachment {field} must not be empty")]
    EmptyAttachmentField { field: &'static str },
    #[error("attachment {field} must not contain NUL")]
    AttachmentFieldContainsNul { field: &'static str },
    #[error("attachment {field} is {length} bytes, above maximum {maximum}")]
    AttachmentFieldTooLong {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error("attachment {field} is {length} characters, above maximum {maximum}")]
    AttachmentFieldTooManyCharacters {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
}

fn validate_required_attachment_field(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
    maximum_chars: Option<usize>,
) -> Result<(), MessageValidationError> {
    if value.trim().is_empty() {
        return Err(MessageValidationError::EmptyAttachmentField { field });
    }
    validate_attachment_field(field, value, maximum_bytes, maximum_chars)
}

fn validate_optional_attachment_field(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), MessageValidationError> {
    if value.trim().is_empty() {
        return Err(MessageValidationError::EmptyAttachmentField { field });
    }
    validate_attachment_field(field, value, maximum_bytes, None)
}

fn validate_attachment_field(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
    maximum_chars: Option<usize>,
) -> Result<(), MessageValidationError> {
    if value.contains('\0') {
        return Err(MessageValidationError::AttachmentFieldContainsNul { field });
    }
    if value.len() > maximum_bytes {
        return Err(MessageValidationError::AttachmentFieldTooLong {
            field,
            length: value.len(),
            maximum: maximum_bytes,
        });
    }
    if let Some(maximum) = maximum_chars {
        let length = value.chars().count();
        if length > maximum {
            return Err(MessageValidationError::AttachmentFieldTooManyCharacters {
                field,
                length,
                maximum,
            });
        }
    }
    Ok(())
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
    /// Whether this observation may produce a visible reply.
    ///
    /// Hosts set this to `false` when another execution path owns user-visible
    /// effects but Core should still observe the message and update state.
    #[serde(default = "default_visible_reply_allowed")]
    pub visible_reply_allowed: bool,
}

const fn default_visible_reply_allowed() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSentEvent {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    pub timestamp: DateTime<Utc>,
    /// Bounded content of a successfully delivered Core action. Older event
    /// payloads may omit it, in which case WorkingState still records timing
    /// and identity without inventing conversation text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageCollisionDetectedEvent {
    pub conversation_id: ConversationId,
    pub outgoing_generation: u64,
    pub conversation_version: u64,
    pub fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCompletedEvent {
    pub operation: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub requires_follow_up: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailedEvent {
    pub operation: String,
    pub error_category: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub requires_follow_up: bool,
}

/// Fixed-point wire representation of semantic evidence already produced by
/// a host understanding pass. Fixed-point fields keep WorldEvent equality and
/// serialization deterministic while the planner consumes normalized floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionCuesObservedEvent {
    pub person_id: PersonId,
    pub sentiment_valence_millis: i16,
    pub sentiment_arousal_millis: i16,
    pub sentiment_confidence_millis: u16,
    pub gratitude_strength_millis: u16,
}

impl InteractionCuesObservedEvent {
    pub fn new(
        person_id: PersonId,
        cues: InteractionCues,
    ) -> Result<Self, InteractionCueValidationError> {
        cues.validate()?;
        Ok(Self {
            person_id,
            sentiment_valence_millis: (cues.sentiment_valence * 1_000.0).round() as i16,
            sentiment_arousal_millis: (cues.sentiment_arousal * 1_000.0).round() as i16,
            sentiment_confidence_millis: (cues.sentiment_confidence * 1_000.0).round() as u16,
            gratitude_strength_millis: (cues.gratitude_strength * 1_000.0).round() as u16,
        })
    }

    #[must_use]
    pub fn cues(self) -> InteractionCues {
        InteractionCues {
            sentiment_valence: f32::from(self.sentiment_valence_millis) / 1_000.0,
            sentiment_arousal: f32::from(self.sentiment_arousal_millis) / 1_000.0,
            sentiment_confidence: f32::from(self.sentiment_confidence_millis) / 1_000.0,
            gratitude_strength: f32::from(self.gratitude_strength_millis) / 1_000.0,
        }
    }

    fn validate(self) -> Result<(), EventValidationError> {
        for (field, value) in [
            ("sentiment_valence_millis", self.sentiment_valence_millis),
            ("sentiment_arousal_millis", self.sentiment_arousal_millis),
        ] {
            if !(-1_000..=1_000).contains(&value) {
                return Err(EventValidationError::InteractionCueOutOfRange { field });
            }
        }
        for (field, value) in [
            (
                "sentiment_confidence_millis",
                self.sentiment_confidence_millis,
            ),
            ("gratitude_strength_millis", self.gratitude_strength_millis),
        ] {
            if value > 1_000 {
                return Err(EventValidationError::InteractionCueOutOfRange { field });
            }
        }
        Ok(())
    }
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

/// A host-generated opportunity for the agent to continue an active
/// conversation without a new inbound message.
///
/// The conversation scope carries the routing identity; the optional flag
/// records whether a user explicitly requested an open-ended multi-turn
/// exchange. The host remains responsible for admission, cooldowns, and
/// eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AutonomousConversationTickEvent {
    #[serde(default)]
    pub explicit_continuation_requested: bool,
    /// Legacy compatibility field. Open-ended continuation no longer forces a
    /// minimum number of messages; this value is ignored by current runtimes.
    #[serde(default)]
    pub minimum_messages_pending: bool,
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

/// A proposed action that was refused before a host attempted a side effect.
///
/// Rejections are events as well as return values so callers can feed the
/// decision back into the same bounded runtime without pretending delivery
/// failed at the platform layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRejectedEvent {
    pub idempotency_key: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WorldEventKind {
    MessageReceived(MessageReceivedEvent),
    MessageSent(MessageSentEvent),
    MessageCollisionDetected(MessageCollisionDetectedEvent),
    InteractionCuesObserved(InteractionCuesObservedEvent),
    ToolCompleted(ToolCompletedEvent),
    ToolFailed(ToolFailedEvent),
    ReminderDue(ReminderDueEvent),
    GoalUpdated(GoalUpdatedEvent),
    GoalCompleted(GoalCompletedEvent),
    ProspectiveMemoryDue(ProspectiveMemoryEvent),
    AutonomousConversationTick(AutonomousConversationTickEvent),
    ActionSucceeded(ActionSucceededEvent),
    ActionFailed(ActionFailedEvent),
    ActionRejected(ActionRejectedEvent),
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
            Self::MessageCollisionDetected(_) => EventType::MessageCollisionDetected,
            Self::InteractionCuesObserved(_) => EventType::InteractionCuesObserved,
            Self::ToolCompleted(_) => EventType::ToolCompleted,
            Self::ToolFailed(_) => EventType::ToolFailed,
            Self::ReminderDue(_) => EventType::ReminderDue,
            Self::GoalUpdated(_) => EventType::GoalUpdated,
            Self::GoalCompleted(_) => EventType::GoalCompleted,
            Self::ProspectiveMemoryDue(_) => EventType::ProspectiveMemoryDue,
            Self::AutonomousConversationTick(_) => EventType::AutonomousConversationTick,
            Self::ActionSucceeded(_) => EventType::ActionSucceeded,
            Self::ActionFailed(_) => EventType::ActionFailed,
            Self::ActionRejected(_) => EventType::ActionRejected,
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
    MessageCollisionDetected,
    InteractionCuesObserved,
    ToolCompleted,
    ToolFailed,
    ReminderDue,
    GoalUpdated,
    GoalCompleted,
    ProspectiveMemoryDue,
    AutonomousConversationTick,
    ActionSucceeded,
    ActionFailed,
    ActionRejected,
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
    /// Optional trusted actor provenance for host-produced action results.
    ///
    /// This lives on the envelope rather than the public payload structs so
    /// older callers that construct `ToolCompletedEvent`/`ToolFailedEvent`
    /// remain source-compatible. Missing provenance is intentionally treated
    /// as unauthorised for actions that require an actor.
    // Provenance is process-local trust data. It may be serialized for
    // diagnostics, but wire input must never be able to mint an actor.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    actor: Option<PersonId>,
    /// Opaque originating Core message for derived action results. This is
    /// process-local context used by adapters that need to map back to a
    /// platform message; it is never accepted from or emitted to the wire.
    #[serde(skip)]
    source_message_id: Option<MessageId>,
    /// Trusted request-level preference for how tool result follow-ups should
    /// be delivered. It follows the causal chain but cannot be set by wire
    /// input.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    tool_notification_policy: Option<ToolNotificationPolicy>,
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
            actor: None,
            source_message_id: None,
            tool_notification_policy: None,
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
            actor: None,
            source_message_id: parent.source_message_id(),
            tool_notification_policy: parent.tool_notification_policy(),
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

    /// Returns the trusted actor provenance attached by the producing host.
    #[must_use]
    pub const fn actor(&self) -> Option<PersonId> {
        self.actor
    }

    /// Attaches trusted actor provenance to a derived event.
    #[must_use]
    pub const fn with_actor(mut self, actor: PersonId) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Returns the originating Core message for this event's causal chain.
    /// Received messages derive it from their payload; later derived events
    /// carry it on the private envelope.
    #[must_use]
    pub const fn source_message_id(&self) -> Option<MessageId> {
        match self.source_message_id {
            Some(message_id) => Some(message_id),
            None => match &self.kind {
                WorldEventKind::MessageReceived(message) => Some(message.message_id),
                _ => None,
            },
        }
    }

    #[must_use]
    pub const fn tool_notification_policy(&self) -> Option<ToolNotificationPolicy> {
        self.tool_notification_policy
    }

    #[must_use]
    pub const fn with_tool_notification_policy(mut self, policy: ToolNotificationPolicy) -> Self {
        self.tool_notification_policy = Some(policy);
        self
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
            WorldEventKind::MessageCollisionDetected(collision) => Some(collision.conversation_id),
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
        if let WorldEventKind::InteractionCuesObserved(cues) = &self.kind
            && self.scope
                != (EventScope::Person {
                    person_id: cues.person_id,
                })
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
        if matches!(self.kind, WorldEventKind::AutonomousConversationTick(_))
            && !matches!(self.scope, EventScope::Conversation { .. })
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
                validate_event_message_content(&message.content)
            }
            WorldEventKind::MessageSent(message) => message
                .content
                .as_ref()
                .map_or(Ok(()), validate_event_message_content),
            WorldEventKind::ToolCompleted(tool) => {
                check_payload_size("tool_operation", tool.operation.len(), MAX_OPERATION_BYTES)?;
                check_payload_size("tool_output", tool.output.len(), MAX_TOOL_RESULT_BYTES)?;
                check_payload_char_count(
                    "tool_output",
                    tool.output.chars().count(),
                    MAX_TOOL_RESULT_CHARS,
                )
            }
            WorldEventKind::ToolFailed(tool) => {
                check_payload_size("tool_operation", tool.operation.len(), MAX_OPERATION_BYTES)?;
                check_payload_size(
                    "error_category",
                    tool.error_category.len(),
                    MAX_ERROR_CATEGORY_BYTES,
                )?;
                check_payload_size(
                    "tool_error_detail",
                    tool.detail.len(),
                    MAX_TOOL_ERROR_DETAIL_BYTES,
                )?;
                check_payload_char_count(
                    "tool_error_detail",
                    tool.detail.chars().count(),
                    MAX_TOOL_ERROR_DETAIL_CHARS,
                )
            }
            WorldEventKind::InteractionCuesObserved(cues) => cues.validate(),
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
            WorldEventKind::ActionRejected(action) => {
                check_payload_size(
                    "idempotency_key",
                    action.idempotency_key.len(),
                    MAX_IDEMPOTENCY_KEY_BYTES,
                )?;
                check_payload_size(
                    "action_rejection_reason",
                    action.reason.len(),
                    MAX_ERROR_CATEGORY_BYTES,
                )
            }
            _ => Ok(()),
        }
    }
}

fn validate_event_message_content(content: &MessageContent) -> Result<(), EventValidationError> {
    match content.validate() {
        Ok(()) => Ok(()),
        Err(MessageValidationError::TextTooLong { length, maximum }) => {
            Err(EventValidationError::PayloadTooLarge {
                field: "message_content",
                length,
                maximum,
            })
        }
        Err(MessageValidationError::TextTooManyCharacters { length, maximum }) => {
            Err(EventValidationError::PayloadTooLong {
                field: "message_content",
                length,
                maximum,
            })
        }
        Err(error) => Err(EventValidationError::InvalidMessageContent(error)),
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
    #[error("event contains invalid message content: {0}")]
    InvalidMessageContent(MessageValidationError),
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
    #[error("interaction cue `{field}` is outside its fixed-point range")]
    InteractionCueOutOfRange { field: &'static str },
}

#[cfg(test)]
mod tests {
    use super::{
        Attachment, AttachmentKind, AutonomousConversationTickEvent, EventPriority, EventScope,
        EventValidationError, GoalCompletedEvent, GoalUpdatedEvent, InteractionCuesObservedEvent,
        MAX_MESSAGE_ATTACHMENTS, MAX_TOOL_ERROR_DETAIL_CHARS, MAX_TOOL_RESULT_BYTES, Message,
        MessageContent, MessageReceivedEvent, MessageSentEvent, MessageValidationError,
        ToolCompletedEvent, ToolFailedEvent, TraceError, WorldEvent, WorldEventKind,
    };
    use crate::{
        ConversationId, ConversationKind, EventId, GoalId, InteractionCues, MessageId, PersonId,
        ToolNotificationPolicy,
    };
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
    fn derived_events_keep_the_opaque_source_message_context() {
        let message_id = MessageId::new();
        let conversation_id = ConversationId::new();
        let root = WorldEvent::message_received(
            EventPriority::High,
            MessageReceivedEvent {
                message_id,
                conversation_id,
                sender: PersonId::new(),
                content: MessageContent::text("查资料"),
                reply_to: None,
                timestamp: Utc::now(),
                conversation_kind: ConversationKind::Direct,
                addressed_to_agent: true,
                replies_to_agent: false,
                stop_requested: false,
                explicit_request: true,
                visible_reply_allowed: true,
            },
        );
        let child = WorldEvent::derived_from(
            &root,
            Utc::now(),
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::ToolCompleted(ToolCompletedEvent {
                operation: "web.search".to_owned(),
                output: "结果".to_owned(),
                requires_follow_up: true,
            }),
            8,
        )
        .expect("derived tool event should be valid");
        assert_eq!(root.source_message_id(), Some(message_id));
        assert_eq!(child.source_message_id(), Some(message_id));
    }

    #[test]
    fn legacy_tool_events_default_to_observation_only() {
        let completed: ToolCompletedEvent = serde_json::from_value(serde_json::json!({
            "operation": "weather.current"
        }))
        .expect("legacy completed event");
        let failed: ToolFailedEvent = serde_json::from_value(serde_json::json!({
            "operation": "weather.current",
            "error_category": "timeout"
        }))
        .expect("legacy failed event");

        assert!(completed.output.is_empty());
        assert!(!completed.requires_follow_up);
        assert!(failed.detail.is_empty());
        assert!(!failed.requires_follow_up);
    }

    #[test]
    fn tool_feedback_payloads_are_bounded() {
        let completed = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::ToolCompleted(ToolCompletedEvent {
                operation: "weather.current".to_string(),
                output: "x".repeat(MAX_TOOL_RESULT_BYTES + 1),
                requires_follow_up: true,
            }),
        );
        assert!(matches!(
            completed.validate(8),
            Err(EventValidationError::PayloadTooLarge {
                field: "tool_output",
                ..
            })
        ));

        let failed = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::ToolFailed(ToolFailedEvent {
                operation: "weather.current".to_string(),
                error_category: "timeout".to_string(),
                detail: "错".repeat(MAX_TOOL_ERROR_DETAIL_CHARS + 1),
                requires_follow_up: true,
            }),
        );
        assert!(matches!(
            failed.validate(8),
            Err(EventValidationError::PayloadTooLong {
                field: "tool_error_detail",
                ..
            })
        ));
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
    fn autonomous_conversation_ticks_require_conversation_scope() {
        let valid = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation {
                conversation_id: ConversationId::new(),
            },
            EventPriority::Low,
            WorldEventKind::AutonomousConversationTick(AutonomousConversationTickEvent::default()),
        );
        assert_eq!(valid.validate(8), Ok(()));

        let invalid = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Low,
            WorldEventKind::AutonomousConversationTick(AutonomousConversationTickEvent::default()),
        );
        assert_eq!(
            invalid.validate(8),
            Err(EventValidationError::ScopeMismatch)
        );
    }

    #[test]
    fn goal_events_require_their_goal_scope() {
        let goal_id = GoalId::new();
        let valid = WorldEvent::new(
            Utc::now(),
            EventScope::Goal { goal_id },
            EventPriority::Normal,
            WorldEventKind::GoalUpdated(GoalUpdatedEvent { goal_id }),
        );
        assert_eq!(valid.validate(8), Ok(()));

        let mismatched = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::Normal,
            WorldEventKind::GoalCompleted(GoalCompletedEvent { goal_id }),
        );
        assert_eq!(
            mismatched.validate(8),
            Err(EventValidationError::ScopeMismatch)
        );
    }

    #[test]
    fn interaction_cues_use_a_validated_fixed_point_wire_format() {
        let person_id = PersonId::new();
        let observed = InteractionCuesObservedEvent::new(
            person_id,
            InteractionCues {
                sentiment_valence: -0.625,
                sentiment_arousal: 0.375,
                sentiment_confidence: 0.875,
                gratitude_strength: 0.75,
            },
        )
        .expect("bounded cues");
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Person { person_id },
            EventPriority::Normal,
            WorldEventKind::InteractionCuesObserved(observed),
        );

        assert_eq!(observed.sentiment_valence_millis, -625);
        assert_eq!(observed.sentiment_arousal_millis, 375);
        assert_eq!(observed.sentiment_confidence_millis, 875);
        assert_eq!(observed.gratitude_strength_millis, 750);
        assert_eq!(event.validate(8), Ok(()));

        let encoded = serde_json::to_string(&event).expect("cue event should serialize");
        let decoded: WorldEvent =
            serde_json::from_str(&encoded).expect("cue event should deserialize");
        assert_eq!(decoded, event);
        assert_eq!(decoded.validate(8), Ok(()));
    }

    #[test]
    fn interaction_cues_reject_out_of_range_fixed_point_values() {
        let person_id = PersonId::new();
        for (observed, field) in [
            (
                InteractionCuesObservedEvent {
                    person_id,
                    sentiment_valence_millis: 1_001,
                    sentiment_arousal_millis: 0,
                    sentiment_confidence_millis: 0,
                    gratitude_strength_millis: 0,
                },
                "sentiment_valence_millis",
            ),
            (
                InteractionCuesObservedEvent {
                    person_id,
                    sentiment_valence_millis: 0,
                    sentiment_arousal_millis: -1_001,
                    sentiment_confidence_millis: 0,
                    gratitude_strength_millis: 0,
                },
                "sentiment_arousal_millis",
            ),
            (
                InteractionCuesObservedEvent {
                    person_id,
                    sentiment_valence_millis: 0,
                    sentiment_arousal_millis: 0,
                    sentiment_confidence_millis: 1_001,
                    gratitude_strength_millis: 0,
                },
                "sentiment_confidence_millis",
            ),
            (
                InteractionCuesObservedEvent {
                    person_id,
                    sentiment_valence_millis: 0,
                    sentiment_arousal_millis: 0,
                    sentiment_confidence_millis: 0,
                    gratitude_strength_millis: 1_001,
                },
                "gratitude_strength_millis",
            ),
        ] {
            let event = WorldEvent::new(
                Utc::now(),
                EventScope::Person { person_id },
                EventPriority::Normal,
                WorldEventKind::InteractionCuesObserved(observed),
            );
            assert_eq!(
                event.validate(8),
                Err(EventValidationError::InteractionCueOutOfRange { field })
            );
        }
    }

    #[test]
    fn interaction_cues_require_their_person_scope() {
        let person_id = PersonId::new();
        let observed = InteractionCuesObservedEvent::new(
            person_id,
            InteractionCues {
                sentiment_confidence: 0.8,
                ..InteractionCues::default()
            },
        )
        .expect("bounded cues");
        let mismatched = WorldEvent::new(
            Utc::now(),
            EventScope::Person {
                person_id: PersonId::new(),
            },
            EventPriority::Normal,
            WorldEventKind::InteractionCuesObserved(observed),
        );

        assert_eq!(
            mismatched.validate(8),
            Err(EventValidationError::ScopeMismatch)
        );
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
                visible_reply_allowed: true,
            },
        );
        let encoded = serde_json::to_string(&event).expect("event should serialize");
        let decoded: WorldEvent = serde_json::from_str(&encoded).expect("event should deserialize");
        assert_eq!(decoded, event);
        assert_eq!(decoded.validate(8), Ok(()));

        let mut legacy = serde_json::to_value(&event).expect("event should serialize");
        legacy["kind"]["payload"]
            .as_object_mut()
            .expect("message payload")
            .remove("visible_reply_allowed");
        let legacy: WorldEvent =
            serde_json::from_value(legacy).expect("older message event should deserialize");
        assert!(matches!(
            legacy.kind(),
            WorldEventKind::MessageReceived(message) if message.visible_reply_allowed
        ));

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

    #[test]
    fn actor_provenance_is_not_accepted_from_wire_input() {
        let actor = PersonId::new();
        let event = WorldEvent::new(
            Utc::now(),
            EventScope::Conversation {
                conversation_id: ConversationId::new(),
            },
            EventPriority::High,
            WorldEventKind::ToolCompleted(ToolCompletedEvent {
                operation: "weather.current".to_owned(),
                output: "晴".to_owned(),
                requires_follow_up: true,
            }),
        )
        .with_actor(actor);
        let encoded = serde_json::to_value(&event).expect("event should serialize");
        let actor_wire = actor.to_string();
        assert_eq!(
            encoded.get("actor").and_then(serde_json::Value::as_str),
            Some(actor_wire.as_str())
        );
        let decoded: WorldEvent =
            serde_json::from_value(encoded).expect("event should deserialize");
        assert_eq!(decoded.actor(), None);
    }

    #[test]
    fn tool_notification_policy_is_trusted_and_propagates_to_children() {
        let root = WorldEvent::new(
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::HostStarted,
        )
        .with_tool_notification_policy(ToolNotificationPolicy::EachAndFinal);
        let child = WorldEvent::derived_from(
            &root,
            Utc::now(),
            EventScope::Global,
            EventPriority::High,
            WorldEventKind::IdleTick,
            8,
        )
        .expect("derived event should be valid");
        assert_eq!(
            child.tool_notification_policy(),
            Some(ToolNotificationPolicy::EachAndFinal)
        );

        let encoded = serde_json::to_value(child).expect("event should serialize");
        assert_eq!(
            encoded
                .get("tool_notification_policy")
                .and_then(serde_json::Value::as_str),
            Some("each_and_final")
        );
        let decoded: WorldEvent =
            serde_json::from_value(encoded).expect("event should deserialize");
        assert_eq!(decoded.tool_notification_policy(), None);
    }

    #[test]
    fn sent_message_content_is_bounded_and_backward_compatible() {
        let timestamp = Utc::now();
        let conversation_id = ConversationId::new();
        let event = WorldEvent::new(
            timestamp,
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::MessageSent(MessageSentEvent {
                message_id: MessageId::new(),
                conversation_id,
                timestamp,
                content: Some(MessageContent::text("已经发出的回复")),
            }),
        );
        assert_eq!(event.validate(8), Ok(()));

        let mut older = serde_json::to_value(&event).expect("sent event should serialize");
        older["kind"]["payload"]
            .as_object_mut()
            .expect("sent message payload")
            .remove("content");
        let older: WorldEvent =
            serde_json::from_value(older).expect("older sent event should deserialize");
        assert!(matches!(
            older.kind(),
            WorldEventKind::MessageSent(message) if message.content.is_none()
        ));

        let oversized = WorldEvent::new(
            timestamp,
            EventScope::Conversation { conversation_id },
            EventPriority::High,
            WorldEventKind::MessageSent(MessageSentEvent {
                message_id: MessageId::new(),
                conversation_id,
                timestamp,
                content: Some(MessageContent::text("x".repeat(32 * 1_024 + 1))),
            }),
        );
        assert_eq!(
            oversized.validate(8),
            Err(EventValidationError::PayloadTooLarge {
                field: "message_content",
                length: 32 * 1_024 + 1,
                maximum: 32 * 1_024,
            })
        );
    }

    #[test]
    fn structured_message_content_preserves_text_compatibility() {
        let image = Attachment::new(AttachmentKind::Image, "asset:sha256:abc")
            .expect("opaque reference")
            .with_media_type(Some("image/png".to_owned()))
            .expect("media type")
            .with_file_name(Some("photo.png".to_owned()))
            .expect("file name");
        let content = MessageContent::text("")
            .with_attachments(vec![image.clone()])
            .expect("attachment-only content is valid");

        assert_eq!(content.as_text(), "");
        assert_eq!(content.attachments(), &[image]);
        assert!(!content.is_empty());

        let legacy: MessageContent =
            serde_json::from_str(r#"{"text":"legacy"}"#).expect("legacy text-only JSON");
        assert_eq!(legacy.as_text(), "legacy");
        assert!(legacy.attachments().is_empty());
        assert_eq!(
            serde_json::to_value(MessageContent::text("legacy")).expect("serialize content"),
            serde_json::json!({"text": "legacy"})
        );
    }

    #[test]
    fn attachment_and_message_deserialization_enforce_bounds() {
        let attachment =
            Attachment::new(AttachmentKind::File, "asset:document").expect("attachment");
        assert_eq!(
            MessageContent::text("body")
                .with_attachments(vec![attachment; MAX_MESSAGE_ATTACHMENTS + 1])
                .expect_err("attachment count is bounded"),
            MessageValidationError::TooManyAttachments {
                length: MAX_MESSAGE_ATTACHMENTS + 1,
                maximum: MAX_MESSAGE_ATTACHMENTS,
            }
        );
        assert!(
            serde_json::from_str::<Attachment>(
                r#"{"kind":"image","reference":"","media_type":null,"file_name":null}"#,
            )
            .is_err()
        );

        let message = Message::new(
            MessageId::new(),
            ConversationId::new(),
            PersonId::new(),
            MessageContent::text("hello"),
            Utc::now(),
        )
        .expect("valid message")
        .with_reply_to(Some(MessageId::new()));
        let encoded = serde_json::to_string(&message).expect("serialize message");
        assert_eq!(
            serde_json::from_str::<Message>(&encoded).expect("deserialize message"),
            message
        );
    }
}
