//! # 配置管理模块
//! 
//! 提供完整的配置管理功能，包括：
//! - 配置文件加载和验证
//! - 自动重载监控
//! - 默认配置生成
//! - 线程安全的配置访问
//! - 配置验证和错误处理

use crate::config::prompt::Prompt;
use crate::config::server::ServerConfig;
use anyhow::Context;
use config::{Config, FileFormat};
use kovi::toml;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, LazyLock, RwLock};
use std::time::Duration;

mod prompt;
mod server;

/// 全局配置实例
/// 
/// 使用LazyLock确保线程安全的单例模式，在首次访问时加载配置
/// 配置存储在RwLock中，支持多读单写访问
static MODEL_CONFIG: LazyLock<Arc<RwLock<ModelConfig>>> =
    LazyLock::new(|| Arc::new(RwLock::new(ModelConfig::load().expect("Failed to load config file"))));

/// 自动重载功能控制标志
static AUTO_RELOAD_ENABLED: AtomicBool = AtomicBool::new(false);
/// 配置监控线程运行状态
static WATCHER_RUNNING: AtomicBool = AtomicBool::new(false);

/// 模型配置结构体
/// 
/// 包含机器人的所有配置信息，包括提示词和服务器配置
#[derive(Debug, Deserialize, Serialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct ModelConfig {
    /// 提示词配置
    prompt: Prompt,
    /// 服务器配置
    server_config: ServerConfig,
}

impl ModelConfig {
    /// 加载配置文件
    /// 
    /// 从 `bot.conf.toml` 文件加载配置，如果文件不存在则创建默认配置
    /// 
    /// # 返回值
    /// 成功时返回配置实例，失败时返回错误
    pub fn load() -> anyhow::Result<Self> {
        let config_path = "bot.conf.toml";
        if !Path::new(config_path).exists() {
            println!("[INFO] 配置文件不存在，创建默认配置文件: {}", config_path);
            Self::create_default_config_file(config_path)
                .with_context(|| anyhow::anyhow!("Failed to create default config file"))?;
        };
        let config = Self::try_deserialize_config()?;
        config.validate()?;
        Ok(config)
    }

    /// 验证配置的有效性
    pub fn validate(&self) -> anyhow::Result<()> {
        // 验证服务器配置
        self.server_config.validate()?;
        
        // 验证提示配置
        self.prompt.validate()?;
        
        println!("[INFO] 配置验证通过");
        Ok(())
    }

    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }

    fn create_default_config_file(config_path: &str) -> anyhow::Result<()> {
        let default_config = ModelConfig::default();
        let toml_content = toml::to_string_pretty(&default_config)
            .with_context(|| anyhow::anyhow!("Failed to serialize default config"))?;
        fs::write(config_path, toml_content)
            .with_context(|| anyhow::anyhow!("Failed to write config file: {}", config_path))?;
        Ok(())
    }

    /// 重载配置文件
    pub fn reload() -> anyhow::Result<()> {
        let new_config = Self::load()
            .with_context(|| anyhow::anyhow!("Failed to reload config"))?;
        let mut config_guard = MODEL_CONFIG.write()
            .map_err(|_| anyhow::anyhow!("Failed to acquire write lock for config"))?;

        *config_guard = new_config;

        Ok(())
    }

    /// 强制重载配置文件（忽略环境变量）
    pub fn reload_from_file() -> anyhow::Result<()> {
        let config_path = "bot.conf.toml";
        if !Path::new(config_path).exists() {
            return Err(anyhow::anyhow!("Config file {} does not exist", config_path));
        }
        let new_config = Self::try_deserialize_config()?;
        let mut config_guard = MODEL_CONFIG.write()
            .map_err(|_| anyhow::anyhow!("Failed to acquire write lock for config"))?;
        *config_guard = new_config;
        Ok(())
    }


    fn try_deserialize_config() -> anyhow::Result<ModelConfig> {
        Ok(Config::builder()
            .add_source(
                config::File::with_name("bot.conf")
                    .format(FileFormat::Toml)
                    .required(true),
            )
            .build()
            .with_context(|| anyhow::anyhow!("Failed to load config from file"))?
            .try_deserialize::<ModelConfig>()
            .with_context(|| anyhow::anyhow!("Failed to deserialize config from file"))?)
    }

    /// 获取当前配置的克隆
    pub fn get_current() -> anyhow::Result<Self> {
        let config_guard = MODEL_CONFIG.read()
            .map_err(|_| anyhow::anyhow!("Failed to acquire read lock for config"))?;

        Ok(config_guard.clone())
    }

    /// 启用配置文件自动重载监控
    pub fn enable_auto_reload(check_interval: Duration) {
        if AUTO_RELOAD_ENABLED.load(Ordering::Relaxed) {
            return;
        }

        AUTO_RELOAD_ENABLED.store(true, Ordering::Relaxed);

        if WATCHER_RUNNING.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            std::thread::spawn(move || {
                Self::config_watcher_loop(check_interval);
            });
        }
    }

    /// 禁用配置文件自动重载监控
    pub fn disable_auto_reload() {
        AUTO_RELOAD_ENABLED.store(false, Ordering::Relaxed);
    }

    /// 检查配置文件是否有变化并自动重载
    pub fn check_and_reload() -> anyhow::Result<bool> {
        let config_path = "bot.conf.toml";
        if !Path::new(config_path).exists() {
            return Ok(false);
        }

        let file_config = Self::try_deserialize_config()?;

        // 获取当前内存中的配置
        let current_config = {
            let config_guard = MODEL_CONFIG.read()
                .map_err(|_| anyhow::anyhow!("Failed to acquire read lock for config"))?;
            config_guard.clone()
        };

        // 比较配置是否有变化（只比较文件部分）
        if file_config != current_config {
            Self::reload().with_context(|| anyhow::anyhow!("Failed to reload config after detecting changes"))?;
            return Ok(true);
        }

        Ok(false)
    }


    fn config_watcher_loop(check_interval: Duration) {
        let mut last_check_failed = false;

        loop {
            if !AUTO_RELOAD_ENABLED.load(Ordering::Relaxed) {
                break;
            }

            match Self::check_and_reload() {
                Ok(reloaded) => {
                    if reloaded && last_check_failed {
                        last_check_failed = false;
                    }
                }
                Err(_) => {
                    if !last_check_failed {
                        last_check_failed = true;
                    }
                }
            }

            std::thread::sleep(check_interval);
        }

        WATCHER_RUNNING.store(false, Ordering::Relaxed);
    }

    /// 获取自动重载状态
    pub fn is_auto_reload_enabled() -> bool {
        AUTO_RELOAD_ENABLED.load(Ordering::Relaxed)
    }
}

/// 获取当前配置的克隆
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

/// 启用配置自动重载
pub fn enable_auto_reload(check_interval: Duration) {
    ModelConfig::enable_auto_reload(check_interval);
}

/// 禁用配置自动重载
pub fn disable_auto_reload() {
    ModelConfig::disable_auto_reload();
}

/// 手动检查并重载配置
pub fn check_and_reload() -> anyhow::Result<bool> {
    ModelConfig::check_and_reload()
}

/// 获取自动重载状态
pub fn is_auto_reload_enabled() -> bool {
    ModelConfig::is_auto_reload_enabled()
}

