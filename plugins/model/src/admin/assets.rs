//! 内嵌的前端资源。
//!
//! 刻意不做 Node 构建链：后台界面是几个静态文件，`include_str!` 进二进制后
//! 发布仍然只有"一个可执行文件 + 配置"，不会出现"部署时忘了拷 assets 目录"。

use axum::http::header;
use axum::response::{IntoResponse, Response};

const INDEX: &str = include_str!("assets/index.html");
const CSS: &str = include_str!("assets/app.css");
const JS: &str = include_str!("assets/app.js");

pub(crate) async fn index() -> Response {
    with_type(INDEX, "text/html; charset=utf-8")
}

pub(crate) async fn css() -> Response {
    with_type(CSS, "text/css; charset=utf-8")
}

pub(crate) async fn js() -> Response {
    with_type(JS, "text/javascript; charset=utf-8")
}

fn with_type(body: &'static str, content_type: &'static str) -> Response {
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
