//! # 服务器配置模块
//!
//! 管理AI模型服务器的连接配置

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use url::Url;

/// 主模型密钥的来源。
///
/// 只用于诊断与后台展示；取密钥本身一律走
/// [`ServerConfig::resolved_api_key`]，免得两处各判一次先后顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// 直接写在配置里（管理后台填的那把）。
    Config,
    /// 来自这个环境变量。
    Environment(String),
    /// 两边都没有。
    Missing,
}

impl ApiKeySource {
    /// 给人看的一句话。`#系统信息`、健康检查与后台概览共用这一处措辞，
    /// 免得三处各写一套、说法还不一样。
    pub fn describe(&self, enabled: bool, requires_auth: bool) -> String {
        if !enabled {
            return "外部模型已禁用".to_string();
        }
        if !requires_auth {
            return "无需密钥".to_string();
        }
        match self {
            Self::Config => "已配置（写在配置里）".to_string(),
            Self::Environment(name) => format!("已配置（环境变量 {name}）"),
            Self::Missing => "未配置".to_string(),
        }
    }
}

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
    /// 主模型 API Key。留空时回退到 `api_key_env` 指向的环境变量。
    ///
    /// 允许直接写在这里是为了管理后台能配：运维不该为了换一把 key 去 SSH 改
    /// systemd 环境变量再重启。它被标注为密钥（后台只回显掩码、写回时留空表示
    /// 不改），并且只该写在运行时覆盖文件（0600）里，不要提交进仓库。
    api_key: String,
    /// 是否要求主模型携带 Bearer Token
    requires_auth: bool,
    /// 可选的自定义请求头 x-openai-actor-authorization
    actor_authorization: String,
    /// Provider reasoning mode. `auto` leaves the provider default untouched;
    /// `disabled` reserves the output budget for visible text. This is useful
    /// for DeepSeek v4, whose hidden reasoning otherwise counts against
    /// `max_tokens` and can leave an otherwise successful stream without text.
    #[serde(default = "default_thinking_mode")]
    thinking_mode: String,
    /// 单次回复允许模型生成的最大 token 数
    max_output_tokens: u32,
    /// 单次 HTTP 请求超时秒数
    request_timeout_secs: u64,
    /// 可重试错误的额外重试次数
    max_retries: u8,
}

fn default_thinking_mode() -> String {
    // The built-in model is DeepSeek v4. Operators using a provider with a
    // different reasoning contract can opt into `auto` explicitly.
    "disabled".to_string()
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

    /// 直接配置的 API Key（空串表示没配，回退环境变量）。
    pub fn api_key(&self) -> &str {
        self.api_key.as_str()
    }

    /// 这一轮实际要用的 API Key。
    ///
    /// 顺序是**配置里的 key → 环境变量 → 没有**：后台填的那把必须能立刻生效，
    /// 否则"配了却不生效"会比没配更难查；环境变量保留为兜底，老部署不受影响。
    pub fn resolved_api_key(&self) -> Option<String> {
        self.resolve_api_key(&|name| std::env::var(name).ok()).0
    }

    /// 密钥从哪来（诊断与后台展示用）。取密钥一律走 [`Self::resolved_api_key`]。
    pub fn api_key_source(&self) -> ApiKeySource {
        self.resolve_api_key(&|name| std::env::var(name).ok()).1
    }

    /// 密钥解析的唯一实现：先看配置，再看环境变量。
    ///
    /// 环境那一路是注入的，测试可以直接喂一张假表——不去改进程级环境变量
    /// （edition 2024 里那是 `unsafe`，并行用例还会互相看见）。
    fn resolve_api_key(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> (Option<String>, ApiKeySource) {
        let configured = self.api_key.trim();
        if !configured.is_empty() {
            return (Some(configured.to_string()), ApiKeySource::Config);
        }
        let env_name = self.api_key_env.trim();
        if env_name.is_empty() {
            return (None, ApiKeySource::Missing);
        }
        match lookup(env_name).filter(|value| !value.trim().is_empty()) {
            Some(value) => (Some(value), ApiKeySource::Environment(env_name.to_string())),
            None => (None, ApiKeySource::Missing),
        }
    }

    /// 密钥缺失时给人看的一句话：说清楚两个位置都可以配，别让人只盯着环境变量。
    pub fn missing_api_key_message(&self) -> String {
        let env_name = self.api_key_env.trim();
        if env_name.is_empty() {
            "未配置主模型密钥（server.api_key 为空，server.api_key_env 也没写）".to_string()
        } else {
            format!("未配置主模型密钥（server.api_key 为空，环境变量 {env_name} 也没有值）")
        }
    }

    pub fn requires_auth(&self) -> bool {
        self.requires_auth
    }

    pub fn actor_authorization(&self) -> &str {
        self.actor_authorization.as_str()
    }

    pub fn thinking_mode(&self) -> &str {
        self.thinking_mode.as_str()
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
        if !matches!(self.thinking_mode.as_str(), "auto" | "disabled") {
            return Err(anyhow::anyhow!(
                "server.thinking_mode 只支持 auto 或 disabled"
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
        if self.api_key.trim().is_empty() && self.api_key_env.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "server.api_key 与 server.api_key_env 至少要有一个：前者直接写密钥（管理后台用），后者指向存放密钥的环境变量"
            ));
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
            api_key: String::new(),
            requires_auth: true,
            actor_authorization: String::new(),
            // The built-in model is DeepSeek v4, whose hidden reasoning can
            // consume the whole visible-output budget when left implicit.
            // Providers that support their own reasoning defaults can opt back
            // into `auto` in bot.conf.toml.
            thinking_mode: default_thinking_mode(),
            max_output_tokens: 1_200,
            request_timeout_secs: 60,
            max_retries: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiKeySource, ServerConfig};

    #[test]
    fn deepseek_default_is_text_only() {
        let config = ServerConfig::default();
        assert!(config.enabled());
        assert_eq!(config.model_name(), "deepseek-v4-flash");
        assert_eq!(config.wire_api(), "chat_completions");
        assert!(!config.supports_vision());
        assert_eq!(config.api_key_env(), "BOT_API_TOKEN");
        assert!(config.requires_auth());
        assert_eq!(config.thinking_mode(), "disabled");
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

    /// 后台填的那把 key 必须能立刻生效；环境变量保留为兜底。这个先后顺序是
    /// "配了却不生效"这类最难查的问题的唯一防线，所以钉住它。
    #[test]
    fn configured_api_key_wins_over_the_environment() {
        let env_name = "KOVI_TEST_KEY_FALLBACK";
        let lookup = |name: &str| (name == env_name).then(|| "from-env".to_string());

        let from_env = ServerConfig {
            api_key: String::new(),
            api_key_env: env_name.to_string(),
            ..ServerConfig::default()
        };
        assert_eq!(
            from_env.resolve_api_key(&lookup),
            (
                Some("from-env".to_string()),
                ApiKeySource::Environment(env_name.to_string())
            )
        );

        let from_config = ServerConfig {
            api_key: "from-config".to_string(),
            api_key_env: env_name.to_string(),
            ..ServerConfig::default()
        };
        assert_eq!(
            from_config.resolve_api_key(&lookup),
            (Some("from-config".to_string()), ApiKeySource::Config)
        );

        // 两边都没有时是"缺失"，而不是空串冒充一把 key。
        let missing = ServerConfig {
            api_key: "   ".to_string(),
            api_key_env: "KOVI_TEST_KEY_UNSET".to_string(),
            ..ServerConfig::default()
        };
        assert_eq!(missing.resolve_api_key(&lookup).0, None);
        assert_eq!(missing.resolve_api_key(&lookup).1, ApiKeySource::Missing);
        assert!(
            missing
                .missing_api_key_message()
                .contains("未配置主模型密钥")
        );

        // 环境变量名没写、配置也没写：一样是缺失，不能 panic。
        let blank = ServerConfig {
            api_key: String::new(),
            api_key_env: String::new(),
            ..ServerConfig::default()
        };
        assert_eq!(blank.resolve_api_key(&lookup).1, ApiKeySource::Missing);
    }

    /// 展示用的措辞只有一处：禁用、无需密钥、已配置、未配置四种说法。
    #[test]
    fn api_key_source_reads_as_one_sentence() {
        assert_eq!(
            ApiKeySource::Config.describe(true, true),
            "已配置（写在配置里）"
        );
        assert_eq!(
            ApiKeySource::Environment("BOT_API_TOKEN".to_string()).describe(true, true),
            "已配置（环境变量 BOT_API_TOKEN）"
        );
        assert_eq!(ApiKeySource::Missing.describe(true, true), "未配置");
        assert_eq!(
            ApiKeySource::Missing.describe(false, true),
            "外部模型已禁用"
        );
        assert_eq!(ApiKeySource::Missing.describe(true, false), "无需密钥");
    }

    /// 直接写密钥时不再强制要求环境变量名，但两个都空要说清楚。
    #[test]
    fn a_configured_key_replaces_the_environment_variable_name() {
        let configured = ServerConfig {
            api_key: "sk-test".to_string(),
            api_key_env: String::new(),
            ..ServerConfig::default()
        };
        assert!(configured.validate().is_ok());

        let neither = ServerConfig {
            api_key: String::new(),
            api_key_env: String::new(),
            ..ServerConfig::default()
        };
        let error = neither.validate().expect_err("两个都空必须拒绝");
        assert!(error.to_string().contains("server.api_key"));
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

    #[test]
    fn missing_thinking_mode_uses_the_safe_builtin_default() {
        let value: kovi::toml::Value = kovi::toml::from_str(
            r#"
                enabled = true
                url = "https://example.com/v1"
                model_name = "example"
                wire_api = "chat_completions"
                supports_vision = false
                api_key_env = "BOT_API_TOKEN"
                requires_auth = true
                actor_authorization = ""
                max_output_tokens = 1200
                request_timeout_secs = 60
                max_retries = 2
            "#,
        )
        .expect("valid server config TOML");
        let config: ServerConfig = value.try_into().expect("server config should deserialize");
        assert_eq!(config.thinking_mode(), "disabled");
    }

    #[test]
    fn unsupported_thinking_mode_is_rejected() {
        let config = ServerConfig {
            thinking_mode: "sometimes".to_string(),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
