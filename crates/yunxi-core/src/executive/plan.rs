//! Declarative plans and bounded revision lifecycle.

use super::{ExpectationId, PlanId, PlanStepId};
use crate::{ActionId, GoalId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_PLAN_STEPS: usize = 32;
pub const MAX_PLAN_REVISIONS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Active,
    Paused,
    Completed,
    Failed,
    Cancelled,
    NeedsRevision,
}

impl PlanStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PlanStepKind {
    Action(ActionId),
    Observe,
    Wait,
    Evaluate,
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    Active,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff_seconds: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff_seconds: 0,
        }
    }
}

impl RetryPolicy {
    pub fn validate(self) -> Result<(), PlanValidationError> {
        if self.max_attempts == 0 || self.max_attempts > 8 || self.backoff_seconds > 86_400 {
            return Err(PlanValidationError::InvalidRetryPolicy);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: PlanStepId,
    pub kind: PlanStepKind,
    pub status: PlanStepStatus,
    pub expected_result: Option<ExpectationId>,
    pub retry_policy: RetryPolicy,
}

impl PlanStep {
    #[must_use]
    pub fn new(kind: PlanStepKind) -> Self {
        Self {
            id: PlanStepId::new(),
            kind,
            status: PlanStepStatus::Pending,
            expected_result: None,
            retry_policy: RetryPolicy::default(),
        }
    }

    pub fn validate(&self) -> Result<(), PlanValidationError> {
        self.retry_policy.validate()?;
        if let PlanStepKind::Custom(value) = &self.kind
            && (value.trim().is_empty() || value.len() > 512 || value.contains('\0'))
        {
            return Err(PlanValidationError::InvalidStepKind);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStaleReason {
    GoalChanged,
    GoalResolved,
    ConversationChanged,
    UserCancelled,
    CapabilityChanged,
    ExpectationViolated,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRevision {
    pub from_version: u64,
    pub to_version: u64,
    pub reason: PlanStaleReason,
    pub revised_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanState {
    pub id: PlanId,
    pub goal_id: GoalId,
    pub status: PlanStatus,
    pub steps: Vec<PlanStep>,
    pub current_step: usize,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub revision_count: u8,
    #[serde(default)]
    pub stale_reason: Option<PlanStaleReason>,
    #[serde(default)]
    pub revisions: Vec<PlanRevision>,
}

/// Snapshot and state have the same bounded wire representation in V3.
pub type PlanSnapshot = PlanState;

impl PlanState {
    pub fn new(
        id: PlanId,
        goal_id: GoalId,
        steps: Vec<PlanStep>,
        now: DateTime<Utc>,
    ) -> Result<Self, PlanValidationError> {
        let plan = Self {
            id,
            goal_id,
            status: PlanStatus::Draft,
            steps,
            current_step: 0,
            version: 1,
            created_at: now,
            updated_at: now,
            revision_count: 0,
            stale_reason: None,
            revisions: Vec::new(),
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<(), PlanValidationError> {
        if self.steps.is_empty() || self.steps.len() > MAX_PLAN_STEPS {
            return Err(PlanValidationError::InvalidStepCount {
                length: self.steps.len(),
            });
        }
        if self.current_step > self.steps.len() {
            return Err(PlanValidationError::InvalidCurrentStep);
        }
        if self.version == 0 || self.revision_count > MAX_PLAN_REVISIONS {
            return Err(PlanValidationError::InvalidVersion);
        }
        if self.revisions.len() > usize::from(MAX_PLAN_REVISIONS) {
            return Err(PlanValidationError::TooManyRevisions);
        }
        for step in &self.steps {
            step.validate()?;
        }
        Ok(())
    }

    pub fn activate(&mut self, now: DateTime<Utc>) -> Result<(), PlanError> {
        if self.status.is_terminal() {
            return Err(PlanError::Terminal);
        }
        self.status = PlanStatus::Active;
        if let Some(step) = self.steps.get_mut(self.current_step) {
            step.status = PlanStepStatus::Active;
        }
        self.updated_at = now;
        Ok(())
    }

    pub fn pause(&mut self, now: DateTime<Utc>) -> Result<(), PlanError> {
        if self.status.is_terminal() {
            return Err(PlanError::Terminal);
        }
        self.status = PlanStatus::Paused;
        self.updated_at = now;
        Ok(())
    }

    pub fn mark_stale(&mut self, reason: PlanStaleReason, now: DateTime<Utc>) -> bool {
        if self.status.is_terminal() {
            return false;
        }
        self.status = PlanStatus::NeedsRevision;
        self.stale_reason = Some(reason);
        self.updated_at = now;
        true
    }

    pub fn advance(&mut self, now: DateTime<Utc>) -> Result<PlanStepStatus, PlanError> {
        if self.status != PlanStatus::Active {
            return Err(PlanError::NotActive);
        }
        let Some(step) = self.steps.get_mut(self.current_step) else {
            self.status = PlanStatus::Completed;
            self.updated_at = now;
            return Ok(PlanStepStatus::Completed);
        };
        step.status = PlanStepStatus::Completed;
        self.current_step = self.current_step.saturating_add(1);
        if self.current_step >= self.steps.len() {
            self.status = PlanStatus::Completed;
        } else if let Some(next) = self.steps.get_mut(self.current_step) {
            next.status = PlanStepStatus::Active;
        }
        self.updated_at = now;
        Ok(PlanStepStatus::Completed)
    }

    pub fn fail(&mut self, now: DateTime<Utc>) -> Result<(), PlanError> {
        if self.status.is_terminal() {
            return Err(PlanError::Terminal);
        }
        if let Some(step) = self.steps.get_mut(self.current_step) {
            step.status = PlanStepStatus::Failed;
        }
        self.status = PlanStatus::Failed;
        self.updated_at = now;
        Ok(())
    }

    pub fn cancel(&mut self, now: DateTime<Utc>) -> Result<(), PlanError> {
        if self.status.is_terminal() {
            return Err(PlanError::Terminal);
        }
        self.status = PlanStatus::Cancelled;
        self.updated_at = now;
        Ok(())
    }

    pub fn revise(
        &mut self,
        steps: Vec<PlanStep>,
        reason: PlanStaleReason,
        now: DateTime<Utc>,
        max_revisions: u8,
    ) -> Result<(), PlanError> {
        if self.status.is_terminal() {
            return Err(PlanError::Terminal);
        }
        let max_revisions = max_revisions.min(MAX_PLAN_REVISIONS);
        if max_revisions == 0 || self.revision_count >= max_revisions {
            self.status = PlanStatus::Failed;
            self.updated_at = now;
            return Err(PlanError::RevisionLimit {
                maximum: max_revisions,
            });
        }
        let revision = PlanRevision {
            from_version: self.version,
            to_version: self.version.saturating_add(1),
            reason,
            revised_at: now,
        };
        let replacement = Self {
            id: self.id,
            goal_id: self.goal_id,
            status: PlanStatus::Active,
            steps,
            current_step: 0,
            version: revision.to_version,
            created_at: self.created_at,
            updated_at: now,
            revision_count: self.revision_count.saturating_add(1),
            stale_reason: None,
            revisions: {
                let mut revisions = self.revisions.clone();
                revisions.push(revision);
                revisions
            },
        };
        replacement.validate().map_err(PlanError::Invalid)?;
        *self = replacement;
        Ok(())
    }

    #[must_use]
    pub fn is_stale_for(&self, goal_version: u64, current_plan_version: u64) -> bool {
        self.status == PlanStatus::NeedsRevision
            || goal_version != current_plan_version && current_plan_version != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanValidationError {
    #[error("plan has {length} steps; expected 1..={}", MAX_PLAN_STEPS)]
    InvalidStepCount { length: usize },
    #[error("plan current step is out of bounds")]
    InvalidCurrentStep,
    #[error("plan version or revision count is invalid")]
    InvalidVersion,
    #[error("plan has too many revisions")]
    TooManyRevisions,
    #[error("plan step kind is invalid")]
    InvalidStepKind,
    #[error("plan retry policy is invalid")]
    InvalidRetryPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlanError {
    #[error("plan is terminal")]
    Terminal,
    #[error("plan is not active")]
    NotActive,
    #[error("plan revision limit reached (maximum {maximum})")]
    RevisionLimit { maximum: u8 },
    #[error("revised plan is invalid: {0}")]
    Invalid(PlanValidationError),
}
