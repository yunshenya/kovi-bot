//! Deterministic Executive policy and hard-priority boundaries.

use crate::model::CognitiveTier;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_MAX_PLAN_REVISIONS: u8 = 3;
pub const DEFAULT_MAX_CANDIDATE_COUNT: usize = 4;
pub const DEFAULT_MAX_ACTIVE_CONFLICTS: usize = 16;
pub const DEFAULT_DEEP_REFLECTION_BUDGET: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutivePolicy {
    pub max_plan_revisions: u8,
    pub max_candidate_count: usize,
    pub conflict_threshold: f32,
    pub max_active_conflicts: usize,
    pub deep_reflection_budget: u32,
    pub attention_budget_capacity: f32,
    pub critical_attention_reserve: f32,
    pub confidence_max_normal_delta: f32,
    pub decision_record_limit: usize,
    pub expectation_limit: usize,
}

impl Default for ExecutivePolicy {
    fn default() -> Self {
        Self {
            max_plan_revisions: DEFAULT_MAX_PLAN_REVISIONS,
            max_candidate_count: DEFAULT_MAX_CANDIDATE_COUNT,
            conflict_threshold: 0.60,
            max_active_conflicts: DEFAULT_MAX_ACTIVE_CONFLICTS,
            deep_reflection_budget: DEFAULT_DEEP_REFLECTION_BUDGET,
            attention_budget_capacity: 20.0,
            critical_attention_reserve: 6.0,
            confidence_max_normal_delta: 0.20,
            decision_record_limit: 32,
            expectation_limit: 8,
        }
    }
}

impl ExecutivePolicy {
    pub fn validate(self) -> Result<(), ExecutivePolicyError> {
        if self.max_plan_revisions == 0 || self.max_plan_revisions > 16 {
            return Err(ExecutivePolicyError::InvalidBound {
                field: "max_plan_revisions",
            });
        }
        if !(2..=4).contains(&self.max_candidate_count) {
            return Err(ExecutivePolicyError::InvalidBound {
                field: "max_candidate_count",
            });
        }
        for (field, value) in [
            ("conflict_threshold", self.conflict_threshold),
            (
                "confidence_max_normal_delta",
                self.confidence_max_normal_delta,
            ),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(ExecutivePolicyError::InvalidUnit { field });
            }
        }
        if self.max_active_conflicts == 0 || self.max_active_conflicts > 128 {
            return Err(ExecutivePolicyError::InvalidBound {
                field: "max_active_conflicts",
            });
        }
        if self.attention_budget_capacity <= 0.0
            || !self.attention_budget_capacity.is_finite()
            || self.critical_attention_reserve < 0.0
            || self.critical_attention_reserve > self.attention_budget_capacity
        {
            return Err(ExecutivePolicyError::InvalidBudget);
        }
        if self.decision_record_limit == 0 || self.decision_record_limit > 256 {
            return Err(ExecutivePolicyError::InvalidBound {
                field: "decision_record_limit",
            });
        }
        if self.expectation_limit == 0 || self.expectation_limit > 64 {
            return Err(ExecutivePolicyError::InvalidBound {
                field: "expectation_limit",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ExecutivePolicyError {
    #[error("executive policy bound `{field}` is invalid")]
    InvalidBound { field: &'static str },
    #[error("executive policy unit `{field}` is outside 0..=1")]
    InvalidUnit { field: &'static str },
    #[error("executive attention budget is invalid")]
    InvalidBudget,
}

/// Hard policy classes are ordered above all soft Executive choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardPriority {
    Safety,
    Permission,
    MustExecute,
    CriticalAction,
    DirectRequest,
}

impl HardPriority {
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Safety => 5,
            Self::Permission => 4,
            Self::MustExecute => 3,
            Self::CriticalAction => 2,
            Self::DirectRequest => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutiveTierDecision {
    Reflex,
    Intrinsic,
    Standard,
    Enhanced,
    Defer,
}

impl From<CognitiveTier> for ExecutiveTierDecision {
    fn from(value: CognitiveTier) -> Self {
        match value {
            CognitiveTier::Reflex => Self::Reflex,
            CognitiveTier::Intrinsic => Self::Intrinsic,
            CognitiveTier::Standard => Self::Standard,
            CognitiveTier::Enhanced => Self::Enhanced,
        }
    }
}
