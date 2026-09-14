#!/usr/bin/env python3
"""按模板渲染一首，逐音核对音高、时值与响度是否落在计划上。

歌声服务的验收工具：改完音高轮廓、时长或响度对齐之后跑一次，能立刻看出哪几个音
跑调、哪个音没出声、哪个音被削短，而不必靠耳朵逐个听。

    python verify_song.py --template zichang-qingkuai --lyrics 月亮挂在窗台上

判据（逐个音报告，最后汇总）：
  * 音高：音符中段的基频中位数与目标频率之差，单位音分。**±25 音分以内才算唱准**
    ——旧版用 ±8%（约 ±135 音分）当"命中"，那个容差正好掩盖了后来发现的整体走音。
  * 时值：这个音实际发声的时长 vs 计划时值。
  * 响度：有声段 RMS 相对全曲中位数的偏差（dB）。

计划直接复用 service.py 里的 `split_syllables` / `fit_notes`，不再自己抄一份：
抄一份的下场是两边逻辑一漂移，验收就变成自欺。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request

import numpy as np
import parselmouth
from parselmouth.praat import call

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from service import (  # noqa: E402
    BREATH_SECONDS,
    LEVEL_ALIGN_DB,
    fit_breaths,
    fit_notes,
    split_syllables,
)

CENTS_TOLERANCE = 25.0
OVERLAP_SECONDS = 0.015
LEVEL_SPREAD_DB = LEVEL_ALIGN_DB  # 与服务端的对齐上限同一个口径


def note_hz(degree: int, octave: int, transpose: float) -> float:
    semitones = {1: 0, 2: 2, 3: 4, 4: 5, 5: 7, 6: 9, 7: 11}
    if degree > 7:
        degree -= 7
        octave += 1
    base = 440.0 * 2 ** ((semitones[degree] + (octave - 4) * 12 - 9) / 12)
    return base * (2 ** (transpose / 12))


def http_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.loads(response.read().decode("utf-8"))


def note_windows(spans: list[float], breath_after: tuple[int, ...]) -> list[tuple[float, float]]:
    """复刻 render_song 的拼装顺序，算出每个音在成品里的 [起点, 终点]。

    每次交叠拼接都会让总长少掉一个交叠时长，所以位置不能简单累加时值——旧版就是
    这么算的，二十几个音下来偏移能攒到半秒，逐音核对自然对不上。
    """
    pieces: list[tuple[bool, float]] = []  # (是不是音符, 这一段多长)
    last = len(spans) - 1
    for index, span in enumerate(spans):
        pieces.append((True, max(0.12, span) + (OVERLAP_SECONDS if index != last else 0.0)))
        if index + 1 in breath_after:
            pieces.append((False, BREATH_SECONDS))
    windows: list[tuple[float, float]] = []
    cursor = 0.0
    for position, (is_note, seconds) in enumerate(pieces):
        if position > 0:
            cursor -= OVERLAP_SECONDS
        if is_note:
            windows.append((cursor, cursor + seconds))
        cursor += seconds
    return windows


def measure(sound: parselmouth.Sound) -> tuple[float, float, float]:
    """返回 (基频中位数, 有声时长, 有声段 RMS)。"""
    samples = sound.values[0]
    pitch = call(sound, "To Pitch", 0.01, 75, 1400)
    times = np.arange(0.0, sound.duration, 0.01)
    values = np.array(
        [call(pitch, "Get value at time", float(t), "Hertz", "linear") for t in times]
    )
    voiced = ~np.isnan(values)
    if not voiced.any():
        return 0.0, 0.0, 0.0
    first = int(np.argmax(voiced))
    last = int(len(voiced) - np.argmax(voiced[::-1]))
    voiced_seconds = (last - first) * 0.01
    region = samples[int(first * 0.01 * sound.sampling_frequency) :
                     int(last * 0.01 * sound.sampling_frequency)]
    level = float(np.sqrt(np.mean(region**2))) if region.size else 0.0
    return float(np.median(values[voiced])), voiced_seconds, level


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:6121")
    parser.add_argument("--template", required=True)
    parser.add_argument("--lyrics", required=True)
    parser.add_argument("--output", default="/tmp/verify-song.wav")
    args = parser.parse_args()

    available = {item["id"] for item in http_json(f"{args.url}/v1/templates")["templates"]}
    if args.template not in available:
        print(f"没有这个模板: {args.template}（可选: {', '.join(sorted(available))}）",
              file=sys.stderr)
        return 2
    here = os.path.dirname(os.path.abspath(__file__))
    with open(os.path.join(here, "templates.json"), encoding="utf-8") as handle:
        template = {item["id"]: item for item in json.load(handle)["templates"]}[args.template]

    body = json.dumps({"template": args.template, "lyrics": args.lyrics}).encode("utf-8")
    request = urllib.request.Request(
        f"{args.url}/v1/sing", data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(request, timeout=180) as response:
        wav = response.read()
    with open(args.output, "wb") as handle:
        handle.write(wav)

    octave = int(template.get("octave", 4))
    transpose = float(template.get("transpose", 0.0))
    beat_seconds = 60.0 / float(template.get("bpm", 100))
    template_notes = [[float(d), float(b)] for d, b in template["notes"]]
    syllables = split_syllables(args.lyrics)
    fitted = fit_notes(template_notes, len(syllables))
    if not fitted:
        print("模板没有音符", file=sys.stderr)
        return 2
    # 换气点跟旋律一起铺开：服务端就是这么算的，验收端必须用同一个口径，
    # 否则每漏一次换气，后面的窗口就往前漂 0.2 秒，逐音核对全是假的。
    breath_after = fit_breaths(template_notes, len(syllables),
                               tuple(int(v) for v in template.get("breath_after", ())))
    spans = [beats * beat_seconds for _degree, beats in fitted]
    windows = note_windows(spans, breath_after)

    sound = parselmouth.Sound(args.output)
    print(f"{args.template}: 渲染 {sound.duration:.2f}s / {len(fitted)} 个音（{args.output}）")
    print(f"{'#':>3} {'字':3} {'级':>3} {'目标Hz':>8} {'实测Hz':>8} {'偏差':>8} "
          f"{'计划s':>6} {'发声s':>6} {'响度dB':>7}")

    rows: list[tuple] = []
    for index, ((degree, _beats), syllable) in enumerate(zip(fitted, syllables, strict=True)):
        start, end = windows[index]
        clip = sound.extract_part(from_time=max(0.0, start),
                                  to_time=min(end, sound.duration), preserve_times=False)
        middle = clip.extract_part(from_time=clip.duration * 0.30,
                                   to_time=max(clip.duration * 0.31, clip.duration * 0.85),
                                   preserve_times=False)
        target = note_hz(int(round(degree)), octave, transpose)
        measured, voiced_seconds, level = measure(middle)
        cents = 1200 * np.log2(measured / target) if measured > 0 else float("nan")
        rows.append((index, syllable, int(round(degree)), target, measured, cents,
                     spans[index], voiced_seconds, level))

    levels = [row[8] for row in rows if row[8] > 1e-5]
    median_level = float(np.median(levels)) if levels else 0.0
    off_pitch: list[int] = []
    silent: list[int] = []
    uneven: list[int] = []
    for index, syllable, degree, target, measured, cents, planned, voiced, level in rows:
        level_db = (20 * np.log10(level / median_level)
                    if level > 1e-5 and median_level > 0 else float("nan"))
        mark = "✓"
        if measured <= 0:
            mark = "无声"
            silent.append(index + 1)
        elif abs(cents) > CENTS_TOLERANCE:
            mark = "跑调"
            off_pitch.append(index + 1)
        elif abs(level_db) > LEVEL_SPREAD_DB:
            mark = "响度"
            uneven.append(index + 1)
        print(f"{index + 1:3d} {syllable:3} {degree:3d} {target:8.1f} {measured:8.1f} "
              f"{cents:+8.0f} {planned:6.2f} {voiced:6.2f} {level_db:+7.1f} {mark}")

    spread = 20 * np.log10(max(levels) / min(levels)) if len(levels) > 1 else 0.0
    print(f"\n唱准 {len(fitted) - len(off_pitch) - len(silent)}/{len(fitted)}"
          f"（|偏差| ≤ {CENTS_TOLERANCE:.0f} 音分）；无声 {len(silent)} 个 {silent}；"
          f"跑调 {len(off_pitch)} 个 {off_pitch}；响度超差 {len(uneven)} 个 {uneven}")
    print(f"逐音响度极差 {spread:.1f}dB（对齐上限 {LEVEL_SPREAD_DB:.0f}dB）")
    return 0 if not off_pitch and not silent and not uneven else 1


if __name__ == "__main__":
    raise SystemExit(main())
