use super::common::{
    MindScope, MindValidationError, SCHEMA_VERSION, normalized_key, validate_mind_text,
    validate_unit,
};
use crate::{ConversationId, PersonId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mind_id!(CuriosityId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CuriosityStatus {
    Open,
    Asked,
    Resolved,
    Dropped,
    Expired,
}

impl CuriosityStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Dropped | Self::Expired)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Asked => "asked",
            Self::Resolved => "resolved",
            Self::Dropped => "dropped",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CuriosityItem {
    id: CuriosityId,
    question: String,
    question_key: String,
    subject: Option<PersonId>,
    conversation_id: Option<ConversationId>,
    salience: f32,
    status: CuriosityStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    asked_at: Option<DateTime<Utc>>,
    resolved_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    version: u64,
    schema_version: u16,
}

impl CuriosityItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: CuriosityId,
        question: impl Into<String>,
        subject: Option<PersonId>,
        conversation_id: Option<ConversationId>,
        salience: f32,
        created_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Self, MindValidationError> {
        let question = validate_mind_text(question, "curiosity question")?;
        let item = Self {
            id,
            question_key: normalized_key(&question),
            question,
            subject,
            conversation_id,
            salience: validate_unit(salience, "curiosity salience")?,
            status: CuriosityStatus::Open,
            created_at,
            updated_at: created_at,
            asked_at: None,
            resolved_at: None,
            expires_at,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        item.validate()?;
        Ok(item)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        let question = validate_mind_text(self.question.clone(), "curiosity question")?;
        if normalized_key(&question) != self.question_key {
            return Err(MindValidationError::InvalidProposal {
                reason: "curiosity question key does not match its question",
            });
        }
        validate_unit(self.salience, "curiosity salience")?;
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported curiosity schema version",
            });
        }
        if self.updated_at < self.created_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "curiosity updated_at predates created_at",
            });
        }
        if self
            .expires_at
            .is_some_and(|expires| expires <= self.created_at)
        {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "curiosity expiry must follow creation",
            });
        }
        match self.status {
            CuriosityStatus::Open if self.asked_at.is_some() || self.resolved_at.is_some() => {
                return Err(MindValidationError::InvalidProposal {
                    reason: "open curiosity cannot have terminal timestamps",
                });
            }
            CuriosityStatus::Asked if self.asked_at.is_none() || self.resolved_at.is_some() => {
                return Err(MindValidationError::InvalidProposal {
                    reason: "asked curiosity requires asked_at only",
                });
            }
            status if status.is_terminal() && self.resolved_at.is_none() => {
                return Err(MindValidationError::InvalidProposal {
                    reason: "terminal curiosity requires resolved_at",
                });
            }
            _ => {}
        }
        Ok(())
    }

    pub fn transition(
        &self,
        next: CuriosityStatus,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let allowed = matches!(
            (self.status, next),
            (CuriosityStatus::Open, CuriosityStatus::Asked)
                | (CuriosityStatus::Open, CuriosityStatus::Resolved)
                | (CuriosityStatus::Open, CuriosityStatus::Dropped)
                | (CuriosityStatus::Open, CuriosityStatus::Expired)
                | (CuriosityStatus::Asked, CuriosityStatus::Resolved)
                | (CuriosityStatus::Asked, CuriosityStatus::Dropped)
                | (CuriosityStatus::Asked, CuriosityStatus::Expired)
        ) || self.status == next;
        if !allowed {
            return Err(MindValidationError::InvalidTransition {
                from: self.status.as_str(),
                to: next.as_str(),
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "curiosity transition predates stored state",
            });
        }
        if self.status == next {
            return Ok(self.clone());
        }
        let mut updated = self.clone();
        updated.status = next;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        if next == CuriosityStatus::Asked {
            updated.asked_at = Some(now);
        }
        if next.is_terminal() {
            updated.resolved_at = Some(now);
        }
        updated.validate()?;
        Ok(updated)
    }

    pub fn expire_if_due(&self, now: DateTime<Utc>) -> Result<Self, MindValidationError> {
        if !self.status.is_terminal() && self.expires_at.is_some_and(|expires| expires <= now) {
            self.transition(CuriosityStatus::Expired, now)
        } else {
            Ok(self.clone())
        }
    }

    #[must_use]
    pub const fn id(&self) -> CuriosityId {
        self.id
    }

    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    #[must_use]
    pub fn question_key(&self) -> &str {
        &self.question_key
    }

    #[must_use]
    pub const fn subject(&self) -> Option<PersonId> {
        self.subject
    }

    #[must_use]
    pub const fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation_id
    }

    #[must_use]
    pub const fn scope(&self) -> MindScope {
        if let Some(conversation_id) = self.conversation_id {
            MindScope::Conversation { conversation_id }
        } else if let Some(person_id) = self.subject {
            MindScope::Person { person_id }
        } else {
            MindScope::Global
        }
    }

    #[must_use]
    pub const fn salience(&self) -> f32 {
        self.salience
    }

    #[must_use]
    pub const fn status(&self) -> CuriosityStatus {
        self.status
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    #[must_use]
    pub const fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }
}
