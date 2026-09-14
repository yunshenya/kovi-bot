//! 表情包素材库接口：在后台里看她能发哪些表情、上传新的、删掉不要的。
//!
//! 素材仍然只有**一个**来源——`qq_sticker.dir` 那个目录。这里不另存一份数据库
//! 副本：后台写入的就是她发送时读的那些文件，所以列表里看到的一定就是能发的，
//! 也不存在"数据库里有、磁盘上没有"这种两边对不上的状态。
//!
//! 上传走原始字节体（`Content-Type` 随便填，标签放在 query 上），不引入 multipart：
//! 一张图就是一个请求体，`curl --data-binary @开心.png '.../api/stickers?label=开心'`
//! 直接可用。校验（大小、格式、标签安全性、数量上限）全在
//! [`crate::sticker_library::store_upload`] 里，这里只负责把它翻成 HTTP 语义。

use super::ApiError;
use super::config_api::directory_writable;
use crate::sticker_library::{self, StickerStoreError};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// 上传体上限。
///
/// `qq_sticker.max_file_kb` 最大可配到 8192 KB，而 axum 默认只收 2 MB——不放开这层，
/// 调大配置也会在进入我们的校验之前被框架以 413 拒掉。这里给到配置上限再加一点余量。
const MAX_UPLOAD_BODY_BYTES: usize = 9 * 1024 * 1024;

/// 挂给路由的 body 限制层。
pub(crate) fn body_limit() -> DefaultBodyLimit {
    DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES)
}

#[derive(Debug, Deserialize)]
pub(crate) struct UploadQuery {
    /// 这张表情的标签（也是文件名主干）。
    label: String,
}

fn store_error(error: StickerStoreError) -> ApiError {
    match error {
        // 请求本身不合法：原样把原因告诉人（标签、格式、大小、数量上限）。
        StickerStoreError::Invalid(message) => ApiError::bad_request(message),
        StickerStoreError::Io(message) => ApiError::internal(message),
    }
}

/// 素材库现状：配置、目录、可写性、数量与清单。
fn library_json() -> Value {
    let config = crate::config::get();
    let sticker = config.qq_sticker();
    let dir = sticker_library::directory_path();
    let entries = sticker_library::listing();
    let files: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "name": entry.name,
                "label": entry.label,
                "bytes": entry.bytes,
                "modified": entry.modified_unix_secs.map(|seconds| seconds.to_string()),
            })
        })
        .collect();
    let labels = entries
        .iter()
        .map(|entry| entry.label.clone())
        .collect::<BTreeSet<_>>()
        .len();
    json!({
        "enabled": sticker.enabled(),
        "dir": dir.display().to_string(),
        "exists": dir.is_dir(),
        "writable": directory_writable(&dir),
        "max_files": sticker.max_files(),
        "max_file_kb": sticker.max_file_kb(),
        "rescan_secs": sticker.rescan_secs(),
        "file_count": entries.len(),
        "label_count": labels,
        "files": files,
    })
}

/// `GET /api/stickers`：素材库现状与清单。
pub(crate) async fn list() -> Result<Json<Value>, ApiError> {
    Ok(Json(library_json()))
}

/// `POST /api/stickers?label=开心`：上传一张表情（请求体就是图片字节）。
///
/// 返回写下的文件名与刷新后的清单：标签相同不会覆盖，而是加编号并存。
pub(crate) async fn upload(
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = sticker_library::store_upload(&query.label, &body).map_err(store_error)?;
    Ok(Json(json!({
        "ok": true,
        "name": name,
        "library": library_json(),
    })))
}

/// `GET /api/stickers/file/{name}`：读原图（后台缩略图用）。
pub(crate) async fn download(Path(name): Path<String>) -> Result<Response, ApiError> {
    let bytes = sticker_library::read_upload(&name).map_err(store_error)?;
    let content_type = HeaderValue::from_static(sticker_library::image_content_type(&bytes));
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        bytes,
    )
        .into_response();
    // 素材随时可能被替换/删掉，别让浏览器把缩略图缓存成旧的。
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// `DELETE /api/stickers/file/{name}`：删掉一张素材。
pub(crate) async fn remove(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    sticker_library::delete_upload(&name).map_err(store_error)?;
    Ok(Json(json!({
        "ok": true,
        "library": library_json(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上传接口的 body 上限必须真的覆盖配置允许的最大值，否则调大
    /// `qq_sticker.max_file_kb` 会在框架层被 413 掉，而我们自己的校验根本看不到。
    #[test]
    fn upload_body_limit_covers_the_largest_configured_file() {
        let largest_configured = 8_192 * 1024;
        assert!(
            MAX_UPLOAD_BODY_BYTES > largest_configured,
            "body 上限必须大于配置允许的最大文件"
        );
    }

    /// 请求错误与机器错误要分开：前者是 400（把原因原样告诉人），后者是 500。
    #[test]
    fn store_errors_map_to_the_right_status() {
        let invalid = store_error(StickerStoreError::Invalid("标签不能为空".to_string()));
        assert_eq!(invalid.into_response().status(), StatusCode::BAD_REQUEST);
        let io = store_error(StickerStoreError::Io("磁盘满了".to_string()));
        assert_eq!(
            io.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        kovi::tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(future)
    }

    /// 一条能被认出来的 PNG 头（这条链路不解码图片，只按文件头认格式）。
    fn png_bytes() -> Vec<u8> {
        vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R',
        ]
    }

    /// 端到端：真 HTTP 服务 + 真目录，把后台这条链路整条走一遍
    /// （带 Token 上传 → 列表 → 取原图 → 越界路径被拒 → 删除）。
    ///
    /// 需要改进程级配置把素材目录指到临时目录，所以按仓库既有约定标 `#[ignore]`，
    /// 由 `ci.yml` 点名单跑；不需要数据库，也不需要 Redis。
    #[test]
    #[ignore = "mutates the process-global config; run via --ignored --exact"]
    fn sticker_api_round_trips_over_http() {
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!(
            "kovi-admin-stickers-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let previous = crate::config::get();
        // 上限抬到 8 MB：下面要验证"超过 axum 默认 2 MB 的图也能传"。
        let source = format!(
            "[qq_sticker]\nenabled = true\ndir = \"{}\"\nmax_file_kb = 8192\n",
            dir.display()
        );
        crate::config::install(crate::config::validate_candidate(&source).expect("候选配置应合法"))
            .expect("应安装测试配置");
        crate::sticker_library::invalidate_index();

        let state = super::super::AdminState::for_test("test-token");
        let app = super::super::router(Arc::clone(&state));
        let png = png_bytes();

        block_on(async move {
            let listener = kovi::tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("应能监听回环端口");
            let address = listener.local_addr().expect("应能读到端口");
            let server = kovi::tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let client = reqwest::Client::new();
            let base = format!("http://{address}");
            let authorized = |request: reqwest::RequestBuilder| request.bearer_auth("test-token");

            // 没有 Token 的请求必须被挡在门外。
            let anonymous = client
                .get(format!("{base}/api/stickers"))
                .send()
                .await
                .expect("应能请求");
            assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

            // 上传：请求体就是图片字节，标签放在 query 上。目录此前不存在，由这一步创建。
            let uploaded =
                authorized(client.post(format!("{base}/api/stickers?label=%E5%BC%80%E5%BF%83")))
                    .body(png.clone())
                    .send()
                    .await
                    .expect("应能上传");
            assert_eq!(uploaded.status(), reqwest::StatusCode::OK);
            let uploaded: Value = uploaded.json().await.expect("应返回 JSON");
            assert_eq!(uploaded["name"], "开心.png");
            assert_eq!(uploaded["library"]["file_count"], 1);
            assert_eq!(uploaded["library"]["label_count"], 1);

            // 同名再传一张：不覆盖，自动加编号，仍然归到「开心」。
            let again =
                authorized(client.post(format!("{base}/api/stickers?label=%E5%BC%80%E5%BF%83")))
                    .body(png.clone())
                    .send()
                    .await
                    .expect("应能上传");
            let again: Value = again.json().await.expect("应返回 JSON");
            assert_eq!(again["name"], "开心-2.png");
            assert_eq!(again["library"]["label_count"], 1);

            // 大于 axum 默认 2 MB 的图也必须能传进来（body 上限这一层真的生效了，
            // 否则会在框架层被 413 掉，我们自己的校验根本看不到）。
            let mut big = png.clone();
            big.resize(3 * 1024 * 1024, 0);
            let big_response =
                authorized(client.post(format!("{base}/api/stickers?label=%E5%A4%A7%E5%9B%BE")))
                    .body(big)
                    .send()
                    .await
                    .expect("应能上传");
            assert_eq!(
                big_response.status(),
                reqwest::StatusCode::OK,
                "3 MB 的图不该被框架层拒掉"
            );

            // 不是图片的字节要被拒，并且说清楚原因。
            let rejected =
                authorized(client.post(format!("{base}/api/stickers?label=%E5%BC%80%E5%BF%83")))
                    .body(b"not an image".to_vec())
                    .send()
                    .await
                    .expect("应能上传");
            assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
            let rejected: Value = rejected.json().await.expect("应返回 JSON");
            assert!(
                rejected["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("图片"),
                "错误信息要说清楚为什么被拒: {rejected}"
            );

            // 列表：两张都在。
            let listing: Value = authorized(client.get(format!("{base}/api/stickers")))
                .send()
                .await
                .expect("应能请求")
                .json()
                .await
                .expect("应返回 JSON");
            assert_eq!(listing["file_count"], 3);
            assert_eq!(listing["enabled"], true);
            assert_eq!(listing["writable"], true);

            // 原图：拿回来的必须就是传上去的字节。
            let bytes =
                authorized(client.get(format!("{base}/api/stickers/file/%E5%BC%80%E5%BF%83.png")))
                    .send()
                    .await
                    .expect("应能取图")
                    .bytes()
                    .await
                    .expect("应能读体");
            assert_eq!(bytes.as_ref(), png.as_slice());

            // 越界路径拼不出目录之外的文件。
            let escape =
                authorized(client.get(format!("{base}/api/stickers/file/..%2Fbot.conf.toml")))
                    .send()
                    .await
                    .expect("应能请求");
            assert!(
                escape.status().is_client_error(),
                "越界文件名必须被拒: {}",
                escape.status()
            );

            // 删除：删掉一张，另一张还在。
            let deleted = authorized(
                client.delete(format!("{base}/api/stickers/file/%E5%BC%80%E5%BF%83-2.png")),
            )
            .send()
            .await
            .expect("应能删除");
            assert_eq!(deleted.status(), reqwest::StatusCode::OK);
            let deleted: Value = deleted.json().await.expect("应返回 JSON");
            assert_eq!(deleted["library"]["file_count"], 2);

            // 再删同一张：要如实说"不存在"，不能假装成功。
            let missing = authorized(
                client.delete(format!("{base}/api/stickers/file/%E5%BC%80%E5%BF%83-2.png")),
            )
            .send()
            .await
            .expect("应能删除");
            assert!(missing.status().is_client_error());

            server.abort();
        });

        crate::sticker_library::invalidate_index();
        crate::config::install(previous).expect("应还原配置");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
