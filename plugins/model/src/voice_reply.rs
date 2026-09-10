//! 让芸汐把回复以 QQ 语音消息的形式说出来。
//!
//! 模型在 `[[REPLY_ACTION]]` 里把这一轮标记为 `voice` 时走这里：用本机 TTS 合成
//! PCM，写成 WAV，再以 OneBot 的 `record` 段交给 NapCat。NapCat 内部会把非 silk
//! 音频自动转成 silk（`convertToNTSilkTct`），所以这里直接给 WAV 即可。
//!
//! 两条硬约束：
//!
//! - **任何一步失败都回退成文字**。语音是表达方式，不该因为 TTS 抖动把回复弄丢。
//! - **音频必须落在 NapCat 能打开的路径上**。NapCat 常常与机器人不在同一个文件
//!   系统命名空间里（我们的部署里 NapCat 跑在容器内），所以写进双方共享的暂存
//!   目录，再把 NapCat 侧的路径交给它（见 [`QqVoiceConfig`]）。

use crate::config::QqVoiceConfig;
use crate::speech::{SpeechClient, pcm_to_wav};
use kovi::Message;
use kovi::bot::message::Segment;
use serde_json::json;
use std::path::{Path, PathBuf};

/// 单条语音的时长上限（秒）。超出部分会被截断，避免模型给出超长文本时发出几十秒的语音。
const MAX_VOICE_SECONDS: usize = 60;

/// 合成一句话并落盘，返回可以直接发送的语音消息。
///
/// 返回 `None` 表示这条不该用语音发出（配置关闭、合成失败、盘写入失败等），
/// 调用方应当回退成文字。
pub(crate) async fn build_voice_message(config: &QqVoiceConfig, text: &str) -> Option<Message> {
    if !config.enabled() {
        return None;
    }
    let spoken = truncate_chars(text.trim(), config.max_chars());
    if spoken.is_empty() {
        return None;
    }

    let client = match SpeechClient::for_tts_only(
        config.tts_url(),
        config.tts_timeout_secs(),
        config.sample_rate(),
    ) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[WARN] 语音消息客户端创建失败，回退成文字: {error}");
            return None;
        }
    };

    let mut stream = match client.synthesize(&spoken).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("[WARN] 语音消息合成失败，回退成文字: {error}");
            return None;
        }
    };
    let sample_rate = stream.sample_rate().max(8_000);
    let max_bytes = sample_rate as usize * 2 * MAX_VOICE_SECONDS;
    let mut pcm: Vec<u8> = Vec::new();
    loop {
        match stream.next_chunk().await {
            Ok(Some(chunk)) => {
                pcm.extend_from_slice(&chunk);
                if pcm.len() >= max_bytes {
                    pcm.truncate(max_bytes);
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("[WARN] 读取语音合成结果失败，回退成文字: {error}");
                return None;
            }
        }
    }
    if pcm.is_empty() {
        eprintln!("[WARN] 语音合成为空，回退成文字");
        return None;
    }

    let path = write_staged_wav(config, &pcm, sample_rate)?;
    let napcat_path = napcat_path_for(config, &path)?;
    Some(Message::from(vec![Segment::new(
        "record",
        json!({ "file": format!("file://{napcat_path}") }),
    )]))
}

/// 把 PCM 写成 WAV 落到暂存目录，并清理超出保留数量的旧文件。
fn write_staged_wav(config: &QqVoiceConfig, pcm: &[u8], sample_rate: u32) -> Option<PathBuf> {
    let dir = PathBuf::from(config.staging_dir());
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("[WARN] 无法创建语音暂存目录 {}: {error}", dir.display());
        return None;
    }
    let path = dir.join(format!("voice-{}.wav", uuid::Uuid::new_v4()));
    if let Err(error) = std::fs::write(&path, pcm_to_wav(pcm, sample_rate)) {
        eprintln!("[WARN] 写入语音文件失败 {}: {error}", path.display());
        return None;
    }
    prune_staged_files(&dir, config.keep_files());
    Some(path)
}

/// 把机器人侧的暂存路径换算成 NapCat 能打开的路径。
fn napcat_path_for(config: &QqVoiceConfig, host_path: &Path) -> Option<String> {
    let host_dir = config.staging_dir().trim_end_matches('/');
    let napcat_dir = config.napcat_staging_dir().trim_end_matches('/');
    let relative = host_path.strip_prefix(host_dir).ok()?;
    Some(format!("{napcat_dir}/{}", relative.display()))
}

/// 只保留最近 `keep` 个语音文件，避免暂存目录无限增长。
fn prune_staged_files(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("voice-"))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    for (_, path) in files.iter().take(files.len() - keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// 按字符（而非字节）截断，避免把多字节汉字截成半个。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars).collect();
    // 尽量切在句末标点上，读出来不会突兀地断在半句。
    let cut = head
        .char_indices()
        .filter(|(_, character)| matches!(character, '。' | '！' | '？' | '!' | '?' | '…' | '；'))
        .map(|(index, character)| index + character.len_utf8())
        .rfind(|index| *index >= head.len() / 2);
    match cut {
        Some(index) => head[..index].to_owned(),
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::{napcat_path_for, truncate_chars};
    use crate::config::QqVoiceConfig;
    use std::path::Path;

    fn staged_config() -> QqVoiceConfig {
        kovi::toml::from_str(
            r#"
enabled = true
staging_dir = "/home/ubuntu/napcat-qq-call/voice"
napcat_staging_dir = "/app/qq-call/voice"
"#,
        )
        .expect("配置应可反序列化")
    }

    #[test]
    fn napcat_path_maps_across_namespaces() {
        let config = staged_config();
        let host = Path::new("/home/ubuntu/napcat-qq-call/voice/voice-abc.wav");
        assert_eq!(
            napcat_path_for(&config, host).as_deref(),
            Some("/app/qq-call/voice/voice-abc.wav")
        );
    }

    #[test]
    fn napcat_path_rejects_paths_outside_the_staging_dir() {
        let config = staged_config();
        assert_eq!(napcat_path_for(&config, Path::new("/etc/passwd")), None);
    }

    #[test]
    fn short_text_is_kept_as_is() {
        assert_eq!(truncate_chars("你好呀", 80), "你好呀");
    }

    #[test]
    fn long_text_cuts_at_a_sentence_boundary() {
        let text =
            "我今天其实有点累了，下午一直在整理房间，还洗了衣服，现在想歇一会儿再继续写东西。";
        let cut = truncate_chars(text, 20);
        assert!(cut.chars().count() <= 20, "截断后不该超长: {cut}");
        assert!(text.starts_with(&cut));
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // 全是多字节汉字，按字节截断会切出半个字。
        let text = "芸".repeat(50);
        assert_eq!(truncate_chars(&text, 10).chars().count(), 10);
    }
}
