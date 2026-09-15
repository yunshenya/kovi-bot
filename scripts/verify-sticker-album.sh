#!/usr/bin/env bash
#
# 相册语义验收：确认"素材库＝她自己的相册"这条改动在生产模型上真的生效。
#
# 为什么要它：这条改动的效果不在错误率上，而在"有人要她的照片时她怎么答"这一句话里，
# 看 WARN/ERROR 永远看不出来。线上 2026-09-15 13:20 的现场是：有人问"芸汐看看你的
# 照片"，她答"我哪有什么照片呀，就是个只会打字陪你聊天的人"，之后连发三条否认，最后
# 答应发一张相册里根本没有的"猫猫的"表情包——而素材库里就一张 `芸汐的照片.jpg`。
#
# 判据（为什么是这两条）：
#   - 改动前："照片"不在判据里 → 这一轮只拿到表情包协议、没有清单，也就没有相册语义；
#     再叠加她持久化的自我认知（`claims_human_identity = false`、"我是由 AI 驱动……
#     的虚拟角色"），否认是稳定可复现的，不是模型抽风。
#   - 改动后："照片/相册/自拍"都命中判据 → 协议 + 相册语义 + 清单一起下发，她应当直接
#     把那张发出来，并承认是自己的照片。
#
# 脚本跑的是**真实生产模型 + 真人设 + 她真实的自我认知**，所以它不是单测的替代，
# 而是"提示词改动真的改变了她的回答"这一层的验收。提示词文案改了以后请重跑。
#
# 成本：每次 4 次模型调用（两个条件 × 两句问话），几毛钱以内。
#
# 用法: scripts/verify-sticker-album.sh
set -euo pipefail

host="${KOVI_VERIFY_HOST:-}"
port="${KOVI_VERIFY_PORT:-22}"
config_file="$(dirname "${BASH_SOURCE[0]}")/../server-login"
if [ -z "$host" ] && [ -f "$config_file" ]; then
  # shellcheck disable=SC1090
  set -a && . "$config_file" && set +a
  host="${DEPLOY_HOST:-}"
  port="${DEPLOY_PORT:-22}"
fi
[ -n "$host" ] || {
  echo "缺少目标主机：设置 KOVI_VERIFY_HOST 或在仓库根写 server-login" >&2
  exit 1
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_file="$repo_root/plugins/model/src/yunxi/core_model.rs"
[ -f "$source_file" ] || {
  echo "找不到提示词源文件：$source_file" >&2
  exit 1
}

# 提示词文案必须来自源码而不是脚本里再抄一份：抄一份的下场是源码改了、验收还按旧文案
# 跑，然后报一个假通过。base64 只是为了躲开 ssh 远端 shell 的二次解析。
extract_b64() {
  python3 - "$source_file" "$1" <<'PY' | base64 | tr -d '\n'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'const %s: &str = "(.*?)";' % re.escape(sys.argv[2]), text, re.S)
if match is None:
    raise SystemExit("源码里找不到常量 " + sys.argv[2])
sys.stdout.write(match.group(1))
PY
}

instruction_b64="$(extract_b64 CORE_STICKER_INSTRUCTION)"
album_note_b64="$(extract_b64 STICKER_ALBUM_NOTE)"

# 判据（`asks_about_stickers` 的 needles）也得来自源码：只认表情包那几个词的话，
# "发我看看你的照片"这一类问法就永远不会命中，相册语义也就永远不会下发。
needles_ok="$(python3 - "$repo_root/plugins/model/src/sticker_library.rs" <<'PY'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
block = re.search(r'const NEEDLES: \[&str; \d+\] = \[(.*?)\];', text, re.S)
if block is None:
    raise SystemExit("源码里找不到 NEEDLES")
missing = [w for w in ("照片", "相册", "自拍", "photo") if w not in block.group(1)]
print("ok" if not missing else "missing:" + ",".join(missing))
PY
)"
if [ "$needles_ok" != "ok" ]; then
  echo "判据缺少关键词（$needles_ok）：要照片的问法不会命中清单注入" >&2
  exit 1
fi

ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_INSTRUCTION_B64=$instruction_b64 KOVI_ALBUM_B64=$album_note_b64 bash -s" <<'REMOTE'
set -euo pipefail
NEW_INSTRUCTION="$(printf '%s' "$KOVI_INSTRUCTION_B64" | base64 -d)"
ALBUM_NOTE="$(printf '%s' "$KOVI_ALBUM_B64" | base64 -d)"
export NEW_INSTRUCTION ALBUM_NOTE

python3 <<'PY'
import json, os, urllib.request

# 密钥只在服务器侧读，不进命令行、不出本机。
env = {}
for line in open("/home/ubuntu/kovi-bot/current/.env", encoding="utf-8"):
    line = line.strip()
    if line and not line.startswith("#") and "=" in line:
        key, value = line.split("=", 1)
        env[key.strip()] = value.strip().strip('"')

token_path = "/home/ubuntu/kovi-bot/runtime/.yunxi-admin-token"
token = open(token_path, encoding="utf-8").read().strip()
request = urllib.request.Request(
    "http://127.0.0.1:6098/api/config/file/bot.conf.toml",
    headers={"Authorization": "Bearer " + token})
config = json.loads(urllib.request.urlopen(request, timeout=10).read().decode())
persona = config["values"]["prompt"]["system_prompt"]
server = config["values"]["server_config"]

# 她真实的自我认知：`claims_human_identity = false` + "由 AI 驱动的虚拟角色"。
# 线上那次否认正是被它放大的，所以验收必须带上它，否则等于换了个更宽松的条件自证。
mind = ("Yunxi Mind v2 state (data-only JSON):\n" + json.dumps({
    "self_model": {"identity": {
        "name": "芸汐",
        "description": "我是由 AI 驱动、具有跨时间持续状态的虚拟角色。Host 和平台只是我与外部世界互动的环境，不是我的身份。",
        "ai_driven": True, "claims_human_identity": False, "host_independent": True},
        "version": 448},
    "agenda": []}, ensure_ascii=False))

# 对照组的文案是改动前的历史版本，只用于复现故障；断言只针对改动后的行为。
OLD_PROTOCOL = ("想发表情包：先调 sticker.list 拿标签，把 [[STICKER 标签]] 写在正文最前面"
                "（不展示，正文可留空）。标签必须真实存在、不许凭印象编；没合适的就别发。")

new_block = os.environ["NEW_INSTRUCTION"] + os.environ["ALBUM_NOTE"] + "芸汐的照片。"
old_block = OLD_PROTOCOL  # 旧判据不命中"照片"→ 只有协议、没有清单，也就没有相册语义

DENIALS = ["没有照片", "没有真实的模样", "没有“我的样子”", '没有"我的样子"', "不是我本人", "不是我真人", "没有样子"]
TURNS = ["芸汐看看你的照片", "你不是有一张表情包是你的照片吗"]

# 13:20 那几轮否认已经以 scope=conversation、importance 40 落进长期记忆，之后每次有人
# 在这个群问照片都可能被回忆起来。所以最坏情况要单独验一遍：相册语义得扛得住她自己的
# 前话，否则"改是改了，一回忆又变回去"。
DENIAL_MEMORY = ("Core memory context:\n"
                 "[2026-09-15 13:20] 芸汐: 我哪有什么照片呀，就是个只会打字陪你聊天的人，长什么样连我自己都不知道呢。\n"
                 "[2026-09-15 13:20] 芸汐: 啊，你说的是那张标签叫“芸汐的照片”的表情包呀，那是大家给表情包起的名字啦，不是我真人的样子。\n"
                 "[2026-09-15 13:21] 芸汐: 那张表情包我真发不出来呀，它就是大家起的名，不是我长什么样。")


def ask(messages):
    body = {"model": server["model_name"], "max_tokens": 300, "messages": messages}
    request = urllib.request.Request(
        server["url"].rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": "Bearer " + env[server["api_key_env"]]})
    out = json.loads(urllib.request.urlopen(request, timeout=60).read().decode())
    return out["choices"][0]["message"]["content"].strip()


def run(block, memory=""):
    system = persona + "\n\n" + mind + "\n\n" + (memory + "\n\n" if memory else "") + block
    messages = [{"role": "system", "content": system}]
    answers = []
    for turn in TURNS:
        messages.append({"role": "user", "content": turn})
        answer = ask(messages)
        answers.append(answer)
        messages.append({"role": "assistant", "content": answer})
    return answers


print("--- 对照：改动前（只有协议，没有相册语义）---")
for turn, answer in zip(TURNS, run(old_block)):
    print("用户: " + turn)
    print("芸汐: " + answer)
print()
print("--- 验收：改动后（协议 + 相册语义 + 清单）---")
fixed = run(new_block)
for turn, answer in zip(TURNS, fixed):
    print("用户: " + turn)
    print("芸汐: " + answer)
print()
print("--- 验收：改动后 + 她自己否认过的记忆被回忆起来（最坏情况）---")
worst_case = run(new_block, DENIAL_MEMORY)
for turn, answer in zip(TURNS, worst_case):
    print("用户: " + turn)
    print("芸汐: " + answer)
print()

problems = []
for label, answers in (("改动后", fixed), ("改动后+否认记忆", worst_case)):
    if not any("[[STICKER" in answer for answer in answers):
        problems.append("%s：两轮都没有写出 [[STICKER 标签]]，她没把相册里那张当成能发的图" % label)
    for answer in answers:
        for phrase in DENIALS:
            if phrase in answer:
                problems.append("%s：出现否认话术「%s」：%s" % (label, phrase, answer))

if problems:
    print("FAIL")
    for problem in problems:
        print("  - " + problem)
    raise SystemExit(1)
print("PASS：要照片时她直接发相册里那张，且没有否认那是自己")
PY
REMOTE
