use std::fs;
use std::path::Path;
use crate::config::prompt::Prompt;
use crate::config::server::ServerConfig;
use anyhow::Context;
use config::{Config, FileFormat};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use kovi::toml;

mod prompt;
mod server;

static MODEL_CONFIG: LazyLock<ModelConfig> =
    LazyLock::new(|| ModelConfig::load().expect("Failed to load config file"));

static SERVER_CONFIG: LazyLock<ServerConfig> = LazyLock::new(ServerConfig::default);

static PROMPT: LazyLock<Prompt> = LazyLock::new(Prompt::default);

#[derive(Debug, Deserialize, Serialize)]
pub struct ModelConfig {
    #[serde(default)]
    prompt: Option<Prompt>,
    #[serde(default)]
    server_config: Option<ServerConfig>,
}

impl ModelConfig {
    pub fn load() -> anyhow::Result<Self> {
        let config_path = "bot.conf.toml";
        if !Path::new(config_path).exists() {
            Self::create_default_config_file(config_path)
                .with_context(|| anyhow::anyhow!("Failed to create default config file"))?;
        };

        Config::builder()
            .add_source(
                config::File::with_name("bot.conf")
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
        self.prompt.as_ref().unwrap_or(&PROMPT)
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config.as_ref().unwrap_or(&SERVER_CONFIG)
    }

    fn create_default_config_file(config_path: &str) -> anyhow::Result<()> {
        let default_config = ModelConfig::default();
        let toml_content = toml::to_string_pretty(&default_config)
            .with_context(|| anyhow::anyhow!("Failed to serialize default config"))?;
        fs::write(config_path, toml_content)
            .with_context(|| anyhow::anyhow!("Failed to write config file: {}", config_path))?;
        Ok(())
    }
}

pub fn get() -> &'static ModelConfig {
    &MODEL_CONFIG
}


impl Default for ModelConfig {
    fn default() -> Self {
        Self{
            prompt: Option::from(Prompt::default()),
            server_config: Option::from(ServerConfig::default()),
        }
    }
}

#[cfg(test)]
mod model {
    use super::*;


    #[test]
    fn load() {
        ModelConfig::load().unwrap();
    }
}