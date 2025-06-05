use serde::Deserialize;

#[derive(Deserialize, Debug, Default)]
pub struct ServerConfig {
    url: Option<String>,
    model_name: Option<String>,
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        self.url.as_deref().unwrap_or("默认url")
    }

    pub fn model_name(&self) -> &str {
        self.model_name.as_deref().unwrap_or("默认名字")
    }
}
