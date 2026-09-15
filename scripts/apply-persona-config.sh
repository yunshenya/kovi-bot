#!/usr/bin/env bash
#
# 把仓库里的 `[prompt]` 段推到生产（写进运行时 override，热加载、不重启）。
#
# 为什么需要它：人格统一之后，配置里多了唯一一份 `persona`，而 `system_prompt` /
# `private_prompt` 从"人格 + 场景"降级成"只写场景差异"。线上那份还是旧的两段全文——
# 不换的话新代码会拼成"persona + 旧全文"，人格在一轮里出现两遍。
#
# 为什么走 override 而不是改发布目录：`current/` 是只读发布目录，且下次发布会被替换；
# `runtime/bot.conf.override.toml` 不随发布丢失，管理后台走的也是这条"校验 → 备份 →
# 原子写 → 热加载"的路。
#
# **默认只演练**：打印将要写入的 `[prompt]` 段与现有段，不动线上。确认无误后加 `--apply`。
#
# 用法:
#   scripts/apply-persona-config.sh            # 演练：只看会写什么
#   scripts/apply-persona-config.sh --apply    # 真写（服务端自动留备份）
set -euo pipefail

apply=0
for arg in "$@"; do
  case "$arg" in
    --apply) apply=1 ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "未知参数：$arg（只支持 --apply）" >&2; exit 1 ;;
  esac
done

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
# `[prompt]` 段从仓库的 bot.conf.toml 里取，不在脚本里再抄一份：抄一份就会漂移。
prompt_b64="$(python3 - "$repo_root/bot.conf.toml" <<'PY' | base64 | tr -d '\n'
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'\[prompt\]\n(?:.*\n)*?(?=\n\[|\Z)', text)
if match is None:
    raise SystemExit("bot.conf.toml 里找不到 [prompt] 段")
sys.stdout.write(match.group(0).rstrip() + "\n")
PY
)"

ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_PROMPT_B64=$prompt_b64 KOVI_APPLY=$apply bash -s" <<'REMOTE'
set -euo pipefail
export PROMPT_BLOCK="$(printf '%s' "$KOVI_PROMPT_B64" | base64 -d)"
export APPLY="${KOVI_APPLY:-0}"

python3 <<'PY'
import json, os, re, urllib.request

TOKEN_PATH = "/home/ubuntu/kovi-bot/runtime/.yunxi-admin-token"
ENDPOINT = "http://127.0.0.1:6098/api/config/file/bot.conf.override.toml"
token = open(TOKEN_PATH, encoding="utf-8").read().strip()

def call(method, body=None):
    request = urllib.request.Request(
        ENDPOINT,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"},
        method=method)
    return json.loads(urllib.request.urlopen(request, timeout=15).read().decode())

raw = call("GET")["raw"]
block = os.environ["PROMPT_BLOCK"].strip() + "\n"

current = re.search(r'\[prompt\]\n(?:.*\n)*?(?=\n\[|\Z)', raw)
if current:
    print("--- 线上 override 里现有的 [prompt] 段 ---")
    print(current.group(0).rstrip())
    updated = raw[:current.start()] + block + raw[current.end():]
else:
    print("--- 线上 override 里还没有 [prompt] 段（目前用主配置里那份旧的）---")
    updated = raw.rstrip() + "\n\n" + block

print()
print("--- 将要写入的 [prompt] 段 ---")
print(block.rstrip())
print()
print("人格字段长度：persona=%d 字，system_prompt=%d 字，private_prompt=%d 字" % (
    len(re.search(r'persona = "(.*?)"', block, re.S).group(1)),
    len(re.search(r'system_prompt = "(.*?)"', block, re.S).group(1)),
    len(re.search(r'private_prompt = "(.*?)"', block, re.S).group(1)),
))

if os.environ.get("APPLY") != "1":
    print()
    print("演练结束：没有写入。确认无误后加 --apply。")
    raise SystemExit(0)

result = call("PUT", {"raw": updated})
print()
print("写入结果：ok=%s reloaded=%s backup=%s" % (
    result.get("ok"), result.get("reloaded"), result.get("backup")))
PY
REMOTE
