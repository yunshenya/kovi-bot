use serde::{Deserialize, Serialize};

/// QQ 实时语音通话配置。
///
/// 通话媒体链路完全由服务器上的 NapCat AV 桥负责（来电信令、自动接听、
/// PulseAudio 虚拟声卡）；本插件只做三件事：轮询桥的通话状态、对
/// PulseAudio 设备收发 PCM、调用本机语音服务做 ASR/TTS。默认关闭，
/// 未配置时不影响任何现有链路。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct QqCallConfig {
    /// 是否启用 QQ 语音通话。
    enabled: bool,
    /// NapCat AV 桥控制接口地址。只允许回环地址。
    bridge_url: String,
    /// 桥接 Bearer Token 的环境变量名；优先级高于 `bridge_token_file`。
    bridge_token_env: String,
    /// 桥接 Bearer Token 文件路径（安装器生成的 0600 文件）。
    bridge_token_file: String,
    /// 通话状态轮询间隔毫秒数。
    poll_interval_ms: u64,
    /// 单次桥接请求超时秒数。
    request_timeout_secs: u64,
    /// 单次通话最长秒数；超过后主动收尾并拒绝继续播放。
    max_call_seconds: u64,
    /// 桥接隔离 PulseAudio 服务地址，例如
    /// `unix:/home/ubuntu/maibot-qq-voice-call/runtime/pulse/native`。
    pulse_server: String,
    /// 访问隔离 PulseAudio 所需的 cookie 文件。
    ///
    /// 桥的 PulseAudio 配置里虽然写了 `auth-anonymous=1`，但实测该选项在
    /// 常见版本上并不生效：客户端必须携带与服务端一致的 cookie。容器里的
    /// QQ/AV Host 以 root 运行本就免认证，而机器人以普通用户运行时必须显式
    /// 指定 cookie，否则会被服务端以 "invalid authentication data" 拒绝。
    /// 安装脚本会把服务端 cookie 同步到共享目录，这里填它的路径即可。
    pulse_cookie: String,
    /// 对端声音的采集设备（桥的 speaker sink 的 monitor）。
    capture_device: String,
    /// 芸汐声音的播放设备（桥的虚拟麦克风 sink）。
    playback_device: String,
    /// 采集采样率，必须与桥的 ASR 输入约定一致。
    capture_sample_rate: u32,
    /// 语音活动检测帧长毫秒数。
    frame_ms: u32,
    /// 判定说完所需的静音帧数。
    end_of_speech_frames: u32,
    /// 判定对方插话所需的语音帧数。
    barge_in_speech_frames: u32,
    /// 最短语音片段毫秒数，短于该长度的片段直接丢弃。
    min_utterance_ms: u64,
    /// 片段内最短有效语音毫秒数。
    min_speech_ms: u64,
    /// 单段语音最长毫秒数，超过后强制切段。
    max_utterance_ms: u64,
    /// 本机语音服务的识别接口。
    asr_url: String,
    /// 单次识别超时秒数。
    asr_timeout_secs: u64,
    /// 本机语音服务的合成接口。
    tts_url: String,
    /// 合成输出采样率。
    tts_sample_rate: u32,
    /// 合成首包等待超时秒数。
    tts_timeout_secs: u64,
    /// PulseAudio 播放缓冲毫秒数，过低会卡顿。
    tts_playback_latency_ms: u32,
    /// 电话回复的最大字数，超过会截断后再送给 TTS。
    max_reply_chars: usize,
    /// 通话内保留的历史轮数。
    history_turns: usize,
    /// 没有上下文时的固定接通问候语。
    greeting: String,
    /// 电话模式系统提示，留空时使用内置默认。
    system_prompt: String,
    /// 允许来电的 QQ 号白名单；为空时只允许主管理员来电。
    allowed_callers: Vec<i64>,
    /// 来电者不在白名单时播报的一句婉拒；留空表示直接静音不回应。
    ///
    /// 这句婉拒在来电者不在有效授权名单里时播报（默认行为：接通后婉拒）。
    refuse_message: String,
    /// 是否让桥在接听前按名单拦截（默认关闭＝保持"接通后婉拒"的原有行为）。
    ///
    /// 打开后：名单外的来电**根本不会被接通**，让铃声自然结束。这是为了绕开
    /// "客户端无法挂断、接通后通话一直留在 connected"的问题，但会改变来电者
    /// 听到的结果（从一句婉拒变成无人接听），所以默认关闭，由部署者决定。
    caller_allowlist_enabled: bool,
    /// 机器人写给桥的"有效通话授权"名单文件（宿主路径）。
    ///
    /// 内容是 `{"callers": [...], "updatedAt": "..."}`，由机器人把授权名单、
    /// 副管理员与主管理员取并集后写入；桥在接听前读它，名单外不接听。留空关闭
    /// 这个能力（回到"接通后婉拒"）。
    caller_allowlist_file: String,
    /// 通话中对方说了这些词就当作"要求挂断"：说一句道别后真的挂断电话。
    hangup_keywords: Vec<String>,
    /// 对方要求挂断（或到达通话时长上限）时说的最后一句道别。
    farewell: String,
    /// 会话结束时是否让桥真的挂断电话（AVSDK cmd 8 = `Quit`）。
    ///
    /// 上游桥的控制接口白名单原本只有 `login(1)/accept(5)/kernel-forward(55)`，
    /// 所以机器人只能"停止参与"、等对方挂断。AVSDK 插件本身一直有
    /// `Quit(uint roomId, int reason)`；桥现在把它透出成
    /// `POST /v1/calls/hangup`。打开后，机器人结束会话时会主动挂断。
    hangup_enabled: bool,
    /// 用哪个 AVSDK 控制方法挂断（`close`/`quit`/`reject`/`clearRoom`）。
    ///
    /// 实测（2026-09-12，真机通话中）：只发 `quit` 本端会离开房间但服务器不销毁，
    /// 紧接着的 `close` 才真的结束这通电话（桥立刻 `ended`、`endReason=4`、
    /// AVSDK 事件计数停止增长）。因此默认 `close`。
    hangup_method: String,
    /// 传给 AVSDK 控制方法的原因码；默认 1。
    hangup_reason: i64,
    /// 挂断后是否把通话记录写回来电者的私聊记忆。
    archive_to_memory: bool,
}

impl QqCallConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn bridge_url(&self) -> &str {
        &self.bridge_url
    }

    pub fn bridge_token_env(&self) -> &str {
        &self.bridge_token_env
    }

    pub fn bridge_token_file(&self) -> &str {
        &self.bridge_token_file
    }

    pub fn poll_interval_ms(&self) -> u64 {
        self.poll_interval_ms
    }

    pub fn request_timeout_secs(&self) -> u64 {
        self.request_timeout_secs
    }

    pub fn max_call_seconds(&self) -> u64 {
        self.max_call_seconds
    }

    pub fn pulse_server(&self) -> &str {
        &self.pulse_server
    }

    pub fn pulse_cookie(&self) -> &str {
        &self.pulse_cookie
    }

    pub fn capture_device(&self) -> &str {
        &self.capture_device
    }

    pub fn playback_device(&self) -> &str {
        &self.playback_device
    }

    pub fn capture_sample_rate(&self) -> u32 {
        self.capture_sample_rate
    }

    pub fn frame_ms(&self) -> u32 {
        self.frame_ms
    }

    pub fn end_of_speech_frames(&self) -> u32 {
        self.end_of_speech_frames
    }

    pub fn barge_in_speech_frames(&self) -> u32 {
        self.barge_in_speech_frames
    }

    pub fn min_utterance_ms(&self) -> u64 {
        self.min_utterance_ms
    }

    pub fn min_speech_ms(&self) -> u64 {
        self.min_speech_ms
    }

    pub fn max_utterance_ms(&self) -> u64 {
        self.max_utterance_ms
    }

    pub fn asr_url(&self) -> &str {
        &self.asr_url
    }

    pub fn asr_timeout_secs(&self) -> u64 {
        self.asr_timeout_secs
    }

    pub fn tts_url(&self) -> &str {
        &self.tts_url
    }

    pub fn tts_sample_rate(&self) -> u32 {
        self.tts_sample_rate
    }

    pub fn tts_timeout_secs(&self) -> u64 {
        self.tts_timeout_secs
    }

    pub fn tts_playback_latency_ms(&self) -> u32 {
        self.tts_playback_latency_ms
    }

    pub fn max_reply_chars(&self) -> usize {
        self.max_reply_chars
    }

    pub fn history_turns(&self) -> usize {
        self.history_turns
    }

    pub fn greeting(&self) -> &str {
        &self.greeting
    }

    pub fn system_prompt(&self) -> &str {
        if self.system_prompt.is_empty() {
            DEFAULT_PHONE_PROMPT
        } else {
            &self.system_prompt
        }
    }

    pub fn allowed_callers(&self) -> &[i64] {
        &self.allowed_callers
    }

    pub fn refuse_message(&self) -> &str {
        &self.refuse_message
    }

    pub fn caller_allowlist_file(&self) -> &str {
        self.caller_allowlist_file.trim()
    }

    pub fn caller_allowlist_enabled(&self) -> bool {
        self.caller_allowlist_enabled
    }

    pub fn hangup_keywords(&self) -> &[String] {
        &self.hangup_keywords
    }

    pub fn farewell(&self) -> &str {
        self.farewell.trim()
    }

    /// 通话中对方这句话是不是"要求挂断"。
    pub fn is_hangup_request(&self, utterance: &str) -> bool {
        let text = utterance.trim();
        if text.is_empty() {
            return false;
        }
        self.hangup_keywords
            .iter()
            .map(|keyword| keyword.trim())
            .filter(|keyword| !keyword.is_empty())
            .any(|keyword| text.contains(keyword))
    }

    pub fn archive_to_memory(&self) -> bool {
        self.archive_to_memory
    }

    /// 会话结束时是否让桥真的挂断电话。
    pub fn hangup_enabled(&self) -> bool {
        self.hangup_enabled
    }

    /// 用哪个 AVSDK 控制方法挂断。
    pub fn hangup_method(&self) -> &str {
        self.hangup_method.trim()
    }

    /// 传给 AVSDK 控制方法的原因码。
    pub fn hangup_reason(&self) -> i64 {
        self.hangup_reason
    }

    /// 该 QQ 号是否允许来电。白名单为空时只允许主管理员。
    pub fn caller_allowed(&self, caller: i64, main_admin: Option<i64>) -> bool {
        if self.allowed_callers.contains(&caller) {
            return true;
        }
        main_admin == Some(caller)
    }

    /// 单帧字节数（单声道 S16LE）。
    pub fn frame_bytes(&self) -> usize {
        (self.capture_sample_rate as usize * self.frame_ms as usize / 1000) * 2
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !is_loopback_http_url(&self.bridge_url) {
            return Err(anyhow::anyhow!(
                "qq_call.bridge_url 必须是回环地址的 http 地址（127.0.0.1、localhost 或 [::1]）"
            ));
        }
        if !is_loopback_http_url(&self.asr_url) || !is_loopback_http_url(&self.tts_url) {
            return Err(anyhow::anyhow!(
                "qq_call.asr_url 与 qq_call.tts_url 必须是回环地址的 http 地址"
            ));
        }
        if self.bridge_token_env.trim().is_empty() && self.bridge_token_file.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "qq_call 必须配置 bridge_token_env 或 bridge_token_file"
            ));
        }
        if self.pulse_server.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "qq_call.pulse_server 必须指向桥隔离的 PulseAudio socket"
            ));
        }
        if self.capture_device.trim().is_empty() || self.playback_device.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "qq_call.capture_device 与 qq_call.playback_device 不能为空"
            ));
        }
        if !(8_000..=48_000).contains(&self.capture_sample_rate) {
            return Err(anyhow::anyhow!(
                "qq_call.capture_sample_rate 必须在 8000 到 48000 之间"
            ));
        }
        if !(8_000..=48_000).contains(&self.tts_sample_rate) {
            return Err(anyhow::anyhow!(
                "qq_call.tts_sample_rate 必须在 8000 到 48000 之间"
            ));
        }
        if !(10..=60).contains(&self.frame_ms)
            || !(self.capture_sample_rate * self.frame_ms).is_multiple_of(1000)
        {
            return Err(anyhow::anyhow!(
                "qq_call.frame_ms 必须在 10 到 60 之间，且采样率与帧长的乘积必须是 1000 的整数倍"
            ));
        }
        if !(50..=5_000).contains(&self.poll_interval_ms) {
            return Err(anyhow::anyhow!(
                "qq_call.poll_interval_ms 必须在 50 到 5000 之间"
            ));
        }
        if self.request_timeout_secs == 0 || self.request_timeout_secs > 30 {
            return Err(anyhow::anyhow!(
                "qq_call.request_timeout_secs 必须在 1 到 30 秒之间"
            ));
        }
        if self.max_call_seconds < 10 || self.max_call_seconds > 3_600 {
            return Err(anyhow::anyhow!(
                "qq_call.max_call_seconds 必须在 10 到 3600 秒之间"
            ));
        }
        if !(3..=200).contains(&self.end_of_speech_frames) {
            return Err(anyhow::anyhow!(
                "qq_call.end_of_speech_frames 必须在 3 到 200 之间"
            ));
        }
        if !(3..=200).contains(&self.barge_in_speech_frames) {
            return Err(anyhow::anyhow!(
                "qq_call.barge_in_speech_frames 必须在 3 到 200 之间"
            ));
        }
        if self.min_utterance_ms == 0 || self.min_utterance_ms > self.max_utterance_ms {
            return Err(anyhow::anyhow!(
                "qq_call.min_utterance_ms 必须大于 0 且不超过 max_utterance_ms"
            ));
        }
        if self.min_speech_ms > self.min_utterance_ms {
            return Err(anyhow::anyhow!(
                "qq_call.min_speech_ms 不能超过 min_utterance_ms"
            ));
        }
        if self.max_utterance_ms > 60_000 {
            return Err(anyhow::anyhow!("qq_call.max_utterance_ms 不能超过 60000"));
        }
        if self.asr_timeout_secs == 0 || self.asr_timeout_secs > 120 {
            return Err(anyhow::anyhow!(
                "qq_call.asr_timeout_secs 必须在 1 到 120 秒之间"
            ));
        }
        if self.tts_timeout_secs == 0 || self.tts_timeout_secs > 120 {
            return Err(anyhow::anyhow!(
                "qq_call.tts_timeout_secs 必须在 1 到 120 秒之间"
            ));
        }
        if !(20..=2_000).contains(&self.tts_playback_latency_ms) {
            return Err(anyhow::anyhow!(
                "qq_call.tts_playback_latency_ms 必须在 20 到 2000 之间"
            ));
        }
        if self.max_reply_chars == 0 || self.max_reply_chars > 500 {
            return Err(anyhow::anyhow!(
                "qq_call.max_reply_chars 必须在 1 到 500 之间"
            ));
        }
        if self.history_turns > 40 {
            return Err(anyhow::anyhow!("qq_call.history_turns 不能超过 40"));
        }
        if self.allowed_callers.iter().any(|caller| *caller <= 0) {
            return Err(anyhow::anyhow!(
                "qq_call.allowed_callers 必须是正整数 QQ 号"
            ));
        }
        if self.caller_allowlist_file.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "qq_call.caller_allowlist_file 不能为空（留空即关闭接听授权）"
            ));
        }
        if self
            .hangup_keywords
            .iter()
            .any(|keyword| keyword.trim().is_empty())
        {
            return Err(anyhow::anyhow!("qq_call.hangup_keywords 不能有空字符串"));
        }
        if self.hangup_method.trim().is_empty() || self.hangup_method.trim().len() > 32 {
            return Err(anyhow::anyhow!(
                "qq_call.hangup_method 必须是 1 到 32 个字符（close/quit/reject/clearRoom）"
            ));
        }
        if self.hangup_reason < 0 || self.hangup_reason > 1_000 {
            return Err(anyhow::anyhow!(
                "qq_call.hangup_reason 必须在 0 到 1000 之间（AVSDK 挂断原因码）"
            ));
        }
        if self.farewell.chars().count() > 200 {
            return Err(anyhow::anyhow!("qq_call.farewell 不能超过 200 字"));
        }
        Ok(())
    }
}

/// 电话模式内置提示。电话是低带宽通道：必须短、必须口语、不能有格式。
pub const DEFAULT_PHONE_PROMPT: &str = "你正在和对方打 QQ 语音电话。你只能说话，不能发文字、图片、表情或链接。\
回复必须是自然口语，通常一到两句话，最多 40 个字，不要使用任何标记符号、括号动作描写、emoji 或列表。\
对方说的是语音识别结果，可能有错别字、缺字或断句错误；听不清或明显不通顺时，用一句自然的追问确认，\
不要假装听懂。不要复述对方的话，不要解释自己是 AI。";

impl Default for QqCallConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bridge_url: "http://127.0.0.1:6110".to_string(),
            bridge_token_env: "KOVI_QQ_CALL_BRIDGE_TOKEN".to_string(),
            bridge_token_file: String::new(),
            poll_interval_ms: 250,
            request_timeout_secs: 5,
            max_call_seconds: 1_800,
            pulse_server: String::new(),
            pulse_cookie: String::new(),
            capture_device: "maibot_qq_speaker.monitor".to_string(),
            playback_device: "maibot_qq_mic".to_string(),
            capture_sample_rate: 16_000,
            frame_ms: 30,
            end_of_speech_frames: 18,
            barge_in_speech_frames: 18,
            min_utterance_ms: 700,
            min_speech_ms: 450,
            max_utterance_ms: 15_000,
            asr_url: "http://127.0.0.1:6120/v1/asr".to_string(),
            asr_timeout_secs: 20,
            tts_url: "http://127.0.0.1:6120/v1/tts".to_string(),
            tts_sample_rate: 24_000,
            tts_timeout_secs: 20,
            tts_playback_latency_ms: 80,
            max_reply_chars: 40,
            history_turns: 8,
            greeting: "喂，我在的，怎么啦？".to_string(),
            system_prompt: String::new(),
            allowed_callers: Vec::new(),
            refuse_message: "不好意思，我现在不方便接电话，晚点我打给你呀。".to_string(),
            caller_allowlist_file:
                "/home/ubuntu/napcat-qq-call/bridge/runtime/allowed-callers.json".to_string(),
            caller_allowlist_enabled: false,
            hangup_keywords: vec![
                "挂断".to_string(),
                "挂了吧".to_string(),
                "先挂".to_string(),
                "挂电话".to_string(),
                "挂了".to_string(),
                "不聊了".to_string(),
            ],
            farewell: "好，那我先挂啦，拜拜～".to_string(),
            hangup_enabled: true,
            hangup_method: "close".to_string(),
            hangup_reason: 1,
            archive_to_memory: true,
        }
    }
}

/// 只接受回环地址的 http 地址，避免把桥或语音服务暴露到公网。
fn is_loopback_http_url(value: &str) -> bool {
    let rest = match value.strip_prefix("http://") {
        Some(rest) => rest,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::QqCallConfig;

    #[test]
    fn defaults_are_disabled_and_valid() {
        let config = QqCallConfig::default();
        assert!(!config.enabled());
        assert!(config.validate().is_ok());
        assert_eq!(config.frame_bytes(), 960);
        assert!(
            config
                .caller_allowlist_file()
                .ends_with("allowed-callers.json")
        );
        assert!(config.is_hangup_request("那我先挂了吧"));
        assert!(!config.is_hangup_request("今天天气不错"));
    }

    #[test]
    fn hangup_requests_are_matched_inside_normal_speech() {
        let config = QqCallConfig::default();
        assert!(config.is_hangup_request("好，挂了吧，拜拜"));
        assert!(config.is_hangup_request("你先挂电话吧"));
        assert!(config.is_hangup_request("不聊了，我去吃饭"));
        assert!(!config.is_hangup_request(""));
        assert!(!config.is_hangup_request("   "));
        assert!(!config.is_hangup_request("我挂念你"));
    }

    #[test]
    fn allowlist_file_and_farewell_are_validated() {
        // 关闭状态下 validate 会直接放行，所以这里用"已启用"的最小配置。
        let enabled = || QqCallConfig {
            enabled: true,
            bridge_token_file: "/tmp/kovi-test-token".to_string(),
            pulse_server: "unix:/tmp/kovi-test-pulse".to_string(),
            ..QqCallConfig::default()
        };
        assert!(enabled().validate().is_ok());
        let no_file = QqCallConfig {
            caller_allowlist_file: "   ".to_string(),
            ..enabled()
        };
        assert!(no_file.validate().is_err());
        let empty_keyword = QqCallConfig {
            hangup_keywords: vec!["挂断".to_string(), "  ".to_string()],
            ..enabled()
        };
        assert!(empty_keyword.validate().is_err());
        let long_farewell = QqCallConfig {
            farewell: "啊".repeat(201),
            ..enabled()
        };
        assert!(long_farewell.validate().is_err());
        let bad_reason = QqCallConfig {
            hangup_reason: -1,
            ..enabled()
        };
        assert!(bad_reason.validate().is_err());
        let bad_method = QqCallConfig {
            hangup_method: "  ".to_string(),
            ..enabled()
        };
        assert!(bad_method.validate().is_err());
    }

    #[test]
    fn hangup_is_enabled_by_default() {
        let config = QqCallConfig::default();
        assert!(config.hangup_enabled());
        // 实测只有 close 能让服务器真的销毁房间。
        assert_eq!(config.hangup_method(), "close");
        assert_eq!(config.hangup_reason(), 1);
    }

    #[test]
    fn enabled_requires_bridge_token_and_pulse_server() {
        let config = QqCallConfig {
            enabled: true,
            ..QqCallConfig::default()
        };
        assert!(config.validate().is_err());

        let with_token = QqCallConfig {
            enabled: true,
            bridge_token_file: "/tmp/control.token".to_string(),
            ..QqCallConfig::default()
        };
        assert!(with_token.validate().is_err());

        let complete = QqCallConfig {
            enabled: true,
            bridge_token_file: "/tmp/control.token".to_string(),
            pulse_server: "unix:/tmp/pulse/native".to_string(),
            ..QqCallConfig::default()
        };
        assert!(complete.validate().is_ok());
    }

    #[test]
    fn non_loopback_endpoints_are_rejected() {
        for url in [
            "http://10.0.0.5:6110",
            "https://127.0.0.1:6110",
            "http://call.example.com:6110",
            "127.0.0.1:6110",
        ] {
            let config = QqCallConfig {
                enabled: true,
                bridge_url: url.to_string(),
                bridge_token_file: "/tmp/control.token".to_string(),
                pulse_server: "unix:/tmp/pulse/native".to_string(),
                ..QqCallConfig::default()
            };
            assert!(config.validate().is_err(), "{url} 应被拒绝");
        }
    }

    #[test]
    fn loopback_endpoints_are_accepted() {
        for url in [
            "http://127.0.0.1:6110",
            "http://localhost:6110/v1",
            "http://[::1]:6110",
        ] {
            let config = QqCallConfig {
                enabled: true,
                bridge_url: url.to_string(),
                bridge_token_file: "/tmp/control.token".to_string(),
                pulse_server: "unix:/tmp/pulse/native".to_string(),
                ..QqCallConfig::default()
            };
            assert!(config.validate().is_ok(), "{url} 应被接受");
        }
    }

    #[test]
    fn caller_allow_list_falls_back_to_main_admin() {
        let config = QqCallConfig::default();
        assert!(config.caller_allowed(10001, Some(10001)));
        assert!(!config.caller_allowed(10002, Some(10001)));

        let config = QqCallConfig {
            allowed_callers: vec![20002],
            ..QqCallConfig::default()
        };
        assert!(config.caller_allowed(20002, Some(10001)));
        assert!(config.caller_allowed(10001, Some(10001)));
        assert!(!config.caller_allowed(30003, Some(10001)));
    }

    #[test]
    fn frame_alignment_is_enforced() {
        let config = QqCallConfig {
            enabled: true,
            bridge_token_file: "/tmp/control.token".to_string(),
            pulse_server: "unix:/tmp/pulse/native".to_string(),
            capture_sample_rate: 22_050,
            frame_ms: 25,
            ..QqCallConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn empty_system_prompt_uses_builtin_phone_prompt() {
        let config = QqCallConfig::default();
        assert_eq!(config.system_prompt(), super::DEFAULT_PHONE_PROMPT);
    }
}
