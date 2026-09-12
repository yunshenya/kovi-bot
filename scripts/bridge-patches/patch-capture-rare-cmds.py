#!/usr/bin/env python3
"""记录非 20050 命令的原始内容，用于确认这套 QQ 构建的命令编号。

直方图显示 AVSDK 只回 4 种命令：20050 占绝大多数，另有 cmd 1 / 103 / 20061 各 2 次。
其中 20061 与桥硬编码等待的接听回调 20006 只差一位，需要用内容来确认。
"""
import pathlib
import sys

PLUGIN = pathlib.Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs")

OLD = """  state.avHost.commandHistogram[command] =
    (state.avHost.commandHistogram[command] || 0) + 1;"""

NEW = """  state.avHost.commandHistogram[command] =
    (state.avHost.commandHistogram[command] || 0) + 1;
  if (command !== 20050 && command !== 120043) {
    state.avHost.rareOutputs = state.avHost.rareOutputs || [];
    state.avHost.rareOutputs.push({
      at: new Date().toISOString(),
      command,
      value,
    });
    if (state.avHost.rareOutputs.length > 12) state.avHost.rareOutputs.shift();
    logger?.info(
      `[MaiBotQQCall] 罕见回传 cmd=${command} 内容长度=${
        typeof value === "string" ? value.length : JSON.stringify(value ?? null).length
      }`,
    );
  }"""


def main() -> int:
    if not PLUGIN.is_file():
        print(f"[patch] 找不到插件 {PLUGIN}", file=sys.stderr)
        return 1
    src = PLUGIN.read_text(encoding="utf-8")
    if "rareOutputs" in src:
        print("[patch] 罕见命令捕获已在位")
        return 0
    if OLD not in src:
        print("[patch] 未找到锚点（先应用 20050 补丁）", file=sys.stderr)
        return 2
    PLUGIN.write_text(src.replace(OLD, NEW, 1), encoding="utf-8")
    print("[patch] 已加入罕见命令内容捕获")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
