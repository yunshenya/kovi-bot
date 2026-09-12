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

/// 私聊指令 `#打给我` 的实现：让芸汐主动拨给发起者。
///
/// 只允许授权名单里的人——规则是"谁让我打，我就打给谁"，不接受任意号码，免得变成
/// 骚扰工具。**打完必须确认电话真的响了**：AVSDK 的外呼命令目前会被直接丢弃，
/// 桥返回成功不代表拨出去了，所以这里几秒内轮询阶段，据实回复。
pub(crate) async fn request_outgoing_call(bot: &kovi::RuntimeBot, requester: i64) -> String {
    let config = config::get().qq_call().clone();
    if !config.enabled() {
        return "QQ 语音通话没启用，打不了电话。".to_string();
    }
    if !config.outgoing_enabled() {
        return "主动外呼被关掉了（qq_call.outgoing_enabled = false）。".to_string();
    }
    if !caller_is_allowed(&config, bot.get_main_admin().ok(), requester).await {
        return "你不在通话授权名单里，我不能打给你。".to_string();
    }
    let client = match BridgeClient::new(&config) {
        Ok(client) => client,
        Err(error) => return format!("打不了电话：{error}"),
    };
    if let Ok(state) = client.current_call().await
        && state.phase().is_live()
    {
        return "现在正通着话呢，等这通结束我再打给你。".to_string();
    }
    if let Err(error) = client.dial(requester).await {
        return format!("打不出去：{error}");
    }
    // 确认电话真的拨出去了：桥受理 ≠ AVSDK 真的拨号。判据是插件记下的外呼回执
    // （AVSDK 回报"对方是否在线"），因为呼出的通话不会让桥进入 ringing/connected。
    let deadline = std::time::Instant::now() + DIAL_CONFIRM_WINDOW;
    while std::time::Instant::now() < deadline {
        kovi::tokio::time::sleep(Duration::from_millis(400)).await;
        if let Ok(state) = client.current_call().await
            && (state.phase().is_live() || state.dial_reached_at.is_some())
        {
            return "好，我打给你啦，接一下～".to_string();
        }
    }
    "我让桥拨了，但没等到 AVSDK 的回执，多半是没拨出去——这个我还在查。".to_string()
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
                    report_phase_change(phase, observed_phase, &state, &config, &bot, traced).await;
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
                        if let Err(error) =
                            session::run(Arc::clone(&bot), &config, &client, &effective).await
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
) {
    let caller = state.caller();
    let caller_name = state.caller_name.as_deref();
    let label = caller_label(caller, caller_name);
    // 先取"这通有没有进过房"：下面 note_call_ended / begin_call 会把它清掉。
    let connected = diagnostics::last_call_connected();

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
            println!("[INFO] QQ 语音通话桥报告已挂断: {label}");
            diagnostics::note_call_ended("桥报告已挂断");
            if is_missed_call(previous, connected) {
                notify_missed_call(bot, config, state, previous).await;
            }
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
