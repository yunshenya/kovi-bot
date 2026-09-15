use crate::config;
use crate::group_access;
use crate::group_cooling::{
    AmbientInterjectionWatch, AmbientWatchStep, GroupCoolingVerdict, advance_ambient_watch,
    group_cooling_evidence, group_cooling_verdict,
};
use crate::health_check::HealthChecker;
use crate::memory::{GroupProfile, MEMORY_MANAGER};
use crate::model::coalesce::{MessageCoalescer, MessagePart};
use crate::model::conversation_coordinator::{
    ConversationCoordinator, IncomingAdmission, PendingTurn, WindowClaim, WindowQueueDecision,
    window_queue_decision,
};
use crate::model::conversation_state::{ConversationDecision, GroupConversationState};
use crate::model::interrupt::{
    ReplyScope, ReplyTicket, clear_reply_state_locked, is_active, scope_mutex,
};
use crate::model::recall::{
    clear_reply_scope_locked, has_recalled_messages, is_recent_bot_message,
    send_tracked_group_message,
};
use crate::model::reply::{clear_reply_targets, record_reply_target};
use crate::model::semantic::{MessageUnderstanding, UnderstandingRequest, understand};
use crate::model::traffic::{InboundScope, bounded_input, should_suppress};
use crate::model::utils::{
    clear_group_runtime_data, command_help, is_agent_task_command, is_bot_admin, is_group_paused,
    is_help_command, is_private_only_command, is_restricted_command,
    learn_user_profile_from_message, process_group_reply_claimed, report_vision_failure,
    send_sys_info, set_group_paused,
};
use crate::model::waiting_room::{self, DrainGuard, QueueView};
use crate::redis_store;
use crate::reminders;
use crate::sticker_memory;
use crate::sticker_memory::{
    StickerCandidateCommand, StickerScope, confirm_candidate, dismiss_candidate, extract_stickers,
    format_candidate_list, has_reply, has_usage, known_labels, parse_candidate_command,
    pending_candidates, quoted_message_context, stickers_for_teaching, teach, teaching_label,
    with_quoted_context, with_sticker_context, with_unknown_sticker_context,
};
use crate::vision::{
    ImageRequestScope, VisionImage, clear_group_pending_image_requests,
    consume_pending_image_request, extract_image_attachments, is_vision_command,
    merge_image_attachments, resolve_image_urls, strip_vision_command, with_social_image_context,
};
use chrono::Local;
use kovi::event::GroupMsgEvent;
use kovi::serde_json::json;
use kovi::tokio::sync::Mutex;
use kovi::{Message, RuntimeBot};
use rand::RngExt;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// 当前配置的接续对话窗口（秒）。取有效值：窗口长于群聊回复间隔时按
/// 回复间隔收口，否则一次可见回复之后窗口会一直敞开，未点名消息会
/// 变成"每句都进语义评估"。
fn continuation_window_secs() -> u64 {
    config::get()
        .group_interjection()
        .effective_continuation_window_secs()
}

/// 一次"群聊可见回复"的名额预留。
///
/// `confirmed` 只表示**真的发出去了**：预留发生在生成之前，模型完全可能最终
/// 沉默——那种情况下这一格必须归还，否则它会继续按间隔挡住别人（包括点名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleReplySlot {
    at: Instant,
    confirmed: bool,
    /// 这一条是**点名回复**。点名与未点名各自记账（见
    /// `GroupInterjectionConfig::addressed_reply_rate_limit`）。
    addressed: bool,
}

/// 未被确认的预留最多占位多久。
///
/// 它是对**显式归还**的兜底：正常路径在回合结束时就还（Host 看
/// `process_group_reply_claimed` 的真实结果，Core 看最终计划有没有可见正文），
/// 这里只兜"调用方在归还之前就离开了"（中途 `return`、取消、panic）。
///
/// 取值要同时满足两头：必须覆盖"预留 → 生成 → 真的发出去"的正常时延（早了
/// 会把真发出去的回复漏记，账本偏松、她变得更爱说话），又不能长到让一次异常
/// 把整群的回复节奏锁死（晚了就是 16:18 那类静默丢消息）。30 秒是两者的折中，
/// 而且偏早的代价只是少记一笔，偏晚的代价是用户看见的"她不回我"。
const UNCONFIRMED_REPLY_SLOT_TTL: Duration = Duration::from_secs(30);

/// 对话焦点：她上一次可见回复是在跟谁说话。
///
/// 只记 QQ 号与时刻：判定"这条未点名消息是不是接着跟她说"需要的就是这两个
/// 事实，不引入任何身份推断。别人的发言一到就清空（见
/// [`break_group_conversation_focus`]），所以它天然只覆盖一对一的连续来回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupConversationFocus {
    user_id: i64,
    since: Instant,
    /// 这段对话里她已经回了几句（含回复点名消息的那一轮）。仅供提示模型。
    replies: u32,
}

#[derive(Default)]
struct GroupInterjectionState {
    eligible_messages_since_sample: u32,
    last_interjection: Option<Instant>,
    interjection_in_flight: bool,
    decision_attempts: VecDeque<Instant>,
    conversation: GroupConversationState,
    /// 无论哪条发送路径（Host 或 Core）在本群发出可见消息的时间。
    last_bot_reply: Option<Instant>,
    /// 对话焦点：她上一次可见回复是在跟谁说话（QQ 号）+ 说话时刻。
    ///
    /// 与 `last_bot_reply`（只是"她刚说过话"的时间戳）不同，焦点带**对象**，
    /// 因此可以用来判断"这条未点名消息是不是接着跟她说"。
    ///
    /// 别人在群里说话**不会**结束焦点：2026-09-14 15:23 实测，群里 5 秒内就有人
    /// 插一句闲话，按"别人一说话就结束"实现的话，接续活不过一轮、功能等于没有。
    /// 焦点只按时间收敛（TTL），以及被她下一次可见回复换成新的对象。
    conversation_focus: Option<GroupConversationFocus>,
    /// 本群可见聊天回复的时间记录（有界），用于群级回复节奏硬限制。
    ///
    /// 每次预留都先记成**未确认**：真发出可见消息时由
    /// [`mark_group_reply_sent`] 确认；回合最终判沉默时由调用方归还
    /// （[`release_group_reply_slot`]），另有 30 秒的 stale 兜底。
    /// 线上 2026-09-14 16:18 实测：一个"只观察没回复"的回合预占了名额，
    /// 导致管理员紧接着的点名指令被 20 秒点名间隔挡住（只差 110 毫秒）。
    visible_replies: VecDeque<VisibleReplySlot>,
    /// 她最近一次**未点名插话**之后的观察：群里有没有人接她的话。
    ///
    /// 只在她主动插话（抽样路径真的发出可见回复）之后开始，被点名回答别人
    /// 不开始——那种"之后没人说话"多半只是对话结束了，不是被晾着。
    ambient_watch: Option<AmbientInterjectionWatch>,
}

impl GroupInterjectionState {
    /// 当前焦点对象；`now` / `ttl` 由调用方给定，纯逻辑便于测试与热路径复用。
    ///
    /// **不看句数**：这段对话已经接了几轮只作为事实交给模型（见
    /// `group_conversation_focus_state_now`），要不要继续说由她判断——
    /// 用"最多 N 句"当闸等于把话头交给计数器。
    fn focus_user_at(&self, now: Instant, ttl: Duration) -> Option<i64> {
        self.conversation_focus
            .filter(|focus| now.saturating_duration_since(focus.since) < ttl)
            .map(|focus| focus.user_id)
    }
}

/// 未点名接话只维护本地计数和冷却状态；不会为每一条群消息调用模型。
static GROUP_INTERJECTION_STATE: LazyLock<Mutex<HashMap<i64, GroupInterjectionState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static GROUP_MESSAGE_BATCHES: LazyLock<MessageCoalescer<(i64, i64)>> =
    LazyLock::new(Default::default);

type PendingWindowMessage = PendingTurn;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Addressing {
    at_self: bool,
    reply_to_self: bool,
    named_in_text: bool,
}

impl Addressing {
    fn detect(message: &Message, text: &str, self_id: i64, replied_sender_id: Option<i64>) -> Self {
        Self {
            at_self: message_at_self(message, self_id),
            reply_to_self: replied_sender_id == Some(self_id),
            named_in_text: text_mentions_bot(text),
        }
    }

    fn directly_addressed(self) -> bool {
        self.at_self || self.reply_to_self || self.named_in_text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupSenderIdentity {
    /// 稳定身份：记忆与历史里认人靠它。
    user_id: i64,
    qq_nickname: String,
    group_card: Option<String>,
}

impl GroupSenderIdentity {
    fn from_event(event: &GroupMsgEvent) -> Self {
        Self {
            user_id: event.user_id,
            qq_nickname: normalized_sender_name(event.sender.nickname.as_deref())
                .unwrap_or_else(|| "未设置昵称".to_string()),
            group_card: normalized_sender_name(event.sender.card.as_deref()),
        }
    }

    fn display_name(&self) -> &str {
        self.group_card.as_deref().unwrap_or(&self.qq_nickname)
    }

    /// 记忆与提示词里怎么标这条消息的说话人。
    ///
    /// **QQ 号在前、称呼只是显示**：群名片可以随时改、也可以被别人改成一样，
    /// 只记称呼的话"某某说过什么"会张冠李戴，事后无从分辨（用户 2026-09-14
    /// 明确要求：认人要用 QQ 号，不许用昵称查）。
    fn model_sender(&self, time: &str) -> String {
        format!(
            "[{}] 群成员 QQ={} 称呼={}",
            time,
            self.user_id,
            json!(self.display_name())
        )
    }

    fn reply_target_label(&self) -> String {
        self.display_name().to_string()
    }
}

fn normalized_sender_name(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect::<String>();
    (!normalized.is_empty()).then_some(normalized)
}

/// 当前回复期间使用有界 FIFO 保存完整 turn，避免跨成员混合正文和附件。
static PENDING_WINDOW_MESSAGES: LazyLock<Mutex<HashMap<i64, VecDeque<PendingWindowMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct DirectTriggerState {
    recent_triggers: VecDeque<Instant>,
    blocked_until: Option<Instant>,
    last_seen: Option<Instant>,
}

/// 防刷状态按“群 + 成员”隔离，不影响群内其他人正常聊天。
static DIRECT_TRIGGER_STATES: LazyLock<Mutex<HashMap<(i64, i64), DirectTriggerState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[allow(dead_code)]
pub async fn group_message_event(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    if event.user_id == event.self_id {
        println!(
            "[INFO] 忽略群聊自发消息回流 (群组: {}, 消息: {})",
            event.group_id, event.message_id
        );
        return;
    }
    let admission =
        ConversationCoordinator::begin_incoming(ReplyScope::Group(event.group_id)).await;
    crate::model::llm_trace::with_purpose(
        "group_reply",
        group_message_event_after_ingress(event, bot, admission),
    )
    .await;
    ConversationCoordinator::abandon_incoming(admission).await;
}

/// Core owns ordinary group replies, but agent tasks still need a cheap raw
/// observation of every member message to complete their cross-group loop.
pub(crate) async fn record_group_message_observation(event: &GroupMsgEvent) {
    let sender_identity = GroupSenderIdentity::from_event(event);
    let message = bounded_input(event.borrow_text().unwrap_or_default());
    if let Err(error) = crate::agent_tasks::record_group_message(
        event.group_id,
        event.message_id,
        event.user_id,
        sender_identity.display_name(),
        &message,
        &event.message,
        event.self_id,
    )
    .await
    {
        eprintln!(
            "[WARN] 保存跨群问答群成员回复失败 (群组: {}, 消息: {}): {}",
            event.group_id, event.message_id, error
        );
    }
}

/// 入站级相处证据：把这一轮里"指向她"的话交给模型判一次，折进关系张力与群气氛。
///
/// **必须由两条路都会经过的入站点调用**（`lib.rs` 的群聊入站闭包，与
/// [`record_group_message_observation`] 同一处）。它原先挂在 Host 群聊入口，
/// 而"指向她"的消息在 `classify_group` 里一律判给 Core，Host 根本不跑：骂她的
/// 那条必须指向她，指向她的那条又绕开那个函数——线上近 3 天
/// `[RELATION] 相处证据已记账` 一条都没有。
///
/// 定向在这里判（结构化 `@` 她 / 正文叫她的名字），情绪交给
/// [`crate::relation_evidence`] 的模型判定，且判定是后台任务：这一轮该走 Core
/// 还是 Host、该不该回，都不因为这个判定而等待或改变。
pub(crate) fn record_group_target_experience(event: &GroupMsgEvent) {
    let config = crate::config::get();
    let enabled = config.silence().relation_evidence_model_enabled();
    let text = bounded_input(event.borrow_text().unwrap_or_default());
    let directed_to_her =
        message_at_self(&event.message, event.self_id) || text_mentions_bot(&text);
    if !crate::relation_evidence::should_judge(directed_to_her, enabled) {
        return;
    }
    let sender_label = GroupSenderIdentity::from_event(event)
        .display_name()
        .to_string();
    crate::relation_evidence::spawn_judgement(
        event.group_id,
        event.user_id,
        crate::relation_evidence::EvidenceInput {
            sender_label: &sender_label,
            text: &text,
            question: crate::relation_evidence::EvidenceQuestion::TowardHer,
        },
    );
}

/// 群聊暂停控制命令的确定性解析：`Some(true)` = 禁言，`Some(false)` = 结束禁言。
///
/// 只有这两个字面命令会命中；其余 `#` 命令走各自的控制面分支。
fn group_pause_command(message: &str) -> Option<bool> {
    match message.trim() {
        "#禁言" => Some(true),
        "#结束禁言" => Some(false),
        _ => None,
    }
}

/// 暂停控制命令的用户可见回执。
fn group_pause_acknowledgement(paused: bool) -> &'static str {
    if paused {
        "禁言成功"
    } else {
        "结束成功"
    }
}

/// Apply the same bounded traffic and direct-address limits used by the Host
/// 群聊控制命令回执在会话占线时的重试等待（毫秒）：首次立即尝试，之后按这些间隔各重试一次。
const GROUP_DIRECT_RESPONSE_RETRY_DELAYS_MS: [u64; 3] = [200, 500, 1000];

/// before a Core-owned group message can consume ingress or model capacity.
pub(crate) async fn should_suppress_core_group_message(
    event: &GroupMsgEvent,
    bot: &RuntimeBot,
) -> bool {
    let sender_is_admin = is_bot_admin(bot, event.user_id);
    let directly_addressed = message_at_self(&event.message, event.self_id)
        || event.borrow_text().is_some_and(text_mentions_bot);
    // 点名她的消息不进"按人限流 + 120 秒整段封锁"：那是对她说的请求，被静默吞掉
    // 正是"她不回我"最伤的形态（线上 2026-09-14 20:44：不忻一分钟发了 48 条，
    // 之后 4 次 `[at] 说句话` 全部被整段封锁吃掉，他以为被拉黑了）。防重复点名
    // 由下面的 `should_suppress_direct_trigger` 负责，她的回复节奏（点名档 20 秒 +
    // 10 条/10 分钟）继续兜住输出；全局 300/60 秒上限对所有人仍然有效。
    if should_suppress(
        InboundScope::Group {
            group_id: event.group_id,
            user_id: event.user_id,
        },
        sender_is_admin || directly_addressed,
    )
    .await
    {
        return true;
    }
    directly_addressed
        && !sender_is_admin
        && should_suppress_direct_trigger(event.group_id, event.user_id).await
}

pub(crate) async fn group_message_event_after_ingress(
    event: Arc<GroupMsgEvent>,
    bot: Arc<RuntimeBot>,
    initial_admission: IncomingAdmission,
) {
    let ingress = initial_admission.ticket;
    let group_id = event.group_id;
    let time_now_data = Local::now();
    let time = time_now_data.format("%H:%M:%S").to_string();
    let sender_identity = GroupSenderIdentity::from_event(&event);
    let nickname = sender_identity.qq_nickname.clone();
    let sender = sender_identity.model_sender(&time);
    let bounded_message = bounded_input(event.borrow_text().unwrap_or_default());
    let message = bounded_message.as_str();
    let sender_is_admin = is_bot_admin(&bot, event.user_id);
    let restricted_command = is_restricted_command(message);
    if restricted_command {
        println!(
            "[INFO] 群聊管理命令收到 (群组: {}, 用户: {}, 管理员: {}, 命令: {})",
            group_id,
            event.user_id,
            sender_is_admin,
            message.trim()
        );
    }
    if restricted_command && !sender_is_admin {
        println!(
            "[INFO] 群聊未授权命令已静默 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }
    // #禁言 / #结束禁言 是确定性控制面：在任何批次合并、窗口排队或模型调用
    // 之前直接落状态并回执。此前它们和普通消息走同一条链路，只要本群还有
    // 回复在途就会整条命令进 PENDING_WINDOW_MESSAGES，而队列只在"回复完成"
    // 路径里被 drain，禁言状态根本没写进去——管理员看到的现象就是
    // "发 #禁言 没有效果"。控制命令永远不该被排队或合并。
    if let Some(paused) = group_pause_command(message) {
        set_group_paused(group_id, paused).await;
        println!(
            "[INFO] 群聊禁言状态已更新 (群组: {}, 用户: {}, 禁言: {})",
            group_id, event.user_id, paused
        );
        // 直接回执会顶掉本群在途/排队的回复，让禁言立即生效而不是"这一轮说完
        // 再说"；drain 由该路径统一负责，不会留下悬空队列。
        send_group_direct_response(
            &bot,
            group_id,
            initial_admission,
            group_pause_acknowledgement(paused),
        )
        .await;
        return;
    }
    if is_help_command(message) {
        send_group_direct_response(&bot, group_id, initial_admission, command_help()).await;
        return;
    }
    if group_access::is_authorization_command(message) {
        let reply = group_access::handle_command(&bot, message, Some(group_id), event.user_id)
            .await
            .unwrap_or_else(|| group_access::command_help().to_string());
        send_group_direct_response(&bot, group_id, initial_admission, reply).await;
        return;
    }
    if is_agent_task_command(message) {
        println!(
            "[INFO] 群聊跨群任务命令已忽略（该控制面仅限主管理员私聊） (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }
    if is_private_only_command(message) {
        println!(
            "[INFO] 群聊私聊专用命令已忽略（仅限私聊） (群组: {}, 用户: {}, 命令: {})",
            group_id,
            event.user_id,
            message.trim()
        );
        return;
    }
    let stickers = extract_stickers(&event.message);
    let current_images = extract_image_attachments(&event.message);
    let vision_command = is_vision_command(message);
    let sticker_scope = StickerScope::Group(group_id);
    let sticker_teaching_message = (sender_is_admin
        && (!stickers.is_empty() || has_reply(&event.message)))
    .then(|| event.message.clone());
    let reply_scope = ReplyScope::Group(group_id);
    // Phase 3 影子:批次真实走向(回复/沉默)配对,不改变路由。
    let mut shadow_guard =
        crate::yunxi::turn_gate_shadow::OutcomeGuard::new(yunxi_core::TurnScope::Group);
    if event.user_id == event.self_id {
        println!(
            "[INFO] 忽略群聊自发消息回流 (群组: {}, 消息: {})",
            group_id, event.message_id
        );
        return;
    }
    if let Err(error) = crate::agent_tasks::record_group_message(
        group_id,
        event.message_id,
        event.user_id,
        sender_identity.display_name(),
        message,
        &event.message,
        event.self_id,
    )
    .await
    {
        eprintln!(
            "[WARN] 保存跨群问答群成员回复失败 (群组: {}, 消息: {}): {}",
            group_id, event.message_id, error
        );
    }
    // 与 Core 侧同一条规则：点名她的消息不受"按人限流 + 整段封锁"影响
    // （防重复点名另有 `should_suppress_direct_trigger`）。引用她需要异步解析，
    // 这里先用与 Core 一致的两个廉价判据：结构化 @ 或正文点名。
    let directly_addressed =
        message_at_self(&event.message, event.self_id) || text_mentions_bot(message);
    if should_suppress(
        InboundScope::Group {
            group_id,
            user_id: event.user_id,
        },
        sender_is_admin || directly_addressed,
    )
    .await
    {
        println!(
            "[INFO] 群聊入站流量已抑制 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }
    match message.trim() {
        "#删除本群数据" => {
            send_group_direct_response(
                &bot,
                group_id,
                initial_admission,
                "这会删除本群的长期记忆、群档案、摘要、本群表情记忆和以本群为目标的角色动作记录。若确认，请发送：#删除本群数据 确认",
            )
            .await;
            return;
        }
        "#删除本群数据 确认" => {
            delete_group_data(group_id, &bot).await;
            return;
        }
        "#系统信息" => {
            println!("[INFO] 群聊系统信息命令进入处理分支 (群组: {})", group_id);
            if ConversationCoordinator::resolve_active_reply_for_direct_response(initial_admission)
                .await
            {
                send_sys_info(Arc::clone(&bot), group_id).await;
            }
            // 系统信息使用独立的工具发送路径；无论解析或发送是否成功，
            // 都要把显式命令替换后留下的 FIFO 交回 drainer。
            drain_pending_window_messages_from_current(group_id, &bot).await;
            return;
        }
        "#健康检查" => {
            if ConversationCoordinator::resolve_active_reply_for_direct_response(initial_admission)
                .await
            {
                send_health_status(&bot, group_id).await;
            }
            drain_pending_window_messages_from_current(group_id, &bot).await;
            return;
        }
        _ => {}
    }
    if let Some(command) = parse_candidate_command(message) {
        let reply = match command {
            StickerCandidateCommand::List => match pending_candidates(Some(sticker_scope), 8).await
            {
                Ok(candidates) => format_candidate_list(&candidates),
                Err(error) => {
                    eprintln!("[ERROR] 读取群聊表情包候选失败: {}", error);
                    "暂时读取不到待确认表情候选，请稍后再试。".to_string()
                }
            },
            StickerCandidateCommand::Confirm {
                candidate_id,
                label,
            } => match confirm_candidate(candidate_id, &label, event.user_id, Some(sticker_scope))
                .await
            {
                Ok(true) => format!("已确认候选 {}，以后这个表情表示“{}”。", candidate_id, label),
                Ok(false) => "找不到这个待确认候选，可能已经处理过或不属于本群。".to_string(),
                Err(error) => {
                    eprintln!("[ERROR] 确认群聊表情包候选失败: {}", error);
                    "这次没能确认这个表情候选，请稍后再试。".to_string()
                }
            },
            StickerCandidateCommand::Reject { candidate_id } => {
                match dismiss_candidate(candidate_id, event.user_id, Some(sticker_scope), false, 30)
                    .await
                {
                    Ok(true) => format!("已驳回候选 {}，近期不会重复提醒。", candidate_id),
                    Ok(false) => "找不到这个待确认候选，可能已经处理过或不属于本群。".to_string(),
                    Err(error) => {
                        eprintln!("[ERROR] 驳回群聊表情包候选失败: {}", error);
                        "这次没能驳回这个表情候选，请稍后再试。".to_string()
                    }
                }
            }
            StickerCandidateCommand::Ignore { candidate_id, days } => {
                match dismiss_candidate(
                    candidate_id,
                    event.user_id,
                    Some(sticker_scope),
                    true,
                    days,
                )
                .await
                {
                    Ok(true) => format!("已忽略候选 {}，{} 天内不会重复提醒。", candidate_id, days),
                    Ok(false) => "找不到这个待确认候选，可能已经处理过或不属于本群。".to_string(),
                    Err(error) => {
                        eprintln!("[ERROR] 忽略群聊表情包候选失败: {}", error);
                        "这次没能忽略这个表情候选，请稍后再试。".to_string()
                    }
                }
            }
            StickerCandidateCommand::Invalid => {
                "格式：#待确认表情、#确认表情 编号 含义、#驳回表情 编号、#忽略表情 编号 [天数]。"
                    .to_string()
            }
        };
        send_group_direct_response(&bot, group_id, initial_admission, reply).await;
        return;
    }
    // 素材库命令：她发表情包走的那条出口，管理员可以在 QQ 里直接验收，不经过模型。
    if let Some(command) = crate::sticker_library::parse_command(message) {
        println!(
            "[INFO] 群聊表情包素材库命令进入处理分支 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        handle_sticker_library_command(&bot, group_id, initial_admission, command).await;
        return;
    }
    if is_recent_bot_message(reply_scope, event.message_id).await {
        println!(
            "[INFO] 忽略群聊已记录消息回流 (群组: {}, 消息: {})",
            group_id, event.message_id
        );
        return;
    }
    let structured_at_self = message_at_self(&event.message, event.self_id);
    let locally_addressed = structured_at_self || text_mentions_bot(message);
    // 相处证据曾经在这里记账。它只对"指向她"的消息生效，而这批消息在
    // `classify_group` 里判给 Core、走不到这个 Host 入口，所以已经上移到两条路
    // 共同的入站点（`lib.rs` 的 `record_group_target_experience`）。不要在这里
    // 加回来：那会让 Host 路重复记账，而 Core 路继续漏。
    // Shadow-mode World Model social scene feed: deterministic, no model
    // call, no reply influence (v4 §145–146). No-op when disabled.
    crate::yunxi::world_model::record_group_scene(
        crate::yunxi::world_model::scene_group_conversation_id(group_id),
        crate::yunxi::world_model::scene_person_id(event.user_id),
        vec![crate::yunxi::world_model::scene_person_id(event.user_id)],
        locally_addressed,
    );
    // R4 shadow soft signal: read the world back and log what an Executive
    // would see. Log only — the behavioral wiring stays disabled until the
    // shadow metrics are reviewed (v4 §217).
    if crate::config::get().world_model().enabled()
        && let Some(summary) = crate::yunxi::world_model::conversation_world_summary(
            crate::yunxi::world_model::scene_group_conversation_id(group_id),
        )
    {
        kovi::log::debug!("[YUNXI_WORLD] soft_signal {}", summary.render());
    }
    // Behavioral gate (v4 §103/§197): when influence_mode=active and the
    // world says this is a rapid unaddressed discussion with the floor held
    // by others, stay silent instead of interjecting. Default disabled → no
    // behavioral change. 与普通成员一致：管理员也不越过这条场合判断。
    if !locally_addressed
        && crate::yunxi::world_model::interruption_guard(
            crate::yunxi::world_model::scene_group_conversation_id(group_id),
        ) > 0.7
    {
        println!(
            "[INFO] 世界模型场合抑制插话 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        kovi::log::debug!(
            "[YUNXI_WORLD] influence_interrupt_suppressed 请核对影子指标后再评估是否保留"
        );
        return;
    }
    if locally_addressed && should_suppress_direct_trigger(group_id, event.user_id).await {
        println!(
            "[INFO] 群聊重复或高频点名已静默 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }
    record_reply_target(
        reply_scope,
        event.message_id,
        Some(event.user_id),
        sender_identity.reply_target_label(),
        &event.human_text,
    )
    .await;
    if let Some(label) = teaching_label(message) {
        match stickers_for_teaching(&event.message, &bot, sticker_scope).await {
            Ok(teaching_stickers) if !teaching_stickers.is_empty() => {
                match teach(&teaching_stickers, &label, event.user_id, sticker_scope).await {
                    Ok(count) => {
                        send_group_direct_response(
                            &bot,
                            group_id,
                            initial_admission,
                            format!("记住啦，这 {count} 个表情以后表示“{label}”。"),
                        )
                        .await;
                    }
                    Err(error) => {
                        eprintln!("[ERROR] 群聊保存表情包记忆失败: {}", error);
                        send_group_direct_response(
                            &bot,
                            group_id,
                            initial_admission,
                            "这次没能记住，稍后再教我一次吧。",
                        )
                        .await;
                    }
                }
            }
            Ok(_) => {
                send_group_direct_response(
                    &bot,
                    group_id,
                    initial_admission,
                    "请回复（引用）那张表情包，再发送 #教芸汐 这个表情是……哦。",
                )
                .await;
            }
            Err(error) => {
                eprintln!("[ERROR] 群聊读取被引用表情失败: {}", error);
                send_group_direct_response(
                    &bot,
                    group_id,
                    initial_admission,
                    "我没能读到被引用的表情，请重新引用后再试一次哦。",
                )
                .await;
            }
        }
        return;
    }

    // 暂停期间的静默不在这里提前 return：#禁言 生效后非管理员回合会在
    // `process_group_reply_inner` 里被确定性挡下，管理员回合仍要走到模型，
    // 因为工具链在 group_paused 下只允许 read-only 与 group.resume，并确定性
    // 返回静默——这是「恢复本群回复」的自然语言恢复通道，提前 return 会把它
    // 掐断，只剩 #结束禁言 一条路。

    let labels = match known_labels(&stickers, sticker_scope).await {
        Ok(labels) => labels,
        Err(error) => {
            eprintln!("[ERROR] 群聊读取表情包记忆失败: {}", error);
            Vec::new()
        }
    };
    let quoted = match quoted_message_context(&event.message, &bot, sticker_scope).await {
        Ok(quoted) => quoted,
        Err(error) => {
            eprintln!("[ERROR] 群聊读取引用消息失败: {}", error);
            None
        }
    };
    if let Some(quoted) = quoted.as_ref()
        && let Some(message_id) = quoted.message_id
    {
        record_reply_target(
            reply_scope,
            message_id,
            quoted.sender_id,
            quoted.sender_label.as_deref().unwrap_or("引用消息"),
            &quoted.content,
        )
        .await;
    }
    let quoted_images = quoted
        .as_ref()
        .map(|quoted| quoted.images.as_slice())
        .unwrap_or_default();
    let images = merge_image_attachments(&current_images, quoted_images);
    let addressing = Addressing::detect(
        &event.message,
        message,
        event.self_id,
        quoted.as_ref().and_then(|quoted| quoted.sender_id),
    );
    let addressed_to_bot = addressing.directly_addressed();
    // 群级降温的第二维证据：她插话后有没有人接。
    // 与个人级那条相处证据（`relation_evidence`，在入站点发起）分工见
    // `crate::group_cooling`：个人级记某个人对她的张力，群级记这个群的整体气氛；
    // 后者只在未点名抽样的入口生效，不改任何被点名/被引用的回合。
    observe_group_cooling(
        group_id,
        event.user_id,
        sender_identity.display_name(),
        message,
        addressed_to_bot,
    )
    .await;
    if addressed_to_bot
        && !locally_addressed
        && should_suppress_direct_trigger(group_id, event.user_id).await
    {
        println!(
            "[INFO] 群聊重复或高频点名已静默 (群组: {}, 用户: {})",
            group_id, event.user_id
        );
        return;
    }
    let pending_image_request = consume_pending_image_request(
        ImageRequestScope::Group {
            group_id,
            user_id: event.user_id,
        },
        !images.is_empty(),
    )
    .await;
    // 消息通过 at/reply 段明确指向其他成员（而非芸汐）时，只观察不插话，
    // 也不触发接续对话：点名谁就由谁接话，避免“只要有 @ 就回复”的错觉。
    if !addressed_to_bot
        && !vision_command
        && !pending_image_request
        && directed_at_others(&event.message)
    {
        println!(
            "[INFO] 群聊消息指向其他成员，仅观察不回复 (群组: {}, 用户: {}, {} at_self={} reply_to_self={} named={})",
            group_id,
            event.user_id,
            addressing_evidence(&event.message, event.self_id),
            addressing.at_self,
            addressing.reply_to_self,
            addressing.named_in_text,
        );
        return;
    }
    if message.trim().is_empty()
        && (!images.is_empty() || !stickers.is_empty())
        && !vision_command
        && !pending_image_request
        && !addressed_to_bot
    {
        println!("[INFO] 收到群聊纯图片状态，保持静默 (群组: {})", group_id);
        return;
    }
    let active_reply = is_active(reply_scope).await;
    let (conversation_active, conversation_context) = group_conversation_snapshot(group_id).await;
    // 合并前只做确定性的本地判断，完整语义理解在批次形成后只调用一次。
    let mut vision_requested = vision_command
        || pending_image_request
        || (addressed_to_bot && !images.is_empty() && labels.is_empty());
    if vision_command && images.is_empty() {
        send_group_direct_response(
            &bot,
            group_id,
            initial_admission,
            "请把截图和 #看截图 放在一起，或回复那张截图再发送命令哦。",
        )
        .await;
        return;
    }
    if message.trim().is_empty()
        && stickers.is_empty()
        && !has_reply(&event.message)
        && !locally_addressed
        && !vision_requested
    {
        return;
    }
    let text_message = if vision_command {
        strip_vision_command(message)
    } else {
        message.to_string()
    };
    let sticker_used_before = if stickers.is_empty() {
        false
    } else {
        match has_usage(&stickers, sticker_scope).await {
            Ok(used) => used,
            Err(error) => {
                eprintln!("[ERROR] 群聊读取表情包使用记录失败: {}", error);
                false
            }
        }
    };
    let current_message = if labels.is_empty() && !stickers.is_empty() {
        with_unknown_sticker_context(&text_message, stickers.len(), sticker_used_before)
    } else {
        with_sticker_context(&text_message, &labels)
    };
    let model_message = quoted.as_ref().map_or(current_message.clone(), |quoted| {
        with_quoted_context(&current_message, quoted)
    });
    let model_message = if structured_at_self {
        with_structured_bot_mention_context(&model_message)
    } else {
        model_message
    };
    let mut turn_gate_response = None;
    let (
        model_message,
        addressed_to_bot,
        plain_text,
        intent_text,
        batch_vision_requested,
        images,
        source_message_ids,
    ) = if !message.trim_start().starts_with('#') {
        let turn_gate_context = crate::model::coalesce::TurnGateBatchContext {
            scope: yunxi_core::TurnScope::Group,
            conversation_active: crate::model::conversation_continuation_active_now(group_id),
            addressed_to_agent: addressed_to_bot,
            replies_to_agent: has_reply(&event.message),
            pending_task: false,
            pending_outgoing: false,
        };
        let Some(combined) = GROUP_MESSAGE_BATCHES
            .push_with_turn_gate(
                (group_id, event.user_id),
                MessagePart {
                    text: model_message,
                    intent_text: message.to_string(),
                    addressed: addressed_to_bot,
                    plain_text: stickers.is_empty() && quoted.is_none(),
                    vision_requested,
                    sticker_reaction: false,
                    images,
                    message_ids: vec![event.message_id],
                },
                turn_gate_context,
                || async {
                    if let Some(runtime) = crate::yunxi::intrinsic_runtime::get() {
                        runtime.classify_input_completion(message).await
                    } else {
                        yunxi_core::InputCompletion::Incomplete
                    }
                },
            )
            .await
        else {
            return;
        };
        turn_gate_response = combined.turn_gate_response;
        (
            combined.text,
            combined.addressed,
            combined.plain_text,
            combined.intent_text,
            combined.vision_requested,
            combined.images,
            combined.message_ids,
        )
    } else {
        (
            model_message,
            addressed_to_bot,
            false,
            message.to_string(),
            vision_requested,
            images,
            vec![event.message_id],
        )
    };
    if has_recalled_messages(reply_scope, &source_message_ids).await {
        println!(
            "[INFO] 群聊输入已撤回，丢弃尚未开始的回复 (群组: {})",
            group_id
        );
        return;
    }
    let batch_request = UnderstandingRequest {
        message: intent_text.clone(),
        context: "group_chat_batch".to_string(),
        quoted_message: quoted.as_ref().map(|value| value.content.clone()),
        has_images: !images.is_empty(),
        quoted_has_images: !quoted_images.is_empty(),
        has_recent_images: false,
        explicit_vision_command: batch_vision_requested,
        pending_image_request: false,
        addressed_to_bot,
        conversation_active: active_reply || conversation_active,
        conversation_context,
        sticker_reaction: false,
    };
    // 未点名抽样的边界就此划定：被 @、被引用、被点名、显式识图，以及正处在
    // 她自己回合里（`active_reply` / 接续窗口）的消息都不走这条路。群级降温
    // 装在 `reserve_interjection_decision` 里，因此它碰不到上述任何一种回合。
    let ambient_sampling_eligible = ambient_sampling_eligible(
        addressed_to_bot,
        batch_vision_requested,
        !images.is_empty(),
        active_reply,
        conversation_active,
    );
    let semantic_required = !ambient_sampling_eligible;
    let mut sampled_for_interjection = if semantic_required {
        None
    } else {
        reserve_interjection_decision(group_id, &intent_text).await
    };
    let understanding = if semantic_required || sampled_for_interjection.is_some() {
        understand(batch_request.clone()).await
    } else {
        MessageUnderstanding::default()
    };
    crate::yunxi::events::project_interaction_cues(event.user_id, understanding.interaction_cues());
    let asks_for_silence = plain_text && (understanding.wants_no_reply || understanding.wants_stop);
    if asks_for_silence {
        if let Some(attempt) = sampled_for_interjection.take() {
            attempt.complete(false).await;
        }
        stop_group_reply(group_id, event.user_id, ingress).await;
        println!(
            "[INFO] 合并后的群聊消息请求停止当前回复 (群组: {})",
            group_id
        );
        return;
    }
    if !understanding.interjection_worthy
        && let Some(attempt) = sampled_for_interjection.take()
    {
        attempt.complete(false).await;
    }
    vision_requested = !config::get().vision().disabled()
        && (batch_vision_requested || understanding.should_understand_image(&batch_request));
    if intent_text.trim().is_empty()
        && !vision_requested
        && model_message.trim().is_empty()
        && (!images.is_empty() || !stickers.is_empty())
    {
        println!("[INFO] 收到群聊纯图片状态，保持静默 (群组: {})", group_id);
        return;
    }
    let model_message = if !images.is_empty() && !vision_requested && !intent_text.trim().is_empty()
    {
        with_social_image_context(&model_message)
    } else {
        model_message
    };
    let vision_images = if vision_requested {
        match resolve_image_urls(&images, &bot).await {
            Ok(images) if !images.is_empty() => images,
            Ok(_) => {
                report_vision_failure(
                    &bot,
                    &format!("群聊 {}", group_id),
                    message,
                    "未解析到可用图片地址",
                )
                .await;
                return;
            }
            Err(error) => {
                report_vision_failure(
                    &bot,
                    &format!("群聊 {}", group_id),
                    message,
                    &error.to_string(),
                )
                .await;
                return;
            }
        }
    } else {
        Vec::new()
    };
    if !message.trim().is_empty() {
        update_group_profile(group_id, event.user_id, &understanding).await;
        learn_user_profile_from_message(event.user_id, message, &nickname, false, &understanding)
            .await;
    }
    // 被点名时始终处理；未点名消息仅由本地节流器偶尔抽样，不逐条调用模型。
    let group_paused = is_group_paused(group_id).await;
    let explicit_sticker_teaching =
        sender_is_admin && sticker_teaching_message.is_some() && !message.trim().is_empty();
    // 暂停期间管理员仍然保留一次模型回合：工具链在 group_paused 下只会静默
    // 或执行 group.resume，因此这里不会让禁言期间出现闲聊回复。
    let primary_reply_expected = addressed_to_bot
        || vision_requested
        || explicit_sticker_teaching
        || matches!(message.trim(), "#禁言" | "#结束禁言")
        || (group_paused && sender_is_admin);
    let conversation_decision = observe_group_conversation(
        group_id,
        event.user_id,
        &understanding,
        primary_reply_expected
            || (sampled_for_interjection.is_some() && understanding.interjection_worthy),
    )
    .await;
    let continue_conversation = !primary_reply_expected && conversation_decision.continue_reply;
    // 群聊可见回复节奏硬限制（同群所有普通聊天回复共享额度）。显式识图
    // 请求、表情教学与禁言命令不受限；额度被拒时本条仅作观察，不生成可见
    // 回复——这是"每句话都回/扑上来接话"的确定性兜底。
    // 被点名/被引用的消息用更短的 `addressed_reply_gap_secs`：直接提问
    // 不该因为"21 秒前刚回过别人"就被静默丢掉。
    // 管理员只豁免**等待间隔**（间隔是为了不刷屏，管理员的话是明确指令），
    // `reply_rate_limit` 频率上限对所有人一致——避免"管理员句句都回"。
    let addressed_gap_secs = paced_group_reply_gap_secs(primary_reply_expected, sender_is_admin);
    // 预留是乐观的（生成之前先占位），所以拿到的 token 要留到回合结束：
    // 这一轮最终没发出可见消息时必须归还，否则名额会继续按间隔挡住别人。
    let paced_reply_pending = primary_reply_expected
        || continue_conversation
        || (sampled_for_interjection.is_some() && understanding.interjection_worthy);
    let pacing_exempt = vision_requested
        || explicit_sticker_teaching
        || matches!(message.trim(), "#禁言" | "#结束禁言");
    let mut group_reply_slot: Option<Instant> = None;
    let reply_budget_ok = if !paced_reply_pending || pacing_exempt {
        true
    } else {
        // `primary_reply_expected` 就是「被点名/被引用/显式请求」这一类，走点名额度。
        // 管理员点名那一档额度也豁免（间隔早就豁免了）：他的话是明确指令，
        // 不该被"她刚刚聊得很热闹"静默丢掉。未点名的自动接话仍按普通额度收着。
        let budget_class = reply_budget_class(primary_reply_expected, sender_is_admin);
        match reserve_group_chat_reply_slot(group_id, addressed_gap_secs, budget_class).await {
            Some(at) => {
                group_reply_slot = Some(at);
                true
            }
            None => false,
        }
    };
    if !reply_budget_ok && let Some(attempt) = sampled_for_interjection.take() {
        attempt.complete(false).await;
    }
    let direct_reply_expected = reply_budget_ok
        && (primary_reply_expected
            || continue_conversation
            || (sampled_for_interjection.is_some() && understanding.interjection_worthy));
    let Some(admission) = admit_understood_group_turn(
        initial_admission,
        &understanding,
        direct_reply_expected,
        message.trim().is_empty(),
    )
    .await
    else {
        println!(
            "[INFO] 群聊语义决定已过期，丢弃旧批次 (群组: {}, 消息: {:?})",
            group_id, source_message_ids
        );
        release_unconfirmed_reply_slot(group_id, group_reply_slot, false).await;
        return;
    };
    // Phase 4 门控(仅 response_mode=active 且 bundle 就绪):未点名的
    // 批次被 response head 判 Ignore/Wait 时保持沉默;被点名/视觉/教学/
    // 命令(primary_reply_expected)不受影响;Abstain 不写入字段按原路径。
    if !primary_reply_expected
        && crate::yunxi::turn_gate_runtime::response_gate_active_global()
        && matches!(
            turn_gate_response,
            Some(yunxi_core::TurnResponseDecision::Ignore | yunxi_core::TurnResponseDecision::Wait)
        )
    {
        println!(
            "[INFO] TurnGate 门控：群聊批次保持沉默 (群组: {})",
            group_id
        );
        release_unconfirmed_reply_slot(group_id, group_reply_slot, false).await;
        return;
    }
    if primary_reply_expected && reply_budget_ok {
        if !stickers.is_empty()
            && let Err(error) = sticker_memory::record_usage(
                &stickers,
                sticker_scope,
                event.message_id,
                &text_message,
                "",
                Arc::clone(&bot),
            )
            .await
        {
            eprintln!("[ERROR] 群聊保存表情包使用记录失败: {}", error);
        }
        let claim = claim_or_queue_group_reply(
            reply_scope,
            admission,
            true,
            group_id,
            event.user_id,
            sender.clone(),
            model_message.clone(),
            vision_images.clone(),
            source_message_ids.clone(),
            sticker_teaching_message.clone(),
            understanding.clone(),
        )
        .await;
        let Some(ticket) = settle_window_claim(claim, group_id, &bot).await else {
            return;
        };
        let turn_marker = begin_conversation_turn(group_id, event.user_id, &understanding).await;
        let replied = process_group_reply_claimed(
            group_id,
            event.user_id,
            &model_message,
            Arc::clone(&bot),
            sender,
            &[],
            ticket,
            None,
            vision_images.clone(),
            source_message_ids.clone(),
            sticker_teaching_message.clone(),
            understanding.clone(),
            true,
        )
        .await;
        shadow_guard.mark_replied(replied);
        release_unconfirmed_reply_slot(group_id, group_reply_slot, replied).await;
        finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
        drain_pending_window_messages(group_id, Arc::clone(&bot), ticket).await;
    } else if continue_conversation && reply_budget_ok {
        println!("[INFO] 群聊接续对话 (群组: {})", group_id);
        if !stickers.is_empty()
            && let Err(error) = sticker_memory::record_usage(
                &stickers,
                sticker_scope,
                event.message_id,
                &text_message,
                "",
                Arc::clone(&bot),
            )
            .await
        {
            eprintln!("[ERROR] 群聊保存表情包使用记录失败: {}", error);
        }
        let claim = claim_or_queue_group_reply(
            reply_scope,
            admission,
            true,
            group_id,
            event.user_id,
            sender.clone(),
            model_message.clone(),
            vision_images.clone(),
            source_message_ids.clone(),
            sticker_teaching_message.clone(),
            understanding.clone(),
        )
        .await;
        let Some(ticket) = settle_window_claim(claim, group_id, &bot).await else {
            return;
        };
        let turn_marker = begin_conversation_turn(group_id, event.user_id, &understanding).await;
        let replied = process_group_reply_claimed(
            group_id,
            event.user_id,
            &model_message,
            Arc::clone(&bot),
            sender,
            &[],
            ticket,
            None,
            vision_images.clone(),
            source_message_ids.clone(),
            sticker_teaching_message.clone(),
            understanding.clone(),
            true,
        )
        .await;
        shadow_guard.mark_replied(replied);
        release_unconfirmed_reply_slot(group_id, group_reply_slot, replied).await;
        finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
        drain_pending_window_messages(group_id, Arc::clone(&bot), ticket).await;
    } else if sampled_for_interjection.is_some()
        && understanding.interjection_worthy
        && reply_budget_ok
    {
        println!("[INFO] 群聊未点名接话 (群组: {})", group_id);
        if !stickers.is_empty()
            && let Err(error) = sticker_memory::record_usage(
                &stickers,
                sticker_scope,
                event.message_id,
                &text_message,
                "",
                Arc::clone(&bot),
            )
            .await
        {
            eprintln!("[ERROR] 群聊保存表情包使用记录失败: {}", error);
        }
        let claim = claim_or_queue_group_reply(
            reply_scope,
            admission,
            false,
            group_id,
            event.user_id,
            sender.clone(),
            model_message.clone(),
            vision_images.clone(),
            source_message_ids.clone(),
            sticker_teaching_message.clone(),
            understanding.clone(),
        )
        .await;
        let Some(ticket) = settle_window_claim(claim, group_id, &bot).await else {
            return;
        };
        let turn_marker = begin_conversation_turn(group_id, event.user_id, &understanding).await;
        let max_output_tokens = config::get()
            .group_interjection()
            .interjection_max_output_tokens();
        let replied = process_group_reply_claimed(
            group_id,
            event.user_id,
            &model_message,
            Arc::clone(&bot),
            sender,
            &[],
            ticket,
            Some(max_output_tokens),
            vision_images.clone(),
            source_message_ids.clone(),
            sticker_teaching_message.clone(),
            understanding.clone(),
            false,
        )
        .await;
        if let Some(attempt) = sampled_for_interjection.take() {
            attempt.complete(replied).await;
        }
        shadow_guard.mark_replied(replied);
        release_unconfirmed_reply_slot(group_id, group_reply_slot, replied).await;
        finish_conversation_turn(group_id, event.user_id, turn_marker, replied).await;
        drain_pending_window_messages(group_id, Arc::clone(&bot), ticket).await;
    } else {
        // 这条路径不生成可见回复，只记一条观察：正常推演下 `group_reply_slot`
        // 已经是 None（有预留就一定会走进上面三个分支），这里仍然归还一次，
        // 守住"每一条结束路径都归还名额"这个不变量——将来在上面新增分支
        // （比如新的静默拦截）时也不会漏。
        release_unconfirmed_reply_slot(group_id, group_reply_slot, false).await;
        if let Err(error) = MEMORY_MANAGER
            .add_conversation_memory_with_hints(
                group_id,
                &format!("{}: {}", sender, model_message),
                "group_observation",
                Some(understanding.memory_importance()),
                &understanding.memory_tags(),
            )
            .await
        {
            eprintln!(
                "[ERROR] 群聊观察记忆记录失败 (群组: {}): {}",
                group_id, error
            );
        }
    }
}

async fn send_group_direct_response(
    bot: &Arc<RuntimeBot>,
    group_id: i64,
    admission: IncomingAdmission,
    content: impl Into<String>,
) -> bool {
    let content = content.into();
    let resolved =
        ConversationCoordinator::resolve_active_reply_for_direct_response(admission).await;
    let mut sent = false;
    if resolved {
        // 同私聊：群里有普通回合在收尾时直发会直接返回 ConversationBusy。
        // #禁言 / #结束禁言 是控制命令，必须拿到回执，所以短暂重试。
        for (attempt, delay_ms) in std::iter::once(0_u64)
            .chain(GROUP_DIRECT_RESPONSE_RETRY_DELAYS_MS.iter().copied())
            .enumerate()
        {
            if attempt > 0 {
                kovi::tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            sent = send_tracked_group_message(bot, group_id, content.clone()).await;
            if sent {
                break;
            }
        }
    }
    // A direct response may have advanced the generation after replacing an
    // active model turn. Always kick the scope drainer, including failed
    // sends, so a queue cannot be left behind when the transport rejects the
    // control response.
    drain_pending_window_messages_from_current(group_id, bot).await;
    sent
}

/// `#表情列表` / `#发表情 标签`：素材库命令的群聊实现。
///
/// 文字回执与图片表情走的是同一条直发链路：先把控制面这一轮赢下来（在途回复会被
/// 代替代），再按与回执相同的间隔有限重试；图片发不出去时如实回一句文字，绝不假装
/// 发过——管理员正是靠这条回执验收"她在 QQ 里真的能发表情包"。
async fn handle_sticker_library_command(
    bot: &Arc<RuntimeBot>,
    group_id: i64,
    admission: IncomingAdmission,
    command: crate::sticker_library::StickerLibraryCommand,
) {
    use crate::sticker_library::StickerLibraryCommand;
    let label = match command {
        StickerLibraryCommand::List => {
            send_group_direct_response(
                bot,
                group_id,
                admission,
                crate::sticker_library::library_listing_reply(),
            )
            .await;
            return;
        }
        StickerLibraryCommand::Invalid => {
            send_group_direct_response(
                bot,
                group_id,
                admission,
                crate::sticker_library::command_help(),
            )
            .await;
            return;
        }
        StickerLibraryCommand::Send { label } => label,
    };
    let Some(message) = crate::sticker_library::build_sticker_message(&label) else {
        send_group_direct_response(
            bot,
            group_id,
            admission,
            crate::sticker_library::missing_label_reply(&label),
        )
        .await;
        return;
    };
    let mut sent = false;
    if ConversationCoordinator::resolve_active_reply_for_direct_response(admission).await {
        for (attempt, delay_ms) in std::iter::once(0_u64)
            .chain(GROUP_DIRECT_RESPONSE_RETRY_DELAYS_MS.iter().copied())
            .enumerate()
        {
            if attempt > 0 {
                kovi::tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            sent = crate::model::tracked_send::send_tracked_message_with_revalidation(
                bot,
                crate::model::MessageDestination::Group(group_id),
                message.clone(),
                crate::model::interrupt::OutgoingSource::Reply,
                None,
                || async { true },
            )
            .await
            .is_ok();
            if sent {
                break;
            }
        }
    }
    drain_pending_window_messages_from_current(group_id, bot).await;
    if !sent {
        eprintln!(
            "[ERROR] 群聊表情包命令发送失败 (群组: {}, 标签: {})",
            group_id, label
        );
        send_group_direct_response(
            bot,
            group_id,
            admission,
            format!("“{label}”这张表情没能发出去，稍后再试。"),
        )
        .await;
    }
}

async fn drain_pending_window_messages_from_current(group_id: i64, bot: &Arc<RuntimeBot>) {
    let scope = ReplyScope::Group(group_id);
    if let Some(ticket) = ConversationCoordinator::current_ticket(scope).await {
        drain_pending_window_messages(group_id, Arc::clone(bot), ticket).await;
        return;
    }
    // 没有回复状态（例如本群数据刚被清除）时，队列留着只会永远排不空——
    // 那正是这次要根除的形态。宁可丢掉这几条待处理消息并留一行告警，也不
    // 要让这个群因为一口排不空的队列一直静音。
    let dropped = PENDING_WINDOW_MESSAGES
        .lock()
        .await
        .remove(&group_id)
        .map_or(0, |queue| queue.len());
    // 队列被丢掉，观测里的深度也必须跟着归零，否则后台会一直显示"还有人在等"。
    publish_group_queue_state(group_id).await;
    if dropped > 0 {
        println!(
            "[WARN] 群聊回复状态已不存在，丢弃 {} 条排队消息 (群组: {})",
            dropped, group_id
        );
    }
}

/// 队列非空的群（看门狗用）。
///
/// 这里老老实实等锁，不用 `try_lock`：等待是**看门狗**在等（聊天路径不受影响，
/// 这张表的锁只在入队/领取那几行代码里持有），而 `try_lock` 漏掉一次扫描的
/// 代价是"这个群继续沉默一个间隔"——那正是看门狗要防的事。
async fn pending_window_group_ids() -> Vec<i64> {
    let pending = PENDING_WINDOW_MESSAGES.lock().await;
    pending
        .iter()
        .filter(|(_, queue)| !queue.is_empty())
        .map(|(group_id, _)| *group_id)
        .collect()
}

/// 看门狗：把"队列非空却没人排空"的群补踢一次。
///
/// 排空只在回合收尾时触发，而 Core 链路收尾、panic、取消这三类路径都不走
/// 那里；这个扫描是最后一道保险，保证 waiting room 不会烂在内存里（间隔见
/// `traffic.window_drain_sweep_secs`）。真要不要领取由协调器判定：会话忙时
/// `claim_follow_up_locked` 直接拒绝、队列原样保留，所以重复踢是安全的。
pub(crate) async fn sweep_group_window_queues(bot: &Arc<RuntimeBot>) {
    for group_id in pending_window_group_ids().await {
        // 看门狗**不等** pending admission：领不到就留给下一轮（30 秒后），
        // 整轮扫描必须很快返回。
        let scope = ReplyScope::Group(group_id);
        if let Some(ticket) = ConversationCoordinator::current_ticket(scope).await {
            drain_pending_window_messages_with(
                group_id,
                Arc::clone(bot),
                ticket,
                WindowDrainWait::Never,
            )
            .await;
        }
    }
}

async fn send_health_status(bot: &Arc<RuntimeBot>, group_id: i64) {
    let mut health_checker = HealthChecker::new(Arc::clone(&MEMORY_MANAGER));
    let health_status = health_checker.check_health().await;

    let status_msg = if health_status.is_healthy && health_status.warnings.is_empty() {
        format!(
            "✅ 系统健康状态良好\n📊 记忆数量: {}\n👥 用户档案: {}\n🏢 群组档案: {}\n💾 记忆快照大小: {:.2}MB",
            health_status.memory_usage.total_memories,
            health_status.memory_usage.user_profiles,
            health_status.memory_usage.group_profiles,
            health_status.memory_usage.storage_size_bytes as f64 / 1024.0 / 1024.0
        )
    } else if health_status.is_healthy {
        format!(
            "⚠️ 系统可以运行，但有警告\n{}\n📊 记忆数量: {}\n💾 记忆快照大小: {:.2}MB",
            health_status.warnings.join("\n"),
            health_status.memory_usage.total_memories,
            health_status.memory_usage.storage_size_bytes as f64 / 1024.0 / 1024.0,
        )
    } else {
        format!(
            "❌ 系统健康状态异常\n错误: {}\n警告: {}",
            health_status.errors.join(", "),
            health_status.warnings.join(", ")
        )
    };

    send_tracked_group_message(bot, group_id, status_msg).await;
}

async fn delete_group_data(group_id: i64, bot: &RuntimeBot) {
    let scope = ReplyScope::Group(group_id);
    {
        let scope_lock = scope_mutex(scope);
        let _scope_guard = scope_lock.lock().await;
        ConversationCoordinator::interrupt_locked(scope).await;
        GROUP_MESSAGE_BATCHES
            .cancel_where(|(candidate_group_id, _)| *candidate_group_id == group_id)
            .await;
        PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
        publish_group_queue_state(group_id).await;
        clear_group_erasure_reply_state_locked(scope).await;
    }
    GROUP_INTERJECTION_STATE.lock().await.remove(&group_id);
    DIRECT_TRIGGER_STATES
        .lock()
        .await
        .retain(|(candidate_group_id, _), _| *candidate_group_id != group_id);
    clear_group_runtime_data(group_id).await;
    clear_reply_targets(scope).await;
    clear_group_pending_image_requests(group_id).await;
    // 群级降温是"这个群"的派生状态，删除群数据时必须一起清掉：否则换个
    // 用途重新开始，她还带着上一个群留下的冷场。删不掉只告警——它不该
    // 拖住真正的数据删除。
    if let Some(store) = crate::yunxi::group_cooling_store()
        && let Err(error) = store.delete_group(group_id).await
    {
        eprintln!("[WARN] 清除群级降温压力失败 (群组: {group_id}): {error}");
    }

    let core_erasure = match crate::yunxi::begin_qq_group_data_erasure(group_id).await {
        Ok(erasure) => erasure,
        Err(error) => {
            eprintln!(
                "[ERROR] 无法建立群 Core 数据删除屏障 (群组: {}): {}",
                group_id, error
            );
            send_group_erasure_receipt(
                bot,
                group_id,
                "群数据删除未开始：Core 删除屏障不可用，请稍后重试或让管理员检查日志。",
            )
            .await;
            return;
        }
    };
    let mind_conversation_ids = core_erasure.conversation_ids().to_vec();

    // Preserve the canonical conversation mapping until Mind deletion has
    // succeeded, otherwise a restart cannot reliably retry a failed erase.
    let mind_erasure = match crate::yunxi::delete_mind_conversation_data(&mind_conversation_ids)
        .await
    {
        Ok(erasure) => erasure,
        Err(error) => {
            eprintln!(
                "[ERROR] 群 Mind 数据删除失败，保留入口屏障 (群组: {}): {}",
                group_id, error
            );
            send_group_erasure_receipt(
                bot,
                group_id,
                "群 Mind 数据删除未完成；为防止数据被重新写入，当前入口已保持关闭，请让管理员检查日志后重启并重试。",
            )
            .await;
            return;
        }
    };

    let memory_result = MEMORY_MANAGER.delete_group_data(group_id).await;
    let sticker_result = sticker_memory::delete_group_data(group_id).await;
    let reminder_result = reminders::delete_group_data(group_id).await;
    let agent_goal_result = crate::agent_runtime::delete_group_data(group_id).await;
    // Core deletion acquires the cross-process memory barrier. Run a second
    // idempotent legacy pass after it to remove any projection that completed
    // in the gap between the first cleanup and the owner purge.
    let core_result = crate::yunxi::delete_qq_group_domain_data(group_id).await;
    let final_memory_result = MEMORY_MANAGER.delete_group_data(group_id).await;
    match (
        memory_result,
        sticker_result,
        reminder_result,
        agent_goal_result,
        core_result,
        final_memory_result,
    ) {
        (
            Ok(memory_rows),
            Ok(sticker_rows),
            Ok(reminder_rows),
            Ok(agent_goal_rows),
            Ok(core_rows),
            Ok(final_memory_rows),
        ) => match core_erasure.finish().await {
            Ok(()) => {
                if let Some(mind_erasure) = mind_erasure {
                    mind_erasure.finish().await;
                }
                send_group_erasure_receipt(
                        bot,
                        group_id,
                        format!(
                            "本群可归属数据已删除（记忆/档案/摘要 {} 项，表情记忆 {sticker_rows} 项，提醒 {reminder_rows} 项，角色目标 {agent_goal_rows} 项，Core 数据 {core_rows} 项）。",
                            memory_rows.saturating_add(final_memory_rows)
                        ),
                    )
                    .await;
            }
            Err(error) => {
                eprintln!(
                    "[ERROR] 群数据已删除但屏障恢复失败，继续保持关闭 (群组: {}): {}",
                    group_id, error
                );
                send_group_erasure_receipt(
                        bot,
                        group_id,
                        "群数据已删除但安全屏障未能恢复；当前入口继续保持关闭，请让管理员检查日志后重启。",
                    )
                    .await;
            }
        },
        (memory, stickers, reminders, agent_goals, core, final_memory) => {
            eprintln!(
                "[ERROR] 群数据删除未完全成功，保留安全屏障 (群组: {}, 记忆: {:?}, 表情: {:?}, 提醒: {:?}, 角色目标: {:?}, Core: {:?}, 记忆二次清理: {:?})",
                group_id, memory, stickers, reminders, agent_goals, core, final_memory
            );
            send_group_erasure_receipt(
                bot,
                group_id,
                "群数据删除没有全部完成；为防止数据被重新写入，当前入口已保持关闭，请让管理员检查日志后重启并重试。",
            )
            .await;
        }
    }
}

async fn clear_group_erasure_reply_state_locked(scope: ReplyScope) {
    clear_reply_state_locked(scope).await;
    clear_reply_scope_locked(scope).await;
}

const fn group_erasure_receipt_destination(group_id: i64) -> crate::model::MessageDestination {
    crate::model::MessageDestination::Group(group_id)
}

async fn send_group_erasure_receipt(
    bot: &RuntimeBot,
    group_id: i64,
    content: impl Into<String>,
) -> bool {
    match crate::model::send_tracked_unrecorded_plain_text(
        bot,
        group_erasure_receipt_destination(group_id),
        content.into(),
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            eprintln!(
                "[ERROR] 群数据删除回执发送失败 (群组: {}): {}",
                group_id, error
            );
            false
        }
    }
}

async fn should_suppress_direct_trigger(group_id: i64, user_id: i64) -> bool {
    let limits = config::get().group_interjection().clone();
    let now = Instant::now();
    let local_suppressed = {
        let mut states = DIRECT_TRIGGER_STATES.lock().await;
        if states.len() > 2_048 {
            let retention =
                Duration::from_secs(limits.direct_spam_cooldown_secs().saturating_mul(2));
            states.retain(|_, state| {
                state
                    .last_seen
                    .is_some_and(|last_seen| now.duration_since(last_seen) < retention)
            });
        }
        let state = states.entry((group_id, user_id)).or_default();
        suppress_direct_trigger(
            state,
            now,
            Duration::from_secs(limits.direct_spam_cooldown_secs()),
            Duration::from_secs(limits.direct_rate_window_secs()),
            limits.direct_rate_limit(),
        )
    };
    if local_suppressed {
        return true;
    }

    let Some(store) = redis_store::get().await else {
        return false;
    };
    let rate_window = Duration::from_secs(limits.direct_rate_window_secs());
    let rate_key = format!("rate:direct-trigger:group:{group_id}:user:{user_id}");
    match store.increment_expiring(&rate_key, rate_window).await {
        Ok(count) if count > limits.direct_rate_limit() as i64 => {
            let mut states = DIRECT_TRIGGER_STATES.lock().await;
            if let Some(state) = states.get_mut(&(group_id, user_id)) {
                state.blocked_until =
                    Some(Instant::now() + Duration::from_secs(limits.direct_spam_cooldown_secs()));
            }
            true
        }
        Ok(_) => false,
        Err(error) => {
            eprintln!("[WARN] Redis 直接点名限流失败，继续使用本地限流: {}", error);
            false
        }
    }
}

fn suppress_direct_trigger(
    state: &mut DirectTriggerState,
    now: Instant,
    cooldown: Duration,
    rate_window: Duration,
    rate_limit: usize,
) -> bool {
    state.last_seen = Some(now);
    if state.blocked_until.is_some_and(|until| until > now) {
        return true;
    }
    state.blocked_until = None;

    while state
        .recent_triggers
        .front()
        .is_some_and(|seen_at| now.duration_since(*seen_at) >= rate_window)
    {
        state.recent_triggers.pop_front();
    }
    state.recent_triggers.push_back(now);
    if state.recent_triggers.len() > rate_limit {
        state.blocked_until = Some(now + cooldown);
        return true;
    }
    false
}

async fn group_conversation_snapshot(group_id: i64) -> (bool, String) {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    state.conversation.prune();
    (
        conversation_active_for_observation(state, Instant::now()),
        state.conversation.context(),
    )
}

/// 会话是否处于“接续对话”激活状态：有正在处理的回复回合，或芸汐刚在本群
/// 发过可见消息（还在接续窗口内）。语义会话状态本身不随回复衰减，因此
/// 这里必须由“最近一次可见回复”的时间窗口来限定：窗口一过期，未点名消息
/// 回到低频抽样，避免“每句话都接”的永久高敏感模式。
fn conversation_active_for_observation(state: &GroupInterjectionState, now: Instant) -> bool {
    state.conversation.has_pending_turn()
        || state.last_bot_reply.is_some_and(|last| {
            now.saturating_duration_since(last) < Duration::from_secs(continuation_window_secs())
        })
}

/// 同步查询"群聊可见回复预算":当前是否还有名额（供 bridge 采样门在
/// 入队前快速判断）。这只是咨询,不消耗名额;正在处理中的并发写由
/// `reserve_group_chat_reply` 原子预留。与写锁竞争时按"还有名额"处理,
/// 避免把已入队的有效对话误杀。
pub(crate) fn group_reply_budget_available_now(group_id: i64) -> bool {
    let config = config::get().group_interjection().clone();
    match GROUP_INTERJECTION_STATE.try_lock() {
        Ok(states) => states.get(&group_id).is_none_or(|state| {
            let now = Instant::now();
            let rate_window = Duration::from_secs(config.reply_rate_window_secs());
            let within_window = state
                .visible_replies
                .iter()
                .filter(|slot| now.saturating_duration_since(slot.at) < rate_window)
                .count();
            if within_window >= config.reply_rate_limit() {
                return false;
            }
            state.visible_replies.back().is_none_or(|last| {
                now.saturating_duration_since(last.at)
                    >= Duration::from_secs(config.reply_gap_secs())
            })
        }),
        Err(_) => true,
    }
}

/// 标记芸汐在本群发出了一条可见消息。Host 回复与 Core 回复都经过
/// MessageTransport 发送，因此这里能统一刷新连续会话窗口。
pub(crate) async fn mark_group_reply_sent(group_id: i64) {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    let now = Instant::now();
    state.last_bot_reply = Some(now);
    confirm_visible_reply_slot(state, now);
}

/// 她刚刚在群里可见回复了 `user_id`：把对话焦点记成这个人。
///
/// **时机是"计划出可见回复"而不是"平台受理发送"**：Core 的计划在投递之前
/// 产生，这里用的是计划时已知的发言者。乐观的代价有界——万一那条回复最终
/// 没发出去，焦点也只会让同一个人的后续消息按接续处理一次，TTL 到期自然
/// 结束，不会误伤别人（焦点一被别人的发言打断就没了）。
pub(crate) async fn note_group_conversation_focus(group_id: i64, user_id: i64, continuation: bool) {
    if !config::get().group_interjection().continuation_enabled() {
        return;
    }
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    // 计数口径：同一段对话里"接续回复"累加；她**被点名**回的一轮重置为 1
    // （对方重新 @ 一次就是重新开一段对话）；换了对象则整段重来。
    // 这个数只喂给模型参考，不做闸门。
    let replies = match state.conversation_focus {
        Some(focus) if focus.user_id == user_id && continuation => focus.replies.saturating_add(1),
        _ => 1,
    };
    state.conversation_focus = Some(GroupConversationFocus {
        user_id,
        since: Instant::now(),
        replies,
    });
}

/// 同步查询对话焦点，供采样门这类同步判定使用；抢不到锁时按"没有焦点"处理。
pub(crate) fn group_conversation_focus_user_now(group_id: i64, speaker_user_id: i64) -> bool {
    if !config::get().group_interjection().continuation_enabled() {
        return false;
    }
    let ttl = Duration::from_secs(
        config::get()
            .group_interjection()
            .continuation_focus_ttl_secs()
            .max(1),
    );
    match GROUP_INTERJECTION_STATE.try_lock() {
        Ok(states) => states
            .get(&group_id)
            .and_then(|state| state.focus_user_at(Instant::now(), ttl))
            .is_some_and(|user_id| user_id == speaker_user_id),
        Err(_) => false,
    }
}

/// 这段对话里她已经回了多少句（含回复点名消息的那一轮）。
///
/// 只作为**事实**喂给模型（"你已经回了 N 句"），让它自己判断还要不要继续说；
/// 宿主不拿它做闸门。拿不到锁时按 0 处理：宁可少一句提示，也不阻塞热路径。
pub(crate) fn group_conversation_focus_state_now(group_id: i64, speaker_user_id: i64) -> u32 {
    match GROUP_INTERJECTION_STATE.try_lock() {
        Ok(states) => states
            .get(&group_id)
            .and_then(|state| state.conversation_focus)
            .filter(|focus| focus.user_id == speaker_user_id)
            .map_or(0, |focus| focus.replies),
        Err(_) => 0,
    }
}

/// 同步查询"接续对话"窗口（同步调用路径上的 bridge 采样门使用）。
/// 与写锁竞争时保持严格语义（按窗口外处理），宁可少采样也不误放行。
pub(crate) fn conversation_continuation_active_now(group_id: i64) -> bool {
    match GROUP_INTERJECTION_STATE.try_lock() {
        Ok(states) => states.get(&group_id).is_some_and(|state| {
            state.last_bot_reply.is_some_and(|last| {
                last.elapsed() < Duration::from_secs(continuation_window_secs())
            })
        }),
        Err(_) => false,
    }
}

/// 为本群预留一次"群聊可见回复"的名额（硬节奏控制，点名/未点名共用）。
///
/// 接续对话有语义入口但没有冷却，模型在窗口内几乎"每句都接"，是刷屏的
/// 主通道；插话路径虽有抽样冷却，这里统一按群施加确定性的回复
/// 间隔 + 频率上限：预留成功才允许生成可见回复，被拒时本条只作观察。
/// 预留是乐观的（模型可能最终沉默），方向只保守不激进。
///
/// `gap` 由调用方给出：普通回复用 `reply_gap_secs`，被明确点名的消息用
/// 更短的 `addressed_reply_gap_secs`，管理员发言用 0（管理员的话是明确
/// 指令，间隔不该把指令挡在门外）。频率上限（`reply_rate_limit` /
/// `reply_rate_window_secs`）对所有人一致，所以放松的只是"等待"，不是额度。
///
/// 返回预留时刻（token）：真发出可见消息时由 [`mark_group_reply_sent`]
/// 确认，回合判沉默时由调用方归还（[`release_group_reply_slot`]）。
pub(crate) async fn reserve_group_chat_reply_slot(
    group_id: i64,
    gap_secs: u64,
    class: ReplyBudgetClass,
) -> Option<Instant> {
    let limits = ReplyBudgetLimits::from_config();
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    let now = Instant::now();
    match reserve_visible_reply_slot(state, now, Duration::from_secs(gap_secs), limits, class) {
        ReplySlotReservation::Reserved => Some(now),
        ReplySlotReservation::ReservedOverBudget => {
            println!(
                "[INFO] 群聊回复额度已满但放行 (群组: {}, 类别={:?})",
                group_id, class
            );
            Some(now)
        }
        ReplySlotReservation::Rejected => None,
    }
}

/// 归还一格**未确认**的预留：这一轮最终没有发出可见消息。
pub(crate) async fn release_group_reply_slot(group_id: i64, at: Instant) {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return;
    };
    release_unconfirmed_slot(state, at);
}

/// 纯函数：按预留时刻精确归还那一格。已确认（真的发出去过）的名额不还，
/// 别人的预留也不会被误伤。
fn release_unconfirmed_slot(state: &mut GroupInterjectionState, at: Instant) {
    state
        .visible_replies
        .retain(|slot| slot.at != at || slot.confirmed);
}

/// 排空 waiting room 时的等待策略。
///
/// 这个区别在线上踩出来过（2026-09-14 19:14）：看门狗那一轮扫描去等一个还没
/// 解决的 pending admission，等满 60 秒触发超时告警，还把这一轮扫描整段取消
/// ——而 30 秒后它本来就会再来一次。等待只属于"回合收尾"的语义。
///
/// 群聊与私聊共用这一对取值：私聊看门狗原先没有这个开关，走的是请求路径那套
/// 排空逻辑，单个残留 admission 就能让整轮扫描卡到租约上限（180 秒），本轮其他
/// 私聊用户一起被跳过。
///
/// 群聊与私聊共用这一对取值：私聊看门狗原先没有这个开关，走的是请求路径那套
/// 排空逻辑，单个残留 admission 就能让整轮扫描卡到租约上限（180 秒），本轮其他
/// 私聊用户一起被跳过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowDrainWait {
    /// 等到在途 admission 解决再领队：回合收尾用，后面的消息要按序接上。
    ForPendingAdmission,
    /// 领不到就走：看门狗用，下一轮马上还会来，绝不能让整轮扫描卡住。
    Never,
}

/// 回合结束时的收尾：这一轮最终**没有**发出可见消息，就把乐观预留的
/// 名额还回去。`replied` 来自回复管线的真实结果（不是"打算回"）。
async fn release_unconfirmed_reply_slot(group_id: i64, slot: Option<Instant>, replied: bool) {
    if let Some(at) = slot
        && !replied
    {
        release_group_reply_slot(group_id, at).await;
    }
}

/// 确认"这条群消息真的发出去了"：名额从预留转为已用。
fn confirm_visible_reply_slot(state: &mut GroupInterjectionState, now: Instant) {
    if let Some(slot) = state
        .visible_replies
        .iter_mut()
        .rev()
        .find(|slot| !slot.confirmed && slot.at <= now)
    {
        slot.confirmed = true;
    }
}

/// 诊断用：本群回复节奏的当前状态快照（间隔毫秒 + 窗口内条数 + 上限）。
/// 只在拒绝路径上读取，用来把"为什么这条没回"写进日志。`gap_secs` 要传
/// 本次实际使用的间隔，否则日志会把被点名通道说成还差满 90 秒。
pub(crate) struct GroupReplyBudgetSnapshot {
    pub(crate) gap_remaining_ms: Option<u64>,
    pub(crate) replies_in_window: usize,
    pub(crate) rate_limit: usize,
}

pub(crate) async fn group_reply_budget_snapshot(
    group_id: i64,
    gap_secs: u64,
    class: ReplyBudgetClass,
) -> GroupReplyBudgetSnapshot {
    let limits = ReplyBudgetLimits::from_config();
    let gap = Duration::from_secs(gap_secs);
    let rate_window = limits.rate_window;
    // 报"这次这一类"的账：点名看全部（点名额度是更宽的那份总闸），未点名只看
    // 未点名的条数。这样日志里的 `replies_in_window/rate_limit` 与拒绝判据一致。
    let rate_limit = if class.is_addressed() {
        limits.addressed_limit
    } else {
        limits.unaddressed_limit
    };
    let now = Instant::now();
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let Some(state) = states.get(&group_id) else {
        return GroupReplyBudgetSnapshot {
            gap_remaining_ms: None,
            replies_in_window: 0,
            rate_limit,
        };
    };
    let replies_in_window = state
        .visible_replies
        .iter()
        .filter(|slot| now.saturating_duration_since(slot.at) < rate_window)
        .filter(|slot| !class.is_addressed() || slot.addressed)
        .count();
    let gap_remaining_ms = state.visible_replies.back().and_then(|last| {
        let seen = now.saturating_duration_since(last.at);
        (seen < gap).then(|| (gap - seen).as_millis() as u64)
    });
    GroupReplyBudgetSnapshot {
        gap_remaining_ms,
        replies_in_window,
        rate_limit,
    }
}

/// 纯函数：在单群状态上执行"可见回复"名额预留。
///
/// 时间轴只记录**真正拿到名额**的回复：被拒绝的预留不写时间戳。否则一次
/// 被拒的尝试会把"上次回复"顶到当前时刻，紧接着的重试（比如稍后一条点名
/// 提问）会看到 0 秒间隔再次被拒，问题就被永久压在冷却里——现场正是这种
/// "问了两遍也没人理"的观感。频率上限与间隔都只看真实回复。
/// 这次可见回复属于哪一类额度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyBudgetClass {
    /// 未点名接话：普通额度。
    Ambient,
    /// 被点名/被引用/显式请求：更宽的那份额度。
    Addressed,
    /// 管理员点名的可见回复：额度豁免。
    ///
    /// 为什么单独开一档：线上 2026-09-14 21:19 管理员 @ 她问话被静默丢掉
    /// （`replies_in_window=10/10`、`gap_secs=0`）——等待间隔早就豁免了，额度没有，
    /// 于是"你直接问她"在热闹时段一样会被吞掉。管理员的话是明确指令；防"她句句
    /// 都回"靠的是**未点名接话**那份额度与自动接话抽样，与这一档无关。
    AddressedUncapped,
}

impl ReplyBudgetClass {
    /// 记账时算不算"点名"（`AddressedUncapped` 也是点名回复，只是额度豁免）。
    const fn is_addressed(self) -> bool {
        matches!(self, Self::Addressed | Self::AddressedUncapped)
    }

    /// 额度用完时是否仍然放行。
    const fn bypasses_rate_limit(self) -> bool {
        matches!(self, Self::AddressedUncapped)
    }
}

/// 由"是不是点名"和"是不是管理员"决定走哪一档额度。
pub(crate) const fn reply_budget_class(addressed: bool, sender_is_admin: bool) -> ReplyBudgetClass {
    match (addressed, sender_is_admin) {
        (true, true) => ReplyBudgetClass::AddressedUncapped,
        (true, false) => ReplyBudgetClass::Addressed,
        (false, _) => ReplyBudgetClass::Ambient,
    }
}

/// 一次预留的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplySlotReservation {
    /// 正常拿到名额。
    Reserved,
    /// 额度已经用完，但这一档豁免：放行，并照实记账（调用方会留一行日志）。
    ReservedOverBudget,
    /// 被拒。
    Rejected,
}

/// 一次预留要用到的窗口额度：总窗口 + 未点名额度 + 点名额度。
///
/// 打包成一个结构而不是继续加参数：这些值同源同变（都从配置来、都在同一个判据里
/// 比较），散成四个位置参数最容易在调用点上写反。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplyBudgetLimits {
    pub(crate) rate_window: Duration,
    pub(crate) unaddressed_limit: usize,
    pub(crate) addressed_limit: usize,
}

impl ReplyBudgetLimits {
    fn from_config() -> Self {
        let config = config::get();
        let group = config.group_interjection();
        Self {
            rate_window: Duration::from_secs(group.reply_rate_window_secs()),
            unaddressed_limit: group.reply_rate_limit(),
            addressed_limit: group.effective_addressed_reply_rate_limit(),
        }
    }
}

fn reserve_visible_reply_slot(
    state: &mut GroupInterjectionState,
    now: Instant,
    gap: Duration,
    limits: ReplyBudgetLimits,
    class: ReplyBudgetClass,
) -> ReplySlotReservation {
    while state
        .visible_replies
        .front()
        .is_some_and(|slot| now.saturating_duration_since(slot.at) >= limits.rate_window)
    {
        state.visible_replies.pop_front();
    }
    // 兜底：既没被确认、也没被显式归还的预留（调用方早退/取消/panic 残余）
    // 不该长期占位——否则一次异常就能把这个群的回复节奏锁死。
    state.visible_replies.retain(|slot| {
        slot.confirmed || now.saturating_duration_since(slot.at) < UNCONFIRMED_REPLY_SLOT_TTL
    });
    // 窗口外的槽已经在上面清掉了，所以这里 `len()` 就是窗口内的总数。
    let in_window = state.visible_replies.len();
    let addressed_in_window = state
        .visible_replies
        .iter()
        .filter(|slot| slot.addressed)
        .count();
    let unaddressed_in_window = in_window.saturating_sub(addressed_in_window);
    // 点名与未点名分开记账：未点名撞的是普通额度，点名撞的是更宽的那份额度；
    // 两者都受后者封顶，避免"手里全是点名"时无上限。
    let within_budget = if class.is_addressed() {
        in_window < limits.addressed_limit
    } else {
        unaddressed_in_window < limits.unaddressed_limit && in_window < limits.addressed_limit
    };
    // 额度和间隔是两件事：豁免额度的那一档仍然要等间隔（管理员点名那一档的间隔
    // 由调用方按 0 秒传入，所以实际不等）。
    if state
        .visible_replies
        .back()
        .is_some_and(|last| now.saturating_duration_since(last.at) < gap)
    {
        return ReplySlotReservation::Rejected;
    }
    if !within_budget && !class.bypasses_rate_limit() {
        return ReplySlotReservation::Rejected;
    }
    // 豁免档超额度也照实记账：账本必须反映"她真的说了这么多"，否则下一个人的
    // 额度和日志都会失真。
    state.visible_replies.push_back(VisibleReplySlot {
        at: now,
        confirmed: false,
        addressed: class.is_addressed(),
    });
    if within_budget {
        ReplySlotReservation::Reserved
    } else {
        ReplySlotReservation::ReservedOverBudget
    }
}

/// 群聊可见回复要等的间隔：管理员按 0 秒处理（他的话是明确指令，"刚回过
/// 别人"不该把它挡在门外），被点名/被引用的消息用更短的点名档，其余用未点名
/// 的防刷屏档。频率上限 `reply_rate_limit` 对所有人一致——豁免的只是等待。
fn paced_group_reply_gap_secs(addressed: bool, sender_is_admin: bool) -> u64 {
    if sender_is_admin {
        return 0;
    }
    let config = config::get();
    let group = config.group_interjection();
    if addressed {
        group.effective_addressed_reply_gap_secs()
    } else {
        group.reply_gap_secs()
    }
}

async fn observe_group_conversation(
    group_id: i64,
    user_id: i64,
    understanding: &crate::model::semantic::MessageUnderstanding,
    direct_reply_expected: bool,
) -> ConversationDecision {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    state
        .conversation
        .observe(user_id, understanding, direct_reply_expected)
}

/// Mark a real reply as a pending conversation turn. The options are derived
/// while the turn is claimed so queued messages use the state that actually
/// precedes their reply.
async fn begin_conversation_turn(
    group_id: i64,
    user_id: i64,
    understanding: &crate::model::semantic::MessageUnderstanding,
) -> u64 {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    prune_interjection_states(&mut states);
    let state = states.entry(group_id).or_default();
    let options = state
        .conversation
        .observe(user_id, understanding, true)
        .turn;
    state
        .conversation
        .begin_turn(user_id, options, &understanding.topics)
}

async fn finish_conversation_turn(
    group_id: i64,
    user_id: i64,
    turn_generation: u64,
    replied: bool,
) {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return;
    };
    state
        .conversation
        .finish_turn(user_id, turn_generation, replied);
}

async fn admit_understood_group_turn(
    initial: IncomingAdmission,
    understanding: &MessageUnderstanding,
    direct_reply_expected: bool,
    carries_no_text: bool,
) -> Option<IncomingAdmission> {
    ConversationCoordinator::refine_current_incoming(
        initial,
        ConversationCoordinator::context_for_understood_turn(
            understanding,
            direct_reply_expected,
            carries_no_text,
        ),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn queue_pending_window_message(
    group_id: i64,
    user_id: i64,
    reply_expected: bool,
    sender: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    sticker_teaching_message: Option<Message>,
    understanding: MessageUnderstanding,
) {
    let mut pending = PENDING_WINDOW_MESSAGES.lock().await;
    let queue = pending.entry(group_id).or_default();
    ConversationCoordinator::enqueue(
        queue,
        PendingWindowMessage {
            user_id,
            sender,
            message,
            reply_expected,
            folded: Vec::new(),
            vision_images,
            message_ids,
            sticker_teaching_message,
            understanding,
            enqueued_at: Instant::now(),
        },
        "群聊",
        group_id,
    );
    waiting_room::record_queue(
        ReplyScope::Group(group_id),
        &QueueView {
            queued: queue.len(),
            oldest_enqueued_at: queue.front().map(|turn| turn.enqueued_at),
        },
        queue
            .front()
            .map(|turn| (turn.sender.as_str(), turn.message.as_str())),
    );
}

/// 把 waiting room 的归属结果落到"这一轮到底要不要生成回复"上。
///
/// `QueuedNeedsDrain` 是**队列残局**：队列非空却没有任何在途工作（Core 链路
/// 收尾、panic、取消都可能是把它落下的原因）。这时刚入队的这条消息必须立刻
/// 排空，否则它会和队列一起烂在内存里，直到进程重启——线上 2026-09-14 18:33
/// 主群静了四分多钟就是这个状态（排队 8 次、排空 0 次）。
async fn settle_window_claim(
    claim: WindowClaim,
    group_id: i64,
    bot: &Arc<RuntimeBot>,
) -> Option<crate::model::ReplyTicket> {
    match claim {
        WindowClaim::Claimed(ticket) => Some(ticket),
        WindowClaim::Queued => None,
        WindowClaim::QueuedNeedsDrain => {
            drain_pending_window_messages_from_current(group_id, bot).await;
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn claim_or_queue_group_reply(
    scope: ReplyScope,
    admission: IncomingAdmission,
    reply_expected: bool,
    group_id: i64,
    user_id: i64,
    sender: String,
    message: String,
    vision_images: Vec<VisionImage>,
    message_ids: Vec<i32>,
    sticker_teaching_message: Option<Message>,
    understanding: MessageUnderstanding,
) -> WindowClaim {
    let scope_lock = scope_mutex(scope);
    let _scope_guard = scope_lock.lock().await;
    let active = ConversationCoordinator::is_active_locked(scope).await;
    let has_queued = PENDING_WINDOW_MESSAGES
        .lock()
        .await
        .get(&group_id)
        .is_some_and(|queue| !queue.is_empty());
    let has_pending_admission =
        ConversationCoordinator::has_other_pending_incoming_locked(admission).await;
    let queue_decision = window_queue_decision(
        active,
        has_queued,
        has_pending_admission,
        admission.decision,
        admission.preserved_prepared,
    );
    if queue_decision != WindowQueueDecision::Process {
        println!(
            "[INFO] 群聊已有回复或排队消息进行中，排队窗口消息 (群组: {}, 用户: {}, 立刻排空: {})",
            group_id,
            user_id,
            queue_decision == WindowQueueDecision::QueueThenDrain,
        );
        queue_pending_window_message(
            group_id,
            user_id,
            reply_expected,
            sender,
            message,
            vision_images,
            message_ids,
            sticker_teaching_message,
            understanding,
        )
        .await;
        // The payload now lives in the FIFO; release this admission's own
        // coordinator reservation so it cannot block the next turn.
        ConversationCoordinator::abandon_incoming_locked(admission).await;
        return match queue_decision {
            WindowQueueDecision::QueueThenDrain => WindowClaim::QueuedNeedsDrain,
            _ => WindowClaim::Queued,
        };
    }
    let ticket = admission.ticket;
    if ConversationCoordinator::begin_reply_locked(scope, ticket, message_ids.clone()).await {
        WindowClaim::Claimed(ticket)
    } else {
        // A newer semantic hand-off may have won between refinement and this
        // claim. Keep the complete turn for the FIFO instead of dropping it.
        // 走到这里说明刚有人抢到了回合（`begin_reply_locked` 只有在别人活跃、
        // 或这批源消息已被撤回时才失败），交给那个回合收尾时排空即可。
        queue_pending_window_message(
            group_id,
            user_id,
            reply_expected,
            sender,
            message,
            vision_images,
            message_ids,
            sticker_teaching_message,
            understanding,
        )
        .await;
        ConversationCoordinator::abandon_incoming_locked(admission).await;
        WindowClaim::Queued
    }
}

async fn stop_group_reply(group_id: i64, user_id: i64, ingress: ReplyTicket) {
    let scope = ReplyScope::Group(group_id);
    let scope_lock = scope_mutex(scope);
    let _scope_guard = scope_lock.lock().await;
    let _ = ConversationCoordinator::cancel_current_incoming_locked(ingress).await;
    GROUP_MESSAGE_BATCHES.cancel((group_id, user_id)).await;
    PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
    publish_group_queue_state(group_id).await;
    if let Some(state) = GROUP_INTERJECTION_STATE.lock().await.get_mut(&group_id) {
        state.conversation.close();
    }
}

async fn drain_pending_window_messages(
    group_id: i64,
    bot: Arc<RuntimeBot>,
    completed: crate::model::ReplyTicket,
) {
    drain_pending_window_messages_with(
        group_id,
        bot,
        completed,
        WindowDrainWait::ForPendingAdmission,
    )
    .await;
}

async fn drain_pending_window_messages_with(
    group_id: i64,
    bot: Arc<RuntimeBot>,
    mut completed: crate::model::ReplyTicket,
    wait: WindowDrainWait,
) {
    let scope = ReplyScope::Group(group_id);
    // 活性记账：卡死时后台要能看出"排空在不在跑、多久没推进"（见 waiting_room）。
    // guard 的 Drop 保证取消/panic 路径也会把 active 落回去，不会留下假活性。
    let guard = DrainGuard::begin(scope);
    let mut drained = 0_usize;
    loop {
        let Some((pending, ticket)) = take_pending_window_turn(group_id, completed, wait).await
        else {
            if drained > 0 {
                println!(
                    "[INFO] 群聊排队窗口已排空 (群组: {}, 本轮处理 {} 条)",
                    group_id, drained
                );
            }
            publish_group_queue_state(group_id).await;
            drop(guard);
            return;
        };
        drained += 1;
        guard.note_progress();

        println!("[INFO] 群聊开始处理排队窗口消息 (群组: {})", group_id);
        // 回合观测：从这一刻起，"做到哪一步"记进台账，卡住时日志与后台能指名道姓。
        // 用 task-local 传播而不是给链上十来个函数加参数（同 `llm_trace` 的用途标签）；
        // 这条链全程在同一个任务里，不跨 `spawn`。
        let watch = waiting_room::TurnWatch::observe(
            scope,
            Some(&format!("{}: {}", pending.sender, pending.message)),
        );
        watch
            .enter(async {
                waiting_room::TurnWatch::step(waiting_room::TurnStep::BeginTurn);
                let turn_marker =
                    begin_conversation_turn(group_id, pending.user_id, &pending.understanding)
                        .await;
                let replied = crate::model::utils::process_group_reply_claimed(
                    group_id,
                    pending.user_id,
                    &pending.message,
                    bot.clone(),
                    pending.sender,
                    &pending.folded,
                    ticket,
                    None,
                    pending.vision_images,
                    pending.message_ids,
                    pending.sticker_teaching_message,
                    pending.understanding,
                    pending.reply_expected,
                )
                .await;
                waiting_room::TurnWatch::step(waiting_room::TurnStep::Finish);
                finish_conversation_turn(group_id, pending.user_id, turn_marker, replied).await;
                replied
            })
            .await;
        drop(watch);
        publish_group_queue_state(group_id).await;
        completed = ticket;
    }
}

/// 把群聊等待房间的当前深度与最老一条摘进运行时观测。
///
/// 目的一是后台能直接看到"谁在等、等了多久"，二是它同时是**活性打点**：卡住时
/// 这个数字会一直停在上一次推进，正常时它每处理一条就往前走一次。
async fn publish_group_queue_state(group_id: i64) {
    let pending = PENDING_WINDOW_MESSAGES.lock().await;
    let queue = pending.get(&group_id);
    waiting_room::record_queue(
        ReplyScope::Group(group_id),
        &QueueView {
            queued: queue.map_or(0, VecDeque::len),
            oldest_enqueued_at: queue
                .and_then(|queue| queue.front())
                .map(|turn| turn.enqueued_at),
        },
        queue
            .and_then(|queue| queue.front())
            .map(|turn| (turn.sender.as_str(), turn.message.as_str())),
    );
}

async fn take_pending_window_turn(
    group_id: i64,
    mut completed: crate::model::ReplyTicket,
    wait: WindowDrainWait,
) -> Option<(PendingWindowMessage, crate::model::ReplyTicket)> {
    let scope = ReplyScope::Group(group_id);
    loop {
        let (result, should_wait) = {
            let scope_lock = scope_mutex(scope);
            let _scope_guard = scope_lock.lock().await;
            let mut pending_by_group = PENDING_WINDOW_MESSAGES.lock().await;
            let queue = pending_by_group.entry(group_id).or_default();
            // 先丢掉"已经被回答过"的 turn：同一条消息可能先被 Core 链路答了，
            // 又被 Host 链路排进这里（两条链路都会看到同一条入站消息），不再
            // 检查一次就会答第二遍。判据见 `source_messages_already_answered`
            // ——它只认真的发出过消息的轮次，"想过但沉默"的不会误判。
            //
            // 锁序：会话锁 → PENDING_WINDOW_MESSAGES → REPLY_LIFECYCLES。反向
            // 不存在（recall 模块不认识 waiting room），所以不会死锁。
            while let Some(oldest) = queue.front() {
                let answered =
                    crate::model::source_messages_already_answered(scope, &oldest.message_ids)
                        .await;
                if !answered {
                    break;
                }
                let Some(dropped) = queue.pop_front() else {
                    break;
                };
                println!(
                    "[INFO] 群聊排队消息已被回复覆盖，丢弃 (群组: {}, 用户: {}, 消息: {:?})",
                    group_id, dropped.user_id, dropped.message_ids
                );
            }
            let result =
                ConversationCoordinator::claim_next_locked(scope, &mut completed, queue).await;
            let should_wait = result.is_none()
                && !queue.is_empty()
                && ConversationCoordinator::pending_incoming_for_ticket_locked(completed).await;
            if queue.is_empty() {
                pending_by_group.remove(&group_id);
            } else if result.is_some() {
                // 领走一条之后立刻刷一次深度：活性的心跳点就落在这里（`note_progress`），
                // 观测里的"排队还剩几条"与"最老一条等了多久"因此每处理一条都往前走一次。
                waiting_room::record_queue(
                    scope,
                    &QueueView {
                        queued: queue.len(),
                        oldest_enqueued_at: queue.front().map(|turn| turn.enqueued_at),
                    },
                    queue
                        .front()
                        .map(|turn| (turn.sender.as_str(), turn.message.as_str())),
                );
            }
            (result, should_wait)
        };
        if let Some(result) = result {
            return Some(result);
        }
        if !should_wait || wait == WindowDrainWait::Never {
            return None;
        }
        if !ConversationCoordinator::wait_for_pending_incoming(completed).await {
            return None;
        }
    }
}

/// 只用消息长度、计数、额度和概率决定是否值得调用一次语义模型。
///
/// 群级降温（[`group_cooling_verdict`]）也在这里生效：未点名抽样是这条通道
/// 唯一的行为出口，命中时**放弃这一次机会**（下一次要再等一批候选消息），
/// 所以它降低的是她主动插话的频率，而不是给她一个"不许说话"的状态。
///
/// 实现上刻意分成两段：本地判据全在锁内一次做完，需要读数据库的降温判据
/// 放在锁外——`GROUP_INTERJECTION_STATE` 是所有群路径共用的锁，不能压在
/// 一次 PG 往返上。两段之间靠先占住 `interjection_in_flight` 保证并发消息
/// 不会各抽一次。
async fn reserve_interjection_decision(
    group_id: i64,
    message: &str,
) -> Option<InterjectionAttempt> {
    let config = config::get().group_interjection().clone();
    if !config.enabled() || !has_interjection_candidate(message, config.min_message_chars()) {
        return None;
    }

    {
        let mut states = GROUP_INTERJECTION_STATE.lock().await;
        prune_interjection_states(&mut states);
        let state = states.entry(group_id).or_default();
        let now = Instant::now();
        prune_decision_attempts(
            state,
            now,
            Duration::from_secs(config.decision_rate_window_secs()),
        );
        if state.interjection_in_flight {
            return None;
        }
        if state.last_interjection.is_some_and(|last| {
            now.duration_since(last) < Duration::from_secs(config.cooldown_secs())
        }) {
            return None;
        }

        state.eligible_messages_since_sample =
            state.eligible_messages_since_sample.saturating_add(1);
        if state.eligible_messages_since_sample < config.min_eligible_messages() {
            return None;
        }
        if !decision_budget_available(
            state,
            now,
            Duration::from_secs(config.decision_cooldown_secs()),
            config.decision_rate_limit(),
        ) {
            // 保留已累计的候选；额度恢复后下一条有效消息即可再次抽样。
            state.eligible_messages_since_sample = config.min_eligible_messages();
            return None;
        }
        // 每积累一批候选消息才抽样一次；未抽中也重新累计，避免逐条消耗 token。
        state.eligible_messages_since_sample = 0;
        // 先占住这一轮尝试：下面要放开锁去读群级压力。
        state.interjection_in_flight = true;
    }
    // 从这一刻起"有插话在途"就挂在这个凭据上：调用方要么显式 complete，要么在
    // 任何一条早退路径上由 Drop 兜底解掉。原先靠调用方记得手写 finish，漏一条
    // 这个群就永久停在"有插话在途"，再也采样不到（prune 又刻意保留在途项）。
    let attempt = InterjectionAttempt {
        group_id,
        completed: false,
    };

    if interjection_sampling_vetoed(group_cooling_gate(group_id).await) {
        attempt.complete(false).await;
        return None;
    }
    if !rand::rng().random_ratio(config.response_probability_percent().into(), 100) {
        attempt.complete(false).await;
        return None;
    }

    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let state = states.entry(group_id).or_default();
    state.decision_attempts.push_back(Instant::now());
    Some(attempt)
}

/// 群降温命中时放弃这一次抽样机会——概率上本来会抽中也一样放弃。
///
/// 单独成函数是为了让"判定 → 处理"这条链在测试里能一眼看到：`Skip` 只可能
/// 来自 [`group_cooling_verdict`]，而它只在开关打开且压力过线时返回 `Skip`。
fn interjection_sampling_vetoed(cooling: GroupCoolingVerdict) -> bool {
    matches!(cooling, GroupCoolingVerdict::Skip { .. })
}

/// 群级降温判据的入口：取配置与压力（可能读一次 PG），交给纯判据。
///
/// 读不到压力（存储未初始化、或数据库抖动）时往下传 `None`——这是降频通道，
/// 失败方向必须是"照常说话"，绝不能因为一次读失败让她沉默。
async fn group_cooling_gate(group_id: i64) -> GroupCoolingVerdict {
    let enabled = config::get().silence().group_cooling_enabled();
    let Some(store) = crate::yunxi::group_cooling_store() else {
        return cooling_gate_verdict(group_id, None, enabled);
    };
    let pressure = match store.load(group_id).await {
        Ok(Some(state)) => Some(state.pressure),
        Ok(None) => Some(0.0),
        Err(error) => {
            eprintln!("[WARN] 读取群级降温压力失败 (群组: {group_id}): {error}");
            None
        }
    };
    cooling_gate_verdict(group_id, pressure, enabled)
}

/// 群级降温判据的判定与影子日志：返回 `Allow` 才会继续后面的概率抽样。
///
/// 三条边界都在这里：
/// 1. 读不到压力（`None`）一律放行。
/// 2. 开关关闭时只打影子日志（`shadow=true` 写明"如果打开，这一次会被跳过"），
///    结论仍是 `Allow`，可见行为与现在完全一致。
/// 3. 压力只在过线时打日志：低于阈值是常态，逐条打印会把日志淹掉。
fn cooling_gate_verdict(
    group_id: i64,
    pressure: Option<f32>,
    enabled: bool,
) -> GroupCoolingVerdict {
    let Some(pressure) = pressure else {
        return GroupCoolingVerdict::Allow;
    };
    let GroupCoolingVerdict::Skip { reason } = group_cooling_verdict(pressure, enabled) else {
        return GroupCoolingVerdict::Allow;
    };
    println!(
        "[GROUP_COOLING] shadow={} group={group_id} pressure={pressure:.3} reason={reason}",
        !enabled
    );
    GroupCoolingVerdict::Skip { reason }
}

/// 把一条群消息记成群级降温证据，并推进"她插话后有没有人接"的观察。
///
/// 记账与生效是分开的：开关关闭时证据照记（影子阶段要能看到"如果打开会怎样"），
/// 只有 `group_cooling_gate` 会真的跳过抽样。
///
/// 两件事放在一起是因为它们共用同一个 `directed_to_her` 判定：确定性的"她插话后
/// 有没有人接"在这里记账，而"这条消息是不是在赶她"由模型判定（`relation_evidence`）
/// 在别处记账——**指向她**的那份在入站点就发起了，这里只管她插过话没人接、
/// 而这条不是对她说的那一档：那种否定只有配上这个窗口才站得住，所以上下文由这里给。
async fn observe_group_cooling(
    group_id: i64,
    user_id: i64,
    sender_label: &str,
    message: &str,
    directed_to_her: bool,
) {
    let watch_step = advance_ambient_interjection_watch(group_id, directed_to_her).await;
    if directed_to_her {
        // 指向她的消息由入站点的模型判定负责（个人级 + 群级共用一份结论）。
        return;
    }
    if matches!(watch_step, AmbientWatchStep::Waiting)
        && crate::config::get()
            .silence()
            .relation_evidence_model_enabled()
    {
        // 她刚插过话、还没人接：问一次"这句话是不是在否定她那次开口"。同样是
        // 后台任务——判定不该拖慢这一轮的回复，也不改变这条消息的去向。
        crate::relation_evidence::spawn_judgement(
            group_id,
            user_id,
            crate::relation_evidence::EvidenceInput {
                sender_label,
                text: message,
                question: crate::relation_evidence::EvidenceQuestion::NegatingInterjection,
            },
        );
    }
    let Some(signal) = group_cooling_evidence(watch_step) else {
        return;
    };
    let Some(store) = crate::yunxi::group_cooling_store() else {
        return;
    };
    match store.nudge(group_id, signal, Some(user_id)).await {
        Ok(Some(state)) => println!(
            "[GROUP_COOLING] 群级证据已记账 group={group_id} signal={} user={user_id} pressure={:.3}",
            signal.label(),
            state.pressure
        ),
        Ok(None) => {}
        Err(error) => eprintln!("[WARN] 写入群级降温证据失败 (群组: {group_id}): {error}"),
    }
}

/// 推进本群的"无人应答"观察（纯内存，不打日志）。
async fn advance_ambient_interjection_watch(
    group_id: i64,
    directed_to_her: bool,
) -> AmbientWatchStep {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    let Some(state) = states.get_mut(&group_id) else {
        return AmbientWatchStep::Idle;
    };
    advance_ambient_watch(&mut state.ambient_watch, directed_to_her, Instant::now())
}

/// 未点名回合的边界：只有这些条件全不成立时，消息才会进入"未点名抽样"。
///
/// 群级降温、接话抽样、插话预算都只作用在这条路上。被 `@`、被引用、被点名
/// （`addressed_to_bot`）、显式识图，以及正处在她自己回合里的消息都在另一侧
/// ——那是"直接问她"或"接着刚才的话说"，任何"这个群冷不冷"的信号在那里
/// 都没有发言权。
fn ambient_sampling_eligible(
    addressed_to_bot: bool,
    vision_requested: bool,
    carries_images: bool,
    active_reply: bool,
    conversation_active: bool,
) -> bool {
    !(addressed_to_bot || vision_requested || carries_images || active_reply || conversation_active)
}

/// 表情回应只针对指向芸汐的已点名消息（在 addressing 判定后的主回复路径中
/// 处理）；未点名的纯表情包与普通图片一样只观察，不再自动回复。
fn has_interjection_candidate(message: &str, min_message_chars: usize) -> bool {
    let text = message.trim();
    !text.starts_with('#') && text.chars().count() >= min_message_chars
}

fn prune_decision_attempts(
    state: &mut GroupInterjectionState,
    now: Instant,
    rate_window: Duration,
) {
    while state
        .decision_attempts
        .front()
        .is_some_and(|attempt| now.duration_since(*attempt) >= rate_window)
    {
        state.decision_attempts.pop_front();
    }
}

fn decision_budget_available(
    state: &GroupInterjectionState,
    now: Instant,
    cooldown: Duration,
    rate_limit: usize,
) -> bool {
    state
        .decision_attempts
        .back()
        .is_none_or(|attempt| now.duration_since(*attempt) >= cooldown)
        && state.decision_attempts.len() < rate_limit
}

/// 模型选择静默时只结束本轮尝试；真正发出消息后才开始冷却。
/// "这一轮插话尝试"的凭据。
///
/// `reserve_interjection_decision` 占住 `interjection_in_flight` 之后把它交给调用方：
/// 调用方**必须**显式 [`Self::complete`]，否则 Drop 会兜底解掉。原来只有"记得手写
/// finish"这一条路，而中间任何一条早退（语义过期、纯图片、额度不够、排队等）都会让
/// 这个群永久停在"有插话在途"——`reserve` 从此直接返回 false，且 prune 刻意保留在途项，
/// 于是那个群再也不会主动插话，直到进程重启。
struct InterjectionAttempt {
    group_id: i64,
    completed: bool,
}

impl InterjectionAttempt {
    async fn complete(mut self, replied: bool) {
        self.completed = true;
        finish_interjection_attempt(self.group_id, replied).await;
    }
}

impl Drop for InterjectionAttempt {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // 状态表是 tokio 的 Mutex，Drop 里不能 await，所以把兜底清理交给运行时
        // （与 lib.rs 的 IncomingAdmissionGuard 同一手法）。
        let group_id = self.group_id;
        if let Ok(handle) = kovi::tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                finish_interjection_attempt(group_id, false).await;
            });
        }
    }
}

async fn finish_interjection_attempt(group_id: i64, replied: bool) {
    let mut states = GROUP_INTERJECTION_STATE.lock().await;
    if let Some(state) = states.get_mut(&group_id) {
        complete_interjection_attempt(state, replied, Instant::now());
    }
}

fn complete_interjection_attempt(
    state: &mut GroupInterjectionState,
    replied: bool,
    completed_at: Instant,
) {
    state.interjection_in_flight = false;
    if replied {
        state.last_interjection = Some(completed_at);
        // 真正发出了一条未点名插话：从这里开始观察"群里有没有人接她的话"。
        // 只有抽样插话会走到这里；被点名回答别人的回合不开始观察——那种
        // "之后没人说话"多半只是对话结束了，记成"被晾着"会冤枉整个群。
        state.ambient_watch = Some(AmbientInterjectionWatch::started_at(completed_at));
    }
}

fn prune_interjection_states(states: &mut HashMap<i64, GroupInterjectionState>) {
    let now = Instant::now();
    let interjection_config = config::get().group_interjection().clone();
    let cooldown = Duration::from_secs(interjection_config.cooldown_secs());
    let decision_window = Duration::from_secs(interjection_config.decision_rate_window_secs());
    let reply_rate_window = Duration::from_secs(interjection_config.reply_rate_window_secs());
    for state in states.values_mut() {
        state.conversation.prune();
        prune_decision_attempts(state, now, decision_window);
        while state
            .visible_replies
            .front()
            .is_some_and(|slot| now.saturating_duration_since(slot.at) >= reply_rate_window)
        {
            state.visible_replies.pop_front();
        }
    }
    if states.len() <= 1_024 {
        return;
    }
    states.retain(|_, state| {
        state.interjection_in_flight
            || state.conversation.is_active()
            || !state.decision_attempts.is_empty()
            || !state.visible_replies.is_empty()
            // 观察中的群不能被回收：丢了它，这一次"无人应答"就再也不会记账。
            || state.ambient_watch.is_some()
            || state
                .last_interjection
                .is_some_and(|last| now.duration_since(last) < cooldown)
    })
}

fn message_at_self(message: &Message, self_id: i64) -> bool {
    message.iter().any(|segment| {
        if segment.type_ != "at" {
            return false;
        }

        match segment.data.get("qq") {
            // @所有人 同样包含芸汐，按点名处理，避免“必须逐次 @她”才能被看见。
            Some(qq) if qq.as_str() == Some("all") => true,
            Some(qq) => {
                qq.as_i64() == Some(self_id)
                    || qq.as_str().and_then(|value| value.parse::<i64>().ok()) == Some(self_id)
            }
            None => false,
        }
    })
}

/// 把消息里的定向证据渲染成一行短文本：`at=[123,all] reply=456 self=789`。
///
/// 为什么要它：这条判定原先只输出结论（"指向其他成员"），不输出依据。
/// 2026-09-14 22:27 群里有人 @ 她问"真能用吗"、她被静默，排查时卡住的唯一一件事
/// 就是**看不出那个 `[at]` 指向谁**（`Message::to_human_string` 对任何人的 @ 都只
/// 渲染成 `[at]`，`InboundMessage` 那条同名日志的注释也写着"离线采集无法还原到底
/// 在叫谁"）。而目标恰恰是判定成立的唯一依据：`at=[]`（悬空 @ / 空目标）与
/// `at=[别人的号]` 是两种完全不同的情况，出了事只能靠这一行区分。
///
/// 只保留前几个目标：入站消息的段数有上限，这里再夹一道，避免异常消息把日志撑爆。
pub(crate) fn addressing_evidence(message: &Message, self_id: i64) -> String {
    const MAX_TARGETS: usize = 4;
    let mut targets: Vec<String> = Vec::new();
    let mut reply = String::from("none");
    for segment in message.iter() {
        match segment.type_.as_str() {
            "at" if targets.len() < MAX_TARGETS => {
                targets.push(match segment.data.get("qq") {
                    Some(qq) => qq.as_str().map_or_else(|| qq.to_string(), str::to_owned),
                    // 段在、目标不在：QQ 客户端发出的悬空 @。
                    None => "?".to_owned(),
                });
            }
            "reply" => {
                reply = segment
                    .data
                    .get("id")
                    .map_or_else(|| "?".to_owned(), ToString::to_string);
            }
            _ => {}
        }
    }
    format!("at=[{}] reply={reply} self={self_id}", targets.join(","))
}

/// 消息是否携带 at/reply 定向段。调用方必须先排除“指向芸汐本人”的情况
/// （结构化 at 自己或引用自己），因此这里只需判断是否存在定向段：
/// 点名或引用其他成员的消息是定向消息，不应触发插话或接续对话。
fn directed_at_others(message: &Message) -> bool {
    message
        .iter()
        .any(|segment| matches!(segment.type_.as_str(), "at" | "reply"))
}

fn text_mentions_bot(message: &str) -> bool {
    ["芸汐", "云汐"].iter().any(|name| message.contains(name))
}

fn with_structured_bot_mention_context(message: &str) -> String {
    let context = "<QQ点名事件 data-only=\"true\">本条消息包含一个指向芸汐 QQ 账号的结构化 at 事件。即使正文只显示为“@”或“我”，也要理解为对方正在直接和芸汐说话。不要复述这段资料，也不要解释消息协议。</QQ点名事件>";
    if message.trim().is_empty() {
        context.to_string()
    } else {
        format!("{message}\n{context}")
    }
}

async fn update_group_profile(group_id: i64, user_id: i64, understanding: &MessageUnderstanding) {
    let topics = understanding.topics.clone();
    let group_atmosphere = understanding.group_atmosphere.trim().to_string();
    let now = Local::now();
    if let Err(e) = MEMORY_MANAGER
        .mutate_group_profile(group_id, move |current| {
            let mut profile = current.unwrap_or_else(|| GroupProfile {
                group_id,
                group_name: format!("群组_{}", group_id),
                active_members: Vec::new(),
                group_personality: "friendly".to_string(),
                conversation_topics: Vec::new(),
                last_activity: now,
                activity_level: 1,
            });
            profile.last_activity = now;
            profile.activity_level = profile.activity_level.saturating_add(1).min(10);
            if !profile.active_members.contains(&user_id) {
                profile.active_members.push(user_id);
                if profile.active_members.len() > 100 {
                    profile.active_members.remove(0);
                }
            }
            for topic in topics
                .iter()
                .map(|topic| topic.trim())
                .filter(|topic| !topic.is_empty())
            {
                if !profile
                    .conversation_topics
                    .iter()
                    .any(|existing| existing == topic)
                {
                    profile.conversation_topics.push(topic.to_string());
                }
            }
            if profile.conversation_topics.len() > 20 {
                profile
                    .conversation_topics
                    .drain(0..profile.conversation_topics.len() - 20);
            }
            if !group_atmosphere.is_empty() {
                profile.group_personality = group_atmosphere;
            }
            profile
        })
        .await
    {
        eprintln!("[ERROR] 更新群组档案失败 (群组: {}): {}", group_id, e);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Addressing, DirectTriggerState, GROUP_INTERJECTION_STATE, GroupConversationFocus,
        GroupInterjectionState, GroupSenderIdentity, InterjectionAttempt, PENDING_WINDOW_MESSAGES,
        ReplyBudgetClass, ReplyBudgetLimits, ReplySlotReservation, WindowDrainWait,
        addressing_evidence, admit_understood_group_turn, ambient_sampling_eligible,
        clear_group_erasure_reply_state_locked, complete_interjection_attempt,
        confirm_visible_reply_slot, continuation_window_secs, conversation_active_for_observation,
        cooling_gate_verdict, decision_budget_available, directed_at_others,
        group_conversation_focus_state_now, group_conversation_focus_user_now, group_cooling_gate,
        group_erasure_receipt_destination, group_pause_acknowledgement, group_pause_command,
        interjection_sampling_vetoed, message_at_self, normalized_sender_name,
        note_group_conversation_focus, paced_group_reply_gap_secs, pending_window_group_ids,
        prune_decision_attempts, queue_pending_window_message, release_unconfirmed_slot,
        reply_budget_class, reserve_visible_reply_slot, suppress_direct_trigger,
        take_pending_window_turn, text_mentions_bot, with_structured_bot_mention_context,
    };
    use crate::group_cooling::{
        GROUP_COOLING_SKIP_THRESHOLD, GroupCoolingVerdict, group_cooling_verdict,
    };
    use crate::model::MessageDestination;
    use crate::model::conversation_coordinator::{
        ConversationCoordinator, OutgoingExecutiveDecision,
    };
    use crate::model::conversation_coordinator::{WindowQueueDecision, window_queue_decision};
    use crate::model::conversation_state::ConversationTurnOptions;
    use crate::model::interrupt::{
        OutgoingSource, OutgoingState, ReplyScope, commit_outgoing, interrupt, interrupt_locked,
        is_current, is_scope_epoch_current, mark_active, mark_outgoing_failed,
        outgoing_fingerprint, prepare_outgoing, scope_mutex, test_outgoing_state,
    };
    use crate::model::semantic::MessageUnderstanding;
    use crate::model::utils::{is_group_admin_command, is_restricted_command};
    use crate::vision::VisionImage;
    use kovi::Message;
    use kovi::bot::message::Segment;
    use kovi::serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn group_pause_commands_are_recognized_deterministically() {
        // 只有字面命令命中；空白不影响判断。
        assert_eq!(group_pause_command("#禁言"), Some(true));
        assert_eq!(group_pause_command("  #结束禁言 "), Some(false));
        assert_eq!(group_pause_command(" #禁言\n"), Some(true));
        // 其余 # 命令和普通聊天都不属于暂停控制面。
        assert_eq!(group_pause_command("#结束禁言 一下"), None);
        assert_eq!(group_pause_command("#取消禁言"), None);
        assert_eq!(group_pause_command("#系统信息"), None);
        assert_eq!(group_pause_command("禁言"), None);
        assert_eq!(group_pause_command(""), None);
        // 回执与状态一致：禁言/恢复各自一句确定性文本。
        assert_eq!(group_pause_acknowledgement(true), "禁言成功");
        assert_eq!(group_pause_acknowledgement(false), "结束成功");
    }

    #[test]
    fn group_erasure_rotates_scope_epoch_and_uses_an_unrecorded_group_receipt() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_120_001;
                let scope = ReplyScope::Group(group_id);
                let erased = interrupt(scope).await;
                let lock = scope_mutex(scope);
                let guard = lock.lock().await;
                clear_group_erasure_reply_state_locked(scope).await;
                drop(guard);
                let replacement = interrupt(scope).await;

                assert!(!is_scope_epoch_current(erased).await);
                assert!(is_scope_epoch_current(replacement).await);
                assert_eq!(
                    group_erasure_receipt_destination(group_id),
                    MessageDestination::Group(group_id)
                );
            });
    }

    #[test]
    fn evidence_is_only_collected_for_messages_that_point_at_her() {
        // 定向由代码判、情绪交给模型；群友互相斗嘴既不该触发判定，也不该被她
        // 记成"对我不好"。这条门原先挂在 Host 群聊入口，而"指向她"的消息在
        // `classify_group` 里判给 Core、走不到那里——判据没问题，是接线让它
        // 一条都记不上；现在调用点在两条路共同的入站点（`lib.rs`）。
        assert!(
            !crate::relation_evidence::should_judge(false, true),
            "不指向她的消息不该问模型"
        );
        assert!(crate::relation_evidence::should_judge(true, true));
        assert!(
            !crate::relation_evidence::should_judge(true, false),
            "开关关掉就不问"
        );
        // 判据本身的强度刻度由 `relation_evidence` 的测试守着。
    }

    #[test]
    fn structured_at_segments_identify_only_the_bot_account() {
        let self_id = 123_456;

        let string_id = Message::from(vec![Segment::new("at", json!({"qq": "123456"}))]);
        let numeric_id = Message::from(vec![Segment::new("at", json!({"qq": 123456}))]);
        let another_user = Message::from(vec![Segment::new("at", json!({"qq": "654321"}))]);
        let everyone = Message::from(vec![Segment::new("at", json!({"qq": "all"}))]);
        let no_at = Message::from("芸汐在吗");
        let multiple_targets = Message::from(vec![
            Segment::new("at", json!({"qq": "654321"})),
            Segment::new("text", json!({"text": "还有"})),
            Segment::new("at", json!({"qq": "123456"})),
        ]);

        assert!(message_at_self(&string_id, self_id));
        assert!(message_at_self(&numeric_id, self_id));
        assert!(!message_at_self(&another_user, self_id));
        assert!(message_at_self(&everyone, self_id));
        assert!(!message_at_self(&no_at, self_id));
        assert!(message_at_self(&multiple_targets, self_id));
    }

    /// 判定要留下依据，否则出事时只能看到结论。2026-09-14 22:27 那次静默就卡在
    /// "看不出 `[at]` 指向谁"，其中最难区分的是**悬空 @**（段在、目标不在）——
    /// 它既不是"在叫她"，也不是"在叫别人"，只看结论完全看不出来。
    #[test]
    fn addressing_evidence_names_targets_reply_and_self() {
        let at_other = Message::from(vec![
            Segment::new("at", json!({"qq": "654321"})),
            Segment::new("text", json!({"text": "快回来直播"})),
        ]);
        assert_eq!(
            addressing_evidence(&at_other, 123_456),
            "at=[654321] reply=none self=123456"
        );

        let dangling = Message::from(vec![Segment::new("at", json!({}))]);
        assert_eq!(
            addressing_evidence(&dangling, 123_456),
            "at=[?] reply=none self=123456"
        );

        let both = Message::from(vec![
            Segment::new("reply", json!({"id": 42})),
            Segment::new("at", json!({"qq": "all"})),
        ]);
        assert_eq!(addressing_evidence(&both, 7), "at=[all] reply=42 self=7");

        // 没有定向段时也要给出完整形状：`at=[]` 与"没打印这一行"是两回事。
        let plain = Message::from("今天群里有点安静");
        assert_eq!(addressing_evidence(&plain, 7), "at=[] reply=none self=7");
    }

    #[test]
    fn directed_at_others_only_matches_at_and_reply_segments() {
        let plain = Message::from("今天群里有点安静");
        assert!(!directed_at_others(&plain));

        let at_other = Message::from(vec![
            Segment::new("at", json!({"qq": "654321"})),
            Segment::new("text", json!({"text": "快回来直播"})),
        ]);
        assert!(directed_at_others(&at_other));

        let at_all = Message::from(vec![Segment::new("at", json!({"qq": "all"}))]);
        assert!(directed_at_others(&at_all));

        let reply_other = Message::from(vec![
            Segment::new("reply", json!({"id": 42})),
            Segment::new("text", json!({"text": "我说完了"})),
        ]);
        assert!(directed_at_others(&reply_other));

        let at_self = Message::from(vec![Segment::new("at", json!({"qq": "123456"}))]);
        // 调用方在进入本函数前已排除“指向芸汐本人”，本例只验证段类型判定本身。
        assert!(directed_at_others(&at_self));
    }

    /// 对话焦点：她可见回复谁，谁就是焦点；句数只记账不设闸；TTL 到期才失效。
    ///
    /// 断言走纯逻辑 + 阻塞读全局态：生产门用的是 `try_lock`（抢不到锁按"没有
    /// 焦点"处理，宁可少接一次），那种写法在并发测试里本来就不确定。
    #[test]
    fn conversation_focus_tracks_who_and_how_many_without_capping() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_120_777;
                let partner = 1_651_505_261_i64;
                let ttl = Duration::from_secs(
                    crate::config::get()
                        .group_interjection()
                        .continuation_focus_ttl_secs(),
                );

                assert!(!group_conversation_focus_user_now(group_id, partner));
                // 她回复被点名的消息：建立焦点，计数从 1 起。
                note_group_conversation_focus(group_id, partner, false).await;
                assert_eq!(group_conversation_focus_state_now(group_id, partner), 1);

                // 接续回复：累加；句数再多也不影响"还算不算接续"。
                note_group_conversation_focus(group_id, partner, true).await;
                note_group_conversation_focus(group_id, partner, true).await;
                assert_eq!(group_conversation_focus_state_now(group_id, partner), 3);
                {
                    let states = GROUP_INTERJECTION_STATE.lock().await;
                    let state = states.get(&group_id).expect("focus state");
                    assert_eq!(state.focus_user_at(Instant::now(), ttl), Some(partner));
                }

                // 对方再 @ 一次：计数重置（新的一段对话）。
                note_group_conversation_focus(group_id, partner, false).await;
                assert_eq!(group_conversation_focus_state_now(group_id, partner), 1);

                // TTL 到期：不再算接续。
                {
                    let mut states = GROUP_INTERJECTION_STATE.lock().await;
                    let state = states.get_mut(&group_id).expect("focus state");
                    state.conversation_focus = Some(GroupConversationFocus {
                        user_id: partner,
                        since: Instant::now() - ttl - Duration::from_secs(1),
                        replies: 1,
                    });
                }
                assert!(!group_conversation_focus_user_now(group_id, partner));
            });
    }

    #[test]
    fn fresh_bot_reply_opens_continuation_window_then_expires() {
        let window = continuation_window_secs();
        assert!(window > 0, "接续窗口必须大于 0 秒");
        let now = Instant::now();
        let mut state = GroupInterjectionState::default();
        assert!(!conversation_active_for_observation(&state, now));

        // 断言跟着配置走：窗口只有几十秒时，写死的 60 秒会落到窗口之外。
        state.last_bot_reply = Some(now - Duration::from_secs(window - 1));
        assert!(conversation_active_for_observation(&state, now));

        state.last_bot_reply = Some(now - Duration::from_secs(window + 1));
        assert!(!conversation_active_for_observation(&state, now));
    }

    #[test]
    fn continuation_window_requires_fresh_reply_even_semantically_active() {
        // 语义会话状态本身不随回复衰减：即使 `conversation.active` 仍为真，
        // 只要最近一次可见回复已过期，观察门就必须关闭，避免"每句话都回"。
        let window = continuation_window_secs();
        let now = Instant::now();
        let mut state = GroupInterjectionState::default();
        let marker = state.conversation.begin_turn(
            42,
            ConversationTurnOptions {
                reset_context: true,
                close_after_reply: false,
            },
            &["面试".to_owned()],
        );
        state.conversation.finish_turn(42, marker, true);
        assert!(state.conversation.is_active());

        state.last_bot_reply = Some(now - Duration::from_secs(window + 1));
        assert!(!conversation_active_for_observation(&state, now));

        state.last_bot_reply = Some(now - Duration::from_secs(10));
        assert!(conversation_active_for_observation(&state, now));
    }

    #[test]
    fn structured_bot_mentions_are_explained_to_the_model_without_visible_protocol_text() {
        let context = with_structured_bot_mention_context("oi");

        assert!(context.starts_with("oi\n"));
        assert!(context.contains("指向芸汐 QQ 账号的结构化 at 事件"));
        assert!(context.contains("不要复述这段资料"));
    }

    #[test]
    fn at_and_reply_are_unified_as_direct_addressing() {
        let self_id = 123_456;
        let at_self = Message::from(vec![Segment::new("at", json!({"qq": "123456"}))]);
        let plain = Message::from("继续说");

        let mention = Addressing::detect(&at_self, "在吗", self_id, None);
        assert!(mention.at_self);
        assert!(!mention.reply_to_self);
        assert!(!mention.named_in_text);
        assert!(mention.directly_addressed());

        let reply = Addressing::detect(&plain, "继续说", self_id, Some(self_id));
        assert!(!reply.at_self);
        assert!(reply.reply_to_self);
        assert!(!reply.named_in_text);
        assert!(reply.directly_addressed());

        let named = Addressing::detect(&plain, "芸汐你在吗", self_id, None);
        assert!(!named.at_self);
        assert!(!named.reply_to_self);
        assert!(named.named_in_text);
        assert!(named.directly_addressed());

        assert!(!Addressing::detect(&plain, "继续说", self_id, Some(654_321)).directly_addressed());
    }

    #[test]
    fn bot_name_aliases_are_direct_text_mentions() {
        assert!(text_mentions_bot("芸汐你在吗"));
        assert!(text_mentions_bot("云汐，看看这个"));
        assert!(text_mentions_bot("你家芸汐好像有点安静"));
        assert!(!text_mentions_bot("今天群里有点安静"));
    }

    #[test]
    fn group_identity_keeps_card_and_qq_nickname_separate() {
        let identity = GroupSenderIdentity {
            user_id: 123_456_789,
            qq_nickname: "QQ用户名".to_string(),
            group_card: Some("群内昵称".to_string()),
        };
        assert_eq!(identity.display_name(), "群内昵称");
        let sender = identity.model_sender("12:34:56");
        // 称呼用于显示，**身份必须带 QQ 号**：两个人可以把群名片改成同一个，
        // 只靠称呼认人会把记忆张冠李戴（用户 2026-09-14 的要求）。
        assert!(sender.contains("群内昵称"));
        assert!(sender.contains("QQ=123456789"));
        assert!(!sender.contains("QQ用户名"));
        assert_eq!(identity.reply_target_label(), "群内昵称");
    }

    #[test]
    fn sender_names_are_trimmed_without_merging_identity_fields() {
        assert_eq!(
            normalized_sender_name(Some("  群 名片\n测试  ")).as_deref(),
            Some("群 名片 测试")
        );
        assert_eq!(normalized_sender_name(Some("   ")), None);
    }

    #[test]
    fn pending_group_turns_keep_each_senders_payload_atomic() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_200_001;
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
                queue_pending_window_message(
                    group_id,
                    11,
                    true,
                    "成员甲".to_string(),
                    "第一条".to_string(),
                    vec![VisionImage {
                        url: "data:image/png;base64,AA==".to_string(),
                    }],
                    vec![101],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                queue_pending_window_message(
                    group_id,
                    22,
                    false,
                    "成员乙".to_string(),
                    "第二条".to_string(),
                    vec![VisionImage {
                        url: "data:image/jpeg;base64,/9j/2Q==".to_string(),
                    }],
                    vec![202],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                let mut pending = PENDING_WINDOW_MESSAGES.lock().await;
                let queue = pending.get(&group_id).expect("应保留群队列");
                assert_eq!(queue.len(), 2);
                assert_eq!(queue[0].user_id, 11);
                assert_eq!(queue[0].message, "第一条");
                assert_eq!(queue[0].message_ids, vec![101]);
                assert_eq!(queue[0].vision_images[0].url, "data:image/png;base64,AA==");
                assert_eq!(queue[1].user_id, 22);
                assert_eq!(queue[1].message, "第二条");
                assert_eq!(queue[1].message_ids, vec![202]);
                assert_eq!(
                    queue[1].vision_images[0].url,
                    "data:image/jpeg;base64,/9j/2Q=="
                );
                pending.remove(&group_id);
            });
    }

    #[test]
    fn stale_group_drainer_adopts_successor_and_preserves_fifo() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_200_003;
                let scope = ReplyScope::Group(group_id);
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
                let completed = interrupt(scope).await;
                queue_pending_window_message(
                    group_id,
                    33,
                    false,
                    "成员".to_string(),
                    "旧排队消息".to_string(),
                    Vec::new(),
                    vec![303],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;

                let new_message_won = std::sync::Arc::new(kovi::tokio::sync::Notify::new());
                let new_task = {
                    let scope_lock = scope_mutex(scope);
                    let new_message_won = std::sync::Arc::clone(&new_message_won);
                    kovi::tokio::spawn(async move {
                        let _scope_guard = scope_lock.lock().await;
                        let ticket = interrupt_locked(scope).await;
                        new_message_won.notify_one();
                        ticket
                    })
                };
                new_message_won.notified().await;

                let drainer = kovi::tokio::spawn(async move {
                    take_pending_window_turn(
                        group_id,
                        completed,
                        WindowDrainWait::ForPendingAdmission,
                    )
                    .await
                });
                let new_ticket = new_task.await.expect("新消息任务应正常结束");
                let (pending, claimed_ticket) = drainer
                    .await
                    .expect("旧 drainer 应正常结束")
                    .expect("旧 drainer 应接管 successor 并领取 FIFO turn");
                assert_eq!(pending.message, "旧排队消息");
                assert_eq!(pending.message_ids, vec![303]);
                assert!(is_current(claimed_ticket).await);
                crate::model::finish(claimed_ticket).await;
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
                assert!(!is_current(new_ticket).await);
            });
    }

    /// 排空时丢掉"已经被回答过"的 turn：同一条消息可能先被 Core 链路答了，
    /// 又被 Host 链路排进 waiting room（两条链路都看得到这条入站消息），不再
    /// 检查一次就会当着群友的面答第二遍。
    #[test]
    fn drainer_drops_queued_turns_the_core_already_answered() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_200_007;
                let scope = ReplyScope::Group(group_id);
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);

                // 先来一条已经被答过的、再来一条没答过的：只有后者该被领取。
                queue_pending_window_message(
                    group_id,
                    55,
                    true,
                    "成员".to_string(),
                    "已经被 Core 答过的消息".to_string(),
                    Vec::new(),
                    vec![505],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                queue_pending_window_message(
                    group_id,
                    66,
                    true,
                    "成员".to_string(),
                    "还没人回答的消息".to_string(),
                    Vec::new(),
                    vec![606],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;

                // 模拟 Core 链路把 505 答掉了（真的发出过消息的轮次）。
                let answered = interrupt(scope).await;
                assert!(
                    crate::model::recall::begin_reply(scope, answered, vec![505]).await,
                    "Core 轮次应登记成功"
                );
                assert!(
                    crate::model::recall::record_committed_bot_message(
                        scope,
                        answered,
                        90_001,
                        "我答过了"
                    )
                    .await,
                    "Core 发出的消息应记账"
                );
                crate::model::recall::finish_reply(scope, answered).await;

                let completed = interrupt(scope).await;
                let (pending, ticket) = take_pending_window_turn(
                    group_id,
                    completed,
                    WindowDrainWait::ForPendingAdmission,
                )
                .await
                .expect("应领取没被答过的那条");
                assert_eq!(pending.message, "还没人回答的消息");
                assert_eq!(pending.message_ids, vec![606]);
                let remaining = PENDING_WINDOW_MESSAGES.lock().await;
                assert!(
                    remaining
                        .get(&group_id)
                        .is_none_or(|queue| queue.is_empty()),
                    "被答过的那条应当已被丢弃"
                );
                drop(remaining);
                crate::model::finish(ticket).await;
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
            });
    }

    /// 看门狗只该看"真的还有人在等"的群：空队列（以及正在等待中的空壳）
    /// 不该被反复扫到，否则每一轮都要白白锁一次全局表。
    #[test]
    fn window_sweep_only_reports_groups_with_a_non_empty_queue() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let busy_group = 9_200_011;
                let idle_group = 9_200_013;
                PENDING_WINDOW_MESSAGES.lock().await.remove(&busy_group);
                PENDING_WINDOW_MESSAGES.lock().await.remove(&idle_group);
                queue_pending_window_message(
                    busy_group,
                    77,
                    true,
                    "成员".to_string(),
                    "还在等的消息".to_string(),
                    Vec::new(),
                    vec![707],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;

                let groups = pending_window_group_ids().await;
                assert!(groups.contains(&busy_group));
                assert!(!groups.contains(&idle_group));

                PENDING_WINDOW_MESSAGES.lock().await.remove(&busy_group);
                assert!(!pending_window_group_ids().await.contains(&busy_group));
            });
    }

    #[test]
    fn group_drainer_waits_for_unresolved_active_admission() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_200_005;
                let scope = ReplyScope::Group(group_id);
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
                let completed = interrupt(scope).await;
                assert!(mark_active(completed).await);
                let blocker = ConversationCoordinator::begin_incoming(scope).await;
                queue_pending_window_message(
                    group_id,
                    44,
                    true,
                    "成员".to_string(),
                    "应先处理的排队消息".to_string(),
                    Vec::new(),
                    vec![404],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                crate::model::finish(completed).await;

                let drainer = kovi::tokio::spawn(async move {
                    take_pending_window_turn(
                        group_id,
                        completed,
                        WindowDrainWait::ForPendingAdmission,
                    )
                    .await
                });
                kovi::tokio::task::yield_now().await;
                assert!(!drainer.is_finished());

                let refined = ConversationCoordinator::refine_current_incoming(
                    blocker,
                    ConversationCoordinator::context_for_understood_turn(
                        &MessageUnderstanding::default(),
                        false,
                        false,
                    ),
                )
                .await
                .expect("活动 admission 应保持当前");
                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);

                let (pending, ticket) = kovi::tokio::time::timeout(Duration::from_secs(1), drainer)
                    .await
                    .expect("drainer 应被 admission 释放唤醒")
                    .expect("drainer 任务应正常完成")
                    .expect("应领取原队首消息");
                assert_eq!(pending.message, "应先处理的排队消息");
                assert_eq!(pending.message_ids, vec![404]);
                crate::model::finish(ticket).await;
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
            });
    }

    /// 看门狗那一轮**不等** pending admission：线上 19:14 实测它等满 60 秒、
    /// 触发超时告警，还把整轮扫描取消掉，而 30 秒后它本来就会再来。所以这里
    /// 断言的是"立刻返回"，队列原样留在 waiting room 里等下一轮。
    #[test]
    fn watchdog_drain_returns_immediately_instead_of_waiting_for_an_admission() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 9_200_017;
                let scope = ReplyScope::Group(group_id);
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
                let completed = interrupt(scope).await;
                assert!(mark_active(completed).await);
                let blocker = ConversationCoordinator::begin_incoming(scope).await;
                queue_pending_window_message(
                    group_id,
                    88,
                    true,
                    "成员".to_string(),
                    "被等在途工作挡住的消息".to_string(),
                    Vec::new(),
                    vec![808],
                    None,
                    MessageUnderstanding::default(),
                )
                .await;
                crate::model::finish(completed).await;

                let claimed = kovi::tokio::time::timeout(
                    Duration::from_millis(200),
                    take_pending_window_turn(group_id, completed, WindowDrainWait::Never),
                )
                .await
                .expect("看门狗不该在这里等 pending admission");
                assert!(claimed.is_none(), "领不到就返回 None，队列原样保留");
                assert_eq!(
                    PENDING_WINDOW_MESSAGES
                        .lock()
                        .await
                        .get(&group_id)
                        .map(std::collections::VecDeque::len),
                    Some(1),
                    "队列必须还在，留给下一轮"
                );

                ConversationCoordinator::abandon_incoming(blocker).await;
                PENDING_WINDOW_MESSAGES.lock().await.remove(&group_id);
            });
    }

    #[test]
    fn interjection_cooldown_starts_only_after_a_visible_reply() {
        let mut state = GroupInterjectionState {
            interjection_in_flight: true,
            ..GroupInterjectionState::default()
        };
        let now = Instant::now();
        complete_interjection_attempt(&mut state, false, now);
        assert!(!state.interjection_in_flight);
        assert!(state.last_interjection.is_none());
        // 没真的发出可见回复就不算插话：不开始观察，免得把"她其实没说话"
        // 之后的安静记成"没人搭理她"。
        assert!(state.ambient_watch.is_none());

        state.interjection_in_flight = true;
        complete_interjection_attempt(&mut state, true, now);
        assert!(!state.interjection_in_flight);
        assert_eq!(state.last_interjection, Some(now));
        // 真正发出可见插话之后才开始"有没有人接她的话"的观察。
        assert!(state.ambient_watch.is_some());
    }

    /// 凭据被 Drop 时必须兜底解掉"有插话在途"。
    ///
    /// 这正是原来漏掉的那条路：`reserve_interjection_decision` 占住标记之后，中间
    /// 任何一条早退（语义过期、纯图片、额度不够、排队等）都会让这个群永久停在在途
    /// 状态——`reserve` 从此直接返回 false，而 prune 又刻意保留在途项，于是那个群再也
    /// 不会主动插话，直到进程重启。
    #[test]
    fn dropping_an_interjection_attempt_releases_the_in_flight_flag() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let group_id = 987_654_321_i64;
                {
                    let mut states = GROUP_INTERJECTION_STATE.lock().await;
                    states.insert(
                        group_id,
                        GroupInterjectionState {
                            interjection_in_flight: true,
                            ..GroupInterjectionState::default()
                        },
                    );
                }
                {
                    // 拿到凭据却什么都没做就退出作用域：等价于漏写 finish 的早退路径。
                    let _attempt = InterjectionAttempt {
                        group_id,
                        completed: false,
                    };
                }
                // Drop 里是 spawn，让运行时有机会跑它。
                kovi::tokio::time::sleep(Duration::from_millis(100)).await;
                let states = GROUP_INTERJECTION_STATE.lock().await;
                assert!(
                    !states
                        .get(&group_id)
                        .expect("状态还在")
                        .interjection_in_flight,
                    "Drop 应兜底解掉在途标记，否则这个群再也不会主动插话"
                );
                drop(states);
                GROUP_INTERJECTION_STATE.lock().await.remove(&group_id);
            });
    }

    #[test]
    fn ambient_sampling_excludes_every_directed_turn() {
        // 只有"完全没人点她、也没在接她的话"的消息才进未点名抽样。
        assert!(ambient_sampling_eligible(false, false, false, false, false));
        // 被 @、被引用、被点名：直接问她的回合，群级降温与抽样都碰不到。
        assert!(!ambient_sampling_eligible(true, false, false, false, false));
        // 显式识图、带图消息、正在处理的回合、以及接续窗口内的消息同理。
        assert!(!ambient_sampling_eligible(false, true, false, false, false));
        assert!(!ambient_sampling_eligible(false, false, true, false, false));
        assert!(!ambient_sampling_eligible(false, false, false, true, false));
        assert!(!ambient_sampling_eligible(false, false, false, false, true));
    }

    #[test]
    fn group_cooling_vetoes_the_ambient_sampling_chance() {
        let over_threshold = GROUP_COOLING_SKIP_THRESHOLD + 0.05;
        // 开关打开 + 压力过线：这一次未点名抽样机会作废（降频，不是静默：
        // 下一次机会只是要再等一批候选消息，压力衰减后照常）。
        assert!(interjection_sampling_vetoed(group_cooling_verdict(
            over_threshold,
            true
        )));
        // 默认关闭：判据照跑（上面那个结论就是它算出来的），但行为与现在
        // 完全一致——抽样机会一个不少。
        assert!(!interjection_sampling_vetoed(group_cooling_verdict(
            over_threshold,
            false
        )));
        // 压力没到线：照常抽样。
        assert!(!interjection_sampling_vetoed(group_cooling_verdict(
            over_threshold - 0.2,
            true
        )));
        assert_eq!(
            group_cooling_verdict(over_threshold, true),
            GroupCoolingVerdict::Skip {
                reason: "group_pressure"
            }
        );
    }

    #[test]
    fn group_cooling_gate_fails_open_and_stays_shadow_by_default() {
        let over_threshold = GROUP_COOLING_SKIP_THRESHOLD + 0.05;
        // 压力过线 + 开关打开：判据真的跳过这一次抽样。
        assert_eq!(
            cooling_gate_verdict(9_200_777, Some(over_threshold), true),
            GroupCoolingVerdict::Skip {
                reason: "group_pressure"
            }
        );
        // 默认关闭：结论仍是 Allow —— 判据照跑（日志照打），行为与现在完全一致。
        assert_eq!(
            cooling_gate_verdict(9_200_777, Some(over_threshold), false),
            GroupCoolingVerdict::Allow
        );
        // 压力没到线：照常抽样。
        assert_eq!(
            cooling_gate_verdict(9_200_777, Some(0.1), true),
            GroupCoolingVerdict::Allow
        );
        // 读不到压力：绝不因为一次读失败让她沉默。
        assert_eq!(
            cooling_gate_verdict(9_200_777, None, true),
            GroupCoolingVerdict::Allow
        );
    }

    #[test]
    fn group_cooling_gate_without_a_store_does_not_read_a_pressure() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                // 单元测试里没有初始化 Postgres：这条路必须按"照常抽样"处理。
                assert_eq!(
                    group_cooling_gate(9_200_777).await,
                    GroupCoolingVerdict::Allow
                );
            });
    }

    /// 测试专用：一次**真的发出去了**的可见回复。
    ///
    /// 预留只是乐观占位（生成之前先占），只有在
    /// `mark_group_reply_sent` 确认之后才在时间轴里留下记录；所以凡是要
    /// 表达"她确实回了这条"的地方都必须记账 + 确认，不能只预留。
    /// 测试用额度：未点名额度 = 点名额度 = `rate_limit`（这些回归测试只关心
    /// "同一条通道的额度"，不关心两类分开记账）。
    fn limits(rate_window: Duration, rate_limit: usize) -> ReplyBudgetLimits {
        ReplyBudgetLimits {
            rate_window,
            unaddressed_limit: rate_limit,
            addressed_limit: rate_limit,
        }
    }

    fn reserve_sent_reply(
        state: &mut GroupInterjectionState,
        now: Instant,
        gap: Duration,
        rate_window: Duration,
        rate_limit: usize,
    ) -> bool {
        let reserved = reserve_visible_reply_slot(
            state,
            now,
            gap,
            limits(rate_window, rate_limit),
            ReplyBudgetClass::Ambient,
        );
        if reserved != ReplySlotReservation::Rejected {
            confirm_visible_reply_slot(state, now);
        }
        reserved != ReplySlotReservation::Rejected
    }

    /// 线上回归（2026-09-14 16:18）：芸汐一个"只观察、没回复"的回合先在
    /// 群里占了名额，管理员紧接着的点名指令被 20 秒间隔挡掉（只差 110
    /// 毫秒），指令既没被回答也没被执行。预留是乐观的，回合判沉默就必须
    /// 立刻归还；没归还的（调用方早退、取消、panic）也有 30 秒兜底。
    #[test]
    fn released_reservations_stop_blocking_later_replies() {
        let mut state = GroupInterjectionState::default();
        let started = Instant::now();
        let gap = Duration::from_secs(90);
        let rate_window = Duration::from_secs(600);
        let rate_limit = 10;

        // 沉默回合：预留成功，但最终一个字都没发出去。
        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started,
                gap,
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Reserved,
        );
        // 一秒后的点名提问仍然被这一格挡住——这正是当时的现场。
        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started + Duration::from_secs(1),
                Duration::from_secs(20),
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Rejected,
        );
        // 回合结束归还后，同一句点名提问立刻可以通过。
        release_unconfirmed_slot(&mut state, started);
        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started + Duration::from_secs(1),
                Duration::from_secs(20),
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Reserved,
        );
        // 归还只针对未确认的那一格：真发出去的名额不能被还掉，
        // 否则"刚回过一句"就可以被下一句立刻插进来。
        confirm_visible_reply_slot(&mut state, started + Duration::from_secs(1));
        release_unconfirmed_slot(&mut state, started + Duration::from_secs(1));
        assert_eq!(state.visible_replies.len(), 1);
        assert!(state.visible_replies[0].confirmed);
        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started + Duration::from_secs(2),
                Duration::from_secs(20),
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Rejected,
        );
    }

    /// 点名与未点名**分开记账**：未点名接话撞 10 条那份额度，点名走更宽的 20 条；
    /// 两者都受后者封顶。
    ///
    /// 现场（2026-09-14 晚上）：一场热闹的来回里她五分钟就把 10 条用光，之后整段
    /// 窗口对谁都不说话，而被丢掉的恰恰是直接点名她的消息——群里看到的是"她不理人"。
    #[test]
    fn addressed_replies_have_their_own_wider_rate_budget() {
        let mut state = GroupInterjectionState::default();
        let started = Instant::now();
        let budget = ReplyBudgetLimits {
            rate_window: Duration::from_secs(600),
            unaddressed_limit: 10,
            addressed_limit: 20,
        };
        let gap = Duration::from_secs(0);
        // 真发出去的回复会在发送时被确认（`mark_group_reply_sent`），未确认的
        // 预留另有 30 秒兜底会被清掉——所以这里必须按真实形态落账。
        let sent = |state: &mut GroupInterjectionState, at: Instant, addressed: bool| {
            let class = if addressed {
                ReplyBudgetClass::Addressed
            } else {
                ReplyBudgetClass::Ambient
            };
            let reserved = reserve_visible_reply_slot(state, at, gap, budget, class);
            if reserved != ReplySlotReservation::Rejected {
                confirm_visible_reply_slot(state, at);
            }
            reserved != ReplySlotReservation::Rejected
        };

        // 先用未点名把普通额度打满（每次隔 1 秒，避开 gap）。
        for index in 0..10 {
            assert!(sent(
                &mut state,
                started + Duration::from_secs(index),
                false
            ));
        }
        // 未点名再要就被拒 …
        assert!(!sent(&mut state, started + Duration::from_secs(11), false));
        // … 但点名还能用自己那份额度。
        for index in 0..10 {
            assert!(
                sent(&mut state, started + Duration::from_secs(12 + index), true),
                "点名通道应还有额度（第 {} 条）",
                index + 1
            );
        }
        // 点名额度也满了：两类都拒绝（总闸是更宽的那份，不是无限）。
        assert!(!sent(&mut state, started + Duration::from_secs(30), true));
        assert!(!sent(&mut state, started + Duration::from_secs(31), false));
        // 窗口滑过之后重新放行。
        assert!(sent(&mut state, started + Duration::from_secs(601), false));
    }

    /// 管理员**点名**那一档额度也豁免：额度用完时仍然放行，而且照实记账
    /// （账本必须反映"她真的说了这么多"，否则下一个人的额度与日志都会失真）。
    ///
    /// 线上 2026-09-14 21:19：管理员 @ 她问话被静默丢掉（`replies_in_window=10/10`、
    /// `gap_secs=0`）——间隔早就豁免了，额度没有，于是"你直接问她"在热闹时段一样被吞。
    #[test]
    fn admin_addressed_replies_are_allowed_over_budget() {
        let mut state = GroupInterjectionState::default();
        let started = Instant::now();
        let budget = ReplyBudgetLimits {
            rate_window: Duration::from_secs(600),
            unaddressed_limit: 10,
            addressed_limit: 20,
        };
        let gap = Duration::from_secs(0);
        // 真发出去的回复会被确认；未确认的预留另有 30 秒兜底会被清掉——测试必须
        // 按真实形态落账，否则额度会被兜底"悄悄还回来"。
        let reserve_and_confirm =
            |state: &mut GroupInterjectionState, at: Instant, class: ReplyBudgetClass| {
                let reserved = reserve_visible_reply_slot(state, at, gap, budget, class);
                if reserved != ReplySlotReservation::Rejected {
                    confirm_visible_reply_slot(state, at);
                }
                reserved
            };

        // 先把两份额度都用满（10 条未点名 + 10 条点名）。
        for index in 0..10 {
            assert_eq!(
                reserve_and_confirm(
                    &mut state,
                    started + Duration::from_secs(index),
                    ReplyBudgetClass::Ambient,
                ),
                ReplySlotReservation::Reserved
            );
        }
        for index in 0..10 {
            assert_eq!(
                reserve_and_confirm(
                    &mut state,
                    started + Duration::from_secs(20 + index),
                    ReplyBudgetClass::Addressed,
                ),
                ReplySlotReservation::Reserved
            );
        }
        // 普通点名额度满了：拒绝。
        assert_eq!(
            reserve_and_confirm(
                &mut state,
                started + Duration::from_secs(40),
                ReplyBudgetClass::Addressed,
            ),
            ReplySlotReservation::Rejected
        );
        // 管理员点名：超额度也放行 …
        assert_eq!(
            reserve_and_confirm(
                &mut state,
                started + Duration::from_secs(41),
                ReplyBudgetClass::AddressedUncapped,
            ),
            ReplySlotReservation::ReservedOverBudget
        );
        // … 而且照实记账：窗口内确实多了一条（下一个人的账不会失真）。
        assert_eq!(state.visible_replies.len(), 21);
        // 未点名接话仍然被自己那份额度拦着（"她句句都回"的防线不受影响）。
        assert_eq!(
            reserve_and_confirm(
                &mut state,
                started + Duration::from_secs(42),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Rejected
        );
    }

    /// 由"是不是点名"和"是不是管理员"选档：只有管理员点名那一档豁免额度。
    #[test]
    fn budget_class_only_uncaps_admin_addressed_turns() {
        assert_eq!(
            reply_budget_class(true, true),
            ReplyBudgetClass::AddressedUncapped
        );
        assert_eq!(reply_budget_class(true, false), ReplyBudgetClass::Addressed);
        assert_eq!(reply_budget_class(false, true), ReplyBudgetClass::Ambient);
        assert_eq!(reply_budget_class(false, false), ReplyBudgetClass::Ambient);
    }

    /// 管理员豁免的是"等待间隔"，不是频率上限：线上 16:18 管理员那条点名
    /// 指令被 20 秒间隔静默丢掉（只差 110 毫秒），所以管理员发言一律 0 秒，
    /// 普通成员仍走点名档/防刷屏档。
    #[test]
    fn admins_skip_the_reply_gap_but_keep_everyone_elses_waiting() {
        let group = crate::config::get().group_interjection().clone();
        assert_eq!(paced_group_reply_gap_secs(true, true), 0);
        assert_eq!(paced_group_reply_gap_secs(false, true), 0);
        assert_eq!(
            paced_group_reply_gap_secs(true, false),
            group.effective_addressed_reply_gap_secs()
        );
        assert_eq!(
            paced_group_reply_gap_secs(false, false),
            group.reply_gap_secs()
        );
        // 豁免只影响间隔：频率上限仍是同一份配置，没有被"管理员"这条通道绕过。
        assert!(group.reply_rate_limit() > 0);
    }

    /// 兜底：既没确认也没归还的预留（调用方早退/取消/panic 之后的残余）
    /// 不该长期占位——否则一次异常就能把这个群的回复节奏锁死。
    #[test]
    fn unconfirmed_reservations_expire_as_a_backstop() {
        let mut state = GroupInterjectionState::default();
        let started = Instant::now();
        let gap = Duration::from_secs(90);
        let rate_window = Duration::from_secs(600);
        let rate_limit = 10;

        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started,
                gap,
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Reserved,
        );
        // 30 秒内仍算"这一轮正在准备回复"，继续挡住普通间隔。
        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started + Duration::from_secs(29),
                gap,
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Rejected,
        );
        // 超过兜底时限后不再占位，且不计入频率额度。
        assert_eq!(
            reserve_visible_reply_slot(
                &mut state,
                started + Duration::from_secs(31),
                gap,
                limits(rate_window, rate_limit),
                ReplyBudgetClass::Ambient,
            ),
            ReplySlotReservation::Reserved,
        );
        assert_eq!(state.visible_replies.len(), 1);
    }

    #[test]
    fn group_visible_reply_budget_enforces_gap_then_rate_window() {
        let mut state = GroupInterjectionState::default();
        let started = Instant::now();
        let gap = Duration::from_secs(90);
        let rate_window = Duration::from_secs(600);
        let rate_limit = 4;

        // 首次预留成功。
        assert!(reserve_sent_reply(
            &mut state,
            started,
            gap,
            rate_window,
            rate_limit
        ));
        // 冷却内（90 秒前）拒绝。
        assert!(!reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(45),
            gap,
            rate_window,
            rate_limit
        ));
        // 冷却期满后放行，直到窗口内额度用尽。
        for offset in [90, 180, 270] {
            assert!(reserve_sent_reply(
                &mut state,
                started + Duration::from_secs(offset),
                gap,
                rate_window,
                rate_limit
            ));
        }
        assert!(!reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(300),
            gap,
            rate_window,
            rate_limit
        ));
        // 窗口滑过 600 秒后最早的记录失效，重新释放名额。
        assert!(reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(601),
            gap,
            rate_window,
            rate_limit
        ));
    }

    /// 现场回归：群里有人 @ 她提问，21 秒前刚回过别人，被 90 秒间隔静默
    /// 丢掉，永远没有回答。被点名消息现在用更短的间隔，所以那条提问能进
    /// 生成；未点名的接话仍然吃满普通间隔，频率上限也仍然共享。
    #[test]
    fn addressed_messages_use_the_shorter_reply_gap_without_extra_rate_budget() {
        let mut state = GroupInterjectionState::default();
        let started = Instant::now();
        let normal_gap = Duration::from_secs(90);
        let addressed_gap = Duration::from_secs(20);
        let rate_window = Duration::from_secs(600);
        let rate_limit = 4;

        // 她刚回过一条普通消息。
        assert!(reserve_sent_reply(
            &mut state,
            started,
            normal_gap,
            rate_window,
            rate_limit
        ));
        // 5 秒后的点名提问：被点名间隔同样压住，不会被追着答。
        assert!(!reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(5),
            addressed_gap,
            rate_window,
            rate_limit
        ));
        // 21 秒后的点名提问：普通间隔（90s）拒绝，被点名间隔（20s）放行。
        assert!(!reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(21),
            normal_gap,
            rate_window,
            rate_limit
        ));
        assert!(reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(21),
            addressed_gap,
            rate_window,
            rate_limit
        ));
        // 被拒的尝试不写时间戳：别人紧接着问一句，不会因为她这次"差点回复"
        // 而被再压 20 秒。
        assert!(reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(45),
            addressed_gap,
            rate_window,
            rate_limit
        ));
        // 放松的只是等待，不是额度：窗口内第 4 条仍然放行 …
        assert!(reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(90),
            addressed_gap,
            rate_window,
            rate_limit
        ));
        // … 第 5 条被频率上限拒绝，窗口滑过 600 秒后额度才重新释放。
        assert!(!reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(200),
            addressed_gap,
            rate_window,
            rate_limit
        ));
        assert!(reserve_sent_reply(
            &mut state,
            started + Duration::from_secs(621),
            addressed_gap,
            rate_window,
            rate_limit
        ));
    }

    #[test]
    fn unsolicited_model_decisions_obey_cooldown_and_rate_budget() {
        let now = Instant::now();
        let mut state = GroupInterjectionState::default();
        assert!(decision_budget_available(
            &state,
            now,
            Duration::from_secs(60),
            3,
        ));
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(30));
        assert!(!decision_budget_available(
            &state,
            now,
            Duration::from_secs(60),
            3,
        ));
        state.decision_attempts.clear();
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(120));
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(90));
        state
            .decision_attempts
            .push_back(now - Duration::from_secs(30));
        assert!(!decision_budget_available(
            &state,
            now,
            Duration::from_secs(60),
            3,
        ));
        prune_decision_attempts(&mut state, now, Duration::from_secs(100));
        assert_eq!(state.decision_attempts.len(), 2);
    }

    #[test]
    fn executive_keep_queues_behind_active_work_but_other_decisions_regenerate() {
        use WindowQueueDecision::{Process, Queue, QueueThenDrain};

        assert_eq!(
            window_queue_decision(true, false, false, OutgoingExecutiveDecision::Keep, false),
            Queue
        );
        for decision in [
            OutgoingExecutiveDecision::Rewrite,
            OutgoingExecutiveDecision::Merge,
            OutgoingExecutiveDecision::Defer,
        ] {
            assert_eq!(
                window_queue_decision(true, false, false, decision, false),
                Process
            );
        }
        assert_eq!(
            window_queue_decision(false, false, false, OutgoingExecutiveDecision::Keep, false),
            Process
        );
        assert_eq!(
            window_queue_decision(false, false, false, OutgoingExecutiveDecision::Keep, true),
            Queue
        );
        assert_eq!(
            window_queue_decision(
                false,
                true,
                false,
                OutgoingExecutiveDecision::Rewrite,
                false
            ),
            QueueThenDrain
        );
        assert_eq!(
            window_queue_decision(
                false,
                false,
                true,
                OutgoingExecutiveDecision::Rewrite,
                false
            ),
            Queue
        );
    }

    /// 线上回归（2026-09-14 18:33，主群静了四分多钟）：队列曾经因为"自己非空"
    /// 就把后面每条消息继续排进去，而排空只在 Host 回合收尾时触发——Core
    /// 收尾、静默收尾都不触发，于是队列永远排不完（journal 里排队 8 次、
    /// 排空 0 次）。有在途工作时才排队；没有在途工作时入队保序后立刻排空。
    #[test]
    fn a_waiting_room_without_in_flight_work_is_not_a_reason_to_keep_queueing() {
        use WindowQueueDecision::{Process, Queue, QueueThenDrain};

        // 队列非空 + 没有任何在途工作 = 残局：排队（保序）并要求立刻排空。
        for decision in [
            OutgoingExecutiveDecision::Keep,
            OutgoingExecutiveDecision::Rewrite,
            OutgoingExecutiveDecision::Merge,
            OutgoingExecutiveDecision::Defer,
        ] {
            assert_eq!(
                window_queue_decision(false, true, false, decision, false),
                QueueThenDrain,
                "残局必须要求立刻排空，否则队列会自锁（decision={decision:?}）"
            );
        }
        // 有在途工作（活跃回合 / 待定 admission / 保留下来的 prepared）时，
        // 队列非空仍然要求排队：这些情况下确实有人在前面收尾。
        assert_eq!(
            window_queue_decision(true, true, false, OutgoingExecutiveDecision::Rewrite, false),
            Queue
        );
        assert_eq!(
            window_queue_decision(false, true, true, OutgoingExecutiveDecision::Rewrite, false),
            Queue
        );
        assert_eq!(
            window_queue_decision(false, true, false, OutgoingExecutiveDecision::Rewrite, true),
            Queue
        );
        // 队列本来就空、又没有在途工作：直接处理，不进 waiting room。
        assert_eq!(
            window_queue_decision(
                false,
                false,
                false,
                OutgoingExecutiveDecision::Rewrite,
                false
            ),
            Process
        );
    }

    #[test]
    fn production_group_semantic_helper_keeps_no_effect_observation_prepared() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_340_001);
                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert!(mark_active(initial.ticket).await);
                let outgoing = prepare_outgoing(
                    initial.ticket,
                    outgoing_fingerprint("observation does not change this"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply should prepare during semantic work");

                let refined = admit_understood_group_turn(
                    initial,
                    &MessageUnderstanding::default(),
                    false,
                    false,
                )
                .await
                .expect("ingress should remain current");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Keep);
                assert!(commit_outgoing(outgoing).await);
                mark_outgoing_failed(outgoing).await;
            });
    }

    #[test]
    fn production_group_semantic_helper_merges_by_regenerating() {
        kovi::tokio::runtime::Runtime::new()
            .expect("应创建测试运行时")
            .block_on(async {
                let scope = ReplyScope::Group(9_340_002);
                let initial = ConversationCoordinator::begin_incoming(scope).await;
                assert!(mark_active(initial.ticket).await);
                let outgoing = prepare_outgoing(
                    initial.ticket,
                    outgoing_fingerprint("reply before compatible follow-up"),
                    OutgoingSource::Reply,
                )
                .await
                .expect("reply should prepare during semantic work");
                let understanding = MessageUnderstanding {
                    conversation_relevant: true,
                    ..MessageUnderstanding::default()
                };

                let refined = admit_understood_group_turn(initial, &understanding, true, false)
                    .await
                    .expect("ingress should remain current");

                assert_eq!(refined.decision, OutgoingExecutiveDecision::Merge);
                assert_ne!(refined.ticket, initial.ticket);
                assert_eq!(
                    test_outgoing_state(outgoing).await,
                    Some(OutgoingState::Superseded)
                );
                assert!(!commit_outgoing(outgoing).await);
            });
    }

    #[test]
    fn formal_commands_are_classified_before_chat_processing() {
        assert!(is_group_admin_command("#健康检查"));
        assert!(is_restricted_command(" #禁言 "));
        assert!(is_restricted_command("#教芸汐 这个表情是开心"));
        assert!(is_restricted_command("#识图"));
        assert!(!is_restricted_command("芸汐，今天开心吗"));
    }

    #[test]
    fn direct_mentions_are_limited_by_rate_window_then_cooled_down() {
        let mut state = DirectTriggerState::default();
        let started = Instant::now();
        let cooldown = Duration::from_secs(600);
        let rate_window = Duration::from_secs(60);

        assert!(!suppress_direct_trigger(
            &mut state,
            started,
            cooldown,
            rate_window,
            4,
        ));
        for offset in [5, 10, 15] {
            assert!(!suppress_direct_trigger(
                &mut state,
                started + Duration::from_secs(offset),
                cooldown,
                Duration::from_secs(60),
                4,
            ));
        }
        assert!(suppress_direct_trigger(
            &mut state,
            started + Duration::from_secs(20),
            cooldown,
            rate_window,
            4,
        ));
        assert!(suppress_direct_trigger(
            &mut state,
            started + Duration::from_secs(30),
            cooldown,
            rate_window,
            4,
        ));
        assert!(!suppress_direct_trigger(
            &mut state,
            started + Duration::from_secs(620),
            cooldown,
            rate_window,
            4,
        ));
    }
}
