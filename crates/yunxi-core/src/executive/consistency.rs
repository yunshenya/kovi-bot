//! Self-consistency checks that protect stable identity without forbidding
//! evidence-driven belief changes.

use crate::mind::MindSnapshot;
use crate::planner::DecisionPlan;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencyKind {
    Identity,
    Value,
    HighConfidenceBelief,
    GoalCommitment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencySeverity {
    Informational,
    Warning,
    Severe,
    Blocking,
}

impl ConsistencySeverity {
    #[must_use]
    pub const fn score(self) -> f32 {
        match self {
            Self::Informational => 0.20,
            Self::Warning => 0.45,
            Self::Severe => 0.75,
            Self::Blocking => 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelfConsistencyConflict {
    pub kind: ConsistencyKind,
    pub severity: ConsistencySeverity,
    pub score: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencyDecision {
    Allow,
    Replan,
    Block,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsistencyAssessment {
    pub decision: ConsistencyDecision,
    pub conflicts: Vec<SelfConsistencyConflict>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelfConsistencyMonitor {
    pub severe_threshold: f32,
    pub blocking_threshold: f32,
}

impl Default for SelfConsistencyMonitor {
    fn default() -> Self {
        Self {
            severe_threshold: 0.70,
            blocking_threshold: 0.92,
        }
    }
}

impl SelfConsistencyMonitor {
    /// The generic plan contract has no hidden chain-of-thought, so this check
    /// only uses explicit, structured signals. Callers with richer validators
    /// can feed additional conflicts through [`assess_signals`].
    #[must_use]
    pub fn assess(&self, mind: &MindSnapshot, _plan: &DecisionPlan) -> ConsistencyAssessment {
        let mut conflicts = Vec::new();
        if let Some(identity) = mind.self_model()
            && (!identity.identity().is_ai_driven() || !identity.identity().is_host_independent())
        {
            conflicts.push(SelfConsistencyConflict {
                kind: ConsistencyKind::Identity,
                severity: ConsistencySeverity::Blocking,
                score: 1.0,
                reason: "stable self identity flags are invalid".to_owned(),
            });
        }
        let decision = decision_for(&conflicts, self.severe_threshold, self.blocking_threshold);
        ConsistencyAssessment {
            decision,
            conflicts,
        }
    }

    #[must_use]
    pub fn assess_signals(
        &self,
        identity_contradiction: f32,
        value_conflict: f32,
        belief_conflict: f32,
        goal_conflict: f32,
    ) -> ConsistencyAssessment {
        let values = [
            (ConsistencyKind::Identity, identity_contradiction),
            (ConsistencyKind::Value, value_conflict),
            (ConsistencyKind::HighConfidenceBelief, belief_conflict),
            (ConsistencyKind::GoalCommitment, goal_conflict),
        ];
        let conflicts = values
            .into_iter()
            .filter_map(|(kind, score)| {
                let score = if score.is_finite() {
                    score.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                (score >= 0.20).then(|| SelfConsistencyConflict {
                    kind,
                    severity: severity_for(score, self.severe_threshold, self.blocking_threshold),
                    score,
                    reason: "structured output conflicts with a protected state".to_owned(),
                })
            })
            .collect::<Vec<_>>();
        let decision = decision_for(&conflicts, self.severe_threshold, self.blocking_threshold);
        ConsistencyAssessment {
            decision,
            conflicts,
        }
    }
}

fn severity_for(score: f32, severe_threshold: f32, blocking_threshold: f32) -> ConsistencySeverity {
    if score >= blocking_threshold {
        ConsistencySeverity::Blocking
    } else if score >= severe_threshold {
        ConsistencySeverity::Severe
    } else if score >= 0.35 {
        ConsistencySeverity::Warning
    } else {
        ConsistencySeverity::Informational
    }
}

fn decision_for(
    conflicts: &[SelfConsistencyConflict],
    severe_threshold: f32,
    blocking_threshold: f32,
) -> ConsistencyDecision {
    if conflicts
        .iter()
        .any(|conflict| conflict.score >= blocking_threshold)
    {
        ConsistencyDecision::Block
    } else if conflicts
        .iter()
        .any(|conflict| conflict.score >= severe_threshold)
    {
        ConsistencyDecision::Replan
    } else {
        ConsistencyDecision::Allow
    }
}
