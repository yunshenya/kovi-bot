//! Runtime-neutral inference contract and bounded execution wrapper.

use super::completion::InputCompletion;
use super::config::IntrinsicRuntimeConfig;
use crate::model::IntrinsicModelVersion;
use crate::model::health::ModelHealth;
use crate::model::media::{ModelMediaError, ModelMediaLimits, ResolvedImage};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::time::timeout;

pub type IntrinsicInferenceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, IntrinsicInferenceError>> + Send + 'a>>;

/// Cooperative cancellation for a single generation. Engines check this
/// between decoding steps so a newer turn can reclaim the CPU promptly.
#[derive(Clone, Debug)]
pub struct IntrinsicGenerationControl {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for IntrinsicGenerationControl {
    fn default() -> Self {
        Self::new()
    }
}

impl IntrinsicGenerationControl {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Receives a cumulative visible-text snapshot after a generated token. A
/// cumulative snapshot is deliberate: tokenizers may revise the UTF-8 chunk
/// produced by a preceding token, and callers can replace their draft safely.
pub type IntrinsicTokenCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextInferenceRequest {
    pub prompt: String,
    pub max_context_tokens: usize,
    pub max_new_tokens: usize,
}

impl TextInferenceRequest {
    pub fn validate(&self, config: IntrinsicRuntimeConfig) -> Result<(), IntrinsicInferenceError> {
        if self.prompt.trim().is_empty() {
            return Err(IntrinsicInferenceError::InvalidRequest {
                reason: "prompt must not be empty".to_owned(),
            });
        }
        if self.max_context_tokens == 0 || self.max_context_tokens > config.max_context_tokens {
            return Err(IntrinsicInferenceError::InvalidRequest {
                reason: "context limit exceeds the configured bound".to_owned(),
            });
        }
        if self.max_new_tokens == 0 || self.max_new_tokens > config.max_new_tokens {
            return Err(IntrinsicInferenceError::InvalidRequest {
                reason: "output limit exceeds the configured bound".to_owned(),
            });
        }
        let maximum_prompt_bytes = self.max_context_tokens.saturating_mul(4);
        if self.prompt.len() > maximum_prompt_bytes {
            return Err(IntrinsicInferenceError::InvalidRequest {
                reason: "prompt exceeds the configured context bound".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionInferenceRequest {
    pub prompt: String,
    pub image: ResolvedImage,
    pub max_context_tokens: usize,
    pub max_new_tokens: usize,
}

impl VisionInferenceRequest {
    pub fn validate(&self, config: IntrinsicRuntimeConfig) -> Result<(), IntrinsicInferenceError> {
        TextInferenceRequest {
            prompt: self.prompt.clone(),
            max_context_tokens: self.max_context_tokens,
            max_new_tokens: self.max_new_tokens,
        }
        .validate(config)?;
        self.image
            .validate(config.media.max_bytes, config.media.max_pixels)
            .map_err(IntrinsicInferenceError::Media)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrinsicInferenceOutput {
    pub text: String,
    #[serde(default)]
    pub generated_tokens: usize,
}

impl IntrinsicInferenceOutput {
    fn validate(&self, max_new_tokens: usize) -> Result<(), IntrinsicInferenceError> {
        if self.text.contains('\0') {
            return Err(IntrinsicInferenceError::InvalidOutput {
                reason: "output contains NUL".to_owned(),
            });
        }
        if self.generated_tokens > max_new_tokens {
            return Err(IntrinsicInferenceError::InvalidOutput {
                reason: "engine reported more tokens than requested".to_owned(),
            });
        }
        // A tokenizer token is not a UTF-8 character. In particular, the
        // MiniMind Chinese BPE can decode to several bytes per token, so the
        // old four-byte estimate rejected valid short generations.
        if self.text.len() > max_new_tokens.saturating_mul(16).saturating_add(32) {
            return Err(IntrinsicInferenceError::OutputTooLarge {
                length: self.text.len(),
            });
        }
        Ok(())
    }
}

/// Inference engines stay behind this trait. Candle/ONNX handles and tensor
/// types must never leak into Planner, Executive, or Mind public contracts.
pub trait IntrinsicInferenceEngine: Send + Sync {
    fn health(&self) -> ModelHealth;
    fn version(&self) -> IntrinsicModelVersion;

    fn infer_text<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput>;

    fn infer_text_with_control<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
        _control: IntrinsicGenerationControl,
        _on_token: Option<IntrinsicTokenCallback>,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        self.infer_text(request)
    }

    fn classify_completion<'a>(
        &'a self,
        _request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, InputCompletion> {
        Box::pin(async {
            Err(IntrinsicInferenceError::Unsupported {
                capability: "input_completion_classification".to_owned(),
            })
        })
    }

    fn infer_vision<'a>(
        &'a self,
        request: &'a VisionInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        let _ = request;
        Box::pin(async {
            Err(IntrinsicInferenceError::Unsupported {
                capability: "vision".to_owned(),
            })
        })
    }

    fn self_test<'a>(&'a self) -> IntrinsicInferenceFuture<'a, ()> {
        Box::pin(async move {
            if self.health().can_serve() {
                Ok(())
            } else {
                Err(IntrinsicInferenceError::Unavailable)
            }
        })
    }
}

#[derive(Debug, Default)]
pub struct IntrinsicRuntimeMetrics {
    pub inferences: AtomicU64,
    pub vision_inferences: AtomicU64,
    pub failures: AtomicU64,
    pub fallbacks: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrinsicRuntimeMetricsSnapshot {
    pub inferences: u64,
    pub vision_inferences: u64,
    pub failures: u64,
    pub fallbacks: u64,
}

impl IntrinsicRuntimeMetrics {
    #[must_use]
    pub fn snapshot(&self) -> IntrinsicRuntimeMetricsSnapshot {
        IntrinsicRuntimeMetricsSnapshot {
            inferences: self.inferences.load(Ordering::Relaxed),
            vision_inferences: self.vision_inferences.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
        }
    }
}

pub struct IntrinsicModelRuntime {
    engine: Arc<dyn IntrinsicInferenceEngine>,
    config: IntrinsicRuntimeConfig,
    permits: Arc<Semaphore>,
    metrics: Arc<IntrinsicRuntimeMetrics>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IntrinsicRuntimeError {
    #[error("Intrinsic runtime configuration is invalid: {reason}")]
    InvalidConfig { reason: &'static str },
}

impl std::fmt::Debug for IntrinsicModelRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IntrinsicModelRuntime")
            .field("config", &self.config)
            .field("health", &self.health())
            .field("version", &self.version())
            .finish_non_exhaustive()
    }
}

impl IntrinsicModelRuntime {
    pub fn builtin(config: IntrinsicRuntimeConfig) -> Result<Self, IntrinsicRuntimeError> {
        Self::new(
            Arc::new(super::generation::DeterministicIntrinsicEngine::new()),
            config,
        )
    }

    pub fn new(
        engine: Arc<dyn IntrinsicInferenceEngine>,
        config: IntrinsicRuntimeConfig,
    ) -> Result<Self, IntrinsicRuntimeError> {
        config
            .validate()
            .map_err(|reason| IntrinsicRuntimeError::InvalidConfig { reason })?;
        Ok(Self {
            engine,
            permits: Arc::new(Semaphore::new(config.max_parallel)),
            config,
            metrics: Arc::new(IntrinsicRuntimeMetrics::default()),
        })
    }

    pub fn unavailable(
        config: IntrinsicRuntimeConfig,
        version: IntrinsicModelVersion,
    ) -> Result<Self, IntrinsicRuntimeError> {
        Self::new(
            Arc::new(super::generation::UnavailableIntrinsicEngine::new(version)),
            config,
        )
    }

    #[must_use]
    pub const fn config(&self) -> IntrinsicRuntimeConfig {
        self.config
    }

    #[must_use]
    pub fn health(&self) -> ModelHealth {
        if !self.config.enabled {
            ModelHealth::Unavailable
        } else {
            self.engine.health()
        }
    }

    #[must_use]
    pub fn version(&self) -> IntrinsicModelVersion {
        self.engine.version()
    }

    #[must_use]
    pub fn metrics(&self) -> Arc<IntrinsicRuntimeMetrics> {
        Arc::clone(&self.metrics)
    }

    pub fn mark_fallback(&self) {
        self.metrics.fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn self_test(&self) -> Result<(), IntrinsicInferenceError> {
        if !self.config.enabled {
            return Err(IntrinsicInferenceError::Unavailable);
        }
        self.engine.self_test().await
    }

    /// Runs the configured startup check without changing the engine's
    /// ownership or keeping any request data. Hosts can use the result to
    /// publish a health transition while leaving Core alive on failure.
    pub async fn startup_health(&self) -> ModelHealth {
        match self.self_test().await {
            Ok(()) => self.health(),
            Err(_) => ModelHealth::Unavailable,
        }
    }

    pub async fn infer_text(
        &self,
        request: TextInferenceRequest,
    ) -> Result<IntrinsicInferenceOutput, IntrinsicInferenceError> {
        self.infer_text_with_control(request, IntrinsicGenerationControl::new(), None)
            .await
    }

    pub async fn infer_text_with_control(
        &self,
        request: TextInferenceRequest,
        control: IntrinsicGenerationControl,
        on_token: Option<IntrinsicTokenCallback>,
    ) -> Result<IntrinsicInferenceOutput, IntrinsicInferenceError> {
        if !self.config.enabled || !self.health().can_serve() {
            return Err(IntrinsicInferenceError::Unavailable);
        }
        request.validate(self.config)?;
        if control.is_cancelled() {
            return Err(IntrinsicInferenceError::Cancelled);
        }
        let permit = timeout(self.config.queue_timeout(), self.permits.acquire())
            .await
            .map_err(|_| IntrinsicInferenceError::QueueTimeout)?
            .map_err(|_| IntrinsicInferenceError::Unavailable)?;
        if control.is_cancelled() {
            drop(permit);
            return Err(IntrinsicInferenceError::Cancelled);
        }
        let result = self
            .engine
            .infer_text_with_control(&request, control, on_token)
            .await;
        drop(permit);
        self.metrics.inferences.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(output) => {
                if let Err(error) = output.validate(request.max_new_tokens) {
                    self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                    Err(error)
                } else {
                    Ok(output)
                }
            }
            Err(error) => {
                self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    pub async fn classify_completion(
        &self,
        request: TextInferenceRequest,
    ) -> Result<InputCompletion, IntrinsicInferenceError> {
        if !self.config.enabled || !self.health().can_serve() {
            return Err(IntrinsicInferenceError::Unavailable);
        }
        request.validate(self.config)?;
        let permit = timeout(self.config.queue_timeout(), self.permits.acquire())
            .await
            .map_err(|_| IntrinsicInferenceError::QueueTimeout)?
            .map_err(|_| IntrinsicInferenceError::Unavailable)?;
        let result = self.engine.classify_completion(&request).await;
        drop(permit);
        self.metrics.inferences.fetch_add(1, Ordering::Relaxed);
        result.inspect_err(|_| {
            self.metrics.failures.fetch_add(1, Ordering::Relaxed);
        })
    }

    pub async fn infer_vision(
        &self,
        request: VisionInferenceRequest,
    ) -> Result<IntrinsicInferenceOutput, IntrinsicInferenceError> {
        if !self.config.enabled || !self.health().can_serve() {
            return Err(IntrinsicInferenceError::Unavailable);
        }
        request.validate(self.config)?;
        let permit = timeout(self.config.queue_timeout(), self.permits.acquire())
            .await
            .map_err(|_| IntrinsicInferenceError::QueueTimeout)?
            .map_err(|_| IntrinsicInferenceError::Unavailable)?;
        let result = self.engine.infer_vision(&request).await;
        drop(permit);
        self.metrics
            .vision_inferences
            .fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(output) => {
                if let Err(error) = output.validate(request.max_new_tokens) {
                    self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                    Err(error)
                } else {
                    Ok(output)
                }
            }
            Err(error) => {
                self.metrics.failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IntrinsicInferenceError {
    #[error("Intrinsic model is unavailable")]
    Unavailable,
    #[error("Intrinsic generation was cancelled by a newer turn")]
    Cancelled,
    #[error("Intrinsic inference queue timed out")]
    QueueTimeout,
    #[error("Intrinsic request is invalid: {reason}")]
    InvalidRequest { reason: String },
    #[error("Intrinsic output is invalid: {reason}")]
    InvalidOutput { reason: String },
    #[error("Intrinsic output is {length} bytes, above its bound")]
    OutputTooLarge { length: usize },
    #[error("Intrinsic capability is unsupported: {capability}")]
    Unsupported { capability: String },
    #[error(transparent)]
    Media(#[from] ModelMediaError),
    #[error("Intrinsic engine failed: {message}")]
    Engine { message: String, retryable: bool },
}

impl IntrinsicInferenceError {
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::QueueTimeout
                | Self::Engine {
                    retryable: true,
                    ..
                }
        )
    }
}

impl From<ModelMediaLimits> for IntrinsicRuntimeConfig {
    fn from(media: ModelMediaLimits) -> Self {
        Self {
            media,
            ..Self::default()
        }
    }
}
