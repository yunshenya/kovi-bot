#!/usr/bin/env python3
"""AV Host 登录自愈：空闲时每 60 秒补投一次登录参数（幂等）。

背景：AV Host 是承载 QQ 通话的一个独立 QQ 进程，它的登录态由插件从主 QQ 投递
（invokeAVHost(1, ...)）。上游只在插件启动时投一次，因此 AV Host 进程一旦重启
（崩溃、被守护脚本拉起、手动重启）就再也没有登录态——2026-09-11 晚上"所有来电
都到不了桥、打几个都不接"就是这个原因。

本补丁只做一件事：在插件里起一个 60 秒的定时器，只要当前没有进行中的通话，
就把登录参数补投一次；结果记在 state.avHost.loginRefresh* 里，可从 /v1/status 看。

不改任何白名单、不碰通话状态机。
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

REFRESH_TIMER = '''    if (!loginRefreshTimer) {
      loginRefreshTimer = setInterval(() => {
        void refreshAVHostLogin();
      }, 60000);
      loginRefreshTimer.unref?.();
    }
'''


def main() -> int:
    path = next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    if path is None:
        print("[fail] 找不到插件 index.mjs")
        return 1
    text = path.read_text(encoding="utf8")
    if MARKER in text:
        print(f"[skip] 登录自愈已在: {path}")
        return 0
    fn_anchor = "async function acceptActiveInvite() {"
    timer_anchor = "    scheduleAVHostLogin(ctx);\n"
    counts = (text.count(fn_anchor), text.count(timer_anchor))
    if counts != (1, 1):
        print(f"[fail] 锚点未匹配（fn/timer = {counts}）: {path}")
        return 1
    backup = path.with_name(path.name + ".kovi-orig")
    if not backup.exists():
        shutil.copy2(path, backup)
        print(f"[ok] 备份 {path} -> {backup.name}")
    text = text.replace(fn_anchor, REFRESH_FUNCTION + fn_anchor)
    text = text.replace(timer_anchor, timer_anchor + REFRESH_TIMER)
    path.write_text(text, encoding="utf8")
    print(f"[ok] 插件已加入 AV Host 登录自愈: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
