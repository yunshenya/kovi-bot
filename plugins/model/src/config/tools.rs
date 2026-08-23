use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ToolsConfig {
    /// 是否启用模型自主工具调用。
    enabled: bool,
    /// 单次回复最多执行多少轮工具调用。
    max_rounds: u8,
    /// 单次工具调用超时秒数。
    timeout_secs: u64,
    /// 工具结果最多注入模型上下文的字符数。
    max_result_chars: usize,
    /// 是否允许模型查询公开网页。
    web_search_enabled: bool,
    /// 是否允许模型读取公开网页正文。
    web_fetch_enabled: bool,
    /// 网页搜索最多返回多少条结果。
    web_search_max_results: usize,
    /// 网页正文最多保留多少字符。
    web_fetch_max_chars: usize,
    /// 是否允许模型使用新闻专用搜索。
    news_search_enabled: bool,
    /// 是否允许模型查询公开天气数据。
    weather_enabled: bool,
    /// 是否允许模型使用本地安全计算器。
    calculator_enabled: bool,
    /// 是否注册管理员专用健康检查工具。
    health_check_enabled: bool,
    /// 受信任的 MCP stdio 服务。
    mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct McpServerConfig {
    /// 服务在工具名中的稳定标识。
    name: String,
    /// 要启动的可执行文件。
    command: String,
    /// 传给可执行文件的参数。
    args: Vec<String>,
    /// 可选的工作目录。
    cwd: Option<String>,
    /// 仅为这个 MCP 子进程提供的环境变量。
    env: BTreeMap<String, String>,
    /// 从机器人进程环境中按名称传给这个 MCP 子进程的变量。
    inherit_env: Vec<String>,
    /// 只暴露这些工具；空列表表示不暴露任何工具。
    allowed_tools: Vec<String>,
    /// 只允许标记为只读或未声明破坏性的工具。
    read_only: bool,
    /// 是否允许定时任务调用这个 MCP 服务。默认关闭，避免定时任务隐式执行副作用。
    allow_scheduled: bool,
}

impl ToolsConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn max_rounds(&self) -> u8 {
        self.max_rounds
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    pub fn max_result_chars(&self) -> usize {
        self.max_result_chars
    }

    pub fn web_search_enabled(&self) -> bool {
        self.web_search_enabled
    }

    pub fn web_fetch_enabled(&self) -> bool {
        self.web_fetch_enabled
    }

    pub fn web_search_max_results(&self) -> usize {
        self.web_search_max_results
    }

    pub fn web_fetch_max_chars(&self) -> usize {
        self.web_fetch_max_chars
    }

    pub fn news_search_enabled(&self) -> bool {
        self.news_search_enabled
    }

    pub fn weather_enabled(&self) -> bool {
        self.weather_enabled
    }

    pub fn calculator_enabled(&self) -> bool {
        self.calculator_enabled
    }

    pub fn health_check_enabled(&self) -> bool {
        self.health_check_enabled
    }

    pub fn mcp_servers(&self) -> &[McpServerConfig] {
        &self.mcp_servers
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_rounds == 0 || self.max_rounds > 3 {
            return Err(anyhow::anyhow!("tools.max_rounds 必须在 1 到 3 之间"));
        }
        if self.timeout_secs == 0 || self.timeout_secs > 120 {
            return Err(anyhow::anyhow!("tools.timeout_secs 必须在 1 到 120 秒之间"));
        }
        if self.max_result_chars < 500 || self.max_result_chars > 100_000 {
            return Err(anyhow::anyhow!(
                "tools.max_result_chars 必须在 500 到 100000 之间"
            ));
        }
        if self.web_search_max_results == 0 || self.web_search_max_results > 10 {
            return Err(anyhow::anyhow!(
                "tools.web_search_max_results 必须在 1 到 10 之间"
            ));
        }
        if self.web_fetch_max_chars < 500 || self.web_fetch_max_chars > 100_000 {
            return Err(anyhow::anyhow!(
                "tools.web_fetch_max_chars 必须在 500 到 100000 之间"
            ));
        }

        let mut names = HashSet::new();
        for server in &self.mcp_servers {
            server.validate()?;
            if !names.insert(server.name.as_str()) {
                return Err(anyhow::anyhow!(
                    "tools.mcp_servers 中存在重复的 name: {}",
                    server.name
                ));
            }
        }
        Ok(())
    }
}

impl McpServerConfig {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn inherit_env(&self) -> &[String] {
        &self.inherit_env
    }

    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn allow_scheduled(&self) -> bool {
        self.allow_scheduled
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.name.trim().is_empty() || !is_safe_identifier(&self.name) {
            return Err(anyhow::anyhow!(
                "MCP 服务 name 必须只包含字母、数字、下划线或短横线"
            ));
        }
        if self.command.trim().is_empty() {
            return Err(anyhow::anyhow!("tools.mcp_servers.command 不能为空"));
        }
        for key in self.env.keys().chain(&self.inherit_env) {
            if !is_safe_env_name(key) {
                return Err(anyhow::anyhow!(
                    "MCP 服务 {} 的环境变量名无效: {}",
                    self.name,
                    key
                ));
            }
        }
        let mut inherited = HashSet::new();
        for key in &self.inherit_env {
            if !inherited.insert(key.as_str()) || self.env.contains_key(key) {
                return Err(anyhow::anyhow!(
                    "MCP 服务 {} 的 inherit_env 不能重复，也不能与 env 重名",
                    self.name
                ));
            }
        }
        let mut tools = HashSet::new();
        for tool in &self.allowed_tools {
            if tool.trim().is_empty() || !tools.insert(tool.as_str()) {
                return Err(anyhow::anyhow!(
                    "MCP 服务 {} 的 allowed_tools 必须是非空且不重复的名称",
                    self.name
                ));
            }
        }
        Ok(())
    }
}

fn is_safe_identifier(value: &str) -> bool {
    value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn is_safe_env_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rounds: 2,
            timeout_secs: 15,
            max_result_chars: 12_000,
            web_search_enabled: true,
            web_fetch_enabled: true,
            web_search_max_results: 5,
            web_fetch_max_chars: 12_000,
            news_search_enabled: true,
            weather_enabled: true,
            calculator_enabled: true,
            health_check_enabled: true,
            mcp_servers: Vec::new(),
        }
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            inherit_env: Vec::new(),
            allowed_tools: Vec::new(),
            read_only: true,
            allow_scheduled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{McpServerConfig, ToolsConfig};

    #[test]
    fn defaults_enable_bounded_read_tools_without_mcp_servers() {
        let config = ToolsConfig::default();
        assert!(config.enabled());
        assert!(config.web_search_enabled());
        assert!(config.web_fetch_enabled());
        assert!(config.news_search_enabled());
        assert!(config.weather_enabled());
        assert!(config.calculator_enabled());
        assert!(config.health_check_enabled());
        assert!(config.mcp_servers().is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn mcp_environment_passthrough_names_are_validated() {
        let server = McpServerConfig {
            name: "notes".to_string(),
            command: "notes-mcp".to_string(),
            inherit_env: vec!["NOTES_API_TOKEN".to_string()],
            allowed_tools: vec!["search_notes".to_string()],
            ..McpServerConfig::default()
        };
        assert!(server.validate().is_ok());
        assert!(!server.allow_scheduled());

        let invalid = McpServerConfig {
            inherit_env: vec!["NOTES-API-TOKEN".to_string()],
            ..server
        };
        assert!(invalid.validate().is_err());
    }
}
