use super::belief::BeliefId;
use super::common::{
    MAX_RELATED_IDS, MindScope, MindValidationError, SCHEMA_VERSION, normalized_key,
    validate_mind_text, validate_unit,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

mind_id!(OpenQuestionId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenQuestionStatus {
    Open,
    Resolved,
    Dropped,
}

impl OpenQuestionStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Resolved | Self::Dropped)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestion {
    id: OpenQuestionId,
    scope: MindScope,
    question: String,
    question_key: String,
    related_beliefs: Vec<BeliefId>,
    salience: f32,
    status: OpenQuestionStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    version: u64,
    schema_version: u16,
}

impl OpenQuestion {
    pub fn new(
        id: OpenQuestionId,
        scope: MindScope,
        question: impl Into<String>,
        related_beliefs: Vec<BeliefId>,
        salience: f32,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        let question = validate_mind_text(question, "open question")?;
        let item = Self {
            id,
            scope,
            question_key: normalized_key(&question),
            question,
            related_beliefs,
            salience: validate_unit(salience, "open-question salience")?,
            status: OpenQuestionStatus::Open,
            created_at: now,
            updated_at: now,
            resolved_at: None,
            version: 1,
            schema_version: SCHEMA_VERSION,
        };
        item.validate()?;
        Ok(item)
    }

    pub fn validate(&self) -> Result<(), MindValidationError> {
        let question = validate_mind_text(self.question.clone(), "open question")?;
        if normalized_key(&question) != self.question_key {
            return Err(MindValidationError::InvalidProposal {
                reason: "open-question key does not match its question",
            });
        }
        validate_unit(self.salience, "open-question salience")?;
        if self.related_beliefs.len() > MAX_RELATED_IDS {
            return Err(MindValidationError::TooManyItems {
                field: "open-question related beliefs",
                length: self.related_beliefs.len(),
                maximum: MAX_RELATED_IDS,
            });
        }
        let mut related = HashSet::new();
        if self
            .related_beliefs
            .iter()
            .any(|belief| !related.insert(*belief))
        {
            return Err(MindValidationError::Duplicate {
                field: "open-question related belief",
            });
        }
        if self.version == 0 {
            return Err(MindValidationError::ZeroVersion);
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(MindValidationError::InvalidProposal {
                reason: "unsupported open-question schema version",
            });
        }
        if self.updated_at < self.created_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "open-question updated_at predates created_at",
            });
        }
        if self.status.is_terminal() != self.resolved_at.is_some() {
            return Err(MindValidationError::InvalidProposal {
                reason: "open-question terminal status and resolved_at disagree",
            });
        }
        Ok(())
    }

    pub fn transition(
        &self,
        next: OpenQuestionStatus,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        if self.status.is_terminal() && self.status != next {
            return Err(MindValidationError::InvalidTransition {
                from: self.status.as_str(),
                to: next.as_str(),
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "open-question transition predates stored state",
            });
        }
        if self.status == next {
            return Ok(self.clone());
        }
        let mut updated = self.clone();
        updated.status = next;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.resolved_at = next.is_terminal().then_some(now);
        updated.validate()?;
        Ok(updated)
    }

    pub fn refresh(
        &self,
        related_beliefs: Vec<BeliefId>,
        salience: f32,
        now: DateTime<Utc>,
    ) -> Result<Self, MindValidationError> {
        if self.status != OpenQuestionStatus::Open {
            return Err(MindValidationError::InvalidTransition {
                from: self.status.as_str(),
                to: OpenQuestionStatus::Open.as_str(),
            });
        }
        if now < self.updated_at {
            return Err(MindValidationError::InvalidTimestamp {
                reason: "open-question refresh predates stored state",
            });
        }
        let mut updated = self.clone();
        updated.related_beliefs = related_beliefs;
        updated.salience = validate_unit(salience, "open-question salience")?;
        updated.updated_at = now;
        updated.version = updated.version.saturating_add(1);
        updated.validate()?;
        Ok(updated)
    }

    #[must_use]
    pub const fn id(&self) -> OpenQuestionId {
        self.id
    }

    #[must_use]
    pub const fn scope(&self) -> MindScope {
        self.scope
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
    pub fn related_beliefs(&self) -> &[BeliefId] {
        &self.related_beliefs
    }

    #[must_use]
    pub const fn salience(&self) -> f32 {
        self.salience
    }

    #[must_use]
    pub const fn status(&self) -> OpenQuestionStatus {
        self.status
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
