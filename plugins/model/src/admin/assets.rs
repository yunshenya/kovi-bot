//! 内嵌的前端资源。
//!
//! 刻意不做 Node 构建链：后台界面是几个静态文件，`include_str!` 进二进制后
//! 发布仍然只有"一个可执行文件 + 配置"，不会出现"部署时忘了拷 assets 目录"。

use axum::http::header;
use axum::response::{IntoResponse, Response};

const INDEX: &str = include_str!("assets/index.html");
const CSS: &str = include_str!("assets/app.css");
const JS: &str = include_str!("assets/app.js");

/// 首页。登录页与侧栏都写着"监听在哪"——这是一句安全相关的陈述，必须跟着实际
/// 绑定地址走：配置里打开 `admin.allow_non_loopback` 之后仍然写着"只监听回环地址"，
/// 就是在最需要警惕的那台机器上给出一句假话。
pub(crate) async fn index(loopback_only: bool) -> Response {
    let (login_note, sidebar_note) = if loopback_only {
        (
            "只监听回环地址 · 登录状态存在会话 Cookie 里",
            "仅监听回环地址，登录态保存在会话 Cookie 中。",
        )
    } else {
        (
            "监听非回环地址 · 登录状态存在会话 Cookie 里",
            "监听的是非回环地址（admin.allow_non_loopback = true）：登录态虽在会话 Cookie 中，仍请自行确保网络边界。",
        )
    };
    let body = INDEX
        .replace("{{bind-note-login}}", login_note)
        .replace("{{bind-note-sidebar}}", sidebar_note);
    with_owned_type(body, "text/html; charset=utf-8")
}

pub(crate) async fn css() -> Response {
    with_type(CSS, "text/css; charset=utf-8")
}

pub(crate) async fn js() -> Response {
    with_type(JS, "text/javascript; charset=utf-8")
}

fn with_type(body: &'static str, content_type: &'static str) -> Response {
    with_owned_type(body.to_string(), content_type)
}

fn with_owned_type(body: String, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // 前端是随二进制一起发布的：不要让它被浏览器缓存成旧版本，
            // 否则改了界面却看不到变化，只能靠强制刷新。
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::index;

    async fn body_of(loopback_only: bool) -> String {
        let response = index(loopback_only).await;
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("读取首页响应体");
        String::from_utf8(bytes.to_vec()).expect("首页应当是 UTF-8")
    }

    #[tokio::test]
    async fn index_states_the_actual_bind_scope() {
        let loopback = body_of(true).await;
        assert!(loopback.contains("只监听回环地址"), "回环绑定应如实说明");
        assert!(
            loopback.contains("仅监听回环地址，登录态保存在会话 Cookie 中。"),
            "侧栏说明也要跟着走"
        );
        assert!(!loopback.contains("{{"), "占位符不该漏到页面上: {loopback}");

        let exposed = body_of(false).await;
        assert!(
            exposed.contains("监听非回环地址"),
            "非回环绑定必须说出来，而不是继续写「只监听回环地址」"
        );
        assert!(
            !exposed.contains("只监听回环地址"),
            "非回环时不该再出现只监听回环的说法"
        );
        assert!(!exposed.contains("{{"), "占位符不该漏到页面上");
    }
}
