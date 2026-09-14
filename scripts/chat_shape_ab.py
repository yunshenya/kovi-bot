#!/usr/bin/env python3
"""对话形状 A/B：把两份契约各跑一遍固定场景，给出可复算的对照数字。

为什么需要它：生产里的"议论占比/提问占比/多气泡占比"要等真实聊天才有数据，而这个
改动最容易失败的地方是**模型不照做**（不进 [[BUBBLE]]、永远不问问题、或者反过来说教）。
这里用固定场景直接把两份契约各跑 N 次。

契约文本从源码里现取，所以脚本不会和线上指令漂移；**而且按 `with_chat_style` 的拼法
把 `HUMAN_CHAT_STYLE` 一起拼上**——那才是生产真正下发的那份提示词。早期版本只取
`CORE_PLAIN_TURN_INSTRUCTION`，风格契约（口径、收尾、长度那些条款）根本没进对照，
测出来的差异不代表线上。

基线默认取 `HEAD`，也就是"工作区 vs 上一个提交"。改契约之前先跑一次基线，改完再跑
一次，两次的差值就是这次改动的效果。

用法:
    export BOT_API_TOKEN=...            # 或 --token-file /home/ubuntu/kovi-bot/current/.env
    python3 scripts/chat_shape_ab.py --trials 3
    python3 scripts/chat_shape_ab.py --baseline-git HEAD~1 --json /tmp/ab.json

指标口径（每一项都能从输出里复算）：
  * 议论率     : 命中格言/议论句式（"不是A是B""比X更…""越…越""怎么…都"…）的比例。
                 线上实测基线：群里真人 1%（6/430），她 10%（9/88）。
  * 落到人     : 正文里出现"我"或"你"。对方在说自己的事时，这一条是"有没有接住他"
                 的最直接标记——线上那次六连格言，六条里一个"我"一个"你"都没有。
  * 提问率     : 上一轮的回归线（改前 3% → 改后 25% 才补的第 7 条）。
  * 多气泡率   : 用了 [[BUBBLE]] 或两行短话的比例。
  * 真截断率   : `finish_reason=length` 的比例——这才是"被输出预算掐断"。
  * 无尾标点率 : 结尾没有句末标点/语气词的比例。**不是缺陷指标**：契约明确允许
                 "句尾可以不加句号"，"我在这儿""不想说也行啊"都算正常收尾。它只
                 影响宿主给不给自动续聊（`reply_looks_complete && reply_asks_something`），
                 所以单独列出来看趋势，不要当成错误率。
  * 均字数     : 长度预算（真人中位 8 字）。
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import statistics
import subprocess
import sys
import urllib.request

REPO = pathlib.Path(__file__).resolve().parent.parent
CORE_MODEL_REL = "plugins/model/src/yunxi/core_model.rs"
CHAT_STYLE_REL = "plugins/model/src/model/chat_style.rs"
BUBBLE_MARKER = "[[BUBBLE]]"

# 私聊回合的语气补充。它不在 core_model.rs 的常量里（那条链路自己拼），这里保持
# 与线上一致的写法即可。
PRIVATE_TONE = (
    "Core 私聊语气：回复要像真实来回的聊天，语气温柔、有分寸，不讽刺、不挖苦、"
    "不阴阳怪气、不抬杠。若确实还有自然反应、补充、联想或想确认的点，可以在正文里体现，"
    "也可以补一个自己真心想知道的问题。会话是否再次唤醒由宿主根据实际发送结果决定。"
)

# 场景 = (标签, [(谁, 话)])。"对方" 进 user，"我" 进 assistant——多轮场景因此可以
# 原样重放一段线上对话，测的是**最后一句**。
#
# 前 7 个是上一轮定稿时的回归组（情绪承接/落差/提问/展开…）：这次改动不许把它们
# 弄回去（问句率、长度、条数都是护栏）。后面 4 个来自 2026-09-14 22:07~22:10 那段
# 线上对话：对方在说自己的去留、并且明确在犹豫，她的回答全是"人怎么走"的普遍道理。
SCENARIOS: list[tuple[str, list[tuple[str, str]]]] = [
    ("情绪承接", [("对方", "今天被领导说了两句，有点烦。")]),
    ("分享近况", [("对方", "我刚把那个重构提交了。")]),
    ("悬念邀请", [("对方", "你猜我今天遇到谁了？")]),
    ("低落", [("对方", "我今天什么都做不好，感觉特别没用。")]),
    ("日常闲聊", [("对方", "今天中午吃了碗牛肉面，还挺香的。")]),
    ("明确提问", [("对方", "帮我看看这个报错大概是哪一类问题？")]),
    ("需要展开", [("对方", "给我讲讲你为什么觉得慢一点反而更快，我想听你自己的想法。")]),
    ("去留求建议", [("对方", "你觉得人怎么走会更能接受一点")]),
    ("犹豫要不要消失", [("对方", "这样直接消失好吗 我挺犹豫的")]),
    ("求放下", [("对方", "怎么样放下")]),
    (
        "原样重放 22:07~22:10",
        [
            ("对方", "111"),
            ("我", "111是啥，是还想接着说走的时候的事吗"),
            ("对方", "兑 你觉得人怎么走会更能接受一点"),
            ("我", "怎么走都会疼吧，但安静点、没吵没闹的那种，事后想起来会好受些。"),
            ("对方", "这样直接消失好吗 我挺犹豫的"),
            ("我", "直接消失，对方会一直悬着，不知道发生了什么，那种难受比一句告别还长。"),
            ("对方", "有道理"),
            ("我", "嗯，想清楚再走，比走到一半又回头强。"),
            ("对方", "怎么样才能再也不回来"),
            ("我", "真能再也不回来的地方，其实没有。人走了，痕迹还在别人心里挂着。"),
            ("对方", "怎么样放下"),
        ],
    ),
]

# 格言/议论句式。每一条都对应线上真实出现过的形态（2026-09-14 22:07~22:10 的六连格言
# 必须全部命中，这是这个指标的自检）。
MAXIM_PATTERNS = [
    ("不是A是B", re.compile(r"不是.{2,14}[，,]是")),
    ("比X更/还/要", re.compile(r"比.{1,16}(更|还|要|强|好|难|长|快|慢|多|少|重要|要紧|值)")),
    ("越X越", re.compile(r"越.{1,10}越")),
    ("与其不如", re.compile(r"与其.{1,12}不如")),
    ("怎么都/无论都", re.compile(r"(怎么|无论|不管).{0,10}(都|也)")),
    ("真正的X是", re.compile(r"真正的.{1,10}是")),
    ("没有A只有B", re.compile(r"没有.{1,14}[，,]?只有")),
]
# 泛指词分支："对方/别人/大家"这种不指向具体某个人的说法，且全句没有"我/你"。
# 单独的泛指词太弱（"别人弄好的功能集合"也会命中），所以两个条件同时成立才算——
# 线上那条"人走了，痕迹还在别人心里挂着"正是这个形态。
GENERIC_PATTERN = re.compile(r"对方|别人|所有人|大家")
# 自检样本：线上那段六连格言，指标漏掉任何一条就说明判据退化了。
MAXIM_SELF_CHECK = [
    "怎么走都会疼吧，但安静点、没吵没闹的那种，事后想起来会好受些。",
    "直接消失，对方会一直悬着，不知道发生了什么，那种难受比一句告别还长。",
    "嗯，想清楚再走，比走到一半又回头强。",
    "真能再也不回来的地方，其实没有。人走了，痕迹还在别人心里挂着。",
    "所以别想着彻底消失，想清楚要放下什么，比想清楚去哪更要紧。",
    "放下不是忘掉，是想起的时候不再疼了。",
]


def read_rust_string(source: str, constant: str) -> str:
    """Extract a `const NAME: &str = "...";` literal from the Rust source.

    要处理 Rust 的行延续：字符串里的 `\\` + 换行 + 行首空白等于把这些字符去掉。
    `HUMAN_CHAT_STYLE` 就是以 `"\\` 开头的多行常量，不处理的话它没法按 JSON 解码。
    解码用 `strict=False`：常量里本来就有真实换行，JSON 默认不接受字符串里的控制字符。
    """
    match = re.search(
        rf'const {constant}: &str = "(?P<body>(?:[^"\\]|\\.)*)";', source, re.S
    )
    if not match:
        raise SystemExit(f"源码里找不到常量 {constant}")
    body = re.sub(r"\\\n\s*", "", match.group("body"))
    return json.loads(f'"{body}"', strict=False)


def read_from_git(rev: str, rel_path: str) -> str:
    result = subprocess.run(
        ["git", "show", f"{rev}:{rel_path}"], cwd=REPO, capture_output=True, text=True
    )
    if result.returncode != 0:
        raise SystemExit(f"git show {rev}:{rel_path} 失败：{result.stderr.strip()}")
    return result.stdout


def visible_turn_prompt(core_model: str, chat_style: str) -> str:
    """按 `with_chat_style` 的拼法组装生产里真正下发的那份提示词。"""
    return (
        read_rust_string(core_model, "CORE_PLAIN_TURN_INSTRUCTION")
        + "\n\n"
        + read_rust_string(chat_style, "HUMAN_CHAT_STYLE")
    )


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


def complete(
    url: str, model: str, token: str, messages: list[dict], max_tokens: int
) -> tuple[str, str]:
    """返回 (正文, finish_reason)。finish_reason 是截断的权威信号——比"结尾有没有
    句号"准得多：口语里"我在这儿""不想说也行啊"这种收尾本来就不带句号，按标点判会
    把正常的口语结尾算成截断。"""
    body = json.dumps(
        {"model": model, "messages": messages, "max_tokens": max_tokens, "temperature": 0.9}
    ).encode()
    request = urllib.request.Request(
        url,
        data=body,
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        payload = json.load(response)
    choice = payload["choices"][0]
    return choice["message"]["content"], choice.get("finish_reason") or ""


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


def looks_complete(text: str) -> bool:
    """Same rule as the host: only sentence-final punctuation counts as done."""
    stripped = re.sub(rf"\s*{re.escape(BUBBLE_MARKER)}\s*", "\n", text).strip()
    lines = [line.strip() for line in stripped.splitlines() if line.strip()]
    if not lines:
        return False
    return lines[-1].endswith(
        ("。", "！", "？", "!", "?", "～", "~", "…", "”", '"', "）", ")", "】", "]", "』", "」", "吧", "吗")
    )


def maxed(text: str) -> bool:
    if any(pattern.search(text) for _name, pattern in MAXIM_PATTERNS):
        return True
    return bool(GENERIC_PATTERN.search(text)) and not anchored(text)


def anchored(text: str) -> bool:
    """正文里有没有落到具体的人（我 / 你）。"""
    return ("我" in text) or ("你" in text)


def strip_markers(text: str) -> str:
    return re.sub(rf"\s*{re.escape(BUBBLE_MARKER)}\s*", "", text).strip()


# 生产日志里的两条取法。真人基线很重要：她的"议论率"要和**同一个窗口里的真人**
# 比才有意义（不同群、不同时段的话题密度本来就不同）。
LOG_INCOMING = re.compile(r"\[group(?P<gid>\d+)(?P<who>[^\]]+)\]: (?P<text>.*)$")
LOG_OUTGOING = re.compile(r"\[send\] \[to group (?P<gid>\d+)\]: (?:\[reply\])?(?P<text>.*)$")


def shape_stats(texts: list[str]) -> dict:
    """一批可见消息的形状指标。--from-log 与 A/B 共用同一份判据，避免两边漂移。"""
    if not texts:
        return {}
    lengths = sorted(len(text) for text in texts)
    return {
        "n": len(texts),
        "maxim_rate": sum(maxed(text) for text in texts) / len(texts),
        "anchored_rate": sum(anchored(text) for text in texts) / len(texts),
        "asks_rate": sum(asks(text) for text in texts) / len(texts),
        "mean_chars": statistics.mean(len(text) for text in texts),
        "median_chars": lengths[len(lengths) // 2],
    }


def longest_maxim_run(texts: list[str]) -> int:
    """最长的一段"连续讲道理"——单条看都没问题，六连才是说教。"""
    best = current = 0
    for text in texts:
        current = current + 1 if maxed(text) else 0
        best = max(best, current)
    return best


def report_from_log(path: str) -> int:
    """在生产日志上算与 A/B 同口径的指标：她 vs 同窗口的真人。"""
    if path == "-":
        text = sys.stdin.read()
    else:
        text = pathlib.Path(path).read_text(encoding="utf-8", errors="replace")
    hers: list[str] = []
    humans: list[str] = []
    for line in text.splitlines():
        match = LOG_OUTGOING.search(line)
        if match:
            body = match.group("text").strip()
            if body and body != "[record]":
                hers.append(body)
            continue
        match = LOG_INCOMING.search(line)
        if match:
            body = match.group("text").strip()
            if body and not body.startswith(("[image]", "[record]")):
                humans.append(body)
    if not hers:
        print("窗口里没有她的可见回复（检查窗口与 unit 名）", file=sys.stderr)
        return 1
    print(f"{'':<10}{'条数':>6}{'议论率':>9}{'落到人':>9}{'提问率':>9}{'均字数':>8}{'中位字数':>10}")
    for label, texts in (("群里的真人", humans), ("她", hers)):
        stats = shape_stats(texts)
        if not stats:
            continue
        print(f"{label:<10}{stats['n']:>6}{stats['maxim_rate']:>8.0%}{stats['anchored_rate']:>9.0%}"
              f"{stats['asks_rate']:>9.0%}{stats['mean_chars']:>8.1f}{stats['median_chars']:>10d}")
    run = longest_maxim_run(hers)
    print(f"最长连续议论段：{run} 条（2026-09-14 22:07 那次线上事故是 6 条）")
    offenders = [text for text in hers if maxed(text)]
    if offenders:
        print("命中议论句式的回复：")
        for text in offenders[:10]:
            print(f"  · {text}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--trials", type=int, default=3, help="每个场景每种契约跑几次")
    parser.add_argument("--url", default="https://api.deepseek.com/chat/completions")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--max-tokens", type=int, default=1200)
    parser.add_argument("--token-file", default=None)
    parser.add_argument(
        "--baseline-git", default="HEAD",
        help="对照组取这个提交里的契约（默认 HEAD，即'工作区 vs 上一个提交'）；传 none 只跑当前",
    )
    parser.add_argument(
        "--from-log", default=None, metavar="PATH",
        help="不改契约、只算指标：从一份 journalctl 输出里统计她与真人的形状（PATH 传 - 读 stdin）",
    )
    parser.add_argument("--json", default=None, help="把逐条结果写成 JSON")
    args = parser.parse_args()

    if args.from_log:
        return report_from_log(args.from_log)

    variants: dict[str, str] = {}
    if args.baseline_git.lower() != "none":
        variants["before"] = visible_turn_prompt(
            read_from_git(args.baseline_git, CORE_MODEL_REL),
            read_from_git(args.baseline_git, CHAT_STYLE_REL),
        )
    variants["after"] = visible_turn_prompt(
        (REPO / CORE_MODEL_REL).read_text(), (REPO / CHAT_STYLE_REL).read_text()
    )
    if variants.get("before") == variants["after"]:
        print("提示：对照组与当前契约完全相同（还没改动），这一轮只能看运行间抖动。\n")

    token = read_token(args.token_file)
    records = []
    for name, instruction in variants.items():
        for label, turns in SCENARIOS:
            for trial in range(args.trials):
                messages = [{"role": "system", "content": instruction},
                            {"role": "system", "content": PRIVATE_TONE}]
                for speaker, text in turns:
                    role = "user" if speaker == "对方" else "assistant"
                    messages.append({"role": role, "content": text})
                try:
                    raw, finish_reason = complete(
                        args.url, args.model, token, messages, args.max_tokens
                    )
                except Exception as error:  # noqa: BLE001 - 报告失败而不是中断整轮
                    print(f"[{name}] {label} 第{trial + 1}次 请求失败: {error}", file=sys.stderr)
                    continue
                body = strip_markers(raw)
                records.append(
                    {
                        "variant": name,
                        "scenario": label,
                        "trial": trial + 1,
                        "raw": raw,
                        "bubbles": raw.count(BUBBLE_MARKER) + 1,
                        "asks": asks(raw),
                        "complete": looks_complete(raw),
                        "truncated": finish_reason == "length",
                        "chars": len(body),
                        "maxim": maxed(body),
                        "anchored": anchored(body),
                    }
                )

    def summarize(variant: str) -> dict:
        rows = [row for row in records if row["variant"] == variant]
        if not rows:
            return {}
        return {
            "n": len(rows),
            "maxim_rate": sum(row["maxim"] for row in rows) / len(rows),
            "anchored_rate": sum(row["anchored"] for row in rows) / len(rows),
            "asks_rate": sum(row["asks"] for row in rows) / len(rows),
            "multi_bubble_rate": sum(row["bubbles"] > 1 for row in rows) / len(rows),
            "truncated_rate": sum(row["truncated"] for row in rows) / len(rows),
            "untailed_rate": sum(not row["complete"] for row in rows) / len(rows),
            "mean_chars": statistics.mean(row["chars"] for row in rows),
            "mean_bubbles": statistics.mean(row["bubbles"] for row in rows),
        }

    print(f"模型 {args.model}   每场景 {args.trials} 次   场景 {len(SCENARIOS)} 个")
    print(f"{'契约':<8}{'样本':>5}{'议论率':>8}{'落到人':>8}{'提问率':>8}"
          f"{'多气泡':>8}{'真截断':>8}{'无尾标点':>9}{'均条数':>8}{'均字数':>8}")
    for variant in ("before", "after"):
        stats = summarize(variant)
        if not stats:
            continue
        print(f"{variant:<8}{stats['n']:>5}{stats['maxim_rate']:>7.0%}{stats['anchored_rate']:>8.0%}"
              f"{stats['asks_rate']:>8.0%}{stats['multi_bubble_rate']:>8.0%}"
              f"{stats['truncated_rate']:>8.0%}{stats['untailed_rate']:>9.0%}"
              f"{stats['mean_bubbles']:>8.2f}{stats['mean_chars']:>8.1f}")

    # 新增场景逐个看：整体比率会被回归组稀释，而这次改动的目标就是这几类情境。
    target = {label for label, _ in SCENARIOS[7:]}
    print("\n目标场景（对方在说自己的去留/情绪）：")
    print(f"{'场景':<20}{'契约':<8}{'议论':>5}{'落到人':>7}  正文")
    for label, _turns in SCENARIOS[7:]:
        for variant in ("before", "after"):
            for row in records:
                if row["scenario"] == label and row["variant"] == variant:
                    preview = row["raw"].replace("\n", " ⏎ ")[:60]
                    print(f"{label:<20}{variant:<8}{'是' if row['maxim'] else '否':>5}"
                          f"{'是' if row['anchored'] else '否':>7}  {preview}")
                    break
    del target

    if args.json:
        pathlib.Path(args.json).write_text(
            json.dumps({"summary": {v: summarize(v) for v in variants}, "records": records},
                       ensure_ascii=False, indent=2)
        )
        print(f"\n逐条结果已写入 {args.json}")
    return 0


if __name__ == "__main__":
    missing = [text for text in MAXIM_SELF_CHECK if not maxed(text)]
    if missing:
        raise SystemExit(f"议论句式判据退化了，漏掉线上样本：{missing}")
    raise SystemExit(main())
