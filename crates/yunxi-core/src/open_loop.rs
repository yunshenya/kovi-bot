//! Platform-neutral prospective memory ("open loop") domain types.
//!
//! An open loop is an internal future attention point.  It is deliberately
//! separate from reminders: a reminder is a user-mandated delivery task,
//! while an open loop only creates a future observation for the Core.

use crate::identity::{ConversationId, MessageId, OpenLoopId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

pub const MAX_OPEN_LOOP_SUMMARY_BYTES: usize = 4 * 1_024;
pub const MAX_OPEN_LOOP_SUMMARY_CHARS: usize = 1_024;
pub const MAX_OPEN_LOOP_DEDUPE_KEY_BYTES: usize = 512;
pub const MAX_OPEN_LOOP_SALIENCE: u8 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenLoopOwner {
    Person(PersonId),
    Conversation(ConversationId),
    Global,
}

impl OpenLoopOwner {
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

    #[must_use]
    pub const fn is_global(self) -> bool {
        matches!(self, Self::Global)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenLoopKind {
    FollowUp,
    AwaitingOutcome,
    FutureEvent,
    Promise,
    PendingQuestion,
}

impl OpenLoopKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FollowUp => "follow_up",
            Self::AwaitingOutcome => "awaiting_outcome",
            Self::FutureEvent => "future_event",
            Self::Promise => "promise",
            Self::PendingQuestion => "pending_question",
        }
    }
}

impl fmt::Display for OpenLoopKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for OpenLoopKind {
    type Err = OpenLoopValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "follow_up" => Ok(Self::FollowUp),
            "awaiting_outcome" => Ok(Self::AwaitingOutcome),
            "future_event" => Ok(Self::FutureEvent),
            "promise" => Ok(Self::Promise),
            "pending_question" => Ok(Self::PendingQuestion),
            _ => Err(Self::Err::UnknownKind {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenLoopStatus {
    Open,
    Triggered,
    Resolved,
    Expired,
    Cancelled,
}

impl OpenLoopStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Triggered => "triggered",
            Self::Resolved => "resolved",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Expired | Self::Cancelled)
    }
}

impl fmt::Display for OpenLoopStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for OpenLoopStatus {
    type Err = OpenLoopValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "open" => Ok(Self::Open),
            "triggered" => Ok(Self::Triggered),
            "resolved" => Ok(Self::Resolved),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Self::Err::UnknownStatus {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OpenLoopValidationError {
    #[error("open-loop summary must not be empty")]
    EmptySummary,
    #[error("open-loop summary is {length} bytes, above maximum {maximum}")]
    SummaryTooLong { length: usize, maximum: usize },
    #[error("open-loop summary is {length} characters, above maximum {maximum}")]
    SummaryTooManyCharacters { length: usize, maximum: usize },
    #[error("open-loop summary must not contain NUL")]
    SummaryContainsNul,
    #[error("open-loop dedupe key is {length} bytes, above maximum {maximum}")]
    DedupeKeyTooLong { length: usize, maximum: usize },
    #[error("open-loop dedupe key must not contain NUL")]
    DedupeKeyContainsNul,
    #[error("open-loop dedupe key must not be empty")]
    EmptyDedupeKey,
    #[error("open-loop salience {value} is above maximum {maximum}")]
    SalienceTooLarge { value: u8, maximum: u8 },
    #[error("open-loop expiry must be at or after its due time")]
    ExpiryBeforeDue,
    #[error("unknown open-loop kind `{value}`")]
    UnknownKind { value: String },
    #[error("unknown open-loop status `{value}`")]
    UnknownStatus { value: String },
    #[error("invalid open-loop status transition from {from} to {to}")]
    InvalidTransition {
        from: OpenLoopStatus,
        to: OpenLoopStatus,
    },
    #[error("open-loop persisted state is inconsistent for status {status}: {reason}")]
    InvalidPersistedState {
        status: OpenLoopStatus,
        reason: &'static str,
    },
    #[error("open-loop version exhausted")]
    VersionExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenLoopDraft {
    owner: OpenLoopOwner,
    kind: OpenLoopKind,
    summary: String,
    source_message_id: Option<MessageId>,
    due_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    salience: u8,
    dedupe_key: Option<String>,
}

impl OpenLoopDraft {
    pub fn new(
        owner: OpenLoopOwner,
        kind: OpenLoopKind,
        summary: impl Into<String>,
    ) -> Result<Self, OpenLoopValidationError> {
        let summary = validate_summary(summary.into())?;
        Ok(Self {
            owner,
            kind,
            summary,
            source_message_id: None,
            due_at: None,
            expires_at: None,
            salience: 50,
            dedupe_key: None,
        })
    }

    #[must_use]
    pub const fn owner(&self) -> OpenLoopOwner {
        self.owner
    }

    #[must_use]
    pub const fn kind(&self) -> OpenLoopKind {
        self.kind
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub const fn source_message_id(&self) -> Option<MessageId> {
        self.source_message_id
    }

    #[must_use]
    pub const fn due_at(&self) -> Option<DateTime<Utc>> {
        self.due_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn salience(&self) -> u8 {
        self.salience
    }

    #[must_use]
    pub fn dedupe_key(&self) -> Option<&str> {
        self.dedupe_key.as_deref()
    }

    #[must_use]
    pub fn with_source_message_id(mut self, source_message_id: Option<MessageId>) -> Self {
        self.source_message_id = source_message_id;
        self
    }

    #[must_use]
    pub fn with_due_at(mut self, due_at: Option<DateTime<Utc>>) -> Self {
        self.due_at = due_at;
        self
    }

    #[must_use]
    pub fn with_expires_at(mut self, expires_at: Option<DateTime<Utc>>) -> Self {
        self.expires_at = expires_at;
        self
    }

    pub fn with_salience(mut self, salience: u8) -> Result<Self, OpenLoopValidationError> {
        if salience > MAX_OPEN_LOOP_SALIENCE {
            return Err(OpenLoopValidationError::SalienceTooLarge {
                value: salience,
                maximum: MAX_OPEN_LOOP_SALIENCE,
            });
        }
        self.salience = salience;
        Ok(self)
    }

    pub fn with_dedupe_key(
        mut self,
        dedupe_key: Option<String>,
    ) -> Result<Self, OpenLoopValidationError> {
        self.dedupe_key = dedupe_key.map(validate_dedupe_key).transpose()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), OpenLoopValidationError> {
        let _ = validate_summary(self.summary.clone())?;
        if let Some(key) = &self.dedupe_key {
            validate_dedupe_key(key.clone())?;
        }
        if self.salience > MAX_OPEN_LOOP_SALIENCE {
            return Err(OpenLoopValidationError::SalienceTooLarge {
                value: self.salience,
                maximum: MAX_OPEN_LOOP_SALIENCE,
            });
        }
        if let (Some(due_at), Some(expires_at)) = (self.due_at, self.expires_at)
            && expires_at < due_at
        {
            return Err(OpenLoopValidationError::ExpiryBeforeDue);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenLoop {
    id: OpenLoopId,
    owner: OpenLoopOwner,
    kind: OpenLoopKind,
    summary: String,
    source_message_id: Option<MessageId>,
    due_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    salience: u8,
    status: OpenLoopStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    triggered_at: Option<DateTime<Utc>>,
    version: u64,
    dedupe_key: Option<String>,
}

impl OpenLoop {
    /// Creates a new open item with the default draft options.
    pub fn new(
        id: OpenLoopId,
        owner: OpenLoopOwner,
        kind: OpenLoopKind,
        summary: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Self, OpenLoopValidationError> {
        Self::from_draft(id, &OpenLoopDraft::new(owner, kind, summary)?, now)
    }

    pub fn from_draft(
        id: OpenLoopId,
        draft: &OpenLoopDraft,
        now: DateTime<Utc>,
    ) -> Result<Self, OpenLoopValidationError> {
        draft.validate()?;
        Ok(Self {
            id,
            owner: draft.owner,
            kind: draft.kind,
            summary: draft.summary.clone(),
            source_message_id: draft.source_message_id,
            due_at: draft.due_at,
            expires_at: draft.expires_at,
            salience: draft.salience,
            status: OpenLoopStatus::Open,
            created_at: now,
            updated_at: now,
            resolved_at: None,
            triggered_at: None,
            version: 0,
            dedupe_key: draft.dedupe_key.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn restore(
        id: OpenLoopId,
        owner: OpenLoopOwner,
        kind: OpenLoopKind,
        summary: impl Into<String>,
        source_message_id: Option<MessageId>,
        due_at: Option<DateTime<Utc>>,
        expires_at: Option<DateTime<Utc>>,
        salience: u8,
        status: OpenLoopStatus,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        resolved_at: Option<DateTime<Utc>>,
        triggered_at: Option<DateTime<Utc>>,
        version: u64,
        dedupe_key: Option<String>,
    ) -> Result<Self, OpenLoopValidationError> {
        let draft = OpenLoopDraft::new(owner, kind, summary)?
            .with_source_message_id(source_message_id)
            .with_due_at(due_at)
            .with_expires_at(expires_at)
            .with_dedupe_key(dedupe_key)?
            .with_salience(salience)?;
        match status {
            OpenLoopStatus::Open if resolved_at.is_some() || triggered_at.is_some() => {
                return Err(OpenLoopValidationError::InvalidPersistedState {
                    status,
                    reason: "open loops cannot have terminal or lease timestamps",
                });
            }
            OpenLoopStatus::Triggered if resolved_at.is_some() || triggered_at.is_none() => {
                return Err(OpenLoopValidationError::InvalidPersistedState {
                    status,
                    reason: "triggered loops require a lease timestamp and no resolved timestamp",
                });
            }
            status if status.is_terminal() && resolved_at.is_none() => {
                return Err(OpenLoopValidationError::InvalidPersistedState {
                    status,
                    reason: "terminal loops require a resolved timestamp",
                });
            }
            status if status.is_terminal() && triggered_at.is_some() => {
                return Err(OpenLoopValidationError::InvalidPersistedState {
                    status,
                    reason: "terminal loops cannot retain a lease timestamp",
                });
            }
            _ => {}
        }
        Ok(Self {
            id,
            owner: draft.owner,
            kind: draft.kind,
            summary: draft.summary,
            source_message_id: draft.source_message_id,
            due_at: draft.due_at,
            expires_at: draft.expires_at,
            salience: draft.salience,
            status,
            created_at,
            updated_at,
            resolved_at,
            triggered_at,
            version,
            dedupe_key: draft.dedupe_key,
        })
    }

    #[must_use]
    pub const fn id(&self) -> OpenLoopId {
        self.id
    }

    #[must_use]
    pub const fn owner(&self) -> OpenLoopOwner {
        self.owner
    }

    #[must_use]
    pub const fn kind(&self) -> OpenLoopKind {
        self.kind
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub const fn source_message_id(&self) -> Option<MessageId> {
        self.source_message_id
    }

    #[must_use]
    pub const fn due_at(&self) -> Option<DateTime<Utc>> {
        self.due_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn salience(&self) -> u8 {
        self.salience
    }

    #[must_use]
    pub const fn status(&self) -> OpenLoopStatus {
        self.status
    }

    #[must_use]
    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn resolved_at(&self) -> Option<DateTime<Utc>> {
        self.resolved_at
    }

    #[must_use]
    pub const fn triggered_at(&self) -> Option<DateTime<Utc>> {
        self.triggered_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn dedupe_key(&self) -> Option<&str> {
        self.dedupe_key.as_deref()
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.status,
            OpenLoopStatus::Open | OpenLoopStatus::Triggered
        )
    }

    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.is_active() && self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    pub fn transition(
        mut self,
        status: OpenLoopStatus,
        now: DateTime<Utc>,
    ) -> Result<Self, OpenLoopValidationError> {
        if self.status != status
            && !matches!(
                (self.status, status),
                (OpenLoopStatus::Open, OpenLoopStatus::Triggered)
                    | (OpenLoopStatus::Open, OpenLoopStatus::Resolved)
                    | (OpenLoopStatus::Open, OpenLoopStatus::Expired)
                    | (OpenLoopStatus::Open, OpenLoopStatus::Cancelled)
                    | (OpenLoopStatus::Triggered, OpenLoopStatus::Open)
                    | (OpenLoopStatus::Triggered, OpenLoopStatus::Resolved)
                    | (OpenLoopStatus::Triggered, OpenLoopStatus::Expired)
                    | (OpenLoopStatus::Triggered, OpenLoopStatus::Cancelled)
            )
        {
            return Err(OpenLoopValidationError::InvalidTransition {
                from: self.status,
                to: status,
            });
        }
        self.status = status;
        self.updated_at = now;
        if status == OpenLoopStatus::Triggered {
            self.triggered_at = Some(now);
        } else {
            self.triggered_at = None;
        }
        if status.is_terminal() {
            self.resolved_at = Some(now);
        } else {
            self.resolved_at = None;
        }
        self.version = self
            .version
            .checked_add(1)
            .ok_or(OpenLoopValidationError::VersionExhausted)?;
        Ok(self)
    }
}

impl<'de> Deserialize<'de> for OpenLoopDraft {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            owner: OpenLoopOwner,
            kind: OpenLoopKind,
            summary: String,
            source_message_id: Option<MessageId>,
            due_at: Option<DateTime<Utc>>,
            expires_at: Option<DateTime<Utc>>,
            salience: u8,
            dedupe_key: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        OpenLoopDraft::new(wire.owner, wire.kind, wire.summary)
            .map_err(serde::de::Error::custom)?
            .with_source_message_id(wire.source_message_id)
            .with_due_at(wire.due_at)
            .with_expires_at(wire.expires_at)
            .with_salience(wire.salience)
            .and_then(|draft| draft.with_dedupe_key(wire.dedupe_key))
            .and_then(|draft| draft.validate().map(|()| draft))
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for OpenLoop {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: OpenLoopId,
            owner: OpenLoopOwner,
            kind: OpenLoopKind,
            summary: String,
            source_message_id: Option<MessageId>,
            due_at: Option<DateTime<Utc>>,
            expires_at: Option<DateTime<Utc>>,
            salience: u8,
            status: OpenLoopStatus,
            created_at: DateTime<Utc>,
            updated_at: DateTime<Utc>,
            resolved_at: Option<DateTime<Utc>>,
            triggered_at: Option<DateTime<Utc>>,
            version: u64,
            dedupe_key: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::restore(
            wire.id,
            wire.owner,
            wire.kind,
            wire.summary,
            wire.source_message_id,
            wire.due_at,
            wire.expires_at,
            wire.salience,
            wire.status,
            wire.created_at,
            wire.updated_at,
            wire.resolved_at,
            wire.triggered_at,
            wire.version,
            wire.dedupe_key,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn validate_summary(value: String) -> Result<String, OpenLoopValidationError> {
    if value.trim().is_empty() {
        return Err(OpenLoopValidationError::EmptySummary);
    }
    if value.contains('\0') {
        return Err(OpenLoopValidationError::SummaryContainsNul);
    }
    if value.len() > MAX_OPEN_LOOP_SUMMARY_BYTES {
        return Err(OpenLoopValidationError::SummaryTooLong {
            length: value.len(),
            maximum: MAX_OPEN_LOOP_SUMMARY_BYTES,
        });
    }
    let length = value.chars().count();
    if length > MAX_OPEN_LOOP_SUMMARY_CHARS {
        return Err(OpenLoopValidationError::SummaryTooManyCharacters {
            length,
            maximum: MAX_OPEN_LOOP_SUMMARY_CHARS,
        });
    }
    Ok(value)
}

fn validate_dedupe_key(value: String) -> Result<String, OpenLoopValidationError> {
    if value.is_empty() {
        return Err(OpenLoopValidationError::EmptyDedupeKey);
    }
    if value.contains('\0') {
        return Err(OpenLoopValidationError::DedupeKeyContainsNul);
    }
    if value.len() > MAX_OPEN_LOOP_DEDUPE_KEY_BYTES {
        return Err(OpenLoopValidationError::DedupeKeyTooLong {
            length: value.len(),
            maximum: MAX_OPEN_LOOP_DEDUPE_KEY_BYTES,
        });
    }
    Ok(value)
}

/// Build a bounded, low-salience "world follow-up" open loop. Hosts use this
/// so a durable fact about the owner-world (a project, a task, a build state)
/// can later surface as a proactive reach-out — the scheduler picks up due
/// follow-ups and turns them into an opportunity to say something useful. The
/// summary is bounded, salience is clamped, and a dedupe key keeps repeats from
/// piling up the same loop.
pub fn world_loop_draft(
    owner: OpenLoopOwner,
    summary: &str,
    salience: u8,
    due_at: Option<DateTime<Utc>>,
    dedupe_key: &str,
) -> Result<OpenLoopDraft, OpenLoopValidationError> {
    OpenLoopDraft::new(owner, OpenLoopKind::FollowUp, summary)?
        .with_salience(salience.clamp(1, MAX_OPEN_LOOP_SALIENCE))?
        .with_due_at(due_at)
        .with_dedupe_key(Some(dedupe_key.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConversationId, PersonId};
    use chrono::{Duration, Utc};

    #[test]
    fn world_loop_draft_is_bounded_dedupable_and_low_salience() {
        let now = Utc::now();
        let loop_draft = world_loop_draft(
            OpenLoopOwner::Person(PersonId::new()),
            "kovi-bot 构建在 main 上通过",
            200,
            Some(now + Duration::hours(1)),
            "world:build:kovi-bot:main",
        )
        .expect("world loop draft is valid");
        assert_eq!(loop_draft.kind(), OpenLoopKind::FollowUp);
        // Salience is clamped into the valid range.
        assert_eq!(loop_draft.salience(), MAX_OPEN_LOOP_SALIENCE);
        assert_eq!(loop_draft.dedupe_key(), Some("world:build:kovi-bot:main"));
        // An over-long summary is rejected.
        let too_long = "x".repeat(MAX_OPEN_LOOP_SUMMARY_CHARS + 1);
        assert!(
            world_loop_draft(OpenLoopOwner::Global, &too_long, 50, None, "world:too-long",)
                .is_err()
        );
    }

    #[test]
    fn draft_bounds_and_expiry_are_validated() {
        assert!(matches!(
            OpenLoopDraft::new(OpenLoopOwner::Global, OpenLoopKind::FollowUp, "  "),
            Err(OpenLoopValidationError::EmptySummary)
        ));
        assert!(matches!(
            OpenLoopDraft::new(
                OpenLoopOwner::Global,
                OpenLoopKind::FollowUp,
                "x".repeat(MAX_OPEN_LOOP_SUMMARY_CHARS + 1)
            ),
            Err(OpenLoopValidationError::SummaryTooManyCharacters { .. })
        ));
        let now = Utc::now();
        let draft = OpenLoopDraft::new(
            OpenLoopOwner::Person(PersonId::new()),
            OpenLoopKind::AwaitingOutcome,
            "interview",
        )
        .expect("valid draft")
        .with_due_at(Some(now + Duration::hours(2)))
        .with_expires_at(Some(now + Duration::hours(1)));
        assert!(matches!(
            draft.validate(),
            Err(OpenLoopValidationError::ExpiryBeforeDue)
        ));
    }

    #[test]
    fn lifecycle_rejects_terminal_resurrection() {
        let now = Utc::now();
        let loop_item = OpenLoop::from_draft(
            OpenLoopId::new(),
            &OpenLoopDraft::new(
                OpenLoopOwner::Conversation(ConversationId::new()),
                OpenLoopKind::PendingQuestion,
                "answer",
            )
            .expect("valid draft"),
            now,
        )
        .expect("valid open loop");
        let resolved = loop_item
            .transition(OpenLoopStatus::Resolved, now)
            .expect("resolve should be legal");
        assert!(matches!(
            resolved.transition(OpenLoopStatus::Open, now),
            Err(OpenLoopValidationError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn serde_round_trip_uses_validated_constructors() {
        let now = Utc::now();
        let draft = OpenLoopDraft::new(OpenLoopOwner::Global, OpenLoopKind::FutureEvent, "event")
            .expect("valid draft")
            .with_dedupe_key(Some("source".to_string()))
            .expect("valid key");
        let encoded = serde_json::to_string(&draft).expect("serialize");
        assert_eq!(
            serde_json::from_str::<OpenLoopDraft>(&encoded).expect("deserialize"),
            draft
        );

        let item = OpenLoop::from_draft(OpenLoopId::new(), &draft, now).expect("open loop");
        let encoded = serde_json::to_string(&item).expect("serialize");
        assert_eq!(
            serde_json::from_str::<OpenLoop>(&encoded).expect("deserialize"),
            item
        );
    }
}
