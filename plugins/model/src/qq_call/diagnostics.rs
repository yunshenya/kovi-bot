//! 通话链路的可见性：阶段变化日志、最近一次通话的摘要、`#通话状态` 自检。
//!
//! QQ 通话能不能接通由**桥**决定，机器人只能在接通后说话（见模块文档）。这
//! 意味着"打进来没人接"这类问题在机器人侧不会留下任何痕迹：桥没上报就什么都
//! 没有，桥停在 `ringing` 也只是安静地循环。这个模块把三次关键事实记下来，
//! 让用户和管理员不登服务器也能回答"为什么不接电话"：
//!
//! 1. 桥是否上报过这次来电（`来电振铃` 日志与最近通话摘要）；
//! 2. 这次通话有没有真正进房（`connected`）；
//! 3. 来电者是否在通话授权名单里（名单外只会听到一句婉拒）。

use super::bridge::{BridgeClient, CallPhase, CallState};
use crate::config::QqCallConfig;
use crate::speech::SpeechClient;
use chrono::{DateTime, Local};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// 最近一次通话的可见摘要。只保留诊断需要的字段，不含任何通话内容。
#[derive(Debug, Clone)]
pub(super) struct CallTrace {
    pub(super) caller: Option<i64>,
    pub(super) caller_name: Option<String>,
    pub(super) allowed: bool,
    pub(super) first_seen: DateTime<Local>,
    pub(super) connected_at: Option<DateTime<Local>>,
    pub(super) end_reason: Option<String>,
    pub(super) duration_secs: Option<u64>,
}

static LAST_CALL: LazyLock<Mutex<Option<CallTrace>>> = LazyLock::new(|| Mutex::new(None));

fn with_last_call<T>(update: impl FnOnce(&mut Option<CallTrace>) -> T) -> T {
    let mut guard = LAST_CALL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(&mut guard)
}

/// 桥上报的通话阶段的中文说明，用于日志与自检报告。
pub(super) fn phase_description(phase: CallPhase) -> &'static str {
    match phase {
        CallPhase::Idle => "无通话",
        CallPhase::Ringing => "来电振铃（桥尚未接听）",
        CallPhase::Accepting => "正在接听",
        CallPhase::Accepted => "已接听、等待进房",
        CallPhase::Connected => "已进房（音频可收发）",
        CallPhase::Ending => "正在挂断（桥已受理挂断请求）",
        CallPhase::Ended => "已挂断",
        CallPhase::Error => "桥内部错误",
        CallPhase::Unknown => "未知阶段",
    }
}

/// 来电者的显示名：优先"昵称(QQ)"，解析不出来时如实说明。
pub(super) fn caller_label(caller: Option<i64>, caller_name: Option<&str>) -> String {
    let name = caller_name.map(str::trim).filter(|value| !value.is_empty());
    match (name, caller) {
        (Some(name), Some(caller)) => format!("{name}({caller})"),
        (_, Some(caller)) => caller.to_string(),
        _ => "未能解析来电者 QQ 号".to_string(),
    }
}

/// 记录"桥上报了一次进行中的通话"。每次新来电只调用一次。
pub(super) fn begin_call(caller: Option<i64>, caller_name: Option<&str>, allowed: bool) {
    let trace = CallTrace {
        caller,
        caller_name: caller_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        allowed,
        first_seen: Local::now(),
        connected_at: None,
        end_reason: None,
        duration_secs: None,
    };
    with_last_call(|slot| *slot = Some(trace));
}

/// 最近一次通话里那位来电者的授权结果；没有记录时返回 `None`。
pub(super) fn last_call_allowed() -> Option<bool> {
    with_last_call(|slot| slot.as_ref().map(|trace| trace.allowed))
}

/// 最近一次通话是否真正进过房：用来区分"桥接了信令但会话没建立"与正常通话。
pub(super) fn last_call_connected() -> bool {
    with_last_call(|slot| {
        slot.as_ref()
            .is_some_and(|trace| trace.connected_at.is_some())
    })
}

/// 标记这次通话已经进房（桥把音频设备接好了）。
pub(super) fn mark_connected() {
    with_last_call(|slot| {
        if let Some(trace) = slot.as_mut() {
            trace.connected_at = Some(Local::now());
        }
    });
}

/// 记录通话结束。由通话会话在收尾时调用：它知道准确的时长与结束原因。
pub(super) fn finish_call(reason: &str, duration: Option<Duration>) {
    with_last_call(|slot| {
        if let Some(trace) = slot.as_mut() {
            trace.end_reason = Some(reason.to_owned());
            if let Some(duration) = duration {
                trace.duration_secs = Some(duration.as_secs());
            }
        }
    });
}

/// 桥报告通话结束时补一条原因。通话会话已经写过更精确的原因时不覆盖。
pub(super) fn note_call_ended(reason: &str) {
    with_last_call(|slot| {
        if let Some(trace) = slot.as_mut()
            && trace.end_reason.is_none()
        {
            trace.end_reason = Some(reason.to_owned());
        }
    });
}

fn render_last_call(trace: &CallTrace) -> String {
    let label = caller_label(trace.caller, trace.caller_name.as_deref());
    let waited = (Local::now() - trace.first_seen).num_seconds().max(0);
    let waited = if waited >= 3_600 {
        format!("{} 小时前", waited / 3_600)
    } else if waited >= 60 {
        format!("{} 分钟前", waited / 60)
    } else {
        format!("{waited} 秒前")
    };
    let mut line = format!(
        "{label}，通话授权: {}，来电 {waited}",
        if trace.allowed {
            "已授权"
        } else {
            "未授权"
        }
    );
    match (trace.connected_at.is_some(), trace.duration_secs) {
        (true, Some(seconds)) => line.push_str(&format!("，已进房，通话 {seconds} 秒")),
        (true, None) => line.push_str("，已进房"),
        (false, _) => line.push_str("，从未进房（桥没有建立音频会话）"),
    }
    if let Some(reason) = &trace.end_reason {
        line.push_str(&format!("，结束原因: {reason}"));
    }
    line
}

/// `#通话状态` 的管理员自检报告：桥、来电、授权、语音服务各一行。
pub(crate) async fn status_report(bot: &kovi::RuntimeBot, config: &QqCallConfig) -> String {
    if !config.enabled() {
        return "QQ 语音通话：已关闭（[qq_call] enabled = false）。\n\
                接通与接听由服务器上的 NapCat AV 桥负责，机器人只负责接通后说话。"
            .to_string();
    }

    let mut lines = vec![format!(
        "QQ 语音通话：已启用（桥 {}，轮询 {} 毫秒）",
        config.bridge_url(),
        config.poll_interval_ms()
    )];
    lines.push(format!(
        "当前：{}",
        match BridgeClient::new(config) {
            Ok(client) => match client.current_call().await {
                Ok(state) => describe_current(&state, bot, config).await,
                Err(error) => format!("读取桥状态失败 —— {error}"),
            },
            Err(error) => format!("无法创建桥客户端 —— {error}"),
        }
    ));
    lines.push(format!(
        "最近一次通话：{}",
        with_last_call(|slot| slot.as_ref().map(render_last_call))
            .unwrap_or_else(|| "桥还没有上报过任何来电".to_string())
    ));
    lines.push(format!("接听授权名单：{}", allowlist_summary(config)));
    lines.push(format!(
        "语音服务：{}",
        match SpeechClient::new(config) {
            Ok(speech) => match speech.health().await {
                Ok(()) => format!("{} 正常", config.tts_url()),
                Err(error) => format!("不可用 —— {error}"),
            },
            Err(error) => format!("客户端创建失败 —— {error}"),
        }
    ));
    lines.push(if config.hangup_enabled() {
        "提醒：收尾时机器人会请桥挂断这通电话（AVSDK Quit，cmd 8）；\
         关掉 qq_call.hangup_enabled 就只能停止参与、等对方挂断。"
            .to_string()
    } else {
        "提醒：qq_call.hangup_enabled = false，机器人结束会话后不会挂断电话，\
         这通电话要等对方挂断（或服务器超时）。"
            .to_string()
    });
    lines.join("\n")
}

/// 机器人写给桥的接听授权名单概况。
fn allowlist_summary(config: &QqCallConfig) -> String {
    if !config.caller_allowlist_enabled() {
        return "未启用（名单外来电仍会被接通并听到婉拒；想改成「名单外不接」\
                就打开 [qq_call] caller_allowlist_enabled）"
            .to_string();
    }
    let path = config.caller_allowlist_file();
    if path.is_empty() {
        return "已关闭（桥会接听任何来电）".to_string();
    }
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => {
                let count = value
                    .get("callers")
                    .and_then(|callers| callers.as_array())
                    .map(Vec::len)
                    .unwrap_or(0);
                let updated = value
                    .get("updatedAt")
                    .and_then(|updated| updated.as_str())
                    .unwrap_or("未知时间");
                format!("{count} 人（机器人同步于 {updated}）")
            }
            Err(error) => format!("读取失败 —— {error}"),
        },
        Err(error) => format!("未找到（{error}）—— 桥会按旧行为接听任何来电"),
    }
}

async fn describe_current(
    state: &CallState,
    bot: &kovi::RuntimeBot,
    config: &QqCallConfig,
) -> String {
    let phase = state.phase();
    let mut line = format!("桥状态 {}（{}）", phase.as_str(), phase_description(phase));
    match state.caller() {
        Some(caller) => {
            let allowed =
                super::session::caller_is_allowed(config, bot.get_main_admin().ok(), caller).await;
            line.push_str(&format!(
                "，来电者 {}，通话授权: {}",
                caller_label(state.caller(), state.caller_name.as_deref()),
                if allowed { "已授权" } else { "未授权" }
            ));
        }
        None if phase.is_live() => line.push_str("，桥没有给出来电者 QQ 号"),
        None => {}
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{CallTrace, caller_label, phase_description, render_last_call};
    use crate::qq_call::bridge::CallPhase;
    use chrono::Local;
    use std::time::Duration;

    #[test]
    fn caller_label_never_invents_an_identity() {
        assert_eq!(caller_label(Some(10001), Some(" 朋友 ")), "朋友(10001)");
        assert_eq!(caller_label(Some(10001), None), "10001");
        assert_eq!(caller_label(None, Some("朋友")), "未能解析来电者 QQ 号");
    }

    #[test]
    fn phases_render_in_chinese() {
        assert_eq!(
            phase_description(CallPhase::Ringing),
            "来电振铃（桥尚未接听）"
        );
        assert!(phase_description(CallPhase::Connected).contains("音频可收发"));
    }

    #[test]
    fn a_call_that_never_entered_the_room_is_reported_as_such() {
        let trace = CallTrace {
            caller: Some(10001),
            caller_name: Some("朋友".to_string()),
            allowed: true,
            first_seen: Local::now(),
            connected_at: None,
            end_reason: Some("桥报告已挂断".to_string()),
            duration_secs: None,
        };
        let rendered = render_last_call(&trace);
        assert!(rendered.contains("从未进房"), "{rendered}");
        assert!(rendered.contains("通话授权: 已授权"), "{rendered}");
    }

    #[test]
    fn a_completed_call_reports_its_duration() {
        let trace = CallTrace {
            caller: Some(10001),
            caller_name: None,
            allowed: false,
            first_seen: Local::now(),
            connected_at: Some(Local::now()),
            end_reason: Some("对方挂断".to_string()),
            duration_secs: Some(51),
        };
        let rendered = render_last_call(&trace);
        assert!(rendered.contains("通话 51 秒"), "{rendered}");
        assert!(rendered.contains("通话授权: 未授权"), "{rendered}");
    }

    /// 桥在通话结束后还会陆续上报 `ended` / `idle`：这些迟到的事实不能覆盖
    /// 通话会话写下的精确结束原因与时长。
    ///
    /// 这是唯一一处改动全局 `LAST_CALL` 的测试；别处只读渲染用的局部结构。
    #[test]
    fn bridge_notifications_do_not_overwrite_the_session_outcome() {
        super::begin_call(Some(10001), Some("朋友"), true);
        assert!(super::last_call_allowed().unwrap_or(false));
        assert!(!super::last_call_connected());
        super::mark_connected();
        assert!(super::last_call_connected());
        super::finish_call("对方挂断", Some(Duration::from_secs(51)));
        super::note_call_ended("桥回到空闲");

        let rendered = super::with_last_call(|slot| {
            slot.as_ref()
                .map(render_last_call)
                .expect("通话摘要应当存在")
        });
        assert!(rendered.contains("结束原因: 对方挂断"), "{rendered}");
        assert!(rendered.contains("通话 51 秒"), "{rendered}");
    }
}
