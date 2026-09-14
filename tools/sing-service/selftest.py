#!/usr/bin/env python3
"""歌声服务的离线自检：不需要 TTS、不需要网络，验证"歌词与旋律长度不匹配"这类
边界情况不会再把整个请求打成 502。

线上踩过的坑：模型写了完整的四句《小星星》歌词（28 字），而模板只有两句旋律
（14 个音），旧实现用 ``zip(..., strict=True)`` 去配，长度不等直接抛异常 → 502 →
机器人回退成念白，用户听到的就是"跟读的一样"。

    python selftest.py
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).parent
spec = importlib.util.spec_from_file_location("sing_service", HERE / "service.py")
assert spec and spec.loader
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)

XIAOXINGXING = [
    [1, 1], [1, 1], [5, 1], [5, 1], [6, 1], [6, 1], [5, 2],
    [4, 1], [4, 1], [3, 1], [3, 1], [2, 1], [2, 1], [1, 2],
]
FULL_LYRICS = "一闪一闪亮晶晶满天都是小星星挂在天上放光明好像许多小眼睛"  # 28 字
BREATH_AFTER = (4,)  # 模板只标自己那一段的句尾，铺开后应当重复


class StubTts:
    """回声替代：给什么字都回一段 300Hz 的 0.2 秒音频，等价于"这个字读得出来"。"""

    def __init__(self, rate: int = 16_000) -> None:
        self.rate = rate
        self.calls: list[tuple[str, float]] = []

    def synthesize(self, text: str, speed: float = 1.0):
        self.calls.append((text, speed))
        length = int(self.rate * 0.2)
        time = np.arange(length) / self.rate
        tone = 0.3 * np.sin(2 * np.pi * 300 * time)
        return tone, self.rate

    def healthy(self) -> bool:
        return True


def check_fit_notes() -> None:
    for count in (1, 7, 13, 14, 15, 20, 28, 29):
        fitted = service.fit_notes(XIAOXINGXING, count)
        assert len(fitted) == count, f"{count} 字应得到 {count} 个音符，实际 {len(fitted)}"
    # 歌词更长时旋律要重复：第 15 个音应当回到旋律的第一个音。
    tiled = service.fit_notes(XIAOXINGXING, 15)
    assert tiled[14][0] == XIAOXINGXING[0][0], "超出的歌词应当从头重复旋律"
    # 歌词更短时，剩下的时值并进最后一个字：总拍数守恒。
    short = service.fit_notes(XIAOXINGXING, 3)
    assert sum(beats for _, beats in short) == sum(beats for _, beats in XIAOXINGXING)
    print(f"  fit_notes: 1/7/13/14/15/20/28/29 字都得到等长音符，重复与并句行为正确")


def check_fit_breaths() -> None:
    """换气点必须跟着旋律一起重复。

    模板只写自己那一段的句尾（自创·轻快是"第 4 个音之后"），歌词更长时旋律会重复，
    换气点也得重复；旧实现拿模板下标去比绝对音序，20 秒的歌只喘一口气。
    """
    melody = [[1, 1], [2, 1], [3, 1], [5, 1], [3, 1], [2, 1], [1, 1], [1, 2]]
    assert service.fit_breaths(melody, 8, (4,)) == frozenset({4})
    assert service.fit_breaths(melody, 24, (4,)) == frozenset({4, 12, 20})
    # 歌词比旋律短：最后一个音并掉了余下的时值，被并掉段落里的换气点跟到句尾
    assert service.fit_breaths(melody, 3, (4,)) == frozenset({3})
    assert service.fit_breaths(melody, 5, (4,)) == frozenset({4})
    assert service.fit_breaths(melody, 8, ()) == frozenset()
    print("  fit_breaths: 换气点跟着旋律重复（同一段旋律铺到 24 个音时为 4/12/20）")


def check_render_with_stub(lyrics: str) -> tuple[float, float, int]:
    """用假 TTS 走完整的 render_song（不碰网络）。"""
    tts = StubTts()
    notes = XIAOXINGXING
    syllables = len(service.split_syllables(lyrics))
    fitted = service.fit_notes(notes, syllables)
    breaths = service.fit_breaths(notes, syllables, BREATH_AFTER)
    spans = [max(0.12, beats * (60.0 / 104)) for _degree, beats in fitted]
    # 交叠拼接每接一次吃掉一个交叠时长，所以一次换气净贡献 0.22-0.015 秒
    expected = sum(spans) + (service.BREATH_SECONDS - 0.015) * len(breaths)
    wav, rate, sung = service.render_song(tts, notes, lyrics, 5, 104, BREATH_AFTER)
    duration = (len(wav) - 44) / 2 / rate
    return expected, duration, sung


def main() -> int:
    print("歌声服务自检")
    check_fit_notes()
    check_fit_breaths()
    cases = [
        ("一闪一闪亮晶晶满天都是小星星", "正好两句（14 字，与模板等长）"),
        (FULL_LYRICS, "完整四句（28 字，旋律要重复一遍）"),
        ("一闪一闪", "只有一句（4 字，剩余时值并进最后一个字）"),
        ("一闪一闪亮晶晶满天都是小星星挂在天上放光明好像许多小眼睛再唱一句吧", "超长（31 字）"),
    ]
    for lyrics, label in cases:
        expected, duration, sung = check_render_with_stub(lyrics)
        assert sung == len(service.split_syllables(lyrics)), "每个字都该唱出来"
        drift = abs(duration - expected) / expected
        assert drift < 0.02, f"{label}: 时长偏差过大 {duration:.2f}s vs {expected:.2f}s"
        print(f"  {label}: 唱出 {sung} 个字，时长 {duration:.2f}s（计划 {expected:.2f}s，偏差 {drift:.1%}）")
    print("自检通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
