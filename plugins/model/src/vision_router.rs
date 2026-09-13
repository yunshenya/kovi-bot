//! 可替换的图片理解 Provider 路由。
//!
//! 图片先由机器人本地物化并校验，再交给内置视觉接口或受控 MCP 服务。

use crate::config::{self, VisionConfig};
use crate::image_security::decode_validated_image_data_url;
use crate::model::ReplyTicket;
use crate::model::tool_access::tool_registry;
use crate::vision::{VisionImage, analyze_images_with_builtin, default_vision_prompt};
use anyhow::{Result, anyhow};
use kovi::serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 给单张图的结论加标题：多图时必须能分辨哪段是哪张，单图保持原文。
fn label_vision_analysis(index: usize, multi: bool, text: &str) -> String {
    if multi {
        format!("【图 {}】{}", index + 1, text)
    } else {
        text.to_owned()
    }
}

/// 用本机 Intrinsic 视觉模型分析图片。
///
/// 两件事在这里补齐（9-12 的线上巡检里它们占了全部 WARN/ERROR 的 20%）：
///
/// - **多图逐张分析再合并**：本机模型一次只吃一张图，旧实现直接放弃并报"仅支持单图"，
///   于是带图的 4 张消息永远失败。现在逐张分析、按 `【图 N】` 合并；
/// - **超像素图先缩放**：`max_pixels` 只是护栏（SigLIP 内部还会缩到固定尺寸），
///   旧实现在护栏前不缩放，8400 万像素的图必然失败。现在先缩放再推理。
///
/// 单张失败不再拖垮整轮：全部失败才返回 `Err(reason)`，并且 reason 是**具体原因**
/// （像素超限 / 解码失败 / 模型不支持），不再是笼统的"视觉模型不可用"。
async fn analyze_images_with_intrinsic(
    images: &[VisionImage],
    question: &str,
) -> Result<String, String> {
    let runtime = crate::yunxi::intrinsic_runtime::get()
        .ok_or_else(|| "本地 Intrinsic 模型未启用".to_owned())?;
    if !runtime.supports_vision() {
        return Err(format!(
            "模型不支持视觉（supports_vision=false，health={:?}）",
            runtime.health()
        ));
    }
    let config = runtime.runtime().config();
    let prompt = if question.trim().is_empty() {
        default_vision_prompt().to_string()
    } else {
        question.trim().to_string()
    };

    let selected: Vec<&VisionImage> = images.iter().take(MAX_ROUTED_VISION_IMAGES).collect();
    let multi = selected.len() > 1;
    let mut analyses: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (index, image) in selected.iter().enumerate() {
        let label = index + 1;
        let max_bytes = config.media.max_bytes;
        let max_pixels = config.media.max_pixels;
        let url = image.url.clone();
        let scaled = kovi::tokio::task::spawn_blocking(move || {
            crate::image_security::downscale_image_data_url_to_pixel_limit(
                &url, max_bytes, max_pixels,
            )
        })
        .await
        .map_err(|error| format!("图 {label} 缩放任务失败: {error}"))?;
        let url = match scaled {
            Ok(url) => url,
            Err(error) => {
                failures.push(format!("图 {label}: {error}"));
                continue;
            }
        };
        let resolved = match crate::yunxi::intrinsic_runtime::resolved_image_from_data_url(
            &url,
            config.media.max_bytes,
        ) {
            Ok(image) => image,
            Err(error) => {
                failures.push(format!("图 {label}: {error}"));
                continue;
            }
        };
        match runtime
            .infer_vision(yunxi_core::VisionInferenceRequest {
                prompt: prompt.clone(),
                image: resolved,
                max_context_tokens: config.max_context_tokens,
                max_new_tokens: config.max_new_tokens,
            })
            .await
        {
            Ok(output) => {
                let text = output.text.trim().to_owned();
                if text.is_empty() {
                    failures.push(format!("图 {label}: 模型没有给出内容"));
                } else {
                    analyses.push(label_vision_analysis(index, multi, &text));
                }
            }
            Err(error) => failures.push(format!("图 {label}: {error}")),
        }
    }

    if analyses.is_empty() {
        return Err(if failures.is_empty() {
            "没有可分析的图片".to_owned()
        } else {
            format!(
                "{} 张图全部分析失败（{}）",
                failures.len(),
                failures.join("；")
            )
        });
    }
    if !failures.is_empty() {
        eprintln!(
            "[WARN] Intrinsic 视觉部分失败（{} 张成功 / {} 张失败）: {}",
            analyses.len(),
            failures.len(),
            failures.join("；")
        );
    }
    Ok(analyses.join("\n"))
}

const MAX_VISION_QUESTION_CHARS: usize = 4_000;
const MAX_ROUTED_VISION_IMAGES: usize = 4;
const MAX_VISION_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_VISION_IMAGE_BYTES: usize = 20 * 1024 * 1024;
static TEMP_IMAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct VisionRouter {
    config: VisionConfig,
}

impl VisionRouter {
    pub(crate) fn from_config(config: VisionConfig) -> Self {
        Self { config }
    }

    pub(crate) async fn analyze(
        &self,
        images: &[VisionImage],
        question: &str,
        reply_ticket: Option<ReplyTicket>,
    ) -> Result<String> {
        if images.is_empty() {
            return Err(anyhow!("没有可供视觉 Provider 分析的图片"));
        }
        if images.len() > MAX_ROUTED_VISION_IMAGES {
            return Err(anyhow!("一次最多分析 {MAX_ROUTED_VISION_IMAGES} 张图片"));
        }
        let question = if question.trim().is_empty() {
            default_vision_prompt().to_string()
        } else {
            question.chars().take(MAX_VISION_QUESTION_CHARS).collect()
        };

        kovi::tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs()),
            self.analyze_provider(images, &question, reply_ticket),
        )
        .await
        .map_err(|_| anyhow!("视觉 Provider 调用超时"))?
    }

    async fn analyze_provider(
        &self,
        images: &[VisionImage],
        question: &str,
        reply_ticket: Option<ReplyTicket>,
    ) -> Result<String> {
        match self.config.provider() {
            // 本地 Intrinsic 视觉模型：不依赖外部端点，图片完全在本机推理。
            "intrinsic" => analyze_images_with_intrinsic(images, question)
                .await
                .map_err(|reason| anyhow!("本地 Intrinsic 视觉分析失败: {reason}")),
            "builtin" => match analyze_images_with_builtin(images, question).await {
                Ok(result) => Ok(result),
                Err(error) => {
                    eprintln!("[WARN] 内置视觉 Provider 失败: {}", error);
                    // Fall back to the local Intrinsic model so image understanding
                    // keeps working when the hosted provider is unavailable.
                    match analyze_images_with_intrinsic(images, question).await {
                        Ok(out) => Ok(out),
                        Err(reason) => {
                            eprintln!("[WARN] Intrinsic 视觉回退也失败: {reason}");
                            Err(error)
                        }
                    }
                }
            },
            "mcp" => self.analyze_with_mcp(images, question, reply_ticket).await,
            _ => self.analyze_auto(images, question, reply_ticket).await,
        }
    }

    async fn analyze_auto(
        &self,
        images: &[VisionImage],
        question: &str,
        reply_ticket: Option<ReplyTicket>,
    ) -> Result<String> {
        let mcp_configured = !self.config.mcp_server().trim().is_empty();
        let builtin_configured = std::env::var("VISION_API_URL")
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
            && std::env::var("VISION_MODEL_NAME")
                .ok()
                .is_some_and(|value| !value.trim().is_empty());

        let mut builtin_error = None;
        if builtin_configured {
            match analyze_images_with_builtin(images, question).await {
                Ok(result) => return Ok(result),
                Err(error) => {
                    eprintln!("[WARN] 内置视觉 Provider 失败: {}", error);
                    builtin_error = Some(error);
                }
            }
        }
        // Local Intrinsic fallback keeps image understanding working even when
        // the hosted vision endpoint is unavailable or out of balance.
        match analyze_images_with_intrinsic(images, question).await {
            Ok(out) => return Ok(out),
            Err(reason) => eprintln!("[WARN] Intrinsic 视觉回退失败: {reason}"),
        }
        if mcp_configured {
            return self.analyze_with_mcp(images, question, reply_ticket).await;
        }
        if let Some(error) = builtin_error {
            return Err(error);
        }
        analyze_images_with_builtin(images, question).await
    }

    async fn analyze_with_mcp(
        &self,
        images: &[VisionImage],
        question: &str,
        reply_ticket: Option<ReplyTicket>,
    ) -> Result<String> {
        let reply_ticket =
            reply_ticket.ok_or_else(|| anyhow!("MCP 视觉 Provider 需要回复会话上下文"))?;
        let server = self.config.mcp_server().trim();
        if server.is_empty() {
            return Err(anyhow!("未配置 vision.mcp_server"));
        }
        let registry = tool_registry().ok_or_else(|| anyhow!("MCP 工具注册表尚未就绪"))?;
        let images = images.to_vec();
        let files = kovi::tokio::task::spawn_blocking(move || TempVisionFiles::create(&images))
            .await
            .map_err(|error| anyhow!("创建视觉临时文件任务失败: {error}"))??;
        let arguments: Map<String, Value> = serde_json::from_value(json!({
            "question": question,
            "images": files
                .images
                .iter()
                .map(|image| json!({
                    "path": image.path,
                    "mime_type": image.mime_type,
                    "name": image.name,
                }))
                .collect::<Vec<_>>(),
        }))?;
        let tool_name = format!("mcp.{}.{}", server, self.config.mcp_tool());
        let result = registry
            .execute_mcp_for_vision(
                &tool_name,
                arguments,
                reply_ticket,
                Duration::from_secs(self.config.timeout_secs()),
            )
            .await?;
        let result = result.trim();
        if result.is_empty() {
            return Err(anyhow!("MCP 视觉 Provider 返回空结果"));
        }
        Ok(result.to_string())
    }
}

pub(crate) async fn analyze_images(
    images: &[VisionImage],
    question: &str,
    reply_ticket: Option<ReplyTicket>,
) -> Result<String> {
    VisionRouter::from_config(config::get().vision().clone())
        .analyze(images, question, reply_ticket)
        .await
}

struct TempVisionFiles {
    directory: PathBuf,
    images: Vec<TempVisionImage>,
}

struct TempVisionImage {
    path: String,
    mime_type: String,
    name: String,
}

impl TempVisionFiles {
    fn create(images: &[VisionImage]) -> Result<Self> {
        let counter = TEMP_IMAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "kovi-bot-vision-{}-{}",
            std::process::id(),
            counter
        ));
        fs::create_dir(&directory).map_err(|error| anyhow!("创建视觉临时目录失败: {error}"))?;
        if let Err(error) = set_private_permissions(&directory, 0o700) {
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }

        let entries = match write_temp_images(&directory, images) {
            Ok(entries) => entries,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        Ok(Self {
            directory,
            images: entries,
        })
    }
}

fn write_temp_images(directory: &Path, images: &[VisionImage]) -> Result<Vec<TempVisionImage>> {
    let mut entries = Vec::new();
    let mut total_bytes = 0usize;
    for (index, image) in images.iter().enumerate() {
        let remaining = MAX_TOTAL_VISION_IMAGE_BYTES.saturating_sub(total_bytes);
        if remaining == 0 {
            return Err(anyhow!(
                "MCP 视觉 Provider 图片总大小超过 {} MB 限制",
                MAX_TOTAL_VISION_IMAGE_BYTES / 1024 / 1024
            ));
        }
        let (mime_type, bytes) =
            decode_validated_image_data_url(&image.url, MAX_VISION_IMAGE_BYTES.min(remaining))?;
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_TOTAL_VISION_IMAGE_BYTES {
            return Err(anyhow!(
                "MCP 视觉 Provider 图片总大小超过 {} MB 限制",
                MAX_TOTAL_VISION_IMAGE_BYTES / 1024 / 1024
            ));
        }
        let extension = image_extension(&mime_type);
        let name = format!("image-{index}.{extension}");
        let path = directory.join(&name);
        fs::write(&path, bytes).map_err(|error| anyhow!("写入视觉临时图片失败: {error}"))?;
        set_private_permissions(&path, 0o600)?;
        entries.push(TempVisionImage {
            path: path.to_string_lossy().into_owned(),
            mime_type,
            name,
        });
    }
    Ok(entries)
}

impl Drop for TempVisionFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
fn decode_image_data_url(url: &str) -> Result<(String, Vec<u8>)> {
    decode_validated_image_data_url(url, MAX_VISION_IMAGE_BYTES)
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

fn set_private_permissions(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{VisionRouter, decode_image_data_url, image_extension, label_vision_analysis};
    use crate::config::VisionConfig;
    use crate::vision::VisionImage;
    use base64::Engine;

    #[test]
    fn decodes_supported_image_data_urls_for_mcp() {
        let png = b"\x89PNG\r\n\x1a\n";
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        );
        let (mime, bytes) = decode_image_data_url(&url).expect("图片 data URL 应能解码");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, png);
        assert_eq!(image_extension("image/jpeg"), "jpg");
    }

    #[test]
    fn rejects_unsupported_mcp_image_data_urls() {
        assert!(decode_image_data_url("data:image/gif;base64,aGVsbG8=").is_err());
        assert!(decode_image_data_url("data:image/png;base64,aGVsbG8=").is_err());
        assert!(decode_image_data_url("not-a-data-url").is_err());
    }

    #[test]
    fn router_can_be_constructed_from_default_config() {
        let config = VisionConfig::default();
        let router = VisionRouter::from_config(config);
        let _ = (
            router,
            VisionImage {
                url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
            },
        );
    }

    #[test]
    fn single_image_analysis_keeps_plain_text() {
        // 单图不加标题：下游按纯文本消费，标题会污染提示词。
        assert_eq!(label_vision_analysis(0, false, "一只猫"), "一只猫");
    }

    #[test]
    fn multiple_image_analyses_are_labelled_in_order() {
        let merged = [
            label_vision_analysis(0, true, "一只猫"),
            label_vision_analysis(1, true, "一条狗"),
        ]
        .join("\n");
        assert_eq!(merged, "【图 1】一只猫\n【图 2】一条狗");
    }

    #[test]
    fn router_rejects_more_than_four_images_before_calling_a_provider() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let router = VisionRouter::from_config(VisionConfig::default());
                let images = (0..5)
                    .map(|_| VisionImage {
                        url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                    })
                    .collect::<Vec<_>>();
                assert!(router.analyze(&images, "看看", None).await.is_err());
            });
    }
}
