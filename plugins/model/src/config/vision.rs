use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct VisionConfig {
    /// 视觉 Provider：auto、intrinsic、builtin 或 mcp。
    provider: String,
    /// MCP 服务名；必须对应 tools.mcp_servers 中的服务。
    mcp_server: String,
    /// MCP 服务中接收图片分析请求的工具名。
    mcp_tool: String,
    /// 视觉 Provider 单次调用超时秒数。
    timeout_secs: u64,
}

impl VisionConfig {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn mcp_server(&self) -> &str {
        &self.mcp_server
    }

    pub fn mcp_tool(&self) -> &str {
        &self.mcp_tool
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !matches!(
            self.provider.as_str(),
            "auto" | "intrinsic" | "builtin" | "mcp"
        ) {
            return Err(anyhow::anyhow!(
                "vision.provider 必须是 auto、intrinsic、builtin 或 mcp"
            ));
        }
        if self.timeout_secs == 0 || self.timeout_secs > 120 {
            return Err(anyhow::anyhow!(
                "vision.timeout_secs 必须在 1 到 120 秒之间"
            ));
        }
        if !self.mcp_server.is_empty() && !is_safe_identifier(&self.mcp_server) {
            return Err(anyhow::anyhow!(
                "vision.mcp_server 必须只包含字母、数字、下划线或短横线"
            ));
        }
        if self.mcp_tool.trim().is_empty() || self.mcp_tool.chars().any(char::is_control) {
            return Err(anyhow::anyhow!("vision.mcp_tool 必须是非空工具名"));
        }
        if self.provider == "mcp" && self.mcp_server.is_empty() {
            return Err(anyhow::anyhow!(
                "vision.provider = mcp 时必须配置 vision.mcp_server"
            ));
        }
        Ok(())
    }
}

fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            provider: "auto".to_string(),
            mcp_server: String::new(),
            mcp_tool: "analyze_image".to_string(),
            timeout_secs: 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VisionConfig;

    #[test]
    fn defaults_prefer_builtin_with_optional_mcp_fallback() {
        let config = VisionConfig::default();
        assert_eq!(config.provider(), "auto");
        assert_eq!(config.mcp_tool(), "analyze_image");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn mcp_provider_requires_a_server() {
        let config = VisionConfig {
            provider: "mcp".to_string(),
            ..VisionConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn intrinsic_provider_is_valid_without_external_config() {
        let config = VisionConfig {
            provider: "intrinsic".to_string(),
            ..VisionConfig::default()
        };
        assert_eq!(config.provider(), "intrinsic");
        assert!(config.validate().is_ok());
    }
}
