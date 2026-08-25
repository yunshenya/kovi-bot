//! Platform-neutral goal domain types.
//!
//! Goals are durable intentions owned by a person, conversation, or the Core
//! itself.  They are deliberately independent from agent-task or reminder
//! schemas used by a host.

use crate::identity::{ConversationId, GoalId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const MAX_GOAL_TITLE_BYTES: usize = 4 * 1_024;
pub const MAX_GOAL_TITLE_CHARS: usize = 1_024;
pub const MAX_GOAL_DETAILS_BYTES: usize = 16 * 1_024;
pub const MAX_GOAL_DETAILS_CHARS: usize = 8 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOwner {
    Person(PersonId),
    Conversation(ConversationId),
    Global,
}

impl GoalOwner {
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
pub enum GoalKind {
    Personal,
    Conversation,
    FollowUp,
    Project,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Active,
    Paused,
    Completed,
    Cancelled,
}

impl GoalState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GoalValidationError {
    #[error("goal title must not be empty")]
    EmptyTitle,
    #[error("goal title is {length} bytes, above maximum {maximum}")]
    TitleTooLong { length: usize, maximum: usize },
    #[error("goal title is {length} characters, above maximum {maximum}")]
    TitleTooManyCharacters { length: usize, maximum: usize },
    #[error("goal title must not contain NUL")]
    TitleContainsNul,
    #[error("goal details are {length} bytes, above maximum {maximum}")]
    DetailsTooLong { length: usize, maximum: usize },
    #[error("goal details are {length} characters, above maximum {maximum}")]
    DetailsTooManyCharacters { length: usize, maximum: usize },
    #[error("goal details must not contain NUL")]
    DetailsContainsNul,
    #[error("goal state transition from {from:?} to {to:?} is invalid")]
    InvalidTransition { from: GoalState, to: GoalState },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalDraft {
    owner: GoalOwner,
    kind: GoalKind,
    title: String,
    details: Option<String>,
    due_at: Option<DateTime<Utc>>,
}

impl GoalDraft {
    pub fn new(
        owner: GoalOwner,
        kind: GoalKind,
        title: impl Into<String>,
    ) -> Result<Self, GoalValidationError> {
        Ok(Self {
            owner,
            kind,
            title: validate_text(
                title.into(),
                MAX_GOAL_TITLE_BYTES,
                MAX_GOAL_TITLE_CHARS,
                GoalTextField::Title,
            )?,
            details: None,
            due_at: None,
        })
    }

    #[must_use]
    pub const fn owner(&self) -> GoalOwner {
        self.owner
    }
    #[must_use]
    pub const fn kind(&self) -> GoalKind {
        self.kind
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn details(&self) -> Option<&str> {
        self.details.as_deref()
    }
    #[must_use]
    pub const fn due_at(&self) -> Option<DateTime<Utc>> {
        self.due_at
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Result<Self, GoalValidationError> {
        self.details = Some(validate_text(
            details.into(),
            MAX_GOAL_DETAILS_BYTES,
            MAX_GOAL_DETAILS_CHARS,
            GoalTextField::Details,
        )?);
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), GoalValidationError> {
        validate_text(
            self.title.clone(),
            MAX_GOAL_TITLE_BYTES,
            MAX_GOAL_TITLE_CHARS,
            GoalTextField::Title,
        )?;
        if let Some(details) = &self.details {
            validate_text(
                details.clone(),
                MAX_GOAL_DETAILS_BYTES,
                MAX_GOAL_DETAILS_CHARS,
                GoalTextField::Details,
            )?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn with_due_at(mut self, due_at: Option<DateTime<Utc>>) -> Self {
        self.due_at = due_at;
        self
    }
}

impl<'de> Deserialize<'de> for GoalDraft {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            owner: GoalOwner,
            kind: GoalKind,
            title: String,
            details: Option<String>,
            due_at: Option<DateTime<Utc>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let draft = Self {
            owner: wire.owner,
            kind: wire.kind,
            title: wire.title,
            details: wire.details,
            due_at: wire.due_at,
        };
        draft
            .validate()
            .map(|()| draft)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Goal {
    id: GoalId,
    owner: GoalOwner,
    kind: GoalKind,
    title: String,
    details: Option<String>,
    state: GoalState,
    due_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

impl Goal {
    pub fn from_draft(
        id: GoalId,
        draft: &GoalDraft,
        now: DateTime<Utc>,
    ) -> Result<Self, GoalValidationError> {
        draft.validate()?;
        Ok(Self {
            id,
            owner: draft.owner,
            kind: draft.kind,
            title: draft.title.clone(),
            details: draft.details.clone(),
            state: GoalState::Active,
            due_at: draft.due_at,
            created_at: now,
            updated_at: now,
            completed_at: None,
        })
    }

    pub fn validate(&self) -> Result<(), GoalValidationError> {
        GoalDraft {
            owner: self.owner,
            kind: self.kind,
            title: self.title.clone(),
            details: self.details.clone(),
            due_at: self.due_at,
        }
        .validate()
    }

    #[must_use]
    pub const fn id(&self) -> GoalId {
        self.id
    }
    #[must_use]
    pub const fn owner(&self) -> GoalOwner {
        self.owner
    }
    #[must_use]
    pub const fn kind(&self) -> GoalKind {
        self.kind
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn details(&self) -> Option<&str> {
        self.details.as_deref()
    }
    #[must_use]
    pub const fn state(&self) -> GoalState {
        self.state
    }
    #[must_use]
    pub const fn due_at(&self) -> Option<DateTime<Utc>> {
        self.due_at
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
    pub const fn completed_at(&self) -> Option<DateTime<Utc>> {
        self.completed_at
    }

    pub fn transition(
        &mut self,
        state: GoalState,
        now: DateTime<Utc>,
    ) -> Result<(), GoalValidationError> {
        if self.state.is_terminal() && self.state != state {
            return Err(GoalValidationError::InvalidTransition {
                from: self.state,
                to: state,
            });
        }
        if self.state == state {
            return Ok(());
        }
        if state == GoalState::Active && self.state == GoalState::Cancelled {
            return Err(GoalValidationError::InvalidTransition {
                from: self.state,
                to: state,
            });
        }
        self.state = state;
        self.updated_at = now;
        self.completed_at = (state == GoalState::Completed).then_some(now);
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Goal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: GoalId,
            owner: GoalOwner,
            kind: GoalKind,
            title: String,
            details: Option<String>,
            state: GoalState,
            due_at: Option<DateTime<Utc>>,
            created_at: DateTime<Utc>,
            updated_at: DateTime<Utc>,
            completed_at: Option<DateTime<Utc>>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let goal = Self {
            id: wire.id,
            owner: wire.owner,
            kind: wire.kind,
            title: wire.title,
            details: wire.details,
            state: wire.state,
            due_at: wire.due_at,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
            completed_at: wire.completed_at,
        };
        goal.validate()
            .map(|()| goal)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy)]
enum GoalTextField {
    Title,
    Details,
}

fn validate_text(
    value: String,
    max_bytes: usize,
    max_chars: usize,
    field: GoalTextField,
) -> Result<String, GoalValidationError> {
    if value.is_empty() && matches!(field, GoalTextField::Title) {
        return Err(GoalValidationError::EmptyTitle);
    }
    if value.as_bytes().contains(&0) {
        return Err(match field {
            GoalTextField::Title => GoalValidationError::TitleContainsNul,
            GoalTextField::Details => GoalValidationError::DetailsContainsNul,
        });
    }
    if value.len() > max_bytes {
        return Err(match field {
            GoalTextField::Title => GoalValidationError::TitleTooLong {
                length: value.len(),
                maximum: max_bytes,
            },
            GoalTextField::Details => GoalValidationError::DetailsTooLong {
                length: value.len(),
                maximum: max_bytes,
            },
        });
    }
    let chars = value.chars().count();
    if chars > max_chars {
        return Err(match field {
            GoalTextField::Title => GoalValidationError::TitleTooManyCharacters {
                length: chars,
                maximum: max_chars,
            },
            GoalTextField::Details => GoalValidationError::DetailsTooManyCharacters {
                length: chars,
                maximum: max_chars,
            },
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_lifecycle_is_bounded() {
        let draft = GoalDraft::new(GoalOwner::Global, GoalKind::Project, "ship core").unwrap();
        let now = Utc::now();
        let mut goal = Goal::from_draft(GoalId::new(), &draft, now).unwrap();
        goal.transition(GoalState::Paused, now).unwrap();
        goal.transition(GoalState::Completed, now).unwrap();
        assert!(goal.state().is_terminal());
        assert!(goal.completed_at().is_some());
        assert!(goal.transition(GoalState::Active, now).is_err());
    }

    #[test]
    fn serde_cannot_bypass_goal_text_bounds() {
        let draft =
            GoalDraft::new(GoalOwner::Global, GoalKind::Project, "ship core").expect("valid draft");
        let mut encoded = serde_json::to_value(draft).expect("serialize draft");
        encoded["title"] = serde_json::Value::String("x".repeat(MAX_GOAL_TITLE_BYTES + 1));
        assert!(serde_json::from_value::<GoalDraft>(encoded).is_err());
    }
}
