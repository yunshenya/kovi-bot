use serde::Deserialize;

#[derive(Deserialize, Debug, Default)]
pub struct ServerConfig {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        self.url.as_deref().unwrap_or("https://api.siliconflow.cn/v1/chat/completions")
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_deref().unwrap_or("Qwen/QwQ-32B")
    }
}
