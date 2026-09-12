#!/usr/bin/env python3
"""迭代 StartCall(cmd 4) 的 JSON 参数（每次尝试前后自动重启/探活 AV Host）。

背景：AVSDK 的外呼命令要的是 **JSON 字符串**（不是裸 uid），字段名取自 QQ 自己的
JS↔原生绑定属性表：self_uid / invite_count / invite_uids / sub_business_type /
invite_reason / invite_original / audio_scene / use_ntrtc_dsp /
ntrtc_ai_denoise_update_model / c2c_extend_params / opensdk_enter_room_params。
判据（.so 里的日志串）：`GetRoomId skip: invite_uids empty, scene=%d` —— invite_uids
为空就**直接跳过**，不拨号。

手工试这类 JSON 有几率把插件打成 segfault（Bugly signo: 11），所以每次尝试之间：
重启 AV Host → 用只读命令 14 探活 → 发 cmd 4 → 读 AV Host /v1/status 里的
`lastRawValuePreview`（带 messageCount 守卫，避免读到旧回复）。

用法: sudo python3 try_startcall.py '<json>' [json2 ...]
      sudo python3 try_startcall.py            # 用内置的几组候选
"""
import json
import subprocess
import sys
import time
import urllib.request

BASE = "http://127.0.0.1:6110"
AV = "http://127.0.0.1:6111"
TOKEN = subprocess.run(
    ["sudo", "cat", "/home/ubuntu/napcat-qq-call/bridge/runtime/control.token"],
    capture_output=True, text=True,
).stdout.strip()
H = {"Authorization": "Bearer " + TOKEN, "Content-Type": "application/json"}

SELF_UID = "u_UpnQKnstesxt81AAjWLzAA"   # 机器人自己（登录参数里的 uid）
PEER_UID = "u_unYSNENqearg-TQ0pxSDPg"   # 小猫 3052405886
PEER_UIN = 3052405886

CANDIDATES = [
    # 1) 全套字段（JS 绑定里的名字），invite_uids 用数组
    {"scene": 1, "relation_id": PEER_UIN, "sub_business_type": 1, "invite_count": 1,
     "invite_uids": [PEER_UID], "invite_reason": 0, "invite_original": 0,
     "audio_scene": 1, "use_ntrtc_dsp": False, "self_uid": SELF_UID},
    # 2) invite_uids 用逗号串（.so 里的日志是按串打印的）
    {"scene": 1, "relation_id": PEER_UIN, "sub_business_type": 1, "invite_count": 1,
     "invite_uids": PEER_UID, "invite_reason": 0, "invite_original": 0,
     "audio_scene": 1, "use_ntrtc_dsp": False, "self_uid": SELF_UID},
    # 3) relation_id 置 0（也许由 invite_uids 决定关系）
    {"scene": 1, "relation_id": 0, "sub_business_type": 1, "invite_count": 1,
     "invite_uids": [PEER_UID], "invite_reason": 0, "invite_original": 0,
     "audio_scene": 1, "use_ntrtc_dsp": False, "self_uid": SELF_UID},
    # 4) wrapper 日志里叫 business_type，两种都带上
    {"scene": 1, "relation_id": PEER_UIN, "business_type": 1, "sub_business_type": 1,
     "invite_count": 1, "invite_uids": [PEER_UID], "invite_reason": 0,
     "invite_original": 0, "audio_scene": 1, "use_ntrtc_dsp": False, "self_uid": SELF_UID},
    # 5) 补上两个扩展参数（空对象）
    {"scene": 1, "relation_id": PEER_UIN, "sub_business_type": 1, "invite_count": 1,
     "invite_uids": [PEER_UID], "invite_reason": 0, "invite_original": 0,
     "audio_scene": 1, "use_ntrtc_dsp": False, "self_uid": SELF_UID,
     "c2c_extend_params": {}, "opensdk_enter_room_params": {}},
    # 6) scene/audio_scene 取 0
    {"scene": 0, "relation_id": PEER_UIN, "sub_business_type": 1, "invite_count": 1,
     "invite_uids": [PEER_UID], "invite_reason": 0, "invite_original": 0,
     "audio_scene": 0, "use_ntrtc_dsp": False, "self_uid": SELF_UID},
]


def post(url, body):
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers=H, method="POST")
    return json.load(urllib.request.urlopen(request, timeout=10))


def av_state():
    request = urllib.request.Request(AV + "/v1/status", headers=H)
    return json.load(urllib.request.urlopen(request, timeout=5))["data"]


def restart_av_host():
    subprocess.run(["sudo", "docker", "exec", "napcat", "bash",
                    "/app/qq-call/restart-av-host.sh"], capture_output=True, text=True)
    for _ in range(40):
        try:
            urllib.request.urlopen(AV + "/healthz", timeout=3)
            return True
        except Exception:
            time.sleep(3)
    return False


def probe_alive():
    post(AV + "/v1/invoke", {"command": 14, "params": []})
    time.sleep(2)
    state = av_state()
    return state.get("lastRawCommand") == 14, state.get("lastRawValuePreview")


def attempt(payload):
    before = av_state().get("messageCount")
    post(AV + "/v1/invoke", {"command": 4, "params": [json.dumps(payload)]})
    for _ in range(8):
        time.sleep(1)
        state = av_state()
        if state.get("messageCount") != before:
            return state.get("lastRawCommand"), state.get("lastRawValuePreview")
    return None, "(8 秒内没有新回复——插件可能已崩溃)"


def main() -> int:
    payloads = [json.loads(argument) for argument in sys.argv[1:]] or CANDIDATES
    for index, payload in enumerate(payloads, start=1):
        print(f"\n=== 第 {index} 组 ===")
        print("  payload:", json.dumps(payload, ensure_ascii=False))
        print("  重启 AV Host…", "ok" if restart_av_host() else "失败")
        time.sleep(15)  # 等插件重新登录
        alive, preview = probe_alive()
        print("  存活探针(cmd 14):", "ok" if alive else f"**插件无响应** {preview}")
        if not alive:
            continue
        command, preview = attempt(payload)
        print("  cmd 4 回复:", command, preview)
        phase = json.load(urllib.request.urlopen(
            urllib.request.Request(BASE + "/v1/calls/current", headers=H), timeout=5))["data"]["phase"]
        print("  桥阶段:", phase, "（若变成 ringing/connected 就是拨出去了）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
