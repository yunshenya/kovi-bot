//! # 服务器配置模块
//!
//! 管理AI模型服务器的连接配置

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use url::Url;

/// 服务器配置结构体
///
/// 包含连接AI模型服务器所需的配置信息
#[derive(Deserialize, Debug, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    /// 是否启用外部强模型。关闭后 Core/Intrinsic 仍可独立运行。
    enabled: bool,
    /// AI模型服务器API地址
    url: String,
    /// 使用的模型名称
    model_name: String,
    /// API 协议：chat_completions 或 responses
    wire_api: String,
    /// 当前主模型是否可以直接接收图片
    supports_vision: bool,
    /// 读取主模型 Token 的环境变量名
    api_key_env: String,
    /// 是否要求主模型携带 Bearer Token
    requires_auth: bool,
    /// 可选的自定义请求头 x-openai-actor-authorization
    actor_authorization: String,
    /// 单次回复允许模型生成的最大 token 数
    max_output_tokens: u32,
    /// 单次 HTTP 请求超时秒数
    request_timeout_secs: u64,
    /// 可重试错误的额外重试次数
    max_retries: u8,
}

impl ServerConfig {
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_str()
    }

    pub fn wire_api(&self) -> &str {
        self.wire_api.as_str()
    }

    pub fn supports_vision(&self) -> bool {
        self.supports_vision
    }

    pub fn api_key_env(&self) -> &str {
        self.api_key_env.as_str()
    }

    pub fn requires_auth(&self) -> bool {
        self.requires_auth
    }

    pub fn actor_authorization(&self) -> &str {
        self.actor_authorization.as_str()
    }

    pub fn endpoint(&self) -> String {
        let base_url = self.url.trim_end_matches('/');
        let suffix = if self.wire_api == "responses" {
            "/responses"
        } else {
            "/chat/completions"
        };
        if base_url.ends_with(suffix) {
            base_url.to_string()
        } else {
            format!("{base_url}{suffix}")
        }
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    pub fn request_timeout_secs(&self) -> u64 {
        self.request_timeout_secs
    }

    pub fn max_retries(&self) -> u8 {
        self.max_retries
    }

    /// 验证服务器配置
    pub fn validate(&self) -> anyhow::Result<()> {
        if !matches!(self.wire_api.as_str(), "responses" | "chat_completions") {
            return Err(anyhow::anyhow!(
                "server.wire_api 只支持 responses 或 chat_completions"
            ));
        }
        if self.max_output_tokens < 128 {
            return Err(anyhow::anyhow!("server.max_output_tokens 不能小于 128"));
        }
        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!("server.request_timeout_secs 必须大于 0"));
        }

        if !self.enabled {
            println!("[INFO] 外部强模型已禁用，使用本地 Core/Intrinsic 能力");
            return Ok(());
        }

        if self.url.is_empty() {
            return Err(anyhow::anyhow!("服务器URL不能为空"));
        }

        validate_model_url(&self.url)?;

        if self.model_name.is_empty() {
            return Err(anyhow::anyhow!("模型名称不能为空"));
        }
        if self.api_key_env.trim().is_empty() {
            return Err(anyhow::anyhow!("server.api_key_env 不能为空"));
        }

        println!(
            "[INFO] 服务器配置验证通过: URL={}, Model={}",
            self.url, self.model_name
        );
        Ok(())
    }
}

fn validate_model_url(raw_url: &str) -> anyhow::Result<()> {
    let url = Url::parse(raw_url).map_err(|_| anyhow::anyhow!("服务器URL格式无效"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow::anyhow!("服务器URL不能携带用户名或密码"));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(url.host_str().unwrap_or_default()) => Ok(()),
        "http" => Err(anyhow::anyhow!(
            "非本机模型端点必须使用 HTTPS，避免 API Token 明文传输"
        )),
        _ => Err(anyhow::anyhow!(
            "服务器URL只支持 HTTPS；本机回环地址可使用 HTTP"
        )),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "https://api.deepseek.com/chat/completions".to_string(),
            model_name: "deepseek-v4-flash".to_string(),
            wire_api: "chat_completions".to_string(),
            supports_vision: false,
            api_key_env: "BOT_API_TOKEN".to_string(),
            requires_auth: true,
            actor_authorization: String::new(),
            max_output_tokens: 1_200,
            request_timeout_secs: 60,
            max_retries: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServerConfig;

    #[test]
    fn deepseek_default_is_text_only() {
        let config = ServerConfig::default();
        assert!(config.enabled());
        assert_eq!(config.model_name(), "deepseek-v4-flash");
        assert_eq!(config.wire_api(), "chat_completions");
        assert!(!config.supports_vision());
        assert_eq!(config.api_key_env(), "BOT_API_TOKEN");
        assert!(config.requires_auth());
    }

    #[test]
    fn responses_endpoint_is_appended_to_a_provider_base_url() {
        let config = ServerConfig {
            url: "https://example.com/v1".to_string(),
            wire_api: "responses".to_string(),
            ..ServerConfig::default()
        };
        assert_eq!(config.endpoint(), "https://example.com/v1/responses");
    }

    #[test]
    fn plaintext_remote_model_endpoint_is_rejected() {
        let remote = ServerConfig {
            url: "http://example.com/v1".to_string(),
            ..ServerConfig::default()
        };
        assert!(remote.validate().is_err());

        let loopback = ServerConfig {
            url: "http://127.0.0.1:11434/v1".to_string(),
            ..ServerConfig::default()
        };
        assert!(loopback.validate().is_ok());
    }

    #[test]
    fn disabled_external_model_does_not_require_endpoint_or_token() {
        let config = ServerConfig {
            enabled: false,
            url: String::new(),
            model_name: String::new(),
            api_key_env: String::new(),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_ok());
    }
}
