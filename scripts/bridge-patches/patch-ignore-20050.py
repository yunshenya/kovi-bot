#!/usr/bin/env python3
"""不要再把 AVSDK 的 20050/120043 当成「掉线需重登」。

实测证据（通过命令直方图 + 内容捕获得到）：
    cmd 1     -> [0, ""]       登录成功（0 = 成功）
    cmd 103   -> [1, ""]       状态通知
    cmd 20061 -> [[1,2], ["MaiBot_QQ_Speaker","MaiBot_QQ_Microphone_Feed"], ...]
                               音频设备变更上报（内容里就是我们桥的虚拟声卡）
    cmd 20050 -> 72 次         周期性通知，与登录成败无关
    cmd 120043-> 1 次

上游把 20050/120043 当作「会话结束、需要重新登录」，于是每隔 100ms 重登一次；
实测每个健康的会话都会被这次重登踢掉，会话永远无法稳定，cmd 55（内核数据转发）
也就永远得不到处理——来电因此永远停在 ringing。

本补丁只做一件事：20050/120043 只计数与记录，**不再触发重登**。
"""
import pathlib
import sys

PLUGIN = pathlib.Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs")

OLD = """  if (command === 20050 || command === 120043) {
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

NEW = """  if (command === 20050 || command === 120043) {
    // 实测登录成功的返回值是 cmd 1 -> [0, ""]，20050 只是周期性通知，与登录成败
    // 无关。上游把它当成掉线并立刻重登，会把刚建立的健康会话反复踢掉，导致
    // cmd 55 永远得不到处理。这里只计数，不重登。
    state.avHost.sessionEndCount += 1;
    if (state.avHost.sessionEndCount === 1) {
      logger?.info(
        "[MaiBotQQCall] 收到 AVSDK 20050/120043：按实测语义视为普通通知，不触发重登",
      );
    }
  }"""


def main() -> int:
    if not PLUGIN.is_file():
        print(f"[patch] 找不到插件 {PLUGIN}", file=sys.stderr)
        return 1
    src = PLUGIN.read_text(encoding="utf-8")
    if "按实测语义视为普通通知" in src:
        print("[patch] 忽略 20050 补丁已在位")
        return 0
    if OLD not in src:
        print("[patch] 未找到锚点（需先应用 20050 退避补丁）", file=sys.stderr)
        return 2
    PLUGIN.write_text(src.replace(OLD, NEW, 1), encoding="utf-8")
    print("[patch] 已改为忽略 20050/120043，不再触发重登")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
