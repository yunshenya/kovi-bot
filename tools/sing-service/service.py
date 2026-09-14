#!/usr/bin/env python3
"""芸汐的歌声合成服务（"人力VOCALOID"路线）。

为什么是这条路：本机语音服务是**朗读**模型（sherpa-onnx VITS），没有音高/时值
这两个唱歌必需的维度。这里不引入新的歌声模型，而是逐字调用现有 TTS，再用
Praat 的 PSOLA 把每个字的基频改成音符频率、时长拉到音符时值——共振峰保持不变，
所以唱出来还是她自己的音色，CPU 也只需要几十毫秒一个字。

协议（只监听回环，与 speech-service 一样是本机小服务）：

    GET  /healthz        -> {"ok":true,"tts_ok":true,"templates":N}
    GET  /v1/templates   -> {"templates":[{"id","name","mood","syllables","bpm"}]}
    POST /v1/sing        -> 单声道 16 位 PCM 的 WAV
         {"template":"xiaoxingxing","lyrics":"一闪一闪亮晶晶"}
         可选 {"notes":[[1,1],[1,1],...],"octave":5,"tempo":1.0} 直接给简谱

简谱用级数表示：1-7 是音级（C 大调），后面跟的拍数决定时值；octave 默认 5
（1 = C5 = 523.25 Hz）。歌词按"一个汉字一个音节"切分；音节数与音符数不一致时，
多出来的歌词丢掉、歌词不够则把剩下的时值并进最后一个字（乐句仍然落在终止音上）。
"""

from __future__ import annotations

import json
import logging
import os
import sys
import threading
import urllib.error
import urllib.request
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
from pathlib import Path

import numpy as np
import parselmouth
from parselmouth.praat import call

LOG = logging.getLogger("yunxi-sing")

MAX_SECONDS = 45.0
MAX_LYRICS_CHARS = 120
# 请求体上限：与兄弟服务（embed 1 MB / speech 16 MB）同一思路，先夹住再读。
MAX_BODY_BYTES = 64 * 1024
TTS_TIMEOUT_SECS = 20
# 一个汉字的合成结果可以复用：同一首歌里重复字很多。
TTS_CACHE_LIMIT = 512
SAMPLE_RATE = 16_000
BREATH_SECONDS = 0.22

# 简谱级数 -> 相对主音的半音数（C 大调）
DEGREE_SEMITONES = {1: 0, 2: 2, 3: 4, 4: 5, 5: 7, 6: 9, 7: 11}
A4_HZ = 440.0
MIN_DEGREE = 1
MAX_DEGREE = 14  # 8-14 是 1-7 的高八度，方便写《茉莉花》这种跨八度的句子


def _transposed(hz: float, semitones: float) -> float:
    """整体移调。旋律的中心音应当落在她说话的基频附近（实测 248Hz 左右最像她自己）。"""
    if not semitones:
        return hz
    return hz * (2 ** (semitones / 12))


def _hz(degree: int, octave: int) -> float:
    """简谱级数 + 八度 -> 频率。1 在 octave=5 时是 C5，8 是 C6。"""
    if not MIN_DEGREE <= degree <= MAX_DEGREE:
        raise SingRequestError(f"非法音级: {degree}（只支持 1-7 与高八度 8-14）")
    if degree > 7:
        degree -= 7
        octave += 1
    quarter_tones = DEGREE_SEMITONES[degree] + (octave - 4) * 12 - 9
    return A4_HZ * (2 ** (quarter_tones / 12))


class SingRequestError(ValueError):
    """请求本身不合法（模板不存在、歌词为空等）。"""


class TtsClient:
    """调用本机语音服务逐字合成，并缓存结果。

    缓存键带上语速：同一个字在不同音符上要用不同的 length_scale 合成，否则就得靠
    事后拉伸，而拉伸会把字头削掉、听感发飘。
    """

    def __init__(self, url: str) -> None:
        self.url = url
        self._cache: dict[tuple[str, float], tuple[np.ndarray, int]] = {}
        self._lock = threading.Lock()

    def synthesize(self, text: str, speed: float = 1.0) -> tuple[np.ndarray, int]:
        speed = round(max(0.25, min(1.8, speed)), 2)
        key = (text, speed)
        with self._lock:
            cached = self._cache.get(key)
        if cached is not None:
            return cached
        body = json.dumps({"text": text, "sample_rate": SAMPLE_RATE, "speed": speed}).encode(
            "utf-8"
        )
        request = urllib.request.Request(
            self.url, data=body, headers={"Content-Type": "application/json"}
        )
        try:
            with urllib.request.urlopen(request, timeout=TTS_TIMEOUT_SECS) as response:
                rate = int(response.headers.get("X-Sample-Rate", SAMPLE_RATE))
                pcm = response.read()
        except (urllib.error.URLError, OSError, ValueError) as error:
            raise SingRequestError(f"语音服务不可用: {error}") from error
        if not pcm:
            raise SingRequestError("语音服务返回了空音频")
        samples = np.frombuffer(pcm, dtype="<i2").astype(np.float64) / 32768.0
        with self._lock:
            if len(self._cache) >= TTS_CACHE_LIMIT:
                self._cache.pop(next(iter(self._cache)))
            self._cache[key] = (samples, rate)
        return samples, rate

    def healthy(self) -> bool:
        health = self.url.rsplit("/v1/", 1)[0] + "/healthz"
        try:
            with urllib.request.urlopen(health, timeout=3):
                return True
        except Exception:  # noqa: BLE001 - 健康检查不区分原因
            return False


def trim_silence(samples: np.ndarray, rate: int, threshold: float = 0.006) -> np.ndarray:
    """削掉首尾静音。

    阈值刻意压得很低（0.6%）：单字只有一两百毫秒，削狠了会把字头（"一"的介音、
    "闪"的擦音）一起削掉，唱出来就是一个个秃音。
    """
    window = max(1, int(0.01 * rate))
    envelope = np.convolve(np.abs(samples), np.ones(window) / window, mode="same")
    loud = np.where(envelope > threshold)[0]
    if len(loud) == 0:
        return samples
    start = max(0, loud[0] - window)
    end = min(len(samples), loud[-1] + window)
    return samples[start:end]


PITCH_STEP = 0.02
"""基频轮廓的采样间隔（秒）。"""
VIBRATO_DEPTH = 0.007
VIBRATO_HZ = (4.6, 5.6)
JITTER_DEPTH = 0.0035
"""微抖幅度 ±0.35%（约 ±6 音分）。"""


def _pitch_contour(duration: float, freq: float, previous: float | None,
                   seed: int) -> tuple[np.ndarray, np.ndarray]:
    """算出整段的基频轮廓：起音滑音 → 直音 → 渐起的颤音，再叠一层零均值的微抖。

    这里必须守住一条：**轮廓的均值恰好等于 freq**。上一版拿
    ``np.cumsum(rng.normal(0.0, 0.35, 4096))`` 当 jitter，那是布朗游走——它会被 ±3 的
    截断钉在偏离 0 的位置整段不动，等价于每个音固定跑调（实测线上 deg2 低 18 音分、
    deg1/5/7 高 18 音分，相邻音程被压扁最多 36 音分，听感就是一路走音）。真人的
    jitter 是逐周期、零均值、几个音分的快抖，不是慢漂移；所以这里改用平滑过的零均值
    噪声，并在最后按均值归一化，保证"听着抖、平均音高就是那个音"。
    """
    times = np.clip(
        np.arange(0.0, duration + PITCH_STEP * 0.5, PITCH_STEP), 0.0, max(duration, PITCH_STEP)
    )
    count = int(times.size)
    rng = np.random.default_rng(seed)
    vibrato_hz = float(rng.uniform(*VIBRATO_HZ))
    noise = rng.standard_normal(count + 32)
    kernel = np.hanning(17)
    kernel /= kernel.sum()
    jitter = np.convolve(noise, kernel, mode="same")[16 : 16 + count]
    jitter = jitter - jitter.mean()
    span = float(np.max(np.abs(jitter)))
    if span > 0:
        jitter = jitter / span
    # 颤音延迟起振（先直后颤才是人唱的），深 0.7%
    onset = np.clip((times - 0.25 * duration) / max(0.12, 0.3 * duration), 0.0, 1.0)
    values = freq * (1.0 + JITTER_DEPTH * jitter) * (
        1.0 + VIBRATO_DEPTH * onset * np.sin(2 * np.pi * vibrato_hz * times)
    )
    if previous is not None and count:
        # 与上一个音之间 40 毫秒的滑音，第一个采样点只有 20 毫秒，所以取一半
        values[0] = previous + (freq - previous) * min(1.0, PITCH_STEP / 0.04)
    return times, values * (freq / float(np.mean(values)))


def _dominant_hz(samples: np.ndarray, rate: int, floor: float, ceiling: float) -> float:
    """量一下渲染结果中段的基频，只作诊断（不据此重渲）。"""
    sound = parselmouth.Sound(samples, sampling_frequency=rate)
    if sound.duration < 0.12:
        return 0.0
    pitch = call(sound, "To Pitch", 0.01, floor, ceiling)
    start, end = sound.duration * 0.3, sound.duration * 0.8
    values = [call(pitch, "Get value at time", float(t), "Hertz", "linear")
              for t in np.arange(start, max(start + 0.02, end), 0.02)]
    voiced = [value for value in values if value == value]
    return float(np.median(voiced)) if voiced else 0.0


QUIET_PEAK = 0.02
"""低于这个峰值就认为这个字没合成出来（实测单字"是""星"会得到近乎静音）。"""

LEVEL_ALIGN_DB = 12.0
"""逐音响度对齐的上限。旧版是 ±2.5dB，但逐字合成的响度差本来就到 20dB 以上
（实测"慢"的字均 RMS 只有"晶"的十分之一），2.5dB 的夹子等于没夹：轻的字照样
听不见、重的字照样炸。12dB 能把这类字拉回来，又不至于把乐句的起伏压成直线。"""

MIN_VOICED_SECONDS = 0.06
"""短于这个值的有声段撑不住一个长音。个别字在孤立合成下只给几十毫秒的元音，
靠循环这么点碎音凑满一个音符，比"这个音没唱准"还难听。"""


def _peak(samples: np.ndarray) -> float:
    return float(np.max(np.abs(samples))) if samples.size else 0.0


def _sung_level(piece: np.ndarray, rate: int) -> float:
    """一个音"唱出来"那部分的响度：有声段的 RMS，找不到就用整段。

    用整段 RMS 会被清辅音的字头带偏（字头又短又响），对齐之后元音反而更轻。
    """
    if piece.size == 0:
        return 0.0
    region = _voiced_region(piece, rate)
    if region is not None:
        piece = piece[region[0] : region[1]]
    return float(np.sqrt(np.mean(piece**2))) if piece.size else 0.0


def _energy_levels(samples: np.ndarray, rate: int) -> np.ndarray:
    """每 10 毫秒一格的 RMS 包络。"""
    hop = max(16, int(0.01 * rate))
    if samples.size < hop:
        return np.zeros(0)
    return np.array(
        [
            float(np.sqrt(np.mean(samples[i : i + hop] ** 2)))
            for i in range(0, samples.size - hop + 1, hop)
        ]
    )


def _carrier_head(carried: np.ndarray, rate: int) -> np.ndarray | None:
    """从"字，啊"里切出第一个字：在靠后的位置找一段最长的低能量区当切点。

    旧实现只认"最长低能量段 ≤ 8% 峰值"，而且只看全局最长的那一段——它常常落在**开头**
    （前置静音，实测"星，啊"就是 0.03 秒 @0.00 秒）。调用方又要求切点不早于 20 毫秒，
    于是这段被判掉、真正的字间低谷反而从没被考虑，最后整段"字，啊"被当成一个字唱进
    同一个音符——多唱一个"啊"。这里改成：跳过前 25%，在剩下部分里找最长的一段低谷，
    找不到就返回 None，交给调用方决定要不要整段用。
    """
    hop = max(16, int(0.01 * rate))
    levels = _energy_levels(carried, rate)
    if levels.size < 4:
        return None
    threshold = max(1e-4, float(levels.max()) * 0.10)
    best: tuple[int, int] | None = None
    run: int | None = None
    for index in range(max(1, int(levels.size * 0.25)), levels.size):
        if levels[index] <= threshold:
            run = index if run is None else run
            continue
        if run is not None:
            if best is None or index - run > best[1] - best[0]:
                best = (run, index)
            run = None
    if run is not None and (best is None or levels.size - run > best[1] - best[0]):
        best = (run, levels.size)
    if best is None or (best[1] - best[0]) * hop < int(0.03 * rate):
        return None
    head = carried[: best[0] * hop]
    return head if head.size >= int(0.03 * rate) else None


def _singable(samples: np.ndarray, rate: int) -> bool:
    """这段合成撑得住一个长音吗：要有一段够长的周期性（元音），且不是近乎静音。

    唱歌靠元音撑住长音，所以"有没有元音"才是可用性的判据。只看峰值会放过
    "月""慢"这类塌法——峰值够（0.05），但整段一个周期性帧都没有，最后只能把几十
    毫秒的碎音硬拉五倍，听起来就是一声没有音高的怪响。
    """
    if samples.size == 0 or _peak(samples) < QUIET_PEAK:
        return False
    region = _voiced_region(samples, rate)
    return region is not None and region[1] - region[0] >= int(MIN_VOICED_SECONDS * rate)


def synthesize_syllable(tts: "TtsClient", syllable: str, speed: float) -> tuple[np.ndarray, int]:
    """合成一个字的音频；孤立合成塌掉时按"字。"→"字，啊"两级载体把它带出来。

    实测 vits-zh-ll 对孤立汉字有三种塌法：整段近乎无声（"星""事"，峰值 0.001）、
    只有一小段噪声/鼻音而没有元音（"月""慢"）、以及拖得没意义的尾音。前两种都撑不住
    一个音符。载体按"最少多余材料"排序：先试句末的"字。"（模型会当成一句正常读，
    且不留多余音节），再试"字，啊"（最稳，但要切出第一个字）。
    """
    audio, rate = tts.synthesize(syllable, speed)
    if _singable(audio, rate):
        return audio, rate
    # 载体也会失败（TTS 超时/报错），此时保留最响的那一份，绝不让整个请求塌掉
    fallback, fallback_rate = audio, rate
    for template, cut in ((f"{syllable}。", False), (f"{syllable}，啊", True)):
        try:
            carried, carried_rate = tts.synthesize(template, speed)
        except SingRequestError as error:
            LOG.warning("载体合成 %s 失败: %s", template, error)
            continue
        if cut:
            head = _carrier_head(carried, carried_rate)
            if head is not None and _singable(head, carried_rate):
                LOG.debug("单字 %s 撑不住长音，用载体切出前 %.3fs", syllable, head.size / carried_rate)
                return head, carried_rate
            if _singable(carried, carried_rate):
                # 切不开就整段用：多一个"啊"也比这个音没有音高好
                LOG.debug("单字 %s 的载体切不开，整段使用（含尾字）", syllable)
                return carried, carried_rate
        elif _singable(carried, carried_rate):
            LOG.debug("单字 %s 撑不住长音，改用句末读法", syllable)
            return carried, carried_rate
        if _peak(carried) > _peak(fallback):
            fallback, fallback_rate = carried, carried_rate
    # 三种都撑不住：至少别丢音，取更响的那个
    return fallback, fallback_rate


def _voice_eq(samples: np.ndarray, rate: int) -> np.ndarray:
    """把合成的音色往"人声"掰一点。

    实测对比她本人说话：唱歌的 0–300Hz（胸腔）只有 20%（说话 35%），1–4kHz（硬度）
    却有 31%（说话 20%）——又薄又尖正是"听着怪"的主要来源。这里三段一起修：
    250Hz 低架 +4dB 补胸腔、3kHz 峰 −6dB 去硬度、7kHz 以上 −6dB 收毛刺。
    """
    if len(samples) < 64:
        return samples
    spectrum = np.fft.rfft(samples)
    freqs = np.fft.rfftfreq(len(samples), 1 / rate)
    safe = np.maximum(freqs, 1.0)
    shelf_low = 1 + (10 ** (4.0 / 20) - 1) / (1 + (safe / 250.0) ** 2)
    harsh = 1 - 0.50 * np.exp(-0.5 * (np.log(safe / 3000.0) / 0.50) ** 2)
    fizz = 1 - 0.50 / (1 + (7000.0 / safe) ** 4)
    return np.fft.irfft(spectrum * shelf_low * harsh * fizz, n=len(samples))


def _add_shimmer(piece: np.ndarray, rate: int, seed: int) -> np.ndarray:
    """给长音加一点点幅度与音高的自然起伏。

    循环出来的长音是"逐样本重复"的：谐噪比高、抖动接近零，耳朵一听就知道是机器。
    真人唱歌有 3–6Hz 的微颤与几个百分点的幅度起伏，加回去立刻就"活"了。
    """
    if len(piece) < int(0.12 * rate):
        return piece
    rng = np.random.default_rng(seed)
    time = np.arange(len(piece)) / rate
    tremor_hz = float(rng.uniform(3.2, 5.8))
    phase = float(rng.uniform(0, 2 * np.pi))
    depth = float(rng.uniform(0.03, 0.06))
    envelope = 1.0 + depth * np.sin(2 * np.pi * tremor_hz * time + phase)
    # 起音处不要抖，否则像坏了的磁带
    attack = np.clip(time / 0.15, 0.0, 1.0)
    return piece * (1.0 + (envelope - 1.0) * attack)


def _room_reverb(samples: np.ndarray, rate: int, wet: float = 0.16, rt60: float = 0.55) -> np.ndarray:
    """一点短混响：干声听起来像"贴着麦克风念"，加个房间反而更像在唱。"""
    if len(samples) < rate // 4:
        return samples
    rng = np.random.default_rng(7)
    length = int(rt60 * 1.2 * rate)
    time = np.arange(length) / rate
    impulse = rng.standard_normal(length) * np.exp(-6.9 * time / rt60)
    # 冲激必须低通：白噪声尾巴会往 1–8kHz 补一层毛刺，把 EQ 压下去的硬度又加回来
    # （实测 EQ 前后 1–4kHz 占比纹丝不动就是这个原因）。一阶低通足够。
    smooth = max(2, int(0.0004 * rate))
    impulse = np.convolve(impulse, np.ones(smooth) / smooth, mode="same")
    impulse[: int(0.008 * rate)] = 0.0  # 预延迟
    impulse *= np.hanning(length)
    wet_signal = np.convolve(samples, impulse / (np.abs(impulse).sum() + 1e-9), mode="full")[: len(samples)]
    return (1.0 - wet) * samples + wet * wet_signal * 3.0


def _crossfade_join(left: np.ndarray, right: np.ndarray, overlap: int) -> np.ndarray:
    """用 `overlap` 个样本把两段交叠淡化接起来（比硬拼接少一次波形跳变）。"""
    overlap = int(min(overlap, len(left), len(right)))
    if overlap <= 1:
        return np.concatenate([left, right])
    ramp = np.linspace(0.0, 1.0, overlap)
    return np.concatenate(
        [left[:-overlap], left[-overlap:] * (1 - ramp) + right[:overlap] * ramp, right[overlap:]]
    )


def _periodicity(frame: np.ndarray, rate: int) -> float:
    """归一化自相关峰：接近 1 是完全周期（元音），接近 0 是噪声/擦音。"""
    if frame.size < 32:
        return 0.0
    centered = frame - frame.mean()
    energy = float(np.dot(centered, centered))
    if energy <= 1e-12:
        return 0.0
    min_lag = max(2, int(rate / 500))
    max_lag = min(centered.size - 2, int(rate / 80))
    if max_lag <= min_lag:
        return 0.0
    spectrum = np.fft.rfft(centered, n=2 * centered.size)
    correlation = np.fft.irfft(spectrum * np.conj(spectrum))[: max_lag + 1]
    if correlation[0] <= 0:
        return 0.0
    return float(np.max(correlation[min_lag : max_lag + 1]) / correlation[0])


def _voiced_region(samples: np.ndarray, rate: int) -> tuple[int, int] | None:
    """用周期性（自相关峰）找有声区间。

    旧判据是"能量够 + 过零率低于 0.12"。过零率在低响度、擦音比例高的字上会漏判，而
    漏判的后果不是"少切一段"：调用方会退回整段不变调——那个字就用说话的调唱出来了
    （实测线上"慢""开"两处整字没有变调，等于唱错音）。元音帧的自相关峰在 0.5 以上、
    擦音在 0.3 以下，这个判据在几十毫秒的短音上也站得住。
    """
    hop = max(16, int(0.008 * rate))
    window = max(hop, int(0.032 * rate))
    if samples.size < 2 * hop:
        return None
    frames = [
        (
            float(np.sqrt(np.mean(samples[start : start + window] ** 2))),
            _periodicity(samples[start : start + window], rate),
        )
        for start in range(0, samples.size - hop, hop)
    ]
    peak = max(rms for rms, _ in frames)
    if peak <= 0:
        return None
    voiced = [
        index
        for index, (rms, periodicity) in enumerate(frames)
        if periodicity >= 0.45 and rms >= 0.18 * peak
    ]
    if not voiced:
        return None
    start = voiced[0] * hop
    end = min(samples.size, voiced[-1] * hop + window)
    if end - start < int(0.02 * rate):
        return None
    return start, end


def _safe_floor(duration: float, minimum: float = 75.0) -> float:
    """Praat 的分析窗（3/下限）必须装得进音频：短音上给 75Hz 会直接报
    ``minimum pitch must not be less than``。实测门槛正好是 9/时长。"""
    return max(minimum, min(400.0, 9.0 / max(0.01, duration)))


def _resynthesize(samples: np.ndarray, rate: int, freq: float, previous: float | None,
                  target_seconds: float, seed: int) -> np.ndarray:
    """一次 PSOLA 同时改音高与时值。

    旧链路是"PSOLA 改音高 → 循环元音核 → 再用 Lengthen (PSOLA) 补零头"，同一个音要过
    三遍重合成，接缝、颗粒感与颤音被循环复制的问题都从这儿来。Praat 的 Manipulation
    本来就能同时挂 PitchTier 与 DurationTier：一次重合成把两件事做完，共振峰也保得住。
    """
    sound = parselmouth.Sound(samples, sampling_frequency=rate)
    manipulation = call(sound, "To Manipulation", 0.01, _safe_floor(sound.duration), 1400)
    times, values = _pitch_contour(sound.duration, freq, previous, seed)
    tier = call(manipulation, "Create PitchTier", "empty", 0.0, sound.duration)
    for at, value in zip(times.tolist(), values.tolist()):
        call(tier, "Add point", float(at), float(value))
    call([manipulation, tier], "Replace pitch tier")
    ratio = target_seconds / max(1e-6, sound.duration)
    if not 0.98 <= ratio <= 1.02:
        duration_tier = call(
            manipulation, "Create DurationTier", "empty", 0.0, sound.duration
        )
        call(duration_tier, "Add point", 0.0, float(ratio))
        call(duration_tier, "Add point", float(sound.duration), float(ratio))
        call([manipulation, duration_tier], "Replace duration tier")
    return call(manipulation, "Get resynthesis (overlap-add)").values[0]


def _tile_with_crossfade(samples: np.ndarray, rate: int, target_seconds: float) -> np.ndarray:
    """整段平铺到目标长度，只给"整段没有周期性"的清辅音字兜底。

    清辅音没有音高可改，但时值仍然要占满这个音符；重复字头总比空一拍强。
    """
    target = max(1, int(round(target_seconds * rate)))
    if samples.size == 0:
        return np.zeros(target)
    fade = max(1, min(int(0.012 * rate), samples.size // 2))
    audio = samples.copy()
    while audio.size < target:
        audio = _crossfade_join(audio, samples, fade)
    return audio[:target]


def _exact_seconds(samples: np.ndarray, rate: int, target_seconds: float,
                   expected_hz: float) -> np.ndarray:
    """把时长精确对齐到目标秒数。

    Praat 的时长档在 3 倍以上会欠一点（实测目标 1.09s 只给到 0.90s），所以重合成之后
    还要收一次尾：长了从尾部削（并做 5 毫秒收尾防咔哒），短了循环稳态段补齐。
    """
    target = max(1, int(round(target_seconds * rate)))
    if samples.size >= target:
        trimmed = samples[:target].copy()
        fade = min(int(0.005 * rate), target // 2)
        if fade > 1:
            trimmed[-fade:] *= np.linspace(1.0, 0.0, fade)
        return trimmed
    looped = _loop_nucleus(samples, rate, target_seconds, expected_hz)
    if looped is not None and looped.size > samples.size:
        samples = looped
    if samples.size < target:
        samples = _tile_with_crossfade(samples, rate, target_seconds)
    return samples[:target]


MIN_SYNTH_SPEED = 0.40
"""放慢的下限。实测 VITS 到 0.35 还能再长一点，但 0.4 以下个别字会塌成近静音。"""


def _stretch_source(tts: "TtsClient", syllable: str,
                    duration: float) -> tuple[np.ndarray, int]:
    """挑一个尽量长、又没有塌掉的逐字合成结果。

    时值靠"合成时放慢"，不是事后硬拉：单字只有 0.1~0.2 秒，一个音符却常常
    0.55~1.1 秒。旧版按 ``natural/duration`` 算语速再夹到 0.45，而单字几乎必然小于
    0.45，于是**每个字都被夹在 0.45**；慢速下 VITS 有时反而更短（实测"月" speed=1.0
    是 0.21 秒、0.45 是 0.12 秒），短过 30 毫秒后旧代码直接把整个音换成静音——一个音
    就这么没了。这里**先按放慢那一档合成**，只在它撑不住长音时才退回自然语速：一次
    TTS 调用要 120~210 毫秒，占整首歌渲染时间的九成以上，能少一次就少一次（重复字走
    TTS 缓存）。
    """
    best: np.ndarray | None = None
    best_rate = SAMPLE_RATE
    best_length = 0.0
    fallback: np.ndarray | None = None
    fallback_rate = SAMPLE_RATE
    fallback_length = 0.0
    for speed in (MIN_SYNTH_SPEED, 1.0):
        try:
            audio, rate = synthesize_syllable(tts, syllable, speed)
        except SingRequestError as error:
            LOG.warning("合成 %s 失败（speed=%.2f）: %s", syllable, speed, error)
            continue
        audio = trim_silence(audio, rate)
        length = audio.size / rate
        if length > fallback_length:
            fallback, fallback_rate, fallback_length = audio, rate, length
        if length > best_length and _singable(audio, rate):
            best, best_rate, best_length = audio, rate, length
        if best is not None:
            # 放慢那一档已经给出能撑住长音的素材，就不必再合成自然语速那一档
            break
    # 两档都撑不住时也不能丢音：宁可留一个塌掉的字，也不要一个空拍。
    if best is None:
        return (fallback, fallback_rate) if fallback is not None else (np.zeros(0), SAMPLE_RATE)
    return best, best_rate


def _nucleus_span(samples: np.ndarray, rate: int, expected_hz: float) -> tuple[int, int] | None:
    """找元音核（能量包络峰值附近），按整数个音高周期返回循环单元。

    刻意不调 Praat 做基频分析：音高是我们自己刚用 PSOLA 设进去的，
    ``rate / expected_hz`` 就是周期。分析器在"是""星"这种几十毫秒的擦音字上
    一个有效帧都取不到，而按已知周期切是稳的。
    """
    period = rate / max(60.0, expected_hz)
    window = max(1, int(0.005 * rate))
    envelope = np.convolve(np.abs(samples), np.ones(window) / window, mode="same")
    if envelope.size == 0 or float(envelope.max()) <= 0:
        return None
    peak = int(np.argmax(envelope))
    threshold = float(envelope[peak]) * 0.45
    reach = int(0.07 * rate)
    low = peak
    while low > 0 and envelope[low] > threshold and peak - low < reach:
        low -= 1
    high = peak
    while high < envelope.size - 1 and envelope[high] > threshold and high - peak < reach:
        high += 1
    minimum = 3 * period
    if high - low < minimum:
        half = int(minimum / 2)
        low = max(0, peak - half)
        high = min(len(samples), peak + half)
    cycles = max(2, int((high - low) // period))
    unit_length = int(cycles * period)
    if unit_length > len(samples):
        return None
    center = (low + high) // 2
    start = max(0, int(center - unit_length / 2))
    end = start + unit_length
    if end > len(samples):
        end = len(samples)
        start = max(0, end - unit_length)
    if end - start < int(2 * period):
        return None
    return start, end


def _loop_nucleus(
    samples: np.ndarray, rate: int, target: float, expected_hz: float
) -> np.ndarray | None:
    """把元音核（整数个音高周期）循环到目标长度。

    只在 PSOLA 的时长档欠了一点时用来补尾（见 `_exact_seconds`）。
    """
    span = _nucleus_span(samples, rate, expected_hz)
    if span is None:
        return None
    start, end = span
    unit = samples[start:end]
    if len(unit) < 16:
        return None
    head = samples[:start]
    tail = samples[end:]
    # 接缝 12ms：4ms 时实测 30–100Hz 包络抖动占比 28%（她说话只有 17%），
    # 听感就是一层"颗粒/嗡"；循环单元是整数个周期，加长淡化不会梳状滤波。
    fade = max(1, int(0.012 * rate))
    ramp = np.linspace(0.0, 1.0, fade)
    pieces = [head, unit.copy()]
    filled = len(head) + len(unit)
    budget = max(0, int(target * rate) - len(tail))
    guard = 0
    while filled < budget and guard < 500:
        piece = unit.copy()
        overlap = min(fade, len(pieces[-1]), len(piece))
        if overlap > 1:
            piece[:overlap] = piece[:overlap] * ramp[:overlap] + pieces[-1][-overlap:] * (
                1 - ramp[:overlap]
            )
            pieces[-1] = pieces[-1][:-overlap]
        pieces.append(piece)
        filled += len(piece) - overlap
        guard += 1
    pieces.append(tail)
    return np.concatenate(pieces)



def sing_note(tts: TtsClient, syllable: str, freq: float, duration: float,
              previous: float | None, seed: int = 0) -> tuple[np.ndarray, int]:
    """把一个字唱成 freq 这个音、时值 duration 秒。

    四步：合成（自然语速与放慢取更长的一个）→ 切出有声段（清辅音原样保留）→
    一次 PSOLA 把音高换成音符、时值拉到音符时值 → 收尾把总时长精确对齐。
    """
    if duration <= 0:
        return np.zeros(1), SAMPLE_RATE
    source, rate = _stretch_source(tts, syllable, duration)
    if source.size == 0:
        return np.zeros(max(1, int(duration * SAMPLE_RATE))), SAMPLE_RATE
    region = _voiced_region(source, rate)
    if region is None:
        # 整段没有周期性（纯清辅音字）：它本来就没有音高，硬套 PSOLA 只会把擦音
        # 变成嗡声；保住字头与时值，这个音按无音高唱。
        LOG.debug("字 %s 整段无周期性，按清辅音铺满时值", syllable)
        return _tile_with_crossfade(source, rate, duration), rate
    start, end = region
    if end - start < int(MIN_VOICED_SECONDS * rate):
        start, end = 0, source.size
    head, tail = source[:start], source[end:]
    # 元音才是被拉长的部分：清辅音按原速留在音符开头，这正是"唱"的形态
    target_voiced = max(0.04, duration - (head.size + tail.size) / rate)
    sung = _resynthesize(source[start:end], rate, freq, previous, target_voiced, seed)
    edge = max(1, int(0.003 * rate))
    piece = _crossfade_join(_crossfade_join(head, sung, edge), tail, edge)
    return _exact_seconds(piece, rate, duration, freq), rate


_WORKER_TTS: TtsClient | None = None


def _sing_note_job(job: tuple[str, str, float, float, float | None, int]) -> tuple[np.ndarray, int]:
    """进程池里的单音渲染。

    用进程而不是线程：parselmouth 底下是 Praat，带全局状态，多线程并发调用不保证
    安全。fork 出来的子进程各自持有自己的 TTS 客户端与缓存。
    """
    global _WORKER_TTS
    tts_url, syllable, freq, duration, previous, seed = job
    if _WORKER_TTS is None:
        _WORKER_TTS = TtsClient(tts_url)
    return sing_note(_WORKER_TTS, syllable, freq, duration, previous, seed)


def split_syllables(lyrics: str) -> list[str]:
    """一个汉字一个音节；空白与标点丢掉；连续拉丁字母当成一个音节。"""
    syllables: list[str] = []
    latin = ""
    for char in lyrics:
        if char.isascii() and char.isalnum():
            latin += char
            continue
        if latin:
            syllables.append(latin)
            latin = ""
        if "\u4e00" <= char <= "\u9fff":
            syllables.append(char)
    if latin:
        syllables.append(latin)
    return syllables


def fit_notes(notes: list[list[float]], syllable_count: int) -> list[tuple[float, float]]:
    """把旋律铺到歌词上，返回**恰好** ``syllable_count`` 个 (音级, 拍数)。

    三种情况都要能唱，而不是抛异常（线上就是"歌词 28 字 vs 模板 14 音"直接把
    整个请求打成 502，然后回退成念白）：

    - 一样长：原样；
    - 歌词更长：旋律重复（一首歌写满四句，就把两句的曲再唱一遍——人也是这么唱的）；
    - 歌词更短：多出来的音符时值并进最后一个字，乐句仍然落在终止音上。
    """
    if syllable_count <= 0 or not notes:
        return []
    if syllable_count == len(notes):
        return [(float(degree), float(beats)) for degree, beats in notes]
    if syllable_count > len(notes):
        tiled: list[tuple[float, float]] = []
        while len(tiled) < syllable_count:
            tiled.extend((float(degree), float(beats)) for degree, beats in notes)
        return tiled[:syllable_count]
    head = [(float(degree), float(beats)) for degree, beats in notes[: syllable_count - 1]]
    tail_beats = sum(beats for _, beats in notes[syllable_count - 1 :])
    head.append((float(notes[syllable_count - 1][0]), tail_beats))
    return head


def fit_breaths(notes: list[list[float]], syllable_count: int,
                breath_after: tuple[int, ...]) -> frozenset[int]:
    """把"句尾换气"位置跟着旋律一起铺开，返回在哪些音之后换气（1 基）。

    模板里的 ``breath_after`` 只描述它自己那一段的句尾（`zichang-qingkuai` 是 [4]），
    而歌词更长时旋律要重复。旧实现直接拿模板的下标去比绝对音序，于是一首 20 秒的歌
    只在第 4 个字之后喘一口气，后面十几个音一口气唱完——这正是"念经感"的来源之一。
    """
    if not notes or not breath_after or syllable_count <= 0:
        return frozenset()
    length = len(notes)
    marks = {value for value in (int(item) for item in breath_after) if 0 < value <= length}
    if not marks:
        return frozenset()
    if syllable_count >= length:
        return frozenset(
            index + 1 for index in range(syllable_count) if ((index % length) + 1) in marks
        )
    # 歌词比旋律短：前面的音原样保留，最后一个音并掉了余下的时值，
    # 它被并掉的那几个音里的换气点也就跟到句尾。
    kept = {index + 1 for index in range(syllable_count - 1) if (index + 1) in marks}
    if any(value in marks for value in range(syllable_count, length + 1)):
        kept.add(syllable_count)
    return frozenset(kept)


def render_song(tts: TtsClient, notes: list[list[float]], lyrics: str, octave: int,
                tempo: float, breath_after: tuple[int, ...] = (),
                reverb: bool = True, transpose: float = 0.0,
                pool: object | None = None) -> tuple[bytes, int, int]:
    syllables = split_syllables(lyrics)
    if not syllables:
        raise SingRequestError("歌词里没有可唱的字")
    if len(syllables) > MAX_LYRICS_CHARS:
        raise SingRequestError(f"歌词过长（上限 {MAX_LYRICS_CHARS} 个字）")
    fitted = fit_notes(notes, len(syllables))
    if not fitted:
        raise SingRequestError("模板没有音符")
    breath_after = tuple(sorted(fit_breaths(notes, len(syllables), breath_after)))

    beat_seconds = 60.0 / max(30.0, min(200.0, tempo))
    rate = SAMPLE_RATE
    overlap = int(0.015 * rate)
    last_index = len(fitted) - 1

    # 先把每个音的活排出来。滑音起点取"计划里的上一个音"，不依赖渲染结果，
    # 所以这些音彼此独立、可以并行——线上那次 28 个音串行渲染要 10 秒，正好
    # 撞上群里两条附件消息把回复顶掉。
    jobs: list[tuple[str, str, float, float, float | None, int]] = []
    plan: list[tuple[int, int, float, float]] = []  # (index, degree, duration, freq)
    elapsed = 0.0
    previous: float | None = None
    for index, ((degree, beats), syllable) in enumerate(zip(fitted, syllables, strict=True)):
        degree = int(round(degree))
        duration = max(0.12, float(beats) * beat_seconds)
        if elapsed + duration > MAX_SECONDS:
            break
        freq = _transposed(_hz(degree, octave), transpose)
        # 每个音多合成 15 毫秒，专门留给与下一个音的交叠；总时值因此保持不变。
        held = duration + (overlap / rate if index != last_index else 0.0)
        # 种子带上音序号：同一个音级反复出现时，颤音与微抖不会一模一样
        seed = (index + 1) * 7919 + int(round(freq))
        jobs.append((getattr(tts, "url", ""), syllable, freq, held, previous, seed))
        plan.append((index, degree, duration, freq))
        previous = freq
        elapsed += duration
    if not jobs:
        raise SingRequestError("没有渲染出任何音符")

    rendered: list[tuple[np.ndarray, int]]
    if pool is not None and len(jobs) > 1:
        try:
            rendered = list(pool.map(_sing_note_job, jobs))
        except Exception as error:  # noqa: BLE001 - 并行失败就退回串行，不能整首失败
            LOG.warning("并行渲染失败，退回串行: %s", error)
            rendered = [sing_note(tts, syllable, freq, held, previous, seed)
                        for (_url, syllable, freq, held, previous, seed) in jobs]
    else:
        rendered = [sing_note(tts, syllable, freq, held, previous, seed)
                    for (_url, syllable, freq, held, previous, seed) in jobs]

    pieces: list[np.ndarray] = []
    for (index, degree, duration, freq), (piece, rate) in zip(plan, rendered, strict=True):
        if LOG.isEnabledFor(logging.DEBUG):
            # 只给调试用；这条分析本身要几十毫秒一个音，别放热路径上。
            LOG.debug(
                "音符 %d: 音级%d 目标 %.3fs 实际 %.3fs 基频 %.0fHz(实测 %.0fHz)",
                index + 1,
                degree,
                duration,
                len(piece) / rate,
                freq,
                _dominant_hz(piece, rate, 120, 1200),
            )
        pieces.append(piece)
        if (index + 1) in breath_after:
            # 句尾换气：没有呼吸的连续音墙是"念经/机械"感的来源之一。
            pieces.append(np.zeros(int(BREATH_SECONDS * rate)))

    # 逐音做一次有上限的响度对齐：逐字合成出来的响度本来就散（实测最轻的字比最响的
    # 低 18dB 以上），不压就会出现"某个字几乎听不见、某个字炸一下"。
    levels = [_sung_level(piece, rate) for piece in pieces]
    audible = [level for level in levels if level > 1e-5]
    if audible:
        target_level = float(np.median(audible))
        cap = 10 ** (LEVEL_ALIGN_DB / 20)
        for index, piece in enumerate(pieces):
            level = levels[index]
            if level <= 1e-5:
                continue
            gain = min(cap, max(1.0 / cap, target_level / level))
            pieces[index] = piece * gain

    # 音与音之间用 15 毫秒交叠连起来（legato）：逐音淡到零再淡起来会变成一顿一顿的
    # 念白感，硬拼又会咔哒——交叠是这两者之间唯一像"唱"的接法。
    audio = pieces[0]
    piece_index = 0
    for piece in pieces[1:]:
        piece_index += 1
        if len(piece) > 0:
            piece = _add_shimmer(piece, rate, seed=1000 + piece_index)
        audio = _crossfade_join(audio, piece, overlap)
    audio = _voice_eq(audio, rate)
    if reverb:
        audio = _room_reverb(audio, rate)
    fade = min(int(0.01 * rate), len(audio) // 2)
    if fade > 0:
        ramp = np.linspace(0.0, 1.0, fade)
        audio[:fade] *= ramp
        audio[-fade:] *= ramp[::-1]
    # 收敛一点整体电平：之前峰值 0.92 配高八度音区，等于贴耳喊，很刺。
    peak = float(np.max(np.abs(audio))) or 1.0
    pcm = (audio / peak * 0.80 * 32767.0).astype("<i2")
    buffer = BytesIO()
    with wave.open(buffer, "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(pcm.tobytes())
    # 返回唱出的音符数，不是段落数：pieces 里还夹着换气，拿它当"唱了几个字"会虚高。
    return buffer.getvalue(), rate, len(plan)


class Templates:
    def __init__(self, path: Path) -> None:
        self.path = path
        self._lock = threading.Lock()
        self._mtime = 0.0
        self._data: dict[str, dict] = {}
        self.reload()

    def reload(self) -> None:
        stat = self.path.stat()
        with self._lock:
            if stat.st_mtime == self._mtime and self._data:
                return
            raw = json.loads(self.path.read_text(encoding="utf-8"))
            self._data = {item["id"]: item for item in raw["templates"]}
            self._mtime = stat.st_mtime
        LOG.info("载入 %d 个旋律模板", len(self._data))

    def get(self, template_id: str) -> dict:
        self.reload()
        with self._lock:
            template = self._data.get(template_id)
        if template is None:
            raise SingRequestError(f"没有这个模板: {template_id}")
        return template

    def summary(self) -> list[dict]:
        self.reload()
        with self._lock:
            return [
                {
                    "id": item["id"],
                    "name": item["name"],
                    "mood": item.get("mood", ""),
                    "syllables": len(item["notes"]),
                    "bpm": item.get("bpm", 100),
                }
                for item in self._data.values()
            ]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    tts: TtsClient
    templates: Templates
    # 默认不加混响：试听后选定的音色是"干声"（见 docs/qq-singing.md）。
    default_reverb: bool = False
    # 逐音渲染的进程池；None 表示串行（起不来也不影响功能）。
    pool: object | None = None

    def _json(self, payload: dict, status: int = 200) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/healthz":
            self._json(
                {
                    "ok": True,
                    "tts_ok": self.tts.healthy(),
                    "templates": len(self.templates.summary()),
                }
            )
            return
        if self.path == "/v1/templates":
            self._json({"templates": self.templates.summary()})
            return
        self._json({"error": "not found"}, status=404)

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/v1/sing":
            self._json({"error": "not found"}, status=404)
            return
        # 先夹长度再读：`int()` 本身可能抛（Content-Length: abc），而
        # 没有上限时 `Content-Length: 2000000000` 会直接把内存吃光。
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except (TypeError, ValueError):
            self._json({"error": "Content-Length 非法"}, status=400)
            return
        if length <= 0 or length > MAX_BODY_BYTES:
            self._json({"error": "请求体为空或过大"}, status=400)
            return
        try:
            request = json.loads(self.rfile.read(length).decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            self._json({"error": "请求体不是合法 JSON"}, status=400)
            return
        try:
            lyrics = str(request.get("lyrics", ""))
            breath_after: tuple[int, ...] = ()
            reverb = bool(request.get("reverb", self.default_reverb))
            transpose = float(request.get("transpose", -5.0))
            if request.get("notes"):
                notes = [[float(n[0]), float(n[1])] for n in request["notes"]]
                octave = int(request.get("octave", 5))
                tempo = float(request.get("tempo", 100))
                template_id = "inline"
            else:
                template = self.templates.get(str(request.get("template", "")))
                notes = [[float(n[0]), float(n[1])] for n in template["notes"]]
                octave = int(request.get("octave", template.get("octave", 4)))
                tempo = float(request.get("tempo", template.get("bpm", 100)))
                template_id = template["id"]
                breath_after = tuple(int(value) for value in template.get("breath_after", ()))
                transpose = float(request.get("transpose", template.get("transpose", -5.0)))
            wav, rate, sung = render_song(
                self.tts, notes, lyrics, octave, tempo, breath_after, reverb, transpose,
                self.pool,
            )
        except SingRequestError as error:
            LOG.warning("拒绝合成: %s", error)
            self._json({"error": str(error)}, status=400)
            return
        except Exception as error:  # noqa: BLE001 - 任何内部错误都回 502
            LOG.exception("合成失败")
            self._json({"error": f"合成失败: {error}"}, status=502)
            return
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("X-Sample-Rate", str(rate))
        self.send_header("X-Template", template_id)
        self.send_header("X-Syllables", str(sung))
        self.send_header("Content-Length", str(len(wav)))
        self.end_headers()
        self.wfile.write(wav)

    def log_message(self, fmt: str, *args) -> None:
        LOG.info("%s - %s", self.address_string(), fmt % args)


def main() -> None:
    host = "127.0.0.1"
    port = 6121
    tts_url = "http://127.0.0.1:6120/v1/tts"
    templates_path = Path(__file__).with_name("templates.json")
    args = sys.argv[1:]
    reverb_enabled = False
    while args:
        key = args.pop(0)
        value = args.pop(0) if args else ""
        if key == "--reverb":
            reverb_enabled = True
            if value and not value.startswith("--"):
                args.insert(0, value)
            continue
        if key == "--host":
            host = value
        elif key == "--port":
            port = int(value)
        elif key == "--tts":
            tts_url = value
        elif key == "--templates":
            templates_path = Path(value)
        else:
            raise SystemExit(f"未知参数: {key}")

    logging.basicConfig(
        level=os.environ.get("SING_LOG_LEVEL", "INFO"),
        format="[%(asctime)s] %(levelname)s %(message)s",
    )
    Handler.tts = TtsClient(tts_url)
    Handler.templates = Templates(templates_path)
    Handler.default_reverb = reverb_enabled
    # 逐音渲染用进程池（Praat 带全局状态，线程不安全）。留一个核给 TTS 与其他服务。
    try:
        from concurrent.futures import ProcessPoolExecutor

        Handler.pool = ProcessPoolExecutor(max_workers=max(2, min(3, (os.cpu_count() or 3) - 1)))
    except Exception as error:  # noqa: BLE001 - 起不来就串行，不影响正确性
        LOG.warning("并行渲染进程池创建失败，改串行: %s", error)
        Handler.pool = None
    server = ThreadingHTTPServer((host, port), Handler)
    LOG.info("芸汐歌声合成服务已启动: http://%s:%d/ （模板 %s，TTS %s）",
             host, port, templates_path.name, tts_url)
    server.serve_forever()


if __name__ == "__main__":
    main()
