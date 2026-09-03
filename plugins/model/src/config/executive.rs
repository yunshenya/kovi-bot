use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use yunxi_core::ExecutivePolicy;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutiveConfig {
    enabled: bool,
    shadow_mode: bool,
    conflict: ExecutiveConflictConfig,
    confidence: ExecutiveConfidenceConfig,
    attention_budget: ExecutiveAttentionBudgetConfig,
    plan: ExecutivePlanConfig,
    expectation: ExecutiveExpectationConfig,
    decision_record: ExecutiveDecisionRecordConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutiveConflictConfig {
    threshold: f32,
    max_active: usize,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutiveConfidenceConfig {
    max_normal_delta: f32,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutiveAttentionBudgetConfig {
    capacity: f32,
    critical_reserve: f32,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutivePlanConfig {
    max_revisions: u8,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutiveExpectationConfig {
    max_pending_per_scope: usize,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ExecutiveDecisionRecordConfig {
    recent_limit: usize,
}

impl Default for ExecutiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            shadow_mode: true,
            conflict: ExecutiveConflictConfig::default(),
            confidence: ExecutiveConfidenceConfig::default(),
            attention_budget: ExecutiveAttentionBudgetConfig::default(),
            plan: ExecutivePlanConfig::default(),
            expectation: ExecutiveExpectationConfig::default(),
            decision_record: ExecutiveDecisionRecordConfig::default(),
        }
    }
}

impl Default for ExecutiveConflictConfig {
    fn default() -> Self {
        Self {
            threshold: 0.60,
            max_active: 16,
        }
    }
}

impl Default for ExecutiveConfidenceConfig {
    fn default() -> Self {
        Self {
            max_normal_delta: 0.20,
        }
    }
}

impl Default for ExecutiveAttentionBudgetConfig {
    fn default() -> Self {
        Self {
            capacity: 20.0,
            critical_reserve: 6.0,
        }
    }
}

impl Default for ExecutivePlanConfig {
    fn default() -> Self {
        Self { max_revisions: 3 }
    }
}

impl Default for ExecutiveExpectationConfig {
    fn default() -> Self {
        Self {
            max_pending_per_scope: 8,
        }
    }
}

impl Default for ExecutiveDecisionRecordConfig {
    fn default() -> Self {
        Self { recent_limit: 32 }
    }
}

impl ExecutiveConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.conflict.threshold.is_finite() && (0.0..=1.0).contains(&self.conflict.threshold),
            "executive.conflict.threshold 必须在 0..=1"
        );
        ensure!(
            (1..=128).contains(&self.conflict.max_active),
            "executive.conflict.max_active 必须在 1..=128"
        );
        ensure!(
            self.confidence.max_normal_delta.is_finite()
                && (0.0..=1.0).contains(&self.confidence.max_normal_delta),
            "executive.confidence.max_normal_delta 必须在 0..=1"
        );
        ensure!(
            self.attention_budget.capacity.is_finite()
                && self.attention_budget.capacity > 0.0
                && self.attention_budget.critical_reserve.is_finite()
                && (0.0..=self.attention_budget.capacity)
                    .contains(&self.attention_budget.critical_reserve),
            "executive.attention_budget 范围无效"
        );
        ensure!(
            (1..=16).contains(&self.plan.max_revisions),
            "executive.plan.max_revisions 必须在 1..=16"
        );
        ensure!(
            (1..=64).contains(&self.expectation.max_pending_per_scope),
            "executive.expectation.max_pending_per_scope 必须在 1..=64"
        );
        ensure!(
            (1..=256).contains(&self.decision_record.recent_limit),
            "executive.decision_record.recent_limit 必须在 1..=256"
        );
        self.policy().validate().map_err(anyhow::Error::from)
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn shadow_mode(&self) -> bool {
        self.shadow_mode
    }

    #[must_use]
    pub const fn policy(&self) -> ExecutivePolicy {
        ExecutivePolicy {
            max_plan_revisions: self.plan.max_revisions,
            conflict_threshold: self.conflict.threshold,
            max_active_conflicts: self.conflict.max_active,
            attention_budget_capacity: self.attention_budget.capacity,
            critical_attention_reserve: self.attention_budget.critical_reserve,
            confidence_max_normal_delta: self.confidence.max_normal_delta,
            decision_record_limit: self.decision_record.recent_limit,
            expectation_limit: self.expectation.max_pending_per_scope,
        }
    }
}
