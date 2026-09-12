#!/usr/bin/env python3
"""在 AV Host 里记录"插件最近一条原始消息"的截断预览（幂等）。

为什么需要：AV Host 的日志是块缓冲的，最新几十秒的内容看不到；而它的 `/v1/status`
（6111，回环 + token）是实时的。把原始消息记进 `rendererState` 后，就能直接读到
AVSDK 对某个命令的**回复内容**——2026-09-12 定标 StartCall 时正是靠它读出了
`[3,"json parse error"]`，从而发现外呼命令要的是 JSON 而不是裸 uid。

只记录"最近一条"、且截断到 300 字符，仅暴露在本地带 token 的 /v1/status 上。
"""
from __future__ import annotations

import sys
from pathlib import Path

HOST_CANDIDATES = [
    Path("/app/qq-call/bridge/av-host/host.cjs"),
    Path("/home/ubuntu/napcat-qq-call/bridge/av-host/host.cjs"),
]

# 判据用这个字段名本身：手工加过同功能代码（没有标记注释）的现场也能识别为已就位。
MARKER = "lastRawValuePreview"
ANCHOR = """ipcMain.on("maibot-qq-call-avsdk-raw-message", (_event, message) => {
  void forwardPluginMessage(message);
});"""
PATCHED = """ipcMain.on("maibot-qq-call-avsdk-raw-message", (_event, message) => {
  // kovi-raw-preview-v1：记录最近一条原始消息的截断预览，用于确认 AVSDK 对某个命令
  // 的回复内容（错误码/说明）。只暴露在本地带 token 的 /v1/status 上。
  try {
    const encoded = JSON.stringify(message?.value ?? null);
    rendererState = {
      ...rendererState,
      lastRawCommand: Number.isInteger(message?.cmd) ? message.cmd : null,
      lastRawValuePreview: typeof encoded === "string" ? encoded.slice(0, 300) : null,
      lastRawAt: new Date().toISOString(),
    };
  } catch (error) {
    rendererState = { ...rendererState, lastRawError: String(error?.message ?? error) };
  }
  void forwardPluginMessage(message);
});"""


def readable_exists(path: Path) -> bool:
    """宿主上该文件是 root-only，ubuntu 直接 exists() 会抛 PermissionError。"""
    try:
        return path.exists()
    except OSError:
        return False


def main() -> int:
    explicit = [Path(argument) for argument in sys.argv[1:]]
    path = (
        explicit[0]
        if explicit
        else next((p for p in HOST_CANDIDATES if readable_exists(p)), None)
    )
    if path is None:
        print("[fail] 找不到 host.cjs（目录不可读时请用 sudo，或在容器内运行）")
        return 1
    text = path.read_text(encoding="utf8")
    if MARKER in text:
        print(f"[skip] 原始消息预览已在位: {path}")
        return 0
    if ANCHOR not in text:
        print(f"[fail] 锚点缺失（host.cjs 的 raw-message 转发口）: {path}")
        return 1
    updated = text.replace(ANCHOR, PATCHED, 1)
    # 写盘前自检：标记恰好一次，且转发函数仍在。
    for fragment, expected in ((MARKER, 1), ("forwardPluginMessage(message)", 1)):
        count = updated.count(fragment)
        if count != expected:
            print(f"[fail] 自检未通过：`{fragment}` 出现 {count} 次（应为 {expected}）")
            return 1
    path.write_text(updated, encoding="utf8")
    print(f"[ok] 已加入原始消息预览: {path}（{len(text)} → {len(updated)} 字节）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
