#!/usr/bin/env bash
# 一次性数据修正的执行器：清掉由 legacy 关系等级"投影"出来的关系张力。
#
# 背景与判据见同目录的 backfill-relation-tension-seed.sql。
#
# 用法:
#   scripts/backfill-relation-tension-seed.sh                 # dry-run（默认）：列出会改的行，回滚
#   scripts/backfill-relation-tension-seed.sh --apply         # 真写：回写 tension 并留下台账
#   scripts/backfill-relation-tension-seed.sh --host user@ip  # 覆盖发布目标
#
# 目标主机与 SSH 约定跟 scripts/deploy-local.sh 一致（先读 server-login）；
# DATABASE_URL 取服务器上 current/.env，不在本机落任何凭据。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sql_file="$repo_root/scripts/backfill-relation-tension-seed.sql"
remote_base="/home/ubuntu/kovi-bot"
DEFAULT_SSH_PORT="22"

info() { printf '\033[32m[信息]\033[0m %s\n' "$*"; }
die() {
  printf '\033[31m[错误]\033[0m %s\n' "$*" >&2
  exit 1
}

config_file="$repo_root/server-login"
if [ -f "$config_file" ]; then
  # shellcheck disable=SC1090
  set -a && . "$config_file" && set +a
fi

host="${DEPLOY_HOST:-}"
port="${DEPLOY_PORT:-$DEFAULT_SSH_PORT}"
apply=0

while [ $# -gt 0 ]; do
  case "$1" in
  --host)
    [ $# -ge 2 ] || die "--host 需要参数"
    host="$2"
    shift 2
    ;;
  --host=*)
    host="${1#*=}"
    shift
    ;;
  --port)
    [ $# -ge 2 ] || die "--port 需要参数"
    port="$2"
    shift 2
    ;;
  --port=*)
    port="${1#*=}"
    shift
    ;;
  --apply)
    apply=1
    shift
    ;;
  -h | --help)
    sed -n '2,12p' "${BASH_SOURCE[0]}"
    exit 0
    ;;
  *) die "未知参数: $1（可用: --apply / --host / --port）" ;;
  esac
done

[ -n "$host" ] || die "没有发布目标：请在 server-login 里写 DEPLOY_HOST，或用 --host user@host"
[ -f "$sql_file" ] || die "找不到 SQL: $sql_file"
command -v ssh >/dev/null || die "缺少 ssh"
command -v scp >/dev/null || die "缺少 scp"

if [ "$apply" = 1 ]; then
  mode=on
  info "模式: --apply（会真的回写 tension）"
else
  mode=off
  info "模式: dry-run（跑完回滚，只打印会改哪些行；加 --apply 才真写）"
fi

# SSH 连接复用：一次握手贯穿上传与执行；退出时顺手清掉远端临时文件。
ctl_dir="$(mktemp -d "${TMPDIR:-/tmp}/kovi-backfill-ssh.XXXXXX")"
control_path="$ctl_dir/master"
remote_tmp="/tmp/kovi-backfill-relation-tension-seed.$$.sql"
remote_uploaded=0

cleanup_all() {
  if [ "$remote_uploaded" = 1 ] && [ -S "$control_path" ]; then
    ssh -o ControlPath="$control_path" -p "$port" "$host" "rm -f '$remote_tmp'" >/dev/null 2>&1 || true
  fi
  if [ -S "$control_path" ]; then
    ssh -O exit -o ControlPath="$control_path" -p "$port" "$host" >/dev/null 2>&1 || true
  fi
  rm -rf "$ctl_dir"
  return 0
}
trap cleanup_all EXIT

ssh_opts=(-o ConnectTimeout=10 -o ServerAliveInterval=15 \
  -o StrictHostKeyChecking=accept-new -o BatchMode=yes \
  -o ControlMaster=auto -o ControlPath="$control_path" -o ControlPersist=60)

ssh "${ssh_opts[@]}" -p "$port" "$host" true 2>/dev/null ||
  die "无法登录 ${host}。先执行一次: ssh-copy-id -p $port ${host}"
info "SSH 登录正常: $host"

scp "${ssh_opts[@]}" -P "$port" "$sql_file" "$host:$remote_tmp" >/dev/null
remote_uploaded=1
info "SQL 已上传：$remote_tmp"

ssh "${ssh_opts[@]}" -p "$port" "$host" \
  "set -a; . '$remote_base/current/.env' >/dev/null 2>&1; set +a; \
   [ -n \"\${DATABASE_URL:-}\" ] || { echo 'current/.env 里没有 DATABASE_URL' >&2; exit 1; }; \
   psql \"\$DATABASE_URL\" -v apply=$mode -f '$remote_tmp'"

if [ "$apply" = 1 ]; then
  info "已提交。台账表 yunxi_relation_tension_seed_backfill 记录了每一行的修正前值。"
else
  info "dry-run 结束，库里没有任何改动。确认无误后加 --apply 执行。"
fi
