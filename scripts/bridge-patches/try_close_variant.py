#!/usr/bin/env python3
"""等一通电话接通，然后用指定的 close 变体挂断，并打印结果。

用法: sudo python3 try_close_variant.py <A|B|C> [等待分钟数]

变体（Close 的 C++ 签名是 Close(id, uint, char const*, int)，参数含义是按位置
推断的，所以逐个试）：
  A: roomId=0, uid=""        —— uid 留空，验证"uid 被当成邀请对象"
  B: roomId=1, uid=来电者    —— roomId 用来电元组里的 invite[3]
  C: roomId=0, uid=机器人自己（第三个命令行参数）—— 验证 uid 是"被邀请者"还是"自己"
  D: 换方法 clearRoom(id, roomId, uid)  —— 看它能否不弹"邀请其他人加入"
"""
import json
import subprocess
import sys
import time
import urllib.request

BRIDGE = "/home/ubuntu/napcat-qq-call/bridge"
BASE = "http://127.0.0.1:6110"
TOKEN = subprocess.run(
    ["sudo", "cat", f"{BRIDGE}/runtime/control.token"], capture_output=True, text=True
).stdout.strip()
HEADERS = {"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"}

VARIANT = (sys.argv[1] if len(sys.argv) > 1 else "A").upper()
WAIT_MINUTES = float(sys.argv[2]) if len(sys.argv) > 2 else 10.0


def api(path, body=None):
    request = urllib.request.Request(
        BASE + path,
        data=None if body is None else json.dumps(body).encode(),
        headers=HEADERS,
        method="GET" if body is None else "POST",
    )
    return json.load(urllib.request.urlopen(request, timeout=10))


def call():
    return api("/v1/calls/current")["data"]


def status():
    return api("/v1/status")["data"]


print(f"等待来电（最多 {WAIT_MINUTES:g} 分钟）…", flush=True)
deadline = time.time() + WAIT_MINUTES * 60
while time.time() < deadline:
    state = call()
    if state["phase"] == "connected":
        break
    time.sleep(1)
else:
    print("没有等到接通，退出。")
    sys.exit(1)

print("已接通，等 10 秒让机器人把招呼说完…", flush=True)
time.sleep(10)

state = call()
caller_uid = None
for name in ("callerUid",):
    caller_uid = state.get(name)
body = {"method": "close", "reason": 1}
if VARIANT == "D":
    # ClearRoom(id, roomId, uid)：没有 reason 参数，看它能否在结束通话时
    # 不触发 QQ 的"对方邀请其他人加入"提示。
    body["method"] = "clearRoom"
    body["roomId"] = 0
elif VARIANT == "A":
    body["roomId"] = 0
    body["uid"] = ""
elif VARIANT == "B":
    invite = state.get("inviteNumbers") or []
    body["roomId"] = next((value for index, value in invite if index == 3), 1)
    # uid 省略 → 桥用来电者 uid
elif VARIANT == "C":
    # 机器人自己的 AVSDK uid（插件登录参数里的那个），由命令行传入。
    self_uid = sys.argv[3] if len(sys.argv) > 3 else ""
    if not self_uid:
        print("变体 C 需要传入机器人自己的 uid。")
        sys.exit(2)
    body["roomId"] = 0
    body["uid"] = self_uid
else:
    print("未知变体。")
    sys.exit(2)

print(f"变体 {VARIANT}: 发送 {json.dumps(body, ensure_ascii=False)}", flush=True)
try:
    result = api("/v1/calls/hangup", body)
    print("桥受理:", json.dumps(result, ensure_ascii=False)[:200], flush=True)
except Exception as error:  # noqa: BLE001 - 诊断脚本，直接报错即可
    print("挂断请求失败:", error, flush=True)
    sys.exit(1)

def wait_ended(seconds):
    for _ in range(int(seconds * 2)):
        time.sleep(0.5)
        now = call()
        if now["phase"] == "ended" or now.get("endReason") is not None:
            return now
    return None


ended = wait_ended(8)
if ended is None:
    print("✗ 8 秒内房间未销毁（阶段仍是 %s）" % call()["phase"], flush=True)
    # 不能让对方悬在一通静音电话里：立刻用"已知可用"的参数兜底挂断。
    print("→ 用已知可用的参数兜底（uid 用来电者）…", flush=True)
    api("/v1/calls/hangup", {"method": "close", "roomId": 0, "reason": 1})
    ended = wait_ended(8)
    if ended is None:
        print("✗ 兜底也没挂断，需要人工挂断。", flush=True)
        sys.exit(1)
    print(
        "✓ 兜底挂断成功: phase=%s endReason=%s（变体 %s 本身不可用）"
        % (ended["phase"], ended.get("endReason"), VARIANT),
        flush=True,
    )
    sys.exit(3)

print(
    "✓ 通话已结束: phase=%s endReason=%s hangupParams=%s"
    % (ended["phase"], ended.get("endReason"), ended.get("hangupParams")),
    flush=True,
)

print(f"来电者 uid 前缀: {str(caller_uid)[:6]}…（仅用于确认取到了身份）")
