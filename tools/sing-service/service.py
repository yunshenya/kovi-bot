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


def _pitch_tier(manipulation, duration: float, freq: float, previous: float | None):
    """把整段的基频换成 freq（带颤音、jitter 与来自上一个音的短滑音）。"""
    tier = call(manipulation, "Create PitchTier", "empty", 0.0, duration)
    step = 0.02
    rng = np.random.default_rng(int(freq) % 9973)
    # 一个预先抽好的随机游走序列：每个点取一段，够整首歌用
    jitter_values = np.cumsum(rng.normal(0.0, 0.35, 4096))
    jitter_values = jitter_values - jitter_values.mean()
    jitter_values = np.clip(jitter_values, -3.0, 3.0)
    vibrato_hz = float(rng.uniform(4.6, 5.6))
    for index, time in enumerate(np.arange(0.0, duration + step, step)):
        at = min(float(time), duration)
        if index == 0 and previous is not None:
            glide = min(1.0, step / 0.04)
            value = previous + (freq - previous) * glide
        else:
            # 颤音轻、延迟起振（先直后颤才是人唱的），再叠一点点随机游走当 jitter：
            # 完全规则的周期会让音色听起来像合成器。
            depth = 0.007 * min(1.0, max(0.0, (at - 0.25 * duration) / max(0.12, 0.3 * duration)))
            jitter = 1.0 + 0.0035 * jitter_values[index % len(jitter_values)]
            value = freq * jitter * (1.0 + depth * np.sin(2 * np.pi * vibrato_hz * at))
        call(tier, "Add point", at, value)
    call([manipulation, tier], "Replace pitch tier")


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


def _peak(samples: np.ndarray) -> float:
    return float(np.max(np.abs(samples))) if samples.size else 0.0


def _longest_silence(samples: np.ndarray, rate: int) -> tuple[int, int] | None:
    """找最长的低能量段，返回 (起点样本, 长度样本)。"""
    hop = max(16, int(0.01 * rate))
    frames = [samples[i : i + hop] for i in range(0, len(samples) - hop + 1, hop)]
    if len(frames) < 3:
        return None
    levels = np.array([float(np.sqrt(np.mean(frame**2))) for frame in frames])
    threshold = max(1e-4, float(levels.max()) * 0.08)
    best: tuple[int, int] | None = None
    start: int | None = None
    for index, level in enumerate(levels):
        if level <= threshold:
            start = index if start is None else start
            continue
        if start is not None:
            length = index - start
            if best is None or length > best[1]:
                best = (start * hop, length * hop)
            start = None
    if start is not None:
        length = len(levels) - start
        if best is None or length > best[1]:
            best = (start * hop, length * hop)
    if best is None or best[1] < int(0.03 * rate):
        return None
    return best


def synthesize_syllable(tts: "TtsClient", syllable: str, speed: float) -> tuple[np.ndarray, int]:
    """合成一个字的音频；单字读不出来时用"字，啊"载体切出第一个字。

    实测 vits-zh-ll 对个别孤立汉字（"是""星"）会输出近乎静音（峰值 0.002），
    而同样的字放进词组就正常。载体后面跟逗号会形成一段真实静音，正好当切点。
    """
    audio, rate = tts.synthesize(syllable, speed)
    if _peak(audio) >= QUIET_PEAK:
        return audio, rate
    carried, rate = tts.synthesize(f"{syllable}，啊", speed)
    gap = _longest_silence(carried, rate)
    if gap is not None and gap[0] >= int(0.02 * rate):
        head = carried[: gap[0]]
        if _peak(head) >= QUIET_PEAK:
            LOG.debug("单字 %s 合成近乎无声，用载体切出前 %.3fs", syllable, gap[0] / rate)
            return head, rate
    if _peak(carried) > _peak(audio):
        LOG.debug("单字 %s 合成近乎无声，退回整段载体", syllable)
        return carried, rate
    return audio, rate


def _voice_eq(samples: np.ndarray, rate: int) -> np.ndarray:
    """把合成的音色往"人声"掰一点。

    实测对比她本人说话：唱歌的 0–300Hz（胸腔）只有 20%（说话 35%），1–4kHz（硬度）
    却有 31%（说话 20%）——又薄又尖正是"听着怪"的主要来源。这里三段一起修：
    200Hz 低架 +3dB 补胸腔、3kHz 附近 -3.5dB 去硬度、6.5kHz 以上 -4dB 收毛刺。
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


def _voiced_region(samples: np.ndarray, rate: int) -> tuple[int, int] | None:
    """用"低过零率 + 够能量"找有声区间。

    不用 Praat 的基频分析：单字只有几十毫秒，分析窗比字还长，取不到有效帧。
    擦音（sh/x/s）的过零率明显高于元音，这条判据在短音上也成立。
    """
    hop = max(16, int(0.008 * rate))
    if len(samples) < 3 * hop:
        return None
    frame_rms: list[float] = []
    frame_zcr: list[float] = []
    for start in range(0, len(samples) - hop + 1, hop):
        frame = samples[start : start + hop]
        frame_rms.append(float(np.sqrt(np.mean(frame**2))))
        frame_zcr.append(float(np.mean(np.abs(np.diff(np.sign(frame)))) / 2))
    if not frame_rms:
        return None
    peak = max(frame_rms)
    if peak <= 0:
        return None
    voiced = [
        index
        for index, (rms, zcr) in enumerate(zip(frame_rms, frame_zcr))
        if rms > 0.25 * peak and zcr < 0.12
    ]
    if not voiced:
        return None
    start = max(0, voiced[0] * hop - hop)
    end = min(len(samples), (voiced[-1] + 2) * hop)
    if end - start < int(0.02 * rate):
        return None
    return start, end


def _safe_floor(duration: float, minimum: float = 75.0) -> float:
    """Praat 的分析窗（3/下限）必须装得进音频：短音上给 75Hz 会直接报
    ``minimum pitch must not be less than``。实测门槛正好是 9/时长。"""
    return max(minimum, min(400.0, 9.0 / max(0.01, duration)))


def _psola_pitch(samples: np.ndarray, rate: int, freq: float,
                 previous: float | None) -> np.ndarray:
    sound = parselmouth.Sound(samples, sampling_frequency=rate)
    manipulation = call(sound, "To Manipulation", 0.01, _safe_floor(sound.duration), 1400)
    _pitch_tier(manipulation, sound.duration, freq, previous)
    return call(manipulation, "Get resynthesis (overlap-add)").values[0]


def _pitch_shift(samples: np.ndarray, rate: int, freq: float,
                 previous: float | None) -> np.ndarray:
    """只给有声段换音高，清辅音（字头）原样保留。

    Praat 的重合成是由基频脉冲驱动的：整段送进去时，"是""星"这种几乎全是清辅音
    的字会被合成成近乎无声（实测 RMS 0.09 → 0.001），听感上就是那个音消失了。
    辅音本来也没有音高可改，原样接回去既保住了字头，也避开了这个坑。
    """
    region = _voiced_region(samples, rate)
    if region is None:
        return samples
    start, end = region
    head, voiced, tail = samples[:start], samples[start:end], samples[end:]
    if len(voiced) < int(0.02 * rate):
        return samples
    shifted = _psola_pitch(voiced, rate, freq, previous)
    # 清辅音是原速原调的，元音是变调后的，硬接会在每个字上留一个咔哒（实测 99.99
    # 分位差分是正常说话的近两倍，听感就是一路"啪啪"声）。3 毫秒交叠就够。
    edge = max(1, int(0.003 * rate))
    return _crossfade_join(_crossfade_join(head, shifted, edge), tail, edge)


def _fit_duration(
    samples: np.ndarray, rate: int, target: float, expected_hz: float
) -> np.ndarray:
    """把已经定好音高的音频调到 target 秒。

    短了就循环元音核（"人力VOCALOID"处理长音的标准做法——单字只有 0.1 秒，而一个
    音符常常要 0.6~1.2 秒，硬拉会变成走调的嗡嗡声）；长了或只差零头就交给 Praat
    的 Lengthen (PSOLA)。
    """
    actual = len(samples) / rate
    if actual <= 0:
        return np.zeros(int(target * rate))
    if target / actual < 1.15:
        return _psola_scale(samples, rate, target)
    looped = _loop_nucleus(samples, rate, target, expected_hz)
    if looped is not None:
        return _psola_scale(looped, rate, target)
    # 连元音核都找不到（整段清辅音）：只能硬拉。
    return _psola_scale(samples, rate, target)


def _psola_scale(samples: np.ndarray, rate: int, target: float) -> np.ndarray:
    """Praat 的 Lengthen (PSOLA)：保留音高改时长。

    参数顺序是 ``(音高下限, 音高上限, 时长倍数)``——写反了会得到
    ``minimum pitch must not be less than`` 这种看不懂的报错。
    """
    actual = len(samples) / rate
    if actual <= 0 or 0.96 <= target / actual <= 1.04:
        return samples
    try:
        sound = parselmouth.Sound(samples, sampling_frequency=rate)
        stretched = call(
            sound,
            "Lengthen (PSOLA)",
            _safe_floor(sound.duration, 120.0),
            1400,
            target / actual,
        )
        return stretched.values[0]
    except Exception as error:  # noqa: BLE001 - 补时长失败就用原样，不值得整轮失败
        LOG.warning("Lengthen (PSOLA) 失败，保留原时长: %s", error)
        return samples


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

    循环单元按整周期切，接缝的相位就是连续的，所以只需要 1.5 毫秒淡化防咔哒。
    实测不按周期切时，587Hz 的"星"会掉到 133Hz（接缝把相位错开了）。
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
              previous: float | None) -> tuple[np.ndarray, int]:
    """把一个字唱成 freq 这个音、时值 duration 秒。

    时值靠**合成时的语速**（VITS 的 length_scale）拿到：先按正常语速量出这个字
    自然有多长，再按需要把它合成得慢一些。只在最后补一点 PSOLA 的零头，避免
    "把 0.19 秒的字硬拉成 0.6 秒"那种发飘的听感。
    """
    natural, rate = synthesize_syllable(tts, syllable, 1.0)
    natural = trim_silence(natural, rate)
    natural_seconds = len(natural) / rate
    if natural_seconds <= 0:
        return np.zeros(int(duration * rate)), rate
    # 语速只用来"把字拉长一点"，不靠它凑时值：实测 VITS 在 0.45 以下就不再变慢
    # （speed=0.2 甚至比 speed=1.0 还短），剩下的交给 _fit_duration 循环稳态段。
    speed = max(0.45, min(1.0, natural_seconds / max(0.08, duration)))
    if abs(speed - 1.0) < 0.05:
        audio, rate = natural, rate
    else:
        audio, rate = synthesize_syllable(tts, syllable, speed)
        audio = trim_silence(audio, rate)
    if len(audio) < int(0.03 * rate):
        return np.zeros(int(duration * rate)), rate
    sung = _pitch_shift(audio, rate, freq, previous)
    return _fit_duration(sung, rate, duration, freq), rate


_WORKER_TTS: TtsClient | None = None


def _sing_note_job(job: tuple[str, str, float, float, float | None]) -> tuple[np.ndarray, int]:
    """进程池里的单音渲染。

    用进程而不是线程：parselmouth 底下是 Praat，带全局状态，多线程并发调用不保证
    安全。fork 出来的子进程各自持有自己的 TTS 客户端与缓存。
    """
    global _WORKER_TTS
    tts_url, syllable, freq, duration, previous = job
    if _WORKER_TTS is None:
        _WORKER_TTS = TtsClient(tts_url)
    return sing_note(_WORKER_TTS, syllable, freq, duration, previous)


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

    beat_seconds = 60.0 / max(30.0, min(200.0, tempo))
    rate = SAMPLE_RATE
    overlap = int(0.015 * rate)
    last_index = len(fitted) - 1

    # 先把每个音的活排出来。滑音起点取"计划里的上一个音"，不依赖渲染结果，
    # 所以这些音彼此独立、可以并行——线上那次 28 个音串行渲染要 10 秒，正好
    # 撞上群里两条附件消息把回复顶掉。
    jobs: list[tuple[str, str, float, float, float | None]] = []
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
        jobs.append((getattr(tts, "url", ""), syllable, freq, held, previous))
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
            rendered = [sing_note(tts, syllable, freq, held, previous)
                        for (_url, syllable, freq, held, previous) in jobs]
    else:
        rendered = [sing_note(tts, syllable, freq, held, previous)
                    for (_url, syllable, freq, held, previous) in jobs]

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

    # 逐音做一次有上限的响度对齐：清辅音字天然比元音响得多/轻得多，不压一下会
    # 出现某个字几乎听不见。只在 ±2.5dB 内调整，避免把整首歌压成一条没有起伏的线。
    levels = [float(np.sqrt(np.mean(piece**2))) if len(piece) else 0.0 for piece in pieces]
    audible = [level for level in levels if level > 1e-4]
    if audible:
        target_level = float(np.median(audible))
        for index, piece in enumerate(pieces):
            level = levels[index]
            if level <= 1e-4:
                continue
            gain = min(1.33, max(0.75, target_level / level))
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
    return buffer.getvalue(), rate, len(pieces)


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
