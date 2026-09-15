#!/usr/bin/env bash
#
# Mind 观测验收：确认"进 Core 之前的 Mind 状态更新"不再成片丢。
#
# 为什么要它：这条链路的失败是 fail-soft 的——超时只留一行
# `Yunxi Mind event update timed out and failed soft`，回复照常发出，看回复质量或错误率
# 都发现不了。线上 2026-09-15 的基线是 **22/48 ≈ 46%** 的入站事件没进 Mind 观测，
# 而当天 `[ERROR]` 一条没有。所以判据只能是"超时数 / Mind 决策数"这个比值。
#
# 判据（每一项都能从日志复算）：
#   - 观测丢失率 : `event update timed out` / `Yunxi Mind decision`
#                  （每个进 Core 的事件都会打一条 Mind decision，所以它是分母）
#   - 事务告警   : `there is no transaction in progress` —— 超时把 observe_event 从
#                  事务中间掐断时 PG 发的 notice。**不是**超时的计数：3 天里 115 次超时
#                  只有 14 条该 notice（其中 12 条的上一行就是超时），取消正好落在事务里时
#                  才会有。分开数是因为它涨而超时不涨，说明是别处在裸回滚
#   - 资源侧排除 : `slow statement` / `acquired connection ... exceeded slow threshold`
#                  —— 这两个不为 0 时"超时"要按机器/DB 压力解释，不能只怪预算
#
# 基线（2026-09-15，改动前）：观测丢失率 46%（22/48）；同一天 slow statement 0 条。
# 改动后（配置 40 → 150ms，并去掉 resolve 阶段对 memory/goal 的白读）应当显著低于此。
#
# 用法: scripts/verify-mind-observation.sh ["3 hours ago"]
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
# 阈值：改动前是 46%，留一档余量——超过 10% 就说明"预算仍然不够"或"有别处在挤它"。
threshold="${KOVI_MIND_LOSS_THRESHOLD:-10}"

# 远端参数会被 ssh 的远端 shell 二次解析，带空格的窗口串在那里会被拆开，所以用环境变量传。
ssh -o BatchMode=yes -o ConnectTimeout=8 -p "$port" \
  "$host" "KOVI_SINCE=$(printf '%q' "$since") KOVI_THRESHOLD=$(printf '%q' "$threshold") bash -s" <<'REMOTE'
set -uo pipefail
since="${KOVI_SINCE:-3 hours ago}"
threshold="${KOVI_THRESHOLD:-10}"
log="$(mktemp)"
trap 'rm -f "$log"' EXIT
journalctl -u kovi-bot.service --since "$since" --no-pager > "$log" 2>/dev/null

# 取不到日志必须当场失败：下面全是 grep -c，空日志会让每项都是 0，包括"观测丢失率 0%"
# 这种最像"修好了"的假通过（换过 unit 名、没有 journal 读权限、窗口里服务没起过都会走到这）。
if [ ! -s "$log" ]; then
  echo "journalctl 没有取到任何日志（检查 unit 名、窗口与 journal 读权限）" >&2
  exit 1
fi

count() { grep -c "$1" "$log" 2>/dev/null || true; }

decisions="$(count 'Yunxi Mind decision')"
timeouts="$(count 'event update timed out')"
closed="$(count 'cognitive runtime is closed')"
txn="$(count 'there is no transaction in progress')"
slow="$(grep -cE 'slow statement|acquired connection, but time to acquire' "$log" 2>/dev/null || true)"
warns="$(grep -cE '\[(WARN|ERROR)\]|\[Warn\]|\[Error\]' "$log" 2>/dev/null || true)"

echo "窗口: $since   日志行数: $(wc -l < "$log")"
echo
echo "--- Mind 观测 ---"
printf '进 Core 的事件（Mind 决策）  : %s\n' "$decisions"
printf '观测超时被丢弃                : %s\n' "$timeouts"
if [ "$decisions" -gt 0 ]; then
  ratio="$(awk -v t="$timeouts" -v d="$decisions" 'BEGIN{printf "%.1f", t*100/d}')"
  printf '观测丢失率                    : %s%%（改动前基线 46%%）\n' "$ratio"
else
  ratio=""
  printf '观测丢失率                    : 无样本（窗口里没有事件）\n'
fi
printf '事务告警（超时的伴随现象）    : %s\n' "$txn"
echo
echo "--- 资源侧（用来排除「其实是机器/DB 压力」）---"
printf 'slow statement / 慢获取连接   : %s\n' "$slow"
printf 'WARN/ERROR 总条数             : %s\n' "$warns"
echo
echo "--- 其它静默丢消息路径 ---"
printf 'runtime is closed（运行时死亡）: %s\n' "$closed"

status=0
if [ "$decisions" -eq 0 ]; then
  echo
  echo "SKIP：窗口里没有进 Core 的事件，判不了丢失率（换个更长的窗口再跑）"
elif [ "$timeouts" -gt 0 ] && [ "$slow" -gt 0 ]; then
  echo
  echo "INCONCLUSIVE：超时的同时还有 $slow 条 slow statement / 慢获取连接——先按资源压力排查"
  status=1
elif awk -v r="$ratio" -v t="$threshold" 'BEGIN{exit !(r > t)}'; then
  echo
  echo "FAIL：观测丢失率 $ratio% 仍高于阈值 $threshold%（改动前 46%）"
  status=1
else
  echo
  echo "PASS：观测丢失率 $ratio%，低于阈值 $threshold%"
fi

if [ "$closed" -gt 0 ]; then
  echo "注意：窗口里出现 $closed 条 runtime is closed —— 运行时死过一次，丢消息与此无关地另算"
  status=1
fi
exit "$status"
REMOTE
