#!/usr/bin/env python3
"""TurnGate 线上候选样本采集器 (doc §7.4 B/C/E, §7.5)。

输入: `journalctl -u kovi-bot.service` 导出的日志文件。
输出: 待复核 JSONL (schema_version 2, review_status=pending)。

规则:
- 按 (group, sender, 间隔 ≤3s) 切分"一次发言"(unit);unit 内前几条作为
  pending_user_fragments,最后一条作为 current_text;
- 机器人的 `[send]` 行构成 assistant 角色 turn,用于 recent_turns 与
  conversation_active;
- recent_turns 每条带一个**样本内匿名**的说话人编号(`speaker`: a=当前发言者,
  b/c/… 按出现顺序给其他成员)。`role` 只能分出"他人",分不清是几个人——编号
  补上"这几条是不是同一个人"这一维,且只落序号,不落 QQ 号/昵称;
- **目标判定**:日志里的 `[at]`/`[reply]` 只说明"这条消息有指向",不说明指向谁
  (kovi 的 `Message::to_human_string` 对任何人的 @ 都渲染成 `[at]`,并明确写着
  不要靠它做判断)。运行时判定"指的是别人"时会打印「群聊消息指向其他成员,
  仅观察不回复 (群组: N, 用户: M)」,两处判定都带 `!addressed_to_bot`,所以它是
  只在"不是叫她"时才出现的**否定信号**:命中标记记 `targeting = "other_member"`;
  带 at/reply 段却没命中、而该群在日志里出现过标记的,反推为 `targeting = "her"`
  (在叫她/回她);一次标记都没出现过的群仍然丢弃不猜;
- 弱标签(pseudo_lexical_v0)来自高精度本地规则,只作候选,必须人工复核;
  lexical 无法判定的 completion 置 null;
- 不保存 QQ 号/昵称/URL/Token;source_key 为不透明哈希,供删除屏障使用;
- 默认不采集:纯本地工具,由开发者在服务器运行时显式执行。

用法:
    journalctl -u kovi-bot.service -o short-iso --since "2026-09-06 00:00:00" \
        > /tmp/tg-journal.txt
    python3 tools/turngate/collector.py --journal /tmp/tg-journal.txt \
        --out tools/turngate/dataset/review-batch-20260907.jsonl

    导出时**不要**只 grep `[group]`/`[send]`:目标判定还要读
    「群聊消息指向其他成员」标记行。整份日志直接喂给采集器即可,多余的行走
    匹配不到的分支,不会进样本。
"""

import argparse
import hashlib
import json
import re
import sys
from datetime import datetime
from pathlib import Path
from typing import Optional

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
# 运行时判定"这条 at/reply 指的是别人"时打印的标记行（Host 与 Core 两条链路
# 同一条文案）。日志本身只有 kovi 渲染的 `[at]`/`[reply]`，不带目标是谁——
# `Message::to_human_string` 对任何人的 @ 都渲染成 `[at]`，函数注释还写着
# "不要靠此函数做判断"。所以"在叫谁"只能靠这条标记还原。
AT_OTHER_RE = re.compile(r"群聊消息指向其他成员，仅观察不回复 \(群组: (\d+), 用户: (\d+)\)")
# 标记行与消息行的时间差容忍度：两条日志由同一次入站处理打印。
AT_OTHER_WINDOW_SECS = 2.0
# 第一行之后的续行(无 group 前缀)尽量匹配:任何不以 [ 开头的普通文本行。
BODY_CONTINUATION_RE = re.compile(r"^[^\[\]].+$")
# 带 syslog 前缀的行是**独立的日志记录**（`-o short-iso` 与 `-o short` 两种
# 导出格式），绝不可能是上一条消息的续行。漏掉这条判断时，日志里每一条
# INFO / Yunxi Mind / YUNXI_WORLD 行都会被当成续行粘进用户消息——实测
# 2000 条样本里 94% 的正文因此被污染。真正的续行只可能是同一 journald 记录里
# 带内嵌换行的正文，journalctl 对那种行不会再打前缀。
SYSLOG_PREFIX_RE = re.compile(
    r"^(?:\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:[+-]\d{2}:\d{2}|Z)"
    r"|[A-Z][a-z]{2} [ \d]\d \d{2}:\d{2}:\d{2}) \S+ \S+: "
)

URL_RE = re.compile(r"https?://\S+", re.I)
DIGITS_RE = re.compile(r"\d{8,}")


def parse_ts(raw: str) -> datetime:
    # 日志自带的年份前缀 2 位数字在现代 locale 中解析有歧义,固定按主题年。
    return datetime.strptime(raw, "%m-%d %H:%M:%S").replace(year=2026)

# journalctl -o short-iso: "2026-09-06T12:46:37+08:00 host kovi-bot[pid]: ..."
ISO_TS_RE = re.compile(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})")

def parse_syslog_ts(line: str) -> Optional[datetime]:
    """从 `[send]` 行提取时间。

    `[send]` 行**没有**内层时间戳，时间只能来自 journalctl 前缀，因此导出格式
    必须是带时间戳的那种：
      - `-o short-iso`: "2026-09-06T12:46:37+08:00 host kovi-bot[pid]: [send] ..."
      - `-o short`:     "Sep 06 12:46:37 host kovi-bot[pid]: [send] ..."

    以前这里对两种前缀都不匹配时静默返回 `datetime.now()`，而 README 里的示例
    命令恰好是 `-o cat`（**完全没有** syslog 前缀），于是所有 `[send]` 都被记成
    "现在"：assistant turns 与 conversation_active 全为 0，采出来的批次缺失
    机器人上下文却看不出任何异常。现在拿不到时间就返回 None，由调用方记账并在
    结束时报警，而不是悄悄产出一批废样本。
    """
    m = ISO_TS_RE.match(line)
    if m:
        year, month, day, hour, minute, second = (int(g) for g in m.groups())
        return datetime(year, month, day, hour, minute, second)
    m = re.match(r"^(\S+) (\d{2}) (\d{2}):(\d{2}):(\d{2})", line)
    if not m:
        return None
    month_map = {m: i for i, m in enumerate(
        "Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec".split(), start=1)}
    if m.group(1) not in month_map:
        return None
    return datetime(2026, month_map[m.group(1)], int(m.group(2)),
                    int(m.group(3)), int(m.group(4)), int(m.group(5)))


def sanitize(text: str) -> str:
    text = URL_RE.sub("<url>", text)
    text = DIGITS_RE.sub("<id>", text)
    return text


def label_speakers(
    turns: list[dict],
    senders: list[Optional[str]],
    current_sender: str,
) -> None:
    """就地在 `turns` 上写 `speaker`：样本内的匿名说话人编号。

    `role` 只分得清"芸汐 / 当前发言者 / 其他人"，于是一段上下文里的几个其他人
    全被标成"他人"——标注时看不出这是"一个人在连说三条"还是"三个人在互相接话"，
    而这恰恰是 completion/response 的关键线索（doc §7.4 B）。编号补上这一维。

    编号是**样本内**的，只回答"这几条是不是同一个人"，回答不了"这是谁"：

    - `a` 恒为当前发言者（样本正文 `current_text` 与 `pending_user_fragments`
      都属于他，所以它们不需要各自带编号）；
    - `b`/`c`/… 按 `turns` 的时间顺序给其他成员；
    - 芸汐（`assistant`）不编号。

    只落序号，不落 QQ 号、昵称或任何跨样本稳定的标识——脱敏承诺不变。
    """
    letters: dict[str, str] = {}
    for turn, sender in zip(turns, senders):
        if sender is None or turn.get("role") == "assistant":
            # 芸汐不编号：她不是"某个人"，标成说话人X只会让人以为群里多了一个成员。
            continue
        if sender == current_sender:
            turn["speaker"] = "a"
            continue
        if sender not in letters:
            # MAX_RECENT_TURNS 条上限决定了这里最多几个字母，chr 够用也够清楚。
            letters[sender] = chr(ord("b") + len(letters))
        turn["speaker"] = letters[sender]


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
    at_other_marks = []  # (ts, group, user) 运行时判定"at/reply 指的是别人"
    last = None
    untimed_sends = 0
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
                if ts is None:
                    # 导出格式没有时间戳（典型：journalctl -o cat）。这些行是
                    # 机器人上下文，丢了就只剩半张样本，必须显式记账。
                    untimed_sends += 1
                    continue
                events.append(("bot", int(m.group(1)), None, m.group(2), ts))
                last = ("bot", int(m.group(1)), None, ts)
                continue
            m = AT_OTHER_RE.search(line)
            if m:
                ts = parse_syslog_ts(line)
                if ts is not None:
                    at_other_marks.append((ts, int(m.group(1)), int(m.group(2))))
                continue
            # 续行:同组同发送者的多段消息。必须不是独立日志记录（见
            # SYSLOG_PREFIX_RE），否则会把别的日志行粘进消息正文。
            if (
                last
                and not SYSLOG_PREFIX_RE.match(line)
                and BODY_CONTINUATION_RE.match(line)
            ):
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
    if untimed_sends:
        print(f"warning: {untimed_sends} 条 [send] 行没有可解析的时间戳，已跳过；"
              f"这些是机器人上下文（assistant turns / conversation_active），"
              f"缺了样本只有半张。请用带时间戳的格式导出："
              f"journalctl ... -o short-iso", file=sys.stderr)

    # 按群分组切 unit: 同发送者间隔 ≤3s 为一次发言
    by_group: dict[int, list] = {}
    for ev in events:
        by_group.setdefault(ev[1], []).append(ev)

    # 出现过"指向其他成员"标记的群：只有这些群里，"没打标记"才能反证"在叫她"。
    marker_groups = {group for _, group, _ in at_other_marks}

    samples = []
    seen_keys = set()
    unresolved_targets = 0
    inferred_her = 0
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
            # 目标判定：`[at]`/`[reply]` 只是"有指向"，日志不带目标是谁。运行时
            # 判定"指的是别人"时会打印一条标记行，而两处判定都带
            # `!addressed_to_bot` / `!addressed_to_agent`（group.rs / bridge.rs），
            # 所以这条标记是**只在"不是叫她"时才出现的否定信号**：
            #
            # - 命中标记 → other_member：明确指向别人；
            # - 没命中、但该群在整份日志里出现过标记 → 标记机制在这个群是活的，
            #   而这条消息带着 at/reply 段却没被判成"别人"，那它就是在叫她本人
            #   （或是回她的消息）→ targeting = "her"；
            # - 一次标记都没有的群 → unresolved，仍然丢弃：那种日志可能来自旧
            #   版本，或者这条消息根本没走到判定点（群未授权、"等她发图"这类
            #   早退分支），没有证据就不猜。
            at_other = any(
                mark_group == group_id
                and mark_user == sender
                and 0 <= (mark_ts - ts).total_seconds() <= AT_OTHER_WINDOW_SECS
                for mark_ts, mark_group, mark_user in at_other_marks
            )
            if at_other:
                targeting = "other_member"
                has_at_flag = False
                has_reply_flag = False
            elif has_at_flag or has_reply_flag:
                targeting = "her" if group_id in marker_groups else "unresolved"
            else:
                targeting = "none"
            clean = sanitize(cur)
            # recent turns: 之前的 ≤4 个 user/bot unit
            recent = []
            # 与 recent 一一对应的发送者（bot 为 None）。只用来编说话人号，
            # 编完即弃——样本里只留序号，不留身份。
            recent_senders: list[Optional[str]] = []
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
                recent_senders.append(None if p_kind == "bot" else p_sender)
                if len(recent) >= MAX_RECENT_TURNS:
                    break
            recent.reverse()
            recent_senders.reverse()
            label_speakers(recent, recent_senders, sender)
            # conversation_active: 600s 内有 bot 发言或最近 turn
            recent_bot = any(r["role"] == "assistant" for r in recent)
            bot_recent = any(
                e[0] == "bot" and e[1] == group_id and 0 <= (ts - e[4]).total_seconds()
                <= CONTINUATION_WINDOW_SECS
                and e[4] <= ts
                for e in events
            )
            conversation_active = recent_bot or bot_recent

            if targeting == "unresolved":
                # 一次标记都没出现过的群：既可能是旧版本日志，也可能是这条没走到
                # 判定点。没有证据就不猜——猜错比少几条样本贵得多。
                unresolved_targets += 1
                continue
            if targeting == "her":
                inferred_her += 1
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
                # 目标解析结果（复核元数据，不进特征向量）：
                # none=消息不带 at/reply 段；other_member=运行时判定它指向别人
                # （只观察）；her=带 at/reply 段、但运行时没有判成"别人"，
                # 而该群的标记机制是活的 → 在叫她/回她。
                "targeting": targeting,
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
    n_assistant = sum(
        1 for s in samples
        if any(t.get("role") == "assistant" for t in s["context"]["recent_turns"])
    )
    print(
        f"wrote {args.out}: {len(samples)} samples "
        f"({n_pseudo} with lexical completion label, "
        f"{len(samples) - n_pseudo} gray-zone candidates)"
    )
    if unresolved_targets:
        # 不是错误，但要让人看见：这批日志里有多少条消息的"在叫谁"无从判断。
        # 剩下的只可能是"标记机制从没在这些群里出现过"的情况——那时无从反证，
        # 只能丢弃。
        print(
            f"dropped {unresolved_targets} samples with an unresolved at/reply "
            f"target (这些群在整份日志里一次「群聊消息指向其他成员」标记都没有)"
        )
    if inferred_her:
        # 反向推断出来的"在叫她"：标记只在"不是叫她"时打印，所以"带 at/reply
        # 却没打标记"就是她在被叫。数字给出来，方便和人工复核对账。
        print(
            f"inferred {inferred_her} samples as addressed to her "
            f"(带 at/reply 段且未命中「指向其他成员」标记，所在群的标记机制有效)"
        )
    # 机器人上下文覆盖率：0 说明 [send] 行没被解析进来（多半是导出格式的问题），
    # 这种批次只有半张样本，不该被当成可用数据。
    print(
        f"context coverage: {n_assistant}/{len(samples)} samples carry an "
        f"assistant turn"
    )
    if samples and n_assistant == 0:
        print("error: 没有任何样本带 assistant turn，批次缺少机器人上下文；"
              "请检查导出格式（需要 journalctl -o short-iso 或 -o short）",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
