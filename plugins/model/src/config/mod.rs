//! # 配置管理模块
//!
//! 提供完整的配置管理功能，包括：
//! - 配置文件加载和验证
//! - 默认配置生成
//! - 线程安全的配置访问
//! - 配置验证和错误处理

use crate::config::group_interjection::GroupInterjectionConfig;
use crate::config::memory::MemoryConfig;
use crate::config::message_batch::MessageBatchConfig;
use crate::config::mood::MoodConfig;
pub use crate::config::proactive::ProactiveConfig;
use crate::config::prompt::Prompt;
pub(crate) use crate::config::server::ServerConfig;
use crate::config::topic::TopicConfig;
pub(crate) use crate::config::traffic::TrafficConfig;
use anyhow::Context;
use config::{Config, FileFormat};
use kovi::toml;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

mod agent_runs;
mod agent_tasks;
mod cognitive_model;
mod executive;
mod group_interjection;
mod identity;
mod memory;
mod message_batch;
mod mind;
mod mood;
mod proactive;
mod prompt;
mod reminders;
mod server;
mod tools;
mod topic;
mod traffic;
mod vision;

pub use agent_runs::AgentRunConfig;
pub use agent_tasks::AgentTaskConfig;
pub use cognitive_model::{CognitiveModelConfig, IntrinsicConfig, ModelFallbackConfig};
pub use executive::{
    ExecutiveAttentionBudgetConfig, ExecutiveCandidateConfig, ExecutiveConfidenceConfig,
    ExecutiveConfig, ExecutiveConsistencyConfig, ExecutiveDecisionRecordConfig,
    ExecutiveExpectationConfig, ExecutivePlanConfig, ExecutivePriorityConfig,
    ExecutiveReflectionConfig,
};
pub use identity::IdentityConfig;
pub use mind::MindConfig;
pub use reminders::ReminderConfig;
pub use tools::{McpServerConfig, ToolsConfig};
pub use vision::VisionConfig;

/// 全局配置实例
///
/// 使用LazyLock确保线程安全的单例模式，在首次访问时加载配置
/// 配置存储在RwLock中，支持多读单写访问
static MODEL_CONFIG: LazyLock<Arc<RwLock<ModelConfig>>> = LazyLock::new(|| {
    Arc::new(RwLock::new(
        ModelConfig::load().expect("Failed to load config file"),
    ))
});

/// 模型配置结构体
///
/// 包含机器人的所有配置信息，包括提示词和服务器配置
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[serde(default)]
pub struct ModelConfig {
    /// Canonical Yunxi identity and owner mapping.
    identity: IdentityConfig,
    /// 提示词配置
    prompt: Prompt,
    /// 服务器配置
    server_config: ServerConfig,
    /// 随机主动消息配置
    proactive: ProactiveConfig,
    /// 群聊未点名接话配置
    group_interjection: GroupInterjectionConfig,
    /// 长期记忆与短期上下文配置
    memory: MemoryConfig,
    /// Persistent Mind v2 state and gradual behavior activation.
    mind: MindConfig,
    /// 连续消息气泡的本地合并配置
    message_batch: MessageBatchConfig,
    /// 情绪缓存与自然漂移配置
    mood: MoodConfig,
    /// 话题去重配置
    topic: TopicConfig,
    /// 入站流量、排队和模型响应资源上限。
    traffic: TrafficConfig,
    /// 模型可自主调用的受限工具。
    tools: ToolsConfig,
    /// 持久化提醒任务配置。
    reminders: ReminderConfig,
    /// 跨群问答任务配置。
    agent_tasks: AgentTaskConfig,
    /// 通用持久化 Agent Run 配置。
    agent_runs: AgentRunConfig,
    /// 图片理解 Provider 路由配置。
    vision: VisionConfig,
    /// Executive v3 deterministic control configuration.
    executive: ExecutiveConfig,
    /// Intrinsic model and bounded fallback configuration.
    #[serde(rename = "model")]
    model: CognitiveModelConfig,
}

impl ModelConfig {
    /// 加载配置文件
    ///
    /// 从 `bot.conf.toml` 文件加载配置，如果文件不存在则创建默认配置
    ///
    /// # 返回值
    /// 成功时返回配置实例，失败时返回错误
    pub fn load() -> anyhow::Result<Self> {
        let config_path = Self::config_path();
        if !config_path.exists() {
            println!(
                "[INFO] 配置文件不存在，创建默认配置文件: {}",
                config_path.display()
            );
            Self::create_default_config_file(&config_path)
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

        self.proactive.validate()?;
        self.group_interjection.validate()?;
        self.memory.validate()?;
        self.mind.validate()?;
        self.message_batch.validate()?;
        self.mood.validate()?;
        self.topic.validate()?;
        self.traffic.validate()?;
        self.tools.validate()?;
        self.reminders.validate()?;
        self.agent_tasks.validate()?;
        self.agent_runs.validate()?;
        self.vision.validate()?;
        self.executive.validate()?;
        self.model.validate()?;
        if !self.vision.mcp_server().is_empty() && !self.tools.enabled() {
            return Err(anyhow::anyhow!(
                "配置 vision.mcp_server 时必须启用 tools.enabled"
            ));
        }
        if !self.vision.mcp_server().is_empty()
            && !self
                .tools
                .mcp_servers()
                .iter()
                .any(|server| server.name() == self.vision.mcp_server())
        {
            return Err(anyhow::anyhow!(
                "vision.mcp_server 必须对应 tools.mcp_servers 中已配置的服务"
            ));
        }

        println!("[INFO] 配置验证通过");
        Ok(())
    }

    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }

    pub fn identity(&self) -> &IdentityConfig {
        &self.identity
    }

    pub fn server_config(&self) -> &ServerConfig {
        &self.server_config
    }

    pub fn proactive(&self) -> &ProactiveConfig {
        &self.proactive
    }

    pub fn group_interjection(&self) -> &GroupInterjectionConfig {
        &self.group_interjection
    }

    pub fn memory(&self) -> &MemoryConfig {
        &self.memory
    }

    pub fn mind(&self) -> &MindConfig {
        &self.mind
    }

    pub fn message_batch(&self) -> &MessageBatchConfig {
        &self.message_batch
    }

    pub fn mood(&self) -> &MoodConfig {
        &self.mood
    }

    pub fn topic(&self) -> &TopicConfig {
        &self.topic
    }

    pub fn traffic(&self) -> &TrafficConfig {
        &self.traffic
    }

    pub fn tools(&self) -> &ToolsConfig {
        &self.tools
    }

    pub fn reminders(&self) -> &ReminderConfig {
        &self.reminders
    }

    pub fn agent_tasks(&self) -> &AgentTaskConfig {
        &self.agent_tasks
    }

    pub fn agent_runs(&self) -> &AgentRunConfig {
        &self.agent_runs
    }

    pub fn vision(&self) -> &VisionConfig {
        &self.vision
    }

    pub fn executive(&self) -> &ExecutiveConfig {
        &self.executive
    }

    pub fn model(&self) -> &CognitiveModelConfig {
        &self.model
    }

    fn create_default_config_file(config_path: &Path) -> anyhow::Result<()> {
        let default_config = ModelConfig::default();
        let toml_content = toml::to_string_pretty(&default_config)
            .with_context(|| anyhow::anyhow!("Failed to serialize default config"))?;
        fs::write(config_path, toml_content).with_context(|| {
            anyhow::anyhow!("Failed to write config file: {}", config_path.display())
        })?;
        Ok(())
    }

    fn try_deserialize_config() -> anyhow::Result<ModelConfig> {
        let config_path = Self::config_path();
        Config::builder()
            .add_source(
                config::File::from(config_path)
                    .format(FileFormat::Toml)
                    .required(true),
            )
            .build()
            .with_context(|| anyhow::anyhow!("Failed to load config from file"))?
            .try_deserialize::<ModelConfig>()
            .with_context(|| anyhow::anyhow!("Failed to deserialize config from file"))
    }

    /// 获取当前配置的克隆
    pub fn get_current() -> anyhow::Result<Self> {
        let config_guard = MODEL_CONFIG
            .read()
            .map_err(|_| anyhow::anyhow!("Failed to acquire read lock for config"))?;

        Ok(config_guard.clone())
    }

    fn config_path() -> PathBuf {
        #[cfg(test)]
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bot.conf.toml");
        #[cfg(not(test))]
        let path = PathBuf::from("bot.conf.toml");
        path
    }
}

/// 获取当前配置的克隆
pub fn get() -> ModelConfig {
    ModelConfig::get_current().expect("Failed to get current config")
}

#[cfg(test)]
mod tests {
    use super::ModelConfig;

    #[test]
    fn complete_default_configuration_is_valid() {
        assert!(ModelConfig::default().validate().is_ok());
    }

    #[test]
    fn repository_configuration_loads_with_all_sections() {
        let config = ModelConfig::load().expect("仓库配置应可加载");
        assert_eq!(config.server_config().thinking_mode(), "disabled");
        assert_eq!(config.memory().max_entries(), 1000);
        assert_eq!(config.mood().cache_ttl_secs(), 300);
        assert_eq!(config.topic().recent_topic_cooldown_secs(), 604_800);
        assert!(!config.prompt().system_prompt().contains("NEXT_MESSAGE"));
        assert!(!config.prompt().private_prompt().contains("NEXT_MESSAGE"));
        assert!(!config.prompt().system_prompt().contains("回复协议"));
        assert!(!config.prompt().private_prompt().contains("回复协议"));
        assert!(!config.prompt().system_prompt().contains("silent 决策"));
        assert!(!config.prompt().private_prompt().contains("silent 决策"));
        assert!(!config.prompt().system_prompt().contains("REPLY_ACTION"));
        assert!(!config.prompt().private_prompt().contains("REPLY_ACTION"));
    }
}
