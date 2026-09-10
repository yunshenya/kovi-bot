//! 通话语音活动检测（VAD）与切段。
//!
//! 这是一个纯状态机：输入定长 PCM 帧，输出"对方开始说话"和"一句话说完"
//! 两个事件。算法与社区验证过的实现一致——自适应噪声底 + 迟滞阈值 +
//! 预滚缓冲，代价是零依赖、CPU 占用可以忽略，适合常驻在通话链路上。
//!
//! 之所以不用模型 VAD：通话链路已经有 ASR 在做重活，能量 VAD 在这里足以
//! 切开句子，而且不会给服务器再添一份常驻推理负载。

use std::collections::VecDeque;

/// 预滚缓冲帧数：命中起录条件时把这之前的音频一起带上，避免吞掉字头。
const PRE_ROLL_FRAMES: usize = 10;
/// 起录判定窗口帧数。
const ONSET_WINDOW_FRAMES: usize = 5;
/// 起录所需的窗口内语音帧数。
const ONSET_VOICED_FRAMES: usize = 3;
/// 自适应噪声底的更新下限：只有明显低于这个电平时才认为是环境噪声。
const NOISE_FLOOR_CEILING_DBFS: f32 = -32.0;
/// 噪声底平滑系数（旧值权重）。
const NOISE_FLOOR_DECAY: f32 = 0.98;
/// 噪声底平滑系数（新值权重）。
const NOISE_FLOOR_GAIN: f32 = 0.02;
/// 相对噪声底的判定余量（dB）。
const VOICE_MARGIN_DB: f32 = 10.0;
/// 阈值下限与上限，避免安静或嘈杂环境下阈值跑飞。
const MIN_THRESHOLD_DBFS: f32 = -48.0;
const MAX_THRESHOLD_DBFS: f32 = -34.0;
/// 无信号时的电平。
const SILENCE_DBFS: f32 = -100.0;
/// 噪声底初值。
const INITIAL_NOISE_FLOOR_DBFS: f32 = -60.0;

/// 一帧的处理结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrameOutcome {
    /// 对方刚开始说话（用于打断正在播放的 TTS）。
    pub speech_started: bool,
    /// 一段完整语音；包含预滚音频。
    pub utterance: Option<Vec<u8>>,
}

/// 通话语音切段器。
pub struct Segmenter {
    frame_ms: u32,
    end_of_speech_frames: u32,
    barge_in_speech_frames: u32,
    max_frames: usize,
    min_utterance_ms: u64,
    min_speech_ms: u64,
    noise_floor: f32,
    pre_roll: VecDeque<Vec<u8>>,
    recent_voice: VecDeque<bool>,
    recording: Vec<Vec<u8>>,
    speech_frames: u32,
    silent_frames: u32,
    barge_in_notified: bool,
}

impl Segmenter {
    pub fn new(
        frame_ms: u32,
        end_of_speech_frames: u32,
        barge_in_speech_frames: u32,
        max_utterance_ms: u64,
        min_utterance_ms: u64,
        min_speech_ms: u64,
    ) -> Self {
        let max_frames = (max_utterance_ms / u64::from(frame_ms.max(1))).max(1) as usize;
        Self {
            frame_ms,
            end_of_speech_frames,
            barge_in_speech_frames,
            max_frames,
            min_utterance_ms,
            min_speech_ms,
            noise_floor: INITIAL_NOISE_FLOOR_DBFS,
            pre_roll: VecDeque::with_capacity(PRE_ROLL_FRAMES),
            recent_voice: VecDeque::with_capacity(ONSET_WINDOW_FRAMES),
            recording: Vec::new(),
            speech_frames: 0,
            silent_frames: 0,
            barge_in_notified: false,
        }
    }

    /// 当前是否正在录制一段语音。
    pub fn recording(&self) -> bool {
        !self.recording.is_empty()
    }

    /// 丢弃所有进行中的状态（通话结束或尚未接通时调用）。
    pub fn reset(&mut self) {
        self.pre_roll.clear();
        self.recent_voice.clear();
        self.recording.clear();
        self.speech_frames = 0;
        self.silent_frames = 0;
        self.barge_in_notified = false;
    }

    /// 送入一帧定长 PCM（单声道 S16LE）。
    pub fn push(&mut self, frame: &[u8]) -> FrameOutcome {
        let mut outcome = FrameOutcome::default();
        let dbfs = frame_dbfs(frame);
        let threshold =
            (self.noise_floor + VOICE_MARGIN_DB).clamp(MIN_THRESHOLD_DBFS, MAX_THRESHOLD_DBFS);
        let voiced = dbfs >= threshold;
        if !voiced && dbfs < NOISE_FLOOR_CEILING_DBFS {
            self.noise_floor = self.noise_floor * NOISE_FLOOR_DECAY + dbfs * NOISE_FLOOR_GAIN;
        }

        if self.recording.is_empty() {
            push_bounded(&mut self.pre_roll, frame.to_vec(), PRE_ROLL_FRAMES);
            push_bounded(&mut self.recent_voice, voiced, ONSET_WINDOW_FRAMES);
            if self.recent_voice.len() == ONSET_WINDOW_FRAMES
                && self.recent_voice.iter().filter(|value| **value).count() >= ONSET_VOICED_FRAMES
            {
                self.recording = self.pre_roll.iter().cloned().collect();
                self.speech_frames =
                    self.recent_voice.iter().filter(|value| **value).count() as u32;
                self.silent_frames = 0;
                self.barge_in_notified = false;
            }
            return outcome;
        }

        self.recording.push(frame.to_vec());
        if voiced {
            self.speech_frames += 1;
            self.silent_frames = 0;
        } else {
            self.silent_frames += 1;
        }

        if !self.barge_in_notified && self.speech_frames >= self.barge_in_speech_frames {
            self.barge_in_notified = true;
            outcome.speech_started = true;
        }

        let reached_silence = self.silent_frames >= self.end_of_speech_frames;
        let reached_limit = self.recording.len() >= self.max_frames;
        if !reached_silence && !reached_limit {
            return outcome;
        }

        let utterance_ms = self.recording.len() as u64 * u64::from(self.frame_ms);
        let speech_ms = u64::from(self.speech_frames) * u64::from(self.frame_ms);
        if utterance_ms >= self.min_utterance_ms && speech_ms >= self.min_speech_ms {
            let mut pcm = Vec::with_capacity(
                self.recording
                    .iter()
                    .map(|frame| frame.len())
                    .sum::<usize>(),
            );
            for frame in &self.recording {
                pcm.extend_from_slice(frame);
            }
            outcome.utterance = Some(pcm);
        }
        self.reset();
        outcome
    }
}

fn push_bounded<T>(buffer: &mut VecDeque<T>, value: T, capacity: usize) {
    if buffer.len() == capacity {
        buffer.pop_front();
    }
    buffer.push_back(value);
}

/// 一帧 PCM（单声道 S16LE）的满度分贝值。
pub fn frame_dbfs(frame: &[u8]) -> f32 {
    let samples = frame.len() / 2;
    if samples == 0 {
        return SILENCE_DBFS;
    }
    let mut square_sum = 0.0_f64;
    for chunk in frame.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f64;
        square_sum += sample * sample;
    }
    let rms = (square_sum / samples as f64).sqrt();
    if rms <= 0.0 {
        return SILENCE_DBFS;
    }
    (20.0 * (rms / 32768.0).log10()) as f32
}

#[cfg(test)]
mod tests {
    use super::{Segmenter, frame_dbfs};

    const FRAME_MS: u32 = 30;
    const BYTES_PER_FRAME: usize = 16_000 * FRAME_MS as usize / 1000 * 2;

    fn silent_frame() -> Vec<u8> {
        vec![0_u8; BYTES_PER_FRAME]
    }

    fn loud_frame(amplitude: i16) -> Vec<u8> {
        let mut frame = Vec::with_capacity(BYTES_PER_FRAME);
        for _ in 0..BYTES_PER_FRAME / 2 {
            frame.extend_from_slice(&amplitude.to_le_bytes());
        }
        frame
    }

    fn segmenter() -> Segmenter {
        Segmenter::new(FRAME_MS, 18, 18, 15_000, 700, 450)
    }

    #[test]
    fn silence_never_starts_a_recording() {
        let mut segmenter = segmenter();
        for _ in 0..200 {
            let outcome = segmenter.push(&silent_frame());
            assert_eq!(outcome, super::FrameOutcome::default());
        }
        assert!(!segmenter.recording());
    }

    #[test]
    fn speech_then_silence_yields_one_utterance() {
        let mut segmenter = segmenter();
        let mut utterance = None;
        // 先有 0.5 秒安静环境，让噪声底稳定。
        for _ in 0..16 {
            segmenter.push(&silent_frame());
        }
        // 1 秒语音。
        for _ in 0..33 {
            let outcome = segmenter.push(&loud_frame(8_000));
            utterance = utterance.or(outcome.utterance);
        }
        assert!(segmenter.recording(), "持续语音期间应处于录制状态");
        // 1 秒静音触发句尾。
        for _ in 0..34 {
            let outcome = segmenter.push(&silent_frame());
            utterance = utterance.or(outcome.utterance);
        }
        let utterance = utterance.expect("应切出一段完整语音");
        // 至少包含 1 秒语音，且带上了预滚帧。
        assert!(utterance.len() >= 33 * BYTES_PER_FRAME);
        assert!(!segmenter.recording());
    }

    #[test]
    fn short_blip_is_discarded() {
        let mut segmenter = segmenter();
        // 3 帧（90 毫秒）语音不足以满足 min_speech_ms。
        for _ in 0..3 {
            segmenter.push(&loud_frame(9_000));
        }
        let mut utterance = None;
        for _ in 0..20 {
            let outcome = segmenter.push(&silent_frame());
            utterance = utterance.or(outcome.utterance);
        }
        assert_eq!(utterance, None, "过短片段应被丢弃");
    }

    #[test]
    fn speech_started_fires_once_per_utterance() {
        let mut segmenter = segmenter();
        for _ in 0..16 {
            segmenter.push(&silent_frame());
        }
        let mut starts = 0;
        for _ in 0..40 {
            if segmenter.push(&loud_frame(8_000)).speech_started {
                starts += 1;
            }
        }
        assert_eq!(starts, 1);
    }

    #[test]
    fn max_utterance_forces_a_cut() {
        // 上限 300 毫秒、句尾需要 18 帧静音，因此只能靠时长上限切段。
        let mut segmenter = Segmenter::new(FRAME_MS, 100, 100, 300, 100, 100);
        let mut utterance = None;
        for _ in 0..60 {
            let outcome = segmenter.push(&loud_frame(8_000));
            if outcome.utterance.is_some() {
                utterance = outcome.utterance;
                break;
            }
        }
        let utterance = utterance.expect("超过时长上限应强制切段");
        assert_eq!(utterance.len(), 10 * BYTES_PER_FRAME);
    }

    #[test]
    fn reset_clears_recording_state() {
        let mut segmenter = segmenter();
        for _ in 0..10 {
            segmenter.push(&loud_frame(8_000));
        }
        segmenter.reset();
        assert!(!segmenter.recording());
        assert_eq!(segmenter.speech_frames, 0);
    }

    #[test]
    fn dbfs_matches_reference_values() {
        assert_eq!(frame_dbfs(&[]), super::SILENCE_DBFS);
        assert_eq!(frame_dbfs(&silent_frame()), super::SILENCE_DBFS);
        let full = frame_dbfs(&loud_frame(i16::MAX));
        assert!(full > -0.5 && full <= 0.0, "满幅应接近 0 dBFS，实际 {full}");
        let half = frame_dbfs(&loud_frame(i16::MAX / 2));
        assert!((half + 6.02).abs() < 0.1, "半幅应约为 -6 dBFS，实际 {half}");
    }
}
