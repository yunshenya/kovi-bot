use crate::config::prompt::Prompt;
use crate::config::server::ServerConfig;
use anyhow::Context;
use config::{Config, FileFormat};
use serde::Deserialize;
use std::sync::LazyLock;

mod prompt;
mod server;

static MODEL_CONFIG: LazyLock<ModelConfig> =
    LazyLock::new(|| ModelConfig::load().expect("Failed to load config file"));

#[derive(Debug, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default)]
    prompt: Prompt,
    #[serde(default)]
    server_config: ServerConfig,
}

impl ModelConfig {
    pub fn load() -> anyhow::Result<Self> {
        Config::builder()
            .add_source(
                config::File::with_name("kovi.plugin")
                    .format(FileFormat::Toml)
                    .required(false),
            )
            .add_source(
                config::Environment::with_prefix("APP")
                    .try_parsing(true)
                    .separator("_")
                    .list_separator(","),
            )
            .build()
            .with_context(|| anyhow::anyhow!("Failed to load config"))?
            .try_deserialize()
            .with_context(|| anyhow::anyhow!("Failed to deserialize config"))
    }

    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }
}

pub fn get() -> &'static ModelConfig {
    &MODEL_CONFIG
}
