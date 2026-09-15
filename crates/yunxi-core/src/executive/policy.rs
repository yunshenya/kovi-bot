//! Deterministic Executive policy and hard-priority boundaries.

use crate::model::CognitiveTier;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_MAX_PLAN_REVISIONS: u8 = 3;
pub const DEFAULT_MAX_ACTIVE_CONFLICTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutivePolicy {
    pub max_plan_revisions: u8,
    pub conflict_threshold: f32,
    pub max_active_conflicts: usize,
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
            conflict_threshold: 0.60,
            max_active_conflicts: DEFAULT_MAX_ACTIVE_CONFLICTS,
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
        // 预留量也要过 `is_finite`：NaN 与任何数比较都是 false，于是
        // `NaN < 0.0` 和 `NaN > capacity` 同时不成立，一份 NaN 预留能整份通过校验，
        // 一直到 `AttentionBudget::new` 才以另一个错误被拒——那时已经离配置来源很远了。
        if self.attention_budget_capacity <= 0.0
            || !self.attention_budget_capacity.is_finite()
            || !self.critical_attention_reserve.is_finite()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_finite_attention_reserve_is_rejected() {
        // `NaN < 0.0` 与 `NaN > capacity` 同时不成立，所以只比较大小的写法会让
        // 一份 NaN 预留整份通过校验，一直到 AttentionBudget::new 才以另一个错误
        // 被拒——那时已经离配置来源很远了。
        let nan = ExecutivePolicy {
            critical_attention_reserve: f32::NAN,
            ..ExecutivePolicy::default()
        };
        assert!(
            matches!(nan.validate(), Err(ExecutivePolicyError::InvalidBudget)),
            "NaN 预留必须在策略这一层就被拒"
        );
        let infinite = ExecutivePolicy {
            critical_attention_reserve: f32::INFINITY,
            ..ExecutivePolicy::default()
        };
        assert!(matches!(
            infinite.validate(),
            Err(ExecutivePolicyError::InvalidBudget)
        ));
        let normal = ExecutivePolicy {
            critical_attention_reserve: 0.0,
            ..ExecutivePolicy::default()
        };
        assert!(normal.validate().is_ok(), "正常值不受影响");
    }
}
