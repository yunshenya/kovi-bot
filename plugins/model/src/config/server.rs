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
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_str()
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
        
        println!("[INFO] 服务器配置验证通过: URL={}, Model={}", self.url, self.model_name);
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            url: "https://api.siliconflow.cn/v1/chat/completions".to_string(),
            model_name: "Qwen/QwQ-32B".to_string(),
        }
    }
}
