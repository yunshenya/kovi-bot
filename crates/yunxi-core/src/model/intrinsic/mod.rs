//! Intrinsic model domain wrapper and runtime implementation.

mod cache;
mod completion;
mod config;
mod generation;
mod loader;
mod minimind;
mod runtime;
mod text;
mod tokenizer;
mod vision;

use crate::event::{AttachmentKind, WorldEventKind};
use crate::planner::{
    DecisionDisposition, ModelBackend, ModelBackendError, ModelBackendFuture, PlannerInput,
    PlannerOutput,
};
use crate::{CognitiveIntent, MessageContent};
use std::sync::Arc;

pub use cache::BoundedInferenceCache;
pub use completion::{
    InputCompletion, completion_prompt, lexical_completion, parse_input_completion,
};
pub use config::IntrinsicRuntimeConfig;
pub use generation::{
    BuiltinIntrinsicEngine, DeterministicIntrinsicEngine, UnavailableIntrinsicEngine,
};
pub use loader::{IntrinsicAssetLoadReport, IntrinsicAssetLoader, IntrinsicAssetRuntime};
pub use minimind::MiniMindEngine;
pub use runtime::{
    IntrinsicGenerationControl, IntrinsicInferenceEngine, IntrinsicInferenceError,
    IntrinsicInferenceFuture, IntrinsicInferenceOutput, IntrinsicModelRuntime,
    IntrinsicRuntimeError, IntrinsicRuntimeMetrics, IntrinsicRuntimeMetricsSnapshot,
    IntrinsicTokenCallback, TextInferenceRequest, VisionInferenceRequest,
};
pub use text::bounded_text_prompt;
pub use tokenizer::{estimate_tokens, truncate_to_tokens};
pub use vision::validate_vision_input;

/// Adapter from bounded Intrinsic generation to the existing declarative
/// planner contract. It only emits a simple reply intent; tool, permission,
/// destructive, and multi-step planning remain outside the v1 allowlist.
pub struct IntrinsicModelBackend {
    runtime: Arc<IntrinsicModelRuntime>,
    media_resolver: Option<Arc<dyn crate::ModelMediaResolver>>,
}

impl std::fmt::Debug for IntrinsicModelBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IntrinsicModelBackend")
            .field("health", &self.runtime.health())
            .field("version", &self.runtime.version())
            .field("has_media_resolver", &self.media_resolver.is_some())
            .finish()
    }
}

impl IntrinsicModelBackend {
    #[must_use]
    pub fn new(runtime: Arc<IntrinsicModelRuntime>) -> Self {
        Self {
            runtime,
            media_resolver: None,
        }
    }

    pub fn builtin(config: IntrinsicRuntimeConfig) -> Result<Self, runtime::IntrinsicRuntimeError> {
        Ok(Self::new(Arc::new(IntrinsicModelRuntime::builtin(config)?)))
    }

    #[must_use]
    pub fn with_media_resolver(mut self, resolver: Arc<dyn crate::ModelMediaResolver>) -> Self {
        self.media_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub fn runtime(&self) -> &Arc<IntrinsicModelRuntime> {
        &self.runtime
    }

    fn prompt(
        input: &PlannerInput,
    ) -> Option<(String, Option<crate::MessageId>, crate::ConversationId)> {
        let WorldEventKind::MessageReceived(message) = input.event.kind() else {
            return None;
        };
        if !message.visible_reply_allowed || message.stop_requested {
            return None;
        }
        let text = message.content.as_text().trim();
        let has_image = message
            .content
            .attachments()
            .iter()
            .any(|attachment| attachment.kind() == AttachmentKind::Image);
        if (text.is_empty() && !has_image) || text.starts_with('#') {
            return None;
        }
        let prompt = if text.is_empty() {
            "用户发送了一张图片，请基于图片给出简短、谨慎的回应。".to_owned()
        } else {
            format!("当前消息：{}", text)
        };
        Some((prompt, message.reply_to, message.conversation_id))
    }

    async fn generate(
        &self,
        input: &PlannerInput,
        prompt: String,
    ) -> Result<IntrinsicInferenceOutput, IntrinsicInferenceError> {
        let config = self.runtime.config();
        let message = match input.event.kind() {
            WorldEventKind::MessageReceived(message) => Some(message),
            _ => None,
        };
        if let Some(message) = message {
            let image_count = message
                .content
                .attachments()
                .iter()
                .filter(|attachment| attachment.kind() == AttachmentKind::Image)
                .count();
            if image_count > config.media.max_images_per_turn {
                return Err(IntrinsicInferenceError::Media(
                    crate::ModelMediaError::TooManyImages {
                        count: image_count,
                        maximum: config.media.max_images_per_turn,
                    },
                ));
            }
        }
        if let Some(message) = message
            && let Some(attachment) = message
                .content
                .attachments()
                .iter()
                .find(|attachment| attachment.kind() == AttachmentKind::Image)
        {
            // An image-bearing request must remain a vision request. Falling
            // through to text inference would produce a plausible-looking
            // answer to a different input whenever the resolver or vision
            // engine is unavailable.
            let resolver = self.media_resolver.as_ref().ok_or_else(|| {
                IntrinsicInferenceError::Media(crate::ModelMediaError::ResolverFailed {
                    message: "no host media resolver is configured".to_owned(),
                })
            })?;
            let image = resolver.resolve_image(attachment).await?;
            return self
                .runtime
                .infer_vision(VisionInferenceRequest {
                    prompt,
                    image,
                    max_context_tokens: config.max_context_tokens,
                    max_new_tokens: config.max_new_tokens,
                })
                .await;
        }
        self.runtime
            .infer_text(TextInferenceRequest {
                prompt,
                max_context_tokens: config.max_context_tokens,
                max_new_tokens: config.max_new_tokens,
            })
            .await
    }
}

impl ModelBackend for IntrinsicModelBackend {
    fn plan<'a>(&'a self, input: &'a PlannerInput) -> ModelBackendFuture<'a> {
        Box::pin(async move {
            let Some((prompt, reply_to, conversation_id)) = Self::prompt(input) else {
                return Ok(PlannerOutput::silent());
            };
            let output = self
                .generate(input, prompt)
                .await
                .map_err(|error| match error {
                    IntrinsicInferenceError::Unavailable
                    | IntrinsicInferenceError::QueueTimeout => ModelBackendError::Unavailable,
                    other => ModelBackendError::failed(other.to_string(), other.retryable()),
                })?;
            let text = output.text.trim();
            if text.is_empty() || is_silent_marker(text) {
                return Ok(PlannerOutput::silent());
            }
            let content = MessageContent::text(text.to_owned());
            content
                .validate()
                .map_err(|error| ModelBackendError::InvalidPlan {
                    reason: error.to_string(),
                })?;
            Ok(PlannerOutput {
                disposition: DecisionDisposition::Reply,
                intents: vec![CognitiveIntent::respond_to(
                    conversation_id,
                    content,
                    reply_to,
                )],
                state_updates: Vec::new(),
            })
        })
    }
}

fn is_silent_marker(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    matches!(normalized.as_str(), "silent" | "[silent]" | "no_reply")
        || normalized.contains("\"disposition\":\"silent\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Attachment, ConversationKind, MessageReceivedEvent, ModelHealth, ModelMediaFuture,
        ModelMediaResolver, PersonId, PlannerStateSnapshot, ResolvedImage, WorldEvent,
    };
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct TrackingEngine {
        text_calls: AtomicUsize,
        vision_calls: AtomicUsize,
        vision_result: Result<IntrinsicInferenceOutput, IntrinsicInferenceError>,
    }

    impl TrackingEngine {
        fn new(vision_result: Result<IntrinsicInferenceOutput, IntrinsicInferenceError>) -> Self {
            Self {
                text_calls: AtomicUsize::new(0),
                vision_calls: AtomicUsize::new(0),
                vision_result,
            }
        }
    }

    impl IntrinsicInferenceEngine for TrackingEngine {
        fn health(&self) -> ModelHealth {
            ModelHealth::Healthy
        }

        fn version(&self) -> crate::IntrinsicModelVersion {
            crate::IntrinsicModelVersion::new("test-intrinsic", "v1", "test")
                .expect("test Intrinsic version should be valid")
        }

        fn infer_text<'a>(
            &'a self,
            _request: &'a TextInferenceRequest,
        ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
            self.text_calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async {
                Ok(IntrinsicInferenceOutput {
                    text: "text response".to_owned(),
                    generated_tokens: 1,
                })
            })
        }

        fn infer_vision<'a>(
            &'a self,
            _request: &'a VisionInferenceRequest,
        ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
            self.vision_calls.fetch_add(1, Ordering::Relaxed);
            let result = self.vision_result.clone();
            Box::pin(async move { result })
        }
    }

    #[derive(Debug)]
    struct FixedResolver {
        calls: AtomicUsize,
        result: Result<ResolvedImage, crate::ModelMediaError>,
    }

    impl FixedResolver {
        fn image() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Ok(ResolvedImage {
                    bytes: Arc::<[u8]>::from(vec![1_u8]),
                    media_type: Some("image/png".to_owned()),
                    width: 1,
                    height: 1,
                }),
            }
        }
    }

    impl ModelMediaResolver for FixedResolver {
        fn resolve_image<'a>(&'a self, _attachment: &'a Attachment) -> ModelMediaFuture<'a> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    fn image_input() -> PlannerInput {
        let attachment = Attachment::new(AttachmentKind::Image, "asset:photo")
            .expect("image attachment should be valid");
        let content = MessageContent::text("请看看这张图片")
            .with_attachments(vec![attachment])
            .expect("image message content should be valid");
        PlannerInput::new(
            WorldEvent::message_received(
                crate::EventPriority::High,
                MessageReceivedEvent {
                    message_id: crate::MessageId::new(),
                    conversation_id: crate::ConversationId::new(),
                    sender: PersonId::new(),
                    content,
                    reply_to: None,
                    timestamp: Utc::now(),
                    conversation_kind: ConversationKind::Direct,
                    addressed_to_agent: true,
                    replies_to_agent: false,
                    stop_requested: false,
                    explicit_request: true,
                    visible_reply_allowed: true,
                },
            ),
            PlannerStateSnapshot::empty(),
        )
    }

    fn backend(engine: Arc<TrackingEngine>) -> IntrinsicModelBackend {
        let runtime = IntrinsicModelRuntime::new(engine, IntrinsicRuntimeConfig::default())
            .expect("test runtime should be valid");
        IntrinsicModelBackend::new(Arc::new(runtime))
    }

    #[tokio::test]
    async fn image_without_resolver_never_calls_text_inference() {
        let engine = Arc::new(TrackingEngine::new(Ok(IntrinsicInferenceOutput {
            text: "vision response".to_owned(),
            generated_tokens: 1,
        })));
        let backend = backend(Arc::clone(&engine));
        let input = image_input();

        let result = backend.generate(&input, "inspect image".to_owned()).await;

        assert!(matches!(
            result,
            Err(IntrinsicInferenceError::Media(
                crate::ModelMediaError::ResolverFailed { .. }
            ))
        ));
        assert_eq!(engine.text_calls.load(Ordering::Relaxed), 0);
        assert_eq!(engine.vision_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn failed_vision_inference_never_falls_through_to_text() {
        let engine = Arc::new(TrackingEngine::new(Err(IntrinsicInferenceError::Engine {
            message: "vision unavailable".to_owned(),
            retryable: true,
        })));
        let resolver = Arc::new(FixedResolver::image());
        let backend = backend(Arc::clone(&engine)).with_media_resolver(resolver.clone());
        let input = image_input();

        let result = backend.generate(&input, "inspect image".to_owned()).await;

        assert!(matches!(
            result,
            Err(IntrinsicInferenceError::Engine {
                retryable: true,
                ..
            })
        ));
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 1);
        assert_eq!(engine.text_calls.load(Ordering::Relaxed), 0);
        assert_eq!(engine.vision_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn successful_image_inference_uses_vision_only() {
        let engine = Arc::new(TrackingEngine::new(Ok(IntrinsicInferenceOutput {
            text: "vision response".to_owned(),
            generated_tokens: 1,
        })));
        let resolver = Arc::new(FixedResolver::image());
        let backend = backend(Arc::clone(&engine)).with_media_resolver(resolver.clone());
        let input = image_input();

        let output = backend
            .generate(&input, "inspect image".to_owned())
            .await
            .expect("vision inference should succeed");

        assert_eq!(output.text, "vision response");
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 1);
        assert_eq!(engine.text_calls.load(Ordering::Relaxed), 0);
        assert_eq!(engine.vision_calls.load(Ordering::Relaxed), 1);
    }
}
