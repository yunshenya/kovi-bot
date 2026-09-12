#!/usr/bin/env python3
"""诊断 20050：记录 AVSDK 命令直方图，并把「立刻重登」改成退避重登。

背景：上游在收到 20050/120043 后用固定 100ms 立刻重登。实测每次登录 AVSDK 都回
约 32 条消息、末条恒为 20050，于是形成 500+ 次循环（16594/521≈31.9）。如果 20050
的真实语义是「上一个会话被顶掉、你现在是新会话」，那么插件的行为就是刚建立的会话
被自己的重登反复踢掉，会话永远不稳定，cmd 55 也就永远得不到处理。

这个补丁做两件事，用来验证上面的假设：
  1. 记录每个 cmd 的出现次数（暴露在 /v1/status 的 avHost.commandHistogram）；
  2. 把立刻重登改为指数退避，并在若干次后停止重试，给会话来一段不受打扰的稳定期。

幂等：已打过直接返回。可逆：首次修改前备份为 index.mjs.pre20050。
"""
import pathlib
import sys

PLUGIN = pathlib.Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs")
BACKUP = PLUGIN.with_name("index.mjs.pre20050")

OLD_STATE = """  return {
    loginPosted: false,
    kernelActionCount: 0,"""

NEW_STATE = """  return {
    loginPosted: false,
    // 诊断用：AVSDK 回传的各 cmd 出现次数与 20050 次数。
    commandHistogram: {},
    sessionEndCount: 0,
    kernelActionCount: 0,"""

OLD_COUNT = """  state.avHost.outputCount += 1;
  state.avHost.lastOutputCommand = command;"""

NEW_COUNT = """  state.avHost.outputCount += 1;
  state.avHost.lastOutputCommand = command;
  state.avHost.commandHistogram[command] =
    (state.avHost.commandHistogram[command] || 0) + 1;"""

OLD_20050 = """  if (
    (command === 20050 || command === 120043) &&
    state.avHost.loginPosted &&
    pluginContext
  ) {
    state.avHost.loginPosted = false;
    scheduleAVHostLogin(pluginContext, 100);
  }"""

NEW_20050 = """  if (command === 20050 || command === 120043) {
    state.avHost.sessionEndCount += 1;
    if (state.avHost.loginPosted && pluginContext) {
      state.avHost.loginPosted = false;
      const attempt = state.avHost.sessionEndCount;
      if (attempt > 10) {
        // 连续 10 次都被顶掉，说明问题不在重登时机。停在这里，让会话保持一段
        // 不受打扰的稳定期，用来判断 cmd 55 是否能在稳定会话下被处理。
        if (attempt === 11) {
          logger?.warn(
            "[MaiBotQQCall] AVSDK 连续 10 次报告会话结束，停止重登以免继续自我踢下线",
          );
        }
      } else {
        const delay = Math.min(60000, 1000 * 2 ** (attempt - 1));
        logger?.warn(
          `[MaiBotQQCall] AVSDK 会话结束(20050/120043) 第 ${attempt} 次，${delay}ms 后退避重登`,
        );
        scheduleAVHostLogin(pluginContext, delay);
      }
    }
  }"""


def main() -> int:
    if not PLUGIN.is_file():
        print(f"[patch] 找不到插件 {PLUGIN}", file=sys.stderr)
        return 1
    src = PLUGIN.read_text(encoding="utf-8")
    if "commandHistogram" in src:
        print("[patch] 20050 退避补丁已在位")
        return 0
    for old, label in ((OLD_STATE, "idleAVHost"), (OLD_COUNT, "计数"), (OLD_20050, "20050 处理")):
        if old not in src:
            print(f"[patch] 未找到锚点：{label}（插件版本可能变了）", file=sys.stderr)
            return 2
    if not BACKUP.exists():
        BACKUP.write_text(src, encoding="utf-8")
        print(f"[patch] 已备份到 {BACKUP.name}")
    src = src.replace(OLD_STATE, NEW_STATE, 1)
    src = src.replace(OLD_COUNT, NEW_COUNT, 1)
    src = src.replace(OLD_20050, NEW_20050, 1)
    PLUGIN.write_text(src, encoding="utf-8")
    print("[patch] 已加入命令直方图，并把 20050 改为退避重登")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
