//! 单次通话的编排：采集 → VAD 切段 → 本地识别 → 芸汐模型 → 本地合成 → 播放。
//!
//! 结构上分两条并发链路，因为它们的实时性要求不同：
//!
//! - **采集链**（[`run`] 主体）必须永不阻塞：每 30 毫秒读一帧、判一次语音，
//!   顺便检测对方插话和挂断。任何耗时工作都不能放在这里。
//! - **回复链**（[`respond`] 任务）做识别、模型和合成，天然是秒级的。
//!
//! 两条链路之间只有两个有界通道：语音片段队列和打断信号。回复链永远不阻塞
//! 采集链，所以识别变慢只会让回复滞后，不会让通话"听不见"。

use super::audio::{Capture, Playback};
use super::bridge::{BridgeClient, CallPhase, CallState};
use super::vad::Segmenter;
use crate::config::QqCallConfig;
use crate::memory::{MEMORY_MANAGER, MemoryEntry, MemoryType};
use crate::model::utils::{is_model_error_response, params_model_with_plain_style_context};
use crate::model::{BotMemory, Roles};
use crate::speech::SpeechClient;
use kovi::tokio::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 待处理语音队列上限。回复链明显落后时丢弃最新片段而不是无限堆积。
const JOB_QUEUE: usize = 8;
/// 电话回复的模型输出上限。
const PHONE_MAX_TOKENS: u32 = 256;
/// 通话内保留的转写轮数上限。
const MAX_TRANSCRIPT_TURNS: usize = 64;
/// 单次采集读取的超时：定长帧只有几十毫秒，等一秒还没有整帧就说明这一轮
/// 没有数据，先回去看桥的状态。
const CAPTURE_IDLE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// 连续多少次读不到整帧就认为采集链路已死（配合上面的超时即秒数）。
const CAPTURE_STALL_LIMIT: u32 = 30;

/// 挂断后等待回复链收尾的时间。
const RESPONDER_DRAIN: Duration = Duration::from_secs(15);
/// 取用的过往私聊记忆条数。
const CONTEXT_MEMORIES: usize = 12;
/// 每条过往记忆注入提示的最大字数。
const CONTEXT_MEMORY_CHARS: usize = 120;
/// 桥连续失败多少次后判定通话已不可继续。
const BRIDGE_FAILURE_LIMIT: u32 = 5;

/// 回复链的工作项。
enum Job {
    /// 对端语音片段，需要先识别。
    Utterance(Vec<u8>),
    /// 直接播报的文本（接通问候、婉拒）。
    Speak(String),
}

/// 电话里已经说过的一句话。
#[derive(Clone)]
struct Turn {
    from_peer: bool,
    text: String,
}

/// 一次播报的结果。
enum SpeakOutcome {
    /// 完整播完，可以写进电话上下文。
    Completed,
    /// 被对方插话打断，不写进上下文。
    Interrupted,
}

/// 来电者是否被允许与芸汐对话。授权来源有两处，任一命中即放行：
///   1. 数据库里的通话授权名单（含主管理员与副管理员，可用 #授权通话 维护）；
///   2. 静态配置 qq_call.allowed_callers（首次初始化会迁移进数据库，保留是为了
///      授权体系尚未初始化时仍能工作）。
pub(super) async fn caller_is_allowed(
    config: &QqCallConfig,
    main_admin: Option<i64>,
    caller: i64,
) -> bool {
    crate::group_access::is_authorized_caller(caller).await
        || config.caller_allowed(caller, main_admin)
}

/// 执行一次通话，直到挂断、桥不可用或超过时长上限。
pub(super) async fn run(
    bot: Arc<kovi::RuntimeBot>,
    config: &QqCallConfig,
    client: &BridgeClient,
    state: &CallState,
) -> anyhow::Result<()> {
    let caller = state.caller();
    let caller_name = state
        .caller_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let main_admin = bot.get_main_admin().ok();
    let allowed = match caller {
        Some(caller) => caller_is_allowed(config, main_admin, caller).await,
        None => false,
    };

    match (caller, caller_name) {
        (Some(caller), Some(name)) => println!("[INFO] QQ 语音通话已接通: {name}({caller})"),
        (Some(caller), None) => println!("[INFO] QQ 语音通话已接通: {caller}"),
        (None, _) => eprintln!("[WARN] QQ 来电未能解析出来电者 QQ 号，按不在白名单处理"),
    }
    if !allowed {
        println!("[INFO] 来电者不在通话授权名单，播报婉拒后结束本次通话会话");
    }

    let speech = Arc::new(SpeechClient::new(config)?);
    let transcript: Arc<Mutex<Vec<Turn>>> = Arc::new(Mutex::new(Vec::new()));
    let (job_tx, job_rx) = mpsc::channel::<Job>(JOB_QUEUE);
    // 打断通道的发送端必须活到回复链结束，否则 `recv()` 会立刻返回 `None`。
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<()>(JOB_QUEUE);

    // 对方要求挂断 / 通话到点收尾 / 名单外婉拒后，用它让采集链优雅收尾。
    // 会话收尾时如果电话还通着，再用桥的 `POST /v1/calls/hangup`
    // （AVSDK 控制方法，默认 cmd 10 = `Close`）真的挂断，不再只能等对方挂断。
    let hangup_requested = Arc::new(AtomicBool::new(false));
    let responder = kovi::tokio::spawn(respond(
        config.clone(),
        caller,
        Arc::clone(&speech),
        Arc::clone(&transcript),
        job_rx,
        interrupt_rx,
        Arc::clone(&hangup_requested),
    ));

    let opening = if allowed {
        config.greeting().trim().to_owned()
    } else {
        config.refuse_message().trim().to_owned()
    };
    if !opening.is_empty()
        && let Err(error) = job_tx.send(Job::Speak(opening)).await
    {
        eprintln!("[ERROR] QQ 通话开场播报入队失败: {error}");
    }
    if !allowed {
        // 婉拒已经排在回复链里，采集链立即收尾；回复链会把这句话播完再退出。
        hangup_requested.store(true, Ordering::Relaxed);
    }

    let started = Instant::now();
    let mut capture = match Capture::spawn(config) {
        Ok(capture) => capture,
        Err(error) => {
            drop(job_tx);
            let _ = responder.await;
            drop(interrupt_tx);
            return Err(error);
        }
    };
    let mut segmenter = Segmenter::new(
        config.frame_ms(),
        config.end_of_speech_frames(),
        config.barge_in_speech_frames(),
        config.max_utterance_ms(),
        config.min_utterance_ms(),
        config.min_speech_ms(),
    );
    let mut end_reason = "通话已结束";
    let mut bridge_failures: u32 = 0;
    let mut first_speech_seen = false;
    // 电话是否还通着：只有桥报告阶段离开 connected，或者桥自己不可用了，
    // 才不需要再挂断。
    let mut bridge_live = true;

    let poll_every = Duration::from_millis(config.poll_interval_ms().max(50));
    let call_budget = Duration::from_secs(config.max_call_seconds());
    let mut last_poll = Instant::now();
    // 连续多少次读不到整帧。采集卡住时靠它收尾，避免会话永远挂着。
    let mut stalled_polls: u32 = 0;

    loop {
        // 与音频无关的检查（挂断请求、时长上限、桥阶段）一律按时间走：
        // 采集一旦不吐数据，按"帧数"计算的检查就再也跑不到了。
        if last_poll.elapsed() >= poll_every {
            last_poll = Instant::now();
            if hangup_requested.load(Ordering::Relaxed) {
                end_reason = if allowed {
                    "对方要求挂断"
                } else {
                    "名单外婉拒"
                };
                break;
            }
            if started.elapsed() >= call_budget {
                end_reason = "达到通话时长上限";
                let farewell = config.farewell().trim().to_owned();
                if !farewell.is_empty() {
                    // 到点先说一句道别，再结束会话并挂断这通电话。
                    let _ = job_tx.try_send(Job::Speak(farewell));
                }
                break;
            }
            match client.current_call().await {
                Ok(current) => {
                    bridge_failures = 0;
                    if current.phase() != CallPhase::Connected {
                        end_reason = match current.phase() {
                            CallPhase::Idle => "桥报告通话已空闲",
                            CallPhase::Ended => "对方挂断",
                            _ => "通话阶段已结束",
                        };
                        bridge_live = false;
                        println!("[INFO] QQ 通话桥阶段变为 {}", current.phase().as_str());
                        break;
                    }
                }
                Err(error) => {
                    bridge_failures += 1;
                    // 单次抖动不足以结束通话；连续失败说明桥已经不在了。
                    if bridge_failures >= BRIDGE_FAILURE_LIMIT {
                        end_reason = "通话桥不可用";
                        bridge_live = false;
                        eprintln!("[ERROR] QQ 通话桥连续不可用: {error}");
                        break;
                    }
                }
            }
        }

        let frame =
            match kovi::tokio::time::timeout(CAPTURE_IDLE_TIMEOUT, capture.next_frame()).await {
                Ok(Ok(frame)) => {
                    stalled_polls = 0;
                    frame.to_vec()
                }
                Ok(Err(error)) => {
                    end_reason = "通话音频中断";
                    eprintln!("[ERROR] QQ 通话采集结束: {error}");
                    break;
                }
                Err(_) => {
                    // 只是这一小段时间没有整帧：不阻塞，回到循环顶部继续看桥。
                    stalled_polls += 1;
                    if stalled_polls >= CAPTURE_STALL_LIMIT {
                        end_reason = "通话音频长时间无数据";
                        eprintln!(
                            "[WARN] QQ 通话采集连续 {} 次读不到数据，结束本次会话",
                            stalled_polls
                        );
                        break;
                    }
                    continue;
                }
            };

        let outcome = segmenter.push(&frame);
        // 第一次收到对端语音说明整条采集链路是通的；只报一次，避免刷日志。
        if !first_speech_seen && segmenter.recording() {
            first_speech_seen = true;
            println!("[INFO] QQ 通话已收到对端语音，采集链路正常");
        }
        if outcome.speech_started {
            // 对方插话：让回复链立刻停掉正在播放的 TTS。
            let _ = interrupt_tx.try_send(());
        }
        if let Some(pcm) = outcome.utterance
            && allowed
            && let Err(error) = job_tx.try_send(Job::Utterance(pcm))
        {
            match error {
                mpsc::error::TrySendError::Full(_) => {
                    eprintln!("[WARN] QQ 通话语音队列已满，丢弃最新片段");
                }
                mpsc::error::TrySendError::Closed(_) => break,
            }
        }
    }

    capture.shutdown().await;
    drop(job_tx);
    if kovi::tokio::time::timeout(RESPONDER_DRAIN, responder)
        .await
        .is_err()
    {
        eprintln!("[WARN] QQ 通话回复链未在限定时间内收尾");
    }
    // 采集链结束后不再需要打断信号，回复链也已经停止。
    drop(interrupt_tx);
    segmenter.reset();

    // 会话已经收尾（道别/婉拒也已经播完）：如果电话还通着，就让桥真的挂断。
    if bridge_live && config.hangup_enabled() {
        match client
            .hangup(config.hangup_method(), config.hangup_reason())
            .await
        {
            Ok(()) => println!(
                "[INFO] 已请通话桥挂断这通电话（AVSDK {}）",
                config.hangup_method()
            ),
            Err(error) => eprintln!("[WARN] 请通话桥挂断失败: {error}"),
        }
    }

    println!(
        "[INFO] QQ 语音通话结束（{end_reason}，时长 {} 秒）",
        started.elapsed().as_secs()
    );
    // 让 `#通话状态` 能回看这一次的结果（尤其是"接通了但没进房"与时长）。
    super::diagnostics::finish_call(end_reason, Some(started.elapsed()));
    let turns = transcript
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if allowed {
        archive_call(config, caller, caller_name, &turns, end_reason).await;
    }
    Ok(())
}

/// 回复链：识别 → 组装上下文 → 芸汐模型 → 合成 → 播放。
async fn respond(
    config: QqCallConfig,
    caller: Option<i64>,
    speech: Arc<SpeechClient>,
    transcript: Arc<Mutex<Vec<Turn>>>,
    mut jobs: mpsc::Receiver<Job>,
    mut interrupts: mpsc::Receiver<()>,
    hangup_requested: Arc<AtomicBool>,
) {
    let context = match caller {
        Some(caller) => load_caller_context(caller).await,
        None => String::new(),
    };

    while let Some(job) = jobs.recv().await {
        match job {
            Job::Speak(text) => {
                let text = text.trim().to_owned();
                if text.is_empty() {
                    continue;
                }
                match speak(&speech, &config, &text, &mut interrupts).await {
                    Ok(SpeakOutcome::Completed) => push_turn(
                        &transcript,
                        Turn {
                            from_peer: false,
                            text,
                        },
                    ),
                    Ok(SpeakOutcome::Interrupted) => {
                        println!("[INFO] QQ 通话开场白被插话打断");
                    }
                    Err(error) => eprintln!("[ERROR] QQ 通话播报失败: {error}"),
                }
            }
            Job::Utterance(pcm) => {
                if hangup_requested.load(Ordering::Relaxed) {
                    // 已经道别过了，剩下的尾音不再处理。
                    continue;
                }
                let peer_text = match speech.transcribe(&pcm, config.capture_sample_rate()).await {
                    Ok(text) if !text.trim().is_empty() => text.trim().to_owned(),
                    Ok(_) => continue,
                    Err(error) => {
                        eprintln!("[ERROR] QQ 通话语音识别失败: {error}");
                        continue;
                    }
                };
                println!("[INFO] QQ 通话识别: {peer_text}");

                if config.is_hangup_request(&peer_text) {
                    // 对方说"挂了吧/先挂"：她回一句道别，然后结束本次通话会话。
                    // QQ 的 1v1 通话无法由客户端挂断，实际断线要等对方操作。
                    println!("[INFO] QQ 通话对方要求挂断，播报道别后收尾");
                    push_turn(
                        &transcript,
                        Turn {
                            from_peer: true,
                            text: peer_text,
                        },
                    );
                    let farewell = config.farewell().trim().to_owned();
                    if !farewell.is_empty() {
                        match speak(&speech, &config, &farewell, &mut interrupts).await {
                            Ok(SpeakOutcome::Completed) => push_turn(
                                &transcript,
                                Turn {
                                    from_peer: false,
                                    text: farewell,
                                },
                            ),
                            Ok(SpeakOutcome::Interrupted) => {}
                            Err(error) => eprintln!("[ERROR] QQ 通话道别播报失败: {error}"),
                        }
                    }
                    hangup_requested.store(true, Ordering::Relaxed);
                    continue;
                }

                push_turn(
                    &transcript,
                    Turn {
                        from_peer: true,
                        text: peer_text,
                    },
                );

                let Some(reply) = generate_reply(&config, &context, &transcript).await else {
                    continue;
                };
                println!("[INFO] QQ 通话回复: {reply}");
                match speak(&speech, &config, &reply, &mut interrupts).await {
                    Ok(SpeakOutcome::Completed) => push_turn(
                        &transcript,
                        Turn {
                            from_peer: false,
                            text: reply,
                        },
                    ),
                    Ok(SpeakOutcome::Interrupted) => {
                        println!("[INFO] QQ 通话回复被插话打断，不计入电话上下文");
                    }
                    Err(error) => eprintln!("[ERROR] QQ 通话播报失败: {error}"),
                }
            }
        }
    }
}

/// 合成并播放一句话。边合成边写入，首包到达即出声。
async fn speak(
    speech: &SpeechClient,
    config: &QqCallConfig,
    text: &str,
    interrupts: &mut mpsc::Receiver<()>,
) -> anyhow::Result<SpeakOutcome> {
    // 丢掉上一轮遗留的打断信号，避免新回复刚开口就被打断。
    while interrupts.try_recv().is_ok() {}

    let mut stream = speech.synthesize(text).await?;
    let mut playback = Playback::spawn(config, stream.sample_rate())?;
    loop {
        kovi::tokio::select! {
            biased;
            signal = interrupts.recv() => {
                playback.interrupt().await;
                return match signal {
                    Some(()) => Ok(SpeakOutcome::Interrupted),
                    None => Err(anyhow::anyhow!("通话打断通道已关闭")),
                };
            }
            chunk = stream.next_chunk() => {
                match chunk {
                    Ok(Some(pcm)) => {
                        if let Err(error) = playback.write(&pcm).await {
                            playback.interrupt().await;
                            return Err(error);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        playback.interrupt().await;
                        return Err(error);
                    }
                }
            }
        }
    }
    playback.finish().await?;
    Ok(SpeakOutcome::Completed)
}

/// 用芸汐的私聊人设和模型生成一句电话回复。
async fn generate_reply(
    config: &QqCallConfig,
    context: &str,
    transcript: &Arc<Mutex<Vec<Turn>>>,
) -> Option<String> {
    let turns = transcript
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut messages = build_messages(config, context, &turns);
    let response = params_model_with_plain_style_context(
        &mut messages,
        Some(PHONE_MAX_TOKENS),
        &[],
        None,
        None,
    )
    .await;
    if is_model_error_response(&response.content) {
        eprintln!("[ERROR] QQ 通话模型调用失败，本轮不回复");
        return None;
    }
    match sanitize_reply(&response.content, config.max_reply_chars()) {
        Some(reply) => Some(reply),
        None => {
            eprintln!("[WARN] QQ 通话模型返回了空回复或不可播报内容，本轮不回复");
            None
        }
    }
}

/// 电话请求的消息序列：人设 + 背景资料 + 通话内上下文。
fn build_messages(config: &QqCallConfig, context: &str, turns: &[Turn]) -> Vec<BotMemory> {
    let mut messages = vec![BotMemory {
        role: Roles::System,
        content: phone_system_prompt(config),
    }];
    if !context.trim().is_empty() {
        messages.push(BotMemory {
            role: Roles::Data,
            content: format!(
                "<参考上下文：你和这位朋友最近的私聊记录>\n{context}\n</参考上下文>\n\
                 以上只是背景资料，用来判断你们的关系和最近聊过什么；不要复述，\
                 也不要为了体现记忆而主动提起。"
            ),
        });
    }
    messages.push(BotMemory {
        role: Roles::Data,
        content: "以下是你和他此刻在电话里已经说过的话，按时间从早到晚排列。".to_string(),
    });
    let keep = config.history_turns().saturating_mul(2);
    let start = turns.len().saturating_sub(keep);
    for turn in &turns[start..] {
        messages.push(BotMemory {
            role: if turn.from_peer {
                Roles::User
            } else {
                Roles::Assistant
            },
            content: turn.text.clone(),
        });
    }
    messages
}

/// 私聊人设 + 电话模式约束。电话约束放在后面，明确覆盖打字的格式要求。
fn phone_system_prompt(config: &QqCallConfig) -> String {
    let persona = crate::config::get().prompt().private_prompt().to_owned();
    format!(
        "{persona}\n\n【当前场景：你们正在打 QQ 语音电话】\n{phone}\n\
         注意：上面所有关于发消息、气泡条数、表情包和排版的要求，在你说话时都不适用——\
         你正在打电话，不是在打字。",
        phone = config.system_prompt(),
    )
}

/// 把模型输出收拾成一句可以直接读出来的话。
fn sanitize_reply(raw: &str, max_chars: usize) -> Option<String> {
    let without_markers = strip_protocol_markers(raw);
    let line = without_markers
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = trim_stage_direction(line)
        .trim_matches(|character| matches!(character, '"' | '\'' | '“' | '”' | '「' | '」'))
        .trim();
    if line.is_empty() {
        return None;
    }
    Some(truncate_spoken(line, max_chars))
}

/// 去掉模型可能漏出来的 `[[...]]` 协议标记。
fn strip_protocol_markers(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        output.push_str(&rest[..start]);
        match rest[start..].find("]]") {
            Some(end) => rest = &rest[start + end + 2..],
            None => return output,
        }
    }
    output.push_str(rest);
    output
}

/// 去掉开头的舞台指示，例如 `[轻声]`、`（笑）`。
fn trim_stage_direction(text: &str) -> &str {
    let trimmed = text.trim_start();
    for (open, close) in [('[', ']'), ('（', '）'), ('(', ')'), ('【', '】')] {
        if let Some(rest) = trimmed.strip_prefix(open)
            && let Some(end) = rest.find(close)
        {
            let inner = &rest[..end];
            // 只裁掉短的、明显是动作/语气描写的括号内容。
            if inner.chars().count() <= 12 {
                return rest[end + close.len_utf8()..].trim_start();
            }
        }
    }
    trimmed
}

/// 按字数截断，尽量切在句末标点上。
fn truncate_spoken(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let head: String = text.chars().take(max_chars).collect();
    let cut = head
        .char_indices()
        .filter(|(_, character)| {
            matches!(character, '。' | '！' | '？' | '!' | '?' | '…' | '；' | ';')
        })
        .map(|(index, character)| index + character.len_utf8())
        .rfind(|index| *index >= head.len() / 2);
    match cut {
        Some(index) => head[..index].to_owned(),
        None => head,
    }
}

/// 取来电者最近的私聊记忆，作为电话里的背景资料。
async fn load_caller_context(caller: i64) -> String {
    let memories = MEMORY_MANAGER
        .get_recent_memories_for_subject(caller, Some("private"), CONTEXT_MEMORIES)
        .await;
    let mut lines = Vec::new();
    for memory in memories.iter().rev() {
        let content = memory
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if content.is_empty() {
            continue;
        }
        lines.push(format!(
            "- {}",
            content
                .chars()
                .take(CONTEXT_MEMORY_CHARS)
                .collect::<String>()
        ));
    }
    lines.join("\n")
}

fn push_turn(transcript: &Arc<Mutex<Vec<Turn>>>, turn: Turn) {
    let mut turns = transcript
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if turns.len() == MAX_TRANSCRIPT_TURNS {
        turns.remove(0);
    }
    turns.push(turn);
}

/// 挂断后把这次通话写回来电者的私聊记忆。
async fn archive_call(
    config: &QqCallConfig,
    caller: Option<i64>,
    caller_name: Option<&str>,
    turns: &[Turn],
    end_reason: &str,
) {
    if !config.archive_to_memory() || turns.is_empty() {
        return;
    }
    let Some(caller) = caller else {
        return;
    };
    let mut body = String::new();
    for turn in turns {
        let speaker = if turn.from_peer {
            caller_name.unwrap_or("对方")
        } else {
            "芸汐"
        };
        body.push_str(&format!("{speaker}：{}\n", turn.text));
    }
    let entry = MemoryEntry {
        id: format!("qq-call-{}", uuid::Uuid::new_v4()),
        content: format!(
            "[QQ语音通话记录] 结束原因：{end_reason}\n{}",
            body.trim_end()
        ),
        timestamp: chrono::Local::now(),
        memory_type: MemoryType::Conversation,
        importance: 7,
        tags: vec!["qq_call".to_string()],
        context: "private".to_string(),
        subject_id: Some(caller),
    };
    if let Err(error) = MEMORY_MANAGER.add_memory(entry).await {
        eprintln!("[ERROR] QQ 通话记录写入记忆失败: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Turn, build_messages, phone_system_prompt, sanitize_reply, strip_protocol_markers,
    };
    use crate::config::QqCallConfig;

    #[test]
    fn protocol_markers_are_removed() {
        assert_eq!(strip_protocol_markers("好的[[WAIT]]呀"), "好的呀");
        assert_eq!(strip_protocol_markers("[[REPLY_ACTION]]嗯"), "嗯");
        assert_eq!(strip_protocol_markers("没有标记"), "没有标记");
        // 未闭合的标记直接截断，不把半截协议读出来。
        assert_eq!(strip_protocol_markers("好的[[未闭合"), "好的");
    }

    #[test]
    fn stage_directions_are_trimmed() {
        assert_eq!(
            sanitize_reply("[轻声]我在呢", 40).as_deref(),
            Some("我在呢")
        );
        assert_eq!(
            sanitize_reply("（笑）你说吧", 40).as_deref(),
            Some("你说吧")
        );
        // 长括号内容不是舞台指示，保留原样。
        let long = format!("[{}-{}]我在", "a".repeat(10), "b".repeat(10));
        assert!(sanitize_reply(&long, 80).is_some_and(|text| text.contains("我在")));
    }

    #[test]
    fn replies_are_limited_to_one_spoken_line() {
        let reply = sanitize_reply("第一句。\n第二句。", 40).expect("应取第一句");
        assert_eq!(reply, "第一句。");
    }

    #[test]
    fn blank_replies_are_rejected() {
        assert_eq!(sanitize_reply("   \n  ", 40), None);
        assert_eq!(sanitize_reply("[[WAIT]]", 40), None);
    }

    #[test]
    fn long_replies_cut_at_sentence_boundary() {
        let reply = sanitize_reply(
            "我今天其实有点累了，下午一直在整理房间，还洗了衣服，现在想歇一会儿再继续。",
            20,
        )
        .expect("应截断");
        assert!(reply.chars().count() <= 20, "截断后不该超长: {reply}");
        assert!(reply.ends_with('。') || reply.ends_with('，') || reply.chars().count() == 20);
    }

    #[test]
    fn message_sequence_starts_with_persona_and_ends_with_history() {
        let config = QqCallConfig::default();
        let turns = vec![
            Turn {
                from_peer: true,
                text: "喂".to_string(),
            },
            Turn {
                from_peer: false,
                text: "在的".to_string(),
            },
        ];
        let messages = build_messages(&config, "", &turns);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, phone_system_prompt(&config));
        assert_eq!(messages[2].content, "喂");
        assert_eq!(messages[3].content, "在的");
    }

    #[test]
    fn history_window_is_bounded() {
        let config: QqCallConfig =
            kovi::toml::from_str("history_turns = 1").expect("部分配置应可反序列化");
        let turns = vec![
            Turn {
                from_peer: true,
                text: "一".to_string(),
            },
            Turn {
                from_peer: false,
                text: "二".to_string(),
            },
            Turn {
                from_peer: true,
                text: "三".to_string(),
            },
        ];
        let messages = build_messages(&config, "", &turns);
        // system + 上下文说明 + 最后两轮
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].content, "二");
        assert_eq!(messages[3].content, "三");
    }

    #[test]
    fn caller_context_is_injected_as_data() {
        let config = QqCallConfig::default();
        let messages = build_messages(&config, "- 上次说要早点睡", &[]);
        assert_eq!(messages.len(), 3);
        assert!(messages[1].content.contains("<参考上下文"));
        assert!(messages[1].content.contains("上次说要早点睡"));
    }
}
