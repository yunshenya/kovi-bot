use serde::{Deserialize, Serialize};

/// 芸汐主动发语音消息的配置。
///
/// 复用通话功能那套本机 TTS 服务（sherpa-onnx，只监听回环），把模型决定用语音
/// 说出来的回复合成成音频，再交给 NapCat 以 QQ 语音消息发出。默认关闭。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct QqVoiceConfig {
    /// 是否允许芸汐主动发语音消息。
    enabled: bool,
    /// 本机语音服务的合成接口；必须是回环地址。
    tts_url: String,
    /// 单次合成超时秒数。
    tts_timeout_secs: u64,
    /// 请求的合成采样率；服务返回的实际采样率以响应头为准。
    sample_rate: u32,
    /// 单条语音的最大字数，超过会被截断，避免合成出几十秒的音频。
    max_chars: usize,
    /// 机器人写入音频文件的目录（宿主机路径）。
    staging_dir: String,
    /// 同一个目录在 NapCat 侧看到的路径。
    ///
    /// NapCat 与机器人常常不在同一个文件系统命名空间里（我们的部署里 NapCat 跑在
    /// 容器内），而 QQ 语音上传要的是 NapCat 能打开的本地文件。所以音频写到一个
    /// 双方都能看到的共享目录，再把 NapCat 侧的路径交给它。非容器部署时两个路径
    /// 填成一样即可。
    napcat_staging_dir: String,
    /// 音频文件保留数量上限；超出后按时间清理最旧的。
    keep_files: usize,
}

impl QqVoiceConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn tts_url(&self) -> &str {
        &self.tts_url
    }

    pub fn tts_timeout_secs(&self) -> u64 {
        self.tts_timeout_secs
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    pub fn staging_dir(&self) -> &str {
        &self.staging_dir
    }

    pub fn napcat_staging_dir(&self) -> &str {
        &self.napcat_staging_dir
    }

    pub fn keep_files(&self) -> usize {
        self.keep_files
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !is_loopback_http_url(&self.tts_url) {
            return Err(anyhow::anyhow!(
                "qq_voice.tts_url 必须是回环地址的 http 地址（127.0.0.1、localhost 或 [::1]）"
            ));
        }
        if self.staging_dir.trim().is_empty() || self.napcat_staging_dir.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "qq_voice 启用时必须同时配置 staging_dir 与 napcat_staging_dir"
            ));
        }
        if self.tts_timeout_secs == 0 || self.tts_timeout_secs > 120 {
            return Err(anyhow::anyhow!(
                "qq_voice.tts_timeout_secs 必须在 1 到 120 秒之间"
            ));
        }
        if !(8_000..=48_000).contains(&self.sample_rate) {
            return Err(anyhow::anyhow!(
                "qq_voice.sample_rate 必须在 8000 到 48000 之间"
            ));
        }
        if self.max_chars == 0 || self.max_chars > 300 {
            return Err(anyhow::anyhow!("qq_voice.max_chars 必须在 1 到 300 之间"));
        }
        if self.keep_files == 0 || self.keep_files > 1000 {
            return Err(anyhow::anyhow!("qq_voice.keep_files 必须在 1 到 1000 之间"));
        }
        Ok(())
    }
}

impl Default for QqVoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tts_url: "http://127.0.0.1:6120/v1/tts".to_string(),
            tts_timeout_secs: 20,
            sample_rate: 16_000,
            max_chars: 80,
            staging_dir: String::new(),
            napcat_staging_dir: String::new(),
            keep_files: 32,
        }
    }
}

fn is_loopback_http_url(value: &str) -> bool {
    let rest = match value.strip_prefix("http://") {
        Some(rest) => rest,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::QqVoiceConfig;

    #[test]
    fn defaults_are_disabled_and_valid() {
        let config = QqVoiceConfig::default();
        assert!(!config.enabled());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn enabled_requires_both_staging_paths() {
        let config = QqVoiceConfig {
            enabled: true,
            ..QqVoiceConfig::default()
        };
        assert!(config.validate().is_err());

        let half = QqVoiceConfig {
            enabled: true,
            staging_dir: "/tmp/voice".to_string(),
            ..QqVoiceConfig::default()
        };
        assert!(half.validate().is_err());

        let complete = QqVoiceConfig {
            enabled: true,
            staging_dir: "/tmp/voice".to_string(),
            napcat_staging_dir: "/app/qq-call/voice".to_string(),
            ..QqVoiceConfig::default()
        };
        assert!(complete.validate().is_ok());
    }

    #[test]
    fn non_loopback_tts_is_rejected() {
        let config = QqVoiceConfig {
            enabled: true,
            tts_url: "http://10.0.0.5:6120/v1/tts".to_string(),
            staging_dir: "/tmp/voice".to_string(),
            napcat_staging_dir: "/tmp/voice".to_string(),
            ..QqVoiceConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
