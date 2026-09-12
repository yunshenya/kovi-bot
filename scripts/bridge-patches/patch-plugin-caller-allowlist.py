#!/usr/bin/env python3
"""来电授权名单：插件在接听前查一份机器人维护的名单，名单外不接（幂等）。

背景：上游桥对任何来电都自动接听，接通后我们只能播一句婉拒再静音，通话会一直
留在 connected（2026-09-11 晚上卡了一整夜，机器人每次重启都重新起一个幽灵会话）。
客户端侧无法主动挂断（已穷举验证，见 docs/qq-call.md），所以正确的做法是：
**名单外直接不接**，让来电自然结束。

名单文件由机器人写（`#授权通话` / `#授权管理员` 变更时以及启动时刷新）：

  /app/qq-call/bridge/runtime/allowed-callers.json
  {"callers": [123456, 789012], "updatedAt": "2026-09-12T..."}

字段是"有效通话授权"的并集：授权名单 ∪ 副管理员 ∪ 主管理员（机器人侧负责并集）。

文件名可用环境变量 `MAIBOT_QQ_CALL_ALLOWLIST_FILE` 覆盖。文件读不到时**按原行为
接听**（fail-open，避免机器人没起来时谁都打不进来），并把原因记进
`state.avHost.autoAcceptSkipped`，可从 `/v1/status` 看到。
"""
from __future__ import annotations

import shutil
import sys
from pathlib import Path

PLUGIN_CANDIDATES = [
    Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
    Path("/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
]

MARKER = "loadAllowedCallers"

HELPER = '''const CALLER_ALLOWLIST_FILE =
  process.env.MAIBOT_QQ_CALL_ALLOWLIST_FILE ||
  "/app/qq-call/bridge/runtime/allowed-callers.json";

function loadAllowedCallers() {
  try {
    const raw = fs.readFileSync(CALLER_ALLOWLIST_FILE, "utf8");
    const parsed = JSON.parse(raw);
    const list = Array.isArray(parsed?.callers) ? parsed.callers : [];
    const numbers = new Set();
    for (const value of list) {
      const parsedValue = Number.parseInt(String(value), 10);
      if (Number.isInteger(parsedValue) && parsedValue > 0) numbers.add(parsedValue);
    }
    // 默认关闭：只有机器人显式写 enabled=true 时才按名单拦截，否则保持上游
    // "接听任何来电、名单外播报婉拒"的行为。
    if (parsed?.enabled !== true) {
      return { numbers: null, updatedAt: parsed?.updatedAt ?? null, error: null, disabled: true };
    }
    return { numbers, updatedAt: parsed?.updatedAt ?? null, error: null };
  } catch (error) {
    return {
      numbers: null,
      updatedAt: null,
      error: String((error && error.message) || error),
    };
  }
}

'''

OLD_ACCEPT = '''async function acceptActiveInvite() {
  if (!Array.isArray(activeSDKInvite) || state.call.phase === "ended") return;
  const inviteAt = state.call.inviteAt;
  if (!inviteAt || state.avHost.autoAcceptInviteAt === inviteAt) return;
  state.avHost.autoAcceptInviteAt = inviteAt;
  state.avHost.autoAcceptAttemptedAt = new Date().toISOString();
  state.call.phase = "accepting";
  await invokeAVHost(5, buildAcceptParams(activeSDKInvite));
  state.avHost.autoAcceptPostedAt = new Date().toISOString();
}
'''

NEW_ACCEPT = '''async function acceptActiveInvite() {
  if (!Array.isArray(activeSDKInvite) || state.call.phase === "ended") return;
  const inviteAt = state.call.inviteAt;
  if (!inviteAt || state.avHost.autoAcceptInviteAt === inviteAt) return;
  const allow = loadAllowedCallers();
  if (allow.numbers) {
    const callerUin = Number.parseInt(String(state.call?.callerUin ?? ""), 10);
    if (!Number.isInteger(callerUin)) {
      const retries = (state.avHost.autoAcceptIdentityRetries || 0) + 1;
      state.avHost.autoAcceptIdentityRetries = retries;
      if (retries <= 8) {
        scheduleAccept(250);
        return;
      }
      state.avHost.autoAcceptSkipped = {
        at: new Date().toISOString(),
        reason: "identity-unresolved",
      };
      logger?.info("[MaiBotQQCall] 来电者身份未能解析，按不在名单处理：不接听");
      return;
    }
    state.avHost.autoAcceptIdentityRetries = 0;
    if (!allow.numbers.has(callerUin)) {
      state.avHost.autoAcceptSkipped = {
        at: new Date().toISOString(),
        callerUin,
        reason: "not-in-allowlist",
        allowlistUpdatedAt: allow.updatedAt,
      };
      logger?.info(
        `[MaiBotQQCall] 来电者 ${callerUin} 不在通话授权名单（共 ${allow.numbers.size} 人）：不接听`,
      );
      return;
    }
    state.avHost.autoAcceptSkipped = null;
  } else {
    state.avHost.autoAcceptSkipped = {
      at: new Date().toISOString(),
      reason: `allowlist-unavailable: ${allow.error}`,
    };
  }
  state.avHost.autoAcceptInviteAt = inviteAt;
  state.avHost.autoAcceptAttemptedAt = new Date().toISOString();
  state.call.phase = "accepting";
  await invokeAVHost(5, buildAcceptParams(activeSDKInvite));
  state.avHost.autoAcceptPostedAt = new Date().toISOString();
}
'''


def main() -> int:
    path = next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    if path is None:
        print("[fail] 找不到插件 index.mjs")
        return 1
    text = path.read_text(encoding="utf8")
    if MARKER in text:
        print(f"[skip] 名单门控已在: {path}")
        return 0
    fn_anchor = "async function acceptActiveInvite() {"
    if text.count(OLD_ACCEPT) != 1 or text.count(fn_anchor) != 1:
        print(
            f"[fail] 锚点未匹配（accept {text.count(OLD_ACCEPT)} / fn {text.count(fn_anchor)}）: {path}"
        )
        return 1
    backup = path.with_name(path.name + ".kovi-orig")
    if not backup.exists():
        shutil.copy2(path, backup)
        print(f"[ok] 备份 {path} -> {backup.name}")
    text = text.replace(fn_anchor, HELPER + fn_anchor, 1)
    text = text.replace(OLD_ACCEPT, NEW_ACCEPT, 1)
    path.write_text(text, encoding="utf8")
    print(f"[ok] 插件已按授权名单决定是否接听: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
