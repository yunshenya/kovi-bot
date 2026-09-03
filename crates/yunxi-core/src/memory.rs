//! Platform-neutral long-term memory records and persistence port.

use crate::identity::{ConversationId, MemoryId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

pub const MAX_MEMORY_CONTENT_BYTES: usize = 8 * 1_024;
pub const MAX_MEMORY_CONTENT_CHARS: usize = 4_096;
pub const MAX_MEMORY_QUERY_BYTES: usize = 512;
pub const MAX_MEMORY_TAGS: usize = 16;
pub const MAX_MEMORY_TAG_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Person(PersonId),
    Conversation(ConversationId),
    Global,
}

impl MemoryScope {
    #[must_use]
    pub const fn person_id(self) -> Option<PersonId> {
        match self {
            Self::Person(id) => Some(id),
            Self::Conversation(_) | Self::Global => None,
        }
    }

    #[must_use]
    pub const fn conversation_id(self) -> Option<ConversationId> {
        match self {
            Self::Conversation(id) => Some(id),
            Self::Person(_) | Self::Global => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Conversation,
    Profile,
    Event,
    Preference,
    Emotion,
    Fact,
}

impl MemoryKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Profile => "profile",
            Self::Event => "event",
            Self::Preference => "preference",
            Self::Emotion => "emotion",
            Self::Fact => "fact",
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MemoryKind {
    type Err = MemoryValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "conversation" => Ok(Self::Conversation),
            "profile" => Ok(Self::Profile),
            "event" => Ok(Self::Event),
            "preference" => Ok(Self::Preference),
            "emotion" => Ok(Self::Emotion),
            "fact" => Ok(Self::Fact),
            _ => Err(Self::Err::UnknownKind {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MemoryValidationError {
    #[error("memory content must not be empty")]
    EmptyContent,
    #[error("memory content is {length} bytes, above maximum {maximum}")]
    ContentTooLong { length: usize, maximum: usize },
    #[error("memory content is {length} characters, above maximum {maximum}")]
    ContentTooManyCharacters { length: usize, maximum: usize },
    #[error("memory content must not contain NUL")]
    ContentContainsNul,
    #[error("memory importance must be between 0 and 100")]
    ImportanceOutOfRange,
    #[error("memory query is {length} bytes, above maximum {maximum}")]
    QueryTooLong { length: usize, maximum: usize },
    #[error("memory query must not contain NUL")]
    QueryContainsNul,
    #[error("memory tag is empty")]
    EmptyTag,
    #[error("memory tag is {length} bytes, above maximum {maximum}")]
    TagTooLong { length: usize, maximum: usize },
    #[error("memory tag must not contain NUL")]
    TagContainsNul,
    #[error("memory has too many tags")]
    TooManyTags,
    #[error("unknown memory kind `{value}`")]
    UnknownKind { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryDraft {
    scope: MemoryScope,
    kind: MemoryKind,
    content: String,
    importance: u8,
    tags: Vec<String>,
    occurred_at: DateTime<Utc>,
}

impl MemoryDraft {
    pub fn new(
        scope: MemoryScope,
        kind: MemoryKind,
        content: impl Into<String>,
        occurred_at: DateTime<Utc>,
    ) -> Result<Self, MemoryValidationError> {
        Ok(Self {
            scope,
            kind,
            content: validate_content(content.into())?,
            importance: 50,
            tags: Vec::new(),
            occurred_at,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> MemoryScope {
        self.scope
    }

    #[must_use]
    pub const fn kind(&self) -> MemoryKind {
        self.kind
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub const fn importance(&self) -> u8 {
        self.importance
    }

    #[must_use]
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    #[must_use]
    pub const fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    pub fn with_importance(mut self, importance: u8) -> Result<Self, MemoryValidationError> {
        if importance > 100 {
            return Err(MemoryValidationError::ImportanceOutOfRange);
        }
        self.importance = importance;
        Ok(self)
    }

    pub fn with_tags<I, S>(mut self, tags: I) -> Result<Self, MemoryValidationError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = validate_tags(tags)?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), MemoryValidationError> {
        validate_content(self.content.clone())?;
        if self.importance > 100 {
            return Err(MemoryValidationError::ImportanceOutOfRange);
        }
        let _ = validate_tags(self.tags.clone())?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Memory {
    id: MemoryId,
    scope: MemoryScope,
    kind: MemoryKind,
    content: String,
    importance: u8,
    tags: Vec<String>,
    occurred_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
}

impl Memory {
    pub fn from_draft(
        id: MemoryId,
        draft: &MemoryDraft,
        created_at: DateTime<Utc>,
    ) -> Result<Self, MemoryValidationError> {
        draft.validate()?;
        Ok(Self {
            id,
            scope: draft.scope,
            kind: draft.kind,
            content: draft.content.clone(),
            importance: draft.importance,
            tags: draft.tags.clone(),
            occurred_at: draft.occurred_at,
            created_at,
        })
    }

    #[must_use]
    pub const fn id(&self) -> MemoryId {
        self.id
    }

    #[must_use]
    pub const fn scope(&self) -> MemoryScope {
        self.scope
    }

    #[must_use]
    pub const fn kind(&self) -> MemoryKind {
        self.kind
    }

    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    #[must_use]
    pub const fn importance(&self) -> u8 {
        self.importance
    }

    #[must_use]
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    #[must_use]
    pub const fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    #[must_use]
    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

impl<'de> Deserialize<'de> for Memory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: MemoryId,
            scope: MemoryScope,
            kind: MemoryKind,
            content: String,
            importance: u8,
            tags: Vec<String>,
            occurred_at: DateTime<Utc>,
            created_at: DateTime<Utc>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let draft = MemoryDraft::new(wire.scope, wire.kind, wire.content, wire.occurred_at)
            .map_err(serde::de::Error::custom)?
            .with_importance(wire.importance)
            .and_then(|draft| draft.with_tags(wire.tags))
            .map_err(serde::de::Error::custom)?;
        Self::from_draft(wire.id, &draft, wire.created_at).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryQuery {
    scope: MemoryScope,
    text: String,
    min_importance: Option<u8>,
    limit: usize,
}

impl MemoryQuery {
    pub fn new(
        scope: MemoryScope,
        text: impl Into<String>,
        limit: usize,
    ) -> Result<Self, MemoryValidationError> {
        let text = validate_query(text.into())?;
        Ok(Self {
            scope,
            text,
            min_importance: None,
            limit: limit.clamp(1, 32),
        })
    }

    #[must_use]
    pub const fn scope(&self) -> MemoryScope {
        self.scope
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn min_importance(&self) -> Option<u8> {
        self.min_importance
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    pub fn with_min_importance(mut self, importance: u8) -> Result<Self, MemoryValidationError> {
        if importance > 100 {
            return Err(MemoryValidationError::ImportanceOutOfRange);
        }
        self.min_importance = Some(importance);
        Ok(self)
    }
}

impl<'de> Deserialize<'de> for MemoryQuery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            scope: MemoryScope,
            text: String,
            min_importance: Option<u8>,
            limit: usize,
        }
        let wire = Wire::deserialize(deserializer)?;
        let query = MemoryQuery::new(wire.scope, wire.text, wire.limit)
            .map_err(serde::de::Error::custom)?;
        wire.min_importance
            .map_or(Ok(query.clone()), |importance| {
                query.with_min_importance(importance)
            })
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for MemoryDraft {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            scope: MemoryScope,
            kind: MemoryKind,
            content: String,
            importance: u8,
            tags: Vec<String>,
            occurred_at: DateTime<Utc>,
        }
        let wire = Wire::deserialize(deserializer)?;
        MemoryDraft::new(wire.scope, wire.kind, wire.content, wire.occurred_at)
            .map_err(serde::de::Error::custom)?
            .with_importance(wire.importance)
            .and_then(|draft| draft.with_tags(wire.tags))
            .map_err(serde::de::Error::custom)
    }
}

fn validate_content(value: String) -> Result<String, MemoryValidationError> {
    if value.trim().is_empty() {
        return Err(MemoryValidationError::EmptyContent);
    }
    if value.contains('\0') {
        return Err(MemoryValidationError::ContentContainsNul);
    }
    if value.len() > MAX_MEMORY_CONTENT_BYTES {
        return Err(MemoryValidationError::ContentTooLong {
            length: value.len(),
            maximum: MAX_MEMORY_CONTENT_BYTES,
        });
    }
    let chars = value.chars().count();
    if chars > MAX_MEMORY_CONTENT_CHARS {
        return Err(MemoryValidationError::ContentTooManyCharacters {
            length: chars,
            maximum: MAX_MEMORY_CONTENT_CHARS,
        });
    }
    Ok(value)
}

fn validate_query(value: String) -> Result<String, MemoryValidationError> {
    if value.contains('\0') {
        return Err(MemoryValidationError::QueryContainsNul);
    }
    if value.len() > MAX_MEMORY_QUERY_BYTES {
        return Err(MemoryValidationError::QueryTooLong {
            length: value.len(),
            maximum: MAX_MEMORY_QUERY_BYTES,
        });
    }
    Ok(value)
}

fn validate_tags<I, S>(tags: I) -> Result<Vec<String>, MemoryValidationError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut result = Vec::new();
    for raw in tags {
        let tag = raw.into();
        if tag.trim().is_empty() {
            return Err(MemoryValidationError::EmptyTag);
        }
        if tag.contains('\0') {
            return Err(MemoryValidationError::TagContainsNul);
        }
        if tag.len() > MAX_MEMORY_TAG_BYTES {
            return Err(MemoryValidationError::TagTooLong {
                length: tag.len(),
                maximum: MAX_MEMORY_TAG_BYTES,
            });
        }
        if !result.contains(&tag) {
            result.push(tag);
        }
        if result.len() > MAX_MEMORY_TAGS {
            return Err(MemoryValidationError::TooManyTags);
        }
    }
    Ok(result)
}

/// Build a bounded, durable "world fact" memory draft. This is the
/// platform-neutral way for any host (QQ, desktop, CLI) to teach the core a
/// durable, retrievable fact about the owner's world (a project, a task, a
/// build state, a recurring thing) that the core recalls and injects like any
/// other memory, rather than only being derived from chat. Content is bounded
/// and importance is clamped.
pub fn world_fact_draft(
    scope: MemoryScope,
    summary: &str,
    importance: u8,
    occurred_at: DateTime<Utc>,
) -> Result<MemoryDraft, MemoryValidationError> {
    let draft = MemoryDraft::new(scope, MemoryKind::Fact, summary, occurred_at)?;
    draft.with_importance(importance.clamp(0, 100))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn scopes_are_platform_neutral_and_records_are_bounded() {
        let draft = MemoryDraft::new(
            MemoryScope::Person(PersonId::new()),
            MemoryKind::Preference,
            "likes tea",
            Utc::now(),
        )
        .expect("valid draft")
        .with_tags(["drink", "drink"])
        .expect("valid tags");
        assert_eq!(draft.tags().len(), 1);
        let record = Memory::from_draft(MemoryId::new(), &draft, Utc::now()).expect("record");
        assert_eq!(record.scope(), draft.scope());
    }

    #[test]
    fn serde_cannot_bypass_content_validation() {
        let encoded = format!(
            "{{\"scope\":{{\"global\":null}},\"kind\":\"fact\",\"content\":\"{}\",\"importance\":50,\"tags\":[],\"occurred_at\":\"{}\"}}",
            "x".repeat(MAX_MEMORY_CONTENT_CHARS + 1),
            Utc::now().to_rfc3339()
        );
        assert!(serde_json::from_str::<MemoryDraft>(&encoded).is_err());
    }

    #[test]
    fn world_fact_draft_is_bounded_and_clamps_importance() {
        let draft = world_fact_draft(
            MemoryScope::Global,
            "kovi-bot 构建在 main 上通过",
            200,
            Utc::now(),
        )
        .expect("world fact draft is valid");
        assert_eq!(draft.kind(), MemoryKind::Fact);
        assert_eq!(draft.importance(), 100);
        assert_eq!(draft.scope(), MemoryScope::Global);
        // An over-long summary is rejected at the boundary.
        let too_long = "x".repeat(MAX_MEMORY_CONTENT_CHARS + 1);
        assert!(world_fact_draft(MemoryScope::Global, &too_long, 80, Utc::now()).is_err());
    }
}
