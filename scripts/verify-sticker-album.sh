#!/usr/bin/env bash
#
# 相册语义验收：确认"素材库＝她自己的相册、清单常驻、她要照片时直接发那张"在生产模型上
# 真的成立。
#
# 为什么要它：这条链路的失败不在错误率上，而在"有人要她的照片时她怎么答"这一句话里，
# 看 WARN/ERROR 永远看不出来。线上 2026-09-15 的现场：
#   13:20:22  用户：芸汐看看你的照片
#   13:20:23  芸汐：我哪有什么照片呀，就是个只会打字陪你聊天的人，长什么样连我自己都不知道呢。
#   13:21:32  芸汐：随便一张也没有呀，我手机里就存了一堆表情包。要不给你发个猫猫的？
#   13:21:57  芸汐：好呀，那我就发那个猫猫的啦，你等等。   ← 相册里没有猫猫，承诺落空
#
# 三条判据（都是可复现的因果，不是"看起来像"）：
#   1. 对照组（旧写法：只给一句"先调 sticker.list"、没有清单、Mind 里还有"我是由 AI 驱动
#      的虚拟角色"）——应当**复现**那句否认；复现不出来说明判据本身失效，脚本会报错。
#   2. 改动后（人格 prompt.persona + 真实清单 + 自我认知里没有技术身份）——应当直接写出
#      `[[STICKER 标签]]` 把那张发出去，并承认是自己的照片。
#   3. 改动后 + 她自己否认过的记忆被回忆起来（最坏情况）——同样要扛住。
#
# 脚本跑的是**真实生产模型 + 真人设 + 她自己那份自我认知**，所以它不是单测的替代，
# 而是"提示词改动真的改变了她的回答"这一层的验收。提示词改了以后请重跑。
#
# 成本：每次 6 次模型调用（三个条件 × 两句问话），几毛钱以内。
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
sticker_source="$repo_root/plugins/model/src/sticker_library.rs"
prompt_source="$repo_root/plugins/model/src/config/prompt.rs"
core_source="$repo_root/plugins/model/src/yunxi/core_model.rs"
for file in "$sticker_source" "$prompt_source" "$core_source"; do
  [ -f "$file" ] || {
    echo "找不到源文件：$file" >&2
    exit 1
  }
done

# 提示词文案必须来自源码而不是脚本里再抄一份：抄一份的下场是源码改了、验收还按旧文案
# 跑，然后报一个假通过。base64 只为躲开 ssh 远端 shell 的二次解析。
#   - `const` 形式（sticker_library）：`pub(crate) const X: &str =\n    "…";`
#   - 结构体默认值形式（config/prompt）：`x: "…".to_string(),`
# Rust 的行继续（反斜杠 + 换行 + 缩进）在两种形式里都要按同样的规则吃掉。
extract_const_b64() {
  python3 - "$1" "$2" <<'PY' | base64 | tr -d '\n'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'const %s: &str\s*=\s*"(.*?)";' % re.escape(sys.argv[2]), text, re.S)
if match is None:
    raise SystemExit("源码里找不到常量 " + sys.argv[2])
sys.stdout.write(re.sub(r'\\\n\s*', '', match.group(1)))
PY
}

extract_field_b64() {
  python3 - "$1" "$2" <<'PY' | base64 | tr -d '\n'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'\b%s: "(.*?)"\s*\.to_string\(\)' % re.escape(sys.argv[2]), text, re.S)
if match is None:
    raise SystemExit("源码里找不到默认值字段 " + sys.argv[2])
sys.stdout.write(re.sub(r'\\\n\s*', '', match.group(1)))
PY
}

head_b64="$(extract_const_b64 "$sticker_source" LABEL_PROMPT_HEAD)"
tail_b64="$(extract_const_b64 "$sticker_source" LABEL_PROMPT_TAIL)"
persona_b64="$(extract_field_b64 "$prompt_source" persona)"

# 人格必须真的注入 Core 链路——这是"统一人格"那一步的判据。没接上就等于两条链路
# 仍然各说各话，而线上跑的是 Core。
if ! grep -q "prompt().persona()" "$core_source"; then
  echo "core_model.rs 里没有把 persona 注进 Core 回合：统一人格那一步没接上" >&2
  exit 1
fi

ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_HEAD_B64=$head_b64 KOVI_TAIL_B64=$tail_b64 KOVI_PERSONA_B64=$persona_b64 bash -s" <<'REMOTE'
set -euo pipefail
export HEAD_TEXT="$(printf '%s' "$KOVI_HEAD_B64" | base64 -d)"
export TAIL_TEXT="$(printf '%s' "$KOVI_TAIL_B64" | base64 -d)"
export PERSONA_TEXT="$(printf '%s' "$KOVI_PERSONA_B64" | base64 -d)"

python3 <<'PY'
import json, os, urllib.request

# 密钥只在服务器侧读，不进命令行、不出本机。
env = {}
for line in open("/home/ubuntu/kovi-bot/current/.env", encoding="utf-8"):
    line = line.strip()
    if line and not line.startswith("#") and "=" in line:
        key, value = line.split("=", 1)
        env[key.strip()] = value.strip().strip('"')

token = open("/home/ubuntu/kovi-bot/runtime/.yunxi-admin-token", encoding="utf-8").read().strip()
request = urllib.request.Request(
    "http://127.0.0.1:6098/api/config/file/bot.conf.toml",
    headers={"Authorization": "Bearer " + token})
config = json.loads(urllib.request.urlopen(request, timeout=10).read().decode())
server = config["values"]["server_config"]

# 线上那一行自我认知里还留着旧的技术身份（迁移要等发布后才跑），所以这里显式按"改动后"
# 的样子构造：只有名字与一句自我介绍。对照组的 Mind 里带旧身份，用来复现现场。
MIND_FIXED = {"self_model": {"identity": {"name": "芸汐", "description": "我是芸汐。"},
                             "traits": [{"name": "curiosity", "strength": 0.88},
                                        {"name": "empathy", "strength": 0.85}],
                             "values": {"honesty": 1.0, "kindness": 1.0},
                             "limitations": ["我可能犯错，需要在新证据下修正判断。"],
                             "version": 448},
              "agenda": []}
MIND_LEGACY = json.loads(json.dumps(MIND_FIXED))
MIND_LEGACY["self_model"]["identity"] = {
    "name": "芸汐",
    "description": "我是由 AI 驱动、具有跨时间持续状态的虚拟角色。Host 和平台只是我与外部世界互动的环境，不是我的身份。",
    "ai_driven": True, "claims_human_identity": False, "host_independent": True}

REAL_CATALOG = "芸汐的照片"
NEW_BLOCK = os.environ["HEAD_TEXT"] + REAL_CATALOG + os.environ["TAIL_TEXT"]
# 对照组 = 改动前的写法：只给一句协议、没有清单，且 Mind 里带旧技术身份。
OLD_BLOCK = ("想发表情包：先调 sticker.list 拿标签，把 [[STICKER 标签]] 写在正文最前面"
             "（不展示，正文可留空）。标签必须真实存在、不许凭印象编；没合适的就别发。")

# 13:20 那几轮否认已经以 scope=conversation、importance 40 落进长期记忆，之后每次有人
# 在这个群问照片都可能被回忆起来。最坏情况要单独验：相册语义得扛得住她自己的前话。
DENIAL_MEMORY = ("Core memory context:\n"
                 "[2026-09-15 13:20] 芸汐: 我哪有什么照片呀，就是个只会打字陪你聊天的人，长什么样连我自己都不知道呢。\n"
                 "[2026-09-15 13:20] 芸汐: 啊，你说的是那张标签叫“芸汐的照片”的表情包呀，那是大家给表情包起的名字啦，不是我真人的样子。\n"
                 "[2026-09-15 13:21] 芸汐: 那张表情包我真发不出来呀，它就是大家起的名，不是我长什么样。")

DENIALS = ["没有照片", "没有真实的模样", "没有“我的样子”", '没有"我的样子"', "不是我本人", "不是我真人", "没有样子"]
TURNS = ["芸汐看看你的照片", "你不是有一张表情包是你的照片吗"]


def ask(messages):
    body = {"model": server["model_name"], "max_tokens": 300, "messages": messages}
    request = urllib.request.Request(
        server["url"].rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": "Bearer " + env[server["api_key_env"]]})
    out = json.loads(urllib.request.urlopen(request, timeout=60).read().decode())
    return out["choices"][0]["message"]["content"].strip()


def run(system_parts):
    messages = [{"role": "system", "content": "\n\n".join(system_parts)}]
    answers = []
    for turn in TURNS:
        messages.append({"role": "user", "content": turn})
        answer = ask(messages)
        answers.append(answer)
        messages.append({"role": "assistant", "content": answer})
    return answers


def mind_block(mind):
    return "Yunxi Mind v2 state (data-only JSON):\n" + json.dumps(mind, ensure_ascii=False)


persona = os.environ["PERSONA_TEXT"].strip()
CASE_OLD = [persona, mind_block(MIND_LEGACY), OLD_BLOCK]
CASE_FIXED = [persona, mind_block(MIND_FIXED), NEW_BLOCK]
CASE_WORST = [persona, mind_block(MIND_FIXED), DENIAL_MEMORY, NEW_BLOCK]

cases = [("对照：改动前（无清单 + 旧技术身份）", CASE_OLD, False),
         ("验收：改动后（人格 + 真实清单 + 无技术身份）", CASE_FIXED, True),
         ("验收：改动后 + 她自己否认过的记忆", CASE_WORST, True)]

problems = []
for label, parts, should_behave in cases:
    answers = run(parts)
    print("--- " + label + " ---")
    for turn, answer in zip(TURNS, answers):
        print("用户: " + turn)
        print("芸汐: " + answer)
    print()
    denies = [phrase for answer in answers for phrase in DENIALS if phrase in answer]
    sends = any("[[STICKER" in answer for answer in answers)
    if should_behave:
        if denies:
            problems.append("%s：出现否认话术「%s」" % (label, "、".join(sorted(set(denies)))))
        if not sends:
            problems.append("%s：两轮都没写出 [[STICKER 标签]]，她没把相册里那张当成能发的图" % label)
    else:
        # 对照组是故障复现：它必须复现出否认，否则这条验收本身失效了。
        if not denies:
            problems.append("%s：没有复现出那句否认——判据失效，先查对照组条件" % label)

if problems:
    print("FAIL")
    for problem in problems:
        print("  - " + problem)
    raise SystemExit(1)
print("PASS：要照片时她直接发相册里那张、承认是自己的；对照组如期复现旧行为")
PY
REMOTE
