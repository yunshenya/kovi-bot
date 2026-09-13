//! 数据标注接口：TurnGate 复核闭环的网页端（设计文档 §7.4 B）。
//!
//! 与 `tools/turngate/review.py` 读写**同一份 JSONL、同一套语义**：
//!
//! - 队列按"标注价值"排序，只用不依赖策略的客观信号（doc §7.4 E：线上"实际回
//!   了没有"只能作采样依据，不能当标签）；
//! - 标注写入 `label_provenance.source = human_consensus` 与
//!   `review_status = reviewed`，字段与 `--mark` 逐项一致；
//! - 导出只收人工复核过且 agreement 达标的样本，并剥掉 `review_status` 与
//!   `source_key`（doc §7.4 D：这两个字段不进训练集）。
//!
//! 数据只在本机读写：目录来自 `admin.annotation_dir`（默认运行时目录下的
//! `turngate/`），文件名走白名单，任何请求都拼不出目录之外的路径。
//!
//! 并发：进程内用一把互斥锁串行化"读-改-写"，落盘用原子替换；跨进程（运维同时
//! 在终端跑 `review.py`）靠 `revision` 乐观校验兜底——版本对不上就返回 409，
//! 让调用方刷新后重来，而不是把对方的改动悄悄盖掉。

use super::ApiError;
use super::config_api::{directory_writable, write_atomically};
use crate::config;
use axum::Json;
use axum::extract::Query;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 支持的批次 schema 版本（doc §7.5：未知版本拒绝进入训练器）。
const SUPPORTED_SCHEMA_VERSION: u64 = 2;
/// 人工复核的来源标记，与 `review.py` 的 `REVIEWED_SOURCE` 一致。
const REVIEWED_SOURCE: &str = "human_consensus";
const STATUS_REVIEWED: &str = "reviewed";
/// 两个 head 的合法标签（doc §7.5）。`null` 是"判不了"的合法中间态。
const COMPLETION_LABELS: [&str; 2] = ["flush_now", "hold_for_more"];
const RESPONSE_LABELS: [&str; 5] = ["answer", "continue", "ack", "ignore", "wait"];
/// 会被 `queue_reason` 列出的上下文开关，顺序与 `review.py` 一致。
const FLAG_NAMES: [&str; 7] = [
    "addressed_to_agent",
    "replies_to_agent",
    "conversation_active",
    "has_image",
    "has_sticker",
    "pending_outgoing",
    "pending_task",
];
/// 导出文件名前缀：`/batches` 把它当产出而不是待标注批次。
const EXPORT_PREFIX: &str = "train-";
/// 单批次体积上限。真实批次是 1~2 MB 量级，超过它说明路径指错了。
const MAX_BATCH_BYTES: u64 = 64 * 1024 * 1024;
/// 队列单页上限，避免一个请求把整个批次塞进响应。
const QUEUE_LIMIT_DEFAULT: usize = 40;
const QUEUE_LIMIT_MAX: usize = 500;
/// 批次列表里最多解析多少个文件的进度（每个都要整份读一遍）。
const BATCH_SUMMARY_LIMIT: usize = 20;
/// 导出默认阈值，与 `review.py --min-agreement` 的默认值一致。
const DEFAULT_MIN_AGREEMENT: f64 = 0.9;

// ───────────────────────────── 路径与文件 ─────────────────────────────

/// 校验批次文件名：只接受标注目录里的裸 `.jsonl` 文件名。
///
/// `review.py` 与 `collector.py` 生成的名字（`review-batch-20260912.jsonl`、
/// `train_turngate-v0.1.jsonl`）都在这个字符集里。拒绝分隔符与 `..` 之后，
/// `join` 的结果必然是该目录的直接子项，请求无法用它去指别处的文件。
fn validate_batch_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let reject = || {
        ApiError::bad_request(
            "批次名不合法：只接受标注目录内的 *.jsonl 文件名（字母、数字、`.`、`_`、`-`）",
        )
    };
    if name.is_empty() || name.len() > 128 || !name.ends_with(".jsonl") {
        return Err(reject());
    }
    if name.starts_with('.') {
        return Err(reject());
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        return Err(reject());
    }
    Ok(name)
}

fn batch_path(name: &str) -> Result<PathBuf, ApiError> {
    Ok(config::annotation_dir_path().join(validate_batch_name(name)?))
}

/// `sha256` 前 16 位：给客户端的乐观并发令牌。
fn revision_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn load_error(path: &Path, error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            ApiError::not_found(format!("批次不存在: {}", path.display()))
        }
        _ => ApiError::internal(format!("读取批次失败 ({}): {error}", path.display())),
    }
}

/// 标注目录读写失败的统一解释。
///
/// 生产部署里 `current/` 是只读发布目录（systemd `ProtectSystem=strict`），
/// 只有运行时目录可写；这两条错误就是踩到它时的解释。
fn dir_error(dir: &Path, action: &str, error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => {
            ApiError::bad_request(format!(
                "{action}失败 ({}): {error}；生产部署只能写运行时目录，\
                 请把 admin.annotation_dir 指到 runtime/ 下",
                dir.display()
            ))
        }
        _ => ApiError::internal(format!("{action}失败 ({}): {error}", dir.display())),
    }
}

fn write_error(path: &Path, error: std::io::Error) -> ApiError {
    dir_error(path, "写入批次", error)
}

/// 确保标注目录存在（幂等），返回解析后的路径。
///
/// 目录完全由 `admin.annotation_dir` 推导、不含任何请求输入，所以建它不需要
/// 额外授权：部署完就该能直接把批次 scp 进来（scp 到不存在的目录会直接失败），
/// 打开页面时也不该先让人手工 `mkdir`。
pub(crate) fn ensure_dir() -> Result<PathBuf, ApiError> {
    let dir = config::annotation_dir_path();
    if !dir.is_dir() {
        fs::create_dir_all(&dir).map_err(|error| dir_error(&dir, "创建标注目录", error))?;
    }
    Ok(dir)
}

/// 串行化本进程内的读-改-写。跨进程干扰由 `revision` 校验兜底。
fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 一个已经解析进内存的批次。
struct Batch {
    path: PathBuf,
    /// 文件字节的 sha256 前 16 位。
    revision: String,
    samples: Vec<Value>,
}

fn parse_jsonl(text: &str) -> Result<Vec<Value>, ApiError> {
    let mut samples = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let sample: Value = serde_json::from_str(line).map_err(|error| {
            ApiError::bad_request(format!("批次第 {} 行不是合法 JSON: {error}", number + 1))
        })?;
        if !sample.is_object() {
            return Err(ApiError::bad_request(format!(
                "批次第 {} 行不是 JSON 对象",
                number + 1
            )));
        }
        match sample.get("schema_version").and_then(Value::as_u64) {
            Some(SUPPORTED_SCHEMA_VERSION) => {}
            Some(other) => {
                return Err(ApiError::bad_request(format!(
                    "批次第 {} 行的 schema_version={other} 不受支持（本后台只认 {SUPPORTED_SCHEMA_VERSION}）",
                    number + 1
                )));
            }
            None => {
                return Err(ApiError::bad_request(format!(
                    "批次第 {} 行缺少 schema_version",
                    number + 1
                )));
            }
        }
        samples.push(sample);
    }
    if samples.is_empty() {
        return Err(ApiError::bad_request("批次里没有任何样本"));
    }
    Ok(samples)
}

/// 渲染成 JSONL。等价于 `review.py` 的
/// `json.dumps(..., ensure_ascii=False, separators=(",", ":"))`：紧凑、中文不转义
/// （键序按 serde_json 的 BTreeMap 归一化，JSON 语义不受影响，两边都能读）。
fn render_jsonl(samples: &[Value]) -> Result<String, ApiError> {
    let mut out = String::new();
    for sample in samples {
        out.push_str(
            &serde_json::to_string(sample)
                .map_err(|error| ApiError::internal(format!("序列化样本失败: {error}")))?,
        );
        out.push('\n');
    }
    Ok(out)
}

fn load_batch_at(path: &Path) -> Result<Batch, ApiError> {
    let metadata = fs::metadata(path).map_err(|error| load_error(path, error))?;
    if metadata.len() > MAX_BATCH_BYTES {
        return Err(ApiError::bad_request(format!(
            "批次 {} 有 {} 字节，超过上限 {} 字节",
            path.display(),
            metadata.len(),
            MAX_BATCH_BYTES
        )));
    }
    let bytes = fs::read(path).map_err(|error| load_error(path, error))?;
    let text = String::from_utf8(bytes.clone())
        .map_err(|error| ApiError::bad_request(format!("批次不是 UTF-8: {error}")))?;
    let samples = parse_jsonl(&text)?;
    Ok(Batch {
        path: path.to_path_buf(),
        revision: revision_of(&bytes),
        samples,
    })
}

/// 写回批次，并返回写后内容的版本号。
fn save_batch_at(path: &Path, samples: &[Value]) -> Result<String, ApiError> {
    let text = render_jsonl(samples)?;
    write_atomically(path, &text).map_err(|error| write_error(path, error))?;
    Ok(revision_of(text.as_bytes()))
}

async fn blocking<T, F>(work: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    kovi::tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ApiError::internal(format!("标注任务执行失败: {error}")))?
}

// ───────────────────────────── 样本解读 ─────────────────────────────

/// 空上下文/空标签的替身，避免到处 `unwrap`。
fn null_value() -> &'static Value {
    static NULL: Value = Value::Null;
    &NULL
}

fn context_of(sample: &Value) -> &Value {
    sample.get("context").unwrap_or(null_value())
}

fn labels_of(sample: &Value) -> &Value {
    sample.get("labels").unwrap_or(null_value())
}

fn flag(context: &Value, name: &str) -> bool {
    context.get(name).and_then(Value::as_bool).unwrap_or(false)
}

/// 与 `review.py` 的 `human_labels()` 一致：只有枚举内的值才算有效标签，
/// 其余（含 `null`、缺字段、脏数据）一律当作"没标"。
fn human_labels(sample: &Value) -> (Option<&str>, Option<&str>) {
    let labels = labels_of(sample);
    let read = |name: &str, allowed: &[&str]| -> Option<&str> {
        labels
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| allowed.contains(value))
    };
    (
        read("completion", &COMPLETION_LABELS),
        read("response", &RESPONSE_LABELS),
    )
}

fn is_reviewed(sample: &Value) -> bool {
    sample.get("review_status").and_then(Value::as_str) == Some(STATUS_REVIEWED)
}

/// 这条样本带了多少可判断的上下文（越少越依赖模型，越值得标）。
fn context_richness(sample: &Value) -> usize {
    let context = context_of(sample);
    [
        "recent_turns",
        "pending_user_fragments",
        "conversation_active",
        "bot_last_asked_question",
        "pending_outgoing",
        "pending_task",
    ]
    .iter()
    .filter(|name| match context.get(**name) {
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => !text.is_empty(),
        _ => false,
    })
    .count()
}

/// 给待复核样本排"标注价值"的理由，只使用不依赖策略的客观信号。
fn queue_reason(sample: &Value) -> String {
    let context = context_of(sample);
    let (completion, _) = human_labels(sample);
    let mut reasons = Vec::new();
    if completion.is_none() {
        // lexical 规则给不出判断：这正是需要模型补位的灰区，信息量最大。
        reasons.push("gray-zone");
    }
    if !has_context(sample) {
        reasons.push("no-context");
    }
    if !has_bot_turn(sample) {
        reasons.push("no-bot-turn");
    }
    if flag(context, "addressed_to_agent") || flag(context, "replies_to_agent") {
        reasons.push("addressed");
    }
    if !fragments_of(sample).is_empty() {
        reasons.push("multi-fragment");
    }
    if flag(context, "has_image") {
        reasons.push("image");
    }
    if reasons.is_empty() {
        "context-rich".to_owned()
    } else {
        reasons.join(",")
    }
}

fn recent_turns(sample: &Value) -> &[Value] {
    context_of(sample)
        .get("recent_turns")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn fragments_of(sample: &Value) -> &[Value] {
    context_of(sample)
        .get("pending_user_fragments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn has_context(sample: &Value) -> bool {
    !recent_turns(sample).is_empty()
}

fn has_bot_turn(sample: &Value) -> bool {
    recent_turns(sample)
        .iter()
        .any(|turn| turn.get("role").and_then(Value::as_str) == Some("assistant"))
}

/// 越小越先标：灰区（弱标签沉默）> 上下文薄弱 > 其余。
fn queue_tier(sample: &Value) -> u8 {
    let context = context_of(sample);
    let (completion, _) = human_labels(sample);
    if completion.is_none() {
        return if flag(context, "addressed_to_agent") || flag(context, "replies_to_agent") {
            0
        } else {
            1
        };
    }
    if !has_context(sample) {
        return 2;
    }
    if !has_bot_turn(sample) {
        return 3;
    }
    4
}

/// tier 的含义，下标就是 tier 编号，与 [`queue_tier`] 的取值域一一对应。
///
/// 口径与 `tools/turngate/review.py::queue_tier`（以及它的 `--queue` 表头)相同；
/// 前端不再抄一份文案，说明书跟着分布一起从接口下发，改口径时只改这里。
const TIER_LABELS: [&str; 5] = ["灰区 + 被叫到", "灰区", "无上下文", "无机器人发言", "其余"];

/// 待标队列的 tier 分布，下标 = tier。
///
/// 统计范围必须与 `matched` 完全一致——传进来的就是筛过 reviewed/flagged、
/// 排过序的队列本身，否则页头的数字会和列表对不上。
fn tier_distribution<'a>(samples: impl IntoIterator<Item = &'a Value>) -> Vec<usize> {
    let mut counts = vec![0_usize; TIER_LABELS.len()];
    for sample in samples {
        // 不变量：queue_tier 的取值域就是 TIER_LABELS 的下标（单测锁着）。真越界
        // 说明有人加了 tier 却没补说明，这里宁可炸掉也不要悄悄漏统计。
        counts[usize::from(queue_tier(sample))] += 1;
    }
    counts
}

/// 采于"@ 判定"修复之前的批次里，目标不可知的样本。
///
/// 当时 kovi 把**任何人**的 @ 都渲染成 `[at]`，于是"@ 别人"被记成
/// `addressed_to_agent = true`（修复见 commit 85f115c）。带 at/reply 标记又声称
/// 在叫她的样本，目标其实分不出来——默认从队列里排除，免得把"别人被 @ 时该回她"
/// 人工确认一遍再喂进权重（doc §7.4 E）。
///
/// 新采集器会写 `context.targeting`，这类批次不再受影响。
fn legacy_targeting_risk(sample: &Value) -> bool {
    let context = context_of(sample);
    if context.get("targeting").is_some() {
        return false;
    }
    if !(flag(context, "addressed_to_agent") || flag(context, "replies_to_agent")) {
        return false;
    }
    let mut text = sample
        .get("current_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    for fragment in fragments_of(sample) {
        if let Some(fragment) = fragment.as_str() {
            text.push(' ');
            text.push_str(fragment);
        }
    }
    text.contains("[at]") || text.contains("[reply]")
}

fn set_flags(sample: &Value) -> Vec<&'static str> {
    let context = context_of(sample);
    FLAG_NAMES
        .iter()
        .filter(|name| flag(context, name))
        .copied()
        .collect()
}

/// `source_key` 只是本机的删除屏障（doc §7.4 D），没有理由交给浏览器。
fn public_sample(sample: &Value) -> Value {
    let mut value = sample.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("source_key");
    }
    value
}

fn label_distribution(samples: &[Value], head: usize) -> BTreeMap<String, u64> {
    let mut distribution = BTreeMap::new();
    for sample in samples {
        let labels = human_labels(sample);
        let value = if head == 0 { labels.0 } else { labels.1 };
        *distribution
            .entry(value.unwrap_or("null").to_owned())
            .or_insert(0) += 1;
    }
    distribution
}

fn summarize(samples: &[Value]) -> Value {
    let reviewed = samples.iter().filter(|sample| is_reviewed(sample)).count();
    let flagged = samples
        .iter()
        .filter(|sample| legacy_targeting_risk(sample))
        .count();
    json!({
        "total": samples.len(),
        "reviewed": reviewed,
        "pending": samples.len() - reviewed,
        "flagged": flagged,
        "completion": label_distribution(samples, 0),
        "response": label_distribution(samples, 1),
    })
}

// ───────────────────────────── 标注与导出 ─────────────────────────────

/// 校验客户端传来的一个 head 的取值。
///
/// 返回 `None` 表示"别动这个 head"；`Some(None)` 表示"显式记为 null"。
/// 与 `review.py --mark` 的差别：非法取值在这里直接 400，而不是静默写成
/// `null`——手滑把标签清空比报错更难发现。
fn requested_label(
    value: Option<&str>,
    allowed: &[&'static str],
    head: &str,
    clear: bool,
) -> Result<Option<Option<&'static str>>, ApiError> {
    if clear && value.is_some() {
        return Err(ApiError::bad_request(format!(
            "{head} 不能同时给取值和 clear_{head}"
        )));
    }
    if clear {
        return Ok(Some(None));
    }
    match value {
        None => Ok(None),
        Some(value) => allowed
            .iter()
            .find(|allowed| **allowed == value)
            .map(|allowed| Some(Some(*allowed)))
            .ok_or_else(|| {
                ApiError::bad_request(format!(
                    "{head} 取值不合法: {value}（只能是 {} 或显式记为 null）",
                    allowed.join(" / ")
                ))
            }),
    }
}

/// 把标签与来源写进样本，字段与 `review.py --mark` 完全一致。
fn apply_labels(
    sample: &mut Value,
    completion: Option<Option<&str>>,
    response: Option<Option<&str>>,
) -> Result<(), ApiError> {
    let object = sample
        .as_object_mut()
        .ok_or_else(|| ApiError::bad_request("样本不是 JSON 对象"))?;
    let labels = object
        .entry("labels")
        .or_insert_with(|| Value::Object(Map::new()));
    let labels = labels
        .as_object_mut()
        .ok_or_else(|| ApiError::bad_request("样本的 labels 不是 JSON 对象"))?;
    let mut put = |head: &str, value: Option<Option<&str>>| {
        if let Some(value) = value {
            labels.insert(
                head.to_owned(),
                value.map_or(Value::Null, |value| Value::String(value.to_owned())),
            );
        }
    };
    put("completion", completion);
    put("response", response);
    object.insert(
        "label_provenance".to_owned(),
        json!({
            "source": REVIEWED_SOURCE,
            "annotator_count": 1,
            "agreement": 1.0,
        }),
    );
    object.insert(
        "review_status".to_owned(),
        Value::String(STATUS_REVIEWED.to_owned()),
    );
    Ok(())
}

/// 校验版本与序号后打标落盘，返回写后的版本号与最新统计。
fn mark_in_batch(
    batch: &mut Batch,
    index: usize,
    expected_revision: &str,
    completion: Option<Option<&str>>,
    response: Option<Option<&str>>,
) -> Result<Value, ApiError> {
    if batch.revision != expected_revision {
        return Err(ApiError::conflict(
            "批次文件已被别处改动（另一个标签页或终端里的 review.py），请刷新后重试",
        ));
    }
    let total = batch.samples.len();
    let sample = batch.samples.get_mut(index).ok_or_else(|| {
        ApiError::bad_request(format!("序号 {index} 超出批次范围（共 {total} 条）"))
    })?;
    apply_labels(sample, completion, response)?;
    let path = batch.path.clone();
    let revision = save_batch_at(&path, &batch.samples)?;
    batch.revision = revision.clone();
    let summary = summarize(&batch.samples);
    let sample = &batch.samples[index];
    Ok(json!({
        "index": index,
        "revision": revision,
        "labels": labels_of(sample).clone(),
        "provenance": sample.get("label_provenance").cloned().unwrap_or(Value::Null),
        "review_status": sample.get("review_status").cloned().unwrap_or(Value::Null),
        "summary": summary,
    }))
}

/// 只收人工复核过、且 agreement 达标的样本；剥掉 `review_status` 与
/// `source_key`（doc §7.4 D）。
fn export_trainable(samples: &[Value], min_agreement: f64) -> (Vec<Value>, usize) {
    let mut exported = Vec::new();
    let mut human_reviewed = 0usize;
    for sample in samples {
        let provenance = sample.get("label_provenance").unwrap_or(null_value());
        if provenance.get("source").and_then(Value::as_str) != Some(REVIEWED_SOURCE) {
            continue;
        }
        human_reviewed += 1;
        let agreement = provenance
            .get("agreement")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if agreement < min_agreement {
            continue;
        }
        let mut sample = sample.clone();
        if let Some(object) = sample.as_object_mut() {
            object.remove("review_status");
            object.remove("source_key");
        }
        exported.push(sample);
    }
    (exported, human_reviewed)
}

fn export_file_name(batch: &str) -> String {
    let stem = batch.strip_suffix(".jsonl").unwrap_or(batch);
    format!("{EXPORT_PREFIX}{stem}.jsonl")
}

// ───────────────────────────── HTTP 接口 ─────────────────────────────

/// `GET /api/annotation/batches`
pub(crate) async fn batches() -> Result<Json<Value>, ApiError> {
    blocking(|| {
        // 目录是配置推导出来的，缺了就建（幂等）：页面第一次打开时就该是能用的
        // 状态。建不出来也不报错，把原因放进 `error` 让页面说清楚。
        let (dir, problem) = match ensure_dir() {
            Ok(dir) => (dir, Value::Null),
            Err(error) => (config::annotation_dir_path(), Value::String(error.message)),
        };
        let exists = dir.is_dir();
        let mut batches = Vec::new();
        let mut exports = Vec::new();
        let mut summaries = 0usize;
        let mut entries: Vec<(PathBuf, String)> = Vec::new();
        if exists {
            let listing = fs::read_dir(&dir)
                .map_err(|error| ApiError::internal(format!("读取标注目录失败: {error}")))?;
            for entry in listing {
                let entry = entry
                    .map_err(|error| ApiError::internal(format!("读取目录项失败: {error}")))?;
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if validate_batch_name(&name).is_err() {
                    continue;
                }
                entries.push((entry.path(), name));
            }
        }
        // 新的排前面：运维最关心的永远是刚采出来那一批。
        entries.sort_by_key(|(path, _)| {
            std::cmp::Reverse(
                fs::metadata(path)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
            )
        });
        for (path, name) in entries {
            let metadata = fs::metadata(&path).ok();
            let bytes = metadata.as_ref().map(fs::Metadata::len);
            if name.starts_with(EXPORT_PREFIX) {
                exports.push(json!({
                    "name": name,
                    "bytes": bytes,
                    "modified": modified_at(&path),
                }));
                continue;
            }
            let summary = if summaries < BATCH_SUMMARY_LIMIT {
                summaries += 1;
                // 读不动的批次照样列出来，但要说清楚为什么（多半是 schema 版本
                // 不受支持或文件被别的进程写坏），而不是显示成 0 条待标注。
                match load_batch_at(&path) {
                    Ok(batch) => summarize(&batch.samples),
                    Err(error) => json!({ "error": error.message }),
                }
            } else {
                Value::Null
            };
            batches.push(json!({
                "name": name,
                "bytes": bytes,
                "modified": modified_at(&path),
                "summary": summary,
            }));
        }
        Ok(json!({
            "dir": dir.display().to_string(),
            "exists": exists,
            "writable": directory_writable(&dir),
            "error": problem,
            "batches": batches,
            "exports": exports,
        }))
    })
    .await
    .map(Json)
}

fn modified_at(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let elapsed = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    chrono::DateTime::from_timestamp(elapsed as i64, 0).map(|time| time.to_rfc3339())
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueueQuery {
    batch: String,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    limit: Option<usize>,
    /// 连已复核的一起列出（队列 = 待标 + 已标）。它不是"只看已标"——那层语义是
    /// [`Self::reviewed_only`]。早先网页端的「已标注」页签把它当成了后者，于是
    /// 那个页签列出来的是整批样本（4302 条里混着大量待标的）。
    #[serde(default)]
    include_reviewed: bool,
    /// 只看已复核的：网页端「已标注」页签要的是这个。
    #[serde(default)]
    reviewed_only: bool,
    /// 默认跳过 `legacy_targeting_risk` 的样本（老批次里"@ 别人"被记成在叫她）。
    #[serde(default = "default_true")]
    skip_flagged: bool,
}

fn default_true() -> bool {
    true
}

/// 队列该收哪些样本。
///
/// `include_reviewed` 与 `reviewed_only` 是两层**不同**的语义，别混：
///
/// - `include_reviewed`：队列里也放已复核的（待标 + 已标都列）；
/// - `reviewed_only`：只要已复核的——网页端「已标注」页签要的是这个。
///
/// 早先只有前者，而「已标注」页签把它当成了后者，于是那个页签列出来的是整批样本
/// （实测 4302 条里混着 4289 条待标的，页签名字完全对不上，用户点开一条"已标注"
/// 看到的却是没标过的样本）。两者同时为真时取交集（只看已标），
/// `include_reviewed=false` + `reviewed_only=true` 则是空集——那是个自相矛盾的
/// 组合，接口不去纠正它，但行为要一眼看得出来。
fn queue_rows(
    samples: &[Value],
    include_reviewed: bool,
    reviewed_only: bool,
    skip_flagged: bool,
) -> Vec<(usize, &Value)> {
    samples
        .iter()
        .enumerate()
        .filter(|(_, sample)| include_reviewed || !is_reviewed(sample))
        .filter(|(_, sample)| !reviewed_only || is_reviewed(sample))
        .filter(|(_, sample)| !(skip_flagged && legacy_targeting_risk(sample)))
        .collect()
}

/// `GET /api/annotation/queue`
pub(crate) async fn queue(Query(params): Query<QueueQuery>) -> Result<Json<Value>, ApiError> {
    let limit = params
        .limit
        .unwrap_or(QUEUE_LIMIT_DEFAULT)
        .clamp(1, QUEUE_LIMIT_MAX);
    let batch_name = params.batch.clone();
    let offset = params.offset;
    let include_reviewed = params.include_reviewed;
    let reviewed_only = params.reviewed_only;
    let skip_flagged = params.skip_flagged;
    let body = blocking(move || {
        let path = batch_path(&batch_name)?;
        let batch = load_batch_at(&path)?;
        let samples = &batch.samples;
        let mut rows = queue_rows(samples, include_reviewed, reviewed_only, skip_flagged);
        rows.sort_by_key(|(index, sample)| {
            (
                queue_tier(sample),
                std::cmp::Reverse(context_richness(sample)),
                *index,
            )
        });
        let matched = rows.len();
        let tiers = tier_distribution(rows.iter().map(|(_, sample)| *sample));
        let items: Vec<Value> = rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(index, sample)| {
                let provenance = sample.get("label_provenance").unwrap_or(null_value());
                json!({
                    "index": index,
                    "tier": queue_tier(sample),
                    "richness": context_richness(sample),
                    "reason": queue_reason(sample),
                    "scope": context_of(sample).get("scope").cloned().unwrap_or(Value::Null),
                    "current_text": sample.get("current_text").cloned().unwrap_or(Value::Null),
                    "review_status": sample.get("review_status").cloned().unwrap_or(Value::Null),
                    "flagged": legacy_targeting_risk(sample),
                    "labels": labels_of(sample).clone(),
                    "provenance": provenance.get("source").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        // 覆盖率按"整批里还没标的部分"算，口径与 `review.py --queue` 的表头一致。
        let gray_zone = samples
            .iter()
            .filter(|sample| !is_reviewed(sample) && human_labels(sample).0.is_none())
            .count();
        let with_recent_turns = samples
            .iter()
            .filter(|sample| !is_reviewed(sample) && has_context(sample))
            .count();
        let with_bot_turn = samples
            .iter()
            .filter(|sample| !is_reviewed(sample) && has_bot_turn(sample))
            .count();
        Ok(json!({
            "batch": batch_name,
            "dir": config::annotation_dir_path().display().to_string(),
            "revision": batch.revision,
            "offset": offset,
            "limit": limit,
            "matched": matched,
            "items": items,
            "summary": summarize(samples),
            "skipped_flagged": skip_flagged,
            // 回显这次队列的口径：页签切到「已标注」时它应当是 true。
            "reviewed_only": reviewed_only,
            // 队列是按标注价值排的，首屏必然全是最高价值那一档；把分布和每档的
            // 含义一并给出去，"怎么全是 tier 0" 在页面上就能自答。
            "tiers": TIER_LABELS
                .iter()
                .zip(&tiers)
                .enumerate()
                .map(|(tier, (label, count))| json!({
                    "tier": tier,
                    "label": label,
                    "count": count,
                }))
                .collect::<Vec<Value>>(),
            "coverage": {
                "gray_zone": gray_zone,
                "with_recent_turns": with_recent_turns,
                "with_bot_turn": with_bot_turn,
            },
        }))
    })
    .await?;
    Ok(Json(body))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SampleQuery {
    batch: String,
    index: usize,
}

/// `GET /api/annotation/sample`
pub(crate) async fn sample(Query(params): Query<SampleQuery>) -> Result<Json<Value>, ApiError> {
    let batch_name = params.batch.clone();
    let index = params.index;
    let body = blocking(move || {
        let path = batch_path(&batch_name)?;
        let batch = load_batch_at(&path)?;
        let total = batch.samples.len();
        let sample = batch.samples.get(index).ok_or_else(|| {
            ApiError::bad_request(format!("序号 {index} 超出批次范围（共 {total} 条）"))
        })?;
        Ok(json!({
            "batch": batch_name,
            "revision": batch.revision,
            "index": index,
            "total": total,
            "tier": queue_tier(sample),
            "richness": context_richness(sample),
            "reason": queue_reason(sample),
            "flags": set_flags(sample),
            "flagged": legacy_targeting_risk(sample),
            "reviewed": is_reviewed(sample),
            "sample": public_sample(sample),
        }))
    })
    .await?;
    Ok(Json(body))
}

#[derive(Debug, Deserialize)]
pub(crate) struct MarkRequest {
    batch: String,
    index: usize,
    /// 客户端读到的版本号；对不上说明文件被别处改了。
    revision: String,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    response: Option<String>,
    /// 与 `review.py --mark completion=null` 等价：显式记成"判不了"。
    #[serde(default)]
    clear_completion: bool,
    #[serde(default)]
    clear_response: bool,
}

/// `POST /api/annotation/mark`
pub(crate) async fn mark(Json(body): Json<MarkRequest>) -> Result<Json<Value>, ApiError> {
    let completion = requested_label(
        body.completion.as_deref(),
        &COMPLETION_LABELS,
        "completion",
        body.clear_completion,
    )?;
    let response = requested_label(
        body.response.as_deref(),
        &RESPONSE_LABELS,
        "response",
        body.clear_response,
    )?;
    if completion.is_none() && response.is_none() {
        return Err(ApiError::bad_request(
            "至少要给 completion 或 response 一个标签；两个都判不了就勾「记 null」",
        ));
    }
    let batch_name = body.batch.clone();
    let revision = body.revision.clone();
    let index = body.index;
    let result = blocking(move || {
        let _guard = write_lock()
            .lock()
            .map_err(|_| ApiError::internal("标注写入锁已损坏"))?;
        ensure_dir()?;
        let path = batch_path(&batch_name)?;
        let mut batch = load_batch_at(&path)?;
        mark_in_batch(&mut batch, index, &revision, completion, response)
    })
    .await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExportRequest {
    batch: String,
    #[serde(default)]
    min_agreement: Option<f64>,
}

/// `POST /api/annotation/export`
pub(crate) async fn export(Json(body): Json<ExportRequest>) -> Result<Json<Value>, ApiError> {
    let min_agreement = body.min_agreement.unwrap_or(DEFAULT_MIN_AGREEMENT);
    if !(0.0..=1.0).contains(&min_agreement) {
        return Err(ApiError::bad_request("min_agreement 必须落在 0~1 之间"));
    }
    let batch_name = body.batch.clone();
    let result = blocking(move || {
        let _guard = write_lock()
            .lock()
            .map_err(|_| ApiError::internal("标注写入锁已损坏"))?;
        let path = batch_path(&batch_name)?;
        let batch = load_batch_at(&path)?;
        let (exported, human_reviewed) = export_trainable(&batch.samples, min_agreement);
        if exported.is_empty() {
            // 空训练集是个陷阱（train.py 读进去只会报更难懂的错误），不如不写。
            return Err(ApiError::bad_request(format!(
                "还没有可导出的样本：人工复核过 {human_reviewed} 条，\
                 其中 agreement ≥ {min_agreement} 的 0 条"
            )));
        }
        let name = export_file_name(&batch_name);
        let export_path = ensure_dir()?.join(&name);
        let text = render_jsonl(&exported)?;
        write_atomically(&export_path, &text).map_err(|error| write_error(&export_path, error))?;
        Ok(json!({
            "file": name,
            "exported": exported.len(),
            "human_reviewed": human_reviewed,
            "min_agreement": min_agreement,
            "bytes": text.len(),
        }))
    })
    .await?;
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DownloadQuery {
    name: String,
}

/// `GET /api/annotation/download`
pub(crate) async fn download(Query(params): Query<DownloadQuery>) -> Result<Response, ApiError> {
    let name = validate_batch_name(&params.name)?.to_owned();
    let path = config::annotation_dir_path().join(&name);
    let file_name = name.clone();
    let text =
        blocking(move || fs::read_to_string(&path).map_err(|error| load_error(&path, error)))
            .await?;
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{file_name}\""))
        .map_err(|error| ApiError::internal(format!("文件名无法作为响应头: {error}")))?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        text,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与生产代码里的 `review_status` 取值成对；生产侧只判断"是不是 reviewed"。
    const STATUS_PENDING: &str = "pending";

    fn sample(
        text: &str,
        completion: Option<&str>,
        response: Option<&str>,
        addressed: bool,
        turns: usize,
    ) -> Value {
        let turns: Vec<Value> = (0..turns)
            .map(|index| {
                json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "text": format!("turn {index}"),
                })
            })
            .collect();
        json!({
            "schema_version": SUPPORTED_SCHEMA_VERSION,
            "current_text": text,
            "context": {
                "scope": "group",
                "pending_user_fragments": [],
                "recent_turns": turns,
                "conversation_active": !turns.is_empty(),
                "bot_last_asked_question": null,
                "pending_outgoing": false,
                "pending_task": false,
                "addressed_to_agent": addressed,
                "replies_to_agent": false,
                "has_image": false,
                "has_sticker": false,
                "policy_override": "none"
            },
            "labels": { "completion": completion, "response": response },
            "label_provenance": {
                "source": "pseudo_lexical_v0",
                "annotator_count": 0,
                "agreement": 0.0
            },
            "review_status": STATUS_PENDING,
            "source_key": "opaque-key",
        })
    }

    fn temp_batch(name: &str, samples: &[Value]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kovi-annotation-{name}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        fs::write(&path, render_jsonl(samples).expect("测试批次可序列化")).expect("写入临时批次");
        path
    }

    #[test]
    fn batch_names_cannot_escape_the_annotation_directory() {
        assert_eq!(
            validate_batch_name("review-batch-20260912.jsonl").ok(),
            Some("review-batch-20260912.jsonl")
        );
        for rejected in [
            "../secret.jsonl",
            "sub/dir.jsonl",
            "sub\\dir.jsonl",
            "notes.txt",
            ".hidden.jsonl",
            "",
            "批次.jsonl",
        ] {
            assert!(
                validate_batch_name(rejected).is_err(),
                "{rejected} 不该通过白名单"
            );
        }
    }

    #[test]
    fn jsonl_round_trip_keeps_every_sample_and_rejects_bad_schema() {
        let samples = vec![sample("你好", Some("flush_now"), None, false, 1)];
        let text = render_jsonl(&samples).expect("可序列化");
        let parsed = parse_jsonl(&text).expect("可解析");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["current_text"], json!("你好"));

        let wrong_version = r#"{"schema_version":3,"current_text":"x","context":{},"labels":{}}"#;
        assert!(parse_jsonl(wrong_version).is_err());
        assert!(parse_jsonl("not json").is_err());
        assert!(parse_jsonl("").is_err());
    }

    /// `include_reviewed`（连已标一起列）与 `reviewed_only`（只看已标）是两层
    /// 语义。网页端的「已标注」页签曾经只发前者，于是那个页签列出的是整批样本：
    /// 点开一条"已标注"看到的却是没标过的样本、说明行还写着"弱标签"。
    #[test]
    fn reviewed_filters_are_not_the_same_thing() {
        let mut reviewed = sample("标过了", Some("flush_now"), Some("ignore"), false, 2);
        reviewed["review_status"] = json!(STATUS_REVIEWED);
        reviewed["label_provenance"] = json!({
            "source": REVIEWED_SOURCE, "annotator_count": 1, "agreement": 1.0
        });
        let samples = [
            sample("待标一", None, None, true, 2),
            reviewed,
            sample("待标二", None, None, false, 2),
        ];

        // 待标注页签：只看没标过的。
        let pending = queue_rows(&samples, false, false, false);
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|(_, s)| !is_reviewed(s)));

        // 已标注页签：只看标过的（这条就是修掉的那个 bug）。
        let only = queue_rows(&samples, true, true, false);
        assert_eq!(only.len(), 1);
        assert!(is_reviewed(only[0].1));

        // "连已标一起列"：全都要——4302 条那种整批口径。
        assert_eq!(queue_rows(&samples, true, false, false).len(), 3);

        // 自相矛盾的组合（不连已标、又要只看已标）结果是空集，不是"全都给"。
        assert!(queue_rows(&samples, false, true, false).is_empty());
    }

    #[test]
    fn queue_orders_gray_zone_before_everything_else() {
        let gray_addressed = sample("在吗", None, None, true, 1);
        let gray_plain = sample("随便说说", None, None, false, 2);
        let labeled = sample("讲完了", Some("flush_now"), Some("ignore"), false, 2);
        let no_context = sample("没上下文", Some("flush_now"), None, false, 0);
        let no_bot_turn = sample("只有用户说话", Some("flush_now"), None, false, 1);

        assert_eq!(queue_tier(&gray_addressed), 0);
        assert_eq!(queue_tier(&gray_plain), 1);
        assert_eq!(queue_tier(&no_context), 2);
        assert_eq!(queue_tier(&no_bot_turn), 3);
        assert_eq!(queue_tier(&labeled), 4);
        assert!(queue_reason(&gray_addressed).contains("gray-zone"));
        assert!(queue_reason(&labeled).contains("context-rich"));
    }

    /// 页头的分布必须覆盖 queue_tier 的全部取值：任何一档没落到桶里，页面上就会
    /// 出现"队列 1998 条、各档加起来 1200"这种自相矛盾的数字。
    #[test]
    fn tier_distribution_buckets_every_tier() {
        let samples = [
            sample("在吗", None, None, true, 1),
            sample("随便说说", None, None, false, 2),
            sample("没上下文", Some("flush_now"), None, false, 0),
            sample("只有用户说话", Some("flush_now"), None, false, 1),
            sample("讲完了", Some("flush_now"), Some("ignore"), false, 2),
        ];
        let counts = tier_distribution(samples.iter());
        assert_eq!(counts.len(), TIER_LABELS.len());
        assert_eq!(counts, vec![1, 1, 1, 1, 1]);
        assert_eq!(counts.iter().sum::<usize>(), samples.len());

        // 空批次不能炸，也不能编数。
        assert_eq!(tier_distribution(std::iter::empty()), vec![0; 5]);
    }

    #[test]
    fn legacy_batches_flag_unknown_at_targets_but_new_ones_do_not() {
        let legacy = sample("[at] 老大这集我看过", None, None, true, 1);
        assert!(legacy_targeting_risk(&legacy));

        // 新采集器写了 targeting：同一句话不再算"目标不可知"。
        let mut modern = legacy.clone();
        modern["context"]["targeting"] = json!("her");
        assert!(!legacy_targeting_risk(&modern));

        // 没声称在叫她 → 与 @ 判定无关。
        let other = sample("[at] 老大这集我看过", None, None, false, 1);
        assert!(!legacy_targeting_risk(&other));
    }

    #[test]
    fn marking_writes_the_same_fields_as_the_cli_tool() {
        let samples = vec![
            sample("第一句", None, None, false, 1),
            sample("第二句", None, None, false, 1),
        ];
        let path = temp_batch("mark", &samples);
        let mut batch = load_batch_at(&path).expect("加载临时批次");
        let revision = batch.revision.clone();

        let result = mark_in_batch(
            &mut batch,
            1,
            &revision,
            Some(Some("hold_for_more")),
            Some(Some("wait")),
        )
        .expect("标注成功");
        assert_eq!(result["labels"]["completion"], json!("hold_for_more"));
        assert_eq!(result["summary"]["reviewed"], json!(1));
        assert_eq!(result["summary"]["pending"], json!(1));

        let reloaded = load_batch_at(&path).expect("重新加载");
        let marked = &reloaded.samples[1];
        assert_eq!(marked["review_status"], json!(STATUS_REVIEWED));
        assert_eq!(marked["label_provenance"]["source"], json!(REVIEWED_SOURCE));
        assert_eq!(marked["label_provenance"]["annotator_count"], json!(1));
        assert_eq!(marked["label_provenance"]["agreement"], json!(1.0));
        // 没点名的头保持原样（与 `--mark` 只改被点名的键一致）。
        assert_eq!(reloaded.samples[0]["review_status"], json!(STATUS_PENDING));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn marking_rejects_a_stale_revision_and_out_of_range_index() {
        let samples = vec![sample("唯一一句", None, None, false, 1)];
        let path = temp_batch("stale", &samples);
        let mut batch = load_batch_at(&path).expect("加载临时批次");

        let stale = mark_in_batch(&mut batch, 0, "deadbeefdeadbeef", None, Some(Some("ack")));
        assert_eq!(stale.unwrap_err().status, axum::http::StatusCode::CONFLICT);

        let revision = batch.revision.clone();
        let out_of_range = mark_in_batch(&mut batch, 7, &revision, Some(Some("flush_now")), None);
        assert_eq!(
            out_of_range.unwrap_err().status,
            axum::http::StatusCode::BAD_REQUEST
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn explicit_null_records_an_undecidable_head() {
        let samples = vec![sample("判不了", Some("flush_now"), None, false, 1)];
        let path = temp_batch("null-head", &samples);
        let mut batch = load_batch_at(&path).expect("加载临时批次");
        let revision = batch.revision.clone();

        mark_in_batch(&mut batch, 0, &revision, Some(None), None).expect("记 null 成功");
        let reloaded = load_batch_at(&path).expect("重新加载");
        assert_eq!(reloaded.samples[0]["labels"]["completion"], Value::Null);
        assert_eq!(reloaded.samples[0]["review_status"], json!(STATUS_REVIEWED));

        assert!(
            requested_label(Some("flush_now"), &COMPLETION_LABELS, "completion", true).is_err()
        );
        assert!(
            requested_label(Some("nonsense"), &COMPLETION_LABELS, "completion", false).is_err()
        );
        assert_eq!(
            requested_label(None, &COMPLETION_LABELS, "completion", false).ok(),
            Some(None)
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn export_keeps_only_human_reviewed_samples_and_strips_local_fields() {
        let mut human = sample("人标的", Some("flush_now"), Some("answer"), false, 1);
        human["label_provenance"] = json!({
            "source": REVIEWED_SOURCE,
            "annotator_count": 1,
            "agreement": 1.0
        });
        human["review_status"] = json!(STATUS_REVIEWED);

        let mut low_agreement = sample("专家有分歧", Some("flush_now"), None, false, 1);
        low_agreement["label_provenance"] = json!({
            "source": REVIEWED_SOURCE,
            "annotator_count": 2,
            "agreement": 0.5
        });

        let pseudo = sample("弱标签", Some("flush_now"), None, false, 1);
        let samples = vec![human, low_agreement, pseudo];

        let (exported, human_reviewed) = export_trainable(&samples, DEFAULT_MIN_AGREEMENT);
        assert_eq!(human_reviewed, 2);
        assert_eq!(exported.len(), 1);
        assert!(exported[0].get("review_status").is_none());
        assert!(exported[0].get("source_key").is_none());
        assert_eq!(exported[0]["current_text"], json!("人标的"));

        let (all_human, _) = export_trainable(&samples, 0.4);
        assert_eq!(all_human.len(), 2);
    }

    #[test]
    fn export_names_stay_inside_the_annotation_directory() {
        assert_eq!(
            export_file_name("review-batch-20260912.jsonl"),
            "train-review-batch-20260912.jsonl"
        );
    }

    #[test]
    fn public_sample_never_hands_out_the_erasure_key() {
        let sample = sample("你好", None, None, false, 0);
        let public = public_sample(&sample);
        assert!(public.get("source_key").is_none());
        assert_eq!(public["current_text"], json!("你好"));
        assert_eq!(sample["source_key"], json!("opaque-key"));
    }

    #[test]
    fn read_only_paths_say_where_to_put_the_directory() {
        let readonly = dir_error(
            Path::new("/home/ubuntu/kovi-bot/current/turngate"),
            "创建标注目录",
            std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem),
        );
        assert_eq!(readonly.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(readonly.message.contains("创建标注目录失败"));
        assert!(readonly.message.contains("runtime/"));

        let unexpected = dir_error(
            Path::new("/tmp/whatever"),
            "写入批次",
            std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
        );
        assert_eq!(
            unexpected.status,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
