//! Vision-specific bounded input helpers and the Rust/Candle SigLIP2 path.

use crate::model::media::{ModelMediaLimits, ResolvedImage};
use candle_core::{DType, Device, Tensor, pickle::PthTensors, safetensors::MmapedSafetensors};
use image::imageops::FilterType;
use serde::Deserialize;
use std::path::Path;

pub fn validate_vision_input(
    image: &ResolvedImage,
    limits: ModelMediaLimits,
) -> Result<(), crate::model::media::ModelMediaError> {
    image.validate(limits.max_bytes, limits.max_pixels)
}

const DEFAULT_LAYER_NORM_EPS: f64 = 1e-6;

#[derive(Debug, Deserialize)]
struct SiglipConfigFile {
    hidden_size: usize,
    image_size: usize,
    intermediate_size: usize,
    layer_norm_eps: f64,
    num_attention_heads: usize,
    num_channels: usize,
    num_hidden_layers: usize,
    patch_size: usize,
}

#[derive(Clone)]
struct SiglipVisionLayer {
    layer_norm1_weight: Tensor,
    layer_norm1_bias: Tensor,
    q_proj_weight: Tensor,
    q_proj_bias: Tensor,
    k_proj_weight: Tensor,
    k_proj_bias: Tensor,
    v_proj_weight: Tensor,
    v_proj_bias: Tensor,
    out_proj_weight: Tensor,
    out_proj_bias: Tensor,
    layer_norm2_weight: Tensor,
    layer_norm2_bias: Tensor,
    fc1_weight: Tensor,
    fc1_bias: Tensor,
    fc2_weight: Tensor,
    fc2_bias: Tensor,
}

#[derive(Clone)]
struct VisionProjector {
    layer_norm_weight: Tensor,
    layer_norm_bias: Tensor,
    input_weight: Tensor,
    input_bias: Tensor,
    output_weight: Tensor,
    output_bias: Tensor,
    epsilon: f64,
}

/// The fixed-resolution SigLIP2 vision encoder used by MiniMind-O.
///
/// The checkpoint contains a normal Hugging Face `SiglipVisionModel`, so the
/// implementation is intentionally explicit instead of depending on a second
/// model runtime. All tensors are kept in F32 on the same device as the text
/// model; this is slower than a GPU path but deterministic and bounded on CPU.
#[derive(Clone)]
pub(crate) struct SiglipVisionEncoder {
    image_size: usize,
    patch_size: usize,
    num_channels: usize,
    hidden_size: usize,
    num_attention_heads: usize,
    head_dim: usize,
    layer_norm_eps: f64,
    patch_weight: Tensor,
    patch_bias: Tensor,
    position_embedding: Tensor,
    layers: Vec<SiglipVisionLayer>,
    post_layer_norm_weight: Tensor,
    post_layer_norm_bias: Tensor,
    projector: VisionProjector,
    device: Device,
}

impl SiglipVisionEncoder {
    pub(crate) fn load(
        root: impl AsRef<Path>,
        language_tensors: &PthTensors,
        device: &Device,
    ) -> Result<Self, String> {
        let root = root.as_ref();
        let config_path = root.join("config.json");
        let config_raw = std::fs::read_to_string(&config_path)
            .map_err(|error| format!("read {}: {error}", config_path.display()))?;
        let config: SiglipConfigFile = serde_json::from_str(&config_raw)
            .map_err(|error| format!("parse {}: {error}", config_path.display()))?;
        validate_config(&config)?;

        let model_path = root.join("model.safetensors");
        // Mmap only the safetensor header and copy validated tensors into the
        // model device. The mapping can then be released after startup.
        let tensors = unsafe { MmapedSafetensors::new(&model_path) }
            .map_err(|error| format!("index {}: {error}", model_path.display()))?;
        let patch_flat_size = config
            .num_channels
            .checked_mul(config.patch_size)
            .and_then(|value| value.checked_mul(config.patch_size))
            .ok_or_else(|| "SigLIP patch dimension overflows".to_owned())?;
        let patch_count = (config.image_size / config.patch_size).pow(2);

        let patch_weight = load_safetensor(
            &tensors,
            "vision_model.embeddings.patch_embedding.weight",
            &[
                config.hidden_size,
                config.num_channels,
                config.patch_size,
                config.patch_size,
            ],
            device,
        )?
        .reshape((config.hidden_size, patch_flat_size))
        .and_then(|tensor| tensor.transpose(0, 1))
        .and_then(|tensor| tensor.contiguous())
        .map_err(|error| format!("prepare SigLIP patch embedding: {error}"))?;
        let patch_bias = load_safetensor(
            &tensors,
            "vision_model.embeddings.patch_embedding.bias",
            &[config.hidden_size],
            device,
        )?;
        let position_embedding = load_safetensor(
            &tensors,
            "vision_model.embeddings.position_embedding.weight",
            &[patch_count, config.hidden_size],
            device,
        )?;

        let mut layers = Vec::with_capacity(config.num_hidden_layers);
        for layer_index in 0..config.num_hidden_layers {
            let prefix = format!("vision_model.encoder.layers.{layer_index}");
            layers.push(SiglipVisionLayer {
                layer_norm1_weight: load_safetensor(
                    &tensors,
                    &format!("{prefix}.layer_norm1.weight"),
                    &[config.hidden_size],
                    device,
                )?,
                layer_norm1_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.layer_norm1.bias"),
                    &[config.hidden_size],
                    device,
                )?,
                q_proj_weight: load_linear_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.q_proj.weight"),
                    &[config.hidden_size, config.hidden_size],
                    device,
                )?,
                q_proj_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.q_proj.bias"),
                    &[config.hidden_size],
                    device,
                )?,
                k_proj_weight: load_linear_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.k_proj.weight"),
                    &[config.hidden_size, config.hidden_size],
                    device,
                )?,
                k_proj_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.k_proj.bias"),
                    &[config.hidden_size],
                    device,
                )?,
                v_proj_weight: load_linear_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.v_proj.weight"),
                    &[config.hidden_size, config.hidden_size],
                    device,
                )?,
                v_proj_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.v_proj.bias"),
                    &[config.hidden_size],
                    device,
                )?,
                out_proj_weight: load_linear_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.out_proj.weight"),
                    &[config.hidden_size, config.hidden_size],
                    device,
                )?,
                out_proj_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.self_attn.out_proj.bias"),
                    &[config.hidden_size],
                    device,
                )?,
                layer_norm2_weight: load_safetensor(
                    &tensors,
                    &format!("{prefix}.layer_norm2.weight"),
                    &[config.hidden_size],
                    device,
                )?,
                layer_norm2_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.layer_norm2.bias"),
                    &[config.hidden_size],
                    device,
                )?,
                fc1_weight: load_linear_safetensor(
                    &tensors,
                    &format!("{prefix}.mlp.fc1.weight"),
                    &[config.intermediate_size, config.hidden_size],
                    device,
                )?,
                fc1_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.mlp.fc1.bias"),
                    &[config.intermediate_size],
                    device,
                )?,
                fc2_weight: load_linear_safetensor(
                    &tensors,
                    &format!("{prefix}.mlp.fc2.weight"),
                    &[config.hidden_size, config.intermediate_size],
                    device,
                )?,
                fc2_bias: load_safetensor(
                    &tensors,
                    &format!("{prefix}.mlp.fc2.bias"),
                    &[config.hidden_size],
                    device,
                )?,
            });
        }

        let post_layer_norm_weight = load_safetensor(
            &tensors,
            "vision_model.post_layernorm.weight",
            &[config.hidden_size],
            device,
        )?;
        let post_layer_norm_bias = load_safetensor(
            &tensors,
            "vision_model.post_layernorm.bias",
            &[config.hidden_size],
            device,
        )?;
        let projector = VisionProjector::load(
            language_tensors,
            config.hidden_size,
            config.hidden_size,
            device,
        )?;

        Ok(Self {
            image_size: config.image_size,
            patch_size: config.patch_size,
            num_channels: config.num_channels,
            hidden_size: config.hidden_size,
            num_attention_heads: config.num_attention_heads,
            head_dim: config.hidden_size / config.num_attention_heads,
            layer_norm_eps: config.layer_norm_eps,
            patch_weight,
            patch_bias,
            position_embedding,
            layers,
            post_layer_norm_weight,
            post_layer_norm_bias,
            projector,
            device: device.clone(),
        })
    }

    pub(crate) fn token_count(&self) -> usize {
        (self.image_size / self.patch_size).pow(2)
    }

    pub(crate) fn encode(&self, image: &ResolvedImage) -> Result<Tensor, String> {
        let decoded = image::load_from_memory(&image.bytes)
            .map_err(|error| format!("decode image for SigLIP: {error}"))?;
        if decoded.width() != image.width || decoded.height() != image.height {
            return Err(format!(
                "resolved image dimensions {}x{} do not match decoded {}x{}",
                image.width,
                image.height,
                decoded.width(),
                decoded.height()
            ));
        }
        let resized = decoded
            .resize_exact(
                self.image_size as u32,
                self.image_size as u32,
                FilterType::CatmullRom,
            )
            .to_rgb8();
        let patches = self.patchify(&resized)?;
        let hidden = patches
            .matmul(&self.patch_weight)
            .and_then(|value| value.broadcast_add(&self.patch_bias))
            .and_then(|value| value.broadcast_add(&self.position_embedding))
            .map_err(|error| format!("SigLIP patch embedding: {error}"))?;
        let mut hidden = hidden;
        for layer in &self.layers {
            hidden = self.forward_layer(&hidden, layer)?;
        }
        let hidden = layer_norm(
            &hidden,
            &self.post_layer_norm_weight,
            &self.post_layer_norm_bias,
            self.layer_norm_eps,
        )?;
        self.projector.forward(&hidden)
    }

    fn patchify(&self, image: &image::RgbImage) -> Result<Tensor, String> {
        let patch_count_per_side = self.image_size / self.patch_size;
        let patch_values = self
            .token_count()
            .checked_mul(self.num_channels)
            .and_then(|value| value.checked_mul(self.patch_size))
            .and_then(|value| value.checked_mul(self.patch_size))
            .ok_or_else(|| "SigLIP patch buffer size overflows".to_owned())?;
        let mut values = Vec::with_capacity(patch_values);
        for patch_y in 0..patch_count_per_side {
            for patch_x in 0..patch_count_per_side {
                for channel in 0..self.num_channels {
                    for y in 0..self.patch_size {
                        for x in 0..self.patch_size {
                            let pixel = image.get_pixel(
                                (patch_x * self.patch_size + x) as u32,
                                (patch_y * self.patch_size + y) as u32,
                            );
                            // SigLIP's processor rescales [0, 255] to [0, 1]
                            // and normalizes with mean/std 0.5 for every channel.
                            values.push((f32::from(pixel[channel]) / 255.0 - 0.5) / 0.5);
                        }
                    }
                }
            }
        }
        Tensor::from_vec(
            values,
            (
                self.token_count(),
                self.num_channels * self.patch_size * self.patch_size,
            ),
            &self.device,
        )
        .map_err(|error| format!("create SigLIP patch tensor: {error}"))
    }

    fn forward_layer(&self, hidden: &Tensor, layer: &SiglipVisionLayer) -> Result<Tensor, String> {
        let sequence_length = hidden
            .dim(0)
            .map_err(|error| format!("read SigLIP sequence length: {error}"))?;
        let residual = hidden.clone();
        let normalized = layer_norm(
            hidden,
            &layer.layer_norm1_weight,
            &layer.layer_norm1_bias,
            self.layer_norm_eps,
        )?;
        let query = linear(&normalized, &layer.q_proj_weight, &layer.q_proj_bias)?
            .reshape((sequence_length, self.num_attention_heads, self.head_dim))
            .and_then(|value| value.transpose(0, 1))
            .map_err(|error| format!("reshape SigLIP query: {error}"))?;
        let key = linear(&normalized, &layer.k_proj_weight, &layer.k_proj_bias)?
            .reshape((sequence_length, self.num_attention_heads, self.head_dim))
            .and_then(|value| value.transpose(0, 1))
            .map_err(|error| format!("reshape SigLIP key: {error}"))?;
        let value = linear(&normalized, &layer.v_proj_weight, &layer.v_proj_bias)?
            .reshape((sequence_length, self.num_attention_heads, self.head_dim))
            .and_then(|value| value.transpose(0, 1))
            .map_err(|error| format!("reshape SigLIP value: {error}"))?;
        let scores = query
            .matmul(
                &key.transpose(1, 2)
                    .map_err(|error| format!("transpose SigLIP key: {error}"))?,
            )
            .and_then(|value| value / (self.head_dim as f64).sqrt())
            .map_err(|error| format!("SigLIP attention scores: {error}"))?;
        let probabilities = softmax(&scores, 2)?;
        let attention = probabilities
            .matmul(&value)
            .and_then(|value| value.transpose(0, 1))
            .and_then(|value| value.reshape((sequence_length, self.hidden_size)))
            .map_err(|error| format!("SigLIP attention output: {error}"))?;
        let attention = linear(&attention, &layer.out_proj_weight, &layer.out_proj_bias)?;
        let hidden = (residual + attention)
            .map_err(|error| format!("SigLIP attention residual: {error}"))?;

        let residual = hidden.clone();
        let normalized = layer_norm(
            &hidden,
            &layer.layer_norm2_weight,
            &layer.layer_norm2_bias,
            self.layer_norm_eps,
        )?;
        let feed_forward = linear(&normalized, &layer.fc1_weight, &layer.fc1_bias)?
            .gelu()
            .map_err(|error| format!("SigLIP GELU: {error}"))
            .and_then(|value| linear(&value, &layer.fc2_weight, &layer.fc2_bias))?;
        (residual + feed_forward).map_err(|error| format!("SigLIP feed-forward residual: {error}"))
    }
}

impl VisionProjector {
    fn load(
        tensors: &PthTensors,
        input_size: usize,
        output_size: usize,
        device: &Device,
    ) -> Result<Self, String> {
        Ok(Self {
            layer_norm_weight: load_pth_tensor(
                tensors,
                "vision_proj.mlp.0.weight",
                &[input_size],
                device,
            )?,
            layer_norm_bias: load_pth_tensor(
                tensors,
                "vision_proj.mlp.0.bias",
                &[input_size],
                device,
            )?,
            input_weight: load_linear_pth_tensor(
                tensors,
                "vision_proj.mlp.1.weight",
                &[output_size, input_size],
                device,
            )?,
            input_bias: load_pth_tensor(tensors, "vision_proj.mlp.1.bias", &[output_size], device)?,
            output_weight: load_linear_pth_tensor(
                tensors,
                "vision_proj.mlp.3.weight",
                &[output_size, output_size],
                device,
            )?,
            output_bias: load_pth_tensor(
                tensors,
                "vision_proj.mlp.3.bias",
                &[output_size],
                device,
            )?,
            epsilon: DEFAULT_LAYER_NORM_EPS,
        })
    }

    fn forward(&self, hidden: &Tensor) -> Result<Tensor, String> {
        let hidden = layer_norm(
            hidden,
            &self.layer_norm_weight,
            &self.layer_norm_bias,
            self.epsilon,
        )?;
        let hidden = linear(&hidden, &self.input_weight, &self.input_bias)?
            .gelu_erf()
            .map_err(|error| format!("MiniMind vision projector GELU: {error}"))?;
        linear(&hidden, &self.output_weight, &self.output_bias)
    }
}

fn validate_config(config: &SiglipConfigFile) -> Result<(), String> {
    if config.hidden_size == 0
        || config.image_size == 0
        || config.intermediate_size == 0
        || config.num_attention_heads == 0
        || config.num_channels == 0
        || config.num_hidden_layers == 0
        || config.patch_size == 0
    {
        return Err("SigLIP config contains a zero dimension".to_owned());
    }
    if config.num_channels != 3 {
        return Err(format!(
            "SigLIP image channel count {} is unsupported",
            config.num_channels
        ));
    }
    if !config.image_size.is_multiple_of(config.patch_size) {
        return Err("SigLIP image size must be divisible by patch size".to_owned());
    }
    if !config
        .hidden_size
        .is_multiple_of(config.num_attention_heads)
    {
        return Err("SigLIP hidden size is not divisible by attention heads".to_owned());
    }
    if !config.layer_norm_eps.is_finite() || config.layer_norm_eps <= 0.0 {
        return Err("SigLIP layer_norm_eps must be positive and finite".to_owned());
    }
    let patch_count = (config.image_size / config.patch_size).pow(2);
    if patch_count == 0 || patch_count > 4_096 {
        return Err("SigLIP patch count is outside the bounded runtime limit".to_owned());
    }
    Ok(())
}

fn load_safetensor(
    tensors: &MmapedSafetensors,
    name: &str,
    expected_shape: &[usize],
    device: &Device,
) -> Result<Tensor, String> {
    let tensor = tensors
        .load(name, device)
        .map_err(|error| format!("read tensor {name}: {error}"))?;
    if tensor.dims() != expected_shape {
        return Err(format!(
            "tensor {name} has shape {:?}, expected {:?}",
            tensor.dims(),
            expected_shape
        ));
    }
    let tensor = tensor
        .to_dtype(DType::F32)
        .map_err(|error| format!("convert tensor {name}: {error}"))?;
    tensor
        .contiguous()
        .map_err(|error| format!("contiguous tensor {name}: {error}"))
}

fn load_linear_safetensor(
    tensors: &MmapedSafetensors,
    name: &str,
    expected_shape: &[usize],
    device: &Device,
) -> Result<Tensor, String> {
    load_safetensor(tensors, name, expected_shape, device)
        .and_then(|value| {
            value
                .transpose(0, 1)
                .map_err(|error| format!("transpose tensor {name}: {error}"))
        })
        .and_then(|value| {
            value
                .contiguous()
                .map_err(|error| format!("contiguous tensor {name}: {error}"))
        })
        .map_err(|error| format!("prepare linear tensor {name}: {error}"))
}

fn load_pth_tensor(
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
    let tensor = tensor
        .to_dtype(DType::F32)
        .map_err(|error| format!("convert tensor {name}: {error}"))?;
    let tensor = tensor
        .to_device(device)
        .map_err(|error| format!("move tensor {name}: {error}"))?;
    tensor
        .contiguous()
        .map_err(|error| format!("contiguous tensor {name}: {error}"))
}

fn load_linear_pth_tensor(
    tensors: &PthTensors,
    name: &str,
    expected_shape: &[usize],
    device: &Device,
) -> Result<Tensor, String> {
    load_pth_tensor(tensors, name, expected_shape, device)
        .and_then(|value| {
            value
                .transpose(0, 1)
                .map_err(|error| format!("transpose tensor {name}: {error}"))
        })
        .and_then(|value| {
            value
                .contiguous()
                .map_err(|error| format!("contiguous tensor {name}: {error}"))
        })
        .map_err(|error| format!("prepare linear tensor {name}: {error}"))
}

fn linear(input: &Tensor, weight: &Tensor, bias: &Tensor) -> Result<Tensor, String> {
    input
        .matmul(weight)
        .and_then(|value| value.broadcast_add(bias))
        .map_err(|error| format!("linear projection: {error}"))
}

fn layer_norm(
    input: &Tensor,
    weight: &Tensor,
    bias: &Tensor,
    epsilon: f64,
) -> Result<Tensor, String> {
    let last_dimension = input.rank().saturating_sub(1);
    let mean = input
        .mean_keepdim(last_dimension)
        .and_then(|value| input.broadcast_sub(&value))
        .map_err(|error| format!("LayerNorm centering: {error}"))?;
    let variance = mean
        .sqr()
        .and_then(|value| value.mean_keepdim(last_dimension))
        .and_then(|value| (value + epsilon)?.sqrt())
        .map_err(|error| format!("LayerNorm variance: {error}"))?;
    mean.broadcast_div(&variance)
        .and_then(|value| value.broadcast_mul(weight))
        .and_then(|value| value.broadcast_add(bias))
        .map_err(|error| format!("LayerNorm scale: {error}"))
}

fn softmax(input: &Tensor, dimension: usize) -> Result<Tensor, String> {
    let maximum = input
        .max_keepdim(dimension)
        .and_then(|value| input.broadcast_sub(&value))
        .and_then(|value| value.exp())
        .map_err(|error| format!("softmax exponent: {error}"))?;
    let normalizer = maximum
        .sum_keepdim(dimension)
        .map_err(|error| format!("softmax normalizer: {error}"))?;
    maximum
        .broadcast_div(&normalizer)
        .map_err(|error| format!("softmax division: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{SiglipConfigFile, validate_config};

    #[test]
    fn siglip_config_accepts_bundled_shape() {
        let config: SiglipConfigFile = serde_json::from_str(
            r#"{
                "hidden_size": 768,
                "image_size": 256,
                "intermediate_size": 3072,
                "layer_norm_eps": 0.000001,
                "num_attention_heads": 12,
                "num_channels": 3,
                "num_hidden_layers": 12,
                "patch_size": 32
            }"#,
        )
        .expect("valid SigLIP config");
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn siglip_config_rejects_unbounded_patch_grid() {
        let config: SiglipConfigFile = serde_json::from_str(
            r#"{
                "hidden_size": 768,
                "image_size": 4096,
                "intermediate_size": 3072,
                "layer_norm_eps": 0.000001,
                "num_attention_heads": 12,
                "num_channels": 3,
                "num_hidden_layers": 12,
                "patch_size": 1
            }"#,
        )
        .expect("valid JSON");
        assert!(validate_config(&config).is_err());
    }
}
