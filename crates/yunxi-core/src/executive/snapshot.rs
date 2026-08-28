//! Bounded, serializable Executive state exposed to planners.

use super::attention_budget::{AttentionBudget, AttentionBudgetSnapshot};
use super::conflict::ExecutiveConflict;
use super::decision_record::DecisionRecord;
use super::expectation::Expectation;
use super::plan::PlanState;
use super::priority::GoalPrioritySnapshot;
use crate::model::CognitiveCapabilitySnapshot;
use serde::{Deserialize, Serialize};

pub const MAX_SNAPSHOT_ITEMS: usize = 8;

pub type ConflictSnapshot = ExecutiveConflict;
pub type PlanSnapshot = PlanState;
pub type ExpectationSnapshot = Expectation;
pub type DecisionRecordSnapshot = DecisionRecord;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutiveSnapshot {
    pub active_conflicts: Vec<ConflictSnapshot>,
    pub prioritized_goals: Vec<GoalPrioritySnapshot>,
    pub attention_budget: AttentionBudgetSnapshot,
    pub active_plan: Option<PlanSnapshot>,
    pub pending_expectations: Vec<ExpectationSnapshot>,
    pub recent_decisions: Vec<DecisionRecordSnapshot>,
    pub cognitive_capability: CognitiveCapabilitySnapshot,
    pub version: u64,
}

impl Default for ExecutiveSnapshot {
    fn default() -> Self {
        Self {
            active_conflicts: Vec::new(),
            prioritized_goals: Vec::new(),
            attention_budget: AttentionBudget::default().snapshot(),
            active_plan: None,
            pending_expectations: Vec::new(),
            recent_decisions: Vec::new(),
            cognitive_capability: CognitiveCapabilitySnapshot::default(),
            version: 0,
        }
    }
}

impl ExecutiveSnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version == 0
            && (!self.active_conflicts.is_empty()
                || !self.prioritized_goals.is_empty()
                || self.active_plan.is_some()
                || !self.pending_expectations.is_empty()
                || !self.recent_decisions.is_empty())
        {
            return Err("non-empty Executive snapshot must have a version");
        }
        AttentionBudget::from_snapshot(self.attention_budget)
            .map_err(|_| "Executive snapshot contains an invalid attention budget")?;
        for (name, length) in [
            ("conflicts", self.active_conflicts.len()),
            ("goals", self.prioritized_goals.len()),
            ("expectations", self.pending_expectations.len()),
            ("decisions", self.recent_decisions.len()),
        ] {
            if length > MAX_SNAPSHOT_ITEMS {
                return Err(match name {
                    "conflicts" => "Executive snapshot has too many conflicts",
                    "goals" => "Executive snapshot has too many goals",
                    "expectations" => "Executive snapshot has too many expectations",
                    _ => "Executive snapshot has too many decisions",
                });
            }
        }
        for conflict in &self.active_conflicts {
            conflict
                .validate()
                .map_err(|_| "Executive snapshot contains an invalid conflict")?;
        }
        for goal in &self.prioritized_goals {
            goal.validate()?;
        }
        if let Some(plan) = &self.active_plan {
            plan.validate()
                .map_err(|_| "Executive snapshot contains an invalid plan")?;
        }
        for expectation in &self.pending_expectations {
            expectation
                .validate()
                .map_err(|_| "Executive snapshot contains an invalid expectation")?;
        }
        for decision in &self.recent_decisions {
            decision
                .validate()
                .map_err(|_| "Executive snapshot contains an invalid decision")?;
        }
        self.cognitive_capability.validate()
    }
}
