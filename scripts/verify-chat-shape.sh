#!/usr/bin/env bash
#
# 对话形状验收：确认"一问一答感"的修复在生产里真的生效。
#
# 为什么要它：这次改动的效果全在"形状"上（一轮发了几个气泡、有没有真的
# 再想到一句），不是错误率，看 WARN/ERROR 看不出来。这个脚本把判据固定
# 成几条可重跑的命令，避免下次靠肉眼看日志猜。
#
# 指标口径（每一项都能从日志或账本复算）：
#   - 提问占比   : Yunxi Core turn shape 里 asks=true / 全部可见回合
#   - 连续气泡占比: 账本 delivery_key 以 :intent:1 结尾的行 / 全部出站行
#   - 续聊登记率 : conversation continuation registered / directive=Continue 的回合
#   - 续聊成功率 : autonomous conversation tick admitted / 登记数
#   - 泄漏       : 日志里出现字面 [[BUBBLE]]（应当恒为 0）
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

count() { grep -c "$1" "$log" 2>/dev/null || true; }

echo "窗口: $since   日志行数: $(wc -l < "$log")"
echo
echo "--- 对话形状（来自 Yunxi Core turn shape，2026-09-13 起有）---"
shapes="$(grep -c 'Yunxi Core turn shape' "$log" 2>/dev/null || true)"
asks="$(grep 'Yunxi Core turn shape' "$log" 2>/dev/null | grep -c 'asks=true' || true)"
bubbles2="$(grep 'Yunxi Core turn shape' "$log" 2>/dev/null | grep -cE 'bubbles=[23]' || true)"
printf '可见回合数                  : %s\n' "$shapes"
printf '提问占比                    : %s / %s\n' "$asks" "$shapes"
printf '一轮多气泡回合              : %s / %s\n' "$bubbles2" "$shapes"
# 回复延迟：从收到消息到写出正文（含语义判定与限流），不是模型调用耗时。
latencies="$(grep -o 'think_ms=[0-9]*' "$log" 2>/dev/null | cut -d= -f2 | sort -n || true)"
if [ -n "$latencies" ]; then
  printf '回复延迟 中位/ p90 (ms)      : %s / %s\n' \
    "$(printf '%s\n' "$latencies" | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')" \
    "$(printf '%s\n' "$latencies" | awk '{a[NR]=$1} END{print a[int(NR*0.9)]}')"
else
  printf '回复延迟 中位/ p90 (ms)      : 无样本\n'
fi
echo
echo "--- 续聊链路 ---"
printf '登记了续聊回合 (Continue)    : %s\n' "$(count 'conversation continuation registered')"
printf '续聊被降级/未登记            : %s\n' "$(grep -cE 'continuation request was (downgraded|not registered)' "$log" 2>/dev/null || true)"
printf '自主回合被接纳               : %s\n' "$(count 'autonomous conversation tick admitted')"
printf '自主回合被跳过               : %s\n' "$(count 'autonomous conversation tick skipped')"
echo
echo "--- 健康与泄漏 ---"
printf '气泡数超限被截断             : %s\n' "$(count 'bubble budget exceeded')"
printf '[[BUBBLE]] 泄漏成可见文本    : %s\n' "$(count '\[\[BUBBLE\]\]')"
printf '队列满改为折叠               : %s\n' "$(count '折进当前 turn')"
printf 'WARN/ERROR 条数              : %s\n' "$(grep -cE '\[(WARN|ERROR)\]' "$log" 2>/dev/null || true)"
echo
echo "--- 出站账本（intent:1 = 一轮第二个气泡）---"
sudo -u postgres psql -d postgres -tAc \
  "select destination_kind, count(*) filter (where delivery_key like '%:intent:1') as multi_bubble, count(*) as total from yunxi_action_delivery_ledger where created_at > now() - interval '24 hours' group by 1"
REMOTE
