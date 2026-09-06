#!/usr/bin/env python3
"""TurnGate 特征参考实现（与 crates/yunxi-core/src/model/turn_gate.rs 逐字节对齐）。

用途：
- 离线训练器直接 import 本模块提取特征（保证与生产 Rust runtime 完全一致）；
- `python3 tools/turngate/features.py` 打印内置样例的特征向量，用于人工核对；
- 训练器必须在 CI 中运行 `python3 tools/turngate/check_parity.py` 确认与
  Rust 侧 golden 向量一致（Phase 0 验收：相同样本得到完全一致的非零特征索引）。

协议（详见 docs/yunxi-turngate-design.md §5）：
- hash: FNV-1a 32-bit，作用于 UTF-8 字节；桶数 65536；
- 文本特征：字符 2..=5-gram，字段边界标记 \x01 当前 / \x02 提问 /
  \x03 pending / \x04u \x04o \x04a 最近轮（user/other/assistant）；
- 单桶计数上限 2，单次推理文本非零特征上限 512；
- 结构化特征：固定位置布尔位（见 CONTEXT 顺序，与 Rust 的
  context_feature_index 一致）。

注意：任何对该文件的改动都必须同步修改 Rust 端并重新生成 golden 向量，
否则训练器与生产运行时会产生特征漂移。
"""

import json
import sys

FNV_OFFSET = 0x811C9DC5
FNV_PRIME = 0x01000193
HASH_BUCKETS = 65536
NGRAM_MIN = 2
NGRAM_MAX = 5
MAX_TEXT_FEATURES = 512
MAX_FEATURE_COUNT = 2
MAX_CURRENT_CHARS = 512
MAX_CURRENT_BYTES = 2048
MAX_FRAGMENT_CHARS = 160
MAX_PENDING_FRAGMENTS = 4
MAX_RECENT_TURNS = 4
MAX_QUESTION_CHARS = 160

FIELD_MARKERS = {
    "current": "\u0001",
    "question": "\u0002",
    "pending": "\u0003",
    "recent_user": "\u0004u",
    "recent_other": "\u0004o",
    "recent_assistant": "\u0004a",
}

# 结构化特征固定位置（与 Rust context_feature_index 保持一致）。
CONTEXT_INDEX = [
    "scope_private",
    "scope_group",
    "pending_not_empty",
    "recent_turns_not_empty",
    "recent_role_user",
    "recent_role_other",
    "recent_role_assistant",
    "addressed_to_agent",
    "replies_to_agent",
    "conversation_active",
    "had_asked_question",
    "pending_outgoing",
    "pending_task",
    "has_image",
    "has_sticker",
    "policy_must_reply",
    "policy_command",
    "policy_stop",
    "policy_erase",
]


def fnv1a_32(data: bytes) -> int:
    h = FNV_OFFSET
    for byte in data:
        h ^= byte
        h = (h * FNV_PRIME) & 0xFFFFFFFF
    return h


def normalize_text(text: str) -> str:
    out = []
    last_was_space = True
    for ch in text:
        if ch.isspace():
            if not last_was_space:
                out.append(" ")
                last_was_space = True
            continue
        last_was_space = False
        out.append(ch.lower() if ch.isascii() else ch)
    normalized = "".join(out)
    while normalized.endswith(" "):
        normalized = normalized[:-1]
    # 截断到最大字符数
    normalized = normalized[:MAX_CURRENT_CHARS]
    # 截断到最大字节数（不切断 UTF-8 字符）
    buf = ""
    byte_len = 0
    for ch in normalized:
        size = len(ch.encode("utf-8"))
        if byte_len + size > MAX_CURRENT_BYTES:
            break
        buf += ch
        byte_len += size
    return buf


def extract_text_features(text: str, marker: str):
    counts = {}
    normalized = normalize_text(text)
    chars = list(normalized)
    for width in range(NGRAM_MIN, NGRAM_MAX + 1):
        if len(chars) < width:
            break
        for i in range(len(chars) - width + 1):
            gram = marker + "".join(chars[i : i + width])
            bucket = fnv1a_32(gram.encode("utf-8")) % HASH_BUCKETS
            if bucket in counts:
                counts[bucket] = min(counts[bucket] + 1, MAX_FEATURE_COUNT)
            elif len(counts) < MAX_TEXT_FEATURES:
                counts[bucket] = 1
    return counts


def _merge(target: dict, source: dict):
    for bucket, count in source.items():
        if bucket in target:
            target[bucket] = min(target[bucket] + count, MAX_FEATURE_COUNT)
        else:
            target[bucket] = count


def extract_features(sample: dict) -> dict:
    """sample 与训练 JSONL 的 context 字段同构（不含 labels）。"""
    context = sample.get("context", sample)
    current_text = context.get("current_text", "")
    pending = context.get("pending_user_fragments", [])[:MAX_PENDING_FRAGMENTS]
    recent = context.get("recent_turns", [])[:MAX_RECENT_TURNS]
    question = context.get("bot_last_asked_question")

    text_features = {}
    _merge(text_features, extract_text_features(current_text, FIELD_MARKERS["current"]))
    if question is not None:
        _merge(
            text_features,
            extract_text_features(question, FIELD_MARKERS["question"]),
        )
    for fragment in pending:
        _merge(
            text_features,
            extract_text_features(fragment[:MAX_FRAGMENT_CHARS], FIELD_MARKERS["pending"]),
        )
    role_marker = {
        "user": FIELD_MARKERS["recent_user"],
        "other_member": FIELD_MARKERS["recent_other"],
        "assistant": FIELD_MARKERS["recent_assistant"],
    }
    for turn in recent:
        marker = role_marker.get(turn.get("role", "user"), FIELD_MARKERS["recent_user"])
        _merge(
            text_features,
            extract_text_features(turn.get("text", "")[:MAX_FRAGMENT_CHARS], marker),
        )

    text = [{"index": index, "count": count} for index, count in text_features.items()]
    text = text[:MAX_TEXT_FEATURES]

    flags = {
        "scope_private": context.get("scope", "private") == "private",
        "scope_group": context.get("scope") == "group",
        "pending_not_empty": bool(pending),
        "recent_turns_not_empty": bool(recent),
        "recent_role_user": any(t.get("role") == "user" for t in recent),
        "recent_role_other": any(t.get("role") == "other_member" for t in recent),
        "recent_role_assistant": any(t.get("role") == "assistant" for t in recent),
        "addressed_to_agent": bool(context.get("addressed_to_agent", False)),
        "replies_to_agent": bool(context.get("replies_to_agent", False)),
        "conversation_active": bool(context.get("conversation_active", False)),
        "had_asked_question": question is not None,
        "pending_outgoing": bool(context.get("pending_outgoing", False)),
        "pending_task": bool(context.get("pending_task", False)),
        "has_image": bool(context.get("has_image", False)),
        "has_sticker": bool(context.get("has_sticker", False)),
        "policy_must_reply": context.get("policy_override", "none") == "must_reply",
        "policy_command": context.get("policy_override") == "command",
        "policy_stop": context.get("policy_override") == "stop",
        "policy_erase": context.get("policy_override") == "erase",
    }
    context_indices = [i for i, name in enumerate(CONTEXT_INDEX) if flags.get(name)]
    return {"text_features": text, "context_indices": context_indices}


SAMPLE = {
    "current_text": "我想问你一件事",
    "scope": "private",
    "pending_user_fragments": [],
    "recent_turns": [
        {"role": "user", "text": "最近准备去哪里玩"},
        {"role": "assistant", "text": "还没有决定"},
    ],
    "conversation_active": True,
    "bot_last_asked_question": None,
    "pending_outgoing": False,
    "pending_task": False,
    "addressed_to_agent": False,
    "replies_to_agent": False,
    "has_image": False,
    "has_sticker": False,
    "policy_override": "none",
}


def main() -> int:
    sample = SAMPLE
    if len(sys.argv) > 1:
        sample = json.loads(sys.argv[1])
    print(json.dumps(extract_features(sample), ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
