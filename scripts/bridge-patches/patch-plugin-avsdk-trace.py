#!/usr/bin/env python3
"""记录 AVSDK 输出来龙去脉（只观测，不改行为；幂等）。

背景：接听依赖 AV Host 回报的 `20006`（它的 AVSDK 收到邀请），但 2026-09-12 实测
来电时 `20006` 根本没来——只有主 QQ 的监听看到邀请，于是桥一直响到对方放弃。
直方图显示 AV Host 还会发 `20013/20022/20023/20037/20038/20043/20046/20061` 等
命令，但插件从不处理它们，邀请很可能藏在其中之一（或上游假设的 20006 只在某些
部署/版本里出现）。

本补丁只把"最近 20 条非心跳输出"记进 `state.avHost.outputTrail`（命令号 + 值的
类型/长度摘要，不记原始内容），暴露在 `GET /v1/status`。拿到真实来电时的轨迹，
再决定接听该挂在哪条命令上。
"""
from __future__ import annotations

import sys
from pathlib import Path

PLUGIN_CANDIDATES = [
    Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
    Path("/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
]

MARKERS = {"idle": "kovi-trace-idle-v1", "record": "kovi-trace-record-v1"}
LEGACY = ("kovi-trace-v0",)

IDLE_ANCHOR = "    loginPosted: false,\n"
IDLE_ADD = """    // kovi-trace-idle-v1
    outputTrail: [],
"""

RECORD_ANCHOR = "  const value = body?.value;\n"
RECORD_ADD = """  // kovi-trace-record-v1：心跳之外的输出各留一条摘要（类型/长度，不含原始内容）。
  if (command !== 20050 && command !== 120043) {
    state.avHost.outputTrail = [
      ...(state.avHost.outputTrail || []).slice(-19),
      { at: new Date().toISOString(), command, value: summarizeValue(value ?? null) },
    ];
  }
"""

# outputTrail 本来就该出现两次（idleAVHost 初始化 + handleAVSDKOutput 追加），
# 所以只校验标记与函数头各一次。
EXPECTED_ONCE = (
    MARKERS["idle"],
    MARKERS["record"],
    "async function handleAVSDKOutput(",
)
NEVER_TWICE = (
    "const HANGUP_METHODS",
    "async function dialCall(",
    "async function refreshAVHostLogin(",
)


def insert(text, anchor, addition, label, marker):
    if marker in text:
        print(f"[skip] {label} 已是最新（{marker}）")
        return text, False
    for legacy in LEGACY:
        if legacy in text:
            print(f"[fail] {label}: 发现旧标记 {legacy}")
            return text, False
    if anchor not in text:
        print(f"[fail] {label}: 找不到锚点")
        return text, False
    print(f"[ok] {label}")
    return text.replace(anchor, anchor + addition, 1), True


def main() -> int:
    explicit = [Path(argument) for argument in sys.argv[1:]]
    plugin = (
        explicit[1] if len(explicit) > 1 else next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    )
    if plugin is None or not plugin.exists():
        print("[fail] 找不到插件 index.mjs")
        return 1

    text = original = plugin.read_text(encoding="utf8")
    text, changed_idle = insert(text, IDLE_ANCHOR, IDLE_ADD, "idleAVHost 增加 outputTrail", MARKERS["idle"])
    text, changed_record = insert(text, RECORD_ANCHOR, RECORD_ADD, "handleAVSDKOutput 记录轨迹", MARKERS["record"])
    if not (changed_idle or changed_record):
        print("[skip] 追踪补丁已是最新")
        return 0
    for fragment in EXPECTED_ONCE:
        count = text.count(fragment)
        if count != 1:
            print(f"[fail] 自检未通过：`{fragment}` 出现 {count} 次（应为 1），已放弃写入")
            return 1
    for fragment in NEVER_TWICE:
        count = text.count(fragment)
        if count > 1:
            print(f"[fail] 自检未通过：`{fragment}` 出现 {count} 次（最多 1 次），已放弃写入")
            return 1
    plugin.write_text(text, encoding="utf8")
    print(f"[ok] 追踪补丁已写入: {plugin}（{len(original)} → {len(text)} 字节）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
