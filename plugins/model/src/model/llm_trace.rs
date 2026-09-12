//! 模型调用的可观测层：这次调用**是干什么的**、发了什么、回了什么。
//!
//! 为什么需要它：这条会话里连续查出四条"管道在，但静默地不干活"的问题——
//! belief 协议没进提示词、深度反思从未触发、群聊缺一整类命令分支、立场闸门槛太高。
//! 每一次的排查手段都是"读日志猜"，因为日志里只有
//! `Model gateway attempt: ... response_chars=2` 这种**匿名**记录：知道有人调了模型，
//! 不知道是谁、为了什么、发了什么。
//!
//! 这一层补三件事：
//! 1. **用途标签**（[`with_purpose`]）：每次调用属于哪条管线，用 task-local 传播，
//!    调用方不必层层透传参数；
//! 2. **有界轨迹**（[`record`]）：最近 N 次调用的提示词、回复、耗时、工具调用，
//!    供 `#llm-trace` 事后复盘（今天要是有这个，那个 `response_chars=2` 就不用猜了）；
//! 3. **死管道检测**（[`report`]）：把"本该会调的用途"和"真的调用过的用途"对起来，
//!    直接列出**从未调用**的那些。这一条是冲着上面那四个问题去的。
//!
//! 成本：一次调用一条记录，环形缓冲固定上限，不落盘、不进数据库。

use chrono::{DateTime, Local};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// 轨迹里保留的调用条数。
const TRACE_CAPACITY: usize = 64;
/// 单条记录里保留的提示词上限（字符）。够复盘，又不至于把内存吃穿。
const PROMPT_MAX_CHARS: usize = 8_000;
/// 单条记录里保留的回复上限（字符）。
const RESPONSE_MAX_CHARS: usize = 4_000;
/// 列表视图里预览的字符数。
const PREVIEW_CHARS: usize = 120;

kovi::tokio::task_local! {
    /// 当前异步任务正在做的那件事的名字，供模型调用打标签。
    static LLM_PURPOSE: &'static str;
}

/// 给一段异步流程打上用途标签；期间发生的模型调用都归到这个名字下。
///
/// 用 task-local 而不是加参数：模型调用散在几十处，逐个透传既容易漏、又会把
/// 无关的签名全改一遍。标签只需要在**顶层入口**设一次。
pub(crate) async fn with_purpose<F, T>(purpose: &'static str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    LLM_PURPOSE.scope(purpose, future).await
}

/// 当前用途标签；没设过就记成 `unlabeled`——它本身就是"这里漏了打标签"的信号。
///
/// 也供网关日志使用：日志里带上用途，`journalctl | grep purpose=stance_formation`
/// 就能回答"这条管线到底跑没跑"，不必依赖 `#llm-trace` 命令。
pub(crate) fn current_purpose() -> &'static str {
    LLM_PURPOSE
        .try_with(|purpose| *purpose)
        .unwrap_or(UNLABELED)
}

/// 没打标签的调用。报里单列，提醒补 [`with_purpose`]。
const UNLABELED: &str = "unlabeled";

/// 一条模型调用记录。
struct LlmTraceEntry {
    at: DateTime<Local>,
    purpose: &'static str,
    model: String,
    elapsed_ms: u128,
    attempt: u32,
    max_attempts: u32,
    outcome: &'static str,
    prompt: String,
    response: String,
    finish_reason: Option<String>,
    tool_calls: Vec<String>,
}

/// 每个用途的累计计数。用来看"这条管线到底跑没跑过"。
#[derive(Default, Clone)]
struct PurposeStats {
    calls: u64,
    failures: u64,
    last_at: Option<DateTime<Local>>,
}

static TRACE: LazyLock<Mutex<VecDeque<LlmTraceEntry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(TRACE_CAPACITY)));
static PURPOSES: LazyLock<Mutex<HashMap<&'static str, PurposeStats>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 本该会调模型的管线。报告里会把它们和"真的调用过的"对起来，
/// 列出**从未调用**的——那正是"管道没通电"的直接证据。
///
/// 加新管线时把它登记在这里：登记了没跑，报告会说出来；跑了没登记，
/// 会以 `unlabeled` 或未声明用途的形式冒出来。
/// 只登记**已经接上轨迹**的管线：列了却没接，报告会误报"从未调用"。
/// 新管线接上 [`with_purpose`] 时同步登记在这里。
///
/// 还没接的两条（如实记着，别假装覆盖了）：
/// - 看图：`vision.rs` 有自己的 HTTP 客户端，不经 [`round_trip_model_request`]；
/// - 会话摘要：摘要文本是外部传进 `update_conversation_summary` 的，生成点未定位。
const EXPECTED_PURPOSES: &[(&str, &str)] = &[
    ("core_reply", "Core 回复/规划（文本聊天的正式路径）"),
    ("private_reply", "私聊回复（旧链路）"),
    ("group_reply", "群聊回复"),
    ("phone_reply", "电话回复"),
    ("stance_formation", "立场形成（反思里到期问一次）"),
    ("stance_dedup", "立场查重（读两边全文再拍板）"),
];

/// 记录一次成功的模型调用。
pub(crate) fn record_success(
    request_body: &serde_json::Value,
    payload: &super::utils::ModelPayload,
    elapsed: Duration,
    attempt: u32,
    max_attempts: u32,
) {
    let tool_calls = payload
        .tool_calls
        .iter()
        .map(|call| call.name.clone())
        .collect();
    record(
        request_body,
        elapsed,
        attempt,
        max_attempts,
        "ok",
        payload.content.clone(),
        payload.finish_reason.clone(),
        tool_calls,
    );
}

/// 记录一次失败的模型调用（最后一次尝试才记，避免重试把轨迹刷满）。
pub(crate) fn record_failure(
    request_body: &serde_json::Value,
    elapsed: Duration,
    attempt: u32,
    max_attempts: u32,
    error: &str,
) {
    record(
        request_body,
        elapsed,
        attempt,
        max_attempts,
        "failed",
        error.to_string(),
        None,
        Vec::new(),
    );
}

#[allow(clippy::too_many_arguments)]
fn record(
    request_body: &serde_json::Value,
    elapsed: Duration,
    attempt: u32,
    max_attempts: u32,
    outcome: &'static str,
    response: String,
    finish_reason: Option<String>,
    tool_calls: Vec<String>,
) {
    let purpose = current_purpose();
    let model = request_body
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("(未知模型)")
        .to_string();
    let prompt = render_prompt(request_body);
    let at = Local::now();

    {
        let mut purposes = PURPOSES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stats = purposes.entry(purpose).or_default();
        stats.calls += 1;
        if outcome != "ok" {
            stats.failures += 1;
        }
        stats.last_at = Some(at);
    }

    let mut trace = TRACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if trace.len() == TRACE_CAPACITY {
        trace.pop_front();
    }
    trace.push_back(LlmTraceEntry {
        at,
        purpose,
        model,
        elapsed_ms: elapsed.as_millis(),
        attempt,
        max_attempts,
        outcome,
        prompt: truncate_chars(&prompt, PROMPT_MAX_CHARS),
        response: truncate_chars(&response, RESPONSE_MAX_CHARS),
        finish_reason,
        tool_calls,
    });
}

/// 把请求体里的消息渲染成可读文本。工具声明也带上——很多时候问题就出在
/// "工具给了没有、给了几个"。
fn render_prompt(request_body: &serde_json::Value) -> String {
    let mut rendered = String::new();
    if let Some(tools) = request_body.get("tools").and_then(|value| value.as_array()) {
        rendered.push_str(&format!("【工具声明 {} 个】\n", tools.len()));
    }
    if let Some(messages) = request_body
        .get("messages")
        .and_then(|value| value.as_array())
    {
        for message in messages {
            let role = message
                .get("role")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let content = message
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            rendered.push_str(&format!("【{role}】{content}\n"));
        }
    }
    rendered
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut truncated: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        truncated.push_str("\n…（已截断）");
    }
    truncated
}

fn preview(value: &str) -> String {
    let compact = value.replace(['\r', '\n'], " ");
    let compact = compact.trim();
    let mut short: String = compact.chars().take(PREVIEW_CHARS).collect();
    if compact.chars().count() > PREVIEW_CHARS {
        short.push('…');
    }
    short
}

/// `#llm-trace` 的内容：先给"哪条管线跑过、哪条从未跑过"，再列最近的调用。
pub(crate) fn report(limit: usize, detail: Option<usize>) -> String {
    let purposes = PURPOSES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let trace = TRACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(index) = detail {
        return match trace.iter().rev().nth(index.saturating_sub(1)) {
            Some(entry) => {
                let tools = if entry.tool_calls.is_empty() {
                    "无".to_string()
                } else {
                    entry.tool_calls.join("、")
                };
                format!(
                    "第 {index} 近的一次调用\n\
                     时间 {}｜用途 {}｜模型 {}\n\
                     耗时 {} ms｜第 {}/{} 次尝试｜{}｜结束原因 {}｜工具 {}\n\
                     ———— 发的 ————\n{}\n———— 回的 ————\n{}",
                    entry.at.format("%H:%M:%S"),
                    entry.purpose,
                    entry.model,
                    entry.elapsed_ms,
                    entry.attempt,
                    entry.max_attempts,
                    entry.outcome,
                    entry.finish_reason.as_deref().unwrap_or("—"),
                    tools,
                    entry.prompt,
                    entry.response,
                )
            }
            None => format!("没有第 {index} 近的调用（轨迹里只有 {} 条）", trace.len()),
        };
    }

    let mut report = String::from("模型调用轨迹\n【管线运行情况】\n");
    let mut never: Vec<&str> = Vec::new();
    for (name, description) in EXPECTED_PURPOSES {
        match purposes.get(name) {
            Some(stats) => {
                let last = stats
                    .last_at
                    .map_or_else(|| "?".to_string(), |at| at.format("%H:%M:%S").to_string());
                report.push_str(&format!(
                    "  {description}：{} 次调用（失败 {}），最近 {last}\n",
                    stats.calls, stats.failures
                ));
            }
            None => {
                never.push(description);
                report.push_str(&format!("  {description}：**从未调用**\n"));
            }
        }
    }
    if let Some(stats) = purposes.get(UNLABELED) {
        report.push_str(&format!(
            "  未打标签的调用：{} 次——说明有管线漏了 with_purpose\n",
            stats.calls
        ));
    }
    for (name, stats) in &purposes {
        if !EXPECTED_PURPOSES.iter().any(|(known, _)| known == name) && *name != UNLABELED {
            report.push_str(&format!("  （未登记用途）{name}：{} 次\n", stats.calls));
        }
    }
    if !never.is_empty() {
        report.push_str(&format!(
            "\n⚠ 有 {} 条管线自启动以来一次都没调用过——管道可能没通电，\n\
             查它的触发条件，而不是查它的实现。\n",
            never.len()
        ));
    }

    report.push_str(&format!("\n【最近 {} 次调用】\n", limit.min(trace.len())));
    if trace.is_empty() {
        report.push_str("  （还没有任何调用）\n");
        return report;
    }
    for (index, entry) in trace.iter().rev().take(limit).enumerate() {
        let tools = if entry.tool_calls.is_empty() {
            String::new()
        } else {
            format!("｜工具 {}", entry.tool_calls.join("、"))
        };
        report.push_str(&format!(
            "  {}. {} {} {}ms {}c{}｜{}\n     → {}\n",
            index + 1,
            entry.at.format("%H:%M:%S"),
            entry.purpose,
            entry.elapsed_ms,
            entry.prompt.chars().count(),
            if entry.outcome == "ok" { "" } else { " 失败" },
            tools,
            preview(&entry.response),
        ));
    }
    report.push_str("\n用 #llm-trace 详情 <序号> 看某一次的完整提示词与回复。");
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(model: &str, messages: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "model": model,
            "messages": messages
                .iter()
                .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn prompt_rendering_includes_roles_content_and_tool_count() {
        let mut request = body("test-model", &[("system", "你是芸汐"), ("user", "在吗")]);
        request["tools"] = serde_json::json!([{"type": "function"}]);

        let rendered = render_prompt(&request);
        assert!(rendered.contains("【工具声明 1 个】"));
        assert!(rendered.contains("【system】你是芸汐"));
        assert!(rendered.contains("【user】在吗"));
    }

    #[test]
    fn trace_records_purpose_and_survives_the_capacity_bound() {
        // 记录一条并确认用途计数与内容都落到位。
        let request = body("test-model", &[("user", "立场测试")]);
        record(
            &request,
            Duration::from_millis(12),
            1,
            1,
            "ok",
            "[]".into(),
            Some("stop".into()),
            vec![],
        );
        let purposes = PURPOSES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let stats = purposes.get(UNLABELED).expect("未打标签的调用应被记下");
        assert!(stats.calls >= 1);

        // 环形缓冲不会无限增长。
        for _ in 0..(TRACE_CAPACITY + 5) {
            record(
                &request,
                Duration::from_millis(1),
                1,
                1,
                "ok",
                "x".into(),
                None,
                vec![],
            );
        }
        let trace = TRACE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(trace.len(), TRACE_CAPACITY);
    }

    #[test]
    fn report_flags_pipelines_that_never_ran() {
        // 报告必须把"从未调用"的管线点出来——这正是这条会话里反复吃的亏。
        let rendered = report(5, None);
        assert!(rendered.contains("【管线运行情况】"));
        assert!(rendered.contains("从未调用"), "死管道必须以显式文案出现");
        assert!(rendered.contains("查它的触发条件，而不是查它的实现"));
        // 详情视图越界时给一句人话，而不是空字符串。
        assert!(report(0, Some(9_999)).contains("没有第"));
    }
}
