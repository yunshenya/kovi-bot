#!/usr/bin/env python3
"""来电回调缺失时的有限重投（幂等）。

链路是：来电信令 → 主 QQ 的 AVSDK 监听（`OnInviteActionToAVSDK`，带一份 860 字节的
不透明 payload）→ 插件把它 forward 给 AV Host 的 AVSDK（cmd 55 → `OnPenetrateEvent`）
→ **AV Host 回报 20006**（解析后的接听元组）→ 插件据此发 `Accept`(cmd 5) → 进房。

2026-09-12 真机对照：一通 `20006` 到了 → 正常接听；另一通没到 → 电话一直响到对方
放弃（`5`/`20004` 计数都是 0）。监听那条路的邀请对象只有 `relation_id`/`invite_type`/
`from_uid`（`from_uid` 还是空的），拼不出 Accept 需要的 12 元组，所以只能在 20006
这一环做补救：**收到邀请后起 1.5 秒看门狗，期间没等到 20006 就把 payload 重投一次，
最多再投 2 次**，然后放弃并打一条明确的 WARN。

可观测：`/v1/status` 里 `avHost.acceptRetryCount`（重投次数）、`acceptRetryGaveUp`
（重投后仍失败次数）。
"""
from __future__ import annotations

import sys
from pathlib import Path

PLUGIN_CANDIDATES = [
    Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
    Path("/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
]

MARKERS = {
    "funcs": "kovi-accept-retry-funcs-v1",
    "hook": "kovi-accept-retry-hook-v1",
    "clear": "kovi-accept-retry-clear-v1",
}
LEGACY = ("kovi-accept-retry-v0",)

FUNCS_ANCHOR = "function forwardKernelAction(name, args) {"
FUNCS = '''// kovi-accept-retry-funcs-v1
/// 来电重投看门狗：邀请 payload 转给 AV Host 的 AVSDK 后，正常情况下它会回报 20006
/// （解析后的接听元组），接听才有参数可用；实测它偶尔不来（来电被路由到另一台设备
/// 时），此时电话会一直响到对方放弃。这里做有限重投，并在彻底失败时留下明确日志。
const ACCEPT_RETRY_DELAY_MS = 1500;
const MAX_ACCEPT_RETRIES = 2;
let acceptWatchdogTimer = null;
let acceptWatchdogAttempt = 0;
let acceptCallbackGeneration = 0;

function clearAcceptWatchdog() {
  if (acceptWatchdogTimer) clearTimeout(acceptWatchdogTimer);
  acceptWatchdogTimer = null;
  acceptWatchdogAttempt = 0;
}

function watchAcceptCallback(actionType, payload) {
  clearAcceptWatchdog();
  const generation = acceptCallbackGeneration;
  const schedule = (delayMs) => {
    acceptWatchdogTimer = setTimeout(() => {
      acceptWatchdogTimer = null;
      if (acceptCallbackGeneration !== generation) return; // 20006 已经来了
      if (acceptWatchdogAttempt >= MAX_ACCEPT_RETRIES) {
        state.avHost.acceptRetryGaveUp = (state.avHost.acceptRetryGaveUp || 0) + 1;
        logger?.warn(
          "[MaiBotQQCall] 来电回调 20006 未到，重投后仍无法接听（来电可能被路由到另一台设备）",
        );
        return;
      }
      acceptWatchdogAttempt += 1;
      state.avHost.acceptRetryCount = (state.avHost.acceptRetryCount || 0) + 1;
      void invokeAVHost(55, [actionType, payload]).catch((error) => {
        state.avHost.lastError = `accept retry forward failed: ${error?.message ?? String(error)}`;
      });
      schedule(ACCEPT_RETRY_DELAY_MS);
    }, delayMs);
    acceptWatchdogTimer.unref?.();
  };
  schedule(ACCEPT_RETRY_DELAY_MS);
}

'''

HOOK_ANCHOR = '      if (name.toLowerCase() === "onactiontoavsdk" && activeSDKInvite) scheduleAccept(75);\n'
HOOK = """      // kovi-accept-retry-hook-v1：邀请已转给 AV Host 的 AVSDK，等它回报 20006；
      // 没等到就有限重投（见 watchAcceptCallback）。
      if (name.toLowerCase() === "oninviteactiontoavsdk") {
        watchAcceptCallback(actionType, payload);
      }
"""

CLEAR_ANCHOR = "    state.avHost.inviteCallbackSeen = true;\n"
CLEAR = """    // kovi-accept-retry-clear-v1：回调到了，撤销重投看门狗。
    acceptCallbackGeneration += 1;
    clearAcceptWatchdog();
"""

EXPECTED_ONCE = (
    MARKERS["funcs"],
    MARKERS["hook"],
    MARKERS["clear"],
    "function watchAcceptCallback(",
    "function clearAcceptWatchdog(",
)
NEVER_TWICE = (
    "const HANGUP_METHODS",
    "async function dialCall(",
    "async function refreshAVHostLogin(",
    "kovi-trace-record-v1",
)


def insert(text, anchor, addition, label, marker, before=False):
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
    replacement = addition + anchor if before else anchor + addition
    print(f"[ok] {label}")
    return text.replace(anchor, replacement, 1), True


def main() -> int:
    explicit = [Path(argument) for argument in sys.argv[1:]]
    plugin = (
        explicit[1] if len(explicit) > 1 else next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    )
    if plugin is None or not plugin.exists():
        print("[fail] 找不到插件 index.mjs")
        return 1

    text = original = plugin.read_text(encoding="utf8")
    text, c1 = insert(text, FUNCS_ANCHOR, FUNCS, "重投看门狗实现", MARKERS["funcs"], before=True)
    text, c2 = insert(text, HOOK_ANCHOR, HOOK, "邀请转投后挂看门狗", MARKERS["hook"], before=True)
    text, c3 = insert(text, CLEAR_ANCHOR, CLEAR, "20006 到达时撤销看门狗", MARKERS["clear"])
    if not (c1 or c2 or c3):
        print("[skip] 重投补丁已是最新")
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
    print(f"[ok] 重投补丁已写入: {plugin}（{len(original)} → {len(text)} 字节）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
