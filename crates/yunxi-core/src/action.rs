//! Platform-neutral actions proposed by the cognitive core.
//!
//! Actions deliberately describe intent in terms of Core identifiers.  An
//! adapter is responsible for translating these values into a concrete
//! platform operation after the action arbiter has admitted the action.

use crate::goal::{GoalDraft, GoalOwner, GoalValidationError};
use crate::identity::{ConversationId, GoalId, MessageId, OpenLoopId, PersonId};
use crate::open_loop::{OpenLoopDraft, OpenLoopOwner, OpenLoopValidationError};
use crate::proactive::ProactiveMotive;
use crate::{EventId, MessageContent, MessageValidationError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

pub const MAX_ACTION_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_ACTION_IDEMPOTENCY_KEY_CHARS: usize = 128;
pub const MAX_TOOL_NAME_BYTES: usize = 256;
pub const MAX_TOOL_NAME_CHARS: usize = 128;
pub const MAX_TOOL_INPUT_BYTES: usize = 32 * 1_024;
pub const MAX_TOOL_INPUT_CHARS: usize = 16 * 1_024;

/// Stable, event-local idempotency key for one planned action.
///
/// Hosts that retain ingress capabilities can derive this before a
/// [`CognitiveIntent`] is materialized, while the runtime applies the same key
/// to the resulting action. `EventId` makes equal intents from different turns
/// unambiguously distinct.
#[must_use]
pub fn event_action_idempotency_key(event_id: EventId, intent_index: usize) -> String {
    format!("event:{event_id}:intent:{intent_index}")
}

/// The Core scope used for authorization and per-target cooldowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ActionScope {
    Conversation(ConversationId),
    Person(PersonId),
    Global,
}

impl ActionScope {
    #[must_use]
    pub const fn for_open_loop_owner(owner: OpenLoopOwner) -> Self {
        match owner {
            OpenLoopOwner::Conversation(id) => Self::Conversation(id),
            OpenLoopOwner::Person(id) => Self::Person(id),
            OpenLoopOwner::Global => Self::Global,
        }
    }

    #[must_use]
    pub const fn for_goal_owner(owner: GoalOwner) -> Self {
        match owner {
            GoalOwner::Conversation(id) => Self::Conversation(id),
            GoalOwner::Person(id) => Self::Person(id),
            GoalOwner::Global => Self::Global,
        }
    }
}

/// Opaque identity for one proposed side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActionId(Uuid);

impl ActionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for ActionId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for ActionId {
    fn from(value: Uuid) -> Self {
        Self::from_uuid(value)
    }
}

impl From<ActionId> for Uuid {
    fn from(value: ActionId) -> Self {
        value.0
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Metadata used by the arbiter to reject stale or replayed decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionMetadata {
    pub action_id: ActionId,
    pub idempotency_key: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub generation: u64,
    pub actor: Option<PersonId>,
}

impl<'de> Deserialize<'de> for ActionMetadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            action_id: ActionId,
            idempotency_key: String,
            issued_at: DateTime<Utc>,
            expires_at: Option<DateTime<Utc>>,
            generation: u64,
            actor: Option<PersonId>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let metadata = Self {
            action_id: wire.action_id,
            idempotency_key: wire.idempotency_key,
            issued_at: wire.issued_at,
            expires_at: wire.expires_at,
            generation: wire.generation,
            actor: wire.actor,
        };
        metadata
            .validate()
            .map(|()| metadata)
            .map_err(serde::de::Error::custom)
    }
}

impl ActionMetadata {
    pub fn new() -> Result<Self, ActionValidationError> {
        Self::with_idempotency_key(format!("action:{}", ActionId::new()), Utc::now())
    }

    pub fn with_idempotency_key(
        idempotency_key: impl Into<String>,
        issued_at: DateTime<Utc>,
    ) -> Result<Self, ActionValidationError> {
        let metadata = Self {
            action_id: ActionId::new(),
            idempotency_key: idempotency_key.into(),
            issued_at,
            expires_at: None,
            generation: 0,
            actor: None,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    #[must_use]
    pub fn with_action_id(mut self, action_id: ActionId) -> Self {
        self.action_id = action_id;
        self
    }

    #[must_use]
    pub fn with_expiry(mut self, expires_at: Option<DateTime<Utc>>) -> Self {
        self.expires_at = expires_at;
        self
    }

    #[must_use]
    pub fn expires_at(self, expires_at: DateTime<Utc>) -> Self {
        self.with_expiry(Some(expires_at))
    }

    #[must_use]
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    #[must_use]
    pub fn with_actor(mut self, actor: PersonId) -> Self {
        self.actor = Some(actor);
        self
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        validate_idempotency_key(&self.idempotency_key)?;
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at <= self.issued_at)
        {
            return Err(ActionValidationError::ExpiryNotAfterIssue);
        }
        Ok(())
    }
}

/// A message addressed to a known Core conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageAction {
    pub conversation_id: ConversationId,
    pub content: MessageContent,
    pub reply_to: Option<MessageId>,
    pub metadata: ActionMetadata,
}

impl<'de> Deserialize<'de> for SendMessageAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            conversation_id: ConversationId,
            content: MessageContent,
            reply_to: Option<MessageId>,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(
            wire.conversation_id,
            wire.content,
            wire.reply_to,
            wire.metadata,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl SendMessageAction {
    pub fn new(
        conversation_id: ConversationId,
        content: MessageContent,
    ) -> Result<Self, ActionValidationError> {
        Self::with_metadata(conversation_id, content, None, ActionMetadata::new()?)
    }

    pub fn with_metadata(
        conversation_id: ConversationId,
        content: MessageContent,
        reply_to: Option<MessageId>,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self {
            conversation_id,
            content,
            reply_to,
            metadata,
        };
        action.validate()?;
        Ok(action)
    }

    pub fn from_text(
        conversation_id: ConversationId,
        text: impl Into<String>,
    ) -> Result<Self, ActionValidationError> {
        Self::new(conversation_id, MessageContent::text(text))
    }

    #[must_use]
    pub fn with_reply_to(mut self, reply_to: Option<MessageId>) -> Self {
        self.reply_to = reply_to;
        self
    }

    #[must_use]
    pub fn with_action_metadata(mut self, metadata: ActionMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()?;
        validate_message(&self.content)
    }

    #[must_use]
    pub const fn action_id(&self) -> ActionId {
        self.metadata.action_id
    }

    #[must_use]
    pub fn idempotency_key(&self) -> &str {
        &self.metadata.idempotency_key
    }

    #[must_use]
    pub const fn issued_at(&self) -> DateTime<Utc> {
        self.metadata.issued_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.metadata.expires_at
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.metadata.generation
    }

    #[must_use]
    pub const fn actor(&self) -> Option<PersonId> {
        self.metadata.actor
    }
}

/// A high-level request to contact a person.  The host resolves the person to
/// a currently deliverable conversation or channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReachOutAction {
    pub person_id: PersonId,
    pub message: MessageContent,
    pub motive: ProactiveMotive,
    pub metadata: ActionMetadata,
}

impl<'de> Deserialize<'de> for ReachOutAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            person_id: PersonId,
            message: MessageContent,
            motive: ProactiveMotive,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(wire.person_id, wire.message, wire.motive, wire.metadata)
            .map_err(serde::de::Error::custom)
    }
}

impl ReachOutAction {
    pub fn new(
        person_id: PersonId,
        message: MessageContent,
        motive: ProactiveMotive,
    ) -> Result<Self, ActionValidationError> {
        Self::with_metadata(person_id, message, motive, ActionMetadata::new()?)
    }

    pub fn from_intent(intent: crate::ReachOutIntent) -> Result<Self, ActionValidationError> {
        let (person_id, message, motive, _) = intent.into_parts();
        Self::new(person_id, message, motive)
    }

    pub fn with_metadata(
        person_id: PersonId,
        message: MessageContent,
        motive: ProactiveMotive,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self {
            person_id,
            message,
            motive,
            metadata,
        };
        action.validate()?;
        Ok(action)
    }

    pub fn from_text(
        person_id: PersonId,
        text: impl Into<String>,
        motive: ProactiveMotive,
    ) -> Result<Self, ActionValidationError> {
        Self::new(person_id, MessageContent::text(text), motive)
    }

    #[must_use]
    pub fn with_action_metadata(mut self, metadata: ActionMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()?;
        validate_message(&self.message)
    }

    #[must_use]
    pub const fn action_id(&self) -> ActionId {
        self.metadata.action_id
    }

    #[must_use]
    pub fn idempotency_key(&self) -> &str {
        &self.metadata.idempotency_key
    }

    #[must_use]
    pub const fn issued_at(&self) -> DateTime<Utc> {
        self.metadata.issued_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.metadata.expires_at
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.metadata.generation
    }

    #[must_use]
    pub const fn actor(&self) -> Option<PersonId> {
        self.metadata.actor
    }
}

macro_rules! metadata_accessors {
    () => {
        #[must_use]
        pub const fn action_id(&self) -> ActionId {
            self.metadata.action_id
        }

        #[must_use]
        pub fn idempotency_key(&self) -> &str {
            &self.metadata.idempotency_key
        }

        #[must_use]
        pub const fn issued_at(&self) -> DateTime<Utc> {
            self.metadata.issued_at
        }

        #[must_use]
        pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
            self.metadata.expires_at
        }

        #[must_use]
        pub const fn generation(&self) -> u64 {
            self.metadata.generation
        }

        #[must_use]
        pub const fn actor(&self) -> Option<PersonId> {
            self.metadata.actor
        }
    };
}

/// A request to invoke a host-exposed tool with an opaque bounded input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAction {
    pub tool_name: String,
    pub input: String,
    pub scope: ActionScope,
    pub metadata: ActionMetadata,
}

impl ToolAction {
    pub fn new(
        tool_name: impl Into<String>,
        input: impl Into<String>,
        scope: ActionScope,
    ) -> Result<Self, ActionValidationError> {
        Self::with_metadata(tool_name, input, scope, ActionMetadata::new()?)
    }

    pub fn with_metadata(
        tool_name: impl Into<String>,
        input: impl Into<String>,
        scope: ActionScope,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self {
            tool_name: tool_name.into(),
            input: input.into(),
            scope,
            metadata,
        };
        action.validate()?;
        Ok(action)
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()?;
        validate_tool_name(&self.tool_name)?;
        validate_tool_input(&self.input)
    }

    metadata_accessors!();
}

impl<'de> Deserialize<'de> for ToolAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            tool_name: String,
            input: String,
            scope: ActionScope,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(wire.tool_name, wire.input, wire.scope, wire.metadata)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOpenLoopAction {
    pub draft: OpenLoopDraft,
    pub metadata: ActionMetadata,
}

impl CreateOpenLoopAction {
    pub fn new(draft: OpenLoopDraft) -> Result<Self, ActionValidationError> {
        Self::with_metadata(draft, ActionMetadata::new()?)
    }

    pub fn with_metadata(
        draft: OpenLoopDraft,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self { draft, metadata };
        action.validate()?;
        Ok(action)
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()?;
        self.draft
            .validate()
            .map_err(ActionValidationError::OpenLoop)
    }

    metadata_accessors!();
}

impl<'de> Deserialize<'de> for CreateOpenLoopAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            draft: OpenLoopDraft,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(wire.draft, wire.metadata).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveOpenLoopAction {
    pub open_loop_id: OpenLoopId,
    pub owner: OpenLoopOwner,
    pub metadata: ActionMetadata,
}

impl ResolveOpenLoopAction {
    pub fn new(
        open_loop_id: OpenLoopId,
        owner: OpenLoopOwner,
    ) -> Result<Self, ActionValidationError> {
        Self::with_metadata(open_loop_id, owner, ActionMetadata::new()?)
    }

    pub fn with_metadata(
        open_loop_id: OpenLoopId,
        owner: OpenLoopOwner,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self {
            open_loop_id,
            owner,
            metadata,
        };
        action.validate()?;
        Ok(action)
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()
    }

    metadata_accessors!();
}

impl<'de> Deserialize<'de> for ResolveOpenLoopAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            open_loop_id: OpenLoopId,
            owner: OpenLoopOwner,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(wire.open_loop_id, wire.owner, wire.metadata)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartGoalAction {
    pub draft: GoalDraft,
    pub metadata: ActionMetadata,
}

impl StartGoalAction {
    pub fn new(draft: GoalDraft) -> Result<Self, ActionValidationError> {
        Self::with_metadata(draft, ActionMetadata::new()?)
    }

    pub fn with_metadata(
        draft: GoalDraft,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self { draft, metadata };
        action.validate()?;
        Ok(action)
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()?;
        self.draft.validate().map_err(ActionValidationError::Goal)
    }

    metadata_accessors!();
}

impl<'de> Deserialize<'de> for StartGoalAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            draft: GoalDraft,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(wire.draft, wire.metadata).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancelGoalAction {
    pub goal_id: GoalId,
    pub owner: GoalOwner,
    pub metadata: ActionMetadata,
}

impl CancelGoalAction {
    pub fn new(goal_id: GoalId, owner: GoalOwner) -> Result<Self, ActionValidationError> {
        Self::with_metadata(goal_id, owner, ActionMetadata::new()?)
    }

    pub fn with_metadata(
        goal_id: GoalId,
        owner: GoalOwner,
        metadata: ActionMetadata,
    ) -> Result<Self, ActionValidationError> {
        let action = Self {
            goal_id,
            owner,
            metadata,
        };
        action.validate()?;
        Ok(action)
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        self.metadata.validate()
    }

    metadata_accessors!();
}

impl<'de> Deserialize<'de> for CancelGoalAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            goal_id: GoalId,
            owner: GoalOwner,
            metadata: ActionMetadata,
        }

        let wire = Wire::deserialize(deserializer)?;
        Self::with_metadata(wire.goal_id, wire.owner, wire.metadata)
            .map_err(serde::de::Error::custom)
    }
}

/// A platform-neutral side effect proposed by Core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ProposedAction {
    SendMessage(SendMessageAction),
    ReachOut(ReachOutAction),
    UseTool(ToolAction),
    CreateOpenLoop(CreateOpenLoopAction),
    ResolveOpenLoop(ResolveOpenLoopAction),
    StartGoal(StartGoalAction),
    CancelGoal(CancelGoalAction),
    Noop,
}

impl ProposedAction {
    pub fn send_message(
        conversation_id: ConversationId,
        content: MessageContent,
    ) -> Result<Self, ActionValidationError> {
        SendMessageAction::new(conversation_id, content).map(Self::SendMessage)
    }

    pub fn reach_out(
        person_id: PersonId,
        message: MessageContent,
        motive: ProactiveMotive,
    ) -> Result<Self, ActionValidationError> {
        ReachOutAction::new(person_id, message, motive).map(Self::ReachOut)
    }

    pub fn use_tool(
        tool_name: impl Into<String>,
        input: impl Into<String>,
        scope: ActionScope,
    ) -> Result<Self, ActionValidationError> {
        ToolAction::new(tool_name, input, scope).map(Self::UseTool)
    }

    pub fn create_open_loop(draft: OpenLoopDraft) -> Result<Self, ActionValidationError> {
        CreateOpenLoopAction::new(draft).map(Self::CreateOpenLoop)
    }

    pub fn resolve_open_loop(
        open_loop_id: OpenLoopId,
        owner: OpenLoopOwner,
    ) -> Result<Self, ActionValidationError> {
        ResolveOpenLoopAction::new(open_loop_id, owner).map(Self::ResolveOpenLoop)
    }

    pub fn start_goal(draft: GoalDraft) -> Result<Self, ActionValidationError> {
        StartGoalAction::new(draft).map(Self::StartGoal)
    }

    pub fn cancel_goal(goal_id: GoalId, owner: GoalOwner) -> Result<Self, ActionValidationError> {
        CancelGoalAction::new(goal_id, owner).map(Self::CancelGoal)
    }

    /// Stamp the trusted actor supplied by the host event onto an action.
    /// Model intents intentionally do not get to choose this value.
    #[must_use]
    pub fn with_actor(mut self, actor: PersonId) -> Self {
        match &mut self {
            Self::SendMessage(action) => action.metadata.actor = Some(actor),
            Self::ReachOut(action) => action.metadata.actor = Some(actor),
            Self::UseTool(action) => action.metadata.actor = Some(actor),
            Self::CreateOpenLoop(action) => action.metadata.actor = Some(actor),
            Self::ResolveOpenLoop(action) => action.metadata.actor = Some(actor),
            Self::StartGoal(action) => action.metadata.actor = Some(actor),
            Self::CancelGoal(action) => action.metadata.actor = Some(actor),
            Self::Noop => {}
        }
        self
    }

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        match self {
            Self::SendMessage(action) => action.validate(),
            Self::ReachOut(action) => action.validate(),
            Self::UseTool(action) => action.validate(),
            Self::CreateOpenLoop(action) => action.validate(),
            Self::ResolveOpenLoop(action) => action.validate(),
            Self::StartGoal(action) => action.validate(),
            Self::CancelGoal(action) => action.validate(),
            Self::Noop => Ok(()),
        }
    }

    #[must_use]
    pub const fn action_id(&self) -> Option<ActionId> {
        match self {
            Self::SendMessage(action) => Some(action.action_id()),
            Self::ReachOut(action) => Some(action.action_id()),
            Self::UseTool(action) => Some(action.action_id()),
            Self::CreateOpenLoop(action) => Some(action.action_id()),
            Self::ResolveOpenLoop(action) => Some(action.action_id()),
            Self::StartGoal(action) => Some(action.action_id()),
            Self::CancelGoal(action) => Some(action.action_id()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub fn idempotency_key(&self) -> Option<&str> {
        match self {
            Self::SendMessage(action) => Some(action.idempotency_key()),
            Self::ReachOut(action) => Some(action.idempotency_key()),
            Self::UseTool(action) => Some(action.idempotency_key()),
            Self::CreateOpenLoop(action) => Some(action.idempotency_key()),
            Self::ResolveOpenLoop(action) => Some(action.idempotency_key()),
            Self::StartGoal(action) => Some(action.idempotency_key()),
            Self::CancelGoal(action) => Some(action.idempotency_key()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn issued_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::SendMessage(action) => Some(action.issued_at()),
            Self::ReachOut(action) => Some(action.issued_at()),
            Self::UseTool(action) => Some(action.issued_at()),
            Self::CreateOpenLoop(action) => Some(action.issued_at()),
            Self::ResolveOpenLoop(action) => Some(action.issued_at()),
            Self::StartGoal(action) => Some(action.issued_at()),
            Self::CancelGoal(action) => Some(action.issued_at()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::SendMessage(action) => action.expires_at(),
            Self::ReachOut(action) => action.expires_at(),
            Self::UseTool(action) => action.expires_at(),
            Self::CreateOpenLoop(action) => action.expires_at(),
            Self::ResolveOpenLoop(action) => action.expires_at(),
            Self::StartGoal(action) => action.expires_at(),
            Self::CancelGoal(action) => action.expires_at(),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        match self {
            Self::SendMessage(action) => Some(action.generation()),
            Self::ReachOut(action) => Some(action.generation()),
            Self::UseTool(action) => Some(action.generation()),
            Self::CreateOpenLoop(action) => Some(action.generation()),
            Self::ResolveOpenLoop(action) => Some(action.generation()),
            Self::StartGoal(action) => Some(action.generation()),
            Self::CancelGoal(action) => Some(action.generation()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn actor(&self) -> Option<PersonId> {
        match self {
            Self::SendMessage(action) => action.actor(),
            Self::ReachOut(action) => action.actor(),
            Self::UseTool(action) => action.actor(),
            Self::CreateOpenLoop(action) => action.actor(),
            Self::ResolveOpenLoop(action) => action.actor(),
            Self::StartGoal(action) => action.actor(),
            Self::CancelGoal(action) => action.actor(),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn scope(&self) -> ActionScope {
        match self {
            Self::SendMessage(action) => ActionScope::Conversation(action.conversation_id),
            Self::ReachOut(action) => ActionScope::Person(action.person_id),
            Self::UseTool(action) => action.scope,
            Self::CreateOpenLoop(action) => ActionScope::for_open_loop_owner(action.draft.owner()),
            Self::ResolveOpenLoop(action) => ActionScope::for_open_loop_owner(action.owner),
            Self::StartGoal(action) => ActionScope::for_goal_owner(action.draft.owner()),
            Self::CancelGoal(action) => ActionScope::for_goal_owner(action.owner),
            Self::Noop => ActionScope::Global,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ActionValidationError {
    #[error("action message must not be empty")]
    EmptyMessage,
    #[error("action message must not contain NUL")]
    MessageContainsNul,
    #[error("action message is {length} bytes, above maximum {maximum}")]
    MessageTooLong { length: usize, maximum: usize },
    #[error("action message is {length} characters, above maximum {maximum}")]
    MessageTooManyCharacters { length: usize, maximum: usize },
    #[error("action message has {length} attachments, above maximum {maximum}")]
    TooManyAttachments { length: usize, maximum: usize },
    #[error("action message content is invalid: {0}")]
    InvalidMessageContent(MessageValidationError),
    #[error("action idempotency key must not be empty")]
    EmptyIdempotencyKey,
    #[error("action idempotency key is {length} bytes, above maximum {maximum}")]
    IdempotencyKeyTooLong { length: usize, maximum: usize },
    #[error("action idempotency key is {length} characters, above maximum {maximum}")]
    IdempotencyKeyTooManyCharacters { length: usize, maximum: usize },
    #[error("action expiry must be after its issue time")]
    ExpiryNotAfterIssue,
    #[error("tool name must not be empty")]
    EmptyToolName,
    #[error("tool name contains NUL")]
    ToolNameContainsNul,
    #[error("tool name is {length} bytes, above maximum {maximum}")]
    ToolNameTooLong { length: usize, maximum: usize },
    #[error("tool name is {length} characters, above maximum {maximum}")]
    ToolNameTooManyCharacters { length: usize, maximum: usize },
    #[error("tool input contains NUL")]
    ToolInputContainsNul,
    #[error("tool input is {length} bytes, above maximum {maximum}")]
    ToolInputTooLong { length: usize, maximum: usize },
    #[error("tool input is {length} characters, above maximum {maximum}")]
    ToolInputTooManyCharacters { length: usize, maximum: usize },
    #[error(transparent)]
    OpenLoop(#[from] OpenLoopValidationError),
    #[error(transparent)]
    Goal(#[from] GoalValidationError),
}

fn validate_message(message: &MessageContent) -> Result<(), ActionValidationError> {
    message.validate().map_err(|error| match error {
        MessageValidationError::TextContainsNul => ActionValidationError::MessageContainsNul,
        MessageValidationError::TextTooLong { length, maximum } => {
            ActionValidationError::MessageTooLong { length, maximum }
        }
        MessageValidationError::TextTooManyCharacters { length, maximum } => {
            ActionValidationError::MessageTooManyCharacters { length, maximum }
        }
        MessageValidationError::TooManyAttachments { length, maximum } => {
            ActionValidationError::TooManyAttachments { length, maximum }
        }
        error => ActionValidationError::InvalidMessageContent(error),
    })?;
    if message.is_empty() {
        return Err(ActionValidationError::EmptyMessage);
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), ActionValidationError> {
    if value.trim().is_empty() {
        return Err(ActionValidationError::EmptyToolName);
    }
    if value.contains('\0') {
        return Err(ActionValidationError::ToolNameContainsNul);
    }
    if value.len() > MAX_TOOL_NAME_BYTES {
        return Err(ActionValidationError::ToolNameTooLong {
            length: value.len(),
            maximum: MAX_TOOL_NAME_BYTES,
        });
    }
    let chars = value.chars().count();
    if chars > MAX_TOOL_NAME_CHARS {
        return Err(ActionValidationError::ToolNameTooManyCharacters {
            length: chars,
            maximum: MAX_TOOL_NAME_CHARS,
        });
    }
    Ok(())
}

fn validate_tool_input(value: &str) -> Result<(), ActionValidationError> {
    if value.contains('\0') {
        return Err(ActionValidationError::ToolInputContainsNul);
    }
    if value.len() > MAX_TOOL_INPUT_BYTES {
        return Err(ActionValidationError::ToolInputTooLong {
            length: value.len(),
            maximum: MAX_TOOL_INPUT_BYTES,
        });
    }
    let chars = value.chars().count();
    if chars > MAX_TOOL_INPUT_CHARS {
        return Err(ActionValidationError::ToolInputTooManyCharacters {
            length: chars,
            maximum: MAX_TOOL_INPUT_CHARS,
        });
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<(), ActionValidationError> {
    if value.trim().is_empty() {
        return Err(ActionValidationError::EmptyIdempotencyKey);
    }
    if value.len() > MAX_ACTION_IDEMPOTENCY_KEY_BYTES {
        return Err(ActionValidationError::IdempotencyKeyTooLong {
            length: value.len(),
            maximum: MAX_ACTION_IDEMPOTENCY_KEY_BYTES,
        });
    }
    let chars = value.chars().count();
    if chars > MAX_ACTION_IDEMPOTENCY_KEY_CHARS {
        return Err(ActionValidationError::IdempotencyKeyTooManyCharacters {
            length: chars,
            maximum: MAX_ACTION_IDEMPOTENCY_KEY_CHARS,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::ConversationId;
    use crate::{GoalKind, OpenLoopKind};
    use chrono::Duration;

    #[test]
    fn actions_reject_invalid_content_and_metadata() {
        let conversation = ConversationId::new();
        assert_eq!(
            SendMessageAction::new(conversation, MessageContent::text("  "))
                .expect_err("blank action should fail"),
            ActionValidationError::EmptyMessage
        );
        assert_eq!(
            ReachOutAction::new(
                PersonId::new(),
                MessageContent::text("bad\0message"),
                ProactiveMotive::CheckIn,
            )
            .expect_err("NUL action should fail"),
            ActionValidationError::MessageContainsNul
        );

        let now = Utc::now();
        let metadata = ActionMetadata::with_idempotency_key("same", now)
            .expect("valid metadata")
            .with_expiry(Some(now - Duration::seconds(1)));
        assert_eq!(
            SendMessageAction::with_metadata(
                conversation,
                MessageContent::text("hello"),
                None,
                metadata,
            )
            .expect_err("backwards expiry should fail"),
            ActionValidationError::ExpiryNotAfterIssue
        );
    }

    #[test]
    fn proposed_action_serializes_with_stable_variant_names() {
        let action = ProposedAction::reach_out(
            PersonId::new(),
            MessageContent::text("hello"),
            ProactiveMotive::Curiosity,
        )
        .expect("valid action");
        let encoded = serde_json::to_string(&action).expect("serialize action");
        assert!(encoded.contains("reach_out"));
        let decoded: ProposedAction = serde_json::from_str(&encoded).expect("decode action");
        assert_eq!(decoded, action);
    }

    #[test]
    fn action_deserialization_cannot_bypass_validation() {
        let action =
            ProposedAction::send_message(ConversationId::new(), MessageContent::text("hello"))
                .expect("valid action");
        let mut encoded = serde_json::to_value(&action).expect("serialize action");
        encoded["payload"]["content"]["text"] = serde_json::json!("   ");
        assert!(serde_json::from_value::<ProposedAction>(encoded).is_err());

        let mut encoded = serde_json::to_value(action).expect("serialize action");
        encoded["payload"]["metadata"]["idempotency_key"] = serde_json::json!("");
        assert!(serde_json::from_value::<ProposedAction>(encoded).is_err());
    }

    #[test]
    fn management_and_tool_actions_have_validated_scopes_and_stable_serde() {
        let person_id = PersonId::new();
        let conversation_id = ConversationId::new();
        let open_loop = OpenLoopDraft::new(
            OpenLoopOwner::Conversation(conversation_id),
            OpenLoopKind::FollowUp,
            "ask again",
        )
        .expect("open-loop draft");
        let goal = GoalDraft::new(
            GoalOwner::Person(person_id),
            GoalKind::Personal,
            "learn Rust",
        )
        .expect("goal draft");
        let actions = vec![
            ProposedAction::use_tool(
                "weather.current",
                r#"{"city":"Shanghai"}"#,
                ActionScope::Global,
            )
            .expect("tool action"),
            ProposedAction::create_open_loop(open_loop).expect("create open loop"),
            ProposedAction::resolve_open_loop(
                OpenLoopId::new(),
                OpenLoopOwner::Conversation(conversation_id),
            )
            .expect("resolve open loop"),
            ProposedAction::start_goal(goal).expect("start goal"),
            ProposedAction::cancel_goal(GoalId::new(), GoalOwner::Person(person_id))
                .expect("cancel goal"),
        ];

        assert_eq!(actions[0].scope(), ActionScope::Global);
        assert_eq!(
            actions[1].scope(),
            ActionScope::Conversation(conversation_id)
        );
        assert_eq!(actions[3].scope(), ActionScope::Person(person_id));
        for action in actions {
            let encoded = serde_json::to_string(&action).expect("serialize action");
            assert_eq!(
                serde_json::from_str::<ProposedAction>(&encoded).expect("deserialize action"),
                action
            );
        }

        assert_eq!(
            ProposedAction::use_tool(" ", "{}", ActionScope::Global).expect_err("blank tool name"),
            ActionValidationError::EmptyToolName
        );
        assert_eq!(
            ProposedAction::use_tool(
                "weather.current",
                "x".repeat(MAX_TOOL_INPUT_BYTES + 1),
                ActionScope::Global,
            )
            .expect_err("bounded tool input"),
            ActionValidationError::ToolInputTooLong {
                length: MAX_TOOL_INPUT_BYTES + 1,
                maximum: MAX_TOOL_INPUT_BYTES,
            }
        );
    }
}
