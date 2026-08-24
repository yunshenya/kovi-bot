//! Platform-neutral actions proposed by the cognitive core.
//!
//! Actions deliberately describe intent in terms of Core identifiers.  An
//! adapter is responsible for translating these values into a concrete
//! platform operation after the action arbiter has admitted the action.

use crate::MessageContent;
use crate::event::{MAX_MESSAGE_CONTENT_BYTES, MAX_MESSAGE_CONTENT_CHARS};
use crate::identity::{ConversationId, MessageId, PersonId};
use crate::proactive::ProactiveMotive;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

pub const MAX_ACTION_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_ACTION_IDEMPOTENCY_KEY_CHARS: usize = 128;

/// The Core scope used for authorization and per-target cooldowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum ActionScope {
    Conversation(ConversationId),
    Person(PersonId),
    Global,
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

/// A platform-neutral side effect proposed by Core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ProposedAction {
    SendMessage(SendMessageAction),
    ReachOut(ReachOutAction),
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

    pub fn validate(&self) -> Result<(), ActionValidationError> {
        match self {
            Self::SendMessage(action) => action.validate(),
            Self::ReachOut(action) => action.validate(),
            Self::Noop => Ok(()),
        }
    }

    #[must_use]
    pub const fn action_id(&self) -> Option<ActionId> {
        match self {
            Self::SendMessage(action) => Some(action.action_id()),
            Self::ReachOut(action) => Some(action.action_id()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub fn idempotency_key(&self) -> Option<&str> {
        match self {
            Self::SendMessage(action) => Some(action.idempotency_key()),
            Self::ReachOut(action) => Some(action.idempotency_key()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn issued_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::SendMessage(action) => Some(action.issued_at()),
            Self::ReachOut(action) => Some(action.issued_at()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::SendMessage(action) => action.expires_at(),
            Self::ReachOut(action) => action.expires_at(),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        match self {
            Self::SendMessage(action) => Some(action.generation()),
            Self::ReachOut(action) => Some(action.generation()),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn actor(&self) -> Option<PersonId> {
        match self {
            Self::SendMessage(action) => action.actor(),
            Self::ReachOut(action) => action.actor(),
            Self::Noop => None,
        }
    }

    #[must_use]
    pub const fn scope(&self) -> ActionScope {
        match self {
            Self::SendMessage(action) => ActionScope::Conversation(action.conversation_id),
            Self::ReachOut(action) => ActionScope::Person(action.person_id),
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
    #[error("action idempotency key must not be empty")]
    EmptyIdempotencyKey,
    #[error("action idempotency key is {length} bytes, above maximum {maximum}")]
    IdempotencyKeyTooLong { length: usize, maximum: usize },
    #[error("action idempotency key is {length} characters, above maximum {maximum}")]
    IdempotencyKeyTooManyCharacters { length: usize, maximum: usize },
    #[error("action expiry must be after its issue time")]
    ExpiryNotAfterIssue,
}

fn validate_message(message: &MessageContent) -> Result<(), ActionValidationError> {
    let text = message.as_text();
    if text.trim().is_empty() {
        return Err(ActionValidationError::EmptyMessage);
    }
    if text.contains('\0') {
        return Err(ActionValidationError::MessageContainsNul);
    }
    if text.len() > MAX_MESSAGE_CONTENT_BYTES {
        return Err(ActionValidationError::MessageTooLong {
            length: text.len(),
            maximum: MAX_MESSAGE_CONTENT_BYTES,
        });
    }
    let chars = text.chars().count();
    if chars > MAX_MESSAGE_CONTENT_CHARS {
        return Err(ActionValidationError::MessageTooManyCharacters {
            length: chars,
            maximum: MAX_MESSAGE_CONTENT_CHARS,
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
}
