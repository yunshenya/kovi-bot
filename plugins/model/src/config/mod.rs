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

mod admin;
mod agent_runs;
mod agent_tasks;
mod cognitive_model;
mod executive;
mod gag_ledger;
mod group_interjection;
mod identity;
mod memory;
mod message_batch;
mod mind;
mod mood;
mod proactive;
mod prompt;
mod qq_call;
mod qq_sing;
mod qq_sticker;
mod qq_voice;
mod reminders;
mod server;
mod silence;
mod tools;
mod topic;
mod traffic;
mod understanding;
mod vision;
mod world_model;
mod world_sensors;

pub use admin::AdminConfig;
pub use agent_runs::AgentRunConfig;
pub use agent_tasks::AgentTaskConfig;
pub use cognitive_model::{CognitiveModelConfig, IntrinsicConfig, ModelFallbackConfig};
pub use executive::{
    ExecutiveAttentionBudgetConfig, ExecutiveConfidenceConfig, ExecutiveConfig,
    ExecutiveDecisionRecordConfig, ExecutiveExpectationConfig, ExecutivePlanConfig,
};
pub use gag_ledger::GagLedgerConfig;
pub use identity::IdentityConfig;
pub use mind::MindConfig;
pub use qq_call::QqCallConfig;
pub use qq_sing::QqSingConfig;
pub use qq_sticker::QqStickerConfig;
pub use qq_voice::QqVoiceConfig;
pub use reminders::ReminderConfig;
pub use server::ApiKeySource;
pub use silence::SilenceConfig;
pub use tools::{McpServerConfig, ToolsConfig};
pub use understanding::UnderstandingConfig;
pub use vision::VisionConfig;
pub use world_model::WorldModelConfig;
pub use world_sensors::{WorldSensorConfig, WorldSensorsConfig};

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
    /// 相处信号与静默门控配置（默认只影子观察，不改变可见行为）
    silence: SilenceConfig,
    /// 长期记忆与短期上下文配置
    memory: MemoryConfig,
    /// Persistent Mind v2 state and gradual behavior activation.
    mind: MindConfig,
    /// 连续消息气泡的本地合并配置
    message_batch: MessageBatchConfig,
    /// 情绪缓存与自然漂移配置
    mood: MoodConfig,
    /// Core 接管回合的会话理解（情绪 / 兴趣 / 画像）配置
    understanding: UnderstandingConfig,
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
    /// 世界传感器框架配置（默认关闭，增量化）。
    world_sensors: WorldSensorsConfig,
    /// World Model v4 运行时配置（shadow 模式，默认关闭）。
    world_model: WorldModelConfig,
    /// 梗账本配置（许诺/常驻梗/记仇的结构化有界状态）。
    gag_ledger: GagLedgerConfig,
    /// 图片理解 Provider 路由配置。
    vision: VisionConfig,
    /// QQ 实时语音通话配置（默认关闭）。
    qq_call: QqCallConfig,
    /// 芸汐主动发语音消息的配置（默认关闭）。
    qq_voice: QqVoiceConfig,
    /// 芸汐唱歌的配置（默认关闭）。
    qq_sing: QqSingConfig,
    /// 芸汐发表情包的自有素材库配置（默认关闭）。
    qq_sticker: QqStickerConfig,
    /// Executive v3 deterministic control configuration.
    executive: ExecutiveConfig,
    /// Intrinsic model and bounded fallback configuration.
    #[serde(rename = "model")]
    model: CognitiveModelConfig,
    /// 自带 Web 管理后台（配置 + 记忆）。
    admin: AdminConfig,
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
        self.silence.validate()?;
        self.memory.validate()?;
        self.mind.validate()?;
        self.message_batch.validate()?;
        self.mood.validate()?;
        self.understanding.validate()?;
        self.topic.validate()?;
        self.traffic.validate()?;
        self.tools.validate()?;
        self.reminders.validate()?;
        self.agent_tasks.validate()?;
        self.agent_runs.validate()?;
        self.world_sensors.validate()?;
        self.world_model.validate()?;
        self.gag_ledger.validate()?;
        self.vision.validate()?;
        self.qq_call.validate()?;
        self.qq_voice.validate()?;
        self.qq_sing.validate()?;
        self.qq_sticker.validate()?;
        if self.qq_sing.enabled() && !self.qq_voice.enabled() {
            return Err(anyhow::anyhow!(
                "启用 qq_sing 需要同时启用 qq_voice：唱歌复用语音消息的暂存目录与 NapCat 路径映射"
            ));
        }
        self.executive.validate()?;
        self.model.validate()?;
        self.admin.validate()?;
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

    pub fn silence(&self) -> &SilenceConfig {
        &self.silence
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

    pub fn understanding(&self) -> &UnderstandingConfig {
        &self.understanding
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

    pub fn world_sensors(&self) -> &WorldSensorsConfig {
        &self.world_sensors
    }

    pub fn world_model(&self) -> &WorldModelConfig {
        &self.world_model
    }

    pub fn gag_ledger(&self) -> &GagLedgerConfig {
        &self.gag_ledger
    }

    pub fn vision(&self) -> &VisionConfig {
        &self.vision
    }

    pub fn qq_call(&self) -> &QqCallConfig {
        &self.qq_call
    }

    pub fn qq_voice(&self) -> &QqVoiceConfig {
        &self.qq_voice
    }

    pub fn qq_sing(&self) -> &QqSingConfig {
        &self.qq_sing
    }

    pub fn qq_sticker(&self) -> &QqStickerConfig {
        &self.qq_sticker
    }

    pub fn executive(&self) -> &ExecutiveConfig {
        &self.executive
    }

    pub fn model(&self) -> &CognitiveModelConfig {
        &self.model
    }

    pub fn admin(&self) -> &AdminConfig {
        &self.admin
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
        let override_path = override_file_path();
        let mut builder = Config::builder().add_source(
            config::File::from(config_path)
                .format(FileFormat::Toml)
                .required(true),
        );
        if override_path.exists() {
            // 运行时覆盖配置：发布目录是只读的，运维的改动落在可写的运行时目录里，
            // 后加载的 source 覆盖先加载的（表按字段深合并）。
            builder = builder.add_source(
                config::File::from(override_path.clone())
                    .format(FileFormat::Toml)
                    .required(false),
            );
        }
        builder
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

/// 当前配置文件的路径（管理后台读写的对象）。
pub fn config_file_path() -> PathBuf {
    ModelConfig::config_path()
}

/// 运行时状态目录。
///
/// 生产部署把二进制与 `bot.conf.toml` 放在只读的 release 目录里
/// （systemd 的 `ProtectSystem=strict` + `ReadOnlyPaths`），只有
/// `KOVI_READY_FILE` 所在的 `runtime/` 可写。开发机上没有这个变量，
/// 就退回工作目录，行为不变。
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("KOVI_READY_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 运行时覆盖配置的路径。
///
/// 它叠在主配置之上（同名字段以它为准），因此运维在管理后台里的改动既能立即
/// 生效，也不会因为下一次发布覆盖 release 目录而丢失。
pub fn override_file_path() -> PathBuf {
    std::env::var_os("YUNXI_CONFIG_OVERRIDE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_dir().join(OVERRIDE_FILE))
}

/// 运行时覆盖配置的文件名。
pub const OVERRIDE_FILE: &str = "bot.conf.override.toml";

/// 数据标注批次目录的默认名（相对运行时目录）。
pub const DEFAULT_ANNOTATION_DIR: &str = "turngate";

/// 数据标注批次目录（管理后台的「标注」页读写它）。
///
/// 相对路径以运行时目录为基准：生产部署里 `current/` 是只读发布目录
/// （systemd `ProtectSystem=strict`），只有运行时目录可写；开发机上没有
/// `KOVI_READY_FILE`，于是落在工作目录下。配置为绝对路径时原样使用。
pub fn annotation_dir_path() -> PathBuf {
    let configured = MODEL_CONFIG
        .read()
        .map(|config| config.admin().annotation_dir().trim().to_owned())
        .unwrap_or_default();
    let configured = if configured.is_empty() {
        DEFAULT_ANNOTATION_DIR.to_owned()
    } else {
        configured
    };
    let path = Path::new(&configured);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        runtime_dir().join(path)
    }
}

/// 表情包素材目录的默认名（相对运行时目录）。
pub const DEFAULT_STICKER_DIR: &str = "stickers";

/// 表情包素材目录（芸汐能发的那些图就放在这里）。
///
/// 相对路径的基准与 [`annotation_dir_path`] 一致：生产部署里只有运行时目录可写，
/// 开发机上没有 `KOVI_READY_FILE`，于是落在工作目录下。素材是运维手工丢进去的，
/// 不是运行时写出来的，所以这个目录不要求存在——不存在就是"暂时没有素材"。
pub fn sticker_library_path() -> PathBuf {
    let configured = MODEL_CONFIG
        .read()
        .map(|config| config.qq_sticker().dir().trim().to_owned())
        .unwrap_or_default();
    let configured = if configured.is_empty() {
        DEFAULT_STICKER_DIR.to_owned()
    } else {
        configured
    };
    let path = Path::new(&configured);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        runtime_dir().join(path)
    }
}

/// 把内存中的配置替换为给定实例。
///
/// 只应由管理后台在「候选配置已经通过 `validate_candidate` 校验并原子落盘」
/// 之后调用，否则运行中的进程会和磁盘上的文件不一致。
pub fn install(config: ModelConfig) -> anyhow::Result<()> {
    let mut guard = MODEL_CONFIG
        .write()
        .map_err(|_| anyhow::anyhow!("Failed to acquire write lock for config"))?;
    *guard = config;
    Ok(())
}

/// 校验一段候选 TOML 是否能成为合法配置，但不改动任何状态。
///
/// 管理后台用它做「先校验、后落盘」：校验失败时磁盘与内存都保持原样。
pub fn validate_candidate(source: &str) -> anyhow::Result<ModelConfig> {
    let config = Config::builder()
        .add_source(config::File::from_str(source, FileFormat::Toml))
        .build()
        .with_context(|| anyhow::anyhow!("候选配置不是合法的 TOML"))?
        .try_deserialize::<ModelConfig>()
        .with_context(|| anyhow::anyhow!("候选配置的字段类型或取值不合法"))?;
    config
        .validate()
        .with_context(|| anyhow::anyhow!("候选配置未通过业务校验"))?;
    Ok(config)
}

/// 校验一段候选的**主配置**文本，并叠加磁盘上现存的运行时覆盖，但不改动任何状态。
///
/// 主配置不能单独校验。磁盘上的覆盖会叠在主配置之上（加载顺序见
/// `try_deserialize_config`），所以"这次写入之后，这个进程重新加载会得到什么"必须是
/// `候选主配置 + 现存覆盖`。单独校验主配置有两个后果：
///
/// 1. 跨段规则会被误判——某条规则的另一半只在覆盖里时，一个本来合法的改动会被拒；
/// 2. 更糟的是通过之后：管理后台 `install` 的是这个"主配置单独"的结果，于是**覆盖
///    当场从内存配置里失效**（磁盘上还在），要等下次重启才回来。运维在后台改一个主
///    配置字段，实际连带把覆盖里的设置全丢了，而界面上看不出任何异常。
pub fn validate_main_candidate(source: &str) -> anyhow::Result<ModelConfig> {
    validate_main_candidate_with_override(source, &override_file_path())
}

/// [`validate_main_candidate`] 的实现，覆盖文件路径由调用方给出。
///
/// 路径做成参数而不是在里面读环境变量，是为了能直接测：进程级环境是并行测试互相
/// 污染的来源，这里没必要付那个代价。
pub(crate) fn validate_main_candidate_with_override(
    source: &str,
    override_path: &Path,
) -> anyhow::Result<ModelConfig> {
    let mut builder =
        Config::builder().add_source(config::File::from_str(source, FileFormat::Toml));
    if override_path.exists() {
        builder = builder.add_source(config::File::from(override_path).format(FileFormat::Toml));
    }
    let config = builder
        .build()
        .with_context(|| anyhow::anyhow!("候选主配置不是合法的 TOML"))?
        .try_deserialize::<ModelConfig>()
        .with_context(|| anyhow::anyhow!("候选主配置的字段类型或取值不合法"))?;
    config
        .validate()
        .with_context(|| anyhow::anyhow!("候选主配置 + 运行时覆盖未通过业务校验"))?;
    Ok(config)
}

/// 校验一段候选的运行时覆盖配置，但不改动任何状态。
///
/// 覆盖配置本身是稀疏的（只写要改的字段），所以必须与主配置合并之后再校验，
/// 否则会误判成"字段缺失"。
pub fn validate_override_candidate(source: &str) -> anyhow::Result<ModelConfig> {
    let config = Config::builder()
        .add_source(
            config::File::from(ModelConfig::config_path())
                .format(FileFormat::Toml)
                .required(true),
        )
        .add_source(config::File::from_str(source, FileFormat::Toml))
        .build()
        .with_context(|| anyhow::anyhow!("候选覆盖配置不是合法的 TOML"))?
        .try_deserialize::<ModelConfig>()
        .with_context(|| anyhow::anyhow!("候选覆盖配置的字段类型或取值不合法"))?;
    config
        .validate()
        .with_context(|| anyhow::anyhow!("主配置 + 候选覆盖未通过业务校验"))?;
    Ok(config)
}

/// 按磁盘上的文件重新加载配置并热替换。
pub fn reload_from_disk() -> anyhow::Result<ModelConfig> {
    let config = ModelConfig::load()?;
    install(config.clone())?;
    Ok(config)
}

/// 获取当前配置的克隆
pub fn get() -> ModelConfig {
    ModelConfig::get_current().expect("Failed to get current config")
}

/// 只读一个布尔开关：调用方（提示词组装、投递）只需要问「能不能发语音」，
/// 不该为这一个 bit 克隆整份配置。配置未就绪时返回 false。
pub fn qq_voice_enabled() -> bool {
    MODEL_CONFIG
        .read()
        .map(|config| config.qq_voice().enabled())
        .unwrap_or(false)
}

/// 同上，问「能不能唱歌」。
pub fn qq_sing_enabled() -> bool {
    MODEL_CONFIG
        .read()
        .map(|config| config.qq_sing().enabled())
        .unwrap_or(false)
}

/// 同上，问「能不能发表情包」。
pub fn qq_sticker_enabled() -> bool {
    MODEL_CONFIG
        .read()
        .map(|config| config.qq_sticker().enabled())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{ModelConfig, Prompt};
    use config::{Config, FileFormat};

    /// 主配置写入必须叠加磁盘上的运行时覆盖——那才是写完重新加载会得到的配置。
    ///
    /// 单独校验主配置的后果不只是"误判跨段规则"：管理后台 `install` 的是校验结果，
    /// 于是覆盖会**当场从内存里失效**（磁盘上还在），要等下次重启才回来。这条同时
    /// 钉住"旧行为会丢覆盖"，免得有人把合并那步删掉。
    /// 发布后要做的生产配置变更，先在这里按真实合并路径验一遍。
    ///
    /// 现场长这样：主配置 `bot.conf.toml` 里还是**旧的两段全文**（人格 + 场景各写一份，
    /// 且以"你叫芸汐…"开头），override 里新增 `[prompt]` 的 persona + 场景差异。
    /// 合并后必须满足两件事：三段都来自 override；`group_prompt()` 拼出来的人格**只出现
    /// 一次**——否则新代码会把 persona 接在旧全文前面，一轮里说两遍她是谁。
    #[test]
    fn persona_override_replaces_the_duplicated_legacy_prompts() {
        let dir = std::env::temp_dir().join(format!("kovi-persona-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("应建临时目录");
        let override_path = dir.join("bot.conf.override.toml");

        // 主配置：旧写法（人格抄进两份，且是线上那种开头）。
        let main = "[prompt]\n\
                    system_prompt = \"你叫芸汐，是一个温柔的女孩子。群聊规则若干。\"\n\
                    private_prompt = \"你叫芸汐，是一个温柔的女孩子。私聊规则若干。\"\n";

        std::fs::write(
            &override_path,
            "[prompt]\n\
             persona = \"你叫芸汐，是一个温柔、害羞、慢热而认真的女孩子。\"\n\
             system_prompt = \"群聊场景：在群里像朋友一样自然参与。\"\n\
             private_prompt = \"私聊场景：像熟悉已久的朋友一样亲近。\"\n",
        )
        .expect("应写临时覆盖");

        let merged = super::validate_main_candidate_with_override(main, &override_path)
            .expect("主配置 + 覆盖应通过校验");
        let prompt = merged.prompt();
        assert_eq!(
            prompt.persona(),
            "你叫芸汐，是一个温柔、害羞、慢热而认真的女孩子。"
        );
        assert_eq!(
            prompt.system_prompt(),
            "群聊场景：在群里像朋友一样自然参与。"
        );
        assert_eq!(
            prompt.private_prompt(),
            "私聊场景：像熟悉已久的朋友一样亲近。"
        );

        let group = prompt.group_prompt();
        let direct = prompt.direct_prompt();
        assert_eq!(
            group.matches("你叫芸汐").count(),
            1,
            "人格只能说一次：{group}"
        );
        assert_eq!(
            direct.matches("你叫芸汐").count(),
            1,
            "人格只能说一次：{direct}"
        );
        assert!(!group.contains("群聊规则若干"), "旧全文不该留下：{group}");
        assert!(group.contains("群聊场景："), "场景差异要跟上：{group}");

        // 对照：没有 override 时读到的还是旧写法——这正是发布后必须推配置的原因。
        let alone = super::validate_candidate(main).expect("候选主配置本身合法");
        assert_eq!(
            alone.prompt().persona(),
            Prompt::default().persona(),
            "旧主配置里没有 persona，读到的是代码默认值"
        );
        assert!(alone.prompt().system_prompt().contains("你叫芸汐"));

        std::fs::remove_file(&override_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn main_candidate_keeps_the_runtime_override() {
        let dir = std::env::temp_dir().join(format!("kovi-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("应建临时目录");
        let override_path = dir.join("bot.conf.override.toml");
        std::fs::write(&override_path, "[vision]\nprovider = \"intrinsic\"\n")
            .expect("应写临时覆盖");

        let main = "[model]\npush_probability_percent = 35\n";

        // 合并后的结果里，覆盖说了算。
        let merged = super::validate_main_candidate_with_override(main, &override_path)
            .expect("候选主配置 + 覆盖应通过校验");
        assert_eq!(
            merged.vision().provider(),
            "intrinsic",
            "主配置里的缺省不该盖掉覆盖"
        );

        // 对照：单独解析候选主配置（旧行为）拿不到覆盖的值。
        let alone = super::validate_candidate(main).expect("候选主配置本身合法");
        assert_ne!(
            alone.vision().provider(),
            "intrinsic",
            "旧行为（只解析主配置）会丢掉覆盖——这正是要被修掉的地方"
        );

        // 覆盖不存在时就是纯主配置，不该报错。
        let missing = super::validate_main_candidate_with_override(main, &dir.join("nope.toml"))
            .expect("没有覆盖文件时应按纯主配置处理");
        assert_eq!(missing.vision().provider(), alone.vision().provider());

        std::fs::remove_file(&override_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

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
        assert!(
            config.understanding().core_owned_enabled(),
            "仓库配置里 Core 回合的会话理解默认开启"
        );
        assert_eq!(config.topic().recent_topic_cooldown_secs(), 604_800);
        assert!(!config.prompt().system_prompt().contains("NEXT_MESSAGE"));
        assert!(!config.prompt().private_prompt().contains("NEXT_MESSAGE"));
        assert!(!config.prompt().system_prompt().contains("回复协议"));
        assert!(!config.prompt().private_prompt().contains("回复协议"));
        assert!(!config.prompt().system_prompt().contains("silent 决策"));
        assert!(!config.prompt().private_prompt().contains("silent 决策"));
        assert!(!config.prompt().system_prompt().contains("REPLY_ACTION"));
        assert!(!config.prompt().private_prompt().contains("REPLY_ACTION"));
        // 结构化动作的契约随 `reply_action` 工具下发（AGENTS.md 第 6 条），
        // 不许再回到常驻提示词里；字段名同样不该在提示词里出现。
        assert!(!config.prompt().system_prompt().contains("reply_action"));
        assert!(!config.prompt().private_prompt().contains("reply_action"));
        assert!(
            !config
                .prompt()
                .system_prompt()
                .contains("at_current_sender")
        );
        assert!(
            !config
                .prompt()
                .private_prompt()
                .contains("at_current_sender")
        );
    }

    #[test]
    fn shipped_example_configuration_deserializes() {
        // bot.conf.example.toml 是发布流程实际使用的生产模板（部署工作流会把它
        // 拷成 bot.conf.toml 再按 Secrets 打补丁）。它里面的地址是故意不可用的
        // 占位值，所以这里只校验语法与字段类型，不跑 validate()。
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bot.conf.example.toml");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("无法读取 {}: {error}", path.display()));
        let config = Config::builder()
            .add_source(config::File::from_str(&source, FileFormat::Toml))
            .build()
            .and_then(|config| config.try_deserialize::<ModelConfig>())
            .expect("仓库示例配置应可反序列化");
        // 通话段已随部署启用，这里确认它填的是真实可用的地址形态（回环 + 完整路径），
        // 避免把占位值带进生产。
        assert!(
            config.qq_call().validate().is_ok(),
            "示例配置里的通话配置必须通过校验"
        );
    }

    #[test]
    fn legacy_executive_sections_are_tolerated() {
        // Existing deployments still carry the now-removed Executive sections
        // (their consumers, the CandidateEvaluator, ReflectionController,
        // GoalArbitrator and SelfConsistencyMonitor, were removed). serde
        // without deny_unknown_fields must ignore those keys so a live
        // bot.conf.toml keeps loading.
        let source = r#"
            [executive]
            enabled = true
            [executive.priority]
            aging_enabled = true
            [executive.consistency]
            severe_threshold = 0.70
            blocking_threshold = 0.92
            [executive.candidate]
            max_candidates = 4
            [executive.reflection]
            deep_budget_per_day = 4
        "#;
        let config = Config::builder()
            .add_source(config::File::from_str(source, FileFormat::Toml))
            .build()
            .and_then(|config| config.try_deserialize::<ModelConfig>())
            .expect("legacy executive sections must be tolerated");
        assert!(config.executive().validate().is_ok());
    }
}
