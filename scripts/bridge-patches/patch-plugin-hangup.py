#!/usr/bin/env python3
"""给 AV 桥补上"主动挂断"能力（幂等，可重复执行）。

背景：`libAVSDKPlugin.so` 是 PPAPI 插件，页面用 `plugin.postMessage({cmd, id, param})`
调它；`PPP_InitializeModule` 把消息回调注册成 `CallCpp`，而 **`cmd` 就是
`QRTCServiceInterfaceWrapper` 的方法表下标**：

  cmd 1  = `Login`            上游桥在用
  cmd 5  = `Accept`           上游桥在用（自动接听）
  cmd 10 = `Close`            实测唯一能真正结束通话的方法
  cmd 55 = `OnPenetrateEvent` 上游桥的 kernel-forward

上游桥的白名单只有 `{1, 5, 55}`，所以"机器人挂不了电话"从来不是没有 API。真机逐通
试过 `Quit`(8)、`ClearRoom`(11)、uid 留空、uid 传机器人自己等组合，只有
**`Close` + 来电者 uid** 能结束通话（详见仓库 `docs/qq-call.md` 的「挂断参数实验」）。

本补丁只做"把文件归一化成目标状态"这一件事，不保留历史迁移规则：

1. AV Host 的 cmd 白名单 → `{1, 5, 10, 55}`。不管是上游原版 `{1,5,55}`，还是实验
   期间留下的 `{1,5,8,9,10,11,55}`、`{1,5,10,11,55}`，都统一改写；
2. 插件 `index.mjs`：`invokeAVHost` 返回 AV Host 的响应体、`state.call` 记录挂断
   与来电元组、新增 `POST /v1/calls/hangup`（方法表只留 `close`）。
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

HOST_CANDIDATES = [Path(item) for item in ["/app/qq-call/bridge/av-host/host.cjs", "/home/ubuntu/napcat-qq-call/bridge/av-host/host.cjs"]]
PLUGIN_CANDIDATES = [Path(item) for item in ["/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs", "/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"]]

# AV Host：白名单整行归一化（认得出任何一版 Set 内容）。
ALLOWED_LINE = re.compile(r"const ALLOWED_COMMANDS = new Set\(\[[0-9, ]*\]\);")
ALLOWED_TARGET = "const ALLOWED_COMMANDS = new Set([1, 5, 10, 55]);"
# 插件：方法表整行归一化。
METHODS_LINE = re.compile(r"const HANGUP_METHODS = \{[^}]*\};")
METHODS_TARGET = "const HANGUP_METHODS = { close: 10 };"
# 实验期间默认方法一度是 quit，归一到 close。
DEFAULT_METHOD_OLD = 'body.method : "quit";'
DEFAULT_METHOD_NEW = 'body.method : "close";'

# 主体补丁是否已经打上（插件里出现方法表即视为已打）。
MARKER = 'HANGUP_METHODS'


def normalize_host(path: Path) -> bool:
    text = path.read_text(encoding="utf8")
    if not ALLOWED_LINE.search(text):
        print(f"[fail] 找不到 cmd 白名单: {path}")
        return False
    updated = ALLOWED_LINE.sub(ALLOWED_TARGET, text, count=1)
    if updated == text:
        print(f"[skip] AV Host 白名单已是目标值: {path}")
        return False
    path.write_text(updated, encoding="utf8")
    print(f"[ok] AV Host 白名单归一化为 {{1, 5, 10, 55}}: {path}")
    return True


def normalize_plugin(path: Path, text: str) -> tuple[str, bool]:
    """把已打补丁的插件归一化到目标状态，返回新内容与是否有改动。"""
    changed = False
    updated = METHODS_LINE.sub(METHODS_TARGET, text, count=1)
    if updated != text:
        changed = True
        text = updated
    if DEFAULT_METHOD_OLD in text:
        text = text.replace(DEFAULT_METHOD_OLD, DEFAULT_METHOD_NEW, 1)
        changed = True
    return text, changed


def patch_plugin(path: Path) -> bool:
    text = path.read_text(encoding="utf8")
    if MARKER in text:
        text, changed = normalize_plugin(path, text)
        if changed:
            path.write_text(text, encoding="utf8")
            print(f"[ok] 插件已归一化为目标状态: {path}")
        else:
            print(f"[skip] 插件已是目标状态: {path}")
        return changed

    for old, new, label in EDITS:
        if old not in text:
            print(f"[fail] 锚点缺失({{label}}): {{path}}")
            return False
        text = text.replace(old, new, 1)
    # 冷启动路径也要走一遍归一化：模板里的方法表/默认方法与目标值保持一致。
    text, _ = normalize_plugin(path, text)
    path.write_text(text, encoding="utf8")
    print(f"[ok] 已给插件打上挂断补丁: {path}")
    return True


EDITS = [
    ('      (response) => {\n        response.resume();\n        response.on("end", () => {\n          if (response.statusCode >= 200 && response.statusCode < 300) resolve();\n          else reject(new Error(`AV host returned HTTP ${response.statusCode}`));\n        });\n      },\n', '      (response) => {\n        const chunks = [];\n        response.on("data", (chunk) => chunks.push(chunk));\n        response.on("end", () => {\n          if (response.statusCode >= 200 && response.statusCode < 300) {\n            let payload = null;\n            try {\n              payload = JSON.parse(Buffer.concat(chunks).toString("utf8"));\n            } catch {\n              payload = null;\n            }\n            resolve(payload);\n          } else {\n            reject(new Error(`AV host returned HTTP ${response.statusCode}`));\n          }\n        });\n      },\n', "invoke-response"),
    ('    identityResolvedAt: null,\n    identityError: null,\n  };\n}\n', '    identityResolvedAt: null,\n    identityError: null,\n    inviteNumbers: null,\n    hangupRequestedAt: null,\n    hangupMethod: null,\n    hangupParams: null,\n    hangupAcceptedAt: null,\n    hangupError: null,\n  };\n}\n', "idle-call"),
    ('  } else if (command === 20006 && Array.isArray(value)) {\n    activeSDKInvite = value;\n', '  } else if (command === 20006 && Array.isArray(value)) {\n    activeSDKInvite = value;\n    state.call.inviteNumbers = value\n      .map((item, index) => (typeof item === "number" ? [index, item] : null))\n      .filter(Boolean);\n', "invite-numbers"),
    (
        'async function startControlServer() {',
        'const HANGUP_METHODS = { quit: 8, reject: 9, close: 10, clearRoom: 11 };\n\n/// 候选房间号：优先显式传入，其次取来电元组里的第一个正整数（invite[3]）。\nfunction hangupRoomId(body) {\n  if (Number.isInteger(body?.roomId) && body.roomId >= 0) return body.roomId;\n  const numbers = Array.isArray(state.call.inviteNumbers) ? state.call.inviteNumbers : [];\n  const preferred = numbers.find(([index]) => index === 3);\n  if (preferred) return preferred[1];\n  const anyPositive = numbers.find(([, value]) => Number.isInteger(value) && value > 0);\n  return anyPositive ? anyPositive[1] : 0;\n}\n\nfunction buildHangupParams(method, body) {\n  const roomId = hangupRoomId(body);\n  const reason = Number.isInteger(body?.reason) ? body.reason : 1;\n  const uid = typeof body?.uid === "string" ? body.uid : state.call.callerUid || "";\n  if (method === "quit") return [roomId, reason];\n  if (method === "reject") return [roomId, uid, reason];\n  if (method === "close") return [roomId, uid, reason];\n  return [roomId, uid];\n}\n\nasync function hangupCall(body) {\n  const method = typeof body?.method === "string" && body.method ? body.method : "quit";\n  const command = HANGUP_METHODS[method];\n  if (!command) throw new Error("unknown hangup method");\n  const params = buildHangupParams(method, body);\n  state.call.hangupRequestedAt = new Date().toISOString();\n  state.call.hangupMethod = method;\n  state.call.hangupParams = params.map((item) => (typeof item === "number" ? item : "<uid>"));\n  const response = await invokeAVHost(command, params);\n  state.call.hangupAcceptedAt = new Date().toISOString();\n  state.call.hangupError = null;\n  if (state.call.phase !== "ended") state.call.phase = "ending";\n  return { method, command, params: state.call.hangupParams, response };\n}\n\n' + 'async function startControlServer() {',
        "hangup-funcs",
    ),
    ('    if (req.method === "POST" && url.pathname === "/v1/avsdk/output") {\n', '    if (req.method === "POST" && url.pathname === "/v1/calls/hangup") {\n      try {\n        const data = await hangupCall(await readJsonBody(req));\n        return sendJson(res, 200, { code: 0, data });\n      } catch (error) {\n        state.call.hangupError = String(error?.message ?? error);\n        return sendJson(res, 400, { code: -1, message: "hangup failed" });\n      }\n    }\n' + '    if (req.method === "POST" && url.pathname === "/v1/avsdk/output") {\n', "hangup-route"),
]


def main() -> int:
    # 可选：显式给出两个文件路径（便于对着上游原版离线验证本补丁）。
    explicit = [Path(argument) for argument in sys.argv[1:]]
    host = explicit[0] if explicit else next((p for p in HOST_CANDIDATES if p.exists()), None)
    plugin = (
        explicit[1]
        if len(explicit) > 1
        else next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    )
    if host is None:
        print("[fail] 找不到 host.cjs")
        return 1
    if plugin is None:
        print("[fail] 找不到插件 index.mjs")
        return 1
    normalize_host(host)
    patch_plugin(plugin)
    return 0


if __name__ == "__main__":
    sys.exit(main())
