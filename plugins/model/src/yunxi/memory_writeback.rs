//! Core 回合的长期记忆写入（设计见 `docs/yunxi-memory-v2-writeback.md`）。
//!
//! Core 接管回复之后长期记忆就断流了：唯一会写 `kovi_bot_memories` 的是 V1 的
//! `MEMORY_REPOSITORY`，而 V1 如今只处理极少数回合；与此同时 Core 的召回按 context
//! **前缀**读旧表（群 `group_chat%`、私聊 `private_chat%`），于是"写"和"读"两头都停在
//! 旧世界——她的新对话既不进长期记忆，也不在召回里。
//!
//! 这一层把"她真的回了"的回合补回 v2；适配器已有的兼容投影会把它同步回旧表，
//! 所以召回与后台立刻重新有数据，且新旧两表口径一致。
//!
//! 三个刻意的取舍（详细理由见设计页）：
//!
//! - **只有投递成功才写**：`ActionPortOutcome::Delivered` 之前不算数；
//! - **只有入站过的回合才写**：入站行在 ingress 暂存，投递时取用。主动消息
//!   （autonomous tick）没有入站行，本轮不写——那是缺口，不是遗漏；
//! - **写失败绝不影响回复**：只 WARN，丢这一条。

use chrono::{DateTime, Local, Utc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use yunxi_core::{
    EventId, MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_CONTENT_CHARS, MemoryDraft, MemoryKind,
    MemoryScope, MemoryStore,
};

/// 暂存多少条"已入站、还没投递"的回合。超出按入站顺序淘汰最旧的。
///
/// 取值只需覆盖"排队 + 规划 + 发送"这段窗口里的并发事件：一条消息从入站到投递是
/// 秒级，而暂存只在 Core 的事件循环里被消费（进程重启即清空，丢的那一轮不补写）。
const MAX_PENDING_TURNS: usize = 256;

/// 写入的重要度（v2 口径 0..=100）。
///
/// legacy 投影会换算成 `ceil(v2/10)` = 4，与历史 `group_chat` 语料的平均 5.5 同量级；
/// 低于保护阈值 70，所以它会随 `memory.retention_days`（默认 30 天）自然老去。
const WRITEBACK_IMPORTANCE: u8 = 40;

/// 单条正文（入站与回复各自）的字符上限。
///
/// 定得比记忆本身的限额（4096 字符 / 8192 字节）小得多，是因为**渲染会膨胀**：
/// 私聊那条是 JSON，正文里的引号与换行都要转义，最坏情况下字节数翻几倍。1000 字符
/// 留足了余量，代价只是超长回复的记忆副本被截断（发出去的内容不受影响）。
const MAX_TEXT_CHARS: usize = 1_000;

/// 一轮待写的入站行。
struct PendingTurn {
    scope: MemoryScope,
    line: String,
    occurred_at: DateTime<Utc>,
    /// 入站顺序，用于容量淘汰。
    sequence: u64,
}

#[derive(Default)]
struct PendingTurns {
    entries: HashMap<EventId, PendingTurn>,
    sequence: u64,
}

/// Core 回合的入站暂存 + 落库。
pub(crate) struct MemoryWriteback {
    pending: Mutex<PendingTurns>,
}

impl MemoryWriteback {
    fn new() -> Self {
        Self {
            pending: Mutex::new(PendingTurns::default()),
        }
    }

    /// ingress 侧：把这一轮的入站行挂到事件上。
    ///
    /// 事件进 Core 之前调用；只有真的投递成功了才会被取用，所以"她没回"的回合
    /// 不会留下任何记忆。
    pub(crate) fn stash_inbound(
        &self,
        event_id: EventId,
        scope: MemoryScope,
        line: String,
        occurred_at: DateTime<Utc>,
    ) {
        let Ok(mut pending) = self.pending.lock() else {
            // 锁中毒只可能是别的线程 panic 时留下的；记忆是尽力而为，不能反过来
            // 拖垮回复链路。
            kovi::log::warn!("Yunxi memory writeback stash is poisoned; skipping inbound line");
            return;
        };
        if pending.entries.len() >= MAX_PENDING_TURNS
            && let Some(oldest) = pending
                .entries
                .iter()
                .min_by_key(|(_, turn)| turn.sequence)
                .map(|(event_id, _)| *event_id)
        {
            pending.entries.remove(&oldest);
        }
        pending.sequence = pending.sequence.wrapping_add(1);
        let sequence = pending.sequence;
        pending.entries.insert(
            event_id,
            PendingTurn {
                scope,
                line,
                occurred_at,
                sequence,
            },
        );
    }

    /// 投递侧：这一轮真的发出去了，把「对方说的 + 她回的」写进长期记忆。
    ///
    /// 没有暂存（主动消息、或进程重启后丢了暂存）就是空操作。
    pub(crate) async fn record_delivered_turn(&self, event_id: EventId, reply: &str) {
        if !crate::config::get().memory().core_writeback_enabled() {
            return;
        }
        let turn = {
            let Ok(mut pending) = self.pending.lock() else {
                kovi::log::warn!("Yunxi memory writeback stash is poisoned; skipping turn");
                return;
            };
            pending.entries.remove(&event_id)
        };
        let Some(turn) = turn else {
            return;
        };
        let Some(store) = super::memory_store() else {
            kovi::log::warn!("Yunxi memory writeback skipped: memory store is unavailable");
            return;
        };
        let reply = reply_line(reply);
        let mut lines: Vec<(&str, DateTime<Utc>)> = vec![(turn.line.as_str(), turn.occurred_at)];
        if !reply.is_empty() {
            lines.push((reply.as_str(), Utc::now()));
        }
        for (line, occurred_at) in lines {
            match write_memory(store.as_ref(), turn.scope, line, occurred_at).await {
                Ok(()) => {}
                Err(error) => kovi::log::warn!(
                    "Yunxi memory writeback failed: event_id={event_id} scope={:?} error={error}",
                    turn.scope
                ),
            }
        }
    }

    /// 暂存条数（诊断与测试用）。
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .map(|pending| pending.entries.len())
            .unwrap_or_default()
    }
}

/// 进程级实例，与 `memory_store()` / `relation_store()` 同一套注册方式。
static WRITEBACK: OnceLock<Arc<MemoryWriteback>> = OnceLock::new();

/// 幂等安装（重复调用返回同一实例）。
pub(crate) fn install() -> Arc<MemoryWriteback> {
    Arc::clone(WRITEBACK.get_or_init(|| Arc::new(MemoryWriteback::new())))
}

pub(crate) fn writeback() -> Option<Arc<MemoryWriteback>> {
    WRITEBACK.get().cloned()
}

/// 群聊入站行，与 V1 的 `group_chat` 语料同形：
/// `[HH:MM:SS] 群成员称呼="<称呼>": <正文>`（时间按本机时区渲染，与 V1 一致）。
pub(crate) fn group_inbound_line(sender_label: &str, text: &str, at: DateTime<Utc>) -> String {
    format!(
        "[{}] 群成员称呼={}: {}",
        at.with_timezone(&Local).format("%H:%M:%S"),
        serde_json::Value::String(sender_label.trim().to_string()),
        bounded_text(text)
    )
}

/// 私聊入站行，与 V1 的 `private_chat` 语料同形（同一份 JSON 形状）。
pub(crate) fn private_inbound_line(nickname: &str, text: &str) -> String {
    serde_json::json!({
        "消息类型": "私聊",
        "发送者": { "QQ昵称": nickname.trim() },
        "正文": bounded_text(text),
    })
    .to_string()
}

/// 她说出去的话。与 V1 一致：一条回复（多段气泡由调用方用换行连接）。
/// 空回复返回空串，调用方据此决定不写这一条。
pub(crate) fn reply_line(reply: &str) -> String {
    let bounded = bounded_text(reply);
    if bounded.is_empty() {
        return String::new();
    }
    format!("芸汐: {bounded}")
}

/// 收口正文长度。按字符截断，所以不会切坏多字节字符。
fn bounded_text(value: &str) -> String {
    value.trim().chars().take(MAX_TEXT_CHARS).collect()
}

/// 说话人的显示名：**群名片优先，空值退回昵称**，两者都没有才兜底。
///
/// 注意"空字符串"不是"没有"：QQ 上报的名片常常是 `Some("")`（没设群名片），
/// 直接 `card.or(nickname)` 会挑中那个空串，最后记成"未设置昵称"——线上第一轮
/// 写入就踩了这个坑。V1 的 `GroupSenderIdentity` 是先各自归一化再挑的。
pub(crate) fn sender_label(card: Option<&str>, nickname: Option<&str>) -> String {
    let picked = [card, nickname]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty());
    normalized_sender_label(picked)
}

/// 收口说话人显示名：压掉多余空白、最多 80 字、空值兜底成"未设置昵称"。
/// 名字进了记忆正文，所以要显式收口。
pub(crate) fn normalized_sender_label(value: Option<&str>) -> String {
    let normalized = value
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect::<String>();
    if normalized.is_empty() {
        "未设置昵称".to_string()
    } else {
        normalized
    }
}

async fn write_memory(
    store: &dyn MemoryStore,
    scope: MemoryScope,
    content: &str,
    occurred_at: DateTime<Utc>,
) -> Result<(), yunxi_core::MemoryStoreError> {
    // 渲染后的正文（含 JSON 结构）也必须落在记忆的限额内，否则 `MemoryDraft::new`
    // 会整条拒绝——那样丢的是一条记忆，而不是被截断的正文。这里宁可显式跳过并留痕。
    if content.chars().count() > MAX_MEMORY_CONTENT_CHARS
        || content.len() > MAX_MEMORY_CONTENT_BYTES
    {
        return Err(yunxi_core::MemoryStoreError::InvalidRequest {
            reason: format!(
                "rendered memory line exceeds limits: chars={} bytes={}",
                content.chars().count(),
                content.len()
            ),
        });
    }
    let draft = MemoryDraft::new(scope, MemoryKind::Conversation, content, occurred_at)
        .and_then(|draft| draft.with_importance(WRITEBACK_IMPORTANCE))
        .map_err(|error| yunxi_core::MemoryStoreError::InvalidRequest {
            reason: error.to_string(),
        })?;
    store.remember(&draft).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 2026-09-13 18:54:08 +08:00。
    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, 9, 13, hour, minute, 0)
            .single()
            .expect("有效时间")
            .with_timezone(&Utc)
    }

    #[test]
    fn group_lines_match_the_legacy_corpus_shape() {
        let line = group_inbound_line("月月（叉腰！）", " 竟然知道村八分嘛 ", at(18, 54));
        assert_eq!(
            line,
            "[18:54:00] 群成员称呼=\"月月（叉腰！）\": 竟然知道村八分嘛"
        );
    }

    #[test]
    fn private_lines_keep_the_json_shape_and_strip_padding() {
        let line = private_inbound_line("云深不知处", " speak English ");
        let value: serde_json::Value = serde_json::from_str(&line).expect("应是 JSON");
        assert_eq!(value["消息类型"], "私聊");
        assert_eq!(value["发送者"]["QQ昵称"], "云深不知处");
        assert_eq!(value["正文"], "speak English");
    }

    #[test]
    fn reply_lines_are_prefixed_with_her_name_and_empty_stays_empty() {
        assert_eq!(reply_line(" 怎么啦，七七？ "), "芸汐: 怎么啦，七七？");
        assert!(reply_line("   ").is_empty());
    }

    /// 线上第一轮写入踩过的坑：名片是空串（没设群名片）时必须退回昵称，
    /// 否则记忆里会记成"未设置昵称"，而那正是 V1 语料分辨说话人的字段。
    #[test]
    fn empty_group_card_falls_back_to_nickname() {
        assert_eq!(sender_label(Some(""), Some("不忻")), "不忻");
        assert_eq!(sender_label(Some("   "), Some("不忻")), "不忻");
        assert_eq!(sender_label(Some(" 七七铺 "), Some("不忻")), "七七铺");
        assert_eq!(sender_label(None, Some("不忻")), "不忻");
        assert_eq!(sender_label(Some(""), Some("")), "未设置昵称");
        assert_eq!(sender_label(None, None), "未设置昵称");
    }

    /// 截断只能截正文，不能把 JSON 结构切坏——切坏了这条记忆会连读都读不出来。
    #[test]
    fn over_long_text_is_truncated_before_rendering() {
        let long = "芸".repeat(MAX_TEXT_CHARS + 500);
        let group = group_inbound_line("某人", &long, at(9, 0));
        assert_eq!(group.matches('芸').count(), MAX_TEXT_CHARS);

        let private = private_inbound_line("某人", &long);
        let value: serde_json::Value = serde_json::from_str(&private).expect("截断后仍是合法 JSON");
        assert_eq!(
            value["正文"].as_str().expect("正文").chars().count(),
            MAX_TEXT_CHARS
        );

        // 最坏情况（正文全是需要转义的引号与换行）也要留在记忆的字节限额内。
        let hostile = "\"\\\n".repeat(MAX_TEXT_CHARS);
        for line in [
            group_inbound_line("某人", &hostile, at(9, 0)),
            private_inbound_line("某人", &hostile),
            reply_line(&hostile),
        ] {
            assert!(
                line.chars().count() <= MAX_MEMORY_CONTENT_CHARS,
                "chars={}",
                line.chars().count()
            );
            assert!(
                line.len() <= MAX_MEMORY_CONTENT_BYTES,
                "bytes={}",
                line.len()
            );
        }
    }

    #[test]
    fn stash_is_bounded_and_evicts_the_oldest() {
        let writeback = MemoryWriteback::new();
        let oldest = EventId::new();
        writeback.stash_inbound(oldest, MemoryScope::Global, "old".to_string(), Utc::now());
        for _ in 0..MAX_PENDING_TURNS {
            writeback.stash_inbound(
                EventId::new(),
                MemoryScope::Global,
                "new".to_string(),
                Utc::now(),
            );
        }
        assert_eq!(writeback.pending_len(), MAX_PENDING_TURNS);
        let pending = writeback.pending.lock().expect("锁可用");
        assert!(
            !pending.entries.contains_key(&oldest),
            "最早的一条应该先被淘汰"
        );
    }
}
