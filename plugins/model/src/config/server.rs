use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug, Serialize)]
pub struct ServerConfig {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        self.url
            .as_deref()
            .unwrap_or_default()
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_deref().unwrap_or_default()
    }
}


impl Default for ServerConfig {
    fn default() -> Self {
        Self{
            url: Option::from("https://api.siliconflow.cn/v1/chat/completions".to_string()),
            model_name: Option::from("Qwen/QwQ-32B".to_string()),
        }
    }
}