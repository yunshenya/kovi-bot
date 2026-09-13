use serde::{Deserialize, Serialize};

/// 芸汐唱歌的配置。
///
/// 唱歌走一个独立的本地服务（`tools/sing-service`）：它逐字调用本机 TTS，再用
/// Praat 的 PSOLA 把每个字的基频换成音符、时长对齐到音符时值。这里只放"怎么找到
/// 它"和"多久算超时"这类接线参数；旋律模板由服务自己维护并通过
/// `GET /v1/templates` 暴露，避免两处各存一份歌单。
///
/// 默认关闭。开启时要求 `qq_voice.enabled = true`：音频暂存目录与 NapCat 路径映射
/// 都复用语音消息那一套，不重复配置两份路径。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct QqSingConfig {
    /// 是否允许芸汐唱歌。
    enabled: bool,
    /// 歌声合成服务地址；必须是回环地址。
    base_url: String,
    /// 单次合成超时秒数（一首 10 秒的歌在本机约 5 秒渲染完）。
    timeout_secs: u64,
    /// 模板清单缓存秒数：模板很少变，不必每轮都去问一次。
    templates_ttl_secs: u64,
    /// 模型给出的模板不存在时用的兜底模板。
    default_template: String,
}

impl QqSingConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    pub fn templates_ttl_secs(&self) -> u64 {
        self.templates_ttl_secs
    }

    pub fn default_template(&self) -> &str {
        &self.default_template
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !is_loopback_http_url(&self.base_url) {
            return Err(anyhow::anyhow!(
                "qq_sing.base_url 必须是回环地址的 http 地址（127.0.0.1、localhost 或 [::1]）"
            ));
        }
        if self.timeout_secs == 0 || self.timeout_secs > 180 {
            return Err(anyhow::anyhow!(
                "qq_sing.timeout_secs 必须在 1 到 180 秒之间"
            ));
        }
        if !(30..=86_400).contains(&self.templates_ttl_secs) {
            return Err(anyhow::anyhow!(
                "qq_sing.templates_ttl_secs 必须在 30 到 86400 秒之间"
            ));
        }
        if self.default_template.trim().is_empty() {
            return Err(anyhow::anyhow!("qq_sing.default_template 不能为空"));
        }
        Ok(())
    }
}

impl Default for QqSingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://127.0.0.1:6121".to_string(),
            timeout_secs: 45,
            templates_ttl_secs: 600,
            default_template: "zichang-qingkuai".to_string(),
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
    use super::QqSingConfig;

    #[test]
    fn defaults_are_disabled_and_valid() {
        let config = QqSingConfig::default();
        assert!(!config.enabled());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn enabled_requires_a_loopback_service() {
        let remote = QqSingConfig {
            enabled: true,
            base_url: "http://10.0.0.5:6121".to_string(),
            ..QqSingConfig::default()
        };
        assert!(remote.validate().is_err());

        let local = QqSingConfig {
            enabled: true,
            ..QqSingConfig::default()
        };
        assert!(local.validate().is_ok());
    }

    #[test]
    fn timeouts_and_defaults_are_bounded() {
        let zero = QqSingConfig {
            enabled: true,
            timeout_secs: 0,
            ..QqSingConfig::default()
        };
        assert!(zero.validate().is_err());

        let empty_template = QqSingConfig {
            enabled: true,
            default_template: "  ".to_string(),
            ..QqSingConfig::default()
        };
        assert!(empty_template.validate().is_err());
    }
}
