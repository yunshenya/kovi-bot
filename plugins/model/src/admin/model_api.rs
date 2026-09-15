//! 模型接口：在后台看/换/测当前用的对话模型。
//!
//! 与"配置页"的分工：配置页是通用编辑器（每个字段一格），这里只做换模型这一件事
//! —— 选服务商预设、填密钥、选模型名、**点一下测试连通性**、应用。写进去的还是
//! `[server_config]`（落在运行时覆盖配置里，0600），所以两个页面看到的是同一份真相。
//!
//! 三条硬边界：
//!
//! - **密钥只进不出**：接口从不回显密钥，只回"有没有配、从哪来"。写回时空值或
//!   `********` 一律表示"不改"。
//! - **测试连接不落盘**：它拿表单里的值直接发一次最小请求，不影响正在跑的配置。
//! - **探针不跟随跳转**：避免把带密钥的请求被 3xx 引到别处；错误详情里若出现密钥
//!   也会被抹掉再返回。

use super::ApiError;
use super::model_profiles::{self, ModelProfile};
use super::{AdminState, config_api};
use crate::config::{self, ApiKeySource, ServerConfig};
use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 模型设置写进这里（运行时覆盖配置），而不是发布目录里的主配置：
/// 它跨发布保留，而且会被收紧到 0600。
const TARGET_CONFIG: &str = "bot.conf.override.toml";
/// 探针的等待上限。比真实请求短：测试连接是"现在通不通"，不是"慢但能用"。
const PROBE_TIMEOUT_SECS: u64 = 20;
/// 探针最多读回多少字节的错误正文（避免把整个 HTML 错误页塞进响应）。
const MAX_PROBE_ERROR_BYTES: usize = 4 * 1024;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ModelRequest {
    /// 应用/测试一套已保存的档案。
    #[serde(default)]
    profile_id: Option<String>,
    /// 同时把这套设置存成档案（给档案起个名字）。
    #[serde(default)]
    save_as: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    wire_api: Option<String>,
    #[serde(default)]
    thinking_mode: Option<String>,
    #[serde(default)]
    supports_vision: Option<bool>,
    #[serde(default)]
    requires_auth: Option<bool>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    /// 新密钥。缺省、空串或 `********` 都表示"不改"。
    #[serde(default)]
    api_key: Option<String>,
}

/// 当前生效的模型设置（不含密钥本身）。
fn current_model_json(server: &ServerConfig) -> Value {
    let source = server.api_key_source();
    json!({
        "enabled": server.enabled(),
        "url": server.url(),
        "endpoint": server.endpoint(),
        "model_name": server.model_name(),
        "wire_api": server.wire_api(),
        "thinking_mode": server.thinking_mode(),
        "supports_vision": server.supports_vision(),
        "requires_auth": server.requires_auth(),
        "max_output_tokens": server.max_output_tokens(),
        "request_timeout_secs": server.request_timeout_secs(),
        "max_retries": server.max_retries(),
        "has_key": server.resolved_api_key().is_some(),
        "key_source": match &source {
            ApiKeySource::Config => "config",
            ApiKeySource::Environment(_) => "environment",
            ApiKeySource::Missing => "missing",
        },
        "key_source_text": source.describe(server.enabled(), server.requires_auth()),
        "api_key_env": server.api_key_env(),
    })
}

/// 档案列表。`active` 由后端一处判定（[`ModelProfile::is_live`]），页面只负责照着
/// 显示与禁用删除，不再自己重算一遍"地址 + 模型名"那个口径。
fn profiles_json(server: &ServerConfig) -> Result<Value, ApiError> {
    let profiles = model_profiles::list()?;
    let items: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            json!({
                "id": profile.id,
                "label": profile.label,
                "url": profile.url,
                "model_name": profile.model_name,
                "wire_api": profile.wire_api,
                "thinking_mode": profile.thinking_mode,
                "supports_vision": profile.supports_vision,
                "requires_auth": profile.requires_auth,
                "max_output_tokens": profile.max_output_tokens,
                "has_key": !profile.api_key.trim().is_empty(),
                "active": profile.is_live(server),
            })
        })
        .collect();
    Ok(json!({
        "items": items,
        "path": model_profiles::profiles_path().display().to_string(),
    }))
}

/// 一份完整的响应：当前设置 + 档案 + 落盘位置。
fn model_state() -> Result<Value, ApiError> {
    // 整份配置读一次：当前设置与"哪套档案正在用"必须来自同一个快照，
    // 否则两边各读一次，正好卡在切模型中间时会自相矛盾。
    let live = config::get();
    let server = live.server_config();
    Ok(json!({
        "current": current_model_json(server),
        "profiles": profiles_json(server)?,
        "config_file": config::override_file_path().display().to_string(),
    }))
}

/// `GET /api/model`
pub(crate) async fn state() -> Result<Json<Value>, ApiError> {
    Ok(Json(model_state()?))
}

/// 表单里的字段落到一套设置上：缺省项沿用当前生效值，密钥缺省/掩码表示不改。
fn settings_from_request(request: &ModelRequest) -> Result<ModelProfile, ApiError> {
    let current = config::get();
    let server = current.server_config();
    let mut profile = if let Some(id) = request.profile_id.as_deref() {
        model_profiles::find(id)?
            .ok_or_else(|| ApiError::not_found(format!("找不到模型档案: {id}")))?
    } else {
        ModelProfile {
            id: String::new(),
            label: request.label.clone().unwrap_or_default(),
            url: server.url().to_string(),
            model_name: server.model_name().to_string(),
            wire_api: server.wire_api().to_string(),
            thinking_mode: server.thinking_mode().to_string(),
            supports_vision: server.supports_vision(),
            requires_auth: server.requires_auth(),
            max_output_tokens: server.max_output_tokens(),
            // 空串 = 不改配置里已有的密钥（那一把只进不出，前端也拿不到）。
            api_key: String::new(),
        }
    };
    if let Some(url) = request.url.as_deref() {
        profile.url = url.trim().to_string();
    }
    if let Some(model_name) = request.model_name.as_deref() {
        profile.model_name = model_name.trim().to_string();
    }
    if let Some(wire_api) = request.wire_api.as_deref() {
        profile.wire_api = wire_api.trim().to_string();
    }
    if let Some(thinking_mode) = request.thinking_mode.as_deref() {
        profile.thinking_mode = thinking_mode.trim().to_string();
    }
    if let Some(supports_vision) = request.supports_vision {
        profile.supports_vision = supports_vision;
    }
    if let Some(requires_auth) = request.requires_auth {
        profile.requires_auth = requires_auth;
    }
    if let Some(max_output_tokens) = request.max_output_tokens {
        profile.max_output_tokens = max_output_tokens;
    }
    if let Some(label) = request
        .label
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        profile.label = label.to_string();
    }
    if let Some(api_key) = request.api_key.as_deref() {
        let trimmed = api_key.trim();
        // 掩码与空串都表示"不改"：前端从来看不到真密钥，所以它也只能这么表达。
        if !trimmed.is_empty() && trimmed != super::config_api::mask_placeholder() {
            profile.api_key = trimmed.to_string();
        }
    }
    Ok(profile)
}

/// `POST /api/model/apply`：把一套设置写进配置并立刻生效。
pub(crate) async fn apply(
    State(state): State<Arc<AdminState>>,
    Json(request): Json<ModelRequest>,
) -> Result<Json<Value>, ApiError> {
    let profile = settings_from_request(&request)?;
    if profile.model_name.trim().is_empty() {
        return Err(ApiError::bad_request("模型名不能为空"));
    }
    let changes = profile.changes();
    let outcome = config_api::apply_changes(&state, TARGET_CONFIG, changes).await?;

    // 存档案是可选的第二步：先让设置生效，再记下来，顺序反了会留下"档案里有、配置里没有"。
    let saved = match request
        .save_as
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        Some(label) => {
            let mut to_save = profile.clone();
            to_save.label = label.to_string();
            // 换名字就是新的一套：除非这次本来就是从某套档案改过来的。
            if request.profile_id.is_none() {
                to_save.id = String::new();
            }
            let id = model_profiles::upsert(to_save)?;
            println!("[INFO] 模型档案已保存: {label} (id={id})");
            Some(id)
        }
        None => None,
    };

    println!(
        "[INFO] 模型设置已应用: {} (changed={:?})",
        profile.describe(),
        outcome
            .get("changed")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0)
    );
    Ok(Json(json!({
        "ok": true,
        "changed": outcome.get("changed").cloned().unwrap_or(Value::Null),
        "skipped": outcome.get("skipped").cloned().unwrap_or(Value::Null),
        "profile_id": saved,
        "model": model_state()?,
    })))
}

/// `DELETE /api/model/profiles/{id}`
///
/// **正在用的那套不能删**：拦在这里而不是只靠前端把按钮灰掉，接口自己就是那道闸。
/// 返回 409，正文里写清怎么才能删（先切走，或先关掉外部模型）。
pub(crate) async fn remove_profile(Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    // 读一次当前配置：判定"正在用"的依据必须是这一刻生效的那份。
    let live = config::get();
    model_profiles::remove(&id, live.server_config())?;
    println!("[INFO] 模型档案已删除: {id}");
    Ok(Json(json!({ "ok": true, "model": model_state()? })))
}

/// `POST /api/model/test`：用表单里的值真发一次最小请求，不落盘。
pub(crate) async fn test(Json(request): Json<ModelRequest>) -> Result<Json<Value>, ApiError> {
    let profile = settings_from_request(&request)?;
    let api_key = if !profile.api_key.trim().is_empty() {
        Some(profile.api_key.trim().to_string())
    } else {
        config::get().server_config().resolved_api_key()
    };
    if profile.requires_auth && api_key.is_none() {
        return Ok(Json(json!({
            "ok": false,
            "detail": "没有可用的 API Key：先在下面填一把，或改用不需要鉴权的端点。",
        })));
    }
    let started = Instant::now();
    match probe(&profile, api_key.as_deref()).await {
        Ok(detail) => Ok(Json(json!({
            "ok": true,
            "latency_ms": started.elapsed().as_millis() as u64,
            "detail": detail,
            "model_name": profile.model_name,
            "endpoint": endpoint_of(&profile),
        }))),
        Err(detail) => Ok(Json(json!({
            "ok": false,
            "latency_ms": started.elapsed().as_millis() as u64,
            "detail": detail,
            "model_name": profile.model_name,
            "endpoint": endpoint_of(&profile),
        }))),
    }
}

fn endpoint_of(profile: &ModelProfile) -> String {
    let base = profile.url.trim_end_matches('/');
    let suffix = if profile.wire_api == "responses" {
        "/responses"
    } else {
        "/chat/completions"
    };
    if base.ends_with(suffix) {
        base.to_string()
    } else {
        format!("{base}{suffix}")
    }
}

/// 一条 "ping"：非流式、只要 HTTP 2xx 就算通。返回一句给人看的成功描述。
async fn probe(profile: &ModelProfile, api_key: Option<&str>) -> Result<String, String> {
    if profile.url.trim().is_empty() {
        return Err("地址是空的：填一个 base_url 或完整的 completions 地址。".to_string());
    }
    let mut body = if profile.wire_api == "responses" {
        json!({
            "model": profile.model_name,
            "instructions": "只回一个词。",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "ping"}]}],
            "stream": false,
            "max_output_tokens": 32,
        })
    } else {
        json!({
            "model": profile.model_name,
            "messages": [{"role": "user", "content": "ping"}],
            "stream": false,
            "temperature": 0.0,
            "max_tokens": 16,
        })
    };
    if profile.thinking_mode == "disabled" {
        if profile.wire_api == "responses" {
            body["reasoning"] = json!({"effort": "none"});
        } else {
            body["thinking"] = json!({"type": "disabled"});
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        // 不跟随跳转：带密钥的请求不能被 3xx 引到别处去。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("无法创建 HTTP 客户端: {error}"))?;
    let mut request = client
        .post(endpoint_of(profile))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body);
    if profile.requires_auth
        && let Some(key) = api_key
    {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("请求没发出去或超时: {error}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(format!("端点返回 {status}，模型可用"));
    }
    let detail = read_error_detail(response, api_key).await;
    Err(format!("端点返回 {status}：{detail}"))
}

/// 读一小段错误正文，尽量取出 provider 的 message；出现的密钥一律抹掉。
async fn read_error_detail(response: reqwest::Response, api_key: Option<&str>) -> String {
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.ok().flatten() {
        if body.len() + chunk.len() > MAX_PROBE_ERROR_BYTES {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&body).trim().to_string();
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/error"))
                .or_else(|| value.pointer("/detail"))
                .or_else(|| value.pointer("/message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or(text);
    let detail = scrub_secret(&detail, api_key);
    if detail.is_empty() {
        return "（端点没有返回正文）".to_string();
    }
    detail
}

/// 错误详情里若混进了密钥（某些网关会把请求头回显出来），抹掉再外传。
fn scrub_secret(detail: &str, api_key: Option<&str>) -> String {
    let detail: String = detail.chars().take(600).collect();
    match api_key.map(str::trim).filter(|key| key.len() >= 8) {
        Some(key) => detail.replace(key, "******"),
        None => detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        kovi::tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(future)
    }

    /// 假模型端点：只回一个最小 JSON，状态码可指定。记录收到的 Authorization 头，
    /// 用来验证"填了 key 就真的带上去了、没填就不带"。
    async fn spawn_stub_model(status: u16, seen: StdArc<std::sync::Mutex<Vec<String>>>) -> String {
        use kovi::tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = kovi::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub model");
        let port = listener.local_addr().expect("stub model address").port();
        kovi::tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = vec![0_u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let authorization = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if let Ok(mut seen) = seen.lock() {
                    seen.push(authorization);
                }
                let body = if status == 200 {
                    r#"{"choices":[{"message":{"content":"pong"}}]}"#.to_string()
                } else {
                    r#"{"error":{"message":"invalid api key"}}"#.to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://127.0.0.1:{port}/v1/chat/completions")
    }

    #[test]
    fn endpoint_accepts_both_a_base_url_and_a_full_path() {
        let mut profile = ModelProfile {
            id: "t".to_string(),
            label: "t".to_string(),
            url: "https://api.example.com/v1".to_string(),
            model_name: "m".to_string(),
            wire_api: "chat_completions".to_string(),
            thinking_mode: "disabled".to_string(),
            supports_vision: false,
            requires_auth: true,
            max_output_tokens: 0,
            api_key: String::new(),
        };
        assert_eq!(
            endpoint_of(&profile),
            "https://api.example.com/v1/chat/completions"
        );
        profile.url = "https://api.example.com/v1/chat/completions".to_string();
        assert_eq!(
            endpoint_of(&profile),
            "https://api.example.com/v1/chat/completions"
        );
        profile.url = "https://api.example.com/v1/".to_string();
        profile.wire_api = "responses".to_string();
        assert_eq!(
            endpoint_of(&profile),
            "https://api.example.com/v1/responses"
        );
    }

    /// 网关把请求头回显进错误正文时，密钥不能跟着进后台页面。
    #[test]
    fn error_details_never_echo_the_key() {
        let key = "sk-abcdefghijklmnop";
        let detail = format!("unauthorized: Bearer {key} is invalid");
        let scrubbed = scrub_secret(&detail, Some(key));
        assert!(!scrubbed.contains(key));
        assert!(scrubbed.contains("******"));
        // 短得像密码的串不动（免得把正常文案里的词替换掉）。
        assert_eq!(scrub_secret("key=abc", Some("abc")), "key=abc");
    }

    /// 从表单到磁盘：一套档案的改动落在 `[server_config]` 上，密钥也在里面，
    /// 且不会把原来配置里的其它字段弄丢。
    ///
    /// 这条是"后台换了模型，磁盘上到底写了什么"的直接断言——它不读全局配置、
    /// 不写任何真实文件，所以可以在并行用例里跑。
    #[test]
    fn applying_a_profile_lands_in_the_server_config_section() {
        let existing = "[memory]\nretention_days = 30\n";
        let profile = ModelProfile {
            id: "openai".to_string(),
            label: "OpenAI".to_string(),
            url: "https://api.openai.com/v1".to_string(),
            model_name: "gpt-4o-mini".to_string(),
            wire_api: "chat_completions".to_string(),
            thinking_mode: "auto".to_string(),
            supports_vision: true,
            requires_auth: true,
            max_output_tokens: 2000,
            api_key: "sk-landed".to_string(),
        };
        let (text, applied, skipped) =
            config_api::changed_document(existing, &profile.changes()).expect("应能应用改动");
        assert!(skipped.is_empty());
        assert!(applied.contains(&"server_config.api_key".to_string()));

        let parsed: kovi::toml::Value =
            kovi::toml::from_str(&text).expect("写出来的必须是合法 TOML");
        let server = parsed
            .get("server_config")
            .expect("应建出 server_config 段");
        assert_eq!(
            server.get("model_name").and_then(|v| v.as_str()),
            Some("gpt-4o-mini")
        );
        assert_eq!(
            server.get("api_key").and_then(|v| v.as_str()),
            Some("sk-landed")
        );
        assert_eq!(
            server.get("supports_vision").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(server.get("enabled").and_then(|v| v.as_bool()), Some(true));
        // 原来就有的段不能被这次改动吃掉。
        assert_eq!(
            parsed
                .get("memory")
                .and_then(|memory| memory.get("retention_days"))
                .and_then(|value| value.as_integer()),
            Some(30)
        );

        // 掩码值必须被跳过（前端拿不到真密钥，只能用掩码表达"不改"）。
        let mut with_mask = std::collections::BTreeMap::new();
        with_mask.insert(
            "server_config.api_key".to_string(),
            serde_json::json!(config_api::mask_placeholder()),
        );
        let (text, applied, skipped) =
            config_api::changed_document("", &with_mask).expect("掩码不该报错");
        assert!(applied.is_empty());
        assert_eq!(skipped, vec!["server_config.api_key".to_string()]);
        assert!(!text.contains("****"), "掩码绝不能落盘: {text}");
    }

    /// 覆盖配置是装着密钥的文件：写它必须 0600，而且从出现的第一刻就是。
    #[test]
    fn private_config_writes_are_owner_only() {
        let dir = std::env::temp_dir().join(format!("kovi-model-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("应能建临时目录");
        let path = dir.join("bot.conf.override.toml");

        config_api::write_atomically_private(&path, "[server_config]\napi_key = \"sk-x\"\n")
            .expect("应能写入");
        assert!(
            std::fs::read_to_string(&path)
                .expect("应能读回")
                .contains("sk-x")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "覆盖配置必须只有属主可读写，实际 {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 档案文件的增删改查走真实文件（路径由调用方给，不碰进程级目录）。
    #[test]
    fn profiles_round_trip_through_a_private_file() {
        let dir = std::env::temp_dir().join(format!("kovi-model-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("应能建临时目录");
        let path = dir.join(model_profiles::PROFILES_FILE);

        assert!(
            model_profiles::list_at(&path)
                .expect("空文件应当可读")
                .is_empty()
        );
        let mut first = ModelProfile {
            id: String::new(),
            label: "DeepSeek".to_string(),
            url: "https://api.deepseek.com".to_string(),
            model_name: "deepseek-v4-flash".to_string(),
            wire_api: "chat_completions".to_string(),
            thinking_mode: "disabled".to_string(),
            supports_vision: false,
            requires_auth: true,
            max_output_tokens: 1200,
            api_key: "sk-deepseek".to_string(),
        };
        let id = model_profiles::upsert_at(&path, first.clone()).expect("应能保存");
        assert_eq!(id, "deepseek");

        // 同名再存一套：新 id，不覆盖旧的。
        first.id = String::new();
        first.label = "DeepSeek".to_string();
        let second = model_profiles::upsert_at(&path, first.clone()).expect("应能保存");
        assert_eq!(second, "deepseek-2");
        assert_eq!(model_profiles::list_at(&path).expect("应能读取").len(), 2);

        // 按 id 更新不会新增一套。
        let mut renamed = first.clone();
        renamed.id = id.clone();
        renamed.label = "DeepSeek 备用".to_string();
        model_profiles::upsert_at(&path, renamed).expect("应能更新");
        let profiles = model_profiles::list_at(&path).expect("应能读取");
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            profiles
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.label.as_str()),
            Some("DeepSeek 备用")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "档案里存着密钥，必须是 0600，实际 {mode:o}");
        }

        // 删除：不在用的那套删得掉；再删如实回 404（而不是假装删成功）。
        let offline =
            model_profiles::test_server(true, "https://api.example.com/v1", "example-model");
        model_profiles::remove_at(&path, &id, &offline).expect("不在用的那套应能删除");
        let missing = model_profiles::remove_at(&path, &id, &offline).expect_err("再删应报找不到");
        assert_eq!(
            missing.status,
            axum::http::StatusCode::NOT_FOUND,
            "{missing:?}"
        );
        assert_eq!(model_profiles::list_at(&path).expect("应能读取").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 探针：2xx 算通，4xx 要把 provider 的原因带回来，且两种情况下都不泄露密钥。
    #[test]
    fn probe_reports_success_and_provider_errors() {
        let seen = StdArc::new(std::sync::Mutex::new(Vec::new()));
        block_on(async {
            let endpoint = spawn_stub_model(200, StdArc::clone(&seen)).await;
            let profile = ModelProfile {
                id: "stub".to_string(),
                label: "stub".to_string(),
                url: endpoint,
                model_name: "stub-model".to_string(),
                wire_api: "chat_completions".to_string(),
                thinking_mode: "disabled".to_string(),
                supports_vision: false,
                requires_auth: true,
                max_output_tokens: 0,
                api_key: String::new(),
            };
            let ok = probe(&profile, Some("sk-secret-value")).await;
            assert!(ok.is_ok(), "2xx 应当算通: {ok:?}");
            assert!(
                seen.lock()
                    .expect("seen")
                    .last()
                    .is_some_and(|header| header
                        .to_ascii_lowercase()
                        .contains("bearer sk-secret-value")),
                "填了密钥就必须真的带上 Authorization 头"
            );

            let failing = spawn_stub_model(401, StdArc::clone(&seen)).await;
            let profile = ModelProfile {
                url: failing,
                ..profile
            };
            let failure = probe(&profile, Some("sk-secret-value")).await;
            let detail = failure.expect_err("401 必须算没通");
            assert!(detail.contains("invalid api key"), "{detail}");
            assert!(!detail.contains("sk-secret-value"), "错误详情不能带上密钥");
        });
    }

    /// HTTP 层：真 axum 服务 + 真路由。无 Token 401、读回状态（含每套档案的
    /// `active` 标记）、删除不存在的档案回 404、测试连接真发出去并带上密钥。
    /// 这条不写任何文件（apply 的落盘由上面那条 `changed_document` 用例覆盖，
    /// 删除闸门由 `model_profiles` 那条用例覆盖），所以不必进 ignored 名单。
    #[test]
    fn model_endpoints_answer_over_http() {
        let seen = StdArc::new(std::sync::Mutex::new(Vec::new()));
        block_on(async {
            let state = super::super::AdminState::for_test("test-token");
            let app = super::super::router(StdArc::clone(&state));
            let listener = kovi::tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("应能监听回环端口");
            let address = listener.local_addr().expect("应能读到端口");
            let server = kovi::tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let client = reqwest::Client::new();
            let base = format!("http://{address}");

            let anonymous = client
                .get(format!("{base}/api/model"))
                .send()
                .await
                .expect("应能请求");
            assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

            let state: Value = client
                .get(format!("{base}/api/model"))
                .bearer_auth("test-token")
                .send()
                .await
                .expect("应能请求")
                .json()
                .await
                .expect("应返回 JSON");
            assert!(state["current"]["model_name"].is_string(), "{state}");
            assert!(state["current"]["key_source"].is_string(), "{state}");
            assert!(state["profiles"]["items"].is_array(), "{state}");
            assert!(
                !state.to_string().contains("api_key\":\"sk-"),
                "状态接口绝不能回显密钥: {state}"
            );
            // 每套档案都带一个布尔 active：页面照着它标「正在用」、灰掉删除按钮。
            for item in state["profiles"]["items"].as_array().expect("档案列表") {
                assert!(item["active"].is_boolean(), "每套档案都要有 active: {item}");
            }

            // 删不存在的档案：404，而不是"假装删成功"。id 里带下划线，`slug()` 生成的
            // id 只可能是小写字母数字与短横线，所以这个名字不可能是本机真有的档案
            // （这条用例因此不会动到任何真实文件）。
            let missing = client
                .delete(format!("{base}/api/model/profiles/no_such_profile"))
                .bearer_auth("test-token")
                .send()
                .await
                .expect("应能请求");
            assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

            // 测试连接：打到假端点，2xx 算通，并且必须真的带上密钥。
            let endpoint = spawn_stub_model(200, StdArc::clone(&seen)).await;
            let ok: Value = client
                .post(format!("{base}/api/model/test"))
                .bearer_auth("test-token")
                .json(&json!({
                    "url": endpoint,
                    "model_name": "stub-model",
                    "wire_api": "chat_completions",
                    "thinking_mode": "disabled",
                    "requires_auth": true,
                    "api_key": "sk-http-secret",
                }))
                .send()
                .await
                .expect("应能请求")
                .json()
                .await
                .expect("应返回 JSON");
            assert_eq!(ok["ok"], true, "{ok}");
            assert!(
                seen.lock()
                    .expect("seen")
                    .last()
                    .is_some_and(|header| header
                        .to_ascii_lowercase()
                        .contains("bearer sk-http-secret")),
                "测试连接必须真的把密钥发出去"
            );

            // 端点报错时：如实回没通，且不把密钥写进详情。
            let failing = spawn_stub_model(401, StdArc::clone(&seen)).await;
            let bad: Value = client
                .post(format!("{base}/api/model/test"))
                .bearer_auth("test-token")
                .json(&json!({
                    "url": failing,
                    "model_name": "stub-model",
                    "wire_api": "chat_completions",
                    "requires_auth": true,
                    "api_key": "sk-http-secret",
                }))
                .send()
                .await
                .expect("应能请求")
                .json()
                .await
                .expect("应返回 JSON");
            assert_eq!(bad["ok"], false, "{bad}");
            let detail = bad["detail"].as_str().unwrap_or_default().to_string();
            assert!(detail.contains("invalid api key"), "{detail}");
            assert!(!detail.contains("sk-http-secret"), "{detail}");

            server.abort();
        });
    }

    /// 表单里的密钥三种写法都要落到"不改"，否则页面一保存就把真密钥抹成空。
    #[test]
    fn blank_or_masked_keys_leave_the_stored_key_alone() {
        // 这条用例只碰纯逻辑，不读进程配置：构造请求后直接看字段落点。
        for submitted in [
            None,
            Some(String::new()),
            Some("   ".to_string()),
            Some("********".to_string()),
        ] {
            let request = ModelRequest {
                api_key: submitted.clone(),
                ..ModelRequest::default()
            };
            let profile = settings_from_request(&request).expect("应能构造设置");
            assert!(profile.api_key.is_empty(), "{submitted:?} 不该被当成新密钥");
        }
        let request = ModelRequest {
            api_key: Some(" sk-new ".to_string()),
            ..ModelRequest::default()
        };
        let profile = settings_from_request(&request).expect("应能构造设置");
        assert_eq!(profile.api_key, "sk-new");
    }
}
