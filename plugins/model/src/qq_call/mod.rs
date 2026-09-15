//! QQ 实时语音通话。
//!
//! QQ 的语音通话能力不在 OneBot 11 协议里，NapCat 也没有对应的公开接口。
//! 现实可行的做法是把通话媒体链路交给一个独立的 **NapCat AV 桥**（服务器上
//! 单独安装的 NapCat 外部插件 + 第二个加载 QQ 自带 AVSDK 的进程）：桥负责
//! 来电信令、自动接听和原生音频设备，并通过回环 HTTP 暴露一个最小的通话
//! 状态接口，同时把对端声音和芸汐的声音分别接到一套隔离的 PulseAudio
//! 虚拟设备上。
//!
//! 本模块因此只做四件事：
//!
//! 1. 轮询桥的 `GET /v1/calls/current`，把"接通"当成一次会话的开始；
//! 2. 从桥的 speaker monitor 采集对端声音并做能量 VAD 切段；
//! 3. 调用服务器上的本地语音服务做识别，再用芸汐的私聊人设和模型生成回复；
//! 4. 调用本地语音服务合成，流式写入桥的虚拟麦克风。
//!
//! 默认关闭；`qq_call.enabled = false` 时整个模块不会启动任何任务。

mod audio;
mod bridge;
pub(crate) mod diagnostics;
mod session;
mod vad;

use crate::config;
use crate::model::{MessageDestination, OutgoingSource, send_tracked_message_with_revalidation};
use bridge::{BridgeClient, CallPhase, CallState};
use diagnostics::{caller_label, phase_description};
use session::caller_is_allowed;
use std::sync::Arc;
use std::time::Duration;

/// 同一类桥错误的最短重复日志间隔，避免桥长时间离线时刷日志。
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(300);

/// 漏接来电通知的发送超时（通知失败绝不能拖住通话状态机）。
const MISSED_NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

/// 主动外呼后等"电话真的响起来"的窗口。桥受理 ≠ AVSDK 真的拨号。
const DIAL_CONFIRM_WINDOW: Duration = Duration::from_secs(6);

/// 同一个窗口的秒数形式。
///
/// 给需要把它写进话术的地方用（工具结果要说"几秒内没等到回执"）。暴露这一个常量
/// 而不是各处再写一遍 `6`：抄出来的那份永远不会跟着上面改。
pub(crate) const DIAL_CONFIRM_WINDOW_SECS: u64 = DIAL_CONFIRM_WINDOW.as_secs();

/// 私聊指令 `#通话自检 [问题]` 的实现：不通话也能验证电话里的工具链路。
///
/// 试跑：清单与真通话一致（否则验不出"她本来会不会调"），但只有只读工具真跑，
/// 有副作用的动作只记录不执行。授权门槛和打电话一致——能在电话里用工具的人，
/// 才有必要自检。
pub(crate) async fn run_tool_self_test(
    bot: &std::sync::Arc<kovi::RuntimeBot>,
    requester: i64,
    question: &str,
) -> String {
    let config = config::get().qq_call().clone();
    if !config.enabled() {
        return "QQ 语音通话没启用，谈不上电话里的工具。".to_string();
    }
    if !config.phone_tools_enabled() {
        return "通话工具被关掉了（qq_call.phone_tools_enabled = false），\
                所以电话里她只能说话。"
            .to_string();
    }
    if !caller_is_allowed(&config, bot.get_main_admin().ok(), requester).await {
        return "你不在通话授权名单里，用不上通话工具，我也就不自检了。".to_string();
    }
    session::self_test(bot, &config, requester, question).await
}

/// 私聊指令 `#打给我` 的实现：让芸汐主动拨给发起者。
///
/// 实际可达这里的是管理员：**命令只由管理员下达**，`#打给我` 在私聊分发层属于受限命令，
/// 非管理员发的会被静默丢弃（见 `model/private.rs`）。这里另按通话名单判一次，规则是
/// "谁让我打，我就打给谁"，不接受任意号码，免得变成骚扰工具。**打完必须确认电话真的响了**：AVSDK 的外呼命令可能被丢弃，
/// 桥返回成功不代表拨出去了，所以这里几秒内轮询 AVSDK 回执，据实回复。
/// 通话通道是否已配置并允许外呼。
///
/// **只留这一处定义**，因为两个地方必须永远一致：工具注册表决定要不要声明 `call.start`，
/// 宿主的能力快照决定要不要声称自己会 `StartCall`。两边不一致就会出现"Core 以为能打、
/// 清单里却没有这个工具"，或者更糟的反过来——所以它不能有两份实现。
pub(crate) fn outgoing_available() -> bool {
    let config = config::get();
    let call = config.qq_call();
    call.enabled() && call.outgoing_enabled()
}

/// 禁止外呼的时段。
///
/// 支持跨午夜（`23:00-08:00`）。判据用**当天的分钟数**而不是日期时间，比较简单也够用：
/// 时区取本机时区的当前时刻，调用方负责给。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuietHours {
    start_minute: u32,
    end_minute: u32,
}

impl QuietHours {
    /// 解析 `"23:00-08:00"`。留空或格式非法返回 `None`（配置加载时会先报错拦住非法值）。
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let (start, end) = raw.trim().split_once('-')?;
        Some(Self {
            start_minute: parse_hh_mm(start)?,
            end_minute: parse_hh_mm(end)?,
        })
    }

    /// 这个时刻是否落在静默时段内。
    pub(crate) fn contains_minute(self, minute: u32) -> bool {
        if self.start_minute == self.end_minute {
            // 起止相同：视为"整天静默"。写成等价区间会让它变成"从不静默"，
            // 那和配置者的意图正好相反。
            return true;
        }
        if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute).contains(&minute)
        } else {
            // 跨午夜：22:00-08:00 = [22:00, 24:00) ∪ [00:00, 08:00)
            minute >= self.start_minute || minute < self.end_minute
        }
    }
}

fn parse_hh_mm(raw: &str) -> Option<u32> {
    let (hour, minute) = raw.trim().split_once(':')?;
    let hour: u32 = hour.trim().parse().ok()?;
    let minute: u32 = minute.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

fn minute_of_day(now: chrono::DateTime<chrono::Local>) -> u32 {
    use chrono::Timelike;
    now.hour() * 60 + now.minute()
}

/// 外呼账本：每人每天打了几次、上次是什么时候。
///
/// **进程内、重启即清**。这是一个**频次闸门**，不是配额账本：它的作用是在配置真的开启
/// 之后拦住"短时间内反复拨同一个人"，而不是保证一个跨重启的精确日上限。默认两道限制都
/// 是关的（见 `QqCallConfig`），所以这份实现只在有人主动打开开关之后才起作用；真要精确
/// 的跨重启计数，应该落 Redis/Postgres，那是另一个决定。
#[derive(Debug, Default)]
struct DialLedger {
    entries: std::collections::HashMap<i64, DialRecord>,
}

#[derive(Debug, Clone, Copy)]
struct DialRecord {
    day: chrono::NaiveDate,
    count: u32,
    last_at: chrono::DateTime<chrono::Local>,
}

static DIAL_LEDGER: std::sync::LazyLock<std::sync::Mutex<DialLedger>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(DialLedger::default()));

/// 账本条数上限：它按人记，人不该无界增长。
const MAX_DIAL_LEDGER_ENTRIES: usize = 512;

fn with_ledger<T>(action: impl FnOnce(&mut DialLedger) -> T) -> T {
    let mut guard = DIAL_LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action(&mut guard)
}

/// 现在这会儿，频次上允许拨给这个人吗（只查，不记账）。
fn rate_limit_allows(
    config: &config::QqCallConfig,
    peer: i64,
    now: chrono::DateTime<chrono::Local>,
) -> bool {
    let min_interval = config.outgoing_min_interval_secs();
    let daily_limit = config.outgoing_daily_limit();
    if min_interval == 0 && daily_limit == 0 {
        return true;
    }
    with_ledger(|ledger| {
        let Some(record) = ledger.entries.get(&peer) else {
            return true;
        };
        if min_interval > 0 {
            let elapsed = now
                .signed_duration_since(record.last_at)
                .num_seconds()
                .max(0) as u64;
            if elapsed < min_interval {
                return false;
            }
        }
        if daily_limit > 0 && record.day == now.date_naive() && record.count >= daily_limit {
            return false;
        }
        true
    })
}

/// 记一次真的拨出去了的外呼。
fn record_dial(peer: i64, now: chrono::DateTime<chrono::Local>) {
    with_ledger(|ledger| {
        if !ledger.entries.contains_key(&peer) && ledger.entries.len() >= MAX_DIAL_LEDGER_ENTRIES {
            // 丢最早的那个，保持有界而不是拒绝新的。
            if let Some(oldest) = ledger
                .entries
                .iter()
                .min_by_key(|(_, record)| record.last_at)
                .map(|(peer, _)| *peer)
            {
                ledger.entries.remove(&oldest);
            }
        }
        let entry = ledger.entries.entry(peer).or_insert(DialRecord {
            day: now.date_naive(),
            count: 0,
            last_at: now,
        });
        if entry.day != now.date_naive() {
            entry.day = now.date_naive();
            entry.count = 0;
        }
        entry.count += 1;
        entry.last_at = now;
    });
}

/// 静默时段与频次闸门。两道都默认关着（用户 2026-09-16 的决定），但开关一动就生效。
fn outgoing_guard(
    config: &config::QqCallConfig,
    peer: i64,
    now: chrono::DateTime<chrono::Local>,
) -> Option<DialOutcome> {
    if config
        .outgoing_quiet_hours()
        .is_some_and(|quiet| quiet.contains_minute(minute_of_day(now)))
    {
        return Some(DialOutcome::QuietHours);
    }
    if !rate_limit_allows(config, peer, now) {
        return Some(DialOutcome::RateLimited);
    }
    None
}

/// 现在能不能拨给这个人——**只查，不拨**。
///
/// 给"这次主动接触该用哪种媒介"那一步用：先问能不能，再决定用不用电话。反过来的话，
/// 选了电话才发现拨不出去，就只能要么放弃这次接触、要么把一句"喂，是我"当正文发出去，
/// 两种都不好。判据与 `dial_peer` 同一套（通道开关、外呼开关、通话名单、是否正通话）。
pub(crate) async fn can_dial(
    config: &config::QqCallConfig,
    main_admin: Option<i64>,
    peer: i64,
) -> bool {
    if !config.enabled() || !config.outgoing_enabled() {
        return false;
    }
    if !caller_is_allowed(config, main_admin, peer).await {
        return false;
    }
    if outgoing_guard(config, peer, chrono::Local::now()).is_some() {
        return false;
    }
    let Ok(client) = BridgeClient::new(config) else {
        return false;
    };
    match client.current_call().await {
        Ok(state) => !state.phase().is_live(),
        // 桥读不到时保守判"不能打"：宁可发消息，也不要拨一个状态未知的号。
        Err(_) => false,
    }
}

/// 待用开场白：从"拨出去"到桥报告"接通"之间隔着另一条任务链（调度器在轮询桥状态），
/// 所以投递这一侧只能把要说的话先存下，等会话真的建立时再取。
///
/// 带 TTL 而不是永久保存：一通没接的电话不该把开场白留到几小时后那通**来电**上用。
/// 键是被叫 QQ 号——与调度器认领外呼通话用的 `dialedUin` 是同一个值。
static PENDING_OPENINGS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<i64, (String, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 开场白在待用区最多停留多久。
const PENDING_OPENING_TTL: Duration = Duration::from_secs(300);
/// 待用开场白的条数上限：这是宿主自产的短字符串，但也不该无界。
const MAX_PENDING_OPENINGS: usize = 64;

fn with_pending_openings<T>(
    action: impl FnOnce(&mut std::collections::HashMap<i64, (String, std::time::Instant)>) -> T,
) -> T {
    let mut guard = PENDING_OPENINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = std::time::Instant::now();
    guard.retain(|_, (_, stored)| now.duration_since(*stored) < PENDING_OPENING_TTL);
    action(&mut guard)
}

/// 取走这个人的待用开场白（取走即删除：一句话只该被一通电话用一次）。
fn take_opening(peer: i64) -> Option<String> {
    with_pending_openings(|openings| openings.remove(&peer).map(|(opening, _)| opening))
}

/// 记住这次拨号要说的第一句话。
fn remember_opening(peer: i64, opening: &str) {
    let opening = opening.trim();
    if opening.is_empty() {
        return;
    }
    with_pending_openings(|openings| {
        if openings.len() >= MAX_PENDING_OPENINGS {
            // 先丢最早的那个，保持有界而不是拒绝新的。
            if let Some(oldest) = openings
                .iter()
                .min_by_key(|(_, (_, stored))| *stored)
                .map(|(peer, _)| *peer)
            {
                openings.remove(&oldest);
            }
        }
        openings.insert(peer, (opening.to_owned(), std::time::Instant::now()));
    });
}

/// 一次外呼尝试的结局。
///
/// 抽出来是为了让两条触发路径共用**同一份**判定：管理员私聊命令 `#打给我`
/// 和她的 `call.start` 工具。措辞不同（命令像回话，工具像工具结果），但"能不能打、
/// 打没打出去"必须只有一个答案——否则迟早出现"命令说成功了、工具说没拨出去"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DialOutcome {
    /// AVSDK 回了执，邀请真的发出去了（真机上是手机随即响铃）。
    Dialed,
    /// 桥受理了拨号请求，但在确认窗口内没等到 AVSDK 回执——多半没拨出去。
    /// 不假装成功：文档里那条"不会假装成功"就是针对这种情况。
    NoReceipt,
    /// `qq_call.enabled = false`。
    Disabled,
    /// `qq_call.outgoing_enabled = false`。
    OutgoingDisabled,
    /// 目标不在通话授权名单里（主/副管理员 ∪ 数据库名单 ∪ 静态配置）。
    NotAuthorized,
    /// 现在正通着话（可能还没进房）。
    AlreadyInCall,
    /// 落在配置的静默时段里（`qq_call.outgoing_quiet_hours`）。
    QuietHours,
    /// 撞上频次闸门（每人每天上限 / 最短间隔）。
    RateLimited,
    /// 桥不可用，或拨号请求本身失败。
    BridgeUnavailable(String),
}

impl DialOutcome {
    /// 拨号邀请是否真的发出去了。
    pub(crate) fn reached_peer(&self) -> bool {
        matches!(self, Self::Dialed)
    }

    /// 稳定的事件/日志取值。不要用 `Debug`：那个以后改字段名会悄悄改掉指标口径。
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Dialed => "dialed",
            Self::NoReceipt => "no_receipt",
            Self::Disabled => "disabled",
            Self::OutgoingDisabled => "outgoing_disabled",
            Self::NotAuthorized => "not_authorized",
            Self::AlreadyInCall => "already_in_call",
            Self::QuietHours => "quiet_hours",
            Self::RateLimited => "rate_limited",
            Self::BridgeUnavailable(_) => "bridge_unavailable",
        }
    }
}

/// 拨给某个人。**这是唯一的外呼入口**，`#打给我` 与 `call.start` 都走它，
/// 免得"禁用开关"或"授权名单"只在其中一条路径上生效。
///
/// `peer` 必须是调用方自己解析出来的 QQ 号，**绝不能来自模型填的参数**：
/// "打给谁"的决定必须在进来之前就绑定好。
pub(crate) async fn dial_peer(
    config: &config::QqCallConfig,
    main_admin: Option<i64>,
    peer: i64,
) -> DialOutcome {
    dial_peer_with_opening(config, main_admin, peer, None).await
}

/// 拨给某个人，并指定接通后她开口说的第一句话。
///
/// 开场白在**拨号之前**就存下、拨不出去就撤销：接通可能发生在确认回执之后不到一秒，
/// 存晚了会有"会话已经开始、开场白还没到"的窗口。撤销是因为一通没拨出去的电话不该把
/// 这句话留到之后那通**来电**上用。
pub(crate) async fn dial_peer_with_opening(
    config: &config::QqCallConfig,
    main_admin: Option<i64>,
    peer: i64,
    opening: Option<&str>,
) -> DialOutcome {
    if let Some(opening) = opening {
        remember_opening(peer, opening);
    }
    let outcome = dial_peer_inner(config, main_admin, peer).await;
    if !outcome.reached_peer() {
        with_pending_openings(|openings| openings.remove(&peer));
    }
    // 每一次外呼尝试都留一条可事后复盘的记录：拨给谁、成没成、为什么。
    // 打电话是不可撤销的动作，事后要能回答"这通是谁让它打的、结果怎样"。
    println!(
        "[INFO] QQ 语音外呼尝试：peer={peer} outcome={} reached={}",
        outcome.as_str(),
        outcome.reached_peer()
    );
    outcome
}

async fn dial_peer_inner(
    config: &config::QqCallConfig,
    main_admin: Option<i64>,
    peer: i64,
) -> DialOutcome {
    if !config.enabled() {
        return DialOutcome::Disabled;
    }
    if !config.outgoing_enabled() {
        return DialOutcome::OutgoingDisabled;
    }
    if !caller_is_allowed(config, main_admin, peer).await {
        return DialOutcome::NotAuthorized;
    }
    let now = chrono::Local::now();
    // 静默时段与频次：默认两道都关着，但配置一动就在这里挡住。
    if let Some(blocked) = outgoing_guard(config, peer, now) {
        return blocked;
    }
    let client = match BridgeClient::new(config) {
        Ok(client) => client,
        Err(error) => return DialOutcome::BridgeUnavailable(error.to_string()),
    };
    if let Ok(state) = client.current_call().await
        && state.phase().is_live()
    {
        return DialOutcome::AlreadyInCall;
    }
    if let Err(error) = client.dial(peer).await {
        return DialOutcome::BridgeUnavailable(error.to_string());
    }
    // 确认电话真的拨出去了：桥受理 ≠ AVSDK 真的拨号。判据是插件记下的外呼回执
    // （AVSDK 回报"对方是否在线"），因为呼出的通话不会让桥进入 ringing/connected。
    let deadline = std::time::Instant::now() + DIAL_CONFIRM_WINDOW;
    while std::time::Instant::now() < deadline {
        kovi::tokio::time::sleep(Duration::from_millis(400)).await;
        if let Ok(state) = client.current_call().await
            && (state.phase().is_live() || state.dial_reached_at.is_some())
        {
            // 只有真的拨出去了才记账：没拨出去的不该占用频次额度。
            record_dial(peer, now);
            return DialOutcome::Dialed;
        }
    }
    DialOutcome::NoReceipt
}

pub(crate) async fn request_outgoing_call(bot: &kovi::RuntimeBot, requester: i64) -> String {
    let config = config::get().qq_call().clone();
    match dial_peer(&config, bot.get_main_admin().ok(), requester).await {
        DialOutcome::Dialed => "好，我打给你啦，接一下～".to_string(),
        DialOutcome::NoReceipt => {
            "我让桥拨了，但没等到 AVSDK 的回执，多半是没拨出去——这个我还在查。".to_string()
        }
        DialOutcome::Disabled => "QQ 语音通话没启用，打不了电话。".to_string(),
        DialOutcome::OutgoingDisabled => {
            "主动外呼被关掉了（qq_call.outgoing_enabled = false）。".to_string()
        }
        DialOutcome::NotAuthorized => "你不在通话授权名单里，我不能打给你。".to_string(),
        DialOutcome::AlreadyInCall => "现在正通着话呢，等这通结束我再打给你。".to_string(),
        DialOutcome::QuietHours => "现在是配置里的静默时段，我不往外打电话。".to_string(),
        DialOutcome::RateLimited => {
            "今天打给你的次数已经到上限（或间隔太短），先不打了。".to_string()
        }
        DialOutcome::BridgeUnavailable(error) => format!("打不出去：{error}"),
    }
}

/// 启动 QQ 语音通话调度器。默认关闭，未启用时立即返回。
pub(crate) async fn start_scheduler(bot: Arc<kovi::RuntimeBot>) {
    let config = config::get().qq_call().clone();
    if !config.enabled() {
        println!("[INFO] QQ 语音通话已关闭");
        return;
    }
    let client = match BridgeClient::new(&config) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("[ERROR] QQ 语音通话无法启动: {error}");
            return;
        }
    };
    println!(
        "[INFO] QQ 语音通话已启用（桥 {}，轮询 {} 毫秒）",
        config.bridge_url(),
        config.poll_interval_ms()
    );

    let poll_interval = Duration::from_millis(config.poll_interval_ms());
    // 已处理的通话标识。桥重启或来电者变化时会换一个新值，避免同一次通话
    // 被反复启动；通话结束后由"阶段不再活跃"清空。
    let mut identity: Option<String> = None;
    let mut handled = false;
    let mut last_error: Option<String> = None;
    let mut last_error_at: Option<std::time::Instant> = None;
    // 上一次看到的阶段。桥的报告是轮询出来的，只有变化才值得写日志：否则
    // "一直停在 ringing"这类关键事实会被淹没在重复输出里。
    let mut observed_phase = CallPhase::Idle;
    // 这一次通话是否已经记进"最近一次通话"摘要，避免桥在同一阶段反复上报时
    // 覆盖第一次来电的时间。
    let mut traced = false;

    loop {
        match client.current_call().await {
            Ok(state) => {
                last_error = None;
                last_error_at = None;
                let phase = state.phase();
                if phase != observed_phase {
                    report_phase_change(
                        phase,
                        observed_phase,
                        &state,
                        &config,
                        &bot,
                        traced,
                        handled,
                    )
                    .await;
                    traced = phase.is_live();
                    observed_phase = phase;
                }
                if !phase.is_live() {
                    identity = None;
                    handled = false;
                } else if phase == CallPhase::Connected {
                    if identity.as_deref() != state.invite_at.as_deref() {
                        identity = state.invite_at.clone();
                        handled = false;
                    }
                    if !handled {
                        handled = true;
                        // 主动拨出去的通话没有"来电者"：用桥记下的被叫号当对端，
                        // 否则会按未知来电处理并婉拒——等于自己拒接自己。
                        let mut effective = state.clone();
                        if let Some(dialed) = state.dialed_uin {
                            println!("[INFO] QQ 语音通话是主动外呼（被叫 {dialed}）");
                            effective.caller_uin = Some(dialed.to_string());
                            effective.caller_name = None;
                        }
                        // 外呼时存下的开场白在这里被取走；来电没有存过，取到 None。
                        let opening = state.dialed_uin.and_then(take_opening);
                        if let Err(error) =
                            session::run(Arc::clone(&bot), &config, &client, &effective, opening)
                                .await
                        {
                            eprintln!("[ERROR] QQ 语音通话异常结束: {error}");
                        }
                        continue;
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                let now = std::time::Instant::now();
                let should_log = last_error.as_deref() != Some(message.as_str())
                    || last_error_at
                        .map(|at| now.duration_since(at) >= ERROR_LOG_INTERVAL)
                        .unwrap_or(true);
                if should_log {
                    eprintln!("[ERROR] QQ 语音通话无法读取桥状态: {message}");
                    last_error_at = Some(now);
                }
                last_error = Some(message);
            }
        }
        kovi::tokio::time::sleep(poll_interval).await;
    }
}

/// 这通电话该怎么记进认知层。
///
/// 纯函数，方便把三种结局的判据钉住。**只分三种**是因为桥分不出更细的：外呼一律回
/// `ended`，它自己也分不出"对方拒接"和"接通后很快挂断"（见 `docs/qq-call.md`）。
/// 编一个更细的枚举会把猜测说成事实。
///
/// - `allowed == Some(false)` 且是别人打来的 → 名单外婉拒（外呼不可能出现这种，
///   拨号前就查过名单）；
/// - 进过房 → `Completed`；
/// - 其余 → `Unanswered`（外呼没接 / 来电漏接）。
fn call_outcome(
    initiated_by_self: bool,
    allowed: Option<bool>,
    connected: bool,
) -> yunxi_core::CallOutcome {
    if !initiated_by_self && allowed == Some(false) {
        return yunxi_core::CallOutcome::Refused;
    }
    if connected {
        yunxi_core::CallOutcome::Completed
    } else {
        yunxi_core::CallOutcome::Unanswered
    }
}

/// 把通话结束通报给 Core。
///
/// 只记日志、绝不影响通话收尾（`record_call_ended` 自己吞掉所有失败）。
async fn record_core_call_ended(state: &CallState, allowed: Option<bool>, connected: bool) {
    let initiated_by_self = state.dialed_uin.is_some();
    let Some(peer) = state.dialed_uin.or_else(|| state.caller()) else {
        // 连对端都解析不出来：没有可归属的人，Core 那边也没法把事件挂到谁身上。
        return;
    };
    let outcome = call_outcome(initiated_by_self, allowed, connected);
    crate::yunxi::events::record_call_ended(
        peer,
        initiated_by_self,
        outcome,
        diagnostics::last_call_duration_secs(),
    )
    .await;
}

/// 这次阶段变化算不算"漏接"。
///
/// 两个条件缺一不可：**上一阶段还在通话中**（说明这个进程亲眼见过它振铃——
/// 光看 `!connected` 会把"启动时桥里残留的上一次结束状态"误判成漏接，2026-09-12
/// 就这么给一通其实接通了的电话发过通知），以及**这一通从未进房**。
fn is_missed_call(previous: CallPhase, connected: bool) -> bool {
    previous.is_live() && !connected
}

/// 漏接来电通知：桥看到过邀请、但整通从未进房时，主动私聊告诉主管理员。
///
/// 以前这种失败完全静默——你只能从"她没接"察觉；日志里也只有一行 WARN。
/// 通知带上来电者（解析不出来就不编造）并附一条诊断行（停在哪一阶段、endReason），
/// 发送用带幂等键的受跟踪通道，且带超时——通知失败绝不能拖住通话状态机。
async fn notify_missed_call(
    bot: &kovi::RuntimeBot,
    config: &crate::config::QqCallConfig,
    state: &CallState,
    previous: CallPhase,
) {
    if !config.notify_missed_calls() {
        return;
    }
    let Ok(admin) = bot.get_main_admin() else {
        return;
    };
    let caller = state.caller();
    let caller_name = state.caller_name.as_deref();
    eprintln!(
        "[WARN] QQ 语音通话漏接（有过邀请但从未进房，阶段停在 {}，endReason={:?}）: {}",
        previous.as_str(),
        state.end_reason,
        caller_label(caller, caller_name)
    );
    let idempotency = format!(
        "qq_call:missed:{}",
        state.invite_at.as_deref().unwrap_or("unknown")
    );
    let notice = diagnostics::missed_call_notice(caller, caller_name);
    let result = kovi::tokio::time::timeout(
        MISSED_NOTICE_TIMEOUT,
        send_tracked_message_with_revalidation(
            bot,
            MessageDestination::Private(admin),
            kovi::Message::from(notice),
            OutgoingSource::Proactive,
            Some(&idempotency),
            || async { true },
        ),
    )
    .await;
    match result {
        Ok(Ok(_)) => println!("[INFO] 漏接来电已私聊通知主管理员"),
        Ok(Err(error)) => eprintln!("[WARN] 漏接来电通知发送失败: {error}"),
        Err(_) => eprintln!("[WARN] 漏接来电通知发送超时"),
    }
}

/// 来电、有没有真正进房、名单里有没有这位来电者。
/// 把桥的阶段变化写进日志，并在"来电/进房/结束"三个关键点留下可见痕迹。
///
/// 机器人无法接听电话（桥自动接听），所以用户能看到的只有这里：桥有没有上报
/// 来电、有没有真正进房、名单里有没有这位来电者。
async fn report_phase_change(
    phase: CallPhase,
    previous: CallPhase,
    state: &CallState,
    config: &crate::config::QqCallConfig,
    bot: &kovi::RuntimeBot,
    traced: bool,
    // 这一通里我们是否真的起过一次会话。外呼的通知观察窗口不进 ringing/connected，
    // 所以"进过房"对外呼只能靠这个判据，不能读诊断里那份（那可能是上一通的）。
    session_ran: bool,
) {
    let caller = state.caller();
    let caller_name = state.caller_name.as_deref();
    let label = diagnostics::call_label(state.dialed_uin, caller, caller_name);
    // 先取"这通有没有进过房"与授权结果：下面 note_call_ended / begin_call 会把它清掉。
    // 外呼读不到有效的授权记录（外呼没有"来电者"，`begin_call` 不会被调用），所以
    // 那个字段只用于别人打来的电话，判据见 `call_outcome`。
    let connected = diagnostics::last_call_connected() || session_ran;
    let allowed = diagnostics::last_call_allowed();

    // 这次通话的第一次可见阶段：记下来电者与授权结果，供 `#通话状态` 事后回看。
    if phase.is_live() && !traced {
        let allowed = match caller {
            Some(caller) => caller_is_allowed(config, bot.get_main_admin().ok(), caller).await,
            None => false,
        };
        diagnostics::begin_call(caller, caller_name, allowed);
    }

    match phase {
        CallPhase::Ringing => {
            let authorization = match diagnostics::last_call_allowed() {
                Some(true) => "已授权",
                Some(false) => "未授权",
                None => "未知",
            };
            println!(
                "[INFO] QQ 语音通话来电振铃: {label}；通话授权: {authorization}（桥会自动接听，名单外只播报婉拒）"
            );
        }
        CallPhase::Accepting | CallPhase::Accepted => println!(
            "[INFO] QQ 语音通话桥正在接听: {label}（阶段 {}）",
            phase.as_str()
        ),
        CallPhase::Connected => {
            diagnostics::mark_connected();
            println!("[INFO] QQ 语音通话已进房: {label}（桥已接好音频设备）");
        }
        CallPhase::Ending => {
            println!("[INFO] QQ 语音通话正在挂断: {label}（桥已受理挂断请求，等待房间销毁）")
        }
        CallPhase::Ended => {
            match state.dialed_uin {
                Some(_) => {
                    // 呼出的通话不会让桥进入 ringing/connected，所以走到 ended 是正常收尾，
                    // 不是"来电没接住"。
                    println!(
                        "[INFO] QQ 语音通话外呼已收尾: {label}（呼出的通话不进振铃/进房，属正常）"
                    );
                    diagnostics::note_call_ended("外呼收尾");
                }
                None => {
                    println!("[INFO] QQ 语音通话桥报告已挂断: {label}");
                    diagnostics::note_call_ended("桥报告已挂断");
                }
            }
            if is_missed_call(previous, connected) {
                notify_missed_call(bot, config, state, previous).await;
            }
            record_core_call_ended(state, allowed, connected).await;
        }
        CallPhase::Idle if previous.is_live() => {
            if diagnostics::last_call_connected() {
                println!("[INFO] QQ 语音通话已结束: {label}");
                diagnostics::note_call_ended("桥回到空闲");
            } else {
                println!(
                    "[WARN] QQ 语音通话结束但从未进房（阶段停在 {}）: {label}——桥的音频会话没有建立",
                    previous.as_str()
                );
                diagnostics::note_call_ended("未进房就结束");
                notify_missed_call(bot, config, state, previous).await;
            }
            record_core_call_ended(state, allowed, connected).await;
        }
        CallPhase::Error => eprintln!("[ERROR] QQ 语音通话桥报告错误阶段: {label}"),
        CallPhase::Unknown => eprintln!(
            "[WARN] QQ 语音通话桥上报了未知阶段 {}: {label}",
            phase_description(phase)
        ),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    /// 静默时段的区间判据，尤其是**跨午夜**那种。
    ///
    /// 跨午夜写错的方向很危险：把 `23:00-08:00` 判成空区间，等于静默时段完全不生效，
    /// 而失败表现是"半夜打了一通电话"——最不该靠运气的地方。整天静默也是同理：
    /// 起止相同若按"空区间"处理，恰好和配置者的意图相反。
    #[test]
    fn quiet_hours_cover_the_configured_window() {
        use super::QuietHours;

        let night = QuietHours::parse("23:00-08:00").expect("合法区间");
        let minute = |hour: u32, minute: u32| hour * 60 + minute;
        // 跨午夜：晚上那一侧。
        assert!(night.contains_minute(minute(23, 0)), "起点算在内");
        assert!(night.contains_minute(minute(23, 59)));
        assert!(night.contains_minute(minute(0, 0)), "午夜之后仍在窗口内");
        assert!(night.contains_minute(minute(7, 59)));
        assert!(!night.contains_minute(minute(8, 0)), "终点不算在内");
        assert!(!night.contains_minute(minute(12, 0)));
        assert!(!night.contains_minute(minute(22, 59)));

        // 不跨午夜。
        let lunch = QuietHours::parse("12:00-13:30").expect("合法区间");
        assert!(lunch.contains_minute(minute(12, 0)));
        assert!(lunch.contains_minute(minute(13, 29)));
        assert!(!lunch.contains_minute(minute(13, 30)));
        assert!(!lunch.contains_minute(minute(11, 59)));
        assert!(
            !lunch.contains_minute(minute(23, 0)),
            "非跨午夜区间不该吃下夜里"
        );

        // 起止相同 = 整天静默（而不是"从不静默"）。
        let always = QuietHours::parse("00:00-00:00").expect("合法区间");
        assert!(always.contains_minute(minute(0, 0)));
        assert!(always.contains_minute(minute(15, 30)));

        // 非法写法返回 None，由配置加载那一步报错拦住。
        for bad in [
            "",
            "23:00",
            "23:00-",
            "-08:00",
            "24:00-08:00",
            "23:60-08:00",
            "aa-bb",
        ] {
            assert!(
                QuietHours::parse(bad).is_none(),
                "{bad:?} 不该被解析成合法区间"
            );
        }
    }

    /// 频次账本：默认放开时什么都不拦，开了之后按上限与间隔拦。
    ///
    /// 只对**真的拨出去了**的记账（`record_dial` 的调用点在 `Dialed` 那一支），
    /// 所以没拨出去不会白占额度。
    #[test]
    fn the_dial_ledger_counts_only_real_dials() {
        use super::{MAX_DIAL_LEDGER_ENTRIES, record_dial, with_ledger};
        use chrono::{Duration as ChronoDuration, Local};

        let peer = 9_000_001;
        let now = Local::now();
        // 先清干净，免得上一次运行的残留影响判据。
        with_ledger(|ledger| ledger.entries.remove(&peer));
        record_dial(peer, now);
        record_dial(peer, now + ChronoDuration::seconds(1));
        let (count, day) = with_ledger(|ledger| {
            let record = ledger.entries.get(&peer).expect("记过账");
            (record.count, record.day)
        });
        assert_eq!(count, 2);
        assert_eq!(day, now.date_naive());

        // 跨天要重置计数。
        record_dial(peer, now + ChronoDuration::days(1));
        let count = with_ledger(|ledger| ledger.entries[&peer].count);
        assert_eq!(count, 1, "跨天之后计数该从头开始");

        // 账本有界。
        for extra in 0..(MAX_DIAL_LEDGER_ENTRIES as i64 + 20) {
            record_dial(10_000_000 + extra, now);
        }
        let length = with_ledger(|ledger| ledger.entries.len());
        assert!(
            length <= MAX_DIAL_LEDGER_ENTRIES,
            "账本必须有界，实际 {length}"
        );
    }

    /// 通话结局的判据：桥分不出更细的，所以只有三种。
    ///
    /// 判错的具体代价：把"没接通"记成"通了话"，认知层就会以为她真的跟人说过话；
    /// 把"名单外婉拒"记成"没人接"，她会以为自己打过而对方不接——两件事完全不同。
    #[test]
    fn call_outcomes_are_classified_honestly() {
        use super::call_outcome;
        use yunxi_core::CallOutcome;

        // 进了房就是通了话，不管是谁打来的。
        assert_eq!(
            call_outcome(false, Some(true), true),
            CallOutcome::Completed
        );
        assert_eq!(call_outcome(true, None, true), CallOutcome::Completed);

        // 没进房：别人打来的算漏接，我们拨出去的算没接。
        assert_eq!(
            call_outcome(false, Some(true), false),
            CallOutcome::Unanswered
        );
        assert_eq!(call_outcome(true, None, false), CallOutcome::Unanswered);

        // 名单外婉拒：只可能是别人打来的（外呼在拨号前就查过名单）。
        assert_eq!(
            call_outcome(false, Some(false), false),
            CallOutcome::Refused
        );
        // 名单外的来电**也会进房**：桥是自动接听的，名单外只是播报一句婉拒然后静音。
        // 所以"进过房"不能单独用来判"通了话"——授权判据必须排在它前面，否则每一通
        // 被婉拒的来电都会被记成"她跟这个人说过话"。
        assert_eq!(call_outcome(false, Some(false), true), CallOutcome::Refused);
    }

    /// 待用开场白：一句话只给一通电话用一次，而且只给它对应的那个人。
    #[test]
    fn a_pending_opening_is_used_once_and_only_for_its_peer() {
        use super::{remember_opening, take_opening};
        remember_opening(1001, "喂，是我，刚想起件事");
        assert_eq!(take_opening(1001).as_deref(), Some("喂，是我，刚想起件事"));
        assert_eq!(take_opening(1001), None, "取过就该没了");
        assert_eq!(take_opening(1002), None, "不能串到别人身上");
    }

    /// 空开场白不占位——否则一通没有话可说的外呼会挤掉真正有开场白的那通。
    #[test]
    fn a_blank_opening_is_not_stored() {
        use super::{remember_opening, take_opening};
        remember_opening(2001, "   ");
        assert_eq!(take_opening(2001), None);
        remember_opening(2001, "");
        assert_eq!(take_opening(2001), None);
    }

    /// 过期与超量都由写入路径收口，不会无限攒着。
    ///
    /// 过期判据用的是单调时钟，测试直接把它写旧，而不是去 sleep。
    #[test]
    fn pending_openings_expire_and_stay_bounded() {
        use super::{
            MAX_PENDING_OPENINGS, PENDING_OPENING_TTL, PENDING_OPENINGS, remember_opening,
            take_opening,
        };
        use std::time::Duration;

        // 塞一条"早就过期"的，下一次读写就该把它清掉。
        {
            let mut guard = PENDING_OPENINGS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.insert(
                3001,
                (
                    "过期的开场白".to_string(),
                    std::time::Instant::now() - PENDING_OPENING_TTL - Duration::from_secs(1),
                ),
            );
        }
        assert_eq!(take_opening(3001), None, "过期条目不该还能用");

        // 超出上限时丢最早的，长度保持有界。
        for peer in 0..(MAX_PENDING_OPENINGS as i64 + 10) {
            remember_opening(4000 + peer, "在吗");
        }
        let length = PENDING_OPENINGS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        assert!(
            length <= MAX_PENDING_OPENINGS,
            "待用开场白必须有界，实际 {length}"
        );
        // 最早的已经被挤掉，最后写进去的还在。
        assert_eq!(take_opening(4000), None, "最早的应该被挤掉");
        assert!(
            take_opening(4000 + MAX_PENDING_OPENINGS as i64 + 9).is_some(),
            "最后写入的应该还在"
        );
    }

    /// `reached_peer` 只对"真的拨出去了"为真——拨号失败时靠它撤销开场白。
    #[test]
    fn only_a_dialed_outcome_counts_as_reaching_the_peer() {
        use super::DialOutcome;
        assert!(DialOutcome::Dialed.reached_peer());
        for outcome in [
            DialOutcome::NoReceipt,
            DialOutcome::Disabled,
            DialOutcome::OutgoingDisabled,
            DialOutcome::NotAuthorized,
            DialOutcome::AlreadyInCall,
            DialOutcome::BridgeUnavailable("bridge down".to_string()),
        ] {
            assert!(
                !outcome.reached_peer(),
                "{outcome:?} 没拨出去，不能被当成联系上了"
            );
        }
    }

    #[test]
    fn missed_call_needs_a_live_previous_phase() {
        use super::is_missed_call;
        // 真的漏接：亲眼见它振铃，却从未进房。
        assert!(is_missed_call(CallPhase::Ringing, false));
        assert!(is_missed_call(CallPhase::Accepting, false));
        // 接通过就不算漏接。
        assert!(!is_missed_call(CallPhase::Ringing, true));
        // 启动时桥里残留的结束状态（上一阶段是 Idle）不算——那会误报。
        assert!(!is_missed_call(CallPhase::Idle, false));
    }

    use super::bridge::{CallPhase, CallState};

    #[test]
    fn connected_phase_drives_a_session() {
        let state: CallState = serde_json::from_str(
            r#"{"phase":"connected","inviteAt":"2026-09-10T12:00:00.000Z","callerUin":"10001","callerName":"朋友"}"#,
        )
        .expect("桥的接通响应应可解析");
        assert_eq!(state.phase(), CallPhase::Connected);
        assert_eq!(state.caller(), Some(10001));
        assert_eq!(state.caller_name.as_deref(), Some("朋友"));
    }

    #[test]
    fn ringing_does_not_start_a_session() {
        let state: CallState = serde_json::from_str(r#"{"phase":"ringing"}"#).expect("可解析");
        assert!(state.phase().is_live());
        assert_ne!(state.phase(), CallPhase::Connected);
    }
}
