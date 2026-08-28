//! Condition-gated use of the existing Mind v2 reflection queue.

use crate::mind::{ReflectionDepth, ReflectionInput, ReflectionTrigger};
use crate::model::{CognitiveTier, ModelHealth};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionDecision {
    NoReflection,
    LightReflection,
    DeepReflection,
    Defer,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReflectionGateContext {
    pub conflict_count: usize,
    pub high_salience_event: bool,
    pub important_episode_ended: bool,
    pub agenda_pressure: bool,
    pub direct_conversation_active: bool,
    pub critical_task_pending: bool,
    pub model_queue_saturated: bool,
    pub current_tier: CognitiveTier,
    pub intrinsic_health: ModelHealth,
    pub deep_budget_remaining: u32,
}

impl Default for ReflectionGateContext {
    fn default() -> Self {
        Self {
            conflict_count: 0,
            high_salience_event: false,
            important_episode_ended: false,
            agenda_pressure: false,
            direct_conversation_active: false,
            critical_task_pending: false,
            model_queue_saturated: false,
            current_tier: CognitiveTier::Intrinsic,
            intrinsic_health: ModelHealth::Unavailable,
            deep_budget_remaining: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReflectionController {
    pub deep_conflict_threshold: usize,
    pub allow_intrinsic_light: bool,
}

impl Default for ReflectionController {
    fn default() -> Self {
        Self {
            deep_conflict_threshold: 2,
            allow_intrinsic_light: true,
        }
    }
}

impl ReflectionController {
    #[must_use]
    pub fn decide(
        &self,
        input: &ReflectionInput,
        context: ReflectionGateContext,
    ) -> ReflectionDecision {
        if !input.should_reflect() {
            return ReflectionDecision::NoReflection;
        }
        if context.direct_conversation_active
            || context.critical_task_pending
            || context.model_queue_saturated
        {
            return ReflectionDecision::Defer;
        }
        let deep = matches!(input.depth, ReflectionDepth::Deep)
            || context.high_salience_event
            || context.important_episode_ended
            || context.agenda_pressure
            || context.conflict_count >= self.deep_conflict_threshold
            || matches!(
                input.trigger,
                ReflectionTrigger::HighSalienceEvent | ReflectionTrigger::DayBoundary
            );
        if !deep {
            return if self.allow_intrinsic_light && context.intrinsic_health.can_serve() {
                ReflectionDecision::LightReflection
            } else {
                ReflectionDecision::Defer
            };
        }
        if context.deep_budget_remaining == 0 {
            return ReflectionDecision::Defer;
        }
        if context.current_tier.is_strong() {
            return ReflectionDecision::DeepReflection;
        }
        // Intrinsic can act as a bounded light substitute, but it does not
        // receive deep belief-rewrite authority by default.
        ReflectionDecision::Defer
    }

    #[must_use]
    pub fn decide_depth(&self, decision: ReflectionDecision) -> Option<ReflectionDepth> {
        match decision {
            ReflectionDecision::LightReflection => Some(ReflectionDepth::Light),
            ReflectionDecision::DeepReflection => Some(ReflectionDepth::Deep),
            ReflectionDecision::NoReflection | ReflectionDecision::Defer => None,
        }
    }
}
