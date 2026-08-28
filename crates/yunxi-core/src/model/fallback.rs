//! Minimal Strong -> Intrinsic -> Reflex selection.

use super::{CognitiveCapabilitySnapshot, CognitiveTier};
use crate::event::{AttachmentKind, WorldEventKind};
use crate::planner::{
    ModelBackend, ModelBackendError, ModelBackendFuture, PlannerInput, PlannerOutput,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelFallbackPolicy {
    pub strong_to_intrinsic: bool,
    /// Total attempts for one turn. V3 intentionally caps this at two.
    pub max_model_attempts: u8,
}

impl Default for ModelFallbackPolicy {
    fn default() -> Self {
        Self {
            strong_to_intrinsic: true,
            max_model_attempts: 2,
        }
    }
}

impl ModelFallbackPolicy {
    pub fn validate(self) -> Result<(), &'static str> {
        if !(1..=2).contains(&self.max_model_attempts) {
            return Err("max_model_attempts must be within 1..=2");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSelection {
    Strong,
    Intrinsic,
    Reflex,
}

impl ModelSelection {
    #[must_use]
    pub const fn tier(self) -> CognitiveTier {
        match self {
            Self::Strong => CognitiveTier::Standard,
            Self::Intrinsic => CognitiveTier::Intrinsic,
            Self::Reflex => CognitiveTier::Reflex,
        }
    }
}

#[derive(Debug, Default)]
pub struct CognitiveStackMetrics {
    pub strong_calls: AtomicU64,
    pub intrinsic_calls: AtomicU64,
    pub fallback_count: AtomicU64,
    pub reflex_selections: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CognitiveStackMetricsSnapshot {
    pub strong_calls: u64,
    pub intrinsic_calls: u64,
    pub fallback_count: u64,
    pub reflex_selections: u64,
}

impl CognitiveStackMetrics {
    #[must_use]
    pub fn snapshot(&self) -> CognitiveStackMetricsSnapshot {
        CognitiveStackMetricsSnapshot {
            strong_calls: self.strong_calls.load(Ordering::Relaxed),
            intrinsic_calls: self.intrinsic_calls.load(Ordering::Relaxed),
            fallback_count: self.fallback_count.load(Ordering::Relaxed),
            reflex_selections: self.reflex_selections.load(Ordering::Relaxed),
        }
    }
}

/// A thin composite backend. It never retries a successful Strong call and
/// never loops back from Intrinsic to Strong.
pub struct CognitiveModelStack {
    pub intrinsic: Arc<dyn ModelBackend>,
    pub strong: Option<Arc<dyn ModelBackend>>,
    pub policy: ModelFallbackPolicy,
    metrics: Arc<CognitiveStackMetrics>,
}

impl std::fmt::Debug for CognitiveModelStack {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CognitiveModelStack")
            .field("has_strong", &self.strong.is_some())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl CognitiveModelStack {
    pub fn new(
        intrinsic: Arc<dyn ModelBackend>,
        strong: Option<Arc<dyn ModelBackend>>,
        policy: ModelFallbackPolicy,
    ) -> Result<Self, &'static str> {
        policy.validate()?;
        Ok(Self {
            intrinsic,
            strong,
            policy,
            metrics: Arc::new(CognitiveStackMetrics::default()),
        })
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<CognitiveStackMetrics> {
        Arc::clone(&self.metrics)
    }

    #[must_use]
    pub fn select(&self, input: &PlannerInput) -> ModelSelection {
        let preferred = input.executive.cognitive_capability.preferred_tier;
        if input.executive.version == 0 {
            // PlannerInput predates ExecutiveSnapshot. Preserve the old
            // backend-presence behavior for those callers; a versioned
            // snapshot below is the authoritative capability contract.
            if preferred.is_strong() && self.strong.is_some() {
                ModelSelection::Strong
            } else if preferred != CognitiveTier::Reflex {
                ModelSelection::Intrinsic
            } else {
                ModelSelection::Reflex
            }
        } else {
            let mut capability = input.executive.cognitive_capability.clone();
            // The composite backend knows whether a Strong implementation was
            // actually installed. Do not let a stale persisted bit create a
            // Strong selection that cannot be executed.
            capability.strong_available &= self.strong.is_some();
            Self::select_from_capability(&capability, input_requires_vision(input))
        }
    }

    /// Select a tier from a versioned, host-independent capability snapshot.
    /// This is also used by host adapters whose invocation path must retain
    /// platform-specific ticket and delivery context.
    #[must_use]
    pub fn select_from_capability(
        capability: &CognitiveCapabilitySnapshot,
        requires_vision: bool,
    ) -> ModelSelection {
        let intrinsic_available = intrinsic_can_serve(capability, requires_vision);
        if capability.preferred_tier.is_strong() && capability.strong_available {
            ModelSelection::Strong
        } else if capability.preferred_tier != CognitiveTier::Reflex && intrinsic_available {
            ModelSelection::Intrinsic
        } else {
            ModelSelection::Reflex
        }
    }

    pub async fn complete_with_selection(
        &self,
        input: &PlannerInput,
    ) -> Result<PlannerOutput, ModelBackendError> {
        match self.select(input) {
            ModelSelection::Strong => {
                self.metrics.strong_calls.fetch_add(1, Ordering::Relaxed);
                let strong = self.strong.as_ref().expect("selection checked presence");
                match strong.complete(input).await {
                    Ok(output) => Ok(output),
                    Err(error)
                        if self.policy.strong_to_intrinsic
                            && self.policy.max_model_attempts >= 2
                            && is_retryable(&error)
                            && intrinsic_can_serve_for_input(input) =>
                    {
                        self.metrics.fallback_count.fetch_add(1, Ordering::Relaxed);
                        self.metrics.intrinsic_calls.fetch_add(1, Ordering::Relaxed);
                        self.intrinsic.complete(input).await
                    }
                    Err(error) => Err(error),
                }
            }
            ModelSelection::Intrinsic => {
                if input.executive.version > 0
                    && Self::select_from_capability(
                        &input.executive.cognitive_capability,
                        input_requires_vision(input),
                    ) != ModelSelection::Intrinsic
                {
                    // A capability transition can happen between input
                    // construction and invocation. Never call an Intrinsic
                    // backend after the versioned snapshot says it cannot
                    // serve this request.
                    self.metrics
                        .reflex_selections
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(ModelBackendError::Unavailable);
                }
                self.metrics.intrinsic_calls.fetch_add(1, Ordering::Relaxed);
                self.intrinsic.complete(input).await
            }
            ModelSelection::Reflex => {
                self.metrics
                    .reflex_selections
                    .fetch_add(1, Ordering::Relaxed);
                Err(ModelBackendError::Unavailable)
            }
        }
    }
}

fn intrinsic_can_serve(capability: &CognitiveCapabilitySnapshot, requires_vision: bool) -> bool {
    capability.intrinsic_health.can_serve()
        && capability.text_available
        && (!requires_vision || capability.vision_available)
}

fn intrinsic_can_serve_for_input(input: &PlannerInput) -> bool {
    if input.executive.version == 0 {
        // Legacy PlannerInput values predate the capability snapshot. Their
        // injected Intrinsic backend is the only availability signal they
        // have, so retain the v1 fallback contract for those callers.
        true
    } else {
        intrinsic_can_serve(
            &input.executive.cognitive_capability,
            input_requires_vision(input),
        )
    }
}

fn input_requires_vision(input: &PlannerInput) -> bool {
    matches!(
        input.event.kind(),
        WorldEventKind::MessageReceived(message)
            if message
                .content
                .attachments()
                .iter()
                .any(|attachment| attachment.kind() == AttachmentKind::Image)
    )
}

impl ModelBackend for CognitiveModelStack {
    fn complete<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        Box::pin(self.complete_with_selection(input))
    }
}

fn is_retryable(error: &ModelBackendError) -> bool {
    matches!(
        error,
        ModelBackendError::Unavailable
            | ModelBackendError::Failed {
                retryable: true,
                ..
            }
    )
}
