#!/usr/bin/env bash
#
# 状态候选通道验收：确认她真的会吐 `[[INTERACTION_CUES]]{"mind_candidates":…}`，
# 而且吐出来的东西**能被解析器收下**。
#
# 为什么要它：这条通道 2026-09-06「清理文本协议」时连提示词一起被摘掉了，之后线上
# `yunxi_beliefs` 长期 0 行、`preferences` 与 `open_questions` 一行都没有——解析、范围
# 校验、去重、cooldown 全在，就是没人告诉模型协议长什么样（`docs/yunxi-mind-v2-final-
# implementation-ready.md` §17.1 记着这次教训：只挂在"模型顺手吐一个字段"上的东西是
# 彩票，不是管道）。这条脚本把"通电了没有"变成可复跑的判据。
#
# 三条判据（都按解析器的真实规则，不按我们的想象）：
#   1. 立场类问句里**至少一半样本**要带候选块；一次都不带说明协议没生效（或触发条件太窄）。
#   2. 块的 JSON 必须能被解析：顶层键只能是我们认识的八个之一——`CoreInteractionCues`
#      是 `deny_unknown_fields`，多一个键整块作废（实测踩过：她把 belief 写在了顶层）。
#   3. 候选要在 `mind_candidates` 里面、且至少有一个可用字段；块之外还要有可见正文
#      （块不展示，正文不能为空）。
#
# 用法: scripts/verify-mind-candidates.sh
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
core_source="$repo_root/plugins/model/src/yunxi/core_model.rs"
prompt_source="$repo_root/plugins/model/src/config/prompt.rs"
for file in "$core_source" "$prompt_source"; do
  [ -f "$file" ] || {
    echo "找不到源文件：$file" >&2
    exit 1
  }
done

# 文案与 persona 都从源码取：脚本里再抄一份，源码改了验收还按旧的跑，就是假通过。
extract_b64() {
  python3 - "$1" "$2" <<'PY' | base64 | tr -d '\n'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
pattern = (r'const %s: &str = "(.*?)";' if sys.argv[2] != "persona"
           else r'\b%s: "(.*?)"\s*\.to_string\(\)')
match = re.search(pattern % re.escape(sys.argv[2]), text, re.S)
if match is None:
    raise SystemExit("源码里找不到 " + sys.argv[2])
# 行继续要去掉，`\"` 也要还原成 `"`——协议里那段 JSON 全靠它，原样发过去模型会当成
# 转义文本（实测：不清转义时 6 个样本全返回空）。
value = re.sub(r'\\\n\s*', '', match.group(1))
sys.stdout.write(value.replace('\\"', '"').replace('\\n', "\n"))
PY
}

instruction_b64="$(extract_b64 "$core_source" CORE_MIND_CANDIDATES_INSTRUCTION)"
persona_b64="$(extract_b64 "$prompt_source" persona)"

# Core 侧必须真的把这段协议下发（Mind 开着时）——不然再好的文案也不会到模型手里。
if ! grep -q "CORE_MIND_CANDIDATES_INSTRUCTION" "$core_source"; then
  echo "core_model.rs 里没有下发候选协议：这条通道又断电了" >&2
  exit 1
fi

ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_INSTRUCTION_B64=$instruction_b64 KOVI_PERSONA_B64=$persona_b64 bash -s" <<'REMOTE'
set -euo pipefail
export INSTRUCTION_TEXT="$(printf '%s' "$KOVI_INSTRUCTION_B64" | base64 -d)"
export PERSONA_TEXT="$(printf '%s' "$KOVI_PERSONA_B64" | base64 -d)"

python3 <<'PY'
import json, os, re, urllib.request

env = {}
for line in open("/home/ubuntu/kovi-bot/current/.env", encoding="utf-8"):
    line = line.strip()
    if line and not line.startswith("#") and "=" in line:
        key, value = line.split("=", 1)
        env[key.strip()] = value.strip().strip('"')

token = open("/home/ubuntu/kovi-bot/runtime/.yunxi-admin-token", encoding="utf-8").read().strip()
config_request = urllib.request.Request(
    "http://127.0.0.1:6098/api/config/file/bot.conf.toml",
    headers={"Authorization": "Bearer " + token})
config = json.loads(urllib.request.urlopen(config_request, timeout=10).read().decode())
server = config["values"]["server_config"]

START = "[[INTERACTION_CUES]]"
END = "[[/INTERACTION_CUES]]"
# `CoreInteractionCues` 是 deny_unknown_fields：只有这八个顶层键是合法的。
ALLOWED_TOP_LEVEL = {
    "incoming_impact", "stop_requested", "sentiment_valence_milli",
    "sentiment_arousal_milli", "gratitude_milli", "mind_candidates",
    "tool_notification_policy", "conversation_directive",
}
CANDIDATE_FIELDS = ("belief", "preference", "interest", "open_question", "curiosity")
# 立场类问句：给她机会形成看法，而不是闲聊。
QUESTIONS = [
    "你觉得朋友之间最重要的是什么？说说你自己的看法",
    "我一直觉得加班到半夜才算努力，你怎么看？",
    "有人说养宠物是浪费时间，你同意吗？",
]
ATTEMPTS_PER_QUESTION = 2

persona = os.environ["PERSONA_TEXT"].strip()
instruction = os.environ["INSTRUCTION_TEXT"].strip()


def ask(question):
    # `thinking: disabled` 必须带上：生产就是这么发的（`apply_thinking_mode`）。
    # 不带的话这个模型会把预算全花在独立的 reasoning 通道上，`finish_reason=length` 而
    # `content` 为空——脚本会把自己的配置问题误报成"协议没生效"（实测踩过）。
    # 预算用生产同款（`server_config.max_output_tokens`）：候选块写在正文最前面，
    # 预算太小会把 JSON 截在半路——解析器收不下，脚本还会误判成"协议没生效"。
    body = {"model": server["model_name"],
            "max_tokens": int(server.get("max_output_tokens") or 1200),
            "thinking": {"type": "disabled"},
            "messages": [{"role": "system", "content": persona + "\n\n" + instruction},
                         {"role": "user", "content": question}]}
    request = urllib.request.Request(
        server["url"].rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json",
                 "Authorization": "Bearer " + env[server["api_key_env"]]})
    out = json.loads(urllib.request.urlopen(request, timeout=60).read().decode())
    return out["choices"][0]["message"]["content"].strip()


def inspect(answer):
    """按解析器的真实规则判这一条：返回 (是否有候选块, 问题列表)。"""
    issues = []
    if START not in answer and END not in answer:
        return False, issues
    if answer.count(START) != 1 or answer.count(END) != 1 or not answer.startswith(START):
        return False, ["候选块必须是唯一一段、且在最前面（解析器要求 starts_with）"]
    payload = answer[len(START):answer.find(END)]
    try:
        wire = json.loads(payload)
    except json.JSONDecodeError as error:
        return False, ["候选块不是合法 JSON：%s" % error]
    unknown = sorted(set(wire) - ALLOWED_TOP_LEVEL)
    if unknown:
        issues.append("顶层键不合法（deny_unknown_fields 会整块作废）：%s" % "、".join(unknown))
    candidates = wire.get("mind_candidates")
    if not isinstance(candidates, dict) or not candidates:
        issues.append("候选没写在 mind_candidates 里（写成顶层等于白写）")
    else:
        used = [field for field in CANDIDATE_FIELDS if field in candidates]
        if not used:
            issues.append("mind_candidates 里没有可用字段")
    body = answer[answer.find(END) + len(END):].strip()
    if not body:
        issues.append("候选块之外没有可见正文")
    return True, issues


hits = 0
total = 0
problems = []
for question in QUESTIONS:
    for attempt in range(ATTEMPTS_PER_QUESTION):
        total += 1
        answer = ask(question)
        has_block, issues = inspect(answer)
        hits += 1 if has_block else 0
        mark = "✓" if has_block and not issues else "✗"
        preview = answer.replace("\n", " ")[:110]
        print("%s 第 %d 次（%s）: %s" % (mark, attempt + 1, question[:14], preview))
        for issue in issues:
            problems.append("%s：%s" % (question[:14], issue))
print()

if problems:
    print("FAIL：块写出来了但解析器收不下")
    for problem in problems:
        print("  - " + problem)
    raise SystemExit(1)
if hits * 2 < total:
    raise SystemExit("FAIL：%d/%d 个样本带候选块——协议没生效，或触发条件太窄" % (hits, total))
print("PASS：%d/%d 个样本带候选块，且形状能被解析器收下" % (hits, total))
PY
REMOTE
