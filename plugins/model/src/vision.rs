//! 图片附件解析与视觉模型输入。

use crate::image_security::{
    MAX_DATA_IMAGE_URL_BYTES, MAX_REMOTE_IMAGE_URL_BYTES, MAX_TOTAL_IMAGE_BYTES,
    is_safe_onebot_image_file, is_supported_url, materialize_image_url,
};
use crate::model::{ReplyTicket, is_current};
use crate::redis_store;
use anyhow::{Result, anyhow};
use kovi::bot::message::Message;
use kovi::tokio::sync::Mutex;
use kovi::{RuntimeBot, serde_json::Value};
use reqwest::Client;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use url::Url;

const MAX_VISION_IMAGES: usize = 4;
const MAX_VISION_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
static VISION_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("视觉模型 HTTP 客户端应可创建")
});
static PENDING_IMAGE_REQUESTS: LazyLock<Mutex<HashMap<ImageRequestScope, PendingImageRequest>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_IMAGE_UPDATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
const PENDING_IMAGE_REQUEST_TTL: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy)]
struct PendingImageRequest {
    deadline: Instant,
    reply_ticket: ReplyTicket,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ImageRequestScope {
    Group { group_id: i64, user_id: i64 },
    Private(i64),
}

pub(crate) async fn set_pending_image_request_for_reply(
    scope: ImageRequestScope,
    requested: bool,
    reply_ticket: ReplyTicket,
) -> bool {
    let _update_guard = PENDING_IMAGE_UPDATE_LOCK.lock().await;
    if !is_current(reply_ticket).await {
        return false;
    }

    let now = Instant::now();
    let previous = {
        let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
        pending.retain(|_, state| state.deadline > now);
        if !is_current(reply_ticket).await {
            return false;
        }
        if requested {
            pending.insert(
                scope,
                PendingImageRequest {
                    deadline: now + PENDING_IMAGE_REQUEST_TTL,
                    reply_ticket,
                },
            )
        } else {
            pending.remove(&scope)
        }
    };
    if !is_current(reply_ticket).await {
        restore_pending_image_request(scope, previous).await;
        return false;
    }

    let suffix = pending_image_request_key(scope);
    if let Some(store) = redis_store::get().await {
        if !is_current(reply_ticket).await {
            restore_pending_image_request(scope, previous).await;
            return false;
        }
        let result = if requested {
            store
                .set_expiring_text(&suffix, "1", PENDING_IMAGE_REQUEST_TTL)
                .await
        } else {
            store.delete(&suffix).await
        };
        if let Err(error) = result {
            eprintln!("[WARN] Redis 图片请求状态同步失败: {}", error);
        }
        if !is_current(reply_ticket).await {
            restore_pending_image_request(scope, previous).await;
            let compensation = if requested {
                store.delete(&suffix).await
            } else if previous.is_some() {
                store
                    .set_expiring_text(&suffix, "1", PENDING_IMAGE_REQUEST_TTL)
                    .await
            } else {
                Ok(())
            };
            if let Err(error) = compensation {
                eprintln!("[WARN] Redis 过期图片请求状态补偿失败: {}", error);
            }
            return false;
        }
    } else if !is_current(reply_ticket).await {
        restore_pending_image_request(scope, previous).await;
        return false;
    }
    true
}

async fn restore_pending_image_request(
    scope: ImageRequestScope,
    previous: Option<PendingImageRequest>,
) {
    let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
    if let Some(previous) = previous {
        pending.insert(scope, previous);
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
    let _update_guard = PENDING_IMAGE_UPDATE_LOCK.lock().await;
    let now = Instant::now();
    let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
    pending.retain(|_, state| state.deadline > now);
    let local_pending = pending.remove(&scope);
    drop(pending);
    if let Some(local_pending) = local_pending {
        let is_current_request = is_current(local_pending.reply_ticket).await;
        if let Some(store) = redis_store::get().await
            && let Err(error) = store.delete(&pending_image_request_key(scope)).await
        {
            eprintln!("[WARN] Redis 图片请求状态清理失败: {}", error);
        }
        return is_current_request;
    }

    let Some(store) = redis_store::get().await else {
        return false;
    };
    match store.take_text(&pending_image_request_key(scope)).await {
        Ok(value) => value.is_some(),
        Err(error) => {
            eprintln!("[WARN] Redis 图片请求状态读取失败: {}", error);
            false
        }
    }
}

/// 清理指定群聊的本地图片请求状态，并删除本进程已知的对应 Redis 键。
pub(crate) async fn clear_group_pending_image_requests(group_id: i64) -> usize {
    let _update_guard = PENDING_IMAGE_UPDATE_LOCK.lock().await;
    let scopes = {
        let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
        let scopes = pending
            .keys()
            .filter(|scope| {
                matches!(scope, ImageRequestScope::Group { group_id: id, .. } if *id == group_id)
            })
            .copied()
            .collect::<Vec<_>>();
        for scope in &scopes {
            pending.remove(scope);
        }
        scopes
    };
    if let Some(store) = redis_store::get().await {
        for scope in &scopes {
            if let Err(error) = store.delete(&pending_image_request_key(*scope)).await {
                eprintln!("[WARN] Redis 群聊图片请求状态清理失败: {}", error);
            }
        }
    }
    scopes.len()
}

/// 清理指定用户在私聊及各群聊中的本地图片请求状态与本进程已知的 Redis 键。
pub(crate) async fn clear_user_pending_image_requests(user_id: i64) -> usize {
    let _update_guard = PENDING_IMAGE_UPDATE_LOCK.lock().await;
    let scopes = {
        let mut pending = PENDING_IMAGE_REQUESTS.lock().await;
        let scopes = pending
            .keys()
            .filter(|scope| match scope {
                ImageRequestScope::Private(id) => *id == user_id,
                ImageRequestScope::Group { user_id: id, .. } => *id == user_id,
            })
            .copied()
            .collect::<Vec<_>>();
        for scope in &scopes {
            pending.remove(scope);
        }
        scopes
    };
    if let Some(store) = redis_store::get().await {
        let private_scope = ImageRequestScope::Private(user_id);
        if let Err(error) = store
            .delete(&pending_image_request_key(private_scope))
            .await
        {
            eprintln!("[WARN] Redis 私聊图片请求状态清理失败: {}", error);
        }
        for scope in scopes
            .iter()
            .filter(|scope| matches!(scope, ImageRequestScope::Group { .. }))
        {
            if let Err(error) = store.delete(&pending_image_request_key(*scope)).await {
                eprintln!("[WARN] Redis 群内用户图片请求状态清理失败: {}", error);
            }
        }
    }
    scopes.len()
}

fn pending_image_request_key(scope: ImageRequestScope) -> String {
    match scope {
        ImageRequestScope::Group { group_id, user_id } => {
            format!("vision:pending:group:{group_id}:user:{user_id}")
        }
        ImageRequestScope::Private(user_id) => format!("vision:pending:private:{user_id}"),
    }
}

pub(crate) fn with_social_image_context(message: &str) -> String {
    format!(
        "{message}\n<图片使用方式 data-only=\"true\">这张图片更像随聊天附带的状态或情绪表达。只结合它的整体语气自然回应当前文字；除非对方明确询问图片内容，不要逐项描述画面、识别角色或罗列视觉细节。</图片使用方式>"
    )
}

/// 提取普通图片消息段。商城表情和 QQ 内置表情不作为截图输入。
pub(crate) fn extract_image_attachments(message: &Message) -> Vec<ImageAttachment> {
    let mut seen = HashSet::new();
    message
        .iter()
        .filter(|segment| segment.type_ == "image")
        .filter_map(|segment| {
            let file = value_as_bounded_string(&segment.data, "file", 512);
            let url = value_as_image_url(&segment.data, "url");
            let identifier = value_as_bounded_string(&segment.data, "file_unique", 512)
                .or_else(|| value_as_bounded_string(&segment.data, "md5", 512))
                .or_else(|| file.clone())
                .or_else(|| url.as_deref().map(hashed_image_url_key))?;
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
    let mut total_bytes = 0usize;
    for attachment in attachments.iter().take(MAX_VISION_IMAGES) {
        let source_url = if let Some(url) = attachment
            .url
            .as_deref()
            .filter(|url| is_supported_url(url))
        {
            url.to_string()
        } else {
            let Some(file) = attachment
                .file
                .as_deref()
                .filter(|file| is_safe_onebot_image_file(file))
            else {
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
        let remaining = MAX_TOTAL_IMAGE_BYTES.saturating_sub(total_bytes);
        if remaining == 0 {
            return Err(anyhow!(
                "图片总大小超过 {} MB 限制",
                MAX_TOTAL_IMAGE_BYTES / 1024 / 1024
            ));
        }
        let materialized = materialize_image_url(&source_url, remaining).await?;
        total_bytes = total_bytes.saturating_add(materialized.byte_len);
        images.push(VisionImage {
            url: materialized.data_url,
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
    "请认真查看这张图片，先描述其中真正能确认的主要内容；如果图片里有清晰可读的文字、报错、按钮或关键数据，请提取出来，并结合当前问题给出帮助。只有具体区域、小字或被遮挡内容真的无法辨认时，才说明哪里看不清，不要泛泛说整张图看不清。"
}

/// 使用单独的 OpenAI 兼容视觉接口分析图片，主聊天模型只接收分析后的文字。
pub(crate) async fn analyze_images_with_builtin(
    images: &[VisionImage],
    question: &str,
) -> Result<String> {
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
        "请分析用户提供的截图，只陈述图片中能确认的事实。提取清晰可读的文字、页面或应用名称、错误信息、按钮和关键状态；结合用户问题指出相关内容，但不要臆测。只有具体区域、小字或被遮挡内容真的无法辨认时，才说明哪里看不清，不要泛泛说整张图看不清。\n\n用户问题：{}",
        question
    );
    let endpoint = vision_endpoint(&url, &wire_api);
    validate_vision_endpoint(&endpoint)?;
    let request_body = if wire_api == "responses" {
        let mut content = vec![json!({
            "type": "input_text",
            "text": prompt,
        })];
        content.extend(images.iter().map(|image| {
            json!({
                "type": "input_image",
                "image_url": image.url,
                "detail": "high",
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
                "image_url": {"url": image.url, "detail": "high"},
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

    let body_bytes = read_bounded_vision_response(response).await?;
    let body: Value = serde_json::from_slice(&body_bytes)
        .map_err(|error| anyhow!("视觉模型响应解析失败: {error}"))?;
    extract_response_content(&body).ok_or_else(|| anyhow!("视觉模型响应中缺少可读内容"))
}

async fn read_bounded_vision_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VISION_RESPONSE_BYTES as u64)
    {
        return Err(anyhow!("视觉模型响应超过大小上限"));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("读取视觉模型响应失败: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_VISION_RESPONSE_BYTES {
            return Err(anyhow!("视觉模型响应超过大小上限"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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

fn validate_vision_endpoint(raw_url: &str) -> Result<()> {
    let url = Url::parse(raw_url).map_err(|_| anyhow!("VISION_API_URL 格式无效"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("VISION_API_URL 不能携带用户名或密码"));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url.host_str().unwrap_or_default()) => Ok(()),
        "http" => Err(anyhow!(
            "非本机视觉模型端点必须使用 HTTPS，避免 API Token 明文传输"
        )),
        _ => Err(anyhow!(
            "VISION_API_URL 只支持 HTTPS；本机回环地址可使用 HTTP"
        )),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn value_as_bounded_string(data: &Value, field: &str, max_bytes: usize) -> Option<String> {
    data.get(field).and_then(|value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= max_bytes)
            .map(ToString::to_string)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
    })
}

fn value_as_image_url(data: &Value, field: &str) -> Option<String> {
    let value = data.get(field)?.as_str()?.trim();
    if value.is_empty() {
        return None;
    }
    let max_bytes = if value.starts_with("data:image/") {
        MAX_DATA_IMAGE_URL_BYTES
    } else {
        MAX_REMOTE_IMAGE_URL_BYTES
    };
    (value.len() <= max_bytes).then(|| value.to_string())
}

fn hashed_image_url_key(url: &str) -> String {
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("url:{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::{
        default_vision_prompt, extract_image_attachments, extract_response_content,
        is_vision_command, merge_image_attachments, strip_vision_command, validate_vision_endpoint,
        vision_endpoint,
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

    #[test]
    fn rejects_unsafe_vision_endpoints() {
        assert!(validate_vision_endpoint("https://example.com/v1/responses").is_ok());
        assert!(validate_vision_endpoint("http://localhost:8080/v1/responses").is_ok());
        assert!(validate_vision_endpoint("http://127.0.0.1:8080/v1/responses").is_ok());
        assert!(validate_vision_endpoint("http://example.com/v1/responses").is_err());
        assert!(validate_vision_endpoint("https://user:password@example.com/").is_err());
        assert!(validate_vision_endpoint("file:///tmp/vision.sock").is_err());
    }
}
