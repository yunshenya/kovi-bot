//! 桥隔离出来的 PulseAudio 设备上的音频采集与播放。
//!
//! 桥为通话建了一套**独立**的 PulseAudio 服务（只监听一个私有 unix socket），
//! 并提供三个虚拟设备：
//!
//! - `maibot_qq_speaker`：QQ 把对端声音播到这里，我们读它的 `.monitor`；
//! - `maibot_qq_mic`：我们把芸汐的声音播到这里；
//! - `maibot_qq_mic_source`：QQ 把它当作默认麦克风。
//!
//! 采集与播放都用 `parec` / `pacat` 子进程实现：它们随 PulseAudio 客户端库
//! 一起分发，不需要把 libpulse 链接进机器人二进制，也不会让构建多出一套
//! 系统依赖。

use crate::config::QqCallConfig;
use kovi::tokio::io::{AsyncReadExt, AsyncWriteExt};
use kovi::tokio::process::{Child, ChildStdin, ChildStdout, Command};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

/// 保留的 stderr 尾部长度，用于诊断子进程为什么退出。
const STDERR_TAIL_BYTES: usize = 512;

/// 采集流：从 `maibot_qq_speaker.monitor` 读取定长 PCM 帧。
pub struct Capture {
    child: Child,
    stdout: ChildStdout,
    stderr_tail: Arc<Mutex<String>>,
    frame: Vec<u8>,
}

impl Capture {
    pub fn spawn(config: &QqCallConfig) -> anyhow::Result<Self> {
        let mut command = Command::new("parec");
        command
            .arg("--raw")
            .arg(format!("--device={}", config.capture_device()))
            .arg("--format=s16le")
            .arg(format!("--rate={}", config.capture_sample_rate()))
            .arg("--channels=1")
            .arg("--client-name=kovi-qq-call-vad")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        apply_pulse_server(&mut command, config);
        let mut child = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动 parec: {error}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("parec 没有可用的标准输出"))?;
        let stderr_tail = drain_stderr(child.stderr.take());
        Ok(Self {
            child,
            stdout,
            stderr_tail,
            frame: vec![0_u8; config.frame_bytes()],
        })
    }

    /// 读取下一帧。返回的切片在下次调用前有效。
    ///
    /// `parec` 退出（设备消失、PulseAudio 重启）时返回错误，调用方应结束
    /// 本次通话而不是空转重试。
    pub async fn next_frame(&mut self) -> anyhow::Result<&[u8]> {
        match self.stdout.read_exact(&mut self.frame).await {
            Ok(_) => Ok(&self.frame),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                Err(anyhow::anyhow!("parec 已退出（{}）", self.stderr_summary()))
            }
            Err(error) => Err(anyhow::anyhow!("读取通话音频失败: {error}")),
        }
    }

    fn stderr_summary(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trim()
            .to_owned();
        if tail.is_empty() {
            "无 stderr 输出".to_owned()
        } else {
            tail
        }
    }

    /// 结束采集进程。
    pub async fn shutdown(&mut self) {
        terminate(&mut self.child).await;
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // `kill_on_drop` 已经兜底；这里只保证不留下僵尸进程等待。
        let _ = self.child.start_kill();
    }
}

/// 播放流：把芸汐的 PCM 写进 `maibot_qq_mic`。
pub struct Playback {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr_tail: Arc<Mutex<String>>,
}

impl Playback {
    /// 以 `sample_rate` 打开播放流。
    ///
    /// 采样率必须用语音服务实际返回的那个（响应头 `X-Sample-Rate`），否则
    /// 语速和音高都会错。
    pub fn spawn(config: &QqCallConfig, sample_rate: u32) -> anyhow::Result<Self> {
        let mut command = Command::new("pacat");
        command
            .arg("--playback")
            .arg("--raw")
            .arg("--format=s16le")
            .arg(format!("--rate={sample_rate}"))
            .arg("--channels=1")
            .arg(format!("--device={}", config.playback_device()))
            .arg("--client-name=kovi-qq-call-tts")
            .arg(format!(
                "--latency-msec={}",
                config.tts_playback_latency_ms().max(20)
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        apply_pulse_server(&mut command, config);
        let mut child = command
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动 pacat: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("pacat 没有可用的标准输入"))?;
        let stderr_tail = drain_stderr(child.stderr.take());
        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr_tail,
        })
    }

    /// 写入一段 PCM。调用方可以边合成边写，实现流式播放。
    pub async fn write(&mut self, pcm: &[u8]) -> anyhow::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(anyhow::anyhow!("播放流已关闭"));
        };
        stdin
            .write_all(pcm)
            .await
            .map_err(|error| anyhow::anyhow!("写入通话音频失败: {error}"))
    }

    /// 冲刷并等待播放完成。
    pub async fn finish(&mut self) -> anyhow::Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
        }
        match self.child.wait().await {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(anyhow::anyhow!(
                "pacat 以状态 {status} 退出（{}）",
                self.stderr_summary()
            )),
            Err(error) => Err(anyhow::anyhow!("等待 pacat 退出失败: {error}")),
        }
    }

    /// 立即打断播放（对方插话时调用）。
    pub async fn interrupt(&mut self) {
        self.stdin.take();
        terminate(&mut self.child).await;
    }

    fn stderr_summary(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trim()
            .to_owned();
        if tail.is_empty() {
            "无 stderr 输出".to_owned()
        } else {
            tail
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn apply_pulse_server(command: &mut Command, config: &QqCallConfig) {
    if !config.pulse_server().is_empty() {
        command.env("PULSE_SERVER", config.pulse_server());
    }
    // 隔离 PulseAudio 实际要求客户端携带匹配 cookie（配置里的
    // auth-anonymous 在该版本上不生效），容器内的 QQ 以 root 运行免认证，
    // 但机器人以普通用户运行时必须显式带上，否则会被拒绝连接。
    if !config.pulse_cookie().is_empty() {
        command.env("PULSE_COOKIE", config.pulse_cookie());
    }
}

/// 后台排空子进程 stderr，只保留尾部若干字节用于报错。
fn drain_stderr(stderr: Option<kovi::tokio::process::ChildStderr>) -> Arc<Mutex<String>> {
    let tail = Arc::new(Mutex::new(String::new()));
    let Some(mut stderr) = stderr else {
        return tail;
    };
    let sink = Arc::clone(&tail);
    kovi::tokio::spawn(async move {
        let mut chunk = [0_u8; 256];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let text = String::from_utf8_lossy(&chunk[..read]).into_owned();
                    let mut guard = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.push_str(&text);
                    if guard.len() > STDERR_TAIL_BYTES {
                        let cut = guard.len() - STDERR_TAIL_BYTES;
                        let cut = guard
                            .char_indices()
                            .map(|(index, _)| index)
                            .find(|index| *index >= cut)
                            .unwrap_or(guard.len());
                        *guard = guard[cut..].to_owned();
                    }
                }
            }
        }
    });
    tail
}

/// 先礼后兵地结束一个子进程。
async fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}
