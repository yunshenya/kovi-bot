#!/usr/bin/env python3
"""TurnGate 待复核样本的人工复核工具 (doc §7.4 B/C/E 最小闭环)。

流程:
1. collector.py 产出 review-batch-*.jsonl(review_status=pending,伪标签);
2. `--queue` 按"标注价值"排出待复核顺序(只用不依赖策略的客观信号排序,
   见 `queue_reason`),`--show <idx>` 看单条的正文与上下文;
3. 人工复核: `--mark <idx> completion=flush_now response=answer`
   (label 可省略其一; 不确定就用 completion=null 标记待回访);
4. `--status` 查看进度; `--export` 导出可训练集(human_consensus 且
   agreement 达标), 支持 `--include-pseudo` 把弱标签一并导出(仅候选);
5. 隐私/删除: 每个样本带不透明 source_key; `--delete-key <key>`
   打印该键对应的样本并从待复核/训练文件删除(仅本机运维,不得进入
   模型权重——模型更新=新版本+重新评估, doc §7.4 D)。

用法:
    python3 tools/turngate/review.py --batch batch.jsonl --queue
    python3 tools/turngate/review.py --batch batch.jsonl --show 42
    python3 tools/turngate/review.py --batch batch.jsonl --status
    python3 tools/turngate/review.py --batch batch.jsonl --mark 3 completion=flush_now response=ignore
    python3 tools/turngate/review.py --batch batch.jsonl --export train_turngate.jsonl --min-agreement 0.9
"""

import argparse
import json
import sys
from pathlib import Path

REVIEWED_SOURCE = "human_consensus"

# 一屏能扫多少行；`--limit` 可覆盖。
DEFAULT_QUEUE_LIMIT = 40
# 队列行里正文/上下文的截断长度（终端摘要，不是数据）。
SNIPPET_CHARS = 64


def load(path: Path):
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def save(path: Path, samples):
    with open(path, "w", encoding="utf-8") as fh:
        for sample in samples:
            fh.write(json.dumps(sample, ensure_ascii=False, separators=(",", ":")) + "\n")


def human_labels(labels: dict):
    completion = labels.get("completion")
    response = labels.get("response")
    completion = completion if completion in ("flush_now", "hold_for_more") else None
    response = (
        response
        if response in ("answer", "continue", "ack", "ignore", "wait")
        else None
    )
    return completion, response


def is_reviewed(sample: dict) -> bool:
    return sample.get("review_status") == "reviewed"


def context_richness(sample: dict) -> int:
    """这条样本带了多少可判断的上下文（越少越依赖模型，越值得标）。"""
    ctx = sample.get("context", {})
    rich = 0
    rich += 1 if ctx.get("recent_turns") else 0
    rich += 1 if ctx.get("pending_user_fragments") else 0
    rich += 1 if ctx.get("conversation_active") else 0
    rich += 1 if ctx.get("bot_last_asked_question") else 0
    rich += 1 if ctx.get("pending_outgoing") else 0
    rich += 1 if ctx.get("pending_task") else 0
    return rich


def queue_reason(sample: dict) -> str:
    """给待复核样本排"标注价值"的理由。

    只使用**不依赖策略**的客观信号：弱标签有没有给出判断、上下文够不够、
    这个标签在批次里稀不稀有。不用"线上实际回了没有"当依据——doc §7.4 E
    明确说那只能作回访和采样依据，不能当标签，否则等于把现有的概率/冷却/
    时间窗策略抄进权重里。
    """
    ctx = sample.get("context", {})
    completion, _response = human_labels(sample.get("labels", {}))
    reasons = []
    if completion is None:
        # lexical 规则给不出判断：这正是需要模型补位的灰区，信息量最大。
        reasons.append("gray-zone")
    if not ctx.get("recent_turns"):
        reasons.append("no-context")
    if not any(t.get("role") == "assistant" for t in ctx.get("recent_turns", [])):
        reasons.append("no-bot-turn")
    if ctx.get("addressed_to_agent") or ctx.get("replies_to_agent"):
        reasons.append("addressed")
    if ctx.get("pending_user_fragments"):
        reasons.append("multi-fragment")
    if ctx.get("has_image"):
        reasons.append("image")
    return ",".join(reasons) if reasons else "context-rich"


def queue_tier(sample: dict) -> int:
    """越小越先标。灰区（弱标签沉默）> 上下文薄弱 > 其余。"""
    ctx = sample.get("context", {})
    completion, _response = human_labels(sample.get("labels", {}))
    if completion is None:
        return 0 if (ctx.get("addressed_to_agent") or ctx.get("replies_to_agent")) else 1
    if not ctx.get("recent_turns"):
        return 2
    if not any(t.get("role") == "assistant" for t in ctx.get("recent_turns", [])):
        return 3
    return 4


def build_queue(samples, limit: int, include_reviewed: bool = False):
    """排待复核顺序：先按标注价值分 tier，同 tier 内**上下文越全越靠前**。

    上下文全的样本标得动、标签可信；上下文残缺的标了也是猜，排在后面（真要看
    可以用 `--show`）。tier 本身仍按"信息量"排：lexical 给不出判断的灰区优先。
    """
    rows = [
        (idx, s) for idx, s in enumerate(samples)
        if include_reviewed or not is_reviewed(s)
    ]
    rows.sort(key=lambda pair: (
        queue_tier(pair[1]),
        -context_richness(pair[1]),
        pair[0],
    ))
    return rows[:limit] if limit > 0 else rows


def snippet(text: str) -> str:
    flat = " ".join(str(text).split())
    if len(flat) > SNIPPET_CHARS:
        flat = flat[:SNIPPET_CHARS] + "…"
    return flat


def render_sample(idx: int, sample: dict) -> str:
    """单条样本的完整复核视图（正文 + 上下文 + 弱标签）。"""
    ctx = sample.get("context", {})
    lines = [f"#{idx}  reason={queue_reason(sample)}  scope={ctx.get('scope')}"]
    lines.append(f"  current_text: {sample.get('current_text', '')}")
    fragments = ctx.get("pending_user_fragments") or []
    if fragments:
        lines.append(f"  pending_fragments({len(fragments)}): {fragments}")
    for turn in ctx.get("recent_turns", []):
        lines.append(f"  [{turn.get('role')}] {turn.get('text')}")
    flags = [
        name for name in (
            "addressed_to_agent", "replies_to_agent", "conversation_active",
            "has_image", "has_sticker", "pending_outgoing", "pending_task",
        ) if ctx.get(name)
    ]
    lines.append(f"  flags: {', '.join(flags) if flags else '-'}")
    if ctx.get("bot_last_asked_question"):
        lines.append(f"  bot_last_asked_question: {ctx['bot_last_asked_question']}")
    labels = sample.get("labels", {})
    lines.append(
        f"  pseudo: completion={labels.get('completion')} response={labels.get('response')}"
        f"  ({sample.get('label_provenance', {}).get('source')})"
    )
    lines.append(f"  status: {sample.get('review_status')}")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--batch", required=True, type=Path)
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--queue", action="store_true",
                        help="按标注价值排出待复核顺序（不改变任何标签）")
    parser.add_argument("--show", type=int, metavar="IDX",
                        help="打印单条样本的正文/上下文/弱标签")
    parser.add_argument("--limit", type=int, default=DEFAULT_QUEUE_LIMIT,
                        help=f"--queue 打印多少行，0 表示全部（默认 {DEFAULT_QUEUE_LIMIT}）")
    parser.add_argument("--slice", metavar="START:END",
                        help="跳过队列前 START 条再取 END 条（如 300:100 看第 301-400 条）")
    parser.add_argument("--include-reviewed", action="store_true",
                        help="--queue 时连已复核的也列出")
    parser.add_argument("--mark", nargs="+", metavar="IDX completion=X response=Y")
    parser.add_argument("--export", type=Path)
    parser.add_argument("--include-pseudo", action="store_true")
    parser.add_argument("--min-agreement", type=float, default=0.9)
    parser.add_argument("--delete-key")
    args = parser.parse_args()

    samples = load(args.batch)

    if args.queue:
        offset = 0
        if args.slice:
            start, _, count = args.slice.partition(":")
            try:
                offset = int(start)
                args.limit = int(count)
            except ValueError:
                print("--slice 需要 START:END 形式，例如 300:100", file=sys.stderr)
                return 1
            if offset < 0 or args.limit < 0:
                print("--slice 不接受负数", file=sys.stderr)
                return 1
        rows = build_queue(samples, 0, args.include_reviewed)
        total_rows = len(rows)
        rows = rows[offset:offset + args.limit] if args.limit > 0 else rows[offset:]
        pending = [s for s in samples if not is_reviewed(s)]
        gray = sum(1 for s in pending if human_labels(s.get("labels", {}))[0] is None)
        with_ctx = sum(1 for s in pending if s.get("context", {}).get("recent_turns"))
        with_bot = sum(
            1 for s in pending
            if any(t.get("role") == "assistant" for t in s.get("context", {}).get("recent_turns", []))
        )
        print(f"queue={total_rows} pending={len(pending)} "
              f"showing={offset + 1}-{offset + len(rows)} "
              f"(tier: 0/1 灰区, 2 无上下文, 3 无机器人发言, 4 其余)")
        # 上下文覆盖率是判断"标得动多少"的前提：没有上下文只能靠猜，
        # 硬标出来的标签会把噪声当监督。
        print(f"  coverage: gray_zone={gray} with_recent_turns={with_ctx} with_bot_turn={with_bot}")
        print(f"{'idx':>5}  {'tier':>4}  {'ctx':>3}  reason")
        for idx, sample in rows:
            print(f"{idx:>5}  {queue_tier(sample):>4}  {context_richness(sample):>3}  "
                  f"{queue_reason(sample)}  | {snippet(sample.get('current_text', ''))}")
        print("\n看单条: --show <idx>    标注: --mark <idx> completion=... response=...")
        return 0

    if args.show is not None:
        if not (0 <= args.show < len(samples)):
            print("index out of range", file=sys.stderr)
            return 1
        print(render_sample(args.show, samples[args.show]))
        return 0

    if args.status:
        total = len(samples)
        reviewed = sum(1 for s in samples if s.get("review_status") == "reviewed")
        dist_c, dist_r = {}, {}
        for s in samples:
            c, r = human_labels(s["labels"])
            dist_c[c] = dist_c.get(c, 0) + 1
            dist_r[r] = dist_r.get(r, 0) + 1
        print(f"total={total} reviewed={reviewed} pending={total - reviewed}")
        print(f"completion: {dist_c}")
        print(f"response:   {dist_r}")
        return 0

    if args.mark:
        parts = {}
        for token in args.mark:
            if token.isdigit():
                idx = int(token)
                continue
            key, _, value = token.partition("=")
            parts[key] = value
        if "idx" not in dir() and not any(p.isdigit() for p in args.mark):
            print("usage: --mark <idx> completion=... response=...", file=sys.stderr)
            return 1
        idx = next(int(t) for t in args.mark if t.isdigit())
        if not (0 <= idx < len(samples)):
            print("index out of range", file=sys.stderr)
            return 1
        sample = samples[idx]
        labels = sample["labels"]
        if "completion" in parts:
            value = parts["completion"]
            labels["completion"] = value if value in ("flush_now", "hold_for_more") else None
        if "response" in parts:
            value = parts["response"]
            labels["response"] = (
                value if value in ("answer", "continue", "ack", "ignore", "wait") else None
            )
        sample["label_provenance"] = {
            "source": REVIEWED_SOURCE,
            "annotator_count": 1,
            "agreement": 1.0,
        }
        sample["review_status"] = "reviewed"
        save(args.batch, samples)
        print(f"marked #{idx} -> completion={labels['completion']} response={labels['response']}")
        return 0

    if args.delete_key:
        kept = [s for s in samples if s.get("source_key") != args.delete_key]
        print(f"removed {len(samples) - len(kept)} sample(s) for key {args.delete_key}")
        save(args.batch, kept)
        return 0

    if args.export:
        usable = []
        for s in samples:
            source = s.get("label_provenance", {}).get("source")
            if source == REVIEWED_SOURCE:
                if s["label_provenance"].get("agreement", 0.0) >= args.min_agreement:
                    usable.append(s)
            elif args.include_pseudo and s.get("review_status") == "pending":
                usable.append(s)
        for s in usable:
            s.pop("review_status", None)
            s.pop("source_key", None)
        save(args.export, usable)
        print(f"exported {len(usable)} samples -> {args.export}")
        return 0

    print("choose --status / --queue / --show / --mark / --export / --delete-key",
          file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
