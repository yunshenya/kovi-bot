//! 不可信图片来源的下载、大小和格式边界。

use anyhow::{Result, anyhow};
use base64::Engine;
use image::ImageFormat;
use kovi::tokio::sync::Semaphore;
use reqwest::Client;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;
use std::time::Duration;
use url::{Host, Url};

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_TOTAL_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const MAX_REMOTE_IMAGE_URL_BYTES: usize = 4_096;
pub(crate) const MAX_DATA_IMAGE_URL_BYTES: usize = 14 * 1024 * 1024;
const MAX_CONCURRENT_IMAGE_DOWNLOADS: usize = 4;
const IMAGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IMAGE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);

static IMAGE_DOWNLOAD_LIMIT: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_IMAGE_DOWNLOADS));

#[derive(Debug)]
pub(crate) struct MaterializedImage {
    pub(crate) data_url: String,
    pub(crate) byte_len: usize,
}

pub(crate) fn is_supported_url(raw_url: &str) -> bool {
    if raw_url.starts_with("data:image/") {
        return raw_url.len() <= MAX_DATA_IMAGE_URL_BYTES;
    }
    validate_remote_image_url(raw_url).is_ok()
}

/// OneBot `get_image` 只接收不透明缓存标识，不能把路径或 URL 转交给适配器。
pub(crate) fn is_safe_onebot_image_file(file: &str) -> bool {
    !file.is_empty()
        && file.len() <= 512
        && file != "."
        && file != ".."
        && !file.starts_with('~')
        && !file
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':'))
}

pub(crate) async fn materialize_image_url(
    raw_url: &str,
    remaining_total_bytes: usize,
) -> Result<MaterializedImage> {
    let max_bytes = MAX_IMAGE_BYTES.min(remaining_total_bytes);
    let _permit = IMAGE_DOWNLOAD_LIMIT
        .acquire()
        .await
        .map_err(|_| anyhow!("图片处理并发限制器已关闭"))?;

    if raw_url.starts_with("data:image/") {
        if raw_url.len() > MAX_DATA_IMAGE_URL_BYTES {
            return Err(image_size_error(max_bytes));
        }
        let raw_url = raw_url.to_string();
        return kovi::tokio::task::spawn_blocking(move || {
            let (mime_type, bytes) = decode_validated_image_data_url(&raw_url, max_bytes)?;
            Ok(MaterializedImage {
                data_url: encode_image_data_url(&mime_type, &bytes),
                byte_len: bytes.len(),
            })
        })
        .await
        .map_err(|error| anyhow!("图片解码任务失败: {error}"))?;
    }

    let url = validate_remote_image_url(raw_url)?;
    let (client, allowed_addresses) = pinned_image_client(&url).await?;
    let mut response = client
        .get(url.as_str())
        .send()
        .await
        .map_err(|error| anyhow!("下载图片失败: {error}"))?;
    if !response.status().is_success() {
        return Err(anyhow!("下载图片返回 HTTP {}", response.status()));
    }
    if let Some(remote_address) = response.remote_addr()
        && (!is_public_image_ip(remote_address.ip())
            || !allowed_addresses.contains(&remote_address.ip()))
    {
        return Err(anyhow!("图片连接地址与已校验的公网 DNS 结果不一致"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(image_size_error(max_bytes));
    }

    let declared_content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_image_content_type);
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow!("读取图片内容失败: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(image_size_error(max_bytes));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(anyhow!("图片响应为空"));
    }

    // 部分 QQ 图片 CDN 会返回 application/octet-stream 或缺少 Content-Type。
    // 文件内容仍需通过 PNG/JPEG/WebP 签名校验，不能只凭响应头放行。
    let content_type = resolve_image_content_type(declared_content_type.as_deref(), &bytes)?;

    kovi::tokio::task::spawn_blocking(move || {
        validate_image_signature(&content_type, &bytes)?;
        // QQ 表情/动态图常见 GIF。视觉 Provider 可能不支持 GIF 或直接拒绝
        // 动态格式，统一转为 PNG（只取第一帧）再交给下游，避免整张图片
        // 因“格式不支持”被静默丢弃。
        let (mime_type, bytes) = if content_type == "image/gif" {
            transcode_gif_to_png(&bytes, max_bytes)?
        } else {
            (content_type, bytes)
        };
        Ok(MaterializedImage {
            data_url: encode_image_data_url(&mime_type, &bytes),
            byte_len: bytes.len(),
        })
    })
    .await
    .map_err(|error| anyhow!("图片编码任务失败: {error}"))?
}

fn image_size_error(max_bytes: usize) -> anyhow::Error {
    if max_bytes < MAX_IMAGE_BYTES {
        anyhow!(
            "图片总大小超过 {} MB 限制",
            MAX_TOTAL_IMAGE_BYTES / 1024 / 1024
        )
    } else {
        anyhow!("图片超过 {} MB 限制", MAX_IMAGE_BYTES / 1024 / 1024)
    }
}

pub(crate) fn validate_remote_image_url(raw_url: &str) -> Result<Url> {
    if raw_url.len() > MAX_REMOTE_IMAGE_URL_BYTES {
        return Err(anyhow!("图片 URL 过长"));
    }
    let url = Url::parse(raw_url).map_err(|_| anyhow!("图片 URL 格式无效"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("图片 URL 只允许 HTTP 或 HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("图片 URL 不允许携带用户名或密码"));
    }
    let expected_port = if url.scheme() == "https" { 443 } else { 80 };
    if url.port_or_known_default() != Some(expected_port) {
        return Err(anyhow!("图片 URL 只允许使用对应协议的标准端口"));
    }
    match url.host().ok_or_else(|| anyhow!("图片 URL 缺少主机名"))? {
        Host::Domain(host) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            if host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host == "metadata.google.internal"
            {
                return Err(anyhow!("禁止访问本机或内部图片地址"));
            }
        }
        Host::Ipv4(address) => validate_public_image_ip(IpAddr::V4(address))?,
        Host::Ipv6(address) => validate_public_image_ip(IpAddr::V6(address))?,
    }
    Ok(url)
}

async fn pinned_image_client(url: &Url) -> Result<(Client, HashSet<IpAddr>)> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("图片 URL 缺少端口"))?;
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(IMAGE_CONNECT_TIMEOUT)
        .timeout(IMAGE_DOWNLOAD_TIMEOUT)
        .user_agent("kovi-bot/1.0");
    let mut allowed_addresses = HashSet::new();

    match url.host().ok_or_else(|| anyhow!("图片 URL 缺少主机名"))? {
        Host::Domain(host) => {
            let resolved = kovi::tokio::time::timeout(
                IMAGE_CONNECT_TIMEOUT,
                kovi::tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| anyhow!("解析图片主机超时"))?
            .map_err(|_| anyhow!("无法解析图片主机"))?;
            let mut socket_addresses = Vec::new();
            for address in resolved {
                validate_public_image_ip(address.ip())?;
                allowed_addresses.insert(address.ip());
                if !socket_addresses.contains(&address) {
                    socket_addresses.push(address);
                }
            }
            if socket_addresses.is_empty() {
                return Err(anyhow!("图片主机没有可用的公网地址"));
            }
            builder = builder.resolve_to_addrs(host, &socket_addresses);
        }
        Host::Ipv4(address) => {
            validate_public_image_ip(IpAddr::V4(address))?;
            allowed_addresses.insert(IpAddr::V4(address));
        }
        Host::Ipv6(address) => {
            validate_public_image_ip(IpAddr::V6(address))?;
            allowed_addresses.insert(IpAddr::V6(address));
        }
    }

    let client = builder
        .build()
        .map_err(|error| anyhow!("创建安全图片客户端失败: {error}"))?;
    Ok((client, allowed_addresses))
}

fn validate_public_image_ip(address: IpAddr) -> Result<()> {
    if is_public_image_ip(address) {
        Ok(())
    } else {
        Err(anyhow!("禁止访问本机、内网或保留图片地址"))
    }
}

fn is_public_image_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    ![
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 88, 99, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(224, 0, 0, 0), 4),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
    ]
    .into_iter()
    .any(|(network, prefix)| ipv4_in_network(address, network, prefix))
}

fn ipv4_in_network(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    u32::from(address) & mask == u32::from(network) & mask
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    if segments[0] & 0xe000 != 0x2000 {
        return false;
    }

    // 文档、基准测试、Teredo、ORCHID/ORCHIDv2 与 6to4 均不作为图片源地址。
    !(ipv6_prefix_matches(address, Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32)
        || ipv6_prefix_matches(address, Ipv6Addr::new(0x2001, 0x0002, 0, 0, 0, 0, 0, 0), 48)
        || ipv6_prefix_matches(address, Ipv6Addr::new(0x2001, 0x0010, 0, 0, 0, 0, 0, 0), 28)
        || ipv6_prefix_matches(address, Ipv6Addr::new(0x2001, 0x0020, 0, 0, 0, 0, 0, 0), 28)
        || ipv6_prefix_matches(address, Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32)
        || ipv6_prefix_matches(address, Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16))
}

fn ipv6_prefix_matches(address: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    let address = u128::from(address);
    let network = u128::from(network);
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    };
    address & mask == network & mask
}

fn parse_image_content_type(content_type: &str) -> Option<String> {
    let mime_type = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match mime_type.as_str() {
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" => Some(mime_type),
        "image/jpg" => Some("image/jpeg".to_string()),
        _ => None,
    }
}

fn resolve_image_content_type(declared: Option<&str>, bytes: &[u8]) -> Result<String> {
    if let Some(declared) = declared
        && validate_image_signature(declared, bytes).is_ok()
    {
        return Ok(declared.to_string());
    }

    if let Some(detected) = image_content_type_from_signature(bytes) {
        return Ok(detected.to_string());
    }

    if declared.is_some() {
        Err(anyhow!("图片内容不是受支持的 PNG、JPEG 或 WebP"))
    } else {
        Err(anyhow!("图片响应未声明受支持格式，且无法从文件签名识别"))
    }
}

pub(crate) fn decode_validated_image_data_url(
    raw_url: &str,
    max_bytes: usize,
) -> Result<(String, Vec<u8>)> {
    let (header, encoded) = raw_url
        .split_once(',')
        .ok_or_else(|| anyhow!("视觉图片不是有效的 data URL"))?;
    let mime_type = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .and_then(parse_image_content_type)
        .ok_or_else(|| anyhow!("视觉图片只支持 Base64 编码的 PNG、JPEG 或 WebP"))?;
    let max_encoded_len = max_bytes
        .saturating_add(2)
        .saturating_div(3)
        .saturating_mul(4);
    if encoded.len() > max_encoded_len {
        return Err(image_size_error(max_bytes));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| anyhow!("视觉图片 Base64 解码失败: {error}"))?;
    if bytes.len() > max_bytes {
        return Err(image_size_error(max_bytes));
    }
    validate_image_signature(&mime_type, &bytes)?;
    // 与远程下载路径一致：GIF 统一转 PNG（取第一帧），保证下游只见到
    // PNG/JPEG/WebP。
    if mime_type == "image/gif" {
        transcode_gif_to_png(&bytes, max_bytes)
    } else {
        Ok((mime_type, bytes))
    }
}

/// 把 GIF 解码为 PNG（只取第一帧）。QQ 表情/动态图常是 GIF，若不转码，
/// 视觉 Provider 可能因格式不支持而拒绝，导致图片消息被静默丢弃。
fn transcode_gif_to_png(bytes: &[u8], max_bytes: usize) -> Result<(String, Vec<u8>)> {
    let decoded = image::load_from_memory_with_format(bytes, ImageFormat::Gif)
        .map_err(|error| anyhow!("GIF 图片解码失败: {error}"))?;
    let mut png = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut png);
        decoded
            .write_to(&mut cursor, ImageFormat::Png)
            .map_err(|error| anyhow!("GIF 转 PNG 失败: {error}"))?;
    }
    if png.len() > max_bytes {
        return Err(image_size_error(max_bytes));
    }
    Ok(("image/png".to_string(), png))
}

fn validate_image_signature(mime_type: &str, bytes: &[u8]) -> Result<()> {
    let valid = image_content_type_from_signature(bytes) == Some(mime_type);
    if valid {
        Ok(())
    } else {
        Err(anyhow!("图片内容与声明的格式不匹配"))
    }
}

fn image_content_type_from_signature(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

fn encode_image_data_url(mime_type: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        decode_validated_image_data_url, image_content_type_from_signature, is_public_image_ip,
        is_safe_onebot_image_file, parse_image_content_type, resolve_image_content_type,
        transcode_gif_to_png, validate_image_signature, validate_remote_image_url,
    };
    use base64::Engine;
    use std::net::IpAddr;

    /// 最小合法 1x1 GIF89a（透明像素）。
    fn gif_bytes_1x1() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GIF89a");
        bytes.extend_from_slice(&[
            1, 0, // width = 1
            1, 0, // height = 1
            0x80, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, // global color table
            0x21, 0xf9, 0x04, 0x01, 0, 0, 0, 0, // graphic control extension
            0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0,    // image data
            0x3b, // trailer
        ]);
        bytes
    }

    #[test]
    fn rejects_non_public_or_ambiguous_image_urls() {
        for url in [
            "file:///tmp/image.png",
            "http://localhost/image.png",
            "http://127.0.0.1/image.png",
            "http://2130706433/image.png",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.1/image.png",
            "http://[::1]/image.png",
            "https://example.com:8443/image.png",
            "https://user:password@example.com/image.png",
        ] {
            assert!(
                validate_remote_image_url(url).is_err(),
                "应拒绝不安全地址: {url}"
            );
        }
        assert!(validate_remote_image_url("https://example.com/image.png").is_ok());
        assert!(validate_remote_image_url("http://1.1.1.1/image.png").is_ok());
    }

    #[test]
    fn accepts_only_opaque_onebot_image_file_identifiers() {
        assert!(is_safe_onebot_image_file("A1B2C3D4.image"));
        assert!(is_safe_onebot_image_file("照片-01.jpg"));
        for file in [
            "../secret",
            "/etc/passwd",
            "file:///tmp/a.png",
            "https://example.com/a.png",
            r"C:\temp\a.png",
            "~/.ssh/id_rsa",
        ] {
            assert!(
                !is_safe_onebot_image_file(file),
                "应拒绝非不透明文件标识: {file}"
            );
        }
    }

    #[test]
    fn classifies_private_reserved_and_documentation_ips_as_non_public() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            let address: IpAddr = address.parse().expect("测试 IP 应有效");
            assert!(!is_public_image_ip(address), "应拒绝地址: {address}");
        }
        assert!(is_public_image_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_image_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn validates_data_url_mime_signature_and_decoded_size() {
        let png = b"\x89PNG\r\n\x1a\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let url = format!("data:image/png;base64,{encoded}");
        let (mime_type, bytes) = decode_validated_image_data_url(&url, png.len()).unwrap();
        assert_eq!(mime_type, "image/png");
        assert_eq!(bytes, png);

        let mismatched = format!("data:image/jpeg;base64,{encoded}");
        assert!(decode_validated_image_data_url(&mismatched, png.len()).is_err());
        assert!(decode_validated_image_data_url(&url, png.len() - 1).is_err());
        assert!(decode_validated_image_data_url("data:image/png,not-base64", 100).is_err());
    }

    #[test]
    fn accepts_only_supported_image_content_types() {
        assert_eq!(
            parse_image_content_type("Image/JPEG; charset=binary").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            parse_image_content_type("image/gif").as_deref(),
            Some("image/gif")
        );
        assert!(parse_image_content_type("application/octet-stream").is_none());
        assert!(parse_image_content_type("image/svg+xml").is_none());
    }

    #[test]
    fn detects_supported_image_from_signature_when_header_is_missing_or_wrong() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(resolve_image_content_type(None, png).unwrap(), "image/png");
        assert_eq!(
            resolve_image_content_type(Some("application/octet-stream"), png).unwrap(),
            "image/png"
        );
        assert_eq!(
            resolve_image_content_type(Some("image/jpeg"), png).unwrap(),
            "image/png"
        );
        assert!(resolve_image_content_type(None, b"not an image").is_err());
    }

    #[test]
    fn gif_is_detected_and_transcoded_to_png_first_frame() {
        let gif = gif_bytes_1x1();
        assert_eq!(image_content_type_from_signature(&gif), Some("image/gif"));
        assert_eq!(resolve_image_content_type(None, &gif).unwrap(), "image/gif");
        validate_image_signature("image/gif", &gif).expect("GIF 签名应匹配");

        let (mime_type, png) = transcode_gif_to_png(&gif, 1024 * 1024).expect("GIF 应能转 PNG");
        assert_eq!(mime_type, "image/png");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image_content_type_from_signature(&png), Some("image/png"));
    }

    #[test]
    fn gif_data_url_is_decoded_and_transcoded_to_png() {
        let gif = gif_bytes_1x1();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&gif);
        let url = format!("data:image/gif;base64,{encoded}");
        let (mime_type, bytes) = decode_validated_image_data_url(&url, 1024 * 1024)
            .expect("GIF data URL 应解码并转码为 PNG");
        assert_eq!(mime_type, "image/png");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
