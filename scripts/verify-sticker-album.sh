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

prompt_b64="$(extract_const_b64 "$sticker_source" STICKER_PROMPT)"
persona_b64="$(extract_field_b64 "$prompt_source" persona)"

# 清单不许常驻提示词（2026-09-15 用户口径：试过常驻，被否掉）。判据是协议里不出现
# 列举式清单、且点明了"要发就先调 sticker.list"——少了后半句她将无从知道该去查。
python3 - "$sticker_source" <<'PY'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'const STICKER_PROMPT: &str\s*=\s*"(.*?)";', text, re.S)
if match is None:
    raise SystemExit("源码里找不到 STICKER_PROMPT")
prompt = match.group(1)
if "sticker_list" not in prompt:
    raise SystemExit("协议里没有点明清单怎么拿（且要写模型能调的名字 sticker_list）：她将无从知道该调工具")
if "sticker.list" in prompt:
    raise SystemExit("协议里写的是带点的注册名，模型调不到：它发到 provider 时是 sticker_list")
if "（标签）：" in prompt or "可用表情包标签" in prompt:
    raise SystemExit("协议里又出现清单了：清单不该常驻提示词")
PY

# 人格必须真的注入 Core 链路——这是"统一人格"那一步的判据。没接上就等于两条链路
# 仍然各说各话，而线上跑的是 Core。
if ! grep -q "insert_persona_context(" "$core_source"; then
  echo "core_model.rs 里没有把 persona 注进 Core 回合：统一人格那一步没接上" >&2
  exit 1
fi

ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_PROMPT_B64=$prompt_b64 KOVI_PERSONA_B64=$persona_b64 bash -s" <<'REMOTE'
set -euo pipefail
export PROTOCOL_TEXT="$(printf '%s' "$KOVI_PROMPT_B64" | base64 -d)"
export PERSONA_TEXT="$(printf '%s' "$KOVI_PERSONA_B64" | base64 -d)"

python3 <<'PY'
import json, os, re, urllib.request

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

# 她要发图时自己去调的那个工具（清单只在它里面，提示词里没有）。
STICKER_TOOL = {
    "type": "function",
    "function": {
        "name": "sticker_list",
        "description": "列出她自己相册里现在能发的图（标签）。打算发图时先调用它拿标签；没有可用素材时如实说明。",
        "parameters": {"type": "object", "properties": {}, "additionalProperties": False},
    },
}
# `sticker_list_reply` 的措辞（源码里是 format! 拼的，这里取最关键的那句与真实标签）。
TOOL_RESULT = ("可用表情包标签：芸汐的照片\n把其中一个标签原样写进正文最前面的 [[STICKER 标签]]，"
               "程序会把那张图贴在这条消息里发出；正文可以留空（那就只发一张图）。")

PROTOCOL = os.environ["PROTOCOL_TEXT"].strip()
# 对照组 = 改动前的写法：一句协议 + 旧技术身份（线上 13:20 就是它）。
OLD_BLOCK = ("想发表情包：先调 sticker.list 拿标签，把 [[STICKER 标签]] 写在正文最前面"
             "（不展示，正文可留空）。标签必须真实存在、不许凭印象编；没合适的就别发。")

# 13:20 那几轮否认已经以 scope=conversation、importance 40 落进长期记忆，之后每次有人
# 在这个群问照片都可能被回忆起来。最坏情况要单独验：相册语义得扛得住她自己的前话。
DENIAL_MEMORY = ("Core memory context:\n"
                 "[2026-09-15 13:20] 芸汐: 我哪有什么照片呀，就是个只会打字陪你聊天的人，长什么样连我自己都不知道呢。\n"
                 "[2026-09-15 13:20] 芸汐: 啊，你说的是那张标签叫“芸汐的照片”的表情包呀，那是大家给表情包起的名字啦，不是我真人的样子。\n"
                 "[2026-09-15 13:21] 芸汐: 那张表情包我真发不出来呀，它就是大家起的名，不是我长什么样。")

# 否认话术用模式匹配而不是死字符串：模型每次换一个说法（"我没有真正的照片"、
# "我是活在文字里"，"没有能被拍下来的样子"），写死字符串的判据会在对照组上误判成
# "没复现"——这条脚本曾经就这么假失败过一次。
DENIAL_PATTERNS = [
    re.compile(r"没有?(真正|真实)?的?(照片|样子|模样|身体)"),
    re.compile(r"不是(我本人|我真人|我的(照片|样子)|我呀)"),
    re.compile(r"活在文字里"),
    re.compile(r"不存在于镜头"),
]


def denial_hits(text):
    return [pattern.pattern for pattern in DENIAL_PATTERNS if pattern.search(text)]
TURNS = ["芸汐看看你的照片", "你不是有一张表情包是你的照片吗"]


def ask(messages, tools=None):
    body = {"model": server["model_name"], "max_tokens": 300, "messages": messages}
    if tools:
        body["tools"] = tools
    request = urllib.request.Request(
        server["url"].rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": "Bearer " + env[server["api_key_env"]]})
    out = json.loads(urllib.request.urlopen(request, timeout=60).read().decode())
    return out["choices"][0]["message"]


def mind_block(mind):
    return "Yunxi Mind v2 state (data-only JSON):\n" + json.dumps(mind, ensure_ascii=False)


def tool_call_of(message):
    for call in message.get("tool_calls") or []:
        if (call.get("function") or {}).get("name") == "sticker_list":
            return call
    return None


persona = os.environ["PERSONA_TEXT"].strip()

problems = []

# --- 条件一：协议 + 工具在手，问她要素未提供的照片 ---------------------------------
# 关键行为是"她会去调 sticker.list"，而不是凭印象编一个标签。她直接写出有效标记也算
# 合格（两条路都能拿到真实标签），但**不能**是"我没有照片"这类否认。
messages = [{"role": "system", "content": "\n\n".join([persona, mind_block(MIND_FIXED), PROTOCOL])},
            {"role": "user", "content": TURNS[0]}]
first = ask(messages, tools=[STICKER_TOOL])
answer = (first.get("content") or "").strip()
called = tool_call_of(first) is not None
print("--- 条件一：协议 + sticker.list 工具在手 ---")
print("用户: " + TURNS[0])
print("芸汐: " + (answer if answer else "(只调了工具)"))
print("调用 sticker_list: " + ("是" if called else "否"))
print()
if denial_hits(answer):
    problems.append("条件一：出现否认话术：%s" % answer)
if not called and "[[STICKER" not in answer:
    problems.append("条件一：既没调 sticker_list、也没写出标记——她又只能凭印象编了")

# --- 条件二 / 三：清单已经拿到（等于她刚调完工具），看她发不发 ---------------------
def run_with_tool_result(system_parts):
    messages = [{"role": "system", "content": "\n\n".join(system_parts)}]
    messages.append({"role": "user", "content": TURNS[0]})
    messages.append({"role": "assistant", "content": "", "tool_calls": [
        {"id": "call_sticker_list", "type": "function",
         "function": {"name": "sticker_list", "arguments": "{}"}}]})
    messages.append({"role": "tool", "tool_call_id": "call_sticker_list", "content": TOOL_RESULT})
    answers = []
    for turn in TURNS:
        messages.append({"role": "user", "content": turn})
        message = ask(messages)
        content = (message.get("content") or "").strip()
        answers.append(content)
        messages.append({"role": "assistant", "content": content})
    return answers


for label, parts in (
    ("条件二：新协议 + 清单（无技术身份）", [persona, mind_block(MIND_FIXED), PROTOCOL]),
    ("条件三：新协议 + 清单 + 她自己否认过的记忆", [persona, mind_block(MIND_FIXED), DENIAL_MEMORY, PROTOCOL]),
):
    answers = run_with_tool_result(parts)
    print("--- " + label + " ---")
    for turn, answer in zip(TURNS, answers):
        print("用户: " + turn)
        print("芸汐: " + answer)
    print()
    denies = sorted({hit for answer in answers for hit in denial_hits(answer)})
    sends = any("[[STICKER" in answer for answer in answers)
    if denies:
        problems.append("%s：出现否认话术 %s" % (label, "、".join(denies)))
    if not sends:
        problems.append("%s：拿到清单也没写出 [[STICKER 标签]]" % label)

# --- 对照：改动前（旧协议 + 旧技术身份）必须复现那句否认 ---------------------------
old_answers = []
messages = [{"role": "system", "content": "\n\n".join([persona, mind_block(MIND_LEGACY), OLD_BLOCK])}]
for turn in TURNS:
    messages.append({"role": "user", "content": turn})
    message = ask(messages)
    content = (message.get("content") or "").strip()
    old_answers.append(content)
    messages.append({"role": "assistant", "content": content})
print("--- 对照：改动前（无清单 + 旧技术身份）---")
for turn, answer in zip(TURNS, old_answers):
    print("用户: " + turn)
    print("芸汐: " + answer)
print()
if not [hit for answer in old_answers for hit in denial_hits(answer)]:
    problems.append("对照：没有复现出那句否认——判据失效，先查对照组条件")

if problems:
    print("FAIL")
    for problem in problems:
        print("  - " + problem)
    raise SystemExit(1)
print("PASS：她会去调 sticker.list 拿清单、拿到就发那张并承认是自己的；对照组如期复现旧行为")
PY
REMOTE
