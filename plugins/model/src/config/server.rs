use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug, Serialize)]
pub struct ServerConfig {
    #[serde(default="default_url")]
    url: String,
    #[serde(default="default_model_name")]
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
        Self{
            url: default_url(),
            model_name: default_model_name(),
        }
    }
}

fn default_url() -> String { "https://api.siliconflow.cn/v1/chat/completions".to_string() }

fn default_model_name() -> String { "Qwen/QwQ-32B".to_string() }