//! 让芸汐把回复唱出来。
//!
//! 唱歌不在机器人进程里做：本机另有一个歌声合成服务（`tools/sing-service`），它逐字
//! 调用现有 TTS，再用 Praat 的 PSOLA 把每个字的基频换成音符、时长对齐到音符时值，
//! 所以唱出来还是她自己的音色。这里负责三件事：
//!
//! - 取回服务的旋律模板清单（带缓存，用来拼提示词、校验模型给的模板名）；
//! - 把"模板 + 歌词"发给服务，拿回一段 WAV；
//! - 把 WAV 落到与语音消息同一个暂存目录，再以 `record` 段交给 NapCat。
//!
//! 任何一步失败都返回 `None`，调用方按"唱歌 → 念出来 → 打字"的顺序回退。

use crate::config::{QqSingConfig, QqVoiceConfig};
use crate::voice_reply::{napcat_path_for, stage_audio_bytes};
use kovi::Message;
use kovi::bot::message::Segment;
use kovi::tokio::sync::Mutex;
use serde::Deserialize;
use serde_json::json;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// 服务返回的模板摘要。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SingTemplate {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mood: String,
    #[serde(default)]
    pub syllables: usize,
}

#[derive(Deserialize)]
struct TemplatesResponse {
    templates: Vec<SingTemplate>,
}

/// 模板清单缓存：`(取回时刻, 模板)`。
type TemplateCache = Option<(Instant, Vec<SingTemplate>)>;

static TEMPLATE_CACHE: LazyLock<Mutex<TemplateCache>> = LazyLock::new(|| Mutex::new(None));

/// 取模板清单；失败时退回上一次的缓存（陈旧也比没有好）。
pub(crate) async fn templates(config: &QqSingConfig) -> Vec<SingTemplate> {
    if !config.enabled() {
        return Vec::new();
    }
    let ttl = Duration::from_secs(config.templates_ttl_secs());
    {
        let cache = TEMPLATE_CACHE.lock().await;
        if let Some((fetched_at, cached)) = cache.as_ref()
            && fetched_at.elapsed() < ttl
        {
            return cached.clone();
        }
    }
    match fetch_templates(config).await {
        Ok(fetched) => {
            let mut cache = TEMPLATE_CACHE.lock().await;
            *cache = Some((Instant::now(), fetched.clone()));
            fetched
        }
        Err(error) => {
            eprintln!("[WARN] 取歌声模板清单失败，沿用上次结果: {error}");
            let cache = TEMPLATE_CACHE.lock().await;
            cache
                .as_ref()
                .map(|(_, cached)| cached.clone())
                .unwrap_or_default()
        }
    }
}

async fn fetch_templates(config: &QqSingConfig) -> anyhow::Result<Vec<SingTemplate>> {
    let url = format!("{}/v1/templates", config.base_url().trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs().max(1)))
        .build()?;
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("模板接口返回 HTTP {}", response.status().as_u16());
    }
    let payload: TemplatesResponse = response.json().await?;
    Ok(payload.templates)
}

/// 把歌词唱成一条可以直接发送的 QQ 语音消息。
///
/// 返回 `None` 表示这条不该/不能用歌声发出（配置关闭、服务不可用、模板缺失、
/// 落盘失败等），调用方应当回退。
pub(crate) async fn build_sing_message(
    voice_config: &QqVoiceConfig,
    sing_config: &QqSingConfig,
    template: &str,
    lyrics: &str,
) -> Option<Message> {
    if !sing_config.enabled() || !voice_config.enabled() {
        return None;
    }
    let lyrics = lyrics.trim();
    if lyrics.is_empty() {
        return None;
    }
    let available = templates(sing_config).await;
    let template = available
        .iter()
        .find(|item| item.id == template)
        .map(|item| item.id.clone())
        .or_else(|| {
            available
                .iter()
                .find(|item| item.id == sing_config.default_template())
                .map(|item| item.id.clone())
        })
        .unwrap_or_else(|| template.to_owned());

    let url = format!("{}/v1/sing", sing_config.base_url().trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(sing_config.timeout_secs().max(1)))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[WARN] 歌声合成客户端创建失败，回退: {error}");
            return None;
        }
    };
    let response = match client
        .post(&url)
        .json(&json!({ "template": template, "lyrics": lyrics }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            eprintln!("[WARN] 歌声合成请求失败，回退: {error}");
            return None;
        }
    };
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        eprintln!(
            "[WARN] 歌声合成失败（HTTP {}），回退: {}",
            status.as_u16(),
            detail.trim()
        );
        return None;
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("[WARN] 读取歌声合成结果失败，回退: {error}");
            return None;
        }
    };
    if bytes.is_empty() {
        eprintln!("[WARN] 歌声合成返回了空音频，回退");
        return None;
    }

    let path = stage_audio_bytes(voice_config, &bytes, "sing")?;
    let napcat_path = napcat_path_for(voice_config, &path)?;
    Some(Message::from(vec![Segment::new(
        "record",
        json!({ "file": format!("file://{napcat_path}") }),
    )]))
}

#[cfg(test)]
mod tests {
    use super::fetch_templates;
    use crate::config::QqSingConfig;

    /// 起一个只回一份模板清单的假服务，验证解析与错误路径。
    async fn spawn_stub(body: &'static str, status: u16) -> String {
        use kovi::tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = kovi::tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub sing");
        let port = listener.local_addr().expect("addr").port();
        kovi::tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0_u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// 配置字段是私有的，测试里用反序列化构造（与投递侧测试同一做法）。
    fn sing_config(base_url: &str) -> QqSingConfig {
        serde_json::from_value(serde_json::json!({
            "enabled": true,
            "base_url": base_url,
        }))
        .expect("qq_sing test config")
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        kovi::tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(future)
    }

    /// 假服务与调用方必须在同一个 runtime：runtime 一丢，后台 accept 任务也没了。
    fn block_on_with_stub<F, Fut>(body: &'static str, status: u16, probe: F) -> Fut::Output
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future,
    {
        block_on(async move { probe(spawn_stub(body, status).await).await })
    }

    #[test]
    fn templates_are_parsed_from_the_service() {
        let templates = block_on_with_stub(
            r#"{"templates":[{"id":"xiaoxingxing","name":"小星星","mood":"童谣","syllables":14}]}"#,
            200,
            |base| async move { fetch_templates(&sing_config(&base)).await },
        )
        .expect("templates should parse");
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].id, "xiaoxingxing");
        assert_eq!(templates[0].syllables, 14);
    }

    #[test]
    fn a_broken_service_is_an_error_not_a_panic() {
        let result = block_on_with_stub("not json", 200, |base| async move {
            fetch_templates(&sing_config(&base)).await
        });
        assert!(result.is_err());
    }
}
