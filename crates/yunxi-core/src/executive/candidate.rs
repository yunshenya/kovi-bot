//! Small candidate sets and deterministic comparison.

use crate::model::CognitiveTier;
use crate::planner::DecisionDisposition;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use super::CandidateId;

pub const MIN_CANDIDATES: usize = 2;
pub const MAX_CANDIDATES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    ReplyCurrentTopic,
    ResumeAgenda,
    AskQuestion,
    Silent,
    Defer,
    ReachOutLater,
    UseTool,
}

impl CandidateKind {
    #[must_use]
    pub const fn disposition(self) -> DecisionDisposition {
        match self {
            Self::ReplyCurrentTopic => DecisionDisposition::Reply,
            Self::ResumeAgenda => DecisionDisposition::ResumeAgenda,
            Self::AskQuestion => DecisionDisposition::AskQuestion,
            Self::Silent => DecisionDisposition::Silent,
            Self::Defer | Self::ReachOutLater => DecisionDisposition::Defer,
            Self::UseTool => DecisionDisposition::SpecialAction,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CandidateScore {
    pub relevance: f32,
    pub utility: f32,
    pub coherence: f32,
    pub social_fit: f32,
    pub goal_progress: f32,
    pub cost: f32,
    pub risk: f32,
    pub interruption_cost: f32,
    #[serde(default)]
    pub semantic_staleness: f32,
    #[serde(default)]
    pub duplicate_question_cost: f32,
    #[serde(default)]
    pub conversation_change_cost: f32,
    #[serde(default)]
    pub user_already_answered: f32,
    #[serde(default)]
    pub direct_preempts_proactive: f32,
    #[serde(default)]
    pub collision_risk: f32,
    #[serde(default)]
    pub rewrite_value: f32,
}

impl Default for CandidateScore {
    fn default() -> Self {
        Self {
            relevance: 0.0,
            utility: 0.0,
            coherence: 0.0,
            social_fit: 0.0,
            goal_progress: 0.0,
            cost: 0.0,
            risk: 0.0,
            interruption_cost: 0.0,
            semantic_staleness: 0.0,
            duplicate_question_cost: 0.0,
            conversation_change_cost: 0.0,
            user_already_answered: 0.0,
            direct_preempts_proactive: 0.0,
            collision_risk: 0.0,
            rewrite_value: 0.0,
        }
    }
}

impl CandidateScore {
    pub fn validate(self) -> Result<(), &'static str> {
        for value in [
            self.relevance,
            self.utility,
            self.coherence,
            self.social_fit,
            self.goal_progress,
            self.cost,
            self.risk,
            self.interruption_cost,
            self.semantic_staleness,
            self.duplicate_question_cost,
            self.conversation_change_cost,
            self.user_already_answered,
            self.direct_preempts_proactive,
            self.collision_risk,
            self.rewrite_value,
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("candidate score values must be within 0..=1");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn weighted(self) -> f32 {
        (0.22 * self.relevance
            + 0.18 * self.utility
            + 0.14 * self.coherence
            + 0.12 * self.social_fit
            + 0.14 * self.goal_progress
            + 0.08 * self.rewrite_value
            - 0.04 * self.cost
            - 0.04 * self.risk
            - 0.04 * self.interruption_cost
            - 0.03 * self.semantic_staleness
            - 0.03 * self.duplicate_question_cost
            - 0.03 * self.conversation_change_cost
            - 0.03 * self.user_already_answered
            - 0.03 * self.collision_risk)
            .clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: CandidateId,
    pub kind: CandidateKind,
    pub score: CandidateScore,
    pub required_cognitive_tier: CognitiveTier,
    pub intrinsic_suitability: f32,
    pub strong_model_value: f32,
    pub fallback_risk: f32,
}

impl Candidate {
    #[must_use]
    pub fn new(kind: CandidateKind, score: CandidateScore) -> Self {
        Self {
            id: CandidateId::new(),
            kind,
            score,
            required_cognitive_tier: CognitiveTier::Intrinsic,
            intrinsic_suitability: 1.0,
            strong_model_value: 0.0,
            fallback_risk: 0.0,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.score.validate()?;
        for value in [
            self.intrinsic_suitability,
            self.strong_model_value,
            self.fallback_risk,
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("candidate capability values must be within 0..=1");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn total_score(&self, available_tier: CognitiveTier) -> f32 {
        let capability = if available_tier.at_least(self.required_cognitive_tier) {
            0.05 * self.intrinsic_suitability
        } else {
            -0.20 * self.fallback_risk
        };
        (self.score.weighted() + capability).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateEvaluation {
    pub candidates: Vec<Candidate>,
    pub selected: Option<CandidateId>,
    pub gray_zone: bool,
}

impl CandidateEvaluation {
    pub fn new(candidates: Vec<Candidate>) -> Result<Self, &'static str> {
        if candidates.len() < MIN_CANDIDATES || candidates.len() > MAX_CANDIDATES {
            return Err("candidate count must be within 2..=4");
        }
        for candidate in &candidates {
            candidate.validate()?;
        }
        Ok(Self {
            candidates,
            selected: None,
            gray_zone: false,
        })
    }

    #[must_use]
    pub fn select(&mut self, available_tier: CognitiveTier) -> Option<&Candidate> {
        self.select_with_threshold(available_tier, 0.08)
    }

    #[must_use]
    pub fn select_with_threshold(
        &mut self,
        available_tier: CognitiveTier,
        gray_zone_threshold: f32,
    ) -> Option<&Candidate> {
        let gray_zone_threshold = if gray_zone_threshold.is_finite() {
            gray_zone_threshold.clamp(0.0, 1.0)
        } else {
            0.08
        };
        let mut order: Vec<usize> = (0..self.candidates.len()).collect();
        order.sort_by(|left, right| {
            self.candidates[*right]
                .total_score(available_tier)
                .partial_cmp(&self.candidates[*left].total_score(available_tier))
                .unwrap_or(Ordering::Equal)
                .then_with(|| self.candidates[*left].id.cmp(&self.candidates[*right].id))
        });
        let best = order.first().copied()?;
        let second = order.get(1).copied();
        self.gray_zone = second.is_some_and(|second| {
            (self.candidates[best].total_score(available_tier)
                - self.candidates[second].total_score(available_tier))
            .abs()
                < gray_zone_threshold
        });
        self.selected = Some(self.candidates[best].id);
        self.candidates.get(best)
    }

    #[must_use]
    pub fn selected_candidate(&self) -> Option<&Candidate> {
        self.selected
            .and_then(|id| self.candidates.iter().find(|candidate| candidate.id == id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateEvaluator {
    pub max_candidates: usize,
    pub gray_zone_threshold: f32,
}

impl Default for CandidateEvaluator {
    fn default() -> Self {
        Self {
            max_candidates: MAX_CANDIDATES,
            gray_zone_threshold: 0.08,
        }
    }
}

impl CandidateEvaluator {
    pub fn validate(self) -> Result<(), &'static str> {
        if !(MIN_CANDIDATES..=MAX_CANDIDATES).contains(&self.max_candidates) {
            return Err("candidate evaluator capacity must be within 2..=4");
        }
        if !self.gray_zone_threshold.is_finite() || !(0.0..=1.0).contains(&self.gray_zone_threshold)
        {
            return Err("candidate evaluator threshold must be within 0..=1");
        }
        Ok(())
    }

    pub fn evaluate(
        self,
        candidates: Vec<Candidate>,
        available_tier: CognitiveTier,
    ) -> Result<CandidateEvaluation, &'static str> {
        self.validate()?;
        if candidates.len() > self.max_candidates {
            return Err("candidate count exceeds configured evaluator bound");
        }
        let mut evaluation = CandidateEvaluation::new(candidates)?;
        let _ = evaluation.select_with_threshold(available_tier, self.gray_zone_threshold);
        Ok(evaluation)
    }
}
