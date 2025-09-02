use crate::config::prompt::Prompt;
use crate::config::server::ServerConfig;
use anyhow::Context;
use config::{Config, FileFormat};
use kovi::toml;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock, LazyLock};

mod prompt;
mod server;

// 使用 Arc<RwLock<>> 来支持并发安全的热重载
static MODEL_CONFIG: LazyLock<Arc<RwLock<ModelConfig>>> =
    LazyLock::new(|| Arc::new(RwLock::new(ModelConfig::load().expect("Failed to load config file"))));

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[serde(default)]
pub struct ModelConfig {
    prompt: Prompt,
    server_config: ServerConfig,
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
        &self.prompt
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }
    
    pub fn reload() -> anyhow::Result<()> {
        let new_config = Self::load()
            .with_context(|| anyhow::anyhow!("Failed to reload config"))?;

        // 获取写锁并更新配置
        let mut config_guard = MODEL_CONFIG.write()
            .map_err(|_| anyhow::anyhow!("Failed to acquire write lock for config"))?;

        *config_guard = new_config;

        Ok(())
    }

    /// 强制重载配置文件（忽略环境变量）
    /// 只从 bot.conf.toml 文件重载配置
    pub fn reload_from_file() -> anyhow::Result<()> {
        let config_path = "bot.conf.toml";
        if !Path::new(config_path).exists() {
            return Err(anyhow::anyhow!("Config file {} does not exist", config_path));
        }

        let new_config = Config::builder()
            .add_source(
                config::File::with_name("bot.conf")
                    .format(FileFormat::Toml)
                    .required(true),
            )
            .build()
            .with_context(|| anyhow::anyhow!("Failed to load config from file"))?
            .try_deserialize::<ModelConfig>()
            .with_context(|| anyhow::anyhow!("Failed to deserialize config from file"))?;

        // 获取写锁并更新配置
        let mut config_guard = MODEL_CONFIG.write()
            .map_err(|_| anyhow::anyhow!("Failed to acquire write lock for config"))?;

        *config_guard = new_config;

        Ok(())
    }

    /// 获取当前配置的克隆
    /// 这样可以避免长时间持有读锁
    pub fn get_current() -> anyhow::Result<Self> {
        let config_guard = MODEL_CONFIG.read()
            .map_err(|_| anyhow::anyhow!("Failed to acquire read lock for config"))?;

        Ok(config_guard.clone())
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

/// 获取当前配置的克隆
/// 不会长时间持有锁，适合大多数使用场景
pub fn get() -> ModelConfig {
    ModelConfig::get_current().expect("Failed to get current config")
}

/// 重载配置的便捷函数
pub fn reload_config() -> anyhow::Result<()> {
    ModelConfig::reload()
}

/// 从文件重载配置的便捷函数
pub fn reload_config_from_file() -> anyhow::Result<()> {
    ModelConfig::reload_from_file()
}