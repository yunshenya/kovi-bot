//! 模型档案：把"换一个模型"变成一次下拉 + 应用。
//!
//! 存的是"我配过的几套端点"（服务商、地址、模型名、协议、密钥……）。**当前生效的
//! 那套始终写在运行时覆盖配置的 `[server_config]` 里**——档案只是模板，运行时只认
//! 配置。这样配置页、概览页、`#系统信息` 与健康检查看到的一定是同一份真相，不会
//! 出现"档案里写着 A、实际跑着 B"这种最难查的状态。
//!
//! 文件是 `runtime/model_profiles.toml`，**0600**：里面存着各家的 API Key。它不出现在
//! 配置页的编辑器里（那是受管配置文件的地盘），只能通过模型页读写。
//!
//! 「正在用」的判定只写在这里一处（[`ModelProfile::is_live`]）：页面上的标记、卡片
//! 的删除按钮与删除接口的拦截共用同一份口径，免得三处各判一次、说法还不一样。

use super::ApiError;
use super::config_api::write_atomically_private;
use crate::config;
use crate::config::ServerConfig;
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

/// 当前生效端点的身份：`(地址, 模型名)`，地址已归一化。
///
/// 外部模型没启用、或配置里的模型名是空的，就没有任何端点在跑，返回 `None`——
/// 那时"她只用本地能力"（Core / Intrinsic），列表里谁也不该被标成「正在用」。
fn live_identity(server: &ServerConfig) -> Option<(&str, &str)> {
    if !server.enabled() {
        return None;
    }
    let model_name = server.model_name().trim();
    if model_name.is_empty() {
        return None;
    }
    Some((normalize_url(server.url()), model_name))
}

/// 地址的归一化形式，只用于比较：去掉首尾空白与尾部斜杠。
fn normalize_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

impl ModelProfile {
    /// 这套档案是不是"正在生效的那一套"。
    ///
    /// 口径是**外部模型已启用 + 地址与模型名跟当前 `[server_config]` 一致**，与页面
    /// 上那枚「正在用」标记、删除接口的拦截完全同一份。只看这两个字段是有意的：它们是
    /// "这套端点是谁"的身份，而密钥、输出上限、协议都是能就地改的可变量——把它们一起
    /// 比，改过一把密钥之后当前端点就会被判成"另一套"，于是正在用的那套反而变得可删。
    ///
    /// 地址比较前去掉首尾空白与尾部斜杠：`https://x/v1` 与 `https://x/v1/` 拼出来的
    /// 是同一个请求地址，不该因为一个斜杠就当成两套端点。
    pub(crate) fn is_live(&self, server: &ServerConfig) -> bool {
        let Some((url, model_name)) = live_identity(server) else {
            return false;
        };
        self.model_name.trim() == model_name && normalize_url(&self.url) == url
    }

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

/// 删除一套档案。**正在用的那套删不掉**：删掉它就等于把"她现在跑的是什么"从列表里
/// 抹掉，之后再想切回去只能重新填一遍地址与密钥。
pub(crate) fn remove(id: &str, server: &ServerConfig) -> Result<(), ApiError> {
    remove_at(&profiles_path(), id, server)
}

/// [`remove`] 的显式路径版本。
///
/// 判定与删除在同一次读盘里完成（而不是先查一次再删一次）：中间没有窗口能让
/// "刚被切过去的那套"漏过去。
pub(crate) fn remove_at(
    path: &std::path::Path,
    id: &str,
    server: &ServerConfig,
) -> Result<(), ApiError> {
    let mut profiles = list_at(path)?;
    let Some(profile) = profiles.iter().find(|profile| profile.id == id) else {
        return Err(ApiError::not_found(format!("找不到模型档案: {id}")));
    };
    if profile.is_live(server) {
        return Err(ApiError::conflict(format!(
            "「{}」正在用（{} / {}），不能删。先切到另一套档案，或在配置页关掉外部模型，再回来删。",
            profile.label, profile.model_name, profile.url
        )));
    }
    profiles.retain(|profile| profile.id != id);
    save_at(path, &profiles)
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

/// 测试用：造一份"当前生效的 `[server_config]`"。
///
/// `ServerConfig` 的字段是私有的（只有 `config::server` 能直接构造），所以测试从这里
/// 反序列化一份，而不是为了测试去放宽字段可见性。
#[cfg(test)]
pub(crate) fn test_server(enabled: bool, url: &str, model_name: &str) -> ServerConfig {
    serde_json::from_value(serde_json::json!({
        "enabled": enabled,
        "url": url,
        "model_name": model_name,
    }))
    .expect("测试用的 server_config 必须能构造出来")
}

#[cfg(test)]
mod tests {
    use super::{ModelProfile, PROFILES_FILE, slug, test_server, unique_id};
    use axum::http::StatusCode;
    use std::path::PathBuf;

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

    /// 一次性临时目录：名字里带用例名，几个用例并行跑也不会互相踩。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kovi-profiles-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("应能建临时目录");
        dir
    }

    /// 自己的一套档案：地址与模型名都对齐 `test_server(...)` 的调用点。
    fn profile_at(id: &str, label: &str, url: &str, model_name: &str) -> ModelProfile {
        ModelProfile {
            url: url.to_string(),
            model_name: model_name.to_string(),
            ..profile(id, label)
        }
    }

    /// 判定「正在用」：外部模型已启用 + 地址与模型名一致。
    ///
    /// 这份规则同时管着页面上的标记与删除闸门，所以把边界一次写清楚：尾部斜杠、
    /// 首尾空白算同一套；换地址、换模型名、空模型名、外部模型关掉，都不算。
    #[test]
    fn only_the_enabled_endpoint_with_the_same_url_and_model_is_live() {
        let server = test_server(true, "https://api.deepseek.com/v1", "deepseek-v4-flash");
        let mut candidate = profile_at(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com/v1",
            "deepseek-v4-flash",
        );
        assert!(candidate.is_live(&server));

        // 一个斜杠、一点空白，拼出来的是同一个请求地址：仍算"正在用"。
        candidate.url = "https://api.deepseek.com/v1/ ".to_string();
        assert!(
            candidate.is_live(&server),
            "尾部斜杠与空白不该把它判成另一套"
        );
        candidate.url = "https://api.deepseek.com/v1".to_string();
        candidate.model_name = " deepseek-v4-flash ".to_string();
        assert!(candidate.is_live(&server));
        candidate.model_name = "deepseek-v4-flash".to_string();

        // 密钥、输出上限、协议都是能就地改的可变量：改了它还是同一个端点。
        candidate.api_key = "sk-another".to_string();
        candidate.max_output_tokens = 800;
        candidate.wire_api = "responses".to_string();
        assert!(
            candidate.is_live(&server),
            "可变量不该让正在用的那套变成另一套（否则它反而变得可删）"
        );

        // 换地址、换模型名、空模型名：都不是当前那套。
        let mut other = profile_at(
            "other",
            "别的",
            "https://api.deepseek.com/v2",
            "deepseek-v4-flash",
        );
        assert!(!other.is_live(&server));
        other.url = "https://api.deepseek.com/v1".to_string();
        other.model_name = "deepseek-v3".to_string();
        assert!(!other.is_live(&server));
        other.model_name = String::new();
        assert!(!other.is_live(&server), "没有模型名的档案不该冒充当前那套");

        // 外部模型关掉（或配置里模型名是空的）时她只用本地能力，谁也不在跑。
        assert!(!candidate.is_live(&test_server(
            false,
            "https://api.deepseek.com/v1",
            "deepseek-v4-flash"
        )));
        assert!(!candidate.is_live(&test_server(true, "https://api.deepseek.com/v1", "")));
    }

    /// 正在用的那套删不掉：接口回 409，磁盘上那套必须原样还在。
    #[test]
    fn removing_the_live_profile_is_refused_and_leaves_the_file_alone() {
        let path = temp_dir("remove-live").join(PROFILES_FILE);
        let server = test_server(true, "https://api.deepseek.com/v1", "deepseek-v4-flash");
        let live_id = super::upsert_at(
            &path,
            profile_at(
                "",
                "DeepSeek 主力",
                "https://api.deepseek.com/v1",
                "deepseek-v4-flash",
            ),
        )
        .expect("应能存下正在用的那套");
        let backup_id = super::upsert_at(
            &path,
            profile_at("", "备用", "https://api.example.com/v1", "example-model"),
        )
        .expect("应能存下备用那套");
        let before = std::fs::read_to_string(&path).expect("应能读回档案文件");

        let error = super::remove_at(&path, &live_id, &server).expect_err("正在用的那套必须删不掉");
        assert_eq!(error.status, StatusCode::CONFLICT, "{error:?}");
        assert!(
            error.message.contains("正在用") && error.message.contains("先切到另一套档案"),
            "得说清为什么不能删、怎么才能删: {}",
            error.message
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("应能读回档案文件"),
            before,
            "拦下来之后文件不该有任何改动"
        );
        assert_eq!(super::list_at(&path).expect("应能读取").len(), 2);

        // 外部模型关掉之后同一套就可以删了：她确实没在用它。
        super::remove_at(&path, &live_id, &test_server(false, "", "")).expect("禁用后应能删");
        assert_eq!(super::list_at(&path).expect("应能读取").len(), 1);
        // 不在用的那套一直可删；找不到的 id 如实回 404。
        super::remove_at(&path, &backup_id, &server).expect("备用那套应能删");
        assert!(super::list_at(&path).expect("应能读取").is_empty());
        let missing = super::remove_at(&path, &backup_id, &server).expect_err("再删应报找不到");
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{missing:?}");
        let _ = std::fs::remove_dir_all(path.parent().expect("临时目录"));
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
