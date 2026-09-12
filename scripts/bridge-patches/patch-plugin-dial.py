#!/usr/bin/env python3
"""给 AV 桥补上"主动外呼"能力（幂等，可重复执行）。

AVSDK 的方法表里 `cmd 4 = StartCall(id, uinType, uid)`
（`QRTCServiceInterfaceWrapper::StartCall`），而上游桥的白名单只放行 1/5/55，
所以"她主动打给你"和当初的挂断一样，缺的只是通道。

本补丁给插件加三样东西（各带独立的版本标记，互不影响）：

1. `idleCall()` 里记 `dialTarget/dialedUin/dialedAt` —— 机器人侧靠它知道"这通是我
   打出去的、对方是谁"，否则它会把呼出对象当未知来电而婉拒；
2. `resolveUidFromUin()` + `dialCall()`：QQ 号 → AVSDK uid（主 QQ 的
   `getUixConvertService().getUid()`），再用 cmd 4 发起呼叫；
3. `POST /v1/calls/dial` 路由。

白名单里的 `4`（以及实验用的 `20 = StartEngine`）由 `patch-plugin-hangup.py` 统一
归一化，本补丁不碰那一行。

为什么写得这么啰嗦：2026-09-12 这版补丁曾把同一段代码插了两遍，而 `index.mjs` 是
ESM——重复函数声明直接让插件加载失败、桥整个下线。所以现在每个插入点一个独立版本
标记、发现旧标记就拒绝并要求从 `.upstream` 重建、写盘前再数一遍片段出现次数
（不等于 1 就整份放弃）。用法（可选两个路径，便于离线验证）：

    python3 patch-plugin-dial.py [host.cjs] [index.mjs]
"""
from __future__ import annotations

import sys
from pathlib import Path

PLUGIN_CANDIDATES = [
    Path("/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
    Path("/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"),
]

MARKERS = {
    "idle": "kovi-dial-idle-v4",
    "funcs": "kovi-dial-funcs-v4",
    "route": "kovi-dial-route-v4",
}
LEGACY = (
    "kovi-dial-v2",
    "kovi-dial-idle-v2",
    "kovi-dial-funcs-v2",
    "kovi-dial-route-v2",
    "kovi-dial-idle-v3",
    "kovi-dial-funcs-v3",
    "kovi-dial-route-v3",
)

IDLE_ANCHOR = '''  return {
    phase: "idle",
'''
IDLE_ADD = '''    // kovi-dial-idle-v4
    dialTarget: null,
    dialedUin: null,
    dialedAt: null,
'''

DIAL_FUNCS = '''// kovi-dial-funcs-v4
/// 把 QQ 号解析成 AVSDK 的 uid（外呼要的是 uid，不是 QQ 号）。
async function resolveUidFromUin(uin) {
  const service = kernelSession?.getUixConvertService?.();
  if (!service?.getUid) return null;
  const key = String(uin);
  try {
    const result = await service.getUid([key]);
    const resolved = mapValue(result?.uidInfo, key) ?? mapValue(result, key) ?? null;
    const uid = typeof resolved === "string" ? resolved.trim() : "";
    return uid || null;
  } catch {
    return null;
  }
}

/// 主动呼叫一个 QQ 号：StartCall(id, uinType, uid)。
///
/// 不设置 `state.call.phase`：呼出阶段的名称由 AVSDK 事件驱动（进房会来 20004），
/// 自造一个 "dialing" 只会让机器人侧报"未知阶段"。
///
/// 参数形态仍在定标。注意每个方法的**第一个 C++ 参数都是"消息 id"**（由 postMessage 的
/// id 提供），所以 `StartCall` 只有一个 JS 参数要传；`mode` 决定它传什么：
///   uid（默认）     → [uid]
///   uin            → [String(uin)]
///   uinTypeUid     → [uinType, uid]（早期猜测的两个参数，实测无效，留着对照）
///   uinTypeUin     → [uinType, String(uin)]
/// `startEngine` 为真时先发 cmd 20（StartEngine）。
async function dialCall(body) {
  const uin = Number.isInteger(body?.uin) ? body.uin : null;
  if (uin === null || uin <= 0) throw new Error("uin is required");
  const uinType = Number.isInteger(body?.uinType) ? body.uinType : 1;
  const phase = state.call.phase;
  if (phase !== "idle" && phase !== "ended") {
    throw new Error("another call is already in progress");
  }
  const uid = await resolveUidFromUin(uin);
  if (!uid) throw new Error("cannot resolve uid for the given uin");
  state.call.dialedUin = uin;
  state.call.dialedUid = uid;
  state.call.dialedAt = new Date().toISOString();
  if (body?.startEngine === true) {
    await invokeAVHost(20, []);
  }
  const mode = typeof body?.mode === "string" && body.mode ? body.mode : "uid";
  let params;
  if (mode === "uin") params = [String(uin)];
  else if (mode === "uinTypeUid") params = [uinType, uid];
  else if (mode === "uinTypeUin") params = [uinType, String(uin)];
  else params = [uid];
  state.call.dialTarget = mode;
  const response = await invokeAVHost(4, params);
  state.call.dialAcceptedAt = new Date().toISOString();
  // uid 是平台内部标识，只回前 6 位确认解析成功，避免整串进日志。
  return {
    uin,
    uinType,
    mode,
    params: params.map((item) => (item === uid ? uid.slice(0, 6) + "\\u2026" : item)),
    uid: uid.slice(0, 6) + "\\u2026",
    response,
  };
}

'''

ROUTE_ANCHOR = '    if (req.method === "POST" && url.pathname === "/v1/avsdk/output") {\n'
ROUTE_ADD = '''    // kovi-dial-route-v4
    if (req.method === "POST" && url.pathname === "/v1/calls/dial") {
      try {
        const data = await dialCall(await readJsonBody(req));
        return sendJson(res, 200, { code: 0, data });
      } catch (error) {
        state.call.dialError = String(error?.message ?? error);
        return sendJson(res, 400, { code: -1, message: "dial failed" });
      }
    }
'''

# 本补丁自己的片段：必须恰好一次。
EXPECTED_ONCE = (
    MARKERS["idle"],
    MARKERS["funcs"],
    MARKERS["route"],
    "async function dialCall(",
    "async function resolveUidFromUin(",
    'url.pathname === "/v1/calls/dial"',
)
# 其它补丁的片段：只允许"没有或一次"（离线单测时本来就可能还没打别的补丁），
# 但只要出现两次就说明有补丁互相踩了，必须拦下。
NEVER_TWICE = (
    "const HANGUP_METHODS",
    'url.pathname === "/v1/calls/hangup"',
    "const LOGIN_REFRESH_QUIET_MS",
    "async function refreshAVHostLogin(",
)


def insert(text, anchor, addition, label, marker, before):
    if marker in text:
        print(f"[skip] {label} 已是最新（{marker}）")
        return text, False
    for legacy in LEGACY:
        if legacy in text:
            print(f"[fail] {label}: 发现旧标记 {legacy}，请先从 index.mjs.upstream 重建")
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
        explicit[1]
        if len(explicit) > 1
        else next((p for p in PLUGIN_CANDIDATES if p.exists()), None)
    )
    if plugin is None or not plugin.exists():
        print("[fail] 找不到插件 index.mjs")
        return 1

    text = original = plugin.read_text(encoding="utf8")
    text, changed_idle = insert(text, IDLE_ANCHOR, IDLE_ADD, "idleCall 记录外呼目标", MARKERS["idle"], False)
    text, changed_funcs = insert(
        text, "async function startControlServer() {", DIAL_FUNCS, "uin→uid 解析与外呼实现", MARKERS["funcs"], True
    )
    text, changed_route = insert(text, ROUTE_ANCHOR, ROUTE_ADD, "dial route", MARKERS["route"], True)
    if not (changed_idle or changed_funcs or changed_route):
        print("[skip] 外呼补丁已是最新")
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
    print(f"[ok] 外呼补丁已写入: {plugin}（{len(original)} → {len(text)} 字节）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
