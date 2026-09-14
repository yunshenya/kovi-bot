//! 配置读写：全量参数视图、按注释无损写回、校验后再落盘、热替换内存配置。
//!
//! 设计要点（都是为了"改配置不能把机器人改坏"）：
//!
//! 1. **先校验后落盘**：候选文本必须先反序列化成 `ModelConfig` 并通过
//!    `validate()`，失败时磁盘和内存都保持原样。
//! 2. **注释无损**：用 `toml_edit` 只改被点名的键，运维写在配置里的注释和空行
//!    不会因为一次网页保存被抹掉。
//! 3. **每次保存先备份**：改动前把原文件复制成 `*.bak.<时间戳>`，保留最近若干份，
//!    并提供一键回滚。
//! 4. **密钥不回显**：配置里的 Token 类字段读出时打码，写回时打码值表示"不修改"。

use super::{AdminState, ApiError};
use crate::config;
use axum::Json;
use axum::extract::{Path as UrlPath, State};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value as TomlValue};

/// 主配置文件名（唯一做类型化校验与热重载的文件）。
const MAIN_CONFIG: &str = "bot.conf.toml";
/// 运行时覆盖配置：叠在主配置之上，落在可写的运行时目录里。
const OVERRIDE_CONFIG: &str = config::OVERRIDE_FILE;
/// 允许管理后台读写、且只允许这几个名字（防目录穿越）。
const MANAGED_FILES: &[&str] = &[
    "bot.conf.toml",
    "bot.conf.override.toml",
    "kovi.conf.toml",
    "kovi.plugin.toml",
];
/// 打码后回显的占位值。写回时等于这个值表示"不修改"。
const MASK: &str = "********";
/// 通用 TOML 文件里按键名判断是否打码。
const SECRET_KEY_HINTS: &[&str] = &[
    "token",
    "access_token",
    "password",
    "secret",
    "api_key",
    "apikey",
    "cookie",
    "authorization",
    "credential",
];
/// 这些分区在启动时被一次性读进常驻结构，改完必须重启才生效。
///
/// 证据：`tools` 见 `model/tool_access.rs::initialize`，`model.intrinsic` /
/// `model.fallback` 见 `yunxi/intrinsic_runtime.rs::load`（`OnceLock`），
/// `model.turn_gate` 见 `yunxi/turn_gate_runtime.rs::install`（`OnceLock`），
/// `admin` 是本模块自己的监听地址。其余分区都在用到的当刻 `config::get()`。
const RESTART_SECTIONS: &[&str] = &[
    "admin",
    "tools",
    "model.intrinsic",
    "model.fallback",
    "model.turn_gate",
];
/// 备份保留份数。
const MAX_BACKUPS: usize = 20;
/// 单次写入的文本上限，避免把配置文件换成别的东西。
const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;

/// 编译期生成、随进程内嵌的字段说明（见 tools/admin-docs/extract_config_docs.py）。
const CONFIG_DOCS: &str = include_str!("config_docs.json");

struct ManagedFile {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// 是否是主配置（类型化校验 + 热重载）。
    typed: bool,
    /// 值视图：`effective` 显示合并后的生效值，`sparse` 只显示文件里写了的字段。
    view: &'static str,
    /// 是否属于"改完要重启"的文件。
    restart_required: bool,
}

const FILES: &[ManagedFile] = &[
    ManagedFile {
        name: "bot.conf.toml",
        title: "机器人配置",
        description: "芸汐自己的全部参数：人设、主动消息、记忆、Mind、模型、工具、通话等。保存后立即热加载。",
        typed: true,
        restart_required: false,
        view: "effective",
    },
    ManagedFile {
        name: "bot.conf.override.toml",
        title: "运行时覆盖配置",
        description: "只写需要覆盖主配置的字段。它落在可写的运行时目录里，发布新版本不会把它冲掉；生产环境的 current/ 是只读发布目录，长期生效的改动应写在这里。",
        typed: true,
        restart_required: false,
        view: "sparse",
    },
    ManagedFile {
        name: "kovi.conf.toml",
        title: "Kovi 框架配置",
        description: "框架侧参数：主管理员 QQ、副管理员、调试开关，以及连接 NapCat 的地址与 OneBot Token。",
        typed: false,
        restart_required: true,
        view: "sparse",
    },
    ManagedFile {
        name: "kovi.plugin.toml",
        title: "插件访问控制",
        description: "好友与群白名单、是否启用访问控制。仅在插件启动时读取。",
        typed: false,
        restart_required: true,
        view: "sparse",
    },
];

fn managed_file(name: &str) -> Result<&'static ManagedFile, ApiError> {
    FILES
        .iter()
        .find(|file| file.name == name)
        .ok_or_else(|| ApiError::not_found(format!("不支持的文件: {name}")))
}

/// 把用户传来的文件名解析成实际路径。
///
/// 只允许白名单里的裸文件名：不接受任何分隔符、`..` 或绝对路径，
/// 这样即便将来白名单被放宽也不会变成任意文件写入。
fn config_path(name: &str) -> Result<PathBuf, ApiError> {
    managed_file(name)?;
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ApiError::bad_request("文件名不合法"));
    }
    if name == OVERRIDE_CONFIG {
        return Ok(config::override_file_path());
    }
    Ok(PathBuf::from(name))
}

/// 这些改动路径落在哪些"需要重启才生效"的分区里。
fn restart_sections_hit(keys: &[String]) -> Vec<String> {
    let mut hit: Vec<String> = RESTART_SECTIONS
        .iter()
        .filter(|section| {
            keys.iter().any(|key| {
                let doc_path = key.replace("[]", "");
                doc_path == **section || doc_path.starts_with(&format!("{section}."))
            })
        })
        .map(|section| (*section).to_string())
        .collect();
    hit.sort();
    hit.dedup();
    hit
}

/// 目录是否可写（真的建一个探针文件再删掉，而不是猜权限位）。
pub(crate) fn directory_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".yunxi-write-probe-{}", std::process::id()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 文件所在目录是否可写。
fn parent_writable(path: &Path) -> bool {
    let dir = match path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => Path::new("."),
    };
    directory_writable(dir)
}

/// `GET /api/config/files`
pub(crate) async fn list_files() -> Result<Json<Value>, ApiError> {
    let files: Vec<Value> = FILES
        .iter()
        .map(|file| {
            let path = config_path(file.name).unwrap_or_else(|_| PathBuf::from(file.name));
            let metadata = fs::metadata(&path).ok();
            json!({
                "name": file.name,
                "title_full": file.title,
                "view": file.view,
                "writable": parent_writable(&path),
                "path": path.display().to_string(),
                "title": file.title,
                "description": file.description,
                "typed": file.typed,
                "restart_required": file.restart_required,
                "exists": metadata.is_some(),
                "bytes": metadata.as_ref().map(fs::Metadata::len),
                "modified": metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok())
                    .map(format_time),
                "backups": backups_of(&path).len(),
            })
        })
        .collect();
    Ok(Json(json!({
        "files": files,
        "main": MAIN_CONFIG,
        "restart_sections": RESTART_SECTIONS,
    })))
}

/// `GET /api/config/file/{name}`
pub(crate) async fn read_file(UrlPath(name): UrlPath<String>) -> Result<Json<Value>, ApiError> {
    let file = managed_file(&name)?;
    let path = config_path(&name)?;
    let raw = read_config_text(&path, file)?;

    let (values, masked) = if file.typed && file.view == "effective" {
        // 主配置页展示"合并覆盖后真正生效的值"，这样界面上看到的就是进程在用的。
        mask_typed(effective_values())
    } else {
        // 覆盖配置与框架配置展示"文件里真正写了什么"，否则看不出哪些字段被覆盖过。
        let parsed = if raw.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            parse_toml_value(&raw).map_err(ApiError::bad_request)?
        };
        if file.typed {
            mask_typed(parsed)
        } else {
            let mut parsed = parsed;
            let mut masked = Vec::new();
            mask_generic(&mut parsed, &mut Vec::new(), &mut masked);
            (parsed, masked)
        }
    };

    Ok(Json(json!({
        "name": file.name,
        "title": file.title,
        "description": file.description,
        "typed": file.typed,
        "view": file.view,
        "restart_required": file.restart_required,
        "writable": directory_writable(&path),
        "path": path.display().to_string(),
        // `raw` 也要打码，且必须与 `masked` 列表一致：前端把这个字段直接塞进编辑器，
        // 一边说"这些字段打了码"、一边原样回显密钥（NapCat 的 access_token、admin.token、
        // 各种 *_api_key）等于把密钥放到屏幕、截图和浏览器缓存里。
        "raw": mask_raw_secrets(&raw, &masked),
        "values": values,
        "masked": masked,
        "file_paths": present_paths(&raw),
        "docs": if file.typed { docs() } else { Value::Null },
        "restart_sections": if file.typed { json!(RESTART_SECTIONS) } else { Value::Null },
        "modified": modified_at(&path),
    })))
}

#[derive(Deserialize)]
pub(crate) struct RawWrite {
    raw: String,
}

/// `PUT /api/config/file/{name}`：整文件覆盖（表单存不了的东西走这里）。
pub(crate) async fn write_raw(
    State(state): State<Arc<AdminState>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<RawWrite>,
) -> Result<Json<Value>, ApiError> {
    let file = managed_file(&name)?;
    let path = config_path(&name)?;
    if body.raw.len() > MAX_CONFIG_BYTES {
        return Err(ApiError::bad_request("配置文本过大"));
    }

    // 显示侧把密钥打了码，所以整文件写回时必须把 `MASK` 还原成磁盘上的真值——否则
    // 管理员在原始编辑器里点一次保存，所有密钥就变成 `********` 落盘。这与 patch 那条
    // 路的约定一致（那里遇到 MASK 直接跳过该字段）。
    let candidate_text = if file.typed {
        let current = read_config_text(&path, file)?;
        let (_, masked) = mask_typed(effective_values());
        restore_masked_secrets(&body.raw, &current, &masked)
    } else {
        body.raw.clone()
    };

    let validated = validate_candidate(file, &candidate_text)?;

    let backup = backup(&path)?;
    write_atomically(&path, &candidate_text).map_err(|error| write_hint(&path, &error))?;

    let mut reloaded = false;
    if let Some(config) = validated {
        config::install(config).map_err(ApiError::internal)?;
        reloaded = true;
    }
    // 整文件覆盖无法逐字段判断，按"文件级"记录：类型化文件只要有需重启的分区
    // 就整体记一次，宁可多提示一次也不要让改动静静地不生效。
    if file.typed {
        let sections: Vec<String> = RESTART_SECTIONS.iter().map(|s| (*s).to_string()).collect();
        state.note_pending_restart(&sections);
    }

    Ok(Json(json!({
        "ok": true,
        "reloaded": reloaded,
        "backup": backup.map(|path| file_name(&path)),
        "restart_required": file.restart_required,
        "pending_restart": state.pending_restart(),
    })))
}

#[derive(Deserialize)]
pub(crate) struct PatchRequest {
    /// 目标文件，缺省为主配置。
    #[serde(default)]
    file: Option<String>,
    /// `{"分组.字段": 新值}`。值会被写进 TOML 对应位置。
    changes: BTreeMap<String, Value>,
}

/// `POST /api/config/patch`：按字段改主配置。
pub(crate) async fn patch(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<PatchRequest>,
) -> Result<Json<Value>, ApiError> {
    let name = body.file.as_deref().unwrap_or(MAIN_CONFIG);
    let file = managed_file(name)?;
    let path = config_path(name)?;
    if body.changes.is_empty() {
        return Err(ApiError::bad_request("没有要修改的字段"));
    }

    let raw = read_config_text(&path, file)?;
    let mut document = raw
        .parse::<DocumentMut>()
        .map_err(|error| ApiError::bad_request(format!("现有配置无法解析为 TOML: {error}")))?;

    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    for (key, value) in &body.changes {
        // 打码值等于"这个密钥我不改"，不能把 ******** 真的写进配置。
        if value.as_str() == Some(MASK) {
            skipped.push(key.clone());
            continue;
        }
        apply_change(&mut document, key, value)
            .map_err(|error| ApiError::bad_request(format!("{key}: {error}")))?;
        applied.push(key.clone());
    }
    applied.sort();
    skipped.sort();

    let candidate = document.to_string();
    if candidate.len() > MAX_CONFIG_BYTES {
        return Err(ApiError::bad_request("配置文本过大"));
    }

    let validated = validate_candidate(file, &candidate)?;

    if applied.is_empty() {
        return Ok(Json(json!({
            "ok": true,
            "changed": [],
            "skipped": skipped,
            "reloaded": false,
            "message": "没有实际改动",
        })));
    }

    let backup = backup(&path)?;
    write_atomically(&path, &candidate).map_err(|error| write_hint(&path, &error))?;

    let mut reloaded = false;
    if let Some(config) = validated {
        config::install(config).map_err(ApiError::internal)?;
        reloaded = true;
    }
    if file.typed {
        state.note_pending_restart(&restart_sections_hit(&applied));
    }

    // 覆盖配置返回稀疏视图（文件里写了什么），主配置返回生效值。
    let (values, masked) = if file.typed && file.view == "effective" {
        let (values, masked) = mask_typed(effective_values());
        (values, masked)
    } else {
        let mut values = parse_toml_value(&candidate).unwrap_or(Value::Null);
        let mut masked = Vec::new();
        if file.typed {
            let (masked_values, paths) = mask_typed(values);
            values = masked_values;
            masked = paths;
        } else {
            mask_generic(&mut values, &mut Vec::new(), &mut masked);
        }
        (values, masked)
    };

    Ok(Json(json!({
        "ok": true,
        "changed": applied,
        "skipped": skipped,
        "reloaded": reloaded,
        "restart_required": file.restart_required,
        "pending_restart": state.pending_restart(),
        "backup": backup.map(|path| file_name(&path)),
        "raw": candidate,
        "values": values,
        "masked": masked,
    })))
}

/// `POST /api/config/reload`：按磁盘内容重新加载内存配置。
pub(crate) async fn reload() -> Result<Json<Value>, ApiError> {
    let config =
        config::reload_from_disk().map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    let (values, masked) = mask_typed(serde_json::to_value(&config).unwrap_or(Value::Null));
    Ok(Json(json!({
        "ok": true,
        "values": values,
        "masked": masked,
    })))
}

/// `GET /api/config/backups`
pub(crate) async fn list_backups() -> Result<Json<Value>, ApiError> {
    let mut all = Vec::new();
    for file in FILES {
        for backup in backups_of(&config_path(file.name)?) {
            all.push(json!({
                "file": file.name,
                "name": file_name(&backup),
                "bytes": fs::metadata(&backup).ok().map(|meta| meta.len()),
                "modified": modified_at(&backup),
            }));
        }
    }
    Ok(Json(json!({ "backups": all, "keep": MAX_BACKUPS })))
}

#[derive(Deserialize)]
pub(crate) struct RestoreRequest {
    name: String,
}

/// `POST /api/config/restore`：回滚到某个备份。
pub(crate) async fn restore_backup(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<RestoreRequest>,
) -> Result<Json<Value>, ApiError> {
    let backup_path = resolve_backup(&body.name)?;
    let target_name = backup_target(&body.name)
        .ok_or_else(|| ApiError::bad_request("无法从备份名推断目标文件"))?;
    let file = managed_file(&target_name)?;
    let target = config_path(&target_name)?;

    let raw = fs::read_to_string(&backup_path)
        .map_err(|error| ApiError::internal(format!("无法读取备份: {error}")))?;
    let validated = validate_candidate(file, &raw)?;

    let backup = backup(&target)?;
    write_atomically(&target, &raw).map_err(|error| write_hint(&target, &error))?;
    let mut reloaded = false;
    if let Some(config) = validated {
        config::install(config).map_err(ApiError::internal)?;
        reloaded = true;
    }

    if file.typed {
        let sections: Vec<String> = RESTART_SECTIONS.iter().map(|s| (*s).to_string()).collect();
        state.note_pending_restart(&sections);
    }

    Ok(Json(json!({
        "ok": true,
        "restored": body.name,
        "pending_restart": state.pending_restart(),
        "file": target_name,
        "reloaded": reloaded,
        "rollback_backup": backup.map(|path| file_name(&path)),
        "restart_required": file.restart_required,
    })))
}

// ---------------------------------------------------------------- 内部实现

fn format_time(time: std::time::SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn modified_at(path: &Path) -> Option<String> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(format_time)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn read_config_text(path: &Path, _file: &ManagedFile) -> Result<String, ApiError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        // 还没创建的文件按空内容展示：后台允许"从此处创建它"，
        // 而不是把一个打不开的页签留给运维。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(ApiError::bad_request(format!(
            "无法读取 {}: {error}",
            path.display()
        ))),
    }
}

/// 校验候选文本；类型化文件返回解析结果，其余只查语法。失败时无副作用。
fn validate_candidate(
    file: &ManagedFile,
    source: &str,
) -> Result<Option<config::ModelConfig>, ApiError> {
    validate_candidate_with_override(file, source, &config::override_file_path())
}

/// [`validate_candidate`] 的实现，覆盖文件路径由调用方给出（便于测试）。
fn validate_candidate_with_override(
    file: &ManagedFile,
    source: &str,
    override_path: &Path,
) -> Result<Option<config::ModelConfig>, ApiError> {
    if !file.typed {
        parse_toml_value(source).map_err(ApiError::bad_request)?;
        return Ok(None);
    }
    let validated = if file.name == OVERRIDE_CONFIG {
        // 覆盖配置是稀疏的，必须与主配置合并后再校验。
        config::validate_override_candidate(source)
    } else {
        // 主配置必须与磁盘上现存的运行时覆盖合并后再校验：那才是这次写入之后重新
        // 加载会得到的配置。单独校验不仅会误判跨段规则，还会让 `install` 把覆盖
        // 从内存里挤掉（磁盘上还在），要等下次重启才回来。
        config::validate_main_candidate_with_override(source, override_path)
    }
    .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    Ok(Some(validated))
}

/// 写盘失败时补一句"为什么"，避免只留一个 EROFS。
fn write_hint(path: &Path, error: &std::io::Error) -> ApiError {
    let message = format!("无法写入 {}: {error}", path.display());
    match error.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
            ApiError::bad_request(format!(
                "{message}；这个目录在当前部署里是只读的（发布目录不可变），\
                 请改用「运行时覆盖配置」，或从部署流程里更新主配置"
            ))
        }
        _ => ApiError::internal(message),
    }
}

/// 内存里正在生效的配置（包含没写进文件的默认值）。
fn effective_values() -> Value {
    serde_json::to_value(config::get()).unwrap_or(Value::Null)
}

fn docs() -> Value {
    serde_json::from_str(CONFIG_DOCS).unwrap_or(Value::Null)
}

fn parse_toml_value(source: &str) -> Result<Value, String> {
    let parsed: kovi::toml::Value =
        kovi::toml::from_str(source).map_err(|error| format!("不是合法的 TOML: {error}"))?;
    serde_json::to_value(parsed).map_err(|error| format!("无法转换为 JSON: {error}"))
}

/// 把生效配置里的密钥字段打码，并返回被打码的字段路径。
fn mask_typed(mut values: Value) -> (Value, Vec<String>) {
    let mut masked = Vec::new();
    let secret_paths: Vec<String> = docs()
        .get("fields")
        .and_then(Value::as_object)
        .map(|fields| {
            fields
                .iter()
                .filter(|(_, entry)| entry.get("secret").and_then(Value::as_bool) == Some(true))
                .map(|(path, _)| path.clone())
                .collect()
        })
        .unwrap_or_default();

    // 配置结构里另有名字带 token 的字段（例如 bridge_token_env），
    // 它们指向环境变量而不是密钥本身，所以只按显式标注打码。
    let mut paths: Vec<Vec<String>> = secret_paths
        .iter()
        .map(|path| path.split('.').map(str::to_string).collect())
        .collect();
    paths.extend(discover_secret_paths(&values));
    paths.sort();
    paths.dedup();

    for path in paths {
        if let Some(slot) = value_at_mut(&mut values, &path)
            && slot.as_str().is_some_and(|text| !text.is_empty())
        {
            *slot = Value::String(MASK.to_string());
            masked.push(path.join("."));
        }
    }
    (values, masked)
}

/// 按点分路径定位 TOML 里的一个条目（不存在就返回 None）。
fn item_at_toml_path<'a>(document: &'a mut DocumentMut, path: &[&str]) -> Option<&'a mut Item> {
    let mut current: &mut Item = document.as_item_mut();
    for segment in path {
        current = current.as_table_like_mut()?.get_mut(segment)?;
    }
    Some(current)
}

/// 按点分路径读出一份 TOML 里的值（不存在就返回 None）。
fn value_at_toml_path(document: &DocumentMut, path: &[&str]) -> Option<TomlValue> {
    let mut current: &Item = document.as_item();
    for segment in path {
        current = current.as_table_like()?.get(segment)?;
    }
    current.as_value().cloned()
}

/// 把 `raw` 文本里被点名的密钥值换成 `MASK`（显示用）。
///
/// 用 `toml_edit` 而不是字符串替换：只动点名的键，注释与空行原样保留；路径在本文件里
/// 不存在（例如密钥只写在覆盖配置里）就跳过。
fn mask_raw_secrets(raw: &str, masked: &[String]) -> String {
    if masked.is_empty() || raw.trim().is_empty() {
        return raw.to_string();
    }
    let Ok(mut document) = raw.parse::<DocumentMut>() else {
        return raw.to_string();
    };
    let mut changed = false;
    for path in masked {
        let segments: Vec<&str> = path.split('.').collect();
        if let Some(item) = item_at_toml_path(&mut document, &segments)
            && let Some(value) = item.as_value_mut()
        {
            *value = TomlValue::from(MASK);
            changed = true;
        }
    }
    if changed {
        document.to_string()
    } else {
        raw.to_string()
    }
}

/// 写回时把 `MASK` 还原成磁盘上现存的真实值（`MASK` = "这个密钥我不改"）。
fn restore_masked_secrets(candidate: &str, current: &str, masked: &[String]) -> String {
    if masked.is_empty() {
        return candidate.to_string();
    }
    let (Ok(mut target), Ok(source)) = (
        candidate.parse::<DocumentMut>(),
        current.parse::<DocumentMut>(),
    ) else {
        return candidate.to_string();
    };
    let mut restored = false;
    for path in masked {
        let segments: Vec<&str> = path.split('.').collect();
        // 磁盘上没有这个键（或它本来就没值）就没什么可还原的。
        let Some(existing_value) = value_at_toml_path(&source, &segments) else {
            continue;
        };
        let Some(item) = item_at_toml_path(&mut target, &segments) else {
            continue;
        };
        if item.as_value().and_then(TomlValue::as_str) == Some(MASK) {
            *item = Item::Value(existing_value);
            restored = true;
        }
    }
    if restored {
        target.to_string()
    } else {
        candidate.to_string()
    }
}

/// 递归找出名字像密钥的字段（用于通用 TOML 文件）。
fn mask_generic(value: &mut Value, path: &mut Vec<String>, masked: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                path.push(key.clone());
                if is_secret_key(key) && child.as_str().is_some_and(|text| !text.is_empty()) {
                    *child = Value::String(MASK.to_string());
                    masked.push(path.join("."));
                } else {
                    mask_generic(child, path, masked);
                }
                path.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                mask_generic(child, path, masked);
                path.pop();
            }
        }
        _ => {}
    }
}

fn discover_secret_paths(value: &Value) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    collect_secret_paths(value, &mut Vec::new(), &mut found, false);
    found
}

fn collect_secret_paths(
    value: &Value,
    path: &mut Vec<String>,
    found: &mut Vec<Vec<String>>,
    secret_context: bool,
) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, child) in map {
        path.push(key.clone());
        let secret = secret_context || is_secret_key(key);
        if secret && child.is_string() {
            found.push(path.clone());
        } else {
            collect_secret_paths(child, path, found, secret);
        }
        path.pop();
    }
}

/// 键名看起来是不是"密钥本身"。
///
/// `*_env` / `*_file` 指向的是"密钥放在哪儿"（环境变量名、文件路径），
/// 把它们一起打码会让界面连"当前用的是哪个环境变量"都看不到，而且保存时
/// 会被当成"不修改"而跳过。
fn is_secret_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    if lowered.ends_with("_env") || lowered.ends_with("_file") || lowered.ends_with("_path") {
        return false;
    }
    SECRET_KEY_HINTS.iter().any(|hint| lowered.contains(hint))
}

fn value_at_mut<'a>(value: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut current = value;
    for segment in path {
        current = match current {
            Value::Object(map) => map.get_mut(segment)?,
            Value::Array(items) => items.get_mut(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// 文件里真正写出来的字段路径（用于区分"默认值"与"显式配置"）。
fn present_paths(source: &str) -> Vec<String> {
    let document = match source.parse::<DocumentMut>() {
        Ok(document) => document,
        Err(_) => return Vec::new(),
    };
    let mut paths = Vec::new();
    collect_paths(document.as_table(), &mut Vec::new(), &mut paths);
    paths.sort();
    paths
}

fn collect_paths(table: &Table, prefix: &mut Vec<String>, paths: &mut Vec<String>) {
    for (key, item) in table.iter() {
        prefix.push(key.to_string());
        match item {
            Item::Table(child) => collect_paths(child, prefix, paths),
            Item::ArrayOfTables(tables) => {
                paths.push(prefix.join("."));
                if let Some(first) = tables.iter().next() {
                    collect_paths(first, prefix, paths);
                }
            }
            Item::Value(_) => paths.push(prefix.join(".")),
            Item::None => {}
        }
        prefix.pop();
    }
}

/// 把一个 `分组.字段` 的改动写进 TOML 文档。
fn apply_change(document: &mut DocumentMut, key: &str, value: &Value) -> anyhow::Result<()> {
    let segments: Vec<&str> = key
        .split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        anyhow::bail!("字段名不能为空");
    }
    if segments
        .iter()
        .any(|segment| segment.contains(['[', ']', '"']))
    {
        anyhow::bail!("字段名不合法");
    }

    let mut table = document.as_table_mut();
    for segment in &segments[..segments.len() - 1] {
        let entry = table
            .entry(segment)
            .or_insert_with(|| Item::Table(Table::new()));
        if !entry.is_table() {
            // 原来是个标量却要往下写子字段：说明调用方给错了路径，
            // 直接报错比悄悄把它换成表更安全。
            anyhow::bail!("{segment} 已经是标量，不能写入子字段");
        }
        table = entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("{segment} 不是表"))?;
    }

    let last = segments[segments.len() - 1];
    if value.is_null() {
        // null 表示"删掉这个键，回到默认值"。
        table.remove(last);
        return Ok(());
    }

    let item = json_to_item(value)?;
    if let Some(existing) = table.get_mut(last)
        && let Some(existing_value) = existing.as_value_mut()
        && let Item::Value(new_value) = item.clone()
    {
        // 原地换值：`table.insert` 会把整个 Item（连同它的 decor）换成新的，
        // 那样键上方和行尾的注释就没了——运维写在配置里的说明比新值本身更值钱，
        // 所以这里显式把原来的 decor 搬回来。
        let decor = existing_value.decor().clone();
        *existing_value = new_value;
        *existing_value.decor_mut() = decor;
        return Ok(());
    }

    table.insert(last, item);
    Ok(())
}

fn json_to_item(value: &Value) -> anyhow::Result<Item> {
    let item = match value {
        Value::Null => Item::None,
        Value::Bool(flag) => Item::Value(TomlValue::from(*flag)),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Item::Value(TomlValue::from(integer))
            } else if let Some(float) = number.as_f64() {
                Item::Value(TomlValue::from(float))
            } else {
                anyhow::bail!("不支持的数值 {number}");
            }
        }
        Value::String(text) => Item::Value(TomlValue::from(text.as_str())),
        Value::Array(items) => {
            if items.iter().all(Value::is_object) && !items.is_empty() {
                let mut tables = ArrayOfTables::new();
                for item in items {
                    let mut table = Table::new();
                    fill_table(&mut table, item)?;
                    tables.push(table);
                }
                Item::ArrayOfTables(tables)
            } else {
                let mut array = Array::new();
                for item in items {
                    match json_to_item(item)? {
                        Item::Value(value) => array.push(value),
                        _ => anyhow::bail!("数组里不能嵌套表数组"),
                    }
                }
                Item::Value(TomlValue::Array(array))
            }
        }
        Value::Object(map) => {
            let mut table = Table::new();
            for (key, child) in map {
                table.insert(key, json_to_item(child)?);
            }
            Item::Table(table)
        }
    };
    Ok(item)
}

fn fill_table(table: &mut Table, value: &Value) -> anyhow::Result<()> {
    let Value::Object(map) = value else {
        anyhow::bail!("数组元素必须是对象");
    };
    for (key, child) in map {
        table.insert(key, json_to_item(child)?);
    }
    Ok(())
}

/// 保存前把现有文件复制成 `*.bak.<时间戳>`，并清理超过上限的旧备份。
fn backup(path: &Path) -> Result<Option<PathBuf>, ApiError> {
    if !path.exists() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    // 同一毫秒内连续两次保存（例如回滚紧跟着保存）不能互相覆盖，
    // 否则"上一次的备份"会被刚写坏的内容顶掉。
    let mut backup_path = PathBuf::from(format!("{}.bak.{stamp}", path.display()));
    let mut suffix = 0;
    while backup_path.exists() {
        suffix += 1;
        backup_path = PathBuf::from(format!("{}.bak.{stamp}.{suffix}", path.display()));
    }
    fs::copy(path, &backup_path)
        .map_err(|error| ApiError::internal(format!("备份失败: {error}")))?;

    let mut existing = backups_of(path);
    existing.sort();
    while existing.len() > MAX_BACKUPS {
        let oldest = existing.remove(0);
        let _ = fs::remove_file(oldest);
    }
    Ok(Some(backup_path))
}

/// 某个文件的备份列表，按时间从旧到新。
///
/// 备份与目标文件**同目录**（`backup()` 就是这么写的），所以必须去目标文件所在
/// 目录里找。早先这里扫的是进程 CWD，而生产的 CWD 是只读的发布目录
/// (`current/`)，与覆盖配置所在的运行时可写目录不是同一个——于是
/// `bot.conf.override.toml` 的备份在后台永远看不见：列表恒为空（`backups=0`）、
/// 还原接口够不着，连 `backup()` 里的 `MAX_BACKUPS` 清理也从没对它执行过。
fn backups_of(path: &Path) -> Vec<PathBuf> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let prefix = format!("{name}.bak.");
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(backup_dir(path)) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// 备份所在目录：目标文件的父目录。
///
/// `config_path()` 对主配置返回的是裸文件名（相对工作目录），它的
/// `parent()` 是空串，这时按当前目录处理。
fn backup_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn backup_target(backup_name: &str) -> Option<String> {
    // `bot.conf.toml.bak.<毫秒>` 或 `.bak.<毫秒>.<自增>`。
    let (base, _) = backup_name.split_once(".bak.")?;
    MANAGED_FILES
        .iter()
        .find(|name| **name == base)
        .map(|name| (*name).to_string())
}

/// 只接受形如 `bot.conf.toml.bak.1730000000` 的备份名，避免被当成任意路径读。
///
/// 在**目标文件所在目录**里解析：主配置在只读的发布目录、覆盖配置在运行时可写
/// 目录，两者目录不同；按进程 CWD 解析在生产上只会指向发布目录，覆盖配置的备份
/// 因此既列不出来、也还不了原。
fn resolve_backup(name: &str) -> Result<PathBuf, ApiError> {
    let target_name = backup_target(name).ok_or_else(|| ApiError::bad_request("备份名不合法"))?;
    let target = config_path(&target_name)?;
    resolve_backup_in(backup_dir(&target), name)
}

/// 在一个目录里定位备份文件。
///
/// 名字形状的校验也放在这里，而不是只放在调用方：任何新增的调用点都不会因为
/// 忘了先校验而把它变成任意路径读取。
fn resolve_backup_in(directory: &Path, name: &str) -> Result<PathBuf, ApiError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ApiError::bad_request("备份名不合法"));
    }
    let path = directory.join(name);
    if !path.is_file() {
        return Err(ApiError::not_found("备份不存在"));
    }
    Ok(path)
}

/// 原子写：同目录临时文件 + rename，避免半截配置被读到。
///
/// 标注接口（`annotation_api`）复用它：改一份 JSONL 批次和改配置一样，
/// 都不能让读者（例如离线训练器）看到写了一半的文件。
pub(crate) fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let temp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
    fs::write(&temp, text)?;
    if let Ok(metadata) = fs::metadata(path) {
        // 保留原有权限（配置里可能有 Token，通常是 600）。
        let _ = fs::set_permissions(&temp, metadata.permissions());
    }
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写主配置时必须把磁盘上的运行时覆盖一并算进去。
    ///
    /// 单独校验主配置的后果不只是误判跨段规则：`write_raw`/`patch`/`restore_backup`
    /// 都会 `install` 校验结果，于是后台改一个主配置字段，会把覆盖里的设置**从内存里
    /// 挤掉**（磁盘上还在），要等下次重启才回来，而界面上看不出任何异常。
    #[test]
    fn writing_the_main_config_keeps_the_runtime_override() {
        let dir = std::env::temp_dir().join(format!("kovi-admin-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("应建临时目录");
        let override_path = dir.join("bot.conf.override.toml");
        std::fs::write(&override_path, "[vision]\nprovider = \"intrinsic\"\n")
            .expect("应写临时覆盖");

        let file = managed_file("bot.conf.toml").expect("主配置应在白名单里");
        let validated = validate_candidate_with_override(
            file,
            "[model]\npush_probability_percent = 35\n",
            &override_path,
        )
        .expect("候选主配置应通过校验")
        .expect("类型化文件应返回解析结果");
        assert_eq!(
            validated.vision().provider(),
            "intrinsic",
            "写主配置不能把运行时覆盖丢掉"
        );

        // 覆盖文件本身走另一条分支（与主配置合并），确认没被改坏。
        let override_file = managed_file("bot.conf.override.toml").expect("覆盖配置应在白名单里");
        assert!(
            validate_candidate_with_override(
                override_file,
                "[vision]\nprovider = \"intrinsic\"\n",
                &override_path,
            )
            .is_ok()
        );

        std::fs::remove_file(&override_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    /// 原始编辑器的打码/还原必须成对：只打码不还原，管理员点一次保存就把密钥写成
    /// `********`；只还原不打码，密钥就一直在屏幕上。
    #[test]
    fn raw_secrets_are_masked_for_display_and_restored_on_write() {
        let current = "[server]\n# 桥的访问令牌\naccess_token = \"real-token\"\nport = 3001\n";
        let masked = vec!["server.access_token".to_string()];

        let display = mask_raw_secrets(current, &masked);
        assert!(!display.contains("real-token"), "密钥不能回显: {display}");
        assert!(display.contains(MASK));
        assert!(display.contains("port = 3001"), "别的键要原样保留");
        assert!(display.contains("# 桥的访问令牌"), "注释要原样保留");

        // 原样存回：还原成磁盘真值。
        let saved = restore_masked_secrets(&display, current, &masked);
        assert!(
            saved.contains("real-token"),
            "原样保存不能把密钥换成星号: {saved}"
        );
        assert!(!saved.contains(MASK));

        // 改了别的字段：密钥同样要保住。
        let edited = display.replace("port = 3001", "port = 3002");
        let saved = restore_masked_secrets(&edited, current, &masked);
        assert!(saved.contains("real-token") && saved.contains("port = 3002"));

        // 管理员显式轮换了密钥：要写进去，不能被他刚填的值之外的东西覆盖。
        let rotated = display.replace(MASK, "new-token");
        let saved = restore_masked_secrets(&rotated, current, &masked);
        assert!(saved.contains("new-token"), "轮换要生效: {saved}");

        // 路径在本文件里不存在（密钥只在覆盖里）时不该改动任何东西。
        let untouched = mask_raw_secrets(current, &["other.secret".to_string()]);
        assert_eq!(untouched, current);
    }

    #[test]
    fn a_bare_config_name_is_backed_up_in_the_working_directory() {
        assert_eq!(backup_dir(Path::new("bot.conf.toml")), Path::new("."));
        assert_eq!(
            backup_dir(Path::new("/var/lib/kovi/runtime/bot.conf.override.toml")),
            Path::new("/var/lib/kovi/runtime")
        );
    }

    #[test]
    fn backups_are_listed_from_the_target_directory_not_the_working_directory() {
        // 生产上 CWD 是只读的发布目录，覆盖配置的备份在运行时可写目录里，
        // 两者不是同一个目录——所以定位必须跟着目标文件走。
        let dir = std::env::temp_dir().join(format!("kovi-config-backups-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("应能建临时目录");
        let target = dir.join("bot.conf.override.toml");
        fs::write(&target, "enabled = true\n").expect("应能写目标文件");
        let older = dir.join("bot.conf.override.toml.bak.1000");
        let newer = dir.join("bot.conf.override.toml.bak.2000");
        fs::write(&older, "old\n").expect("应能写备份");
        fs::write(&newer, "new\n").expect("应能写备份");
        // 同目录里别的文件的备份不能被算进来（前缀必须连文件名一起匹配）。
        fs::write(dir.join("bot.conf.toml.bak.3000"), "other\n").expect("应能写备份");

        assert_eq!(
            backups_of(&target),
            vec![older, newer],
            "应按时间从旧到新，且只列自己的备份"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_names_resolve_inside_the_target_directory_and_reject_traversal() {
        let dir = std::env::temp_dir().join(format!("kovi-config-restore-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("应能建临时目录");
        let name = "bot.conf.override.toml.bak.1789323903611";
        fs::write(dir.join(name), "raw\n").expect("应能写备份");

        assert_eq!(
            resolve_backup_in(&dir, name).expect("同目录的备份应当解析得到"),
            dir.join(name)
        );
        for rejected in [
            "",
            "../bot.conf.override.toml",
            "sub/dir.bak.1",
            "a\\b",
            ".",
        ] {
            assert!(
                resolve_backup_in(&dir, rejected).is_err(),
                "{rejected:?} 不该被接受"
            );
        }
        assert!(
            resolve_backup_in(&dir, "bot.conf.override.toml.bak.404").is_err(),
            "不存在的备份应当报错"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scalar_changes_are_applied_without_touching_comments() {
        let source = "# 顶部注释\n[memory]\n# 记忆条数\nmax_entries = 1000\n";
        let mut document = source.parse::<DocumentMut>().expect("应可解析");
        apply_change(&mut document, "memory.max_entries", &json!(500)).expect("应可写入");
        let text = document.to_string();
        assert!(text.contains("# 顶部注释"));
        assert!(text.contains("# 记忆条数"));
        assert!(text.contains("max_entries = 500"));
        assert!(!text.contains("1000"));
    }

    #[test]
    fn missing_tables_are_created_on_demand() {
        let mut document = "".parse::<DocumentMut>().expect("空文档应可解析");
        apply_change(&mut document, "executive.conflict.threshold", &json!(0.7))
            .expect("应可创建中间表");
        let text = document.to_string();
        assert!(text.contains("[executive.conflict]"), "实际: {text}");
        assert!(text.contains("threshold = 0.7"));
    }

    #[test]
    fn null_removes_the_key_so_defaults_apply_again() {
        let mut document = "[identity]\nowner_person_id = \"abc\"\n"
            .parse::<DocumentMut>()
            .expect("应可解析");
        apply_change(&mut document, "identity.owner_person_id", &Value::Null).expect("应可删除");
        assert!(!document.to_string().contains("owner_person_id"));
    }

    #[test]
    fn arrays_of_tables_round_trip() {
        let mut document = "".parse::<DocumentMut>().expect("空文档应可解析");
        let value = json!([{ "name": "notes", "command": "notes-mcp", "args": ["--stdio"] }]);
        apply_change(&mut document, "tools.mcp_servers", &value).expect("应可写入数组表");
        let text = document.to_string();
        assert!(text.contains("[[tools.mcp_servers]]"), "实际: {text}");
        assert!(text.contains("command = \"notes-mcp\""));
    }

    #[test]
    fn array_replacement_keeps_the_comment_above_the_key() {
        let mut document = "# 受信任的 MCP 服务\ntools_placeholder = 1\n"
            .parse::<DocumentMut>()
            .expect("应可解析");
        apply_change(&mut document, "tools_placeholder", &json!([1, 2])).expect("应可写入数组");
        let text = document.to_string();
        assert!(text.contains("# 受信任的 MCP 服务"), "实际: {text}");
    }

    #[test]
    fn scalar_cannot_be_replaced_by_a_table() {
        let mut document = "[memory]\nmax_entries = 1000\n"
            .parse::<DocumentMut>()
            .expect("应可解析");
        let error = apply_change(&mut document, "memory.max_entries.value", &json!(1))
            .expect_err("标量下写子字段必须失败");
        assert!(error.to_string().contains("标量"));
    }

    #[test]
    fn secret_fields_are_masked_and_paths_reported() {
        let values = json!({
            "server_config": { "actor_authorization": "secret-value", "url": "https://example.com" },
            "admin": { "token": "", "port": 6098 },
        });
        let (masked, paths) = mask_typed(values);
        assert_eq!(masked["server_config"]["actor_authorization"], MASK);
        assert_eq!(masked["server_config"]["url"], "https://example.com");
        // 空密钥不需要打码：它本来就没有秘密。
        assert_eq!(masked["admin"]["token"], "");
        assert!(paths.contains(&"server_config.actor_authorization".to_string()));
    }

    #[test]
    fn secret_detection_skips_pointers_to_secrets() {
        // 真正的密钥
        assert!(is_secret_key("access_token"));
        assert!(is_secret_key("pulse_cookie"));
        assert!(is_secret_key("actor_authorization"));
        // 「密钥放在哪里」不是密钥
        assert!(!is_secret_key("token_env"));
        assert!(!is_secret_key("api_key_env"));
        assert!(!is_secret_key("bridge_token_file"));
    }

    #[test]
    fn backup_target_accepts_millisecond_names() {
        assert_eq!(
            backup_target("bot.conf.toml.bak.1789266118123.2").as_deref(),
            Some("bot.conf.toml")
        );
    }

    #[test]
    fn generic_files_mask_token_like_keys_only() {
        let mut values = json!({
            "server": { "host": "127.0.0.1", "access_token": "abc" },
            "config": { "debug": false },
        });
        let mut masked = Vec::new();
        mask_generic(&mut values, &mut Vec::new(), &mut masked);
        assert_eq!(values["server"]["access_token"], MASK);
        assert_eq!(values["server"]["host"], "127.0.0.1");
        assert_eq!(masked, vec!["server.access_token".to_string()]);
    }

    #[test]
    fn present_paths_lists_what_the_file_actually_sets() {
        let source = "[memory]\nmax_entries = 1000\n\n[executive.conflict]\nthreshold = 0.6\n";
        let paths = present_paths(source);
        assert!(paths.contains(&"memory.max_entries".to_string()));
        assert!(paths.contains(&"executive.conflict.threshold".to_string()));
        assert!(!paths.contains(&"memory.retention_days".to_string()));
    }

    #[test]
    fn restart_sections_are_detected_from_changed_paths() {
        assert_eq!(
            restart_sections_hit(&["tools.web_fetch_enabled".to_string()]),
            vec!["tools".to_string()]
        );
        assert_eq!(
            restart_sections_hit(&["model.turn_gate.mode".to_string()]),
            vec!["model.turn_gate".to_string()]
        );
        // 热加载分区不该被误报。
        assert!(restart_sections_hit(&["mood.cache_ttl_secs".to_string()]).is_empty());
        // 数组路径同样能对上分区。
        assert_eq!(
            restart_sections_hit(&["tools.mcp_servers[].name".to_string()]),
            vec!["tools".to_string()]
        );
    }

    #[test]
    fn managed_file_names_are_whitelisted() {
        assert!(managed_file("bot.conf.toml").is_ok());
        assert!(managed_file("../bot.conf.toml").is_err());
        assert!(managed_file("/etc/passwd").is_err());
        assert!(managed_file("kovi.conf.toml").is_ok());
    }

    #[test]
    fn backup_names_resolve_only_for_managed_files() {
        assert_eq!(
            backup_target("bot.conf.toml.bak.1730000000").as_deref(),
            Some("bot.conf.toml")
        );
        assert_eq!(backup_target("/etc/passwd"), None);
        assert_eq!(backup_target("bot.conf.toml"), None);
    }

    #[test]
    fn embedded_docs_describe_the_admin_section() {
        // 管理后台自己的参数也必须在界面上有说明：它是最容易被改坏的一段。
        let docs = docs();
        assert!(docs["sections"]["admin"].is_string());
        assert!(docs["fields"]["admin.port"]["doc"].is_string());
        assert_eq!(docs["fields"]["admin.token"]["secret"], json!(true));
    }
}
