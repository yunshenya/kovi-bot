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
use bridge::{BridgeClient, CallPhase, CallState};
use diagnostics::{caller_label, phase_description};
use session::caller_is_allowed;
use std::sync::Arc;
use std::time::Duration;

/// 同一类桥错误的最短重复日志间隔，避免桥长时间离线时刷日志。
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(300);

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
                        if let Err(error) =
                            session::run(Arc::clone(&bot), &config, &client, &state).await
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
