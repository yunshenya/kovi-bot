//! Host lifecycle for the Core-owned Intrinsic runtime.
//!
//! This module is deliberately small: Core owns the inference contract and
//! bounds, while the plugin owns asset discovery, startup reporting, and the
//! conversion from Kovi's already-materialized image data URLs.

use crate::config::{self, IntrinsicConfig};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use yunxi_core::{
    CognitiveCapabilitySnapshot, CognitiveTier, InputCompletion, IntrinsicAssetLoadReport,
    IntrinsicAssetLoader, IntrinsicGenerationControl, IntrinsicInferenceError,
    IntrinsicInferenceOutput, IntrinsicModelRuntime, IntrinsicRuntimeConfig,
    IntrinsicRuntimeMetricsSnapshot, IntrinsicTokenCallback, ModelFallbackPolicy, ModelHealth,
    ResolvedImage, TextInferenceRequest, VisionInferenceRequest, completion_prompt,
    lexical_completion,
};

static HOST_RUNTIME: OnceLock<Arc<IntrinsicHostRuntime>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct IntrinsicHostRuntime {
    runtime: Arc<IntrinsicModelRuntime>,
    report: IntrinsicAssetLoadReport,
    fallback_policy: ModelFallbackPolicy,
    last_self_test_health: AtomicI64,
}

impl IntrinsicHostRuntime {
    pub(crate) fn load() -> Arc<Self> {
        let current = config::get();
        let intrinsic = current.model().intrinsic().clone();
        let fallback_policy = current.model().fallback().policy();
        let bundle = load_bundle(&intrinsic);
        let host = Arc::new(Self {
            last_self_test_health: AtomicI64::new(health_code(bundle.report.health)),
            runtime: bundle.runtime,
            report: bundle.report,
            fallback_policy,
        });
        if intrinsic.runtime_config().startup_self_test && host.health().can_serve() {
            let probe = Arc::clone(&host);
            kovi::tokio::spawn(async move {
                let health = probe.runtime.startup_health().await;
                probe
                    .last_self_test_health
                    .store(health_code(health), Ordering::Release);
                if let Some(controller) = super::executive_controller() {
                    let _ = controller.set_capability(probe.capability_snapshot());
                }
                if health == ModelHealth::Unavailable {
                    kovi::log::warn!("Yunxi Intrinsic startup self-test failed");
                }
            });
        }
        host
    }

    pub(crate) fn runtime(&self) -> &Arc<IntrinsicModelRuntime> {
        &self.runtime
    }

    pub(crate) fn health(&self) -> ModelHealth {
        combine_health(
            self.runtime.health(),
            decode_health(self.last_self_test_health.load(Ordering::Acquire)),
        )
    }

    pub(crate) fn version(&self) -> Option<yunxi_core::IntrinsicModelVersion> {
        self.report.version.clone()
    }

    pub(crate) fn supports_text(&self) -> bool {
        self.report.supports_text && self.health().can_serve()
    }

    pub(crate) fn supports_vision(&self) -> bool {
        self.report.supports_vision && self.supports_text()
    }

    pub(crate) fn fallback_policy(&self) -> ModelFallbackPolicy {
        self.fallback_policy
    }

    pub(crate) fn metrics(&self) -> IntrinsicRuntimeMetricsSnapshot {
        self.runtime.metrics().snapshot()
    }

    pub(crate) fn mark_fallback(&self) {
        self.runtime.mark_fallback();
    }

    pub(crate) fn capability_snapshot(&self) -> CognitiveCapabilitySnapshot {
        let strong_available = strong_is_configured();
        let health = self.health();
        let intrinsic_available = self.supports_text();
        CognitiveCapabilitySnapshot {
            current_tier: if strong_available {
                CognitiveTier::Standard
            } else if intrinsic_available {
                CognitiveTier::Intrinsic
            } else {
                CognitiveTier::Reflex
            },
            preferred_tier: if strong_available {
                CognitiveTier::Standard
            } else if intrinsic_available {
                CognitiveTier::Intrinsic
            } else {
                CognitiveTier::Reflex
            },
            intrinsic_health: health,
            strong_available,
            text_available: intrinsic_available,
            vision_available: self.supports_vision(),
            intrinsic_version: self.version(),
        }
    }

    pub(crate) fn status_report(&self) -> String {
        let metrics = self.metrics();
        let version = self
            .version()
            .map(|version| {
                format!(
                    "{} / base {} / adapter {} / manifest {}",
                    version.model_id,
                    version.base_version,
                    version.adapter_version.as_deref().unwrap_or("none"),
                    version.manifest_hash
                )
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let self_test = decode_health(self.last_self_test_health.load(Ordering::Acquire));
        let asset = self
            .report
            .error
            .as_deref()
            .map(|error| bound_status_text(error, 240))
            .unwrap_or_else(|| "verified or builtin compatibility bundle".to_owned());
        format!(
            "Intrinsic 状态\n加载状态：{:?}\n模型版本：{}\n资产：{}（{} 个文件）\n能力：text={}，vision={}\n队列并发：{}\n推理：text {}，vision {}，失败 {}，fallback {}\n最近 self-test：{:?}\n外部强模型：{}",
            self.health(),
            version,
            asset,
            self.report.asset_count,
            self.supports_text(),
            self.supports_vision(),
            self.runtime.config().max_parallel,
            metrics.inferences,
            metrics.vision_inferences,
            metrics.failures,
            metrics.fallbacks,
            self_test,
            if strong_is_configured() {
                "configured"
            } else {
                "disabled/unconfigured"
            },
        )
    }

    pub(crate) async fn infer_text_with_control(
        &self,
        request: TextInferenceRequest,
        control: IntrinsicGenerationControl,
        on_token: Option<IntrinsicTokenCallback>,
    ) -> Result<IntrinsicInferenceOutput, IntrinsicInferenceError> {
        if !self.supports_text() {
            return Err(IntrinsicInferenceError::Unavailable);
        }
        self.runtime
            .infer_text_with_control(request, control, on_token)
            .await
    }

    /// Classify whether an inbound message is a complete user turn. The
    /// lexical pass handles only unambiguous syntax; the MiniMind classifier
    /// owns the grey area. Failure is intentionally conservative so a partial
    /// thought is not sent to the main model as a finished turn.
    pub(crate) async fn classify_input_completion(&self, text: &str) -> InputCompletion {
        if let Some(result) = lexical_completion(text) {
            return result;
        }
        if !self.supports_text() {
            return InputCompletion::Incomplete;
        }
        let config = self.runtime.config();
        let max_context_tokens = config.max_context_tokens.clamp(1, 512);
        let input_budget = max_context_tokens.saturating_sub(128).max(1);
        let bounded = yunxi_core::truncate_to_tokens(text, input_budget);
        let request = TextInferenceRequest {
            prompt: completion_prompt(&bounded),
            max_context_tokens,
            max_new_tokens: 1,
        };
        match self.runtime.classify_completion(request).await {
            Ok(result) => result,
            Err(error) => {
                kovi::log::debug!("Yunxi input-completion classifier unavailable: {error}");
                InputCompletion::Incomplete
            }
        }
    }

    pub(crate) async fn infer_vision(
        &self,
        request: VisionInferenceRequest,
    ) -> Result<IntrinsicInferenceOutput, IntrinsicInferenceError> {
        if !self.supports_vision() {
            return Err(IntrinsicInferenceError::Unsupported {
                capability: "vision".to_owned(),
            });
        }
        self.runtime.infer_vision(request).await
    }
}

pub(crate) fn install() -> Arc<IntrinsicHostRuntime> {
    let runtime = IntrinsicHostRuntime::load();
    HOST_RUNTIME.set(Arc::clone(&runtime)).unwrap_or(());
    HOST_RUNTIME.get().cloned().unwrap_or(runtime)
}

pub(crate) fn get() -> Option<Arc<IntrinsicHostRuntime>> {
    HOST_RUNTIME.get().cloned()
}

fn load_bundle(intrinsic: &IntrinsicConfig) -> yunxi_core::IntrinsicAssetRuntime {
    let loader = IntrinsicAssetLoader;
    let root = PathBuf::from(intrinsic.asset_dir());
    match loader.load_or_builtin(&root, intrinsic.runtime_config()) {
        Ok(bundle) => {
            if bundle.report.error.is_some() {
                kovi::log::warn!(
                    "Yunxi Intrinsic asset bundle unavailable: {}",
                    bundle.report.error.as_deref().unwrap_or("unknown error")
                );
            } else if bundle.report.asset_count == 0 {
                kovi::log::info!(
                    "Yunxi Intrinsic using builtin deterministic compatibility runtime"
                );
            } else {
                kovi::log::info!(
                    "Yunxi Intrinsic manifest verified: {} assets",
                    bundle.report.asset_count
                );
            }
            bundle
        }
        Err(error) => {
            // Config is validated before plugin startup. This branch is still
            // fail-soft so a malformed runtime setting cannot disable Core's
            // deletion, reminder, or deterministic recovery paths.
            kovi::log::error!("Yunxi Intrinsic runtime setup failed: {error}");
            let disabled = IntrinsicRuntimeConfig {
                enabled: false,
                ..IntrinsicRuntimeConfig::default()
            };
            IntrinsicAssetLoader
                .load_or_builtin("__yunxi_invalid_intrinsic_config__", disabled)
                .expect("disabled builtin Intrinsic runtime must be constructible")
        }
    }
}

fn strong_is_configured() -> bool {
    let server = config::get().server_config().clone();
    server.enabled()
        && (!server.requires_auth()
            || std::env::var(server.api_key_env())
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false))
}

fn health_code(health: ModelHealth) -> i64 {
    match health {
        ModelHealth::Loading => 0,
        ModelHealth::Healthy => 1,
        ModelHealth::Degraded => 2,
        ModelHealth::Unavailable => 3,
    }
}

fn decode_health(code: i64) -> ModelHealth {
    match code {
        1 => ModelHealth::Healthy,
        2 => ModelHealth::Degraded,
        3 => ModelHealth::Unavailable,
        _ => ModelHealth::Loading,
    }
}

fn combine_health(runtime: ModelHealth, self_test: ModelHealth) -> ModelHealth {
    if runtime == ModelHealth::Unavailable || self_test == ModelHealth::Unavailable {
        ModelHealth::Unavailable
    } else if runtime == ModelHealth::Loading {
        ModelHealth::Loading
    } else if runtime == ModelHealth::Degraded || self_test == ModelHealth::Degraded {
        ModelHealth::Degraded
    } else {
        ModelHealth::Healthy
    }
}

fn bound_status_text(value: &str, maximum: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    value.chars().take(maximum).collect()
}

/// Convert one already materialized, host-validated data URL into Core's
/// platform-neutral image value. No URL is fetched here.
pub(crate) fn resolved_image_from_data_url(
    data_url: &str,
    maximum_bytes: usize,
) -> anyhow::Result<ResolvedImage> {
    let (media_type, bytes) =
        crate::image_security::decode_validated_image_data_url(data_url, maximum_bytes)?;
    ResolvedImage::from_bytes(bytes, Some(media_type))
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}
