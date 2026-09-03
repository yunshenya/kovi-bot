//! Deterministic goal-priority snapshot used by the Executive snapshot.

use super::policy::HardPriority;
use crate::{GoalId, GoalState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalPrioritySnapshot {
    pub goal_id: GoalId,
    pub score: f32,
    pub hard_priority: Option<HardPriority>,
    pub state: GoalState,
}

impl GoalPrioritySnapshot {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.score.is_finite() || !(0.0..=1.0).contains(&self.score) {
            return Err("goal priority score must be within 0..=1");
        }
        Ok(())
    }
}
