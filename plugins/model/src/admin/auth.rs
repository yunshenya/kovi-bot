//! Token 登录、会话 Cookie 与接口鉴权。

use super::{AdminState, ApiError};
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// 会话 Cookie 名。
const SESSION_COOKIE: &str = "yunxi_admin";
/// 连续失败多少次开始锁定来源 IP。
const FAILURES_BEFORE_LOCKOUT: u32 = 5;
/// 锁定起点时长，之后按倍数退避。
const LOCKOUT_BASE: Duration = Duration::from_secs(60);
/// 锁定上限。
const LOCKOUT_MAX: Duration = Duration::from_secs(15 * 60);
/// 最多同时保留多少条已登录会话，防止异常客户端把内存撑大。
const MAX_SESSIONS: usize = 64;
/// 一个来源 IP 的失败记录最多保留多久。
const FAILURE_TTL: Duration = Duration::from_secs(30 * 60);
/// 登录挑战的有效期。
const CHALLENGE_TTL: Duration = Duration::from_secs(120);
/// 同时最多保留多少个未使用的挑战。
const MAX_CHALLENGES: usize = 64;

/// 一次性登录挑战。
///
/// 浏览器先用 `sha256(nonce + ":" + token)` 做应答，服务端再算一遍比对，
/// 于是**原始 Token 不上网**（参考实现里 NapCat 也做客户端摘要）。nonce 单次
/// 使用且两分钟过期，因此抓到的应答无法重放。取不到 `crypto.subtle` 的老浏览器
/// 会退回明文提交，此时至少还有回环监听与失败限速兜底。
#[derive(Default)]
pub(crate) struct Challenges {
    inner: Mutex<HashMap<String, Instant>>,
}

impl Challenges {
    fn issue(&self) -> String {
        let nonce = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        if let Ok(mut guard) = self.inner.lock() {
            let now = Instant::now();
            guard.retain(|_, expires_at| *expires_at > now);
            if guard.len() >= MAX_CHALLENGES
                && let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, expires_at)| **expires_at)
                    .map(|(nonce, _)| nonce.clone())
            {
                guard.remove(&oldest);
            }
            guard.insert(nonce.clone(), now + CHALLENGE_TTL);
        }
        nonce
    }

    /// 校验并消费一个挑战；过期、未知或重复使用都返回 false。
    fn consume(&self, nonce: &str) -> bool {
        let Ok(mut guard) = self.inner.lock() else {
            return false;
        };
        match guard.remove(nonce) {
            Some(expires_at) => expires_at > Instant::now(),
            None => false,
        }
    }
}

/// 客户端应答值的计算方式：`sha256(nonce + ":" + token)` 的小写十六进制。
pub(crate) fn login_proof(nonce: &str, token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(nonce.as_bytes());
    hasher.update(b":");
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 已登录会话表。
///
/// 只放在内存里：重启即全部失效，这符合"登录态是可丢失运行状态"的定位，
/// 也避免把长期凭据写进数据库。
pub(crate) struct Sessions {
    ttl: Duration,
    inner: Mutex<HashMap<String, Instant>>,
}

impl Sessions {
    pub(crate) fn new(ttl_secs: u64) -> Self {
        Self {
            ttl: Duration::from_secs(ttl_secs),
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn ttl(&self) -> Duration {
        self.ttl
    }

    /// 新建会话；超出上限时先清掉过期项，仍然满则拒绝。
    fn create(&self) -> Option<String> {
        let mut guard = self.inner.lock().ok()?;
        let now = Instant::now();
        guard.retain(|_, expires_at| *expires_at > now);
        if guard.len() >= MAX_SESSIONS {
            return None;
        }
        let id = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        guard.insert(id.clone(), now + self.ttl);
        Some(id)
    }

    /// 校验会话并按滑动窗口续期。
    fn touch(&self, id: &str) -> bool {
        let Ok(mut guard) = self.inner.lock() else {
            return false;
        };
        let now = Instant::now();
        match guard.get_mut(id) {
            Some(expires_at) if *expires_at > now => {
                *expires_at = now + self.ttl;
                true
            }
            Some(_) => {
                guard.remove(id);
                false
            }
            None => false,
        }
    }

    fn revoke(&self, id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(id);
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|guard| guard.len())
            .unwrap_or_default()
    }
}

/// 登录失败限速：同一个来源 IP 连续失败到阈值后按倍数退避。
#[derive(Default)]
pub(crate) struct LoginGuard {
    inner: Mutex<HashMap<IpAddr, Failures>>,
}

#[derive(Default)]
struct Failures {
    count: u32,
    last_failure: Option<Instant>,
    locked_until: Option<Instant>,
}

impl LoginGuard {
    /// 返回还需要等待的秒数；0 表示可以尝试。
    fn retry_after(&self, ip: IpAddr) -> u64 {
        let Ok(mut guard) = self.inner.lock() else {
            return 0;
        };
        let now = Instant::now();
        guard.retain(|_, entry| {
            entry
                .last_failure
                .is_some_and(|last| now.duration_since(last) < FAILURE_TTL)
        });
        guard
            .get(&ip)
            .and_then(|entry| entry.locked_until)
            .filter(|until| *until > now)
            .map_or(0, |until| until.duration_since(now).as_secs().max(1))
    }

    fn record_failure(&self, ip: IpAddr) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        let entry = guard.entry(ip).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_failure = Some(now);
        if entry.count >= FAILURES_BEFORE_LOCKOUT {
            let over = entry.count - FAILURES_BEFORE_LOCKOUT;
            let backoff = LOCKOUT_BASE
                .saturating_mul(1u32 << over.min(6))
                .min(LOCKOUT_MAX);
            entry.locked_until = Some(now + backoff);
        }
    }

    fn record_success(&self, ip: IpAddr) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(&ip);
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct LoginRequest {
    /// 明文 Token（老浏览器没有 WebCrypto 时的退路）。
    #[serde(default)]
    token: Option<String>,
    /// 挑战值与其应答，二选一。
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    proof: Option<String>,
}

/// `POST /api/login/challenge`：签发一次性挑战。
pub(crate) async fn challenge(State(state): State<Arc<AdminState>>) -> Response {
    Json(json!({
        "nonce": state.challenges.issue(),
        "algorithm": "sha256(nonce + ':' + token)",
        "expires_in_secs": CHALLENGE_TTL.as_secs(),
    }))
    .into_response()
}

/// `POST /api/login`：用 Token（或挑战应答）换会话 Cookie。
pub(crate) async fn login(
    State(state): State<Arc<AdminState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let ip = peer.ip();
    let retry_after = state.guard.retry_after(ip);
    if retry_after > 0 {
        return Err(ApiError::too_many_requests(format!(
            "登录失败次数过多，请 {retry_after} 秒后再试"
        )));
    }

    let accepted = match (request.nonce.as_deref(), request.proof.as_deref()) {
        (Some(nonce), Some(proof)) => {
            // 先消费挑战再比对：无论对错都不能重复使用同一个 nonce。
            let fresh = state.challenges.consume(nonce);
            fresh && state.token_matches_proof(nonce, proof)
        }
        _ => state.token_matches(request.token.as_deref().unwrap_or_default().trim()),
    };
    if !accepted {
        state.guard.record_failure(ip);
        return Err(ApiError::unauthorized("Token 不正确"));
    }
    state.guard.record_success(ip);

    let Some(session) = state.sessions.create() else {
        return Err(ApiError::too_many_requests(
            "已登录会话数达到上限，请先退出其他会话或稍后再试",
        ));
    };

    let ttl_secs = state.sessions.ttl().as_secs();
    let cookie = session_cookie(&session, state.sessions.ttl());
    let mut response = Json(json!({
        "ok": true,
        "session_ttl_secs": ttl_secs,
    }))
    .into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|error| ApiError::internal(format!("无法生成会话 Cookie: {error}")))?,
    );
    Ok(response)
}

/// 拼会话 Cookie。
pub(crate) fn session_cookie(session: &str, ttl: Duration) -> String {
    format!(
        "{SESSION_COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        ttl.as_secs()
    )
}

/// `GET /?token=<token>`：一次性登录入口。
///
/// 对齐参考实现（NapCat 的 `?webui_token=`）：适合做书签、给脚本或监控探针用。
/// 命中后立刻 303 跳到不含 Token 的地址，避免 Token 留在地址栏和前进历史里。
pub(crate) async fn login_via_link(
    state: &AdminState,
    token: Option<&str>,
    target: &str,
) -> Result<Response, ApiError> {
    let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) else {
        return Err(ApiError::bad_request("缺少 token 参数"));
    };
    if !state.token_matches(token) {
        return Err(ApiError::unauthorized("Token 不正确"));
    }
    let Some(session) = state.sessions.create() else {
        return Err(ApiError::too_many_requests("已登录会话数达到上限"));
    };
    let cookie = session_cookie(&session, state.sessions.ttl());
    let mut response = axum::response::Redirect::to(target).into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|error| ApiError::internal(format!("无法生成会话 Cookie: {error}")))?,
    );
    Ok(response)
}

/// `POST /api/logout`：注销当前会话。
pub(crate) async fn logout(State(state): State<Arc<AdminState>>, request: Request) -> Response {
    if let Some(session) = cookie_value(request.headers().get(COOKIE), SESSION_COOKIE) {
        state.sessions.revoke(&session);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_static("yunxi_admin=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    response
}

/// `GET /api/session`：当前登录态（供前端判断是否需要跳登录页）。
pub(crate) async fn session(State(state): State<Arc<AdminState>>) -> Response {
    Json(json!({
        "authenticated": true,
        "sessions": state.sessions.len(),
        "session_ttl_secs": state.sessions.ttl().as_secs(),
    }))
    .into_response()
}

/// 保护所有 `/api/*` 数据接口。
///
/// 浏览器走会话 Cookie，脚本可以直接带 `Authorization: Bearer <token>`。
pub(crate) async fn require_session(
    State(state): State<Arc<AdminState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(bearer) = bearer_token(request.headers().get(AUTHORIZATION))
        && state.token_matches(&bearer)
    {
        return Ok(next.run(request).await);
    }
    if let Some(session) = cookie_value(request.headers().get(COOKIE), SESSION_COOKIE)
        && state.sessions.touch(&session)
    {
        return Ok(next.run(request).await);
    }
    Err(ApiError {
        status: StatusCode::UNAUTHORIZED,
        message: "请先登录".to_string(),
    })
}

fn bearer_token(header: Option<&HeaderValue>) -> Option<String> {
    let value = header?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn cookie_value(header: Option<&HeaderValue>, name: &str) -> Option<String> {
    let raw = header?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(name)
            && let Some(value) = value.strip_prefix('=')
        {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).expect("header 应可构造")
    }

    #[test]
    fn sessions_expire_and_can_be_revoked() {
        let sessions = Sessions::new(3_600);
        let id = sessions.create().expect("应能创建会话");
        assert!(sessions.touch(&id));
        sessions.revoke(&id);
        assert!(!sessions.touch(&id));
    }

    #[test]
    fn expired_sessions_stop_working() {
        let sessions = Sessions::new(0);
        // TTL 为 0 时立即过期；配置校验不允许，但这里确认实现不会永久有效。
        let id = sessions.create().expect("应能创建会话");
        std::thread::sleep(Duration::from_millis(5));
        assert!(!sessions.touch(&id));
    }

    #[test]
    fn login_guard_locks_out_after_repeated_failures() {
        let guard = LoginGuard::default();
        let ip: IpAddr = "127.0.0.1".parse().expect("回环地址应可解析");
        for _ in 0..FAILURES_BEFORE_LOCKOUT {
            assert_eq!(guard.retry_after(ip), 0);
            guard.record_failure(ip);
        }
        assert!(guard.retry_after(ip) > 0);
        guard.record_success(ip);
        assert_eq!(guard.retry_after(ip), 0);
    }

    #[test]
    fn bearer_tokens_are_parsed_case_insensitively() {
        assert_eq!(
            bearer_token(Some(&header("Bearer abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(
            bearer_token(Some(&header("bearer abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(bearer_token(Some(&header("Token abc"))), None);
        assert_eq!(bearer_token(Some(&header("Bearer   "))), None);
    }

    #[test]
    fn cookies_are_parsed_from_a_full_cookie_header() {
        let value = header("other=1; yunxi_admin=deadbeef; trailing=2");
        assert_eq!(
            cookie_value(Some(&value), SESSION_COOKIE).as_deref(),
            Some("deadbeef")
        );
        assert_eq!(cookie_value(Some(&header("other=1")), SESSION_COOKIE), None);
        assert_eq!(
            cookie_value(Some(&header("yunxi_admin=")), SESSION_COOKIE),
            None
        );
    }
}
