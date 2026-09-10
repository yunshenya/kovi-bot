//! 本机语音服务客户端（ASR + TTS）。
//!
//! 通话链路与"发语音消息"共用这一份客户端：两者都只依赖同一个只监听回环的
//! 本机语音服务。
//!
//! 识别与合成都在服务器上的一个常驻本地服务里完成，机器人只按 HTTP 调用它。
//! 这样做的原因：中文流式 ASR/TTS 的实际实现是 ONNX 运行时加模型文件，
//! 把它们链接进机器人二进制会让 CI 构建多出一整套原生依赖；而独立服务可以
//! 单独升级模型、单独限制内存，也不会在通话以外占用机器人进程的资源。
//!
//! 约定的最小协议（服务端实现见 `tools/speech-service`）：
//!
//! - `POST <asr_url>`：请求体是 16 kHz 单声道 S16LE 的 WAV，返回
//!   `{"text": "..."}`；
//! - `POST <tts_url>`：请求体是 `{"text": "..."}`，响应体是单声道 S16LE
//!   裸 PCM，采样率由响应头 `X-Sample-Rate` 给出，边合成边下发。
//!
//! 两个地址都必须是回环地址（配置层已经强制），服务不对外暴露。

use crate::config::QqCallConfig;
use serde::Deserialize;
use std::time::Duration;

/// 单次识别最多接受的字数，防止异常服务把超长文本灌进对话。
const MAX_TRANSCRIPT_CHARS: usize = 2_000;

pub struct SpeechClient {
    http: reqwest::Client,
    asr_url: String,
    tts_url: String,
    tts_sample_rate: u32,
    asr_timeout: Duration,
    tts_timeout: Duration,
}

impl SpeechClient {
    /// 只用于合成（发语音消息）的客户端。
    ///
    /// 与通话链路使用同一个本机语音服务；这里不配置 ASR，因为发语音不需要识别。
    pub(crate) fn for_tts_only(
        tts_url: &str,
        tts_timeout_secs: u64,
        tts_sample_rate: u32,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(tts_timeout_secs.max(1)))
            .build()
            .map_err(|error| anyhow::anyhow!("无法创建语音服务 HTTP 客户端: {error}"))?;
        Ok(Self {
            http,
            asr_url: String::new(),
            tts_url: tts_url.to_owned(),
            tts_sample_rate,
            asr_timeout: Duration::from_secs(tts_timeout_secs.max(1)),
            tts_timeout: Duration::from_secs(tts_timeout_secs.max(1)),
        })
    }

    pub fn new(config: &QqCallConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.tts_timeout_secs().max(1)))
            .build()
            .map_err(|error| anyhow::anyhow!("无法创建语音服务 HTTP 客户端: {error}"))?;
        Ok(Self {
            http,
            asr_url: config.asr_url().to_owned(),
            tts_url: config.tts_url().to_owned(),
            tts_sample_rate: config.tts_sample_rate(),
            asr_timeout: Duration::from_secs(config.asr_timeout_secs()),
            tts_timeout: Duration::from_secs(config.tts_timeout_secs()),
        })
    }

    /// 识别一段完整语音。`pcm` 是单声道 S16LE 裸 PCM。
    pub async fn transcribe(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        if pcm.is_empty() {
            return Ok(String::new());
        }
        let body = pcm_to_wav(pcm, sample_rate);
        let response = self
            .http
            .post(&self.asr_url)
            .header(reqwest::header::CONTENT_TYPE, "audio/wav")
            .timeout(self.asr_timeout)
            .body(body)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("语音识别请求失败: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("语音识别返回 HTTP {}", status.as_u16()));
        }
        let payload: AsrResponse = response
            .json()
            .await
            .map_err(|error| anyhow::anyhow!("语音识别返回了非法 JSON: {error}"))?;
        let text = payload.text.trim();
        if text.chars().count() > MAX_TRANSCRIPT_CHARS {
            return Err(anyhow::anyhow!("语音识别结果异常长，已丢弃"));
        }
        Ok(text.to_owned())
    }

    /// 开始合成一句话，返回可以逐块读取的流。
    pub async fn synthesize(&self, text: &str) -> anyhow::Result<SpeechStream> {
        let response = self
            .http
            .post(&self.tts_url)
            .json(&serde_json::json!({
                "text": text,
                "sample_rate": self.tts_sample_rate,
            }))
            .timeout(self.tts_timeout)
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("语音合成请求失败: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("语音合成返回 HTTP {}", status.as_u16()));
        }
        let sample_rate = response
            .headers()
            .get("x-sample-rate")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(self.tts_sample_rate);
        Ok(SpeechStream {
            response,
            sample_rate,
        })
    }
}

/// 合成结果流。逐块读取可以做到"合成一句、播出半句"。
pub struct SpeechStream {
    response: reqwest::Response,
    sample_rate: u32,
}

impl SpeechStream {
    /// 服务实际使用的输出采样率。
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 读取下一块 PCM；返回 `None` 表示合成结束。
    pub async fn next_chunk(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        // `chunk()` 不需要 reqwest 的 stream feature，且天然按网络到达顺序
        // 返回，正是流式播放想要的语义。
        match self.response.chunk().await {
            Ok(Some(chunk)) => Ok(Some(chunk.to_vec())),
            Ok(None) => Ok(None),
            Err(error) => Err(anyhow::anyhow!("读取合成音频失败: {error}")),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AsrResponse {
    #[serde(default)]
    text: String,
}

/// 给裸 PCM 套一个最小 WAV 头（单声道、16 位）。
pub fn pcm_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * 2;
    let mut wav = Vec::with_capacity(pcm.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

#[cfg(test)]
mod tests {
    use super::pcm_to_wav;

    #[test]
    fn wav_header_describes_mono_s16_pcm() {
        let pcm = vec![0_u8; 320];
        let wav = pcm_to_wav(&pcm, 16_000);
        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]),
            36 + 320
        );
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        assert_eq!(
            u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]),
            32_000
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
        assert_eq!(
            u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]),
            320
        );
    }
}
