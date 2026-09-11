#!/usr/bin/env bash
#
# 本地快速发布：Mac 上交叉编译 → 直连服务器上传 → 复用服务端原子发布与回滚。
#
# 为什么需要它：GitHub Runner 在境外，把 12~15 MB 的 release 包 scp 到国内服务器要
# 跨境走一趟，慢且不稳定。现在 GitHub Actions 只跑 CI，发布走这条本地通道；真正的
# 切换、readiness 校验和失败回滚仍然是服务端那套逻辑，发布语义没有变。
#
# 发布包只包含 kovi-bot 二进制与 REVISION 两个文件：
#   - .env / kovi.conf.toml / kovi.plugin.toml / bot.conf.toml 由服务端从上一版
#     release 复制继承，本机不需要保存生产密钥；
#   - KOVI_DEPLOY_REVISION 在服务端就地改写，readiness 文件因此仍能校验新版本。
# 需要改配置时仍走 GitHub Actions（手动 dispatch）或直接改服务器上的 current。
#
# 首次使用：
#   1) ssh-copy-id -p 22 ubuntu@<服务器>          # 装一次本机公钥，之后免密
#   2) 在仓库根目录写 server-login（已被 .gitignore 忽略）：
#        DEPLOY_HOST=ubuntu@<服务器>
#        DEPLOY_PORT=22
#   3) ./scripts/deploy-local.sh
#
# 常用参数见 --help。默认允许在工作区有未提交改动时发布，此时 revision 会带上
# -dirty.<时间戳> 后缀，方便快速迭代；要严格对齐提交请加 --require-clean。
set -euo pipefail

# 服务端布局与 systemd 单元、sudo 规则绑定，三者必须一致；不要在这里改动。
REMOTE_APP_DIR="/home/ubuntu/kovi-bot"
DEFAULT_TARGET="x86_64-unknown-linux-gnu"
DEFAULT_SSH_PORT="22"

step() { printf '\n==> %s\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '    [警告] %s\n' "$*" >&2; }
die() { printf '\n错误: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
用法: scripts/deploy-local.sh [选项]

选项:
  --host <user@host>   发布目标，默认取 server-login 或环境变量 DEPLOY_HOST
  --port <n>           SSH 端口，默认 22
  --target <triple>    交叉编译目标，默认 x86_64-unknown-linux-gnu
  --no-build           跳过编译，直接使用现有 target/<triple>/release/kovi-bot
  --require-clean      工作区有未提交改动时拒绝发布
  --install-service    同步 systemd 单元与 sudoers（需要交互输入服务器 sudo 密码）
  --password-auth      不用公钥，改为交互输入服务器密码（只需输一次，连接复用）
  --dry-run            只编译、打包、校验，不上传不切换
  --keep-archive       保留本地打包出的 tar.gz（默认删除）
  -h, --help           显示本帮助

发布流程:
  1. git rev-parse HEAD 作为 revision（脏工作区追加 -dirty.<时间戳>）
  2. cargo zigbuild --release --locked --target <triple>
  3. 打包含二进制与 REVISION，scp 到 <app_dir>/incoming/
  4. 服务端解包、继承上一版配置、原子切换 current、重启并等待 readiness
  5. 未就绪则整体回滚到上一版 release，并保留最近 5 个 release
EOF
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
script_start="$(date +%s)"

# ---------------------------------------------------------------- 参数解析
config_file="$repo_root/server-login"
if [ -f "$config_file" ]; then
  # shellcheck disable=SC1090
  set -a && . "$config_file" && set +a
fi

host="${DEPLOY_HOST:-}"
port="${DEPLOY_PORT:-$DEFAULT_SSH_PORT}"
target="${KOVI_DEPLOY_TARGET:-$DEFAULT_TARGET}"
do_build=1
require_clean=0
dry_run=0
keep_archive=0
install_service_flag=0
password_auth=0

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
  --target)
    [ $# -ge 2 ] || die "--target 需要参数"
    target="$2"
    shift 2
    ;;
  --target=*)
    target="${1#*=}"
    shift
    ;;
  --no-build)
    do_build=0
    shift
    ;;
  --require-clean)
    require_clean=1
    shift
    ;;
  --dry-run)
    dry_run=1
    shift
    ;;
  --keep-archive)
    keep_archive=1
    shift
    ;;
  --install-service)
    install_service_flag=1
    shift
    ;;
  --password-auth)
    password_auth=1
    shift
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    die "未知参数: $1"
    ;;
  esac
done

# ---------------------------------------------------------------- 预检
step "预检"

[ -n "$host" ] || die "没有配置发布目标。请在仓库根目录创建 server-login（已被 .gitignore 忽略）：
    DEPLOY_HOST=ubuntu@<服务器>
    DEPLOY_PORT=22
  或者用 --host ubuntu@<服务器> 指定。"

case "$host" in
*@*) ;;
*) die "--host 需要 user@host 形式，当前是 $host" ;;
esac
host_name="${host##*@}"
host_user="${host%@*}"
case "$host_name" in
*[!A-Za-z0-9._-]*) die "服务器名含非法字符: $host_name" ;;
esac
case "$host_user" in
*[!A-Za-z0-9._-]*) die "登录用户含非法字符: $host_user" ;;
esac
case "$port" in
*[!0-9]* | "") die "端口必须是数字: $port" ;;
esac

for tool in git cargo ssh scp tar install; do
  command -v "$tool" >/dev/null 2>&1 || die "缺少命令 $tool"
done
if [ "$do_build" = 1 ]; then
  command -v cargo-zigbuild >/dev/null 2>&1 ||
    die "缺少 cargo-zigbuild，请先 cargo install cargo-zigbuild"
  command -v zig >/dev/null 2>&1 || die "缺少 zig，请先 brew install zig"
fi

if command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
else
  sha256_of() { printf 'unavailable'; }
fi

info "目标: $host_user@$host_name:$port  发布目录 $REMOTE_APP_DIR"
info "交叉编译目标: $target"

# SSH 连接复用：一次握手贯穿后面所有 ssh/scp；配 --password-auth 时也就只问一次密码。
ctl_dir="$(mktemp -d "${TMPDIR:-/tmp}/kovi-ssh.XXXXXX")"
control_path="$ctl_dir/master"
work_dir=""

cleanup_all() {
  if [ -S "$control_path" ]; then
    ssh -O exit -o ControlPath="$control_path" -p "$port" "$host" >/dev/null 2>&1 || true
  fi
  rm -rf "$ctl_dir"
  [ -n "${work_dir:-}" ] && rm -rf "$work_dir"
  return 0
}
trap cleanup_all EXIT

ssh_opts=(-o ConnectTimeout=10 -o ServerAliveInterval=15 \
  -o StrictHostKeyChecking=accept-new \
  -o ControlMaster=auto -o ControlPath="$control_path" -o ControlPersist=60)
if [ "$password_auth" = 1 ]; then
  info "认证方式: 交互输入服务器密码（--password-auth，只问一次）"
else
  ssh_opts+=(-o BatchMode=yes)
fi

ssh_ok=0
if [ "$password_auth" = 1 ]; then
  ssh "${ssh_opts[@]}" -p "$port" "$host" true && ssh_ok=1 || ssh_ok=0
else
  ssh "${ssh_opts[@]}" -p "$port" "$host" true 2>/dev/null && ssh_ok=1 || ssh_ok=0
fi
if [ "$ssh_ok" = 1 ]; then
  info "SSH 登录正常"
elif [ "$dry_run" = 1 ]; then
  warn "无法登录 ${host}；--dry-run 继续，只做本地编译与打包"
  warn "真要发布时先执行一次: ssh-copy-id -p $port ${host}（或用 --password-auth 输密码）"
else
  die "无法登录 ${host}。两种办法：
    1) 装公钥（推荐，之后免密）: ssh-copy-id -p $port $host
    2) 每次输密码: $0 --password-auth"
fi

if [ "$do_build" = 0 ]; then
  info "跳过编译（--no-build）"
fi

# ---------------------------------------------------------------- revision
step "确定发布版本"

head_sha="$(git rev-parse HEAD)"
case "$head_sha" in
*[!0-9a-f]* | "") die "git rev-parse HEAD 结果异常: $head_sha" ;;
esac
[ "${#head_sha}" -eq 40 ] || die "git rev-parse HEAD 不是完整 SHA: $head_sha"

pending="$(git status --porcelain)"
revision="$head_sha"
if [ -n "$pending" ]; then
  if [ "$require_clean" = 1 ]; then
    printf '%s\n' "$pending" >&2
    die "工作区有未提交改动，--require-clean 拒绝发布"
  fi
  revision="$head_sha-dirty.$(date +%s)"
  warn "工作区有未提交改动，revision 记为 $revision"
  info "（这些改动会进二进制；要严格对齐提交请先 commit 或加 --require-clean）"
  pending_count="$(printf '%s\n' "$pending" | wc -l | tr -d ' ')"
  info "未提交条目: $pending_count"
else
  info "工作区干净，revision = $revision"
fi

# ---------------------------------------------------------------- 编译
step "交叉编译 release 二进制"

binary="target/$target/release/kovi-bot"
build_seconds=0
if [ "$do_build" = 1 ]; then
  build_start="$(date +%s)"
  cargo zigbuild --release --locked --target "$target"
  build_seconds=$(( $(date +%s) - build_start ))
  info "编译耗时 ${build_seconds} 秒"
fi

[ -f "$binary" ] || die "找不到二进制 ${binary}（去掉 --no-build 再试）"
if [ "$do_build" = 0 ]; then
  binary_epoch="$(date -r "$binary" +%s 2>/dev/null || echo 0)"
  head_epoch="$(git log -1 --format=%ct)"
  info "复用产物时间: $(date -r "$binary" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || echo 未知)"
  info "HEAD 提交时间: $(git log -1 --format=%cd --date=format:'%Y-%m-%d %H:%M:%S')"
  if [ "$binary_epoch" -lt "$head_epoch" ]; then
    warn "复用产物早于 HEAD 提交，可能不是这次代码编出来的；不确定就去掉 --no-build"
  fi
fi
file_desc="$(file -b "$binary")"
case "$target" in
x86_64-*)
  case "$file_desc" in
  *"ELF 64-bit LSB"*x86-64*) ;;
  *) die "产物架构不符合预期: $file_desc" ;;
  esac
  ;;
aarch64-*)
  case "$file_desc" in
  *"ELF 64-bit LSB"*"ARM aarch64"*) ;;
  *) die "产物架构不符合预期: $file_desc" ;;
  esac
  ;;
esac
info "产物: $file_desc"

# ---------------------------------------------------------------- 打包
step "打包 release"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/kovi-deploy.XXXXXX")"
staging_dir="$work_dir/release"
archive="$repo_root/target/kovi-release-$revision.tar.gz"

install -d -m 700 "$staging_dir"
install -m 0755 "$binary" "$staging_dir/kovi-bot"
printf '%s\n' "$revision" >"$staging_dir/REVISION"
# 模板只是给服务端做“继承来的配置缺不缺新键”的提示用，解包后即删除（仓库公开文件，不含密钥）。
install -m 0644 bot.conf.example.toml "$staging_dir/bot.conf.example.toml"
# macOS 的 install/cp 会把 com.apple.provenance 之类的 xattr 一起带过去，Linux 端解包时
# 会打印 "Ignoring unknown extended header keyword"；发布包里不需要本机元数据。
if command -v xattr >/dev/null 2>&1; then
  for staged in "$staging_dir/kovi-bot" "$staging_dir/REVISION" "$staging_dir/bot.conf.example.toml"; do
    xattr -c "$staged" 2>/dev/null || true
  done
fi
tar -C "$staging_dir" -czf "$archive" .
rm -rf "$staging_dir"

archive_bytes="$(wc -c <"$archive" | tr -d ' ')"
archive_mb="$(awk -v b="$archive_bytes" 'BEGIN { printf "%.1f", b / 1048576 }')"
archive_sha="$(sha256_of "$archive")"
info "包大小 $archive_mb MiB  sha256 $archive_sha"

if [ "$dry_run" = 1 ]; then
  step "dry-run 结束"
  info "已跳过上传与切换；包留在 $archive"
  exit 0
fi

# ---------------------------------------------------------------- 可选：同步服务文件
if [ "$install_service_flag" = 1 ]; then
  step "同步 systemd 单元与 sudo 规则（需要 sudo 密码）"
  remote_tmp="/tmp/kovi-bot-deploy-$revision"
  ssh "${ssh_opts[@]}" -p "$port" "$host" "install -d -m 700 '$remote_tmp'"
  scp "${ssh_opts[@]}" -P "$port" \
    .github/deploy/kovi-bot.service .github/deploy/kovi-bot.sudoers \
    "$host:$remote_tmp/"
  # sudo 需要终端输入密码，这里保留 -t 交互。
  ssh -t "${ssh_opts[@]}" -p "$port" "$host" "sudo bash -s" <<REMOTE
set -euo pipefail
visudo -cf "$remote_tmp/kovi-bot.sudoers"
install -o root -g root -m 0644 "$remote_tmp/kovi-bot.service" /etc/systemd/system/kovi-bot.service
install -o root -g root -m 0440 "$remote_tmp/kovi-bot.sudoers" /etc/sudoers.d/kovi-bot-deploy
visudo -cf /etc/sudoers.d/kovi-bot-deploy
systemctl daemon-reload
systemctl enable kovi-bot.service
rm -rf -- "$remote_tmp"
REMOTE
else
  # 单元文件漂移只提示，不擅自改；需要时用 --install-service。文件是 0644，普通用户可读。
  remote_unit="$(ssh "${ssh_opts[@]}" -p "$port" "$host" \
    'cat /etc/systemd/system/kovi-bot.service 2>/dev/null || true')"
  if [ -n "$remote_unit" ] && [ "$remote_unit" != "$(cat .github/deploy/kovi-bot.service)" ]; then
    warn "线上 systemd 单元与仓库内 .github/deploy/kovi-bot.service 不一致，需要时运行 --install-service"
  fi
fi

# ---------------------------------------------------------------- 上传
step "上传到服务器"

ssh "${ssh_opts[@]}" -p "$port" "$host" bash -s -- "$REMOTE_APP_DIR" "$host_user" <<'REMOTE'
set -euo pipefail
app_dir="$1"
deploy_user="$2"
test "$app_dir" = "/home/ubuntu/kovi-bot"
test "$(id -un)" = "$deploy_user"
install -d -m 0700 "$app_dir/incoming"
install -d -m 0700 "$app_dir/runtime"
install -d -m 0750 "$app_dir/releases"
# 清理一天前的失败残留，成功路径会在激活后删掉本次包。
find "$app_dir/incoming" -mindepth 1 -maxdepth 1 -type f -name '*.tar.gz' -mtime +1 -delete
echo "[deploy] 当前 release: $(readlink -f "$app_dir/current" 2>/dev/null || echo 无)"
# 切换前先确认 sudo 免密 restart 还在（规则由 .github/deploy/kovi-bot.sudoers 安装）。
if sudo -n -l /usr/bin/systemctl restart kovi-bot.service >/dev/null 2>&1; then
  echo "[deploy] 免密 systemctl restart 可用"
else
  echo "[deploy] 警告: 无法确认免密 restart 规则；重启失败会自动回滚，需要时用 --install-service 同步" >&2
fi
REMOTE

upload_start="$(date +%s)"
scp "${ssh_opts[@]}" -P "$port" "$archive" \
  "$host:$REMOTE_APP_DIR/incoming/$revision.tar.gz"
upload_seconds=$(( $(date +%s) - upload_start ))
[ "$upload_seconds" -gt 0 ] || upload_seconds=1
info "上传耗时 ${upload_seconds} 秒（$(awk -v b="$archive_bytes" -v s="$upload_seconds" \
  'BEGIN { printf "%.2f", b / 1048576 / s }') MiB/s）"

# ---------------------------------------------------------------- 激活
step "服务端原子切换并等待 readiness"

ssh "${ssh_opts[@]}" -p "$port" "$host" bash -s -- \
  "$REMOTE_APP_DIR" "$revision" "$host_user" <<'REMOTE'
set -euo pipefail
app_dir="$1"
revision="$2"
deploy_user="$3"
test "$app_dir" = "/home/ubuntu/kovi-bot"
case "$revision" in
  [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*)
    case "$revision" in
      *[!0-9a-f.-]*) echo "非法 revision: $revision" >&2; exit 1 ;;
    esac
    ;;
  *) echo "非法 revision: $revision" >&2; exit 1 ;;
esac
test "$(id -un)" = "$deploy_user"

archive="$app_dir/incoming/$revision.tar.gz"
releases_dir="$app_dir/releases"
release="$releases_dir/$revision"
staging="$releases_dir/$revision.tmp"
ready_file="$app_dir/runtime/ready"
previous=""
if test -L "$app_dir/current"; then
  previous="$(readlink -f "$app_dir/current")"
  case "$previous" in
    "$releases_dir"/*) ;;
    *) previous="" ;;
  esac
fi

cleanup_upload() {
  rm -f -- "$archive"
  if test -d "$staging"; then
    find "$staging" -depth -delete
  fi
}
trap cleanup_upload EXIT

make_service_read_only() {
  target="$1"
  case "$target" in
    "$releases_dir"/*) ;;
    *) echo "拒绝改动 releases 之外的权限: $target" >&2; exit 1 ;;
  esac
  find "$target" -type d -exec chmod 0750 {} +
  find "$target" -type f -exec chmod 0640 {} +
  chmod 0550 "$target/kovi-bot"
  test -z "$(find "$target" ! -type l \( -perm -0020 -o -perm -0002 \) -print -quit)"
}

ensure_model_link() {
  target="$1"
  case "$target" in
    "$releases_dir"/*) ;;
    *) echo "拒绝改动 releases 之外的路径: $target" >&2; exit 1 ;;
  esac
  model_path="$target/models"
  if test -L "$model_path"; then
    test "$(readlink "$model_path")" = "$app_dir/models" || {
      echo "release 里的 models 链接指向异常: $target" >&2
      exit 1
    }
  elif test -e "$model_path"; then
    echo "拒绝覆盖已存在的 models 目录: $target" >&2
    exit 1
  else
    ln -s "$app_dir/models" "$model_path"
  fi
}

if ! test -d "$release"; then
  test -f "$archive" || { echo "缺少上传包: $archive" >&2; exit 1; }
  if test -d "$staging"; then
    find "$staging" -depth -delete
  fi
  install -d -m 750 "$staging"
  tar --no-same-owner -xzf "$archive" -C "$staging"
  test -x "$staging/kovi-bot"
  host_arch="$(uname -m)"
  binary_format="$(file -b "$staging/kovi-bot")"
  case "$host_arch" in
    x86_64)
      [[ "$binary_format" == *"ELF 64-bit LSB pie executable, x86-64"* ]] || {
        echo "二进制架构与主机不符: host=$host_arch binary=$binary_format" >&2
        exit 1
      }
      ;;
    aarch64 | arm64)
      [[ "$binary_format" == *"ELF 64-bit LSB pie executable, ARM aarch64"* ]] || {
        echo "二进制架构与主机不符: host=$host_arch binary=$binary_format" >&2
        exit 1
      }
      ;;
    *)
      echo "不支持的部署主机架构: $host_arch" >&2
      exit 1
      ;;
  esac
  test "$(cat "$staging/REVISION")" = "$revision"

  # 发布包只带二进制，运行配置从上一版 release 继承；找不到就退到最新的一个 release。
  inherit_from="$previous"
  if test -z "$inherit_from" || ! test -d "$inherit_from"; then
    inherit_from="$(find "$releases_dir" -mindepth 1 -maxdepth 1 -type d \
      ! -name '*.tmp' -printf '%T@ %p\n' | sort -nr | head -n 1 | cut -d' ' -f2-)"
  fi
  test -n "$inherit_from" && test -d "$inherit_from" || {
    echo "没有可继承配置的历史 release。" >&2
    echo "请先用 GitHub Actions 的 Deploy production（手动 dispatch）发布一次，或手工准备 .env 与 *.toml。" >&2
    exit 1
  }
  for name in .env kovi.conf.toml kovi.plugin.toml bot.conf.toml; do
    test -s "$inherit_from/$name" || { echo "上一版 release 缺少 $name" >&2; exit 1; }
    cp -p -- "$inherit_from/$name" "$staging/$name"
  done
  chmod 0600 "$staging/.env"
  chmod 0640 "$staging"/*.toml
  if grep -q '^KOVI_DEPLOY_REVISION=' "$staging/.env"; then
    sed -i "s|^KOVI_DEPLOY_REVISION=.*$|KOVI_DEPLOY_REVISION=$revision|" "$staging/.env"
  else
    printf 'KOVI_DEPLOY_REVISION=%s\n' "$revision" >>"$staging/.env"
  fi
  grep -Fqx "KOVI_DEPLOY_REVISION=$revision" "$staging/.env" || {
    echo "无法把 KOVI_DEPLOY_REVISION 改写为 $revision" >&2
    exit 1
  }

  # 继承来的配置可能缺少这次代码新加的键：只提示，不擅自覆盖（改配置要走 Actions 或手工）。
  if test -s "$staging/bot.conf.example.toml"; then
    example_keys="$(mktemp)"
    config_keys="$(mktemp)"
    { grep -oE '^[a-z_][a-z0-9_]*[[:space:]]*=' "$staging/bot.conf.example.toml" || true; } |
      tr -d ' =' | sort -u >"$example_keys"
    { grep -oE '^[a-z_][a-z0-9_]*[[:space:]]*=' "$staging/bot.conf.toml" || true; } |
      tr -d ' =' | sort -u >"$config_keys"
    missing_keys="$(comm -23 "$example_keys" "$config_keys" | tr '\n' ' ')"
    rm -f -- "$example_keys" "$config_keys" "$staging/bot.conf.example.toml"
    if test -n "$missing_keys"; then
      echo "[deploy] 提示: 继承的配置缺少模板里的键（最多列 12 个）: $(echo "$missing_keys" | cut -d' ' -f1-12)" >&2
      echo "[deploy] 这些键不会被本地发布带上去；需要时用 GitHub Actions 手动发布重新生成配置。" >&2
    fi
  fi

  make_service_read_only "$staging"
  mv "$staging" "$release"
  echo "[deploy] 解包完成，配置继承自 $(basename "$inherit_from")"
fi

ensure_model_link "$release"
if test -n "$previous" && test -d "$previous"; then
  ensure_model_link "$previous"
fi
make_service_read_only "$release"
test -x "$release/kovi-bot"
test "$(cat "$release/REVISION")" = "$revision"
test -s "$release/.env"
test -s "$release/kovi.conf.toml"
test -s "$release/kovi.plugin.toml"
test -L "$release/models"
test "$(readlink "$release/models")" = "$app_dir/models"

rm -f -- "$app_dir/current.next"
ln -s "$release" "$app_dir/current.next"
mv -Tf "$app_dir/current.next" "$app_dir/current"
rm -f -- "$ready_file"

ready=0
if sudo -n /usr/bin/systemctl restart kovi-bot.service; then
  for _ in $(seq 1 90); do
    if systemctl is-active --quiet kovi-bot.service &&
      test -f "$ready_file" &&
      grep -Fqx "$revision" "$ready_file"; then
      ready=1
      break
    fi
    sleep 1
  done
fi

if test "$ready" -ne 1; then
  if test -n "$previous" && test -d "$previous"; then
    rm -f -- "$app_dir/current.rollback"
    ln -s "$previous" "$app_dir/current.rollback"
    mv -Tf "$app_dir/current.rollback" "$app_dir/current"
    rm -f -- "$ready_file"
    sudo -n /usr/bin/systemctl restart kovi-bot.service || true
    echo "[deploy] 新版本未就绪，已整体回滚到 $(basename "$previous")" >&2
  else
    echo "[deploy] 新版本未就绪，且没有可回滚的上一版" >&2
  fi
  exit 1
fi

mapfile -t old_releases < <(
  find "$releases_dir" -mindepth 1 -maxdepth 1 -type d \
    ! -name '*.tmp' -printf '%T@ %p\n' | sort -nr | tail -n +6 | cut -d' ' -f2-
)
for candidate in "${old_releases[@]}"; do
  case "$candidate" in
    "$releases_dir"/*)
      if test "$candidate" != "$release" && test "$candidate" != "$previous"; then
        find "$candidate" -depth -delete
      fi
      ;;
  esac
done
echo "[deploy] $revision 已切换并写入 readiness"
REMOTE

# ---------------------------------------------------------------- 结果
step "发布完成"

if [ "$keep_archive" = 1 ]; then
  info "本地包保留在 $archive"
else
  rm -f "$archive"
fi

ssh "${ssh_opts[@]}" -p "$port" "$host" \
  "systemctl --no-pager --full -n 0 status kovi-bot.service; \
   printf '\n[diag] 最近 200 行日志中的 WARN/ERROR 条数: '; \
   journalctl -u kovi-bot.service -n 200 --no-pager -o cat 2>/dev/null \
     | grep -cE '^\[(WARN|ERROR)\]' || true" || true

info "revision: $revision"
info "总耗时 $(( $(date +%s) - script_start )) 秒（编译 $(( ${build_seconds:-0} )) 秒 + 上传 ${upload_seconds} 秒，其余是服务端切换与 readiness）"
info "回滚目标: $REMOTE_APP_DIR/releases/ 里的上一个 release（或把 current 指回去后重启）"
