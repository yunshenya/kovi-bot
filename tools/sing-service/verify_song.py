#!/usr/bin/env python3
"""按模板渲染一首，并逐音核对音高/时值是否落在计划上。

这是歌声服务的验收工具：改完变调或时值逻辑后跑一次，能立刻看出哪几个音跑调、
哪几个音长度不对，而不必靠耳朵逐个听。

    python verify_song.py --template xiaoxingxing --lyrics 一闪一闪亮晶晶满天都是小星星
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.request

import numpy as np
import parselmouth
from parselmouth.praat import call

SEMITONES = {1: 0, 2: 2, 3: 4, 4: 5, 5: 7, 6: 9, 7: 11}


def note_hz(degree: int, octave: int) -> float:
    if degree > 7:
        degree -= 7
        octave += 1
    return 440.0 * 2 ** ((SEMITONES[degree] + (octave - 4) * 12 - 9) / 12)


def http_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.loads(response.read().decode("utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:6121")
    parser.add_argument("--template", required=True)
    parser.add_argument("--lyrics", required=True)
    parser.add_argument("--output", default="/tmp/verify-song.wav")
    args = parser.parse_args()

    templates = {item["id"]: item for item in http_json(f"{args.url}/v1/templates")["templates"]}
    if args.template not in templates:
        print(f"没有这个模板: {args.template}（可选: {', '.join(templates)}）", file=sys.stderr)
        return 2
    template = templates[args.template]

    body = json.dumps({"template": args.template, "lyrics": args.lyrics}).encode("utf-8")
    request = urllib.request.Request(
        f"{args.url}/v1/sing", data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        wav = response.read()
    with open(args.output, "wb") as handle:
        handle.write(wav)

    # 取回模板的完整音符表（/v1/templates 只给摘要，这里直接读文件更省事）
    with open("templates.json", encoding="utf-8") as handle:
        full = {item["id"]: item for item in json.load(handle)["templates"]}[args.template]
    notes = [(int(degree), float(beats)) for degree, beats in full["notes"]]
    octave = int(full.get("octave", 5))
    beat = 60.0 / float(full.get("bpm", 100))

    syllables = [char for char in args.lyrics if "\u4e00" <= char <= "\u9fff"]
    if len(syllables) < len(notes):
        notes = notes[: len(syllables) - 1] + [
            (notes[len(syllables) - 1][0],
             sum(item[1] for item in notes[len(syllables) - 1 :]))
        ]
    else:
        notes = notes[: len(syllables)]

    sound = parselmouth.Sound(args.output)
    pitch = call(sound, "To Pitch", 0.01, 75, 1400)
    print(f"{args.template}: 渲染 {sound.duration:.2f}s，{len(notes)} 个音（{args.output}）")
    hits = 0
    elapsed = 0.0
    for index, ((degree, beats), syllable) in enumerate(zip(notes, syllables, strict=True)):
        span = beats * beat
        target = note_hz(degree, octave)
        at = min(elapsed + span * 0.5, max(0.02, sound.duration - 0.02))
        measured = call(pitch, "Get value at time", float(at), "Hertz", "linear")
        ratio = measured / target if measured == measured and measured > 0 else 0.0
        if 0.92 <= ratio <= 1.08:
            mark = "✓"
            hits += 1
        elif 0.46 <= ratio <= 0.54:
            mark = "低八度"
        elif 1.9 <= ratio <= 2.1:
            mark = "高八度"
        else:
            mark = "✗"
        print(
            f"  {index + 1:2d}. {syllable} 音级{degree} 目标{target:7.1f}Hz "
            f"实测{measured:7.1f}Hz [{span:.2f}s @{elapsed:5.2f}s] {mark}"
        )
        elapsed += span
    print(f"命中 {hits}/{len(notes)}；计划总时长 {elapsed:.2f}s / 实际 {sound.duration:.2f}s")
    return 0 if hits == len(notes) else 1


if __name__ == "__main__":
    raise SystemExit(main())
