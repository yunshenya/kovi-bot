//! NapCat AV 桥的最小控制客户端。
//!
//! 桥（独立的 NapCat 插件 + 第二个 QQ/AVSDK 进程）负责 QQ 通话的来电信令、
//! 自动接听和原生音频设备；本模块只读取它公开的通话状态。音频完全不经
//! 过 HTTP，而是通过桥隔离出来的 PulseAudio 虚拟设备收发（见 [`super::audio`]）。

use crate::config::QqCallConfig;
use serde::Deserialize;
use std::time::Duration;

/// 桥报告的通话阶段。只有 [`CallPhase::Connected`] 表示 AVSDK 已进房、
/// 音频设备可以收发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallPhase {
    /// 没有通话。
    #[default]
    Idle,
    /// 来电振铃中。
    Ringing,
    /// 正在接听。
    Accepting,
    /// 已接受，尚未进房。
    Accepted,
    /// 已进房，音频可收发。
    Connected,
    /// 正在结束（桥已经受理了挂断请求，房间还没销毁）。
    Ending,
    /// 已挂断。
    Ended,
    /// 桥内部错误。
    Error,
    /// 桥上报了未知阶段；按"不在通话中"处理。
    Unknown,
}

impl CallPhase {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "idle" => Self::Idle,
            "ringing" => Self::Ringing,
            "accepting" => Self::Accepting,
            "accepted" => Self::Accepted,
            "connected" => Self::Connected,
            "ending" => Self::Ending,
            "ended" => Self::Ended,
            "error" => Self::Error,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Ringing => "ringing",
            Self::Accepting => "accepting",
            Self::Accepted => "accepted",
            Self::Connected => "connected",
            Self::Ending => "ending",
            Self::Ended => "ended",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }

    /// 通话是否仍在进行（可能尚未进房）。`Ending` 表示桥已经受理挂断、
    /// 房间正在销毁，不再可用。
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Ringing | Self::Accepting | Self::Accepted | Self::Connected
        )
    }
}

/// 桥报告的一次通话状态。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallState {
    /// 原始阶段字符串。
    pub phase: String,
    /// 来电时间（桥的 ISO 8601 时间戳）。
    #[serde(rename = "inviteAt")]
    pub invite_at: Option<String>,
    /// 来电者 QQ 号；桥解析失败时为 `None`。
    #[serde(rename = "callerUin")]
    pub caller_uin: Option<String>,
    /// 来电者昵称或备注。
    #[serde(rename = "callerName")]
    pub caller_name: Option<String>,
    /// 结束原因码（QQ 内部值）。
    #[serde(rename = "endReason")]
    pub end_reason: Option<i64>,
}

impl CallState {
    pub fn phase(&self) -> CallPhase {
        CallPhase::parse(&self.phase)
    }

    /// 来电者 QQ 号；缺失或非法时返回 `None`。
    pub fn caller(&self) -> Option<i64> {
        self.caller_uin
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "0")
            .and_then(|value| value.parse::<i64>().ok())
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    data: CallState,
}

/// 桥控制接口客户端。
pub struct BridgeClient {
    http: reqwest::Client,
    endpoint: String,
    hangup_endpoint: String,
    token: String,
    timeout: Duration,
}

impl BridgeClient {
    pub fn new(config: &QqCallConfig) -> anyhow::Result<Self> {
        let token = load_bridge_token(config)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs()))
            .build()
            .map_err(|error| anyhow::anyhow!("无法创建通话桥 HTTP 客户端: {error}"))?;
        let base = config.bridge_url().trim_end_matches('/').to_owned();
        let endpoint = format!("{base}/v1/calls/current");
        let hangup_endpoint = format!("{base}/v1/calls/hangup");
        Ok(Self {
            http,
            endpoint,
            hangup_endpoint,
            token,
            timeout: Duration::from_secs(config.request_timeout_secs()),
        })
    }

    /// 读取当前通话状态。桥离线或鉴权失败时返回错误。
    pub async fn current_call(&self) -> anyhow::Result<CallState> {
        let response = self
            .http
            .get(&self.endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("通话桥请求失败: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            // 桥的鉴权端点在失败时也返回 JSON，这里只保留状态码，避免把
            // 任何可能的敏感字段带进日志。
            return Err(anyhow::anyhow!("通话桥返回 HTTP {}", status.as_u16()));
        }
        let envelope: Envelope = response
            .json()
            .await
            .map_err(|error| anyhow::anyhow!("通话桥返回了非法 JSON: {error}"))?;
        Ok(envelope.data)
    }

    /// 请桥主动挂断当前通话。
    ///
    /// 方法、房间号和原因都是实测出来的固定值，不开放成配置：AVSDK 的 `Close`
    /// （cmd 10）+ `roomId=0` + 来电者 uid（由桥填）+ `reason=1`。真机逐通试过：
    /// `Quit`(8)/`ClearRoom`(11) 挂不断，uid 留空或换成机器人自己则完全无效，
    /// 详见 `docs/qq-call.md` 的「挂断参数实验」。桥侧把它记为 `state.call.hangup*`；
    /// 请求成功只代表 AVSDK 收下了，通话是否真的结束以随后轮询到的阶段
    /// （`ended` + `endReason`）为准。
    pub async fn hangup(&self) -> anyhow::Result<()> {
        let response = self
            .http
            .post(&self.hangup_endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .json(&serde_json::json!({ "method": "close", "roomId": 0, "reason": 1 }))
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("通话桥挂断请求失败: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("通话桥挂断返回 HTTP {}", status.as_u16()));
        }
        Ok(())
    }
}

/// 读取桥接 Token：环境变量优先，其次 Token 文件。Token 永不写入日志。
fn load_bridge_token(config: &QqCallConfig) -> anyhow::Result<String> {
    let env_name = config.bridge_token_env().trim();
    if !env_name.is_empty()
        && let Ok(value) = std::env::var(env_name)
    {
        let value = value.trim().to_owned();
        if !value.is_empty() {
            return Ok(value);
        }
    }
    let path = config.bridge_token_file().trim();
    if path.is_empty() {
        return Err(anyhow::anyhow!(
            "通话桥 Token 为空：请设置 {} 或 qq_call.bridge_token_file",
            config.bridge_token_env()
        ));
    }
    let token = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("无法读取通话桥 Token 文件 {path}: {error}"))?
        .trim()
        .to_owned();
    if token.len() < 32 {
        return Err(anyhow::anyhow!(
            "通话桥 Token 文件 {path} 的内容短于 32 字节"
        ));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::{CallPhase, CallState};

    #[test]
    fn phases_parse_leniently() {
        assert_eq!(CallPhase::parse("connected"), CallPhase::Connected);
        assert_eq!(CallPhase::parse(" Connected "), CallPhase::Connected);
        assert_eq!(CallPhase::parse("RINGING"), CallPhase::Ringing);
        assert_eq!(CallPhase::parse("something-new"), CallPhase::Unknown);
        assert_eq!(CallPhase::parse(""), CallPhase::Unknown);
    }

    #[test]
    fn live_phases_exclude_terminal_states() {
        assert!(CallPhase::Connected.is_live());
        assert!(CallPhase::Ringing.is_live());
        assert!(CallPhase::Accepting.is_live());
        assert!(CallPhase::Accepted.is_live());
        assert!(!CallPhase::Idle.is_live());
        assert!(!CallPhase::Ended.is_live());
        assert!(!CallPhase::Error.is_live());
        assert!(!CallPhase::Unknown.is_live());
    }

    #[test]
    fn caller_ignores_missing_and_zero() {
        let mut state = CallState::default();
        assert_eq!(state.caller(), None);

        state.caller_uin = Some("0".to_string());
        assert_eq!(state.caller(), None);

        state.caller_uin = Some(" 10001 ".to_string());
        assert_eq!(state.caller(), Some(10001));

        state.caller_uin = Some("not-a-number".to_string());
        assert_eq!(state.caller(), None);
    }

    #[test]
    fn call_state_tolerates_missing_fields() {
        let state: CallState =
            serde_json::from_str("{\"phase\":\"idle\"}").expect("桥的最小响应应可解析");
        assert_eq!(state.phase(), CallPhase::Idle);
        assert_eq!(state.caller(), None);
    }
}
