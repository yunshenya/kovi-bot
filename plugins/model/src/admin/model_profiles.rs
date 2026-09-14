//! 模型档案：把"换一个模型"变成一次下拉 + 应用。
//!
//! 存的是"我配过的几套端点"（服务商、地址、模型名、协议、密钥……）。**当前生效的
//! 那套始终写在运行时覆盖配置的 `[server_config]` 里**——档案只是模板，运行时只认
//! 配置。这样配置页、概览页、`#系统信息` 与健康检查看到的一定是同一份真相，不会
//! 出现"档案里写着 A、实际跑着 B"这种最难查的状态。
//!
//! 文件是 `runtime/model_profiles.toml`，**0600**：里面存着各家的 API Key。它不出现在
//! 配置页的编辑器里（那是受管配置文件的地盘），只能通过模型页读写。

use super::ApiError;
use super::config_api::write_atomically_private;
use crate::config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 档案文件名（落在运行时目录里，与覆盖配置同级）。
pub(crate) const PROFILES_FILE: &str = "model_profiles.toml";

/// 一套模型端点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelProfile {
    /// 稳定标识：应用/删除都按它定位，改标签不会换 id。
    pub(crate) id: String,
    /// 给人看的名字（"DeepSeek 主力"）。
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) model_name: String,
    #[serde(default = "default_wire_api")]
    pub(crate) wire_api: String,
    #[serde(default = "default_thinking_mode")]
    pub(crate) thinking_mode: String,
    #[serde(default)]
    pub(crate) supports_vision: bool,
    #[serde(default = "default_true")]
    pub(crate) requires_auth: bool,
    /// 0 表示"沿用当前配置里的值"。
    #[serde(default)]
    pub(crate) max_output_tokens: u32,
    /// API Key。留空表示这套档案不带密钥（应用时不动当前配置里的那把）。
    #[serde(default)]
    pub(crate) api_key: String,
}

fn default_wire_api() -> String {
    "chat_completions".to_string()
}

fn default_thinking_mode() -> String {
    "disabled".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    profiles: Vec<ModelProfile>,
}

impl ModelProfile {
    /// 写进 `[server_config]` 的字段。空值表示"这一项不动"，避免把现有配置清空。
    pub(crate) fn changes(&self) -> BTreeMap<String, Value> {
        let mut changes = BTreeMap::new();
        if !self.url.trim().is_empty() {
            changes.insert("server_config.url".to_string(), json!(self.url.trim()));
        }
        if !self.model_name.trim().is_empty() {
            changes.insert(
                "server_config.model_name".to_string(),
                json!(self.model_name.trim()),
            );
        }
        changes.insert("server_config.wire_api".to_string(), json!(self.wire_api));
        changes.insert(
            "server_config.thinking_mode".to_string(),
            json!(self.thinking_mode),
        );
        changes.insert(
            "server_config.supports_vision".to_string(),
            json!(self.supports_vision),
        );
        changes.insert(
            "server_config.requires_auth".to_string(),
            json!(self.requires_auth),
        );
        if self.max_output_tokens > 0 {
            changes.insert(
                "server_config.max_output_tokens".to_string(),
                json!(self.max_output_tokens),
            );
        }
        if !self.api_key.trim().is_empty() {
            changes.insert(
                "server_config.api_key".to_string(),
                json!(self.api_key.trim()),
            );
        }
        // 应用一套端点就等于要用它：否则"切过去了却还在用本地模型"是纯粹的困惑。
        changes.insert("server_config.enabled".to_string(), json!(true));
        changes
    }

    /// 日志与回执里用的描述，绝不带密钥。
    pub(crate) fn describe(&self) -> String {
        format!("{}（{} / {}）", self.label, self.model_name, self.url)
    }
}

/// 档案文件路径（运行时目录下）。
pub(crate) fn profiles_path() -> PathBuf {
    config::runtime_dir().join(PROFILES_FILE)
}

/// 读全部档案。文件不存在算"还没有档案"；解析失败如实报错，不静默丢掉。
pub(crate) fn list() -> Result<Vec<ModelProfile>, ApiError> {
    list_at(&profiles_path())
}

/// [`list`] 的显式路径版本：测试直接驱动一个临时文件，不碰进程级运行时目录。
pub(crate) fn list_at(path: &std::path::Path) -> Result<Vec<ModelProfile>, ApiError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ApiError::internal(format!(
                "读取模型档案失败 ({}): {error}",
                path.display()
            )));
        }
    };
    let parsed: ProfilesFile = kovi::toml::from_str(&raw)
        .map_err(|error| ApiError::bad_request(format!("模型档案不是合法 TOML: {error}")))?;
    Ok(parsed.profiles)
}

fn save_at(path: &std::path::Path, profiles: &[ModelProfile]) -> Result<(), ApiError> {
    let file = ProfilesFile {
        profiles: profiles.to_vec(),
    };
    let text = kovi::toml::to_string_pretty(&file)
        .map_err(|error| ApiError::internal(format!("序列化模型档案失败: {error}")))?;
    // 0600：这里面有 API Key。
    write_atomically_private(path, &text).map_err(|error| {
        ApiError::internal(format!("写入模型档案失败 ({}): {error}", path.display()))
    })
}

/// 新增或按 id 覆盖一套档案，返回它的 id。
///
/// 没带 id 时按标签生成一个唯一的 id（重名自动加后缀），所以"存一套新的"永远
/// 不会顶掉同名的那套。
pub(crate) fn upsert(profile: ModelProfile) -> Result<String, ApiError> {
    upsert_at(&profiles_path(), profile)
}

/// [`upsert`] 的显式路径版本。
pub(crate) fn upsert_at(
    path: &std::path::Path,
    mut profile: ModelProfile,
) -> Result<String, ApiError> {
    if profile.label.trim().is_empty() {
        return Err(ApiError::bad_request("档案名不能为空"));
    }
    profile.label = profile.label.trim().to_string();
    let mut profiles = list_at(path)?;
    if profile.id.trim().is_empty() {
        profile.id = unique_id(&profiles, &profile.label);
        profiles.push(profile.clone());
    } else if let Some(slot) = profiles.iter_mut().find(|item| item.id == profile.id) {
        *slot = profile.clone();
    } else {
        profiles.push(profile.clone());
    }
    save_at(path, &profiles)?;
    Ok(profile.id)
}

/// 删除一套档案；返回是否真的删掉了。
pub(crate) fn remove(id: &str) -> Result<bool, ApiError> {
    remove_at(&profiles_path(), id)
}

/// [`remove`] 的显式路径版本。
pub(crate) fn remove_at(path: &std::path::Path, id: &str) -> Result<bool, ApiError> {
    let mut profiles = list_at(path)?;
    let before = profiles.len();
    profiles.retain(|profile| profile.id != id);
    if profiles.len() == before {
        return Ok(false);
    }
    save_at(path, &profiles)?;
    Ok(true)
}

/// 按 id 找一套档案。
pub(crate) fn find(id: &str) -> Result<Option<ModelProfile>, ApiError> {
    Ok(list()?.into_iter().find(|profile| profile.id == id))
}

/// 标签 → 唯一 id：ASCII 字母数字与短横线保留，其余（含中文）走标签哈希，
/// 所以"无语"和"开心"不会都塌成同一个 id。
fn unique_id(profiles: &[ModelProfile], label: &str) -> String {
    let base = slug(label);
    let mut candidate = base.clone();
    let mut suffix = 2;
    while profiles.iter().any(|profile| profile.id == candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

fn slug(label: &str) -> String {
    let mut slug: String = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .take(24)
        .collect();
    if slug.is_empty() {
        // 纯中文（或全是符号）：用标签的哈希做 id，稳定且不会撞。
        slug = format!("p{}", short_hash(label));
    }
    slug
}

fn short_hash(value: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:08x}", (hasher.finish() & 0xffff_ffff) as u32)
}

#[cfg(test)]
mod tests {
    use super::{ModelProfile, slug, unique_id};

    fn profile(id: &str, label: &str) -> ModelProfile {
        ModelProfile {
            id: id.to_string(),
            label: label.to_string(),
            url: "https://api.example.com/v1".to_string(),
            model_name: "example-model".to_string(),
            wire_api: "chat_completions".to_string(),
            thinking_mode: "disabled".to_string(),
            supports_vision: false,
            requires_auth: true,
            max_output_tokens: 0,
            api_key: "sk-test".to_string(),
        }
    }

    /// 应用一套档案要写全"换端点"需要的字段，并且顺手把外部模型打开。
    #[test]
    fn applying_a_profile_writes_the_endpoint_and_enables_it() {
        let changes = profile("deepseek", "DeepSeek").changes();
        assert_eq!(
            changes.get("server_config.url"),
            Some(&serde_json::json!("https://api.example.com/v1"))
        );
        assert_eq!(
            changes.get("server_config.model_name"),
            Some(&serde_json::json!("example-model"))
        );
        assert_eq!(
            changes.get("server_config.enabled"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            changes.get("server_config.api_key"),
            Some(&serde_json::json!("sk-test"))
        );
        // 0 表示沿用当前值，不能把配置里的上限清成 0。
        assert!(!changes.contains_key("server_config.max_output_tokens"));
    }

    /// 空密钥/空地址表示"这一项不动"：切档案不该把现有密钥抹掉。
    #[test]
    fn blank_fields_are_left_untouched() {
        let mut bare = profile("bare", "本地模型");
        bare.api_key = "   ".to_string();
        bare.url = String::new();
        let changes = bare.changes();
        assert!(!changes.contains_key("server_config.api_key"));
        assert!(!changes.contains_key("server_config.url"));
        assert!(changes.contains_key("server_config.model_name"));
    }

    /// 纯中文标签也要拿到不同的 id；重名自动加后缀。
    #[test]
    fn ids_are_stable_and_unique() {
        assert_ne!(slug("无语"), slug("开心"));
        assert_eq!(slug("DeepSeek"), "deepseek");
        let existing = vec![profile(&slug("无语"), "无语")];
        assert_eq!(unique_id(&existing, "无语"), format!("{}-2", slug("无语")));
        assert_eq!(unique_id(&existing, "新的一套"), slug("新的一套"));
    }

    /// 回执与日志里的描述绝不能带密钥。
    #[test]
    fn descriptions_never_leak_the_key() {
        let described = profile("deepseek", "DeepSeek").describe();
        assert!(!described.contains("sk-test"));
        assert!(described.contains("example-model"));
    }
}
