#!/usr/bin/env python3
"""AV Host 登录自愈（幂等，可重复执行）。

背景：AV Host 是承载 QQ 通话的独立进程，它的登录态由插件投递
（`invokeAVHost(1, ...)`）。上游只在插件启动时投一次，所以 AV Host 进程一旦重启
（崩溃、被守护脚本拉起、手动重启）就再也没有登录态——2026-09-11 晚上"所有来电都
到不了桥、打几个都不接"就是这个原因。

**2026-09-12 修正**：最初的自愈是"空闲时每 60 秒重投一次"。一天上千次重投会让
QQ 侧的通话设备注册被反复重置——实测每次投递后 AVSDK 只有十几秒有心跳，之后安静
到下一次投递，而当天的现象正是"来电响在别处、桥这侧收不到任何信令"。所以改成
**只在 AV Host 真的没动静时才补投**：插件在处理 AV Host 输出时打一个
`lastOutputAt` 时间戳，距现在不足 5 分钟就不打扰它。

本补丁只确保四件事存在（不覆盖、不重写已有代码）：
  1. `idleAVHost()` 里有 `lastOutputAt` 字段；
  2. `handleAVSDKOutput()` 每次收到 AV Host 输出就打时间戳；
  3. `refreshAVHostLogin()` 开头有"最近 5 分钟有输出就跳过"的判断；
  4. 启动时挂 60 秒的定时器调用它。
"""
from __future__ import annotations

import shutil
import sys
from pathlib import Path

PLUGIN_CANDIDATES = [
    Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
    Path("/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
]

MARKER = "loginRefreshTimer"

# 1) idleAVHost() 增加时间戳字段
IDLE_ANCHOR = "    loginPosted: false,\n"
IDLE_ADD = "    lastOutputAt: null,\n"

# 2) 每次 AV Host 输出打时间戳
HANDLE_ANCHOR = "async function handleAVSDKOutput(body) {\n"
HANDLE_ADD = "  state.avHost.lastOutputAt = new Date().toISOString();\n"

# 3) 刷新函数：只在 AV Host 安静够久时才投
QUIET_CONST = """/// AV Host 安静多久算"它已经没在跑"。低于这个时间就不打扰它：无脑重投会让 QQ 侧
/// 的设备/通话注册被反复重置（实测每次投递后 AVSDK 只有十几秒心跳）。
const LOGIN_REFRESH_QUIET_MS = 5 * 60 * 1000;

"""

REFRESH_FUNCTION = '''let loginRefreshTimer = null;

async function refreshAVHostLogin() {
  if (!pluginContext) return;
  const phase = state.call?.phase;
  if (
    phase === "ringing" ||
    phase === "accepting" ||
    phase === "accepted" ||
    phase === "connected"
  ) {
    return;
  }
  try {
    const selfUid = String(pluginContext.core?.selfInfo?.uid ?? "");
    const selfUin = String(pluginContext.core?.selfInfo?.uin ?? "");
    const accountPath = String(
      kernelSession?.getAccountPath?.(Number.parseInt(selfUin, 10)) ||
        resolveAccountPath(pluginContext.core?.dataPath, selfUin) ||
        pluginContext.core?.dataPath ||
        "",
    );
    if (!selfUid || !selfUin || !accountPath) return;
    await invokeAVHost(1, [selfUid, selfUin, selfUin, accountPath, ""]);
    state.avHost.loginPosted = true;
    state.avHost.loginRefreshAt = new Date().toISOString();
    state.avHost.loginRefreshCount = (state.avHost.loginRefreshCount || 0) + 1;
    state.avHost.lastError = null;
  } catch (error) {
    state.avHost.loginRefreshError = String(error?.message ?? error);
  }
}

'''

QUIET_GUARD = """  // AV Host 最近还在说话就不重复投登录：每 60 秒无脑重投会把 QQ 侧的通话设备
  // 注册反复重置（2026-09-12 早上"来电响在别处"的现场就是这么来的）。
  const lastOutputAt = state.avHost.lastOutputAt
    ? Date.parse(state.avHost.lastOutputAt)
    : Number.NaN;
  if (Number.isFinite(lastOutputAt) && Date.now() - lastOutputAt < LOGIN_REFRESH_QUIET_MS) {
    state.avHost.loginRefreshSkipped = (state.avHost.loginRefreshSkipped || 0) + 1;
    return;
  }
"""

REFRESH_TIMER = """    if (!loginRefreshTimer) {
      loginRefreshTimer = setInterval(() => {
        void refreshAVHostLogin();
      }, 60000);
      loginRefreshTimer.unref?.();
    }
"""


def ensure(text: str, anchor: str, addition: str, label: str, path: Path) -> tuple[str, bool]:
    """确保 addition 出现在 anchor 之后；已经有了就跳过。"""
    if addition in text:
        print(f"[skip] {label} 已在位: {path}")
        return text, False
    if anchor not in text:
        print(f"[fail] 锚点缺失（{label}）: {path}")
        return text, False
    print(f"[ok] {label}: {path}")
    return text.replace(anchor, anchor + addition, 1), True


def main() -> int:
    path = next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    if path is None:
        print("[fail] 找不到插件 index.mjs")
        return 1
    text = path.read_text(encoding="utf8")
    backup = path.with_name(path.name + ".kovi-orig")
    if not backup.exists():
        shutil.copy2(path, backup)
        print(f"[ok] 备份 {path} -> {backup.name}")

    changed = False
    text, did = ensure(text, IDLE_ANCHOR, IDLE_ADD, "idleAVHost 时间戳字段", path)
    changed |= did
    text, did = ensure(text, HANDLE_ANCHOR, HANDLE_ADD, "handleAVSDKOutput 打时间戳", path)
    changed |= did

    if MARKER not in text:
        fn_anchor = "async function acceptActiveInvite() {"
        if text.count(fn_anchor) != 1:
            print(f"[fail] 找不到 acceptActiveInvite: {path}")
            return 1
        text = text.replace(fn_anchor, QUIET_CONST + REFRESH_FUNCTION + fn_anchor, 1)
        print(f"[ok] 加入登录自愈: {path}")
        changed = True
    else:
        # 老版本自愈没有安静期常量：先补上，否则判断会引用未定义的常量。
        if "const LOGIN_REFRESH_QUIET_MS" not in text:
            fn_anchor = "async function refreshAVHostLogin() {"
            if text.count(fn_anchor) != 1:
                print(f"[fail] 找不到 refreshAVHostLogin: {path}")
                return 1
            text = text.replace(fn_anchor, QUIET_CONST + fn_anchor, 1)
            print(f"[ok] 补上安静期常量: {path}")
            changed = True
        text, did = ensure(
            text,
            "async function refreshAVHostLogin() {\n  if (!pluginContext) return;\n",
            QUIET_GUARD,
            "安静期跳过判断",
            path,
        )
        changed |= did

    timer_anchor = "    scheduleAVHostLogin(ctx);\n"
    if "loginRefreshTimer = setInterval" not in text:
        if text.count(timer_anchor) != 1:
            print(f"[fail] 找不到 scheduleAVHostLogin 调用点: {path}")
            return 1
        text = text.replace(timer_anchor, timer_anchor + REFRESH_TIMER, 1)
        print(f"[ok] 挂上 60 秒定时器: {path}")
        changed = True

    if changed:
        path.write_text(text, encoding="utf8")
    else:
        print(f"[skip] 登录自愈已是最新: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
