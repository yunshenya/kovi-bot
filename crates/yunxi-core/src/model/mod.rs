//! Platform-neutral model capability and Intrinsic runtime boundaries.

mod capability;
mod fallback;
mod health;
pub mod intrinsic;
mod manifest;
mod media;
mod tier;

use serde::{Deserialize, Serialize};

pub use capability::{IntrinsicCapability, IntrinsicCapabilitySet, ModelCapability};
pub use fallback::{
    CognitiveModelStack, CognitiveStackMetrics, CognitiveStackMetricsSnapshot, ModelFallbackPolicy,
    ModelSelection,
};
pub use health::ModelHealth;
pub use intrinsic::{
    BoundedInferenceCache, BuiltinIntrinsicEngine, DeterministicIntrinsicEngine, InputCompletion,
    IntrinsicAssetLoadReport, IntrinsicAssetLoader, IntrinsicAssetRuntime,
    IntrinsicGenerationControl, IntrinsicInferenceEngine, IntrinsicInferenceError,
    IntrinsicInferenceFuture, IntrinsicInferenceOutput, IntrinsicModelBackend,
    IntrinsicModelRuntime, IntrinsicRuntimeConfig, IntrinsicRuntimeError, IntrinsicRuntimeMetrics,
    IntrinsicRuntimeMetricsSnapshot, IntrinsicTokenCallback, MiniMindEngine, TextInferenceRequest,
    UnavailableIntrinsicEngine, VisionInferenceRequest, completion_prompt, estimate_tokens,
    lexical_completion, parse_input_completion, truncate_to_tokens,
};
pub use manifest::{
    INTRINSIC_MANIFEST_VERSION, IntrinsicAsset, IntrinsicModelManifest, MAX_INTRINSIC_ASSETS,
    ManifestError,
};
pub use media::{
    DEFAULT_MAX_IMAGE_BYTES, DEFAULT_MAX_IMAGE_PIXELS, DEFAULT_MAX_IMAGES_PER_TURN,
    ModelMediaError, ModelMediaFuture, ModelMediaLimits, ModelMediaResolver, ResolvedImage,
    validate_resolved_image,
};
pub use tier::CognitiveTier;

/// Version identity is independent of a Rust process or a host provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrinsicModelVersion {
    pub model_id: String,
    pub base_version: String,
    pub adapter_version: Option<String>,
    pub manifest_hash: String,
}

impl IntrinsicModelVersion {
    pub fn new(
        model_id: impl Into<String>,
        base_version: impl Into<String>,
        manifest_hash: impl Into<String>,
    ) -> Result<Self, &'static str> {
        let version = Self {
            model_id: model_id.into(),
            base_version: base_version.into(),
            adapter_version: None,
            manifest_hash: manifest_hash.into(),
        };
        version.validate()?;
        Ok(version)
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter_version: Option<String>) -> Self {
        self.adapter_version = adapter_version;
        self
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        for value in [&self.model_id, &self.base_version, &self.manifest_hash] {
            if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err("Intrinsic model version contains invalid text");
            }
        }
        if let Some(adapter) = &self.adapter_version
            && (adapter.trim().is_empty()
                || adapter.len() > 512
                || adapter.chars().any(char::is_control))
        {
            return Err("Intrinsic adapter version contains invalid text");
        }
        Ok(())
    }
}

/// What the Executive tells a planner about available capability, without
/// leaking secrets, URLs, device handles, or raw model pointers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitiveCapabilitySnapshot {
    pub current_tier: CognitiveTier,
    pub preferred_tier: CognitiveTier,
    pub intrinsic_health: ModelHealth,
    pub strong_available: bool,
    pub text_available: bool,
    pub vision_available: bool,
    pub intrinsic_version: Option<IntrinsicModelVersion>,
}

impl Default for CognitiveCapabilitySnapshot {
    fn default() -> Self {
        Self {
            // Standard preserves the existing Planner/host behavior when an
            // old caller does not provide an Executive snapshot. A stack with
            // no strong backend automatically selects Intrinsic instead.
            current_tier: CognitiveTier::Standard,
            preferred_tier: CognitiveTier::Standard,
            intrinsic_health: ModelHealth::Unavailable,
            strong_available: false,
            text_available: false,
            vision_available: false,
            intrinsic_version: None,
        }
    }
}

impl CognitiveCapabilitySnapshot {
    #[must_use]
    pub fn intrinsic(
        health: ModelHealth,
        version: Option<IntrinsicModelVersion>,
        vision_available: bool,
    ) -> Self {
        let available = health.can_serve();
        Self {
            current_tier: if available {
                CognitiveTier::Intrinsic
            } else {
                CognitiveTier::Reflex
            },
            preferred_tier: CognitiveTier::Intrinsic,
            intrinsic_health: health,
            strong_available: false,
            text_available: available,
            vision_available: available && vision_available,
            intrinsic_version: version,
        }
    }

    #[must_use]
    pub fn reflex() -> Self {
        Self {
            current_tier: CognitiveTier::Reflex,
            preferred_tier: CognitiveTier::Reflex,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.intrinsic_health == ModelHealth::Unavailable
            && (self.text_available || self.vision_available)
        {
            return Err("unavailable Intrinsic cannot advertise media capability");
        }
        if self.vision_available && !self.text_available {
            return Err("vision capability requires text capability");
        }
        if let Some(version) = &self.intrinsic_version {
            version.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventPriority, EventScope, WorldEvent, WorldEventKind};
    use crate::planner::{
        ModelBackend, ModelBackendError, ModelBackendFuture, PlannerInput, PlannerOutput,
    };
    use chrono::Utc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn input() -> PlannerInput {
        PlannerInput::new(
            WorldEvent::new(
                Utc::now(),
                EventScope::Global,
                EventPriority::High,
                WorldEventKind::IdleTick,
            ),
            crate::PlannerStateSnapshot::empty(),
        )
    }

    #[test]
    fn cognitive_tiers_have_a_single_monotonic_order_and_fallback() {
        assert!(CognitiveTier::Enhanced > CognitiveTier::Standard);
        assert!(CognitiveTier::Standard > CognitiveTier::Intrinsic);
        assert_eq!(
            CognitiveTier::Enhanced.fallback(),
            Some(CognitiveTier::Standard)
        );
        assert_eq!(CognitiveTier::Reflex.fallback(), None);
    }

    #[test]
    fn intrinsic_capability_rejects_audio() {
        assert!(IntrinsicCapabilitySet::default().validate().is_ok());
        assert!(
            IntrinsicCapabilitySet {
                text: true,
                vision: true,
                audio: true,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn version_round_trip_keeps_adapter_slot() {
        let version = IntrinsicModelVersion::new("mini", "v1", "hash")
            .expect("version should validate")
            .with_adapter(Some("adapter-2".to_owned()));
        let encoded = serde_json::to_string(&version).expect("version should serialize");
        let decoded: IntrinsicModelVersion =
            serde_json::from_str(&encoded).expect("version should deserialize");
        assert_eq!(decoded, version);
    }

    #[tokio::test]
    async fn builtin_intrinsic_runtime_serves_bounded_text_without_network() {
        let runtime = IntrinsicModelRuntime::builtin(IntrinsicRuntimeConfig::default())
            .expect("builtin runtime should be constructible");
        assert_eq!(runtime.health(), ModelHealth::Degraded);
        assert_eq!(runtime.startup_health().await, ModelHealth::Degraded);
        let output = runtime
            .infer_text(TextInferenceRequest {
                prompt: "你好？".to_owned(),
                max_context_tokens: 128,
                max_new_tokens: 64,
            })
            .await
            .expect("builtin runtime should answer");
        assert!(!output.text.is_empty());
    }

    #[tokio::test]
    async fn disabled_intrinsic_runtime_fails_closed() {
        let runtime = IntrinsicModelRuntime::builtin(IntrinsicRuntimeConfig {
            enabled: false,
            ..IntrinsicRuntimeConfig::default()
        })
        .expect("disabled runtime config should still construct");
        assert_eq!(runtime.health(), ModelHealth::Unavailable);
        assert!(matches!(
            runtime
                .infer_text(TextInferenceRequest {
                    prompt: "hello".to_owned(),
                    max_context_tokens: 32,
                    max_new_tokens: 16,
                })
                .await,
            Err(IntrinsicInferenceError::Unavailable)
        ));
    }

    struct CountingBackend {
        calls: AtomicUsize,
        result: Result<PlannerOutput, ModelBackendError>,
    }

    impl CountingBackend {
        fn success() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Ok(PlannerOutput::silent()),
            }
        }

        fn retryable_failure() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Err(ModelBackendError::Unavailable),
            }
        }
    }

    impl ModelBackend for CountingBackend {
        fn plan<'a>(&'a self, _input: &'a PlannerInput) -> ModelBackendFuture<'a> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn strong_success_does_not_call_intrinsic_and_retry_falls_back_once() {
        let strong = Arc::new(CountingBackend::success());
        let intrinsic = Arc::new(CountingBackend::success());
        let stack = CognitiveModelStack::new(
            Arc::clone(&intrinsic) as Arc<dyn ModelBackend>,
            Some(Arc::clone(&strong) as Arc<dyn ModelBackend>),
            ModelFallbackPolicy::default(),
        )
        .expect("fallback policy should validate");
        let mut strong_input = input();
        strong_input.executive.cognitive_capability.preferred_tier = CognitiveTier::Standard;
        // A version-zero input has no capability snapshot; the legacy stack
        // treats the injected Intrinsic backend itself as the availability
        // signal for fallback.
        stack
            .complete_with_selection(&strong_input)
            .await
            .expect("strong success should pass");
        assert_eq!(strong.calls.load(Ordering::Relaxed), 1);
        assert_eq!(intrinsic.calls.load(Ordering::Relaxed), 0);

        let strong_failure = Arc::new(CountingBackend::retryable_failure());
        let intrinsic_fallback = Arc::new(CountingBackend::success());
        let fallback_stack = CognitiveModelStack::new(
            Arc::clone(&intrinsic_fallback) as Arc<dyn ModelBackend>,
            Some(Arc::clone(&strong_failure) as Arc<dyn ModelBackend>),
            ModelFallbackPolicy::default(),
        )
        .expect("fallback policy should validate");
        fallback_stack
            .complete_with_selection(&strong_input)
            .await
            .expect("retryable strong failure should use intrinsic once");
        assert_eq!(strong_failure.calls.load(Ordering::Relaxed), 1);
        assert_eq!(intrinsic_fallback.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn manifest_rejects_audio_and_unsafe_asset_paths() {
        let mut manifest = IntrinsicModelManifest {
            manifest_version: INTRINSIC_MANIFEST_VERSION,
            model_id: "mini".to_owned(),
            model_version: "v1".to_owned(),
            architecture: "minimind".to_owned(),
            upstream_repository: "upstream".to_owned(),
            upstream_revision: "rev".to_owned(),
            supports_text: true,
            supports_vision: true,
            supports_audio: false,
            context_limit: 2_048,
            image_size: 256,
            assets: Vec::new(),
            adapter_version: None,
        };
        assert!(manifest.validate().is_ok());
        manifest.supports_audio = true;
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::AudioNotSupported)
        ));
        assert!(matches!(
            IntrinsicAsset {
                path: "../weights.bin".to_owned(),
                sha256: "0".repeat(64),
                size_bytes: None,
            }
            .validate(),
            Err(ManifestError::InvalidAssetPath { .. })
        ));
    }

    #[test]
    fn media_validation_enforces_pixel_and_audio_boundaries() {
        let image = ResolvedImage {
            bytes: Arc::<[u8]>::from(vec![1, 2, 3]),
            media_type: Some("image/png".to_owned()),
            width: 2,
            height: 2,
        };
        assert!(image.validate(10, 4).is_ok());
        assert!(matches!(
            image.validate(10, 3),
            Err(ModelMediaError::TooManyPixels { .. })
        ));
    }
}
