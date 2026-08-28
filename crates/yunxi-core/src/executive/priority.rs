//! Deterministic goal ordering with hard-priority protection and aging.

use super::policy::HardPriority;
use crate::{Goal, GoalId, GoalState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GoalPriority {
    pub urgency: f32,
    pub importance: f32,
    pub commitment: f32,
    pub social_relevance: f32,
    pub recency: f32,
    pub staleness: f32,
    pub cost: f32,
    pub risk: f32,
}

impl Default for GoalPriority {
    fn default() -> Self {
        Self {
            urgency: 0.0,
            importance: 0.5,
            commitment: 0.0,
            social_relevance: 0.0,
            recency: 0.0,
            staleness: 0.0,
            cost: 0.0,
            risk: 0.0,
        }
    }
}

impl GoalPriority {
    pub fn validate(self) -> Result<(), &'static str> {
        for value in [
            self.urgency,
            self.importance,
            self.commitment,
            self.social_relevance,
            self.recency,
            self.staleness,
            self.cost,
            self.risk,
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("goal priority values must be within 0..=1");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn score(self) -> f32 {
        // The weights are deliberately boring and deterministic. Model output
        // may provide signals, but it cannot replace this baseline.
        (0.20 * self.urgency
            + 0.20 * self.importance
            + 0.15 * self.commitment
            + 0.15 * self.social_relevance
            + 0.10 * self.recency
            + 0.10 * self.staleness
            - 0.05 * self.cost
            - 0.05 * self.risk)
            .clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SocialCost {
    pub interruption: f32,
    pub repetition: f32,
    pub intrusiveness: f32,
    pub context_switch: f32,
}

impl Default for SocialCost {
    fn default() -> Self {
        Self {
            interruption: 0.0,
            repetition: 0.0,
            intrusiveness: 0.0,
            context_switch: 0.0,
        }
    }
}

impl SocialCost {
    pub fn validate(self) -> Result<(), &'static str> {
        for value in [
            self.interruption,
            self.repetition,
            self.intrusiveness,
            self.context_switch,
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("social cost values must be within 0..=1");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn total(self) -> f32 {
        (self.interruption + self.repetition + self.intrusiveness + self.context_switch) / 4.0
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GoalArbitratorConfig {
    pub aging_enabled: bool,
    pub aging_per_hour: f32,
    pub maximum_aging_bonus: f32,
}

impl Default for GoalArbitratorConfig {
    fn default() -> Self {
        Self {
            aging_enabled: true,
            aging_per_hour: 0.02,
            maximum_aging_bonus: 0.25,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrioritizedGoal {
    pub goal_id: GoalId,
    pub score: f32,
    pub hard_priority: Option<HardPriority>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalArbitration {
    pub selected: Option<GoalId>,
    pub preempted: Option<GoalId>,
    pub ranked: Vec<PrioritizedGoal>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GoalArbitrator {
    config: GoalArbitratorConfig,
}

impl GoalArbitrator {
    #[must_use]
    pub const fn new(config: GoalArbitratorConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub fn score(
        &self,
        priority: GoalPriority,
        waiting_since: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> f32 {
        let aging = if self.config.aging_enabled {
            waiting_since.map_or(0.0, |since| {
                let hours = (now - since).num_seconds().max(0) as f32 / 3_600.0;
                (hours * self.config.aging_per_hour).min(self.config.maximum_aging_bonus)
            })
        } else {
            0.0
        };
        (priority.score() + aging).clamp(0.0, 1.0)
    }

    #[must_use]
    pub fn rank(
        &self,
        goals: &[Goal],
        priorities: &[(GoalId, GoalPriority)],
        hard_priorities: &[(GoalId, HardPriority)],
        waiting_since: &[(GoalId, DateTime<Utc>)],
        now: DateTime<Utc>,
    ) -> Vec<GoalPrioritySnapshot> {
        let mut ranked = goals
            .iter()
            .filter(|goal| goal.state() == GoalState::Active)
            .map(|goal| {
                let priority = priorities
                    .iter()
                    .find(|(id, _)| *id == goal.id())
                    .map_or_else(GoalPriority::default, |(_, priority)| *priority);
                let hard = hard_priorities
                    .iter()
                    .find(|(id, _)| *id == goal.id())
                    .map(|(_, hard)| *hard);
                let waiting = waiting_since
                    .iter()
                    .find(|(id, _)| *id == goal.id())
                    .map(|(_, since)| *since);
                GoalPrioritySnapshot {
                    goal_id: goal.id(),
                    score: self.score(priority, waiting, now),
                    hard_priority: hard,
                    state: goal.state(),
                }
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .hard_priority
                .map_or(0, HardPriority::rank)
                .cmp(&left.hard_priority.map_or(0, HardPriority::rank))
                .then_with(|| {
                    right
                        .score
                        .partial_cmp(&left.score)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| left.goal_id.cmp(&right.goal_id))
        });
        ranked
    }

    #[must_use]
    pub fn arbitrate(
        &self,
        goals: &[Goal],
        priorities: &[(GoalId, GoalPriority)],
        hard_priorities: &[(GoalId, HardPriority)],
        waiting_since: &[(GoalId, DateTime<Utc>)],
        current: Option<GoalId>,
        now: DateTime<Utc>,
    ) -> GoalArbitration {
        let ranked = self.rank(goals, priorities, hard_priorities, waiting_since, now);
        let selected = ranked.first().map(|goal| goal.goal_id);
        GoalArbitration {
            selected,
            preempted: current.filter(|current| Some(*current) != selected),
            ranked: ranked
                .iter()
                .map(|item| PrioritizedGoal {
                    goal_id: item.goal_id,
                    score: item.score,
                    hard_priority: item.hard_priority,
                })
                .collect(),
        }
    }
}
