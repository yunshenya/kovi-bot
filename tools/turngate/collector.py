#!/usr/bin/env python3
"""TurnGate 线上候选样本采集器 (doc §7.4 B/C/E, §7.5)。

输入: `journalctl -u kovi-bot.service` 导出的日志文件。
输出: 待复核 JSONL (schema_version 2, review_status=pending)。

规则:
- 按 (group, sender, 间隔 ≤3s) 切分"一次发言"(unit);unit 内前几条作为
  pending_user_fragments,最后一条作为 current_text;
- 机器人的 `[send]` 行构成 assistant 角色 turn,用于 recent_turns 与
  conversation_active;
- 弱标签(pseudo_lexical_v0)来自高精度本地规则,只作候选,必须人工复核;
  lexical 无法判定的 completion 置 null;
- 不保存 QQ 号/昵称/URL/Token;source_key 为不透明哈希,供删除屏障使用;
- 默认不采集:纯本地工具,由开发者在服务器运行时显式执行。

用法:
    journalctl -u kovi-bot.service --since "2026-09-06 00:00:00" \
        | grep -E "\\[group|\\[send\\]" > /tmp/tg-journal.txt
    python3 tools/turngate/collector.py --journal /tmp/tg-journal.txt \
        --out tools/turngate/dataset/review-batch-20260907.jsonl
"""

import argparse
import hashlib
import json
import re
import sys
from datetime import datetime
from pathlib import Path

SCHEMA_VERSION = 2
FRAGMENT_GAP_SECS = 3.0
EPISODE_GAP_SECS = 300.0
CONTINUATION_WINDOW_SECS = 600.0
MAX_PENDING = 4
MAX_RECENT_TURNS = 4
MAX_FRAGMENT_CHARS = 160
MAX_CURRENT_CHARS = 512

# 用 search:syslog 前缀可变(kovi-bot[pid]:),不在前缀上做贪婪回溯。
LINE_RE = re.compile(
    r"\[(\d{2}-\d{2} \d{2}:\d{2}:\d{2})\] \[group(\d+)([^\]\s]*)\s+(\d+)\]: ?(.*)$"
)
# [send] 行没有内层时间戳,用 syslog 时间(见 parse_from_syslog)。
SEND_RE = re.compile(r"\[send\] \[to group (\d+)\]: ?(.*)$")
# 第一行之后的续行(无 group 前缀)尽量匹配:任何不以 [ 开头的普通文本行。
BODY_CONTINUATION_RE = re.compile(r"^[^\[\]].+$")

URL_RE = re.compile(r"https?://\S+", re.I)
DIGITS_RE = re.compile(r"\d{8,}")


def parse_ts(raw: str) -> datetime:
    # 日志自带的年份前缀 2 位数字在现代 locale 中解析有歧义,固定按主题年。
    return datetime.strptime(raw, "%m-%d %H:%M:%S").replace(year=2026)

def parse_syslog_ts(line: str) -> datetime:
    # "Sep 06 00:46:50 host kovi-bot[pid]: ..." -> 当年对应时刻。
    month_map = {m: i for i, m in enumerate(
        "Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split(), start=1)}
    m = re.match(r"^(\S+) (\d{2}) (\d{2}):(\d{2}):(\d{2})", line)
    if not m:
        return datetime.now()
    return datetime(2026, month_map[m.group(1)], int(m.group(2)),
                    int(m.group(3)), int(m.group(4)), int(m.group(5)))


def sanitize(text: str) -> str:
    text = URL_RE.sub("<url>", text)
    text = DIGITS_RE.sub("<id>", text)
    return text


def mirror_lexical_completion(text: str):
    """镜像 crates/yunxi-core lexical_completion 的高精度规则。"""
    text = text.strip()
    if not text:
        return "hold_for_more"
    unclosed = re.findall(r"[([{【「『]|$", text)
    opens = text.count("(") + text.count("[") + text.count("{") + text.count("【")
    closes = text.count(")") + text.count("]") + text.count("}") + text.count("】")
    if opens > closes:
        return "hold_for_more"
    if text.endswith("。") or text.endswith("！") or text.endswith("？") \
            or text.endswith("!") or text.endswith("?") or text.endswith(".") \
            or text.endswith("～") or text.endswith("~") or text.endswith("”") \
            or text.endswith('"') or text.endswith("）") or text.endswith(")") \
            or text.endswith("】") or text.endswith("]"):
        return "flush_now"
    if text.endswith(("，", ",", "、", "：", ":", "；", ";", "…", "—", "-", "/", "\\", "和", "与", "但")) \
            or text.endswith(("因为", "所以", "如果", "然后", "以及")):
        return "hold_for_more"
    if text.startswith(("我想问", "我有个问题", "请问一下", "我想知道")) \
            and not text.endswith(("吗", "呢", "?", "？")):
        return "hold_for_more"
    if text in ("你好", "嗨", "哈喽", "谢谢", "感谢", "多谢", "好的", "好呀", "收到",
                "知道了", "明白了", "晚安", "早安", "再见", "嗯", "哦", "哈哈", "哈哈哈"):
        return "flush_now"
    if text.endswith(("吗", "呢", "怎么样", "怎么了", "好不好", "是什么", "可以吗", "行不行")):
        return "flush_now"
    return None  # 灰区 → MiniMind/人工


def pseudo_response(text: str, context: dict) -> str:
    """高精度弱标签:仅作候选。实际标签必须人工复核。"""
    scope = context.get("scope", "group")
    clean = text.strip()
    if not clean and (context.get("has_image") or context.get("has_sticker")):
        return "ignore"
    addressed = context.get("addressed_to_agent")
    replies = context.get("replies_to_agent")
    if scope == "private":
        return "ack" if len(clean) <= 8 else "answer"
    if addressed or replies:
        return "ack" if len(clean) <= 8 else "answer"
    return "ignore"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--journal", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--max-samples", type=int, default=2000)
    parser.add_argument("--scope", default="group", choices=["group"])
    args = parser.parse_args()

    events = []  # (ts, kind, group, user, text)
    last = None
    with open(args.journal, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            m = LINE_RE.search(line)
            if m:
                ts = parse_ts(m.group(1))
                events.append(("user", int(m.group(2)), int(m.group(4)), m.group(5), ts))
                last = ("user", int(m.group(2)), int(m.group(4)), ts)
                continue
            m = SEND_RE.search(line)
            if m:
                ts = parse_syslog_ts(line)
                events.append(("bot", int(m.group(1)), None, m.group(2), ts))
                last = ("bot", int(m.group(1)), None, ts)
                continue
            # 续行:同组同发送者的多段消息
            if last and BODY_CONTINUATION_RE.match(line) and not line.startswith("["):
                kind, group, user, ts = last
                if events and events[-1][4] == ts and events[-1][0] == kind:
                    events[-1] = (
                        kind,
                        group,
                        user,
                        events[-1][3] + "\n" + line.strip()[:200],
                        ts,
                    )
    if not events:
        print("no parseable events", file=sys.stderr)
        return 1

    # 按群分组切 unit: 同发送者间隔 ≤3s 为一次发言
    by_group: dict[int, list] = {}
    for ev in events:
        by_group.setdefault(ev[1], []).append(ev)

    samples = []
    seen_keys = set()
    for group_id, evs in by_group.items():
        # 完整时间线: 重建 unit 序列 (每 unit = 同一发言者的连续片段)
        units = []
        for ev in evs:
            if units and units[-1][0] == ev[0] and units[-1][1] == ev[2] \
                    and (ev[4] - units[-1][3]).total_seconds() <= FRAGMENT_GAP_SECS:
                units[-1][2].append((ev[3], ev[4]))
            else:
                units.append([ev[0], ev[2], [(ev[3], ev[4])], ev[4]])
        units.sort(key=lambda u: u[3])

        # bot 发送时间线(判断 conversation_active + assistant turns)
        for idx, unit in enumerate(units):
            kind, sender, fragments, ts = unit
            if kind != "user":
                continue
            texts = [f[0] for f in fragments]
            cur = texts[-1][:MAX_CURRENT_CHARS]
            pending = [t[:MAX_FRAGMENT_CHARS] for t in texts[:-1]][:MAX_PENDING]
            has_at = "[at]" in cur
            has_reply = cur.strip().startswith("[reply]")
            has_image = "[image]" in cur
            has_face = "[face]" in cur
            has_at_flag = has_at or "[at]" in "".join(texts)
            has_reply_flag = has_reply or "[reply]" in "".join(texts)
            clean = sanitize(cur)
            # recent turns: 之前的 ≤4 个 user/bot unit
            recent = []
            recent_ts = ts
            for prev in reversed(units[:idx]):
                p_kind, p_sender, p_frags, p_ts = prev
                if (ts - p_ts).total_seconds() > EPISODE_GAP_SECS and not recent:
                    break
                p_text = sanitize("".join(f[0] for f in p_frags))[:MAX_FRAGMENT_CHARS]
                role = "assistant" if p_kind == "bot" else (
                    "user" if p_sender == sender else "other_member"
                )
                recent.append({"role": role, "text": p_text})
                if len(recent) >= MAX_RECENT_TURNS:
                    break
            recent.reverse()
            # conversation_active: 600s 内有 bot 发言或最近 turn
            recent_bot = any(r["role"] == "assistant" for r in recent)
            bot_recent = any(
                e[0] == "bot" and e[1] == group_id and 0 <= (ts - e[4]).total_seconds()
                <= CONTINUATION_WINDOW_SECS
                and e[4] <= ts
                for e in events
            )
            conversation_active = recent_bot or bot_recent

            context = {
                "scope": "group",
                "pending_user_fragments": [sanitize(t) for t in pending],
                "recent_turns": recent,
                "conversation_active": conversation_active,
                "bot_last_asked_question": None,
                "pending_outgoing": False,
                "pending_task": False,
                "addressed_to_agent": has_at_flag,
                "replies_to_agent": has_reply_flag,
                "has_image": has_image,
                "has_sticker": has_face,
                "policy_override": "must_reply" if (has_at_flag or has_reply_flag) else "none",
            }
            key = hashlib.sha256(
                ("group|%s|%s|%s" % (group_id, clean, json.dumps(recent, ensure_ascii=False)))
                .encode("utf-8")
            ).hexdigest()
            if key in seen_keys:
                continue
            seen_keys.add(key)
            completion = mirror_lexical_completion(cur)
            samples.append(
                {
                    "schema_version": SCHEMA_VERSION,
                    "current_text": clean,
                    "context": context,
                    "labels": {
                        "completion": completion,
                        "response": pseudo_response(cur, context)
                        if completion is not None
                        else None,
                    },
                    "label_provenance": {
                        "source": "pseudo_lexical_v0",
                        "annotator_count": 0,
                        "agreement": 0.0,
                    },
                    "review_status": "pending",
                    "source_key": key,
                }
            )
            if len(samples) >= args.max_samples:
                break
        if len(samples) >= args.max_samples:
            break

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        for sample in samples:
            fh.write(json.dumps(sample, ensure_ascii=False, separators=(",", ":")) + "\n")
    n_pseudo = sum(1 for s in samples if s["labels"]["completion"] is not None)
    print(
        f"wrote {args.out}: {len(samples)} samples "
        f"({n_pseudo} with lexical completion label, "
        f"{len(samples) - n_pseudo} gray-zone candidates)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
