#!/usr/bin/env python3
"""私聊对话形状对照：旧回合契约 vs 新回合契约。

为什么需要它：生产里的"提问占比/多气泡占比"要等真实聊天才有数据，而这个
改动最容易失败的地方是**模型不照做**（不进 [[BUBBLE]]、永远不问问题）。
这里用固定的私聊场景直接把两种契约各跑 N 次，给出可复算的对照数字。

契约文本从 `plugins/model/src/yunxi/core_model.rs` 里现取，所以脚本不会
和线上指令漂移。

用法:
    export BOT_API_TOKEN=...            # 或 --token-file /home/ubuntu/kovi-bot/current/.env
    python3 scripts/chat_shape_ab.py --trials 3
    python3 scripts/chat_shape_ab.py --token-file /path/.env --json /tmp/ab.json
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import statistics
import sys
import urllib.request

REPO = pathlib.Path(__file__).resolve().parent.parent
CORE_MODEL = REPO / "plugins/model/src/yunxi/core_model.rs"
BUBBLE_MARKER = "[[BUBBLE]]"

# 旧契约原文（本次改动前 core_model.rs 的 CORE_PLAIN_TURN_INSTRUCTION）。
LEGACY_TURN_INSTRUCTION = (
    "Core 可见回复：只写一条自然、简短、有实际内容的聊天正文。宿主负责回复动作、"
    "气泡数量、发送顺序、并发覆盖和会话状态；不要输出 JSON、内部标记、动作协议、"
    "格式说明或思考过程，也不要把一个完整想法拆成多条。按问题需要可以保留 Markdown、"
    "换行或代码。用户明确要求多条消息时，宿主会逐条单独调用并发送，当前仍只需写这一条正文。"
    "语气始终温柔、真诚、有分寸：不讽刺、不挖苦、不阴阳怪气、不抬杠、不怼人、"
    "不冷嘲热讽，也不拿对方的短处或失败开玩笑。"
)
LEGACY_TONE_INSTRUCTION = (
    "Core 私聊语气：回复要像真实来回的聊天，语气温柔、有分寸，不讽刺、不挖苦、"
    "不阴阳怪气、不抬杠。若确实还有自然反应、补充、联想或想确认的点，可以在正文里体现，"
    "但不要为了显得主动而追加套话、机械追问或拆分一个完整想法。"
    "会话是否再次唤醒由宿主根据实际发送结果决定。"
)

SCENARIOS = [
    ("情绪承接", "今天被领导说了两句，有点烦。"),
    ("分享近况", "我刚把那个重构提交了。"),
    ("悬念邀请", "你猜我今天遇到谁了？"),
    ("低落", "我今天什么都做不好，感觉特别没用。"),
    ("日常闲聊", "今天中午吃了碗牛肉面，还挺香的。"),
    ("明确提问", "帮我看看这个报错大概是哪一类问题？"),
]


def read_rust_string(source: str, constant: str) -> str:
    """Extract a `const NAME: &str = "...";` literal from the Rust source."""
    match = re.search(
        rf'const {constant}: &str = "(?P<body>(?:[^"\\]|\\.)*)";', source, re.S
    )
    if not match:
        raise SystemExit(f"在 core_model.rs 里找不到常量 {constant}")
    return json.loads(f'"{match.group("body")}"')


def read_token(token_file: str | None) -> str:
    if token_file:
        for line in pathlib.Path(token_file).read_text().splitlines():
            line = line.strip()
            if line.startswith("BOT_API_TOKEN="):
                return line.split("=", 1)[1].strip().strip('"').strip("'")
        raise SystemExit(f"{token_file} 里没有 BOT_API_TOKEN")
    token = os.environ.get("BOT_API_TOKEN", "").strip()
    if not token:
        raise SystemExit("缺少 BOT_API_TOKEN：设置环境变量或用 --token-file")
    return token


def complete(url: str, model: str, token: str, messages: list[dict], max_tokens: int) -> str:
    body = json.dumps(
        {"model": model, "messages": messages, "max_tokens": max_tokens, "temperature": 0.9}
    ).encode()
    request = urllib.request.Request(
        url,
        data=body,
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=90) as response:
        payload = json.load(response)
    return payload["choices"][0]["message"]["content"]


def asks(text: str) -> bool:
    """Same definition as the host: an explicit question mark anywhere, then
    a Chinese question particle or an open ending on the last non-empty line."""
    if "?" in text or "？" in text:
        return True
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if not lines:
        return False
    last = lines[-1]
    return last.endswith(("吗", "呢", "吧", "，", ",", "、", "：", ":", "；", ";", "…", "—", "~"))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--trials", type=int, default=3, help="每个场景每种契约跑几次")
    parser.add_argument("--url", default="https://api.deepseek.com/chat/completions")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--max-tokens", type=int, default=1200)
    parser.add_argument("--token-file", default=None)
    parser.add_argument("--json", default=None, help="把逐条结果写成 JSON")
    args = parser.parse_args()

    source = CORE_MODEL.read_text()
    new_turn = read_rust_string(source, "CORE_PLAIN_TURN_INSTRUCTION")
    token = read_token(args.token_file)

    current_tone = (
        "Core 私聊语气：回复要像真实来回的聊天，语气温柔、有分寸，不讽刺、不挖苦、"
        "不阴阳怪气、不抬杠。若确实还有自然反应、补充、联想或想确认的点，可以在正文里体现，"
        "也可以补一个自己真心想知道的问题。会话是否再次唤醒由宿主根据实际发送结果决定。"
    )
    variants = {
        "legacy": [LEGACY_TURN_INSTRUCTION, LEGACY_TONE_INSTRUCTION],
        "current": [new_turn, current_tone],
    }

    records = []
    for name, instructions in variants.items():
        for label, probe in SCENARIOS:
            for trial in range(args.trials):
                messages = [{"role": "system", "content": text} for text in instructions]
                messages.append({"role": "user", "content": f"对方：{probe}"})
                try:
                    raw = complete(args.url, args.model, token, messages, args.max_tokens)
                except Exception as error:  # noqa: BLE001 - 报告失败而不是中断整轮
                    print(f"[{name}] {label} 第{trial + 1}次 请求失败: {error}", file=sys.stderr)
                    continue
                split = raw.count(BUBBLE_MARKER)
                records.append(
                    {
                        "variant": name,
                        "scenario": label,
                        "trial": trial + 1,
                        "raw": raw,
                        "bubbles": split + 1,
                        "asks": asks(raw),
                        "chars": len(re.sub(rf"\s*{re.escape(BUBBLE_MARKER)}\s*", "", raw).strip()),
                    }
                )

    def summarize(variant: str) -> dict:
        rows = [row for row in records if row["variant"] == variant]
        if not rows:
            return {}
        return {
            "n": len(rows),
            "asks_rate": sum(row["asks"] for row in rows) / len(rows),
            "multi_bubble_rate": sum(row["bubbles"] > 1 for row in rows) / len(rows),
            "mean_chars": statistics.mean(row["chars"] for row in rows),
            "mean_bubbles": statistics.mean(row["bubbles"] for row in rows),
        }

    print(f"模型 {args.model}   每场景 {args.trials} 次   场景 {len(SCENARIOS)} 个")
    print(f"{'契约':<10}{'样本':>5}{'提问率':>9}{'多气泡率':>10}{'均条数':>8}{'均字数':>8}")
    for variant in ("legacy", "current"):
        stats = summarize(variant)
        if not stats:
            continue
        print(
            f"{variant:<10}{stats['n']:>5}{stats['asks_rate']:>8.0%}{stats['multi_bubble_rate']:>10.0%}"
            f"{stats['mean_bubbles']:>8.2f}{stats['mean_chars']:>8.1f}"
        )

    print("\n逐条（current 契约）:")
    for row in records:
        if row["variant"] != "current":
            continue
        preview = row["raw"].replace("\n", " ⏎ ")[:88]
        print(f"  [{row['scenario']}] bubbles={row['bubbles']} asks={row['asks']} :: {preview}")

    if args.json:
        pathlib.Path(args.json).write_text(
            json.dumps({"summary": {v: summarize(v) for v in variants}, "records": records},
                       ensure_ascii=False, indent=2)
        )
        print(f"\n逐条结果已写入 {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
