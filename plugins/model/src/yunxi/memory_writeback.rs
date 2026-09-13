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
//!
//! 2026-09-14 补的那条（`silent_turn_writeback_enabled`）：上面第一条只管"她回了"
//! 的回合，于是**读过但判沉默**的回合连痕迹都没有——短期上下文里她明明读过
//! （`conversation.recent_events`），一小时后却像从没发生过；而没被抽样进 Core 的
//! 噪声反倒被 Host 的观察流留了档。现在沉默回合也落一条**入站行**（没有回复行），
//! 重要度低一档，并吃每会话每小时的护栏。

use chrono::{DateTime, Local, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use yunxi_core::{
    EventId, IdentityStore, MAX_MEMORY_CONTENT_BYTES, MAX_MEMORY_CONTENT_CHARS, MemoryDraft,
    MemoryKind, MemoryScope, MemoryStore,
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

/// 沉默回合落档时打的标签，方便日后按标签统计或清理这一批。
pub(crate) const SILENT_TURN_TAG: &str = "silent_turn";

/// 沉默回合护栏的统计窗口：一小时。
const SILENT_TURN_WINDOW_SECS: u64 = 3_600;

/// 护栏状态最多记多少个会话；超过就整体清空（宁可多放行一个窗口，也不让
/// 进程内状态无界增长）。
const MAX_SILENT_TURN_SCOPES: usize = 1_024;

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
    /// 这一轮是不是**归 Core** 的（`visible_reply_allowed`）。
    ///
    /// 观察副本（Host 路为了让 Core 也看到而注入的那一份）是 false：它偶尔也会
    /// 真的说话（Mind 的 AgendaResume 会越过 Silent 基线），那次的回复必须写；
    /// 但它什么都没说时，这条消息的记忆归 Host 的观察流，Core 再写一份就是同一句
    /// 话落两份。所以区别只在 [`MemoryWriteback::record_silent_turn`] 里生效。
    core_owned: bool,
}

#[derive(Default)]
struct PendingTurns {
    entries: HashMap<EventId, PendingTurn>,
    sequence: u64,
}

/// Core 回合的入站暂存 + 落库。
pub(crate) struct MemoryWriteback {
    pending: Mutex<PendingTurns>,
    /// 沉默回合落档的每小时名额（按会话/人计），窗口是滑动的。
    silent_turns: Mutex<HashMap<MemoryScope, VecDeque<Instant>>>,
}

impl MemoryWriteback {
    fn new() -> Self {
        Self {
            pending: Mutex::new(PendingTurns::default()),
            silent_turns: Mutex::new(HashMap::new()),
        }
    }

    /// ingress 侧：把这一轮的入站行挂到事件上。
    ///
    /// 事件进 Core 之前调用。取用有两条互斥的收尾路径：投递成功 →
    /// [`Self::record_delivered_turn`]（对方说的 + 她回的），什么都没发出去 →
    /// [`Self::record_silent_turn`]（只留"她读到过"，且只对 `core_owned` 的回合）。
    pub(crate) fn stash_inbound(
        &self,
        event_id: EventId,
        scope: MemoryScope,
        line: String,
        occurred_at: DateTime<Utc>,
        core_owned: bool,
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
                core_owned,
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
        let Some(turn) = self.take_pending(event_id) else {
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
            match write_memory(
                store.as_ref(),
                turn.scope,
                line,
                MemoryKind::Conversation,
                WRITEBACK_IMPORTANCE,
                occurred_at,
                &[],
            )
            .await
            {
                Ok(()) => {}
                Err(error) => kovi::log::warn!(
                    "Yunxi memory writeback failed: event_id={event_id} scope={:?} error={error}",
                    turn.scope
                ),
            }
        }
    }

    /// 回合结束了，但什么都没发出去：把入站行按"她读到过"落档。
    ///
    /// 与 [`Self::record_delivered_turn`] 的差别只有三条：没有回复行（她确实没说
    /// 过话，不能写进"她参与过的对话"）、重要度低一档（不与真实对话抢召回）、
    /// 吃每会话每小时的护栏（热闹的群不该把召回池冲淡）。没有暂存就是空操作，
    /// 所以主动消息天然不受影响。
    ///
    /// 观察副本（`core_owned=false`）直接跳过：那条消息 Host 的观察流已经记过，
    /// Core 再写一份就是同一句话落两份。
    pub(crate) async fn record_silent_turn(&self, event_id: EventId) {
        // 先取再判开关：入口已经关闭时暂存里不会有新条目，而已有的条目不该
        // 因为开关被关掉就永远留在暂存里等淘汰。
        let Some(turn) = self.take_pending(event_id) else {
            return;
        };
        if !turn.core_owned {
            return;
        }
        // `config::get()` 返回临时 Arc，先绑成局部变量再借用它的一节。
        let config = crate::config::get();
        let memory = config.memory();
        if !memory.core_writeback_enabled() || !memory.silent_turn_writeback_enabled() {
            return;
        }
        let limit = memory.silent_turn_hourly_limit();
        let Some(store) = super::memory_store() else {
            kovi::log::warn!("Yunxi silent turn memory skipped: memory store is unavailable");
            return;
        };
        // 名额在真的准备写之前才申领：存储不可用不该白白吃掉这一小时的额度。
        if !self.claim_silent_turn_slot(turn.scope, limit) {
            kovi::log::info!(
                "Yunxi silent turn memory skipped: event_id={event_id} scope={:?} reason=hourly_limit limit={limit}",
                turn.scope
            );
            return;
        }
        let importance = memory.silent_turn_importance();
        match write_memory(
            store.as_ref(),
            turn.scope,
            &turn.line,
            MemoryKind::Conversation,
            importance,
            turn.occurred_at,
            &[SILENT_TURN_TAG],
        )
        .await
        {
            Ok(()) => kovi::log::info!(
                "Yunxi silent turn memory recorded: event_id={event_id} scope={:?} importance={importance}",
                turn.scope
            ),
            Err(error) => kovi::log::warn!(
                "Yunxi silent turn memory failed: event_id={event_id} scope={:?} error={error}",
                turn.scope
            ),
        }
    }

    /// 取走这一轮的入站暂存（投递与沉默两条收尾路径共用）。
    fn take_pending(&self, event_id: EventId) -> Option<PendingTurn> {
        let Ok(mut pending) = self.pending.lock() else {
            kovi::log::warn!("Yunxi memory writeback stash is poisoned; skipping turn");
            return None;
        };
        pending.entries.remove(&event_id)
    }

    /// 申领一条"沉默回合落档"名额：同一会话每小时最多 `limit` 条。
    ///
    /// 检查与记账在同一把锁里完成，避免并发下超发。锁中毒时返回 `false`：记忆是
    /// 尽力而为，这条路径不该把回合拖垮。
    fn claim_silent_turn_slot(&self, scope: MemoryScope, limit: usize) -> bool {
        let Ok(mut grants) = self.silent_turns.lock() else {
            return false;
        };
        if grants.len() > MAX_SILENT_TURN_SCOPES {
            grants.clear();
        }
        let now = Instant::now();
        let window = Duration::from_secs(SILENT_TURN_WINDOW_SECS);
        let seen = grants.entry(scope).or_default();
        while seen
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= window)
        {
            seen.pop_front();
        }
        if seen.len() >= limit {
            return false;
        }
        seen.push_back(now);
        true
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
    kind: MemoryKind,
    importance: u8,
    occurred_at: DateTime<Utc>,
    tags: &[&str],
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
    let draft = MemoryDraft::new(scope, kind, content, occurred_at)
        .and_then(|draft| draft.with_importance(importance))
        .and_then(|draft| draft.with_tags(tags.iter().copied()))
        .map_err(|error| yunxi_core::MemoryStoreError::InvalidRequest {
            reason: error.to_string(),
        })?;
    store.remember(&draft).await?;
    Ok(())
}

// ───────────────────────────── 模型自记记忆（memory.remember 工具） ─────────────────────────────
//
// 上面那套是"机械流水"：每个投递成功的回合照抄一遍。这一套是"她自己的判断"：把
// 值得长期留存的东西（对方的偏好、身份细节、约定）写成事实。两者都写 v2，都由适配器
// 同步回旧表，所以召回路子完全一样。
//
// 为什么是工具而不是 Core 的 `StateUpdateProposal::Memory`：工具路线不改 Core 契约，
// 白拿现成的权限闸门（写工具自动被排除在"工具结果回合只能调只读工具"之外），而且
// 模型能收到"已记住"的回执、当场知道成没成。取舍见 docs/yunxi-memory-v2-writeback.md。

/// 模型自记记忆的默认重要度。
const MODEL_MEMORY_DEFAULT_IMPORTANCE: u8 = 50;

/// 模型要求记住的一条记忆（已解析、已收口）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModelMemoryRequest {
    pub(crate) content: String,
    pub(crate) kind: MemoryKind,
    pub(crate) importance: u8,
}

/// 解析 `memory.remember` 的参数。
///
/// 模型给的值一律当作不可信输入：正文必填且收口、kind 只认白名单（不认识就报错，
/// 而不是猜一个——猜错了会把事件记成事实，而且模型收不到纠正信号）、importance
/// 缺省 50、越界夹到 0..=100。
pub(crate) fn parse_model_memory_request(
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<ModelMemoryRequest, String> {
    let content = arguments
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(bounded_text)
        .filter(|content| !content.is_empty())
        .ok_or_else(|| "content 不能为空".to_string())?;
    let kind = match arguments.get("kind").and_then(serde_json::Value::as_str) {
        None => MemoryKind::Fact,
        Some("fact") => MemoryKind::Fact,
        Some("preference") => MemoryKind::Preference,
        Some("event") => MemoryKind::Event,
        Some(other) => {
            return Err(format!(
                "kind 只支持 fact / preference / event，收到 {other}"
            ));
        }
    };
    let importance = match arguments.get("importance") {
        None | Some(serde_json::Value::Null) => MODEL_MEMORY_DEFAULT_IMPORTANCE,
        Some(value) => {
            let Some(number) = value.as_i64() else {
                return Err("importance 必须是整数".to_string());
            };
            u8::try_from(number.clamp(0, 100)).unwrap_or(MODEL_MEMORY_DEFAULT_IMPORTANCE)
        }
    };
    Ok(ModelMemoryRequest {
        content,
        kind,
        importance,
    })
}

/// 执行 `memory.remember`：把模型写下的内容落到**当前会话**对应的作用域。
///
/// 作用域只能由宿主从 `destination` 推出来，模型无法指定——否则它可以往任意私聊对象
/// 或群里写记忆。返回值是给模型看的回执（成功或失败原因都会进对话）。
pub(crate) async fn remember_model_memory(
    destination: crate::model::MessageDestination,
    request: ModelMemoryRequest,
) -> Result<String, String> {
    let scope = model_memory_scope(destination).await?;
    let Some(store) = super::memory_store() else {
        return Err("记忆存储不可用".to_string());
    };
    write_memory(
        store.as_ref(),
        scope,
        &request.content,
        request.kind,
        request.importance,
        Utc::now(),
        &[],
    )
    .await
    .map_err(|error| format!("写入失败: {error}"))?;
    // 留痕：她能自己记什么、记成什么重要度，是这类"模型自主写入"最需要可审计的部分。
    println!(
        "[INFO] 模型自记记忆 scope={:?} kind={:?} importance={}: {}",
        scope, request.kind, request.importance, request.content
    );
    Ok(format!(
        "已记住（{}，重要度 {}）：{}",
        model_kind_label(request.kind),
        request.importance,
        request.content
    ))
}

async fn model_memory_scope(
    destination: crate::model::MessageDestination,
) -> Result<MemoryScope, String> {
    let identities = super::identity_store().ok_or_else(|| "身份存储不可用".to_string())?;
    match destination {
        crate::model::MessageDestination::Private(user_id) => {
            let external = super::qq::person(user_id).map_err(|error| error.to_string())?;
            let person_id = identities
                .resolve_external_identity(&external)
                .await
                .map_err(|error| error.to_string())?;
            Ok(MemoryScope::Person(person_id))
        }
        crate::model::MessageDestination::Group(group_id) => {
            let external = super::qq::group(group_id).map_err(|error| error.to_string())?;
            let conversation_id = identities
                .resolve_external_conversation(&external)
                .await
                .map_err(|error| error.to_string())?;
            Ok(MemoryScope::Conversation(conversation_id))
        }
    }
}

const fn model_kind_label(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fact => "事实",
        MemoryKind::Preference => "偏好",
        MemoryKind::Event => "事件",
        MemoryKind::Conversation => "对话",
        MemoryKind::Profile => "档案",
        MemoryKind::Emotion => "情绪",
    }
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
        writeback.stash_inbound(
            oldest,
            MemoryScope::Global,
            "old".to_string(),
            Utc::now(),
            true,
        );
        for _ in 0..MAX_PENDING_TURNS {
            writeback.stash_inbound(
                EventId::new(),
                MemoryScope::Global,
                "new".to_string(),
                Utc::now(),
                true,
            );
        }
        assert_eq!(writeback.pending_len(), MAX_PENDING_TURNS);
        let pending = writeback.pending.lock().expect("锁可用");
        assert!(
            !pending.entries.contains_key(&oldest),
            "最早的一条应该先被淘汰"
        );
    }

    #[test]
    fn taking_a_pending_turn_consumes_it_exactly_once() {
        // 投递与沉默是两条互斥的收尾路径，谁先取到谁写；取过之后不能再被另一条
        // 路径取一次，否则同一轮会被写两遍。
        let writeback = MemoryWriteback::new();
        let event_id = EventId::new();
        writeback.stash_inbound(
            event_id,
            MemoryScope::Global,
            "行".to_string(),
            Utc::now(),
            true,
        );
        assert!(writeback.take_pending(event_id).is_some());
        assert!(writeback.take_pending(event_id).is_none());
    }

    #[test]
    fn observation_copies_do_not_leave_a_silent_turn_memory() {
        // 观察副本（Host 路注入的那一份）什么都没说时不该落档：那条消息的记忆归
        // Host 的观察流，Core 再写一份就是同一句话落两份。判据是它连"每小时名额"
        // 都不该动——名额只服务于真的会落档的回合。
        let writeback = MemoryWriteback::new();
        let group = MemoryScope::Conversation(yunxi_core::ConversationId::new());
        let event_id = EventId::new();
        writeback.stash_inbound(event_id, group, "行".to_string(), Utc::now(), false);
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(writeback.record_silent_turn(event_id));
        assert!(
            !writeback
                .silent_turns
                .lock()
                .expect("锁可用")
                .contains_key(&group),
            "观察副本不该占用沉默回合的名额"
        );
        assert!(
            writeback.take_pending(event_id).is_none(),
            "取过的暂存不该留在表里"
        );
    }

    #[test]
    fn silent_turn_slots_are_capped_per_scope() {
        // 护栏按作用域计：同一群第 limit+1 条不再落档，另一个群不受影响。
        let writeback = MemoryWriteback::new();
        let group = MemoryScope::Conversation(yunxi_core::ConversationId::new());
        let other = MemoryScope::Conversation(yunxi_core::ConversationId::new());
        assert!(writeback.claim_silent_turn_slot(group, 2));
        assert!(writeback.claim_silent_turn_slot(group, 2));
        assert!(
            !writeback.claim_silent_turn_slot(group, 2),
            "同一会话超过上限后不该再放行"
        );
        assert!(
            writeback.claim_silent_turn_slot(other, 2),
            "护栏是每会话的，别的会话不该被连坐"
        );
    }

    #[test]
    fn expired_silent_turn_slots_are_recycled() {
        // 窗口是滑动的：一小时前的名额不该继续占着。
        let writeback = MemoryWriteback::new();
        let group = MemoryScope::Conversation(yunxi_core::ConversationId::new());
        let expired = Instant::now()
            .checked_sub(Duration::from_secs(SILENT_TURN_WINDOW_SECS + 60))
            .expect("测试时钟应能回退");
        writeback
            .silent_turns
            .lock()
            .expect("锁可用")
            .entry(group)
            .or_default()
            .push_back(expired);
        assert!(
            writeback.claim_silent_turn_slot(group, 1),
            "过期的名额应该被回收后重新可用"
        );
    }

    fn model_args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().cloned().expect("参数应是对象")
    }

    #[test]
    fn model_memory_parses_the_documented_shape() {
        let request = parse_model_memory_request(&model_args(serde_json::json!({
            "content": " 她不吃香菜 ",
            "kind": "preference",
            "importance": 80
        })))
        .expect("应能解析");
        assert_eq!(request.content, "她不吃香菜");
        assert_eq!(request.kind, MemoryKind::Preference);
        assert_eq!(request.importance, 80);

        // 缺省：kind=fact、importance=50。
        let request = parse_model_memory_request(&model_args(
            serde_json::json!({"content": "他养了一只叫团子的猫"}),
        ))
        .expect("应能解析");
        assert_eq!(request.kind, MemoryKind::Fact);
        assert_eq!(request.importance, MODEL_MEMORY_DEFAULT_IMPORTANCE);
    }

    /// 模型给的值一律当不可信输入：空正文、胡编的 kind、越界或非整数的 importance
    /// 都要给回一个明确的错误（模型看得到回执，下次才知道怎么改）。
    #[test]
    fn model_memory_rejects_or_clamps_untrusted_arguments() {
        for arguments in [
            serde_json::json!({}),
            serde_json::json!({"content": "   "}),
            serde_json::json!({"content": 42}),
        ] {
            assert!(
                parse_model_memory_request(&model_args(arguments)).is_err(),
                "空正文或非字符串正文必须报错"
            );
        }

        let unknown_kind = parse_model_memory_request(&model_args(
            serde_json::json!({"content": "x", "kind": "rumor"}),
        ))
        .expect_err("未知 kind 必须报错而不是猜一个");
        assert!(
            unknown_kind.contains("fact / preference / event"),
            "{unknown_kind}"
        );

        let high = parse_model_memory_request(&model_args(
            serde_json::json!({"content": "x", "importance": 250}),
        ))
        .expect("应能解析");
        assert_eq!(high.importance, 100);
        let low = parse_model_memory_request(&model_args(
            serde_json::json!({"content": "x", "importance": -5}),
        ))
        .expect("应能解析");
        assert_eq!(low.importance, 0);
        assert!(
            parse_model_memory_request(&model_args(
                serde_json::json!({"content": "x", "importance": "很高"})
            ))
            .is_err(),
            "非整数 importance 必须报错"
        );
    }

    #[test]
    fn model_memory_content_is_bounded() {
        let long = "记".repeat(MAX_TEXT_CHARS + 100);
        let request =
            parse_model_memory_request(&model_args(serde_json::json!({ "content": long })))
                .expect("应能解析");
        assert_eq!(request.content.chars().count(), MAX_TEXT_CHARS);
    }
}
