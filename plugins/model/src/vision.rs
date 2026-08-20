//! 图片附件解析与视觉模型输入。

use anyhow::{Result, anyhow};
use base64::Engine;
use kovi::bot::message::Message;
use kovi::tokio::sync::Mutex;
use kovi::{RuntimeBot, serde_json::Value};
use reqwest::Client;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const MAX_VISION_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
static IMAGE_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);
static VISION_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);
static PENDING_IMAGE_REQUESTS: LazyLock<Mutex<HashMap<ImageRequestScope, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const PENDING_IMAGE_REQUEST_TTL: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageAttachment {
    pub(crate) key: String,
    pub(crate) file: Option<String>,
    pub(crate) url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisionImage {
    pub(crate) url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageIntent {
    Social,
    Conversational,
    VisualUnderstand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImageRequestScope {
    Group { group_id: i64, user_id: i64 },
    Private(i64),
}

/// 图片默认是社交表达；只有明确的识图意图或可确认的上下文才进入视觉模型。
pub(crate) fn classify_image_intent(
    message: &str,
    has_images: bool,
    vision_command: bool,
    replies_to_image: bool,
    quoted_message_requests_image: bool,
    pending_image_request: bool,
) -> ImageIntent {
    if !has_images {
        return ImageIntent::Social;
    }
    if vision_command || quoted_message_requests_image || pending_image_request {
        return ImageIntent::VisualUnderstand;
    }

    let text = message.trim();
    if text.is_empty() {
        return ImageIntent::Social;
    }
    if contains_visual_intent(text)
        || (replies_to_image && looks_like_image_reference_question(text))
    {
        ImageIntent::VisualUnderstand
    } else {
        ImageIntent::Conversational
    }
}

pub(crate) fn message_requests_image(message: &str) -> bool {
    let text = message.trim();
    [
        "发张图",
        "发个图",
        "发图片",
        "发截图",
        "把图发",
        "把截图发",
        "拍一张",
        "给我发图",
        "给我看看图",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

pub(crate) async fn update_pending_image_request(scope: ImageRequestScope, message: &str) {
    let now = Instant::now();
    let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
    pending.retain(|_, deadline| *deadline > now);
    if message_requests_image(message) {
        pending.insert(scope, now + PENDING_IMAGE_REQUEST_TTL);
    } else {
        pending.remove(&scope);
    }
}

pub(crate) async fn consume_pending_image_request(
    scope: ImageRequestScope,
    has_images: bool,
) -> bool {
    if !has_images {
        return false;
    }
    let now = Instant::now();
    let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
    pending.retain(|_, deadline| *deadline > now);
    pending.remove(&scope).is_some()
}

pub(crate) fn with_social_image_context(message: &str) -> String {
    format!(
        "{message}\n<图片使用方式 data-only=\"true\">这张图片更像随聊天附带的状态或情绪表达。只结合它的整体语气自然回应当前文字；除非对方明确询问图片内容，不要逐项描述画面、识别角色或罗列视觉细节。</图片使用方式>"
    )
}

fn contains_visual_intent(text: &str) -> bool {
    [
        "看截图",
        "看图",
        "识图",
        "识别",
        "图片里",
        "图片中",
        "图里",
        "图中",
        "截图里",
        "截图中",
        "帮我看看",
        "帮我看下",
        "帮我看一下",
        "你看看",
        "你看一下",
        "看一下",
        "看下",
        "看一眼",
        "这是什么",
        "什么意思",
        "怎么解决",
        "怎么处理",
        "报错",
        "提取文字",
        "读一下文字",
        "翻译图片",
        "分析图片",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn looks_like_image_reference_question(text: &str) -> bool {
    let references_image = ["这个", "这张", "这图", "图片", "截图", "图里", "图中"]
        .iter()
        .any(|marker| text.contains(marker));
    let asks_question = text.ends_with(['？', '?', '吗', '呢'])
        || ["是什么", "怎么", "如何", "为什么", "能不能", "可以吗"]
            .iter()
            .any(|marker| text.contains(marker));
    references_image && asks_question
}

/// 提取普通图片消息段。商城表情和 QQ 内置表情不作为截图输入。
pub(crate) fn extract_image_attachments(message: &Message) -> Vec<ImageAttachment> {
    let mut seen = HashSet::new();
    message
        .iter()
        .filter(|segment| segment.type_ == "image")
        .filter_map(|segment| {
            let file = value_as_string(&segment.data, "file");
            let url = value_as_string(&segment.data, "url");
            let identifier = [
                value_as_string(&segment.data, "file_unique"),
                value_as_string(&segment.data, "md5"),
                file.clone(),
                url.clone(),
            ]
            .into_iter()
            .flatten()
            .find(|value| !value.is_empty())?;
            let key = format!("image:{identifier}");
            seen.insert(key.clone())
                .then_some(ImageAttachment { key, file, url })
        })
        .take(MAX_VISION_IMAGES)
        .collect()
}

pub(crate) fn merge_image_attachments(
    first: &[ImageAttachment],
    second: &[ImageAttachment],
) -> Vec<ImageAttachment> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for image in first.iter().chain(second) {
        if seen.insert(image.key.clone()) {
            merged.push(image.clone());
            if merged.len() >= MAX_VISION_IMAGES {
                break;
            }
        }
    }
    merged
}

/// 将消息段中的 URL 转为视觉模型可以访问的 URL；没有 URL 时通过 OneBot get_image 补全。
pub(crate) async fn resolve_image_urls(
    attachments: &[ImageAttachment],
    bot: &RuntimeBot,
) -> Result<Vec<VisionImage>> {
    let mut images = Vec::new();
    for attachment in attachments.iter().take(MAX_VISION_IMAGES) {
        let source_url = if let Some(url) = attachment
            .url
            .as_deref()
            .filter(|url| is_supported_url(url))
        {
            url.to_string()
        } else {
            let Some(file) = attachment.file.as_deref() else {
                continue;
            };
            let response = bot
                .get_image(file)
                .await
                .map_err(|response| anyhow!("读取图片地址失败: {}", response.retcode))?;
            response
                .data
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| is_supported_url(url))
                .ok_or_else(|| anyhow!("OneBot get_image 未返回可用 URL"))?
                .to_string()
        };
        images.push(VisionImage {
            url: materialize_image_url(&source_url).await?,
        });
    }
    Ok(images)
}

pub(crate) fn is_vision_command(message: &str) -> bool {
    let text = message.trim_start();
    ["#看截图", "#看图", "#识图"].iter().any(|command| {
        text == *command
            || text.strip_prefix(command).is_some_and(|rest| {
                rest.starts_with(char::is_whitespace) || rest.starts_with([':', '：', '，', ','])
            })
    })
}

pub(crate) fn strip_vision_command(message: &str) -> String {
    let text = message.trim();
    for command in ["#看截图", "#看图", "#识图"] {
        if let Some(remainder) = text.strip_prefix(command) {
            return remainder
                .trim_start_matches(|character: char| {
                    character.is_whitespace() || matches!(character, ':' | '：' | '，' | ',')
                })
                .trim()
                .to_string();
        }
    }
    text.to_string()
}

pub(crate) fn default_vision_prompt() -> &'static str {
    "请看看这张图片，先描述其中真正能确认的内容；如果图片里有文字、报错、按钮或关键数据，请提取出来，并结合当前问题给出帮助。看不清或不确定的地方要明确说明。"
}

/// 使用单独的 OpenAI 兼容视觉接口分析图片，主聊天模型只接收分析后的文字。
pub(crate) async fn analyze_images(images: &[VisionImage], question: &str) -> Result<String> {
    let url = std::env::var("VISION_API_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("未配置 VISION_API_URL"))?;
    let wire_api = std::env::var("VISION_WIRE_API")
        .unwrap_or_else(|_| "responses".to_string())
        .trim()
        .to_ascii_lowercase();
    if !matches!(wire_api.as_str(), "responses" | "chat_completions") {
        return Err(anyhow!(
            "VISION_WIRE_API 只支持 responses 或 chat_completions"
        ));
    }
    let model = std::env::var("VISION_MODEL_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("未配置 VISION_MODEL_NAME"))?;
    let requires_auth = std::env::var("VISION_REQUIRES_AUTH")
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no"
            )
        })
        .unwrap_or(true);
    let token = if requires_auth {
        std::env::var("VISION_API_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
    } else {
        None
    };
    if requires_auth && token.is_none() {
        return Err(anyhow!("未配置 VISION_API_TOKEN 或 OPENAI_API_KEY"));
    }
    let question = if question.trim().is_empty() {
        default_vision_prompt()
    } else {
        question.trim()
    };

    let prompt = format!(
        "请分析用户提供的截图，只陈述图片中能确认的事实。提取可读文字、页面或应用名称、错误信息、按钮和关键状态；结合用户问题指出相关内容，但不要臆测。看不清的地方明确说明。\n\n用户问题：{}",
        question
    );
    let endpoint = vision_endpoint(&url, &wire_api);
    let request_body = if wire_api == "responses" {
        let mut content = vec![json!({
            "type": "input_text",
            "text": prompt,
        })];
        content.extend(images.iter().map(|image| {
            json!({
                "type": "input_image",
                "image_url": image.url,
                "detail": "auto",
            })
        }));
        json!({
            "model": model,
            "input": [{"role": "user", "content": content}],
            "max_output_tokens": 800,
        })
    } else {
        let mut content = vec![json!({
            "type": "text",
            "text": prompt,
        })];
        content.extend(images.iter().map(|image| {
            json!({
                "type": "image_url",
                "image_url": {"url": image.url},
            })
        }));
        json!({
            "model": model,
            "messages": [{"role": "user", "content": content}],
            "stream": false,
            "temperature": 0.2,
            "max_tokens": 800,
        })
    };

    let mut request = VISION_CLIENT
        .post(endpoint)
        .timeout(Duration::from_secs(60))
        .json(&request_body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Ok(actor_authorization) = std::env::var("VISION_ACTOR_AUTHORIZATION")
        && !actor_authorization.trim().is_empty()
    {
        request = request.header("x-openai-actor-authorization", actor_authorization);
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow!("视觉模型请求失败: {error}"))?;
    if !response.status().is_success() {
        return Err(anyhow!("视觉模型返回 HTTP {}", response.status()));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|error| anyhow!("视觉模型响应解析失败: {error}"))?;
    extract_response_content(&body).ok_or_else(|| anyhow!("视觉模型响应中缺少可读内容"))
}

pub(crate) fn extract_response_content(body: &Value) -> Option<String> {
    if let Some(text) = body.get("output_text").and_then(Value::as_str) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let output_text = body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|part| part.get("text"))
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !output_text.is_empty() {
        return Some(output_text);
    }
    let content = body
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))?;
    if let Some(text) = content.as_str() {
        let text = text.trim();
        return (!text.is_empty()).then_some(text.to_string());
    }
    let parts = content.as_array()?.iter().filter_map(|part| {
        part.get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    });
    let text = parts.collect::<Vec<_>>().join("\n");
    (!text.is_empty()).then_some(text)
}

fn vision_endpoint(base_url: &str, wire_api: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let suffix = if wire_api == "responses" {
        "/responses"
    } else {
        "/chat/completions"
    };
    if base_url.ends_with(suffix) {
        base_url.to_string()
    } else {
        format!("{base_url}{suffix}")
    }
}

fn is_supported_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("data:image/")
}

async fn materialize_image_url(url: &str) -> Result<String> {
    if url.starts_with("data:image/") {
        return Ok(url.to_string());
    }

    let response = IMAGE_CLIENT
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| anyhow!("下载图片失败: {error}"))?;
    if !response.status().is_success() {
        return Err(anyhow!("下载图片返回 HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
    {
        return Err(anyhow!(
            "图片超过 {} MB 限制",
            MAX_IMAGE_BYTES / 1024 / 1024
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .filter(|value| matches!(*value, "image/png" | "image/jpeg" | "image/webp"))
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| anyhow!("读取图片内容失败: {error}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(anyhow!(
            "图片超过 {} MB 限制",
            MAX_IMAGE_BYTES / 1024 / 1024
        ));
    }

    Ok(format!(
        "data:{content_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn value_as_string(data: &Value, field: &str) -> Option<String> {
    data.get(field).and_then(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ImageIntent, classify_image_intent, default_vision_prompt, extract_image_attachments,
        extract_response_content, is_vision_command, merge_image_attachments,
        message_requests_image, strip_vision_command, vision_endpoint,
    };
    use kovi::Message;
    use kovi::bot::message::Segment;
    use serde_json::json;

    #[test]
    fn extracts_and_deduplicates_image_attachments() {
        let message = Message::from(vec![
            Segment::new(
                "image",
                json!({"file_unique": "a", "file": "a.png", "url": "https://example.com/a"}),
            ),
            Segment::new(
                "image",
                json!({"file_unique": "a", "file": "a.png", "url": "https://example.com/a"}),
            ),
        ]);
        let images = extract_image_attachments(&message);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].file.as_deref(), Some("a.png"));
        assert_eq!(images[0].url.as_deref(), Some("https://example.com/a"));
    }

    #[test]
    fn recognizes_and_strips_vision_commands() {
        assert!(is_vision_command("#看截图 这个报错怎么解决"));
        assert!(!is_vision_command("请看看截图"));
        assert_eq!(
            strip_vision_command("#看截图：这个报错怎么解决"),
            "这个报错怎么解决"
        );
        assert_eq!(strip_vision_command("请看看截图"), "请看看截图");
        assert!(!default_vision_prompt().is_empty());
    }

    #[test]
    fn pure_images_are_social_unless_a_visual_request_is_pending() {
        assert_eq!(
            classify_image_intent("", true, false, false, false, false),
            ImageIntent::Social
        );
        assert_eq!(
            classify_image_intent("", true, false, false, false, true),
            ImageIntent::VisualUnderstand
        );
    }

    #[test]
    fn conversational_image_text_does_not_trigger_vision() {
        assert_eq!(
            classify_image_intent("我现在就是这样", true, false, false, false, false),
            ImageIntent::Conversational
        );
        assert_eq!(
            classify_image_intent("帮我看看图里的报错", true, false, false, false, false),
            ImageIntent::VisualUnderstand
        );
        assert!(message_requests_image("方便的话把截图发给我看看"));
    }

    #[test]
    fn merges_images_without_duplicate_keys() {
        let image = super::ImageAttachment {
            key: "image:a".to_string(),
            file: Some("a.png".to_string()),
            url: None,
        };
        let other = super::ImageAttachment {
            key: "image:b".to_string(),
            file: Some("b.png".to_string()),
            url: None,
        };
        let duplicate = image.clone();
        assert_eq!(
            merge_image_attachments(std::slice::from_ref(&image), &[duplicate, other]).len(),
            2
        );
    }

    #[test]
    fn extracts_responses_api_output_text() {
        let body = json!({
            "output": [{
                "content": [
                    {"type": "output_text", "text": "第一段"},
                    {"type": "output_text", "text": "第二段"}
                ]
            }]
        });
        assert_eq!(
            extract_response_content(&body).as_deref(),
            Some("第一段\n第二段")
        );
    }

    #[test]
    fn keeps_explicit_vision_endpoints_and_appends_missing_suffix() {
        assert_eq!(
            vision_endpoint("https://example.com/v1/responses", "responses"),
            "https://example.com/v1/responses"
        );
        assert_eq!(
            vision_endpoint("https://example.com/v1", "chat_completions"),
            "https://example.com/v1/chat/completions"
        );
    }
}
