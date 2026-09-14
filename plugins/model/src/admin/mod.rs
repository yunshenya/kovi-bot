//! # 自带 Web 管理后台
//!
//! 参考 NapCat WebUI 的形态，在机器人进程内起一个只监听回环地址的小型
//! HTTP 服务：
//!
//! - **登录**：必须带 Token（环境变量 / 配置文件 / 首次自动生成并落盘），
//!   通过后换一个 HttpOnly 会话 Cookie；接口同时接受 `Authorization: Bearer`。
//! - **配置**：展示 `bot.conf.toml` 的全部参数（含未写进文件的默认值），
//!   按字段说明渲染表单，保存时用 `toml_edit` 按注释无损写回、先校验后落盘、
//!   再热替换内存里的配置。
//! - **记忆**：把 PostgreSQL 里的长期记忆、情节、人物、目标等按 Hindsight
//!   的方式展示为「可搜索的列表 + 详情」。
//!
//! 前端是无构建的静态资源（`include_str!` 内嵌），因此发布仍然只有一个二进制。

mod annotation_api;
mod assets;
mod auth;
mod config_api;
mod memory_api;
mod status_api;
mod sticker_api;
mod token;

use crate::config::AdminConfig;
use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kovi::RuntimeBot;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 后台运行期共享状态。
pub(crate) struct AdminState {
    /// 唯一登录 Token。进程生命周期内不变。
    token: String,
    /// 已登录会话。
    sessions: auth::Sessions,
    /// 登录失败限速。
    guard: auth::LoginGuard,
    /// 一次性登录挑战。
    challenges: auth::Challenges,
    /// 已保存但需要重启才生效的分区。
    pending_restart: Mutex<BTreeSet<String>>,
    /// 主机与进程的采样器（复用它才能算出 CPU 使用率）。
    monitor: Arc<status_api::SystemMonitor>,
    /// OneBot 侧信息（登录号、版本）的缓存，避免每次刷新都打服务端。
    onebot_cache: Mutex<Option<(Instant, Value)>>,
    /// 机器人句柄；只有通过它才能问到 OneBot 服务端的版本与登录状态。
    bot: Option<Arc<RuntimeBot>>,
    /// 后台启动时刻，用于概览页显示运行时长。
    started_at: Instant,
}

impl AdminState {
    fn new(token: String, session_ttl_secs: u64, bot: Option<Arc<RuntimeBot>>) -> Self {
        Self {
            token,
            sessions: auth::Sessions::new(session_ttl_secs),
            guard: auth::LoginGuard::default(),
            challenges: auth::Challenges::default(),
            pending_restart: Mutex::new(BTreeSet::new()),
            monitor: Arc::new(status_api::SystemMonitor::new()),
            onebot_cache: Mutex::new(None),
            bot,
            started_at: Instant::now(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(token: &str) -> Arc<Self> {
        Arc::new(Self::new(token.to_string(), 3_600, None))
    }

    /// 校验挑战应答：`proof == sha256(nonce + ":" + token)`。
    pub(crate) fn token_matches_proof(&self, nonce: &str, proof: &str) -> bool {
        if self.token.is_empty() || proof.is_empty() {
            return false;
        }
        constant_time_eq(&auth::login_proof(nonce, &self.token), proof)
    }

    /// 记录"改了但要重启才生效"的分区。
    pub(crate) fn note_pending_restart(&self, sections: &[String]) {
        if sections.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.pending_restart.lock() {
            for section in sections {
                guard.insert(section.clone());
            }
        }
    }

    pub(crate) fn pending_restart(&self) -> Vec<String> {
        self.pending_restart
            .lock()
            .map(|guard| guard.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn token_matches(&self, candidate: &str) -> bool {
        // 配置失误导致 Token 为空时，绝不能退化成"空 Token 就能登录"。
        if self.token.is_empty() || candidate.is_empty() {
            return false;
        }
        constant_time_eq(&self.token, candidate)
    }
}

/// 常量时间字符串比较。
///
/// 逐字节累加差异而不是短路返回：短路会在响应时间上泄露"前缀匹配了多少位"，
/// 对 Token 与挑战应答这种可离线爆破的秘密不利。
pub(crate) fn constant_time_eq(expected: &str, actual: &str) -> bool {
    if expected.is_empty() || actual.is_empty() {
        return false;
    }
    let expected = expected.as_bytes();
    let actual = actual.as_bytes();
    let mut diff = (expected.len() ^ actual.len()) as u8;
    for index in 0..expected.len().max(actual.len()) {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = actual.get(index).copied().unwrap_or(0);
        diff |= left ^ right;
    }
    diff == 0
}

/// 接口错误：统一成 `{"error": "..."}`，不再把内部细节包装成 500 空响应。
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
        }
    }

    /// 乐观并发冲突：调用方拿着过期的文件版本提交改动。
    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// 启动后台服务。
///
/// 由插件初始化在数据库就绪之后调用。任何失败都只打印日志：管理后台不可用
/// 不应该阻止机器人本体上线。
pub(crate) fn spawn(config: &AdminConfig, bot: Option<Arc<RuntimeBot>>) {
    if !config.enabled() {
        println!("[INFO] 管理后台已关闭 (admin.enabled = false)");
        return;
    }

    let token = match token::resolve(config) {
        Ok(token) => token,
        Err(error) => {
            eprintln!("[ERROR] 管理后台无法确定登录 Token，已停用: {error}");
            return;
        }
    };

    let address = match format!("{}:{}", config.host(), config.port()).parse::<SocketAddr>() {
        Ok(address) => address,
        Err(error) => {
            eprintln!(
                "[ERROR] admin.host/admin.port 不是可监听地址 ({}:{}): {error}",
                config.host(),
                config.port()
            );
            return;
        }
    };

    let state = Arc::new(AdminState::new(
        token.clone(),
        config.session_ttl_secs(),
        bot,
    ));
    let session_hours = config.session_ttl_secs() / 3_600;

    // 标注目录由 admin.annotation_dir 推导，启动时就建好：运维部署完要能把批次
    // scp 进来（scp 到不存在的目录会直接失败），不该先让人手工 mkdir。建不出来
    // 只告警，标注页会把原因显示出来。
    match annotation_api::ensure_dir() {
        Ok(path) => println!("[INFO] 数据标注目录: {}", path.display()),
        Err(error) => eprintln!("[WARN] 数据标注目录不可用: {}", error.message),
    }

    kovi::tokio::spawn(async move {
        let listener = match kovi::tokio::net::TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("[ERROR] 管理后台无法监听 {address}: {error}");
                return;
            }
        };
        println!("[INFO] 芸汐管理后台已启动: http://{address}/");
        if !token::came_from_environment() {
            println!(
                "[INFO] 登录 Token 见 {}（首次自动生成，权限 600）",
                token::token_path().display()
            );
        }
        println!("[INFO] 登录会话有效期: {session_hours} 小时");
        if let Err(error) = axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            eprintln!("[ERROR] 管理后台已退出: {error}");
        }
    });
}

/// 首页：带 `?token=` 时先换会话 Cookie 再跳转，否则返回登录页。
async fn index(
    axum::extract::State(state): axum::extract::State<Arc<AdminState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(query): axum::extract::Query<IndexQuery>,
) -> Response {
    if query
        .token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
    {
        // 只认白名单里的落地页，避免把 `page` 变成任意跳转。
        let target = match query.page.as_deref() {
            Some("config") => "/#/config",
            Some("memory") => "/#/memory",
            Some("annotation") => "/#/annotation",
            Some("system") => "/#/system",
            _ => "/",
        };
        let secure = auth::is_secure_request(&headers);
        return match auth::login_via_link(&state, query.token.as_deref(), target, secure).await {
            Ok(response) => response,
            // 链接里的 Token 不对时不要停在带 Token 的地址上，但要让登录页
            // 说清楚为什么没进去。
            Err(_) => axum::response::Redirect::to("/?error=token").into_response(),
        };
    }
    assets::index().await
}

/// OneBot 服务端信息：登录号、在线状态、NapCat 版本。
///
/// 带 60 秒缓存：这些值几乎不变，而每次翻系统信息页都打一次服务端既没必要，
/// 也会在对端卡住时拖慢页面。3 秒超时保证"对端挂了"只表现为这一块未知。
async fn onebot_info(state: &Arc<AdminState>) -> Value {
    const TTL: std::time::Duration = std::time::Duration::from_secs(60);
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    if let Ok(cache) = state.onebot_cache.lock()
        && let Some((at, value)) = cache.as_ref()
        && at.elapsed() < TTL
    {
        return value.clone();
    }

    let Some(bot) = state.bot.clone() else {
        return json!({ "available": false, "detail": "机器人句柄不可用" });
    };

    let call = async {
        let login = bot.get_login_info().await.ok();
        let version = bot.get_version_info().await.ok();
        let status = bot.get_status().await.ok();
        json!({
            "available": true,
            "login": login.map(|response| response.data),
            "version": version.map(|response| response.data),
            "status": status.map(|response| response.data),
        })
    };

    let value = match kovi::tokio::time::timeout(TIMEOUT, call).await {
        Ok(value) => value,
        Err(_) => json!({ "available": false, "detail": "OneBot 服务端 3 秒内没有响应" }),
    };
    if let Ok(mut cache) = state.onebot_cache.lock() {
        *cache = Some((Instant::now(), value.clone()));
    }
    value
}

#[derive(serde::Deserialize)]
struct IndexQuery {
    #[serde(default)]
    token: Option<String>,
    /// 可选落地页（白名单：overview / config / memory），便于做深链接书签。
    #[serde(default)]
    page: Option<String>,
}

/// 组装路由。
pub(crate) fn router(state: Arc<AdminState>) -> Router {
    let protected = Router::new()
        .route("/api/session", get(auth::session))
        .route("/api/logout", post(auth::logout))
        .route("/api/status", get(status_api::status))
        .route("/api/system", get(status_api::system))
        .route("/api/config/files", get(config_api::list_files))
        .route(
            "/api/config/file/{name}",
            get(config_api::read_file).put(config_api::write_raw),
        )
        .route("/api/config/patch", post(config_api::patch))
        .route("/api/config/reload", post(config_api::reload))
        .route("/api/config/backups", get(config_api::list_backups))
        .route("/api/config/restore", post(config_api::restore_backup))
        .route("/api/memory/overview", get(memory_api::overview))
        .route("/api/memory/records", get(memory_api::records))
        .route("/api/memory/record/{kind}/{id}", get(memory_api::record))
        .route("/api/memory/people", get(memory_api::people))
        .route("/api/memory/person/{id}", get(memory_api::person))
        .route("/api/memory/graph", get(memory_api::graph))
        .route("/api/memory/tags", get(memory_api::tags))
        .route("/api/memory/stats", get(memory_api::stats))
        .route("/api/annotation/batches", get(annotation_api::batches))
        .route("/api/annotation/queue", get(annotation_api::queue))
        .route("/api/annotation/sample", get(annotation_api::sample))
        .route("/api/annotation/mark", post(annotation_api::mark))
        .route("/api/annotation/export", post(annotation_api::export))
        .route("/api/annotation/download", get(annotation_api::download))
        .route(
            "/api/stickers",
            get(sticker_api::list).post(sticker_api::upload).layer(
                // 上传体是原始图片字节，最大可到几 MB；axum 默认只收 2 MB。
                // 这一层只挂在这个路由上：别的接口（配置、记忆）不该跟着放宽。
                sticker_api::body_limit(),
            ),
        )
        .route(
            "/api/stickers/file/{name}",
            get(sticker_api::download).delete(sticker_api::remove),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_session,
        ));

    Router::new()
        .route("/", get(index))
        .route("/assets/app.css", get(assets::css))
        .route("/assets/app.js", get(assets::js))
        .route("/api/login", post(auth::login))
        .route("/api/login/challenge", post(auth::challenge))
        .merge(protected)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::AdminState;

    #[test]
    fn token_comparison_accepts_only_the_exact_token() {
        let state = AdminState::for_test("s3cret-token");
        assert!(state.token_matches("s3cret-token"));
        assert!(!state.token_matches("s3cret-toke"));
        assert!(!state.token_matches("s3cret-tokenn"));
        assert!(!state.token_matches(""));
        assert!(!state.token_matches("S3cret-token"));
    }

    #[test]
    fn challenge_proofs_verify_and_reject_tampering() {
        let state = AdminState::for_test("s3cret-token");
        let proof = crate::admin::auth::login_proof("nonce-1", "s3cret-token");
        assert!(state.token_matches_proof("nonce-1", &proof));
        // 换个 nonce 就不再成立（防重放靠 nonce 单次使用，这里防的是拼装）。
        assert!(!state.token_matches_proof("nonce-2", &proof));
        // 改一位就不成立。
        let tampered = format!("{}0", &proof[..proof.len() - 1]);
        assert!(!state.token_matches_proof("nonce-1", &tampered));
        assert!(!state.token_matches_proof("nonce-1", ""));
    }

    #[test]
    fn pending_restart_accumulates_unique_sections() {
        let state = AdminState::for_test("t");
        assert!(state.pending_restart().is_empty());
        state.note_pending_restart(&["tools".to_string(), "admin".to_string()]);
        state.note_pending_restart(&["tools".to_string()]);
        let mut pending = state.pending_restart();
        pending.sort();
        assert_eq!(pending, vec!["admin".to_string(), "tools".to_string()]);
    }

    #[test]
    fn empty_tokens_never_match_each_other() {
        // 配置失误导致 Token 为空时，不能变成"空 Token 就能登录"。
        let state = AdminState::for_test("");
        assert!(!state.token_matches(""));
    }
}
