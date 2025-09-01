use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    url: String,
    model_name: String,
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_str()
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
