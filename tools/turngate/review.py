#!/usr/bin/env python3
"""TurnGate 待复核样本的人工复核工具 (doc §7.4 B/C/E 最小闭环)。

流程:
1. collector.py 产出 review-batch-*.jsonl(review_status=pending,伪标签);
2. 人工复核: `--mark <idx> completion=flush_now response=answer`
   (label 可省略其一; 不确定就用 completion=null 标记待回访);
3. `--status` 查看进度; `--export` 导出可训练集(human_consensus 且
   agreement 达标), 支持 `--include-pseudo` 把弱标签一并导出(仅候选);
4. 隐私/删除: 每个样本带不透明 source_key; `--delete-key <key>`
   打印该键对应的样本并从待复核/训练文件删除(仅本机运维,不得进入
   模型权重——模型更新=新版本+重新评估, doc §7.4 D)。

用法:
    python3 tools/turngate/review.py --batch batch.jsonl --status
    python3 tools/turngate/review.py --batch batch.jsonl --mark 3 completion=flush_now response=ignore
    python3 tools/turngate/review.py --batch batch.jsonl --export train_turngate.jsonl --min-agreement 0.9
"""

import argparse
import json
import sys
from pathlib import Path

REVIEWED_SOURCE = "human_consensus"


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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--batch", required=True, type=Path)
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--mark", nargs="+", metavar="IDX completion=X response=Y")
    parser.add_argument("--export", type=Path)
    parser.add_argument("--include-pseudo", action="store_true")
    parser.add_argument("--min-agreement", type=float, default=0.9)
    parser.add_argument("--delete-key")
    args = parser.parse_args()

    samples = load(args.batch)

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

    print("choose --status / --mark / --export / --delete-key", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
