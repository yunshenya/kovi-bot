use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ServerConfig {
    url: String,
    model_name: String,
}

impl ServerConfig {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}
