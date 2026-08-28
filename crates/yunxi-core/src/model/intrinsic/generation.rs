//! A tiny deterministic engine used for domain-only and no-asset operation.
//!
//! Product bundles can replace it with a Rust-native MiniMind/SigLIP engine
//! behind [`IntrinsicInferenceEngine`]. This engine is intentionally marked
//! degraded: it keeps the runtime alive and provides a bounded basic reply,
//! but is not presented as neural weights.

use super::runtime::{
    IntrinsicInferenceEngine, IntrinsicInferenceError, IntrinsicInferenceFuture,
    IntrinsicInferenceOutput, TextInferenceRequest,
};
use crate::model::{IntrinsicModelVersion, ModelHealth};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DeterministicIntrinsicEngine {
    version: IntrinsicModelVersion,
}

impl Default for DeterministicIntrinsicEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl DeterministicIntrinsicEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::with_version(IntrinsicModelVersion {
            model_id: "yunxi-intrinsic-deterministic".to_owned(),
            base_version: "compat-v1".to_owned(),
            adapter_version: None,
            manifest_hash: "builtin".to_owned(),
        })
    }

    #[must_use]
    pub fn with_version(version: IntrinsicModelVersion) -> Self {
        Self { version }
    }

    #[must_use]
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// Engine used when an asset bundle failed integrity checks. Keeping an
/// explicit unavailable engine lets the host report the failure and continue
/// running deterministic Core paths without accidentally serving a partial
/// model.
#[derive(Debug, Clone)]
pub struct UnavailableIntrinsicEngine {
    version: IntrinsicModelVersion,
}

impl UnavailableIntrinsicEngine {
    #[must_use]
    pub fn new(version: IntrinsicModelVersion) -> Self {
        Self { version }
    }
}

impl IntrinsicInferenceEngine for UnavailableIntrinsicEngine {
    fn health(&self) -> ModelHealth {
        ModelHealth::Unavailable
    }

    fn version(&self) -> IntrinsicModelVersion {
        self.version.clone()
    }

    fn infer_text<'a>(
        &'a self,
        _request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        Box::pin(async { Err(IntrinsicInferenceError::Unavailable) })
    }
}

impl IntrinsicInferenceEngine for DeterministicIntrinsicEngine {
    fn health(&self) -> ModelHealth {
        ModelHealth::Degraded
    }

    fn version(&self) -> IntrinsicModelVersion {
        self.version.clone()
    }

    fn infer_text<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        let prompt = request.prompt.clone();
        Box::pin(async move {
            let text = if prompt.contains("?") || prompt.contains('？') || prompt.contains('吗') {
                "我先认真想想这个问题。"
            } else {
                "我收到啦。"
            };
            Ok(IntrinsicInferenceOutput {
                text: text.to_owned(),
                generated_tokens: text.chars().count(),
            })
        })
    }

    fn self_test<'a>(&'a self) -> IntrinsicInferenceFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Explicit name for callers that want the no-asset compatibility path.
pub type BuiltinIntrinsicEngine = DeterministicIntrinsicEngine;

#[allow(dead_code)]
fn _engine_error_is_send_sync(_: IntrinsicInferenceError) {}
