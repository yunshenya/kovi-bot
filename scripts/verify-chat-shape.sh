#!/usr/bin/env bash
#
# 对话形状验收：确认"一问一答感"的修复在生产里真的生效。
#
# 为什么要它：这次改动的效果全在"形状"上（一轮发了几个气泡、有没有真的
# 再想到一句），不是错误率，看 WARN/ERROR 看不出来。这个脚本把判据固定
# 成几条可重跑的命令，避免下次靠肉眼看日志猜。
#
# 用法: scripts/verify-chat-shape.sh ["3 hours ago"]
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

since="${*:-3 hours ago}"

# 远端参数会被 ssh 的远端 shell 二次解析，带空格的窗口串在那里会被拆开
# （`bash -s -- "3 hours ago"` 到远端只剩 "3"）。所以把窗口值用环境变量
# 传过去，远端脚本本身按原样（单引号 heredoc）执行。
ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_SINCE=$(printf '%q' "$since") bash -s" <<'REMOTE'
set -uo pipefail
since="${KOVI_SINCE:-3 hours ago}"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT
journalctl -u kovi-bot.service --since "$since" --no-pager > "$log" 2>/dev/null

echo "窗口: $since   日志行数: $(wc -l < "$log")"
echo "--- 对话形状信号 ---"
printf 'Core 私聊/点名回复          : %s\n' "$(grep -c 'Core Strong result' "$log")"
printf '登记了续聊回合 (Continue)    : %s\n' "$(grep -c 'conversation continuation registered' "$log")"
printf '自主回合被接纳               : %s\n' "$(grep -c 'autonomous conversation tick admitted' "$log")"
printf '自主回合被跳过               : %s\n' "$(grep -c 'autonomous conversation tick skipped' "$log")"
printf '气泡数超限被截断             : %s\n' "$(grep -c 'bubble budget exceeded' "$log")"
printf '[[BUBBLE]] 泄漏成可见文本    : %s\n' "$(grep -c '\[\[BUBBLE\]\]' "$log")"
printf '队列满改为折叠               : %s\n' "$(grep -c '折进当前 turn' "$log")"
echo "--- 出站账本（intent:1 = 一轮第二个气泡）---"
sudo -u postgres psql -d postgres -tAc \
  "select destination_kind, count(*) filter (where delivery_key like '%:intent:1') as multi_bubble, count(*) as total from yunxi_action_delivery_ledger where created_at > now() - interval '24 hours' group by 1"
echo "--- 告警 ---"
printf 'WARN/ERROR 条数              : %s\n' "$(grep -cE '\[(WARN|ERROR)\]' "$log")"
REMOTE
