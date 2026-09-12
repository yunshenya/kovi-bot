#!/usr/bin/env bash
# 核对「仓库里的补丁」和「线上真正在跑的那份」是不是同一份。
#
# 为什么需要它：补丁有两份拷贝——仓库（真源）和服务器容器里的 /app/qq-call/。
# 只在服务器上改、忘了同步回仓库，或者往仓库加了新补丁却没加进 bridge-entry.sh
# 的 patcher 列表，都会造成"功能在仓库里、线上却根本没打"的静默漂移。
# 2026-09-12 就真的漏过一次：bridge-entry.sh 少了最后四个 patcher。
#
# 用法：
#   scripts/bridge-patches/verify-deployed.sh            # 比对全部
#   scripts/bridge-patches/verify-deployed.sh --patchers  # 只比对 patcher 列表
#
# 退出码：0 = 完全一致；1 = 有漂移（详情见输出）。
set -uo pipefail

HOST="${KOVI_BRIDGE_HOST:-ubuntu@139.155.156.152}"
CONTAINER="${KOVI_BRIDGE_CONTAINER:-napcat}"
REMOTE_DIR="${KOVI_BRIDGE_DIR:-/home/ubuntu/napcat-qq-call}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 仓库里所有需要部署到线上的文件（*.sh 里只有 bridge-entry.sh 会进容器）。
FILES=(
  bridge-entry.sh
  patch-plugin-account-path.py
  patch-20050-backoff.py
  patch-ignore-20050.py
  patch-plugin-login-refresh.py
  patch-plugin-caller-allowlist.py
  patch-plugin-hangup.py
  patch-plugin-dial.py
  patch-plugin-avsdk-trace.py
  patch-plugin-accept-retry.py
  patch-avhost-raw-preview.py
)

drift=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# 服务器上那份 bridge-entry.sh 是权威副本（容器直接挂载它）。
if ! ssh "$HOST" "cat $REMOTE_DIR/bridge-entry.sh" > "$tmp/bridge-entry.sh" 2>/dev/null; then
  echo "无法读取 $HOST:$REMOTE_DIR/bridge-entry.sh" >&2
  exit 1
fi

echo "=== 文件内容比对（仓库 vs 线上）==="
for f in "${FILES[@]}"; do
  src="$REPO_DIR/$f"
  [ -f "$src" ] || { printf '  仓库缺失              %s\n' "$f"; drift=1; continue; }
  if [ "$f" = "bridge-entry.sh" ]; then
    remote="$tmp/bridge-entry.sh"
  else
    ssh "$HOST" "sudo docker exec $CONTAINER cat /app/qq-call/$f" > "$tmp/$f" 2>/dev/null
    remote="$tmp/$f"
  fi
  if [ ! -s "$remote" ]; then
    printf '  线上缺失              %s\n' "$f"; drift=1
  elif diff -q "$src" "$remote" >/dev/null; then
    printf '  一致                  %s\n' "$f"
  else
    printf '  **不一致**            %s\n' "$f"
    diff "$remote" "$src" | head -12 | sed 's/^/      /'
    drift=1
  fi
done

if [ "${1:-}" != "--patchers" ]; then
  # patcher 列表决定开机打哪些补丁：列表里有、文件却不在，等于静默跳过。
  echo
  echo "=== 开机 patcher 列表核对 ==="
  list="$(grep -m1 '^for patcher in ' "$REPO_DIR/bridge-entry.sh" | sed 's/^for patcher in //;s/; do$//')"
  for p in $list; do
    if [ -f "$REPO_DIR/$p" ]; then
      printf '  在仓库                %s\n' "$p"
    else
      printf '  **列表有但仓库没有**  %s\n' "$p"; drift=1
    fi
  done
fi

echo
if [ "$drift" -eq 0 ]; then
  echo "结果：仓库与线上完全一致。"
else
  echo "结果：发现漂移——先把仓库或线上补齐，再重启容器重打全套补丁。" >&2
fi
exit "$drift"
