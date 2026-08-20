//! # 服务器配置模块
//!
//! 管理AI模型服务器的连接配置

use serde::{Deserialize, Serialize};

/// 服务器配置结构体
///
/// 包含连接AI模型服务器所需的配置信息
#[derive(Deserialize, Debug, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ServerConfig {
    /// AI模型服务器API地址
    url: String,
    /// 使用的模型名称
    model_name: String,
    /// 单次回复允许模型生成的最大 token 数
    max_output_tokens: u32,
    /// 单次 HTTP 请求超时秒数
    request_timeout_secs: u64,
    /// 可重试错误的额外重试次数
    max_retries: u8,
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_str()
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
        if self.url.is_empty() {
            return Err(anyhow::anyhow!("服务器URL不能为空"));
        }

        if !self.url.starts_with("http://") && !self.url.starts_with("https://") {
            return Err(anyhow::anyhow!("服务器URL必须以http://或https://开头"));
        }

        if self.model_name.is_empty() {
            return Err(anyhow::anyhow!("模型名称不能为空"));
        }
        if self.max_output_tokens < 128 {
            return Err(anyhow::anyhow!("server.max_output_tokens 不能小于 128"));
        }
        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!("server.request_timeout_secs 必须大于 0"));
        }

        println!(
            "[INFO] 服务器配置验证通过: URL={}, Model={}",
            self.url, self.model_name
        );
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: "https://api.deepseek.com/chat/completions".to_string(),
            model_name: "deepseek-v4-flash".to_string(),
            max_output_tokens: 1_200,
            request_timeout_secs: 60,
            max_retries: 2,
        }
    }
}
