//! Rust-native CPU inference for the Thinker shipped in `minimind-3o`.
//!
//! The upstream checkpoint is an Omni checkpoint. This module loads its
//! decoder-only language Thinker and, when the verified `vision/` assets are
//! present, its fixed-resolution SigLIP2 vision encoder. Audio remains outside
//! the V3 runtime boundary.

use super::completion::InputCompletion;
use super::runtime::{
    IntrinsicGenerationControl, IntrinsicInferenceEngine, IntrinsicInferenceError,
    IntrinsicInferenceFuture, IntrinsicInferenceOutput, IntrinsicTokenCallback,
    TextInferenceRequest, VisionInferenceRequest,
};
use super::vision::SiglipVisionEncoder;
use crate::model::{IntrinsicModelVersion, ModelHealth};
use candle_core::{DType, Device, Tensor, pickle::PthTensors};
use serde::Deserialize;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokenizers::Tokenizer;

const DEFAULT_BOS_TOKEN_ID: u32 = 1;
const DEFAULT_EOS_TOKEN_ID: u32 = 2;
const DEFAULT_IMAGE_TOKEN_ID: u32 = 12;
const DEFAULT_IMAGE_TOKEN_LEN: usize = 64;
const DEFAULT_IMAGE_SPECIAL_TOKEN: &str = "<|image_pad|>";

#[derive(Debug, Deserialize)]
struct MiniMindConfigFile {
    #[serde(default = "default_hidden_size")]
    hidden_size: usize,
    #[serde(default = "default_num_hidden_layers")]
    num_hidden_layers: usize,
    #[serde(default = "default_num_attention_heads")]
    num_attention_heads: usize,
    #[serde(default = "default_num_key_value_heads")]
    num_key_value_heads: usize,
    #[serde(default = "default_head_dim")]
    head_dim: usize,
    #[serde(default = "default_intermediate_size")]
    intermediate_size: usize,
    #[serde(default = "default_vocab_size")]
    vocab_size: usize,
    #[serde(default = "default_rms_norm_eps")]
    rms_norm_eps: f64,
    #[serde(default = "default_rope_theta")]
    rope_theta: f64,
    #[serde(default = "default_max_position_embeddings")]
    max_position_embeddings: usize,
    #[serde(default)]
    use_moe: bool,
    #[serde(default = "default_bos_token_id")]
    bos_token_id: u32,
    #[serde(default = "default_eos_token_id")]
    eos_token_id: u32,
    #[serde(default = "default_image_ids")]
    image_ids: Vec<u32>,
    #[serde(default = "default_image_token_len")]
    image_token_len: usize,
    #[serde(default = "default_image_special_token")]
    image_special_token: String,
}

const fn default_hidden_size() -> usize {
    768
}

const fn default_num_hidden_layers() -> usize {
    8
}

const fn default_num_attention_heads() -> usize {
    8
}

const fn default_num_key_value_heads() -> usize {
    4
}

const fn default_head_dim() -> usize {
    96
}

const fn default_intermediate_size() -> usize {
    2_432
}

const fn default_vocab_size() -> usize {
    6_400
}

const fn default_rms_norm_eps() -> f64 {
    1e-6
}

const fn default_rope_theta() -> f64 {
    1_000_000.0
}

const fn default_max_position_embeddings() -> usize {
    32_768
}

const fn default_bos_token_id() -> u32 {
    DEFAULT_BOS_TOKEN_ID
}

const fn default_eos_token_id() -> u32 {
    DEFAULT_EOS_TOKEN_ID
}

fn default_image_ids() -> Vec<u32> {
    vec![DEFAULT_IMAGE_TOKEN_ID]
}

const fn default_image_token_len() -> usize {
    DEFAULT_IMAGE_TOKEN_LEN
}

fn default_image_special_token() -> String {
    DEFAULT_IMAGE_SPECIAL_TOKEN.to_owned()
}

#[derive(Clone)]
struct MiniMindLayer {
    input_layernorm: Tensor,
    post_attention_layernorm: Tensor,
    q_proj: Tensor,
    k_proj: Tensor,
    v_proj: Tensor,
    o_proj: Tensor,
    q_norm: Tensor,
    k_norm: Tensor,
    gate_proj: Tensor,
    down_proj: Tensor,
    up_proj: Tensor,
}

#[derive(Clone)]
struct KvCache {
    key: Tensor,
    value: Tensor,
}

struct MiniMindInner {
    version: IntrinsicModelVersion,
    tokenizer: Tokenizer,
    device: Device,
    embed_tokens: Tensor,
    layers: Vec<MiniMindLayer>,
    norm: Tensor,
    lm_head: Tensor,
    rope_cos: Tensor,
    rope_sin: Tensor,
    hidden_size: usize,
    num_attention_heads: usize,
    num_key_value_heads: usize,
    head_dim: usize,
    rms_norm_eps: f64,
    context_limit: usize,
    bos_token_id: u32,
    eos_token_id: u32,
    image_token_id: u32,
    image_token_len: usize,
    image_special_token: String,
    vision: Option<SiglipVisionEncoder>,
}

/// A loaded MiniMind language Thinker. Weight handles are reference-counted by
/// Candle, so cloning the outer engine does not duplicate the model in memory.
pub struct MiniMindEngine {
    inner: Arc<MiniMindInner>,
}

impl fmt::Debug for MiniMindEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MiniMindEngine")
            .field("version", &self.inner.version)
            .field("context_limit", &self.inner.context_limit)
            .field("layers", &self.inner.layers.len())
            .field("device", &self.inner.device)
            .finish()
    }
}

impl MiniMindEngine {
    /// Load the text Thinker from a verified model directory.
    pub fn load_from_dir(
        root: impl AsRef<Path>,
        version: IntrinsicModelVersion,
        manifest_context_limit: usize,
    ) -> Result<Self, String> {
        let root = root.as_ref();
        let config_path = root.join("config.json");
        let config_raw = std::fs::read_to_string(&config_path)
            .map_err(|error| format!("read {}: {error}", config_path.display()))?;
        let config: MiniMindConfigFile = serde_json::from_str(&config_raw)
            .map_err(|error| format!("parse {}: {error}", config_path.display()))?;
        validate_config(&config)?;

        let tokenizer_path = root.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|error| format!("load {}: {error}", tokenizer_path.display()))?;
        if tokenizer.get_vocab_size(true) < config.vocab_size {
            return Err(format!(
                "tokenizer vocabulary is smaller than checkpoint vocabulary: {} < {}",
                tokenizer.get_vocab_size(true),
                config.vocab_size
            ));
        }

        let weights_path = root.join("pytorch_model.bin");
        let tensors = PthTensors::new(&weights_path, None)
            .map_err(|error| format!("index {}: {error}", weights_path.display()))?;
        let device = Device::Cpu;
        let hidden_size = config.hidden_size;
        let q_size = config.num_attention_heads * config.head_dim;
        let kv_size = config.num_key_value_heads * config.head_dim;

        let embed_tokens = load_tensor(
            &tensors,
            "model.embed_tokens.weight",
            &[config.vocab_size, hidden_size],
            &device,
        )?;
        let norm = load_tensor(&tensors, "model.norm.weight", &[hidden_size], &device)?;
        let lm_head = load_linear(
            &tensors,
            "lm_head.weight",
            &[config.vocab_size, hidden_size],
            &device,
        )?;

        let mut layers = Vec::with_capacity(config.num_hidden_layers);
        for layer_index in 0..config.num_hidden_layers {
            let prefix = format!("model.layers.{layer_index}");
            layers.push(MiniMindLayer {
                input_layernorm: load_tensor(
                    &tensors,
                    &format!("{prefix}.input_layernorm.weight"),
                    &[hidden_size],
                    &device,
                )?,
                post_attention_layernorm: load_tensor(
                    &tensors,
                    &format!("{prefix}.post_attention_layernorm.weight"),
                    &[hidden_size],
                    &device,
                )?,
                q_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.self_attn.q_proj.weight"),
                    &[q_size, hidden_size],
                    &device,
                )?,
                k_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.self_attn.k_proj.weight"),
                    &[kv_size, hidden_size],
                    &device,
                )?,
                v_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.self_attn.v_proj.weight"),
                    &[kv_size, hidden_size],
                    &device,
                )?,
                o_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.self_attn.o_proj.weight"),
                    &[hidden_size, q_size],
                    &device,
                )?,
                q_norm: load_tensor(
                    &tensors,
                    &format!("{prefix}.self_attn.q_norm.weight"),
                    &[config.head_dim],
                    &device,
                )?,
                k_norm: load_tensor(
                    &tensors,
                    &format!("{prefix}.self_attn.k_norm.weight"),
                    &[config.head_dim],
                    &device,
                )?,
                gate_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.mlp.gate_proj.weight"),
                    &[config.intermediate_size, hidden_size],
                    &device,
                )?,
                down_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.mlp.down_proj.weight"),
                    &[hidden_size, config.intermediate_size],
                    &device,
                )?,
                up_proj: load_linear(
                    &tensors,
                    &format!("{prefix}.mlp.up_proj.weight"),
                    &[config.intermediate_size, hidden_size],
                    &device,
                )?,
            });
        }

        let context_limit = manifest_context_limit
            .max(1)
            .min(config.max_position_embeddings);
        let (rope_cos, rope_sin) =
            build_rope(context_limit, config.head_dim, config.rope_theta, &device)?;
        let vision_root = root.join("vision");
        let vision = if vision_root.exists() {
            Some(SiglipVisionEncoder::load(&vision_root, &tensors, &device)?)
        } else {
            None
        };
        let image_token_id = config
            .image_ids
            .first()
            .copied()
            .unwrap_or(DEFAULT_IMAGE_TOKEN_ID);
        if config.image_token_len == 0 {
            return Err("MiniMind image_token_len must be positive".to_owned());
        }

        Ok(Self {
            inner: Arc::new(MiniMindInner {
                version,
                tokenizer,
                device,
                embed_tokens,
                layers,
                norm,
                lm_head,
                rope_cos,
                rope_sin,
                hidden_size,
                num_attention_heads: config.num_attention_heads,
                num_key_value_heads: config.num_key_value_heads,
                head_dim: config.head_dim,
                rms_norm_eps: config.rms_norm_eps,
                context_limit,
                bos_token_id: config.bos_token_id,
                eos_token_id: config.eos_token_id,
                image_token_id,
                image_token_len: config.image_token_len,
                image_special_token: config.image_special_token,
                vision,
            }),
        })
    }

    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.inner.vision.is_some()
    }

    fn generation_future<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
        control: IntrinsicGenerationControl,
        on_token: Option<IntrinsicTokenCallback>,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        let inner = Arc::clone(&self.inner);
        let request = request.clone();
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || {
                inner.generate(&request, &control, on_token.as_ref())
            })
            .await
            .map_err(|error| IntrinsicInferenceError::Engine {
                message: format!("MiniMind worker stopped: {error}"),
                retryable: true,
            })?;
            match result {
                Ok(output) => Ok(output),
                Err(GenerateError::Cancelled) => Err(IntrinsicInferenceError::Cancelled),
                Err(GenerateError::Failed(message)) => Err(IntrinsicInferenceError::Engine {
                    message: format!("MiniMind inference failed: {message}"),
                    retryable: false,
                }),
            }
        })
    }

    fn classification_future<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, InputCompletion> {
        let inner = Arc::clone(&self.inner);
        let request = request.clone();
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || inner.classify(&request))
                .await
                .map_err(|error| IntrinsicInferenceError::Engine {
                    message: format!("MiniMind classifier worker stopped: {error}"),
                    retryable: true,
                })?;
            match result {
                Ok(completion) => Ok(completion),
                Err(GenerateError::Cancelled) => Err(IntrinsicInferenceError::Cancelled),
                Err(GenerateError::Failed(message)) => Err(IntrinsicInferenceError::Engine {
                    message: format!("MiniMind classifier failed: {message}"),
                    retryable: false,
                }),
            }
        })
    }

    fn vision_generation_future<'a>(
        &'a self,
        request: &'a VisionInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        let inner = Arc::clone(&self.inner);
        let request = request.clone();
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || {
                let control = IntrinsicGenerationControl::new();
                inner.generate_vision(&request, &control, None)
            })
            .await
            .map_err(|error| IntrinsicInferenceError::Engine {
                message: format!("MiniMind vision worker stopped: {error}"),
                retryable: true,
            })?;
            match result {
                Ok(output) => Ok(output),
                Err(GenerateError::Cancelled) => Err(IntrinsicInferenceError::Cancelled),
                Err(GenerateError::Failed(message)) => Err(IntrinsicInferenceError::Engine {
                    message: format!("MiniMind vision inference failed: {message}"),
                    retryable: false,
                }),
            }
        })
    }
}

impl IntrinsicInferenceEngine for MiniMindEngine {
    fn health(&self) -> ModelHealth {
        ModelHealth::Healthy
    }

    fn version(&self) -> IntrinsicModelVersion {
        self.inner.version.clone()
    }

    fn infer_text<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        self.generation_future(request, IntrinsicGenerationControl::new(), None)
    }

    fn infer_text_with_control<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
        control: IntrinsicGenerationControl,
        on_token: Option<IntrinsicTokenCallback>,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        self.generation_future(request, control, on_token)
    }

    fn classify_completion<'a>(
        &'a self,
        request: &'a TextInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, InputCompletion> {
        self.classification_future(request)
    }

    fn infer_vision<'a>(
        &'a self,
        request: &'a VisionInferenceRequest,
    ) -> IntrinsicInferenceFuture<'a, IntrinsicInferenceOutput> {
        if !self.supports_vision() {
            return Box::pin(async {
                Err(IntrinsicInferenceError::Unsupported {
                    capability: "vision".to_owned(),
                })
            });
        }
        self.vision_generation_future(request)
    }

    fn self_test<'a>(&'a self) -> IntrinsicInferenceFuture<'a, ()> {
        let request = TextInferenceRequest {
            prompt:
                "<|im_start|>user\n你好<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
                    .to_owned(),
            max_context_tokens: self.inner.context_limit.min(128),
            max_new_tokens: 4,
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let control = IntrinsicGenerationControl::new();
            let result =
                tokio::task::spawn_blocking(move || inner.generate(&request, &control, None))
                    .await
                    .map_err(|error| IntrinsicInferenceError::Engine {
                        message: format!("MiniMind self-test worker stopped: {error}"),
                        retryable: false,
                    })?;
            match result {
                Ok(_) => Ok(()),
                Err(GenerateError::Cancelled) => Err(IntrinsicInferenceError::Cancelled),
                Err(GenerateError::Failed(message)) => Err(IntrinsicInferenceError::Engine {
                    message: format!("MiniMind self-test failed: {message}"),
                    retryable: false,
                }),
            }
        })
    }
}

#[derive(Debug)]
enum GenerateError {
    Cancelled,
    Failed(String),
}

type GenerateResult<T> = Result<T, GenerateError>;

impl MiniMindInner {
    fn classify(&self, request: &TextInferenceRequest) -> GenerateResult<InputCompletion> {
        let control = IntrinsicGenerationControl::new();
        let generated = self.generate(request, &control, None)?;
        if let Some(completion) = generated_completion_label(&generated.text) {
            return Ok(completion);
        }

        let prompt_ids = self.prompt_ids(request)?;
        let complete = self
            .tokenizer
            .encode("完", false)
            .map_err(|error| GenerateError::Failed(format!("tokenize complete label: {error}")))?
            .get_ids()
            .to_vec();
        let incomplete = self
            .tokenizer
            .encode("未", false)
            .map_err(|error| GenerateError::Failed(format!("tokenize incomplete label: {error}")))?
            .get_ids()
            .to_vec();
        if complete.is_empty() || incomplete.is_empty() {
            return Err(GenerateError::Failed(
                "completion labels produced no tokens".to_owned(),
            ));
        }
        let (complete_score, incomplete_score) =
            self.score_pair(&prompt_ids, &complete, &incomplete, &control)?;
        Ok(if complete_score >= incomplete_score {
            InputCompletion::Complete
        } else {
            InputCompletion::Incomplete
        })
    }

    fn prompt_ids(&self, request: &TextInferenceRequest) -> GenerateResult<Vec<u32>> {
        let encoding = self
            .tokenizer
            .encode(request.prompt.as_str(), false)
            .map_err(|error| {
                GenerateError::Failed(format!("tokenize classifier prompt: {error}"))
            })?;
        let mut prompt_ids = encoding.get_ids().to_vec();
        if prompt_ids.is_empty() {
            prompt_ids.push(self.bos_token_id);
        }
        let context_limit = self.context_limit.min(request.max_context_tokens.max(1));
        if prompt_ids.len() >= context_limit {
            let keep = context_limit.saturating_sub(1).max(1);
            let start = prompt_ids.len().saturating_sub(keep);
            prompt_ids = prompt_ids[start..].to_vec();
        }
        Ok(prompt_ids)
    }

    fn score_candidate(
        &self,
        prompt_ids: &[u32],
        candidate_ids: &[u32],
        control: &IntrinsicGenerationControl,
    ) -> GenerateResult<f32> {
        check_cancelled(control)?;
        let mut caches = vec![None; self.layers.len()];
        let prompt_hidden = self.embed(prompt_ids)?;
        let mut logits = self.forward(prompt_hidden, 0, &mut caches, control)?;
        let mut score = 0.0_f32;
        for (index, token_id) in candidate_ids.iter().copied().enumerate() {
            check_cancelled(control)?;
            let token_logit = logits
                .narrow(0, token_id as usize, 1)
                .and_then(|value| value.squeeze(0))
                .and_then(|value| value.to_scalar::<f32>())
                .map_err(|error| GenerateError::Failed(format!("read label logit: {error}")))?;
            let log_normalizer = logits
                .log_sum_exp(0)
                .and_then(|value| value.to_scalar::<f32>())
                .map_err(|error| {
                    GenerateError::Failed(format!("normalize label logits: {error}"))
                })?;
            score += token_logit - log_normalizer;
            if index + 1 < candidate_ids.len() {
                let hidden = self.embed(&[token_id])?;
                logits = self.forward(hidden, prompt_ids.len() + index, &mut caches, control)?;
            }
        }
        Ok(score / candidate_ids.len() as f32)
    }

    fn score_pair(
        &self,
        prompt_ids: &[u32],
        first: &[u32],
        second: &[u32],
        control: &IntrinsicGenerationControl,
    ) -> GenerateResult<(f32, f32)> {
        if first.len() != 1 || second.len() != 1 {
            return Ok((
                self.score_candidate(prompt_ids, first, control)?,
                self.score_candidate(prompt_ids, second, control)?,
            ));
        }
        check_cancelled(control)?;
        let mut caches = vec![None; self.layers.len()];
        let prompt_hidden = self.embed(prompt_ids)?;
        let logits = self.forward(prompt_hidden, 0, &mut caches, control)?;
        let log_normalizer = logits
            .log_sum_exp(0)
            .and_then(|value| value.to_scalar::<f32>())
            .map_err(|error| GenerateError::Failed(format!("normalize label logits: {error}")))?;
        let score = |token_id: u32| -> GenerateResult<f32> {
            logits
                .narrow(0, token_id as usize, 1)
                .and_then(|value| value.squeeze(0))
                .and_then(|value| value.to_scalar::<f32>())
                .map(|value| value - log_normalizer)
                .map_err(|error| GenerateError::Failed(format!("read label logit: {error}")))
        };
        Ok((score(first[0])?, score(second[0])?))
    }

    fn generate(
        &self,
        request: &TextInferenceRequest,
        control: &IntrinsicGenerationControl,
        on_token: Option<&IntrinsicTokenCallback>,
    ) -> GenerateResult<IntrinsicInferenceOutput> {
        self.generate_with_vision(request, None, control, on_token)
    }

    fn generate_vision(
        &self,
        request: &VisionInferenceRequest,
        control: &IntrinsicGenerationControl,
        on_token: Option<&IntrinsicTokenCallback>,
    ) -> GenerateResult<IntrinsicInferenceOutput> {
        let vision = self.vision.as_ref().ok_or_else(|| {
            GenerateError::Failed("SigLIP vision encoder is not loaded".to_owned())
        })?;
        if vision.token_count() != self.image_token_len {
            return Err(GenerateError::Failed(format!(
                "vision token count {} does not match MiniMind image token count {}",
                vision.token_count(),
                self.image_token_len
            )));
        }
        let image_features = vision
            .encode(&request.image)
            .map_err(GenerateError::Failed)?;
        let prompt = format!(
            "{}\n{}",
            request.prompt.trim_end(),
            self.image_special_token.repeat(self.image_token_len)
        );
        let text_request = TextInferenceRequest {
            prompt,
            max_context_tokens: request.max_context_tokens,
            max_new_tokens: request.max_new_tokens,
        };
        self.generate_with_vision(&text_request, Some(&image_features), control, on_token)
    }

    fn generate_with_vision(
        &self,
        request: &TextInferenceRequest,
        image_features: Option<&Tensor>,
        control: &IntrinsicGenerationControl,
        on_token: Option<&IntrinsicTokenCallback>,
    ) -> GenerateResult<IntrinsicInferenceOutput> {
        check_cancelled(control)?;
        // Keep one position available for at least one generated token. The
        // prompt is truncated from the left so the newest conversational turn
        // remains visible when a caller supplies an overlong history.
        let context_limit = self.context_limit.min(request.max_context_tokens.max(1));
        let prompt_ids = self.prompt_ids(request)?;
        let generation_limit = request
            .max_new_tokens
            .min(context_limit.saturating_sub(prompt_ids.len()).max(1));

        let mut caches = vec![None; self.layers.len()];
        let prompt_hidden = if let Some(image_features) = image_features {
            self.embed_with_vision(&prompt_ids, image_features)?
        } else {
            self.embed(&prompt_ids)?
        };
        let mut logits = self.forward(prompt_hidden, 0, &mut caches, control)?;
        let prompt_len = prompt_ids.len();
        let mut generated_ids = Vec::with_capacity(generation_limit);
        let mut generated_tokens = 0_usize;
        let mut last_visible = String::new();

        for _ in 0..generation_limit {
            check_cancelled(control)?;
            let next_token = logits
                .argmax(0)
                .and_then(|tensor| tensor.to_scalar::<u32>())
                .map_err(|error| GenerateError::Failed(format!("select next token: {error}")))?;
            generated_tokens = generated_tokens.saturating_add(1);
            if next_token == self.eos_token_id || next_token == DEFAULT_EOS_TOKEN_ID {
                break;
            }
            generated_ids.push(next_token);
            let raw_text = self
                .tokenizer
                .decode(&generated_ids, true)
                .map_err(|error| GenerateError::Failed(format!("decode token: {error}")))?;
            let visible = clean_generated_text(&raw_text);
            if visible != last_visible {
                if let Some(callback) = on_token {
                    callback(visible.clone());
                }
                last_visible = visible;
            }

            if generated_tokens >= generation_limit {
                break;
            }
            let token_hidden = self.embed(&[next_token])?;
            let position = prompt_len + generated_ids.len() - 1;
            logits = self.forward(token_hidden, position, &mut caches, control)?;
        }

        check_cancelled(control)?;
        let raw_text = self
            .tokenizer
            .decode(&generated_ids, true)
            .map_err(|error| GenerateError::Failed(format!("decode output: {error}")))?;
        let text = clean_generated_text(&raw_text);
        if let Some(callback) = on_token
            && text != last_visible
        {
            callback(text.clone());
        }
        Ok(IntrinsicInferenceOutput {
            text,
            generated_tokens,
        })
    }

    fn embed(&self, ids: &[u32]) -> GenerateResult<Tensor> {
        let ids = Tensor::from_vec(ids.to_vec(), (ids.len(),), &self.device)
            .map_err(|error| GenerateError::Failed(format!("create token tensor: {error}")))?;
        self.embed_tokens
            .index_select(&ids, 0)
            .map_err(|error| GenerateError::Failed(format!("embedding lookup: {error}")))
    }

    fn embed_with_vision(&self, ids: &[u32], image_features: &Tensor) -> GenerateResult<Tensor> {
        let hidden = self.embed(ids)?;
        if image_features.rank() != 2
            || image_features.dims().get(1).copied() != Some(self.hidden_size)
        {
            return Err(GenerateError::Failed(format!(
                "vision features have shape {:?}, expected [tokens, {}]",
                image_features.dims(),
                self.hidden_size
            )));
        }
        let feature_count = image_features
            .dim(0)
            .map_err(|error| GenerateError::Failed(format!("read vision token count: {error}")))?;
        let mut chunks = Vec::new();
        let mut index = 0;
        let mut injected = false;
        while index < ids.len() {
            if ids[index] != self.image_token_id {
                let start = index;
                while index < ids.len() && ids[index] != self.image_token_id {
                    index += 1;
                }
                chunks.push(hidden.narrow(0, start, index - start).map_err(|error| {
                    GenerateError::Failed(format!("slice text embeddings: {error}"))
                })?);
                continue;
            }
            let start = index;
            while index < ids.len() && ids[index] == self.image_token_id {
                index += 1;
            }
            let marker_count = index - start;
            if !injected {
                if marker_count != feature_count {
                    return Err(GenerateError::Failed(format!(
                        "image marker count {marker_count} does not match vision token count {feature_count}"
                    )));
                }
                chunks.push(image_features.clone());
                injected = true;
            } else {
                chunks.push(hidden.narrow(0, start, marker_count).map_err(|error| {
                    GenerateError::Failed(format!("slice repeated image markers: {error}"))
                })?);
            }
        }
        if !injected {
            return Err(GenerateError::Failed(
                "vision request contains no image marker tokens".to_owned(),
            ));
        }
        Tensor::cat(&chunks, 0)
            .map_err(|error| GenerateError::Failed(format!("inject vision embeddings: {error}")))
    }

    fn forward(
        &self,
        mut hidden: Tensor,
        position: usize,
        caches: &mut [Option<KvCache>],
        control: &IntrinsicGenerationControl,
    ) -> GenerateResult<Tensor> {
        let sequence_length = hidden
            .dim(0)
            .map_err(|error| GenerateError::Failed(format!("read hidden shape: {error}")))?;
        if position.saturating_add(sequence_length) > self.context_limit {
            return Err(GenerateError::Failed(
                "sequence exceeds the configured context limit".to_owned(),
            ));
        }
        let cos = self
            .rope_cos
            .narrow(0, position, sequence_length)
            .map_err(|error| GenerateError::Failed(format!("slice RoPE cosine: {error}")))?;
        let sin = self
            .rope_sin
            .narrow(0, position, sequence_length)
            .map_err(|error| GenerateError::Failed(format!("slice RoPE sine: {error}")))?;

        for (layer_index, layer) in self.layers.iter().enumerate() {
            check_cancelled(control)?;
            let residual = hidden.clone();
            let normalized = rms_norm(&hidden, &layer.input_layernorm, self.rms_norm_eps)?;
            let q = linear(&normalized, &layer.q_proj)?
                .reshape((sequence_length, self.num_attention_heads, self.head_dim))
                .map_err(|error| GenerateError::Failed(format!("reshape query: {error}")))?
                .transpose(0, 1)
                .map_err(|error| GenerateError::Failed(format!("transpose query: {error}")))?;
            let k = linear(&normalized, &layer.k_proj)?
                .reshape((sequence_length, self.num_key_value_heads, self.head_dim))
                .map_err(|error| GenerateError::Failed(format!("reshape key: {error}")))?
                .transpose(0, 1)
                .map_err(|error| GenerateError::Failed(format!("transpose key: {error}")))?;
            let v = linear(&normalized, &layer.v_proj)?
                .reshape((sequence_length, self.num_key_value_heads, self.head_dim))
                .map_err(|error| GenerateError::Failed(format!("reshape value: {error}")))?
                .transpose(0, 1)
                .map_err(|error| GenerateError::Failed(format!("transpose value: {error}")))?;
            let q = rms_norm(&q, &layer.q_norm, self.rms_norm_eps)?;
            let k = rms_norm(&k, &layer.k_norm, self.rms_norm_eps)?;
            let q = apply_rope(&q, &cos, &sin)?;
            let k = apply_rope(&k, &cos, &sin)?;

            let previous = caches[layer_index].clone();
            let (key, value) = if let Some(previous) = previous {
                (
                    Tensor::cat(&[previous.key, k], 1).map_err(|error| {
                        GenerateError::Failed(format!("append key cache: {error}"))
                    })?,
                    Tensor::cat(&[previous.value, v], 1).map_err(|error| {
                        GenerateError::Failed(format!("append value cache: {error}"))
                    })?,
                )
            } else {
                (k, v)
            };
            caches[layer_index] = Some(KvCache {
                key: key.clone(),
                value: value.clone(),
            });

            let repeated_key =
                repeat_kv(&key, self.num_attention_heads / self.num_key_value_heads)?;
            let repeated_value =
                repeat_kv(&value, self.num_attention_heads / self.num_key_value_heads)?;
            let key_transposed = repeated_key.transpose(1, 2).map_err(|error| {
                GenerateError::Failed(format!("transpose attention key: {error}"))
            })?;
            let mut scores = q
                .matmul(&key_transposed)
                .map_err(|error| GenerateError::Failed(format!("attention matmul: {error}")))?;
            scores = (scores / (self.head_dim as f64).sqrt())
                .map_err(|error| GenerateError::Failed(format!("attention scale: {error}")))?;
            let total_length = key.dim(1).map_err(|error| {
                GenerateError::Failed(format!("read attention cache length: {error}"))
            })?;
            if sequence_length > 1 {
                let mask = causal_mask(sequence_length, total_length, position, &self.device)?;
                scores = scores
                    .broadcast_add(&mask)
                    .map_err(|error| GenerateError::Failed(format!("causal mask: {error}")))?;
            }
            let max = scores
                .max_keepdim(2)
                .map_err(|error| GenerateError::Failed(format!("attention max: {error}")))?;
            let probabilities = scores
                .broadcast_sub(&max)
                .and_then(|value| value.exp())
                .and_then(|value| value.sum_keepdim(2))
                .and_then(|sum| {
                    scores
                        .broadcast_sub(&max)
                        .and_then(|value| value.exp())
                        .and_then(|value| value.broadcast_div(&sum))
                })
                .map_err(|error| GenerateError::Failed(format!("attention softmax: {error}")))?;
            let attention = probabilities
                .matmul(&repeated_value)
                .map_err(|error| GenerateError::Failed(format!("attention value matmul: {error}")))?
                .transpose(0, 1)
                .map_err(|error| {
                    GenerateError::Failed(format!("transpose attention output: {error}"))
                })?
                .reshape((sequence_length, self.hidden_size))
                .map_err(|error| {
                    GenerateError::Failed(format!("reshape attention output: {error}"))
                })?;
            let attention = linear(&attention, &layer.o_proj)?;
            hidden = (residual + attention)
                .map_err(|error| GenerateError::Failed(format!("attention residual: {error}")))?;

            let residual = hidden.clone();
            let normalized = rms_norm(&hidden, &layer.post_attention_layernorm, self.rms_norm_eps)?;
            let gate = linear(&normalized, &layer.gate_proj)?;
            let up = linear(&normalized, &layer.up_proj)?;
            let activated = gate
                .silu()
                .map_err(|error| GenerateError::Failed(format!("SwiGLU activation: {error}")))?;
            let feed_forward = activated
                .mul(&up)
                .map_err(|error| GenerateError::Failed(format!("SwiGLU product: {error}")))?;
            let feed_forward = linear(&feed_forward, &layer.down_proj)?;
            hidden = (residual + feed_forward).map_err(|error| {
                GenerateError::Failed(format!("feed-forward residual: {error}"))
            })?;
        }

        check_cancelled(control)?;
        let hidden = rms_norm(&hidden, &self.norm, self.rms_norm_eps)?;
        let last = hidden
            .narrow(0, sequence_length.saturating_sub(1), 1)
            .map_err(|error| {
                GenerateError::Failed(format!("select final hidden state: {error}"))
            })?;
        linear(&last, &self.lm_head)?
            .squeeze(0)
            .map_err(|error| GenerateError::Failed(format!("squeeze logits: {error}")))
    }
}

fn validate_config(config: &MiniMindConfigFile) -> Result<(), String> {
    if config.use_moe {
        return Err("MiniMind MoE checkpoints are not supported by this loader".to_owned());
    }
    if config.hidden_size == 0
        || config.num_hidden_layers == 0
        || config.num_attention_heads == 0
        || config.num_key_value_heads == 0
        || config.head_dim == 0
        || config.intermediate_size == 0
        || config.vocab_size == 0
    {
        return Err("MiniMind config contains a zero dimension".to_owned());
    }
    if config.num_attention_heads % config.num_key_value_heads != 0 {
        return Err("attention heads are not divisible by key/value heads".to_owned());
    }
    if !config.rms_norm_eps.is_finite() || config.rms_norm_eps <= 0.0 {
        return Err("rms_norm_eps must be positive and finite".to_owned());
    }
    if !config.rope_theta.is_finite() || config.rope_theta <= 0.0 {
        return Err("rope_theta must be positive and finite".to_owned());
    }
    if config.max_position_embeddings == 0 {
        return Err("max_position_embeddings must be positive".to_owned());
    }
    if config.image_ids.is_empty()
        || config.image_token_len == 0
        || config.image_special_token.trim().is_empty()
    {
        return Err("MiniMind image token configuration is invalid".to_owned());
    }
    Ok(())
}

fn load_tensor(
    tensors: &PthTensors,
    name: &str,
    expected_shape: &[usize],
    device: &Device,
) -> Result<Tensor, String> {
    let tensor = tensors
        .get(name)
        .map_err(|error| format!("read tensor {name}: {error}"))?
        .ok_or_else(|| format!("checkpoint is missing tensor {name}"))?;
    if tensor.dims() != expected_shape {
        return Err(format!(
            "tensor {name} has shape {:?}, expected {:?}",
            tensor.dims(),
            expected_shape
        ));
    }
    tensor
        .to_dtype(DType::F32)
        .and_then(|tensor| tensor.to_device(device))
        .map_err(|error| format!("convert tensor {name}: {error}"))
}

fn load_linear(
    tensors: &PthTensors,
    name: &str,
    expected_shape: &[usize],
    device: &Device,
) -> Result<Tensor, String> {
    load_tensor(tensors, name, expected_shape, device)
        .and_then(|tensor| {
            tensor
                .transpose(0, 1)
                .map_err(|error| format!("transpose {name}: {error}"))
        })
        .and_then(|tensor| {
            tensor
                .contiguous()
                .map_err(|error| format!("contiguous {name}: {error}"))
        })
}

fn build_rope(
    context_limit: usize,
    head_dim: usize,
    rope_theta: f64,
    device: &Device,
) -> Result<(Tensor, Tensor), String> {
    let half = head_dim / 2;
    let mut cosine = Vec::with_capacity(context_limit * head_dim);
    let mut sine = Vec::with_capacity(context_limit * head_dim);
    for position in 0..context_limit {
        for index in 0..head_dim {
            let pair = index % half.max(1);
            let inverse_frequency = 1.0 / rope_theta.powf((2 * pair) as f64 / head_dim as f64);
            let angle = position as f64 * inverse_frequency;
            cosine.push(angle.cos() as f32);
            sine.push(angle.sin() as f32);
        }
    }
    let cosine = Tensor::from_vec(cosine, (context_limit, head_dim), device)
        .map_err(|error| format!("create RoPE cosine: {error}"))?;
    let sine = Tensor::from_vec(sine, (context_limit, head_dim), device)
        .map_err(|error| format!("create RoPE sine: {error}"))?;
    Ok((cosine, sine))
}

fn linear(input: &Tensor, weight_transposed: &Tensor) -> GenerateResult<Tensor> {
    input
        .matmul(weight_transposed)
        .map_err(|error| GenerateError::Failed(format!("linear projection: {error}")))
}

fn rms_norm(input: &Tensor, weight: &Tensor, epsilon: f64) -> GenerateResult<Tensor> {
    let variance = input
        .sqr()
        .and_then(|value| value.mean_keepdim(input.rank().saturating_sub(1)))
        .and_then(|value| (value + epsilon)?.sqrt())
        .map_err(|error| GenerateError::Failed(format!("RMSNorm variance: {error}")))?;
    input
        .broadcast_div(&variance)
        .and_then(|value| value.broadcast_mul(weight))
        .map_err(|error| GenerateError::Failed(format!("RMSNorm scale: {error}")))
}

fn apply_rope(input: &Tensor, cosine: &Tensor, sine: &Tensor) -> GenerateResult<Tensor> {
    let head_dim = input
        .dim(2)
        .map_err(|error| GenerateError::Failed(format!("read RoPE head dimension: {error}")))?;
    let half = head_dim / 2;
    let first = input
        .narrow(2, 0, half)
        .map_err(|error| GenerateError::Failed(format!("slice RoPE first half: {error}")))?;
    let second = input
        .narrow(2, half, head_dim - half)
        .map_err(|error| GenerateError::Failed(format!("slice RoPE second half: {error}")))?;
    let rotated = Tensor::cat(
        &[
            second
                .neg()
                .map_err(|error| GenerateError::Failed(format!("negate RoPE half: {error}")))?,
            first,
        ],
        2,
    )
    .map_err(|error| GenerateError::Failed(format!("join RoPE halves: {error}")))?;
    input
        .broadcast_mul(cosine)
        .and_then(|value| {
            rotated
                .broadcast_mul(sine)
                .and_then(|rotated| value.broadcast_add(&rotated))
        })
        .map_err(|error| GenerateError::Failed(format!("apply RoPE: {error}")))
}

fn repeat_kv(input: &Tensor, repeats: usize) -> GenerateResult<Tensor> {
    if repeats == 1 {
        return Ok(input.clone());
    }
    let heads = input
        .dim(0)
        .map_err(|error| GenerateError::Failed(format!("read KV head count: {error}")))?;
    let mut chunks = Vec::with_capacity(heads * repeats);
    for head in 0..heads {
        let chunk = input
            .narrow(0, head, 1)
            .map_err(|error| GenerateError::Failed(format!("slice KV head: {error}")))?;
        for _ in 0..repeats {
            chunks.push(chunk.clone());
        }
    }
    Tensor::cat(&chunks, 0)
        .map_err(|error| GenerateError::Failed(format!("repeat KV heads: {error}")))
}

fn causal_mask(
    query_length: usize,
    total_length: usize,
    query_start: usize,
    device: &Device,
) -> GenerateResult<Tensor> {
    let mut values = Vec::with_capacity(query_length * total_length);
    for query in 0..query_length {
        let absolute_query = query_start + query;
        for key in 0..total_length {
            values.push(if key > absolute_query { -1e9_f32 } else { 0.0 });
        }
    }
    Tensor::from_vec(values, (query_length, total_length), device)
        .map_err(|error| GenerateError::Failed(format!("create causal mask: {error}")))
}

fn check_cancelled(control: &IntrinsicGenerationControl) -> GenerateResult<()> {
    if control.is_cancelled() {
        Err(GenerateError::Cancelled)
    } else {
        Ok(())
    }
}

fn generated_completion_label(output: &str) -> Option<InputCompletion> {
    let output = output.trim_start();
    if output.starts_with('完') || output.starts_with("complete") {
        Some(InputCompletion::Complete)
    } else if output.starts_with('未')
        || output.starts_with("不完整")
        || output.starts_with("incomplete")
    {
        Some(InputCompletion::Incomplete)
    } else {
        None
    }
}

/// Remove model-internal reasoning and transport markers before the result
/// crosses the Core planner boundary.
fn clean_generated_text(raw: &str) -> String {
    let mut text = raw.to_owned();
    if let Some(start) = text.find("<think>") {
        if let Some(end_relative) = text[start..].find("</think>") {
            let end = start + end_relative + "</think>".len();
            text.replace_range(start..end, "");
        } else {
            text.truncate(start);
        }
    }
    if let Some(end) = text.find("</think>") {
        text.replace_range(..end + "</think>".len(), "");
    }
    for marker in ["<|im_end|>", "<|endoftext|>", "<|im_start|>"] {
        text = text.replace(marker, "");
    }
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{clean_generated_text, generated_completion_label, validate_config};
    use crate::model::{InputCompletion, IntrinsicAssetLoader, IntrinsicRuntimeConfig};

    #[test]
    fn generated_reasoning_and_transport_markers_are_hidden() {
        assert_eq!(
            clean_generated_text("<think>internal</think>\n你好<|im_end|>"),
            "你好"
        );
        assert_eq!(clean_generated_text("<think>unfinished"), "");
    }

    #[test]
    fn generated_completion_label_accepts_only_a_leading_known_label() {
        assert_eq!(
            generated_completion_label("未\n用户："),
            Some(InputCompletion::Incomplete)
        );
        assert_eq!(
            generated_completion_label("完整"),
            Some(InputCompletion::Complete)
        );
        assert_eq!(generated_completion_label("天气不错"), None);
    }

    #[test]
    fn checkpoint_shape_config_is_rejected_when_unsupported() {
        let raw = r#"{
            "hidden_size": 768,
            "num_hidden_layers": 8,
            "num_attention_heads": 8,
            "num_key_value_heads": 4,
            "head_dim": 96,
            "intermediate_size": 2432,
            "vocab_size": 6400,
            "rms_norm_eps": 0.000001,
            "rope_theta": 1000000.0,
            "max_position_embeddings": 32768,
            "use_moe": true
        }"#;
        let config: super::MiniMindConfigFile = serde_json::from_str(raw).unwrap();
        assert!(validate_config(&config).is_err());
    }

    #[test]
    #[ignore = "loads the bundled 0.1B language and 180MB vision checkpoints"]
    fn bundled_minimind_vision_smoke_test() {
        use crate::model::intrinsic::runtime::VisionInferenceRequest;
        use crate::model::media::ResolvedImage;
        use image::{ImageBuffer, ImageFormat, Rgb};
        use std::io::Cursor;
        use std::path::PathBuf;

        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../models/yunxi-intrinsic/minimind-3o");
        let bundle = IntrinsicAssetLoader
            .load_or_builtin(&root, IntrinsicRuntimeConfig::default())
            .expect("bundled manifest, MiniMind and SigLIP weights should load");
        assert!(bundle.report.supports_text);
        assert!(bundle.report.supports_vision);

        let bitmap = ImageBuffer::from_fn(64, 64, |x, y| {
            if (x / 8 + y / 8) % 2 == 0 {
                Rgb([240, 240, 240])
            } else {
                Rgb([30, 120, 200])
            }
        });
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(bitmap)
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("encode smoke-test image");
        let image = ResolvedImage::from_bytes(encoded.into_inner(), Some("image/png".to_owned()))
            .expect("smoke-test image should validate");
        let request = VisionInferenceRequest {
            prompt: "请简短描述图片".to_owned(),
            image,
            max_context_tokens: 128,
            max_new_tokens: 1,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create smoke-test runtime");
        let output = runtime
            .block_on(bundle.runtime.infer_vision(request))
            .expect("SigLIP plus MiniMind vision inference should succeed");
        assert!(output.generated_tokens <= 1);
    }
}
