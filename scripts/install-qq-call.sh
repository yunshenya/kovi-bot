#!/usr/bin/env bash
#
# QQ 语音通话：服务器侧预检与桥接安装。
#
# 这个脚本不实现通话能力本身，它只做三件事：
#
#   1. 探测服务器上的 Linux QQ 与 NapCat，确认版本和 AVSDK 满足桥的要求；
#   2. 安装桥需要的系统依赖（PulseAudio、xvfb）；
#   3. 拉取并调用上游 NapCat AV 桥的官方安装器，把通话信令与音频设备接好。
#
# 桥是独立的上游项目（GPL-3.0），以独立进程方式安装，本仓库不分发也不修改它。
# 默认只做预检；真正落盘需要显式 --apply。
#
# 用法：
#   scripts/install-qq-call.sh                 # 只预检
#   scripts/install-qq-call.sh --apply         # 预检通过后安装依赖与桥
#   scripts/install-qq-call.sh --apply --yes   # 跳过 apt 确认
#
set -euo pipefail

BRIDGE_REPO="https://github.com/ClaudiaGardner/maibot-qq-voice-call.git"
# 固定到验证过的上游提交，避免上游改动直接改变生产行为。
BRIDGE_REF="22f30c021cd3170f75af9dff66cc959a07ebda4b"
BRIDGE_MIN_NAPCAT="4.14.0"
BRIDGE_SRC_DIR="${BRIDGE_SRC_DIR:-$HOME/.cache/qq-voice-bridge-src}"
INSTALL_DIR="${MAIBOT_QQ_CALL_BRIDGE_DIR:-$HOME/.local/share/maibot-qq-voice-call}"

QQ_DIR="${MAIBOT_QQ_CALL_QQ_DIR:-}"
NAPCAT_DIR="${MAIBOT_QQ_CALL_NAPCAT_DIR:-}"
APPLY=0
ASSUME_YES=0
SKIP_DEPS=0

usage() {
    cat <<'USAGE'
Usage: scripts/install-qq-call.sh [options]

Preflight (default) or install the NapCat AV bridge that gives the bot QQ
voice-call support.

Options:
  --apply                 Install system dependencies and the bridge after the
                          preflight passes. Without it nothing is written.
  --yes                   Do not prompt before apt-get install.
  --skip-deps             Never run apt-get, even with --apply.
  --qq-dir DIR            Linux QQ installation directory (contains ./qq).
  --napcat-dir DIR        NapCat directory (contains napcat.mjs and config/).
  --bridge-src DIR        Where to check out the bridge source.
  --install-dir DIR       Bridge runtime directory.
  -h, --help              Show this help.

Environment:
  MAIBOT_QQ_CALL_QQ_DIR, MAIBOT_QQ_CALL_NAPCAT_DIR,
  MAIBOT_QQ_CALL_BRIDGE_DIR, BRIDGE_SRC_DIR
USAGE
}

info() { printf '[info] %s\n' "$*"; }
ok() { printf '[ ok ] %s\n' "$*"; }
warn() { printf '[warn] %s\n' "$*" >&2; }
die() {
    printf '[fail] %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "缺少必需命令: $1"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply) APPLY=1; shift ;;
        --yes) ASSUME_YES=1; shift ;;
        --skip-deps) SKIP_DEPS=1; shift ;;
        --qq-dir) QQ_DIR=${2:?missing value for --qq-dir}; shift 2 ;;
        --napcat-dir) NAPCAT_DIR=${2:?missing value for --napcat-dir}; shift 2 ;;
        --bridge-src) BRIDGE_SRC_DIR=${2:?missing value for --bridge-src}; shift 2 ;;
        --install-dir) INSTALL_DIR=${2:?missing value for --install-dir}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "未知参数: $1" ;;
    esac
done

[[ $(uname -s) == Linux ]] || die "QQ 语音通话桥目前只支持 Linux（当前系统: $(uname -s)）"

# --- 探测 ------------------------------------------------------------------

# 在候选根目录下找包含 napcat.mjs 与 config/ 的目录。
find_napcat_dir() {
    local candidate root
    for root in "$1/resources/app/app_launcher/napcat" "$1/napcat" \
        "$1/resources/app/napcat"; do
        if [[ -f "$root/napcat.mjs" ]]; then
            printf '%s\n' "$root"
            return 0
        fi
    done
    return 1
}

# 找含 ./qq 可执行文件与 resources/app/ 的 QQ 安装目录。
find_qq_dir() {
    local root
    for root in "$HOME/QQ" "$HOME/Napcat/opt/QQ" "$HOME/NapCat/opt/QQ" \
        /opt/QQ /root/Napcat/opt/QQ /root/NapCat/opt/QQ /usr/share/QQ; do
        if [[ -x "$root/qq" && -d "$root/resources/app" ]]; then
            printf '%s\n' "$root"
            return 0
        fi
    done
    return 1
}

detect_paths() {
    if [[ -z $QQ_DIR ]]; then
        QQ_DIR=$(find_qq_dir || true)
    fi
    if [[ -z $NAPCAT_DIR && -n $QQ_DIR ]]; then
        NAPCAT_DIR=$(find_napcat_dir "$QQ_DIR" || true)
    fi
    if [[ -z $QQ_DIR || -z $NAPCAT_DIR ]]; then
        local found_napcat candidate
        # 退而求其次：全盘找 napcat.mjs，再按标准布局（QQ/resources/app/
        # app_launcher/napcat）往上四级推出 QQ 安装目录。
        while IFS= read -r found_napcat; do
            [[ -n $found_napcat ]] || continue
            candidate=$(cd -- "$found_napcat/../../../.." 2>/dev/null && pwd -P) || continue
            if [[ -x "$candidate/qq" && -d "$candidate/resources/app" ]]; then
                NAPCAT_DIR=$found_napcat
                QQ_DIR=$candidate
                break
            fi
        done < <(find "$HOME" /opt /root -maxdepth 6 -name napcat.mjs -printf '%h\n' 2>/dev/null | head -20)
    fi
}

version_ge() {
    # version_ge A B  ->  A >= B
    [[ $1 == "$2" ]] && return 0
    local highest
    highest=$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)
    [[ $highest == "$1" ]]
}

napcat_version() {
    local manifest="$1/package.json"
    [[ -f $manifest ]] || return 1
    python3 - "$manifest" <<'PY'
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    raise SystemExit(1)
version = data.get("version")
if not isinstance(version, str) or not version.strip():
    raise SystemExit(1)
print(version.strip().lstrip("v"))
PY
}

# --- 预检 ------------------------------------------------------------------

failures=0
note_failure() {
    printf '[fail] %s\n' "$1" >&2
    failures=$((failures + 1))
}

detect_paths

if [[ -n $QQ_DIR ]]; then
    ok "QQ 安装目录: $QQ_DIR"
else
    note_failure "未找到 Linux QQ 安装目录；请用 --qq-dir 指定（目录下应有可执行的 qq）"
fi

if [[ -n $NAPCAT_DIR ]]; then
    ok "NapCat 目录: $NAPCAT_DIR"
else
    note_failure "未找到 NapCat 目录；请用 --napcat-dir 指定（目录下应有 napcat.mjs）"
fi

if [[ -n $QQ_DIR && -n $NAPCAT_DIR ]]; then
    if [[ -f "$QQ_DIR/resources/app/avsdk/libAVSDKPlugin.so" ]]; then
        ok "QQ 自带 AVSDK: resources/app/avsdk/libAVSDKPlugin.so"
    else
        note_failure "QQ 安装缺少 resources/app/avsdk/libAVSDKPlugin.so；该 QQ 版本不支持通话桥，需要换用含 AVSDK 的 Linux QQ"
    fi

    if [[ -f "$QQ_DIR/resources/app/loadNapCat.js" ]]; then
        if grep -q 'MAIBOT_QQ_CALL_LOADER_HOOK_V1' "$QQ_DIR/resources/app/loadNapCat.js"; then
            ok "QQ Loader 已带可逆 Hook（重复安装会复用）"
        elif grep -q 'AV_HOST' "$QQ_DIR/resources/app/loadNapCat.js"; then
            note_failure "QQ Loader 已被其它 AV Host 集成改写；需要提供干净的 Loader 备份再安装"
        else
            ok "QQ Loader 干净，安装时可安全备份并加 Hook"
        fi
    else
        note_failure "缺少 QQ Loader: $QQ_DIR/resources/app/loadNapCat.js"
    fi

    if [[ -d "$NAPCAT_DIR/plugins" ]]; then
        ok "NapCat 插件目录可写性待安装时确认: $NAPCAT_DIR/plugins"
    else
        note_failure "缺少 NapCat 插件目录: $NAPCAT_DIR/plugins"
    fi

    if version=$(napcat_version "$NAPCAT_DIR"); then
        if version_ge "$version" "$BRIDGE_MIN_NAPCAT"; then
            ok "NapCat 版本 $version >= $BRIDGE_MIN_NAPCAT"
        else
            note_failure "NapCat 版本 $version 低于桥要求的 $BRIDGE_MIN_NAPCAT，请先升级 NapCat"
        fi
    else
        warn "无法从 $NAPCAT_DIR/package.json 读出版本；桥要求 NapCat >= $BRIDGE_MIN_NAPCAT，请自行确认"
    fi
fi

missing_commands=()
for command in pulseaudio pactl parec pacat xvfb-run curl flock python3; do
    if command -v "$command" >/dev/null 2>&1; then
        :
    else
        missing_commands+=("$command")
    fi
done
if [[ ${#missing_commands[@]} -eq 0 ]]; then
    ok "通话所需命令齐全（pulseaudio/pactl/parec/pacat/xvfb-run/curl/flock/python3）"
else
    warn "缺少命令: ${missing_commands[*]}（--apply 会通过 apt-get 安装）"
fi

# 第二个 QQ（AV Host）是独立 Electron 进程，内存要留够。
available_mb=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || printf '0')
if [[ ${available_mb:-0} -ge 1500 ]]; then
    ok "可用内存 ${available_mb} MB"
else
    warn "可用内存仅 ${available_mb} MB；AV Host 是第二个 QQ 进程，建议至少预留 1.5 GB"
fi

cpu_count=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '0')
info "CPU 核数: $cpu_count"

if [[ -n $QQ_DIR && -n $NAPCAT_DIR ]]; then
    owner=$(stat -c '%U' "$NAPCAT_DIR/plugins" 2>/dev/null || printf 'unknown')
    info "NapCat 目录属主: $owner（安装需要该用户可写，必要时用 sudo 重跑）"
fi

cat <<EOF

桥的安装位置: $INSTALL_DIR
桥源码检出:   $BRIDGE_SRC_DIR
隔离音频设备: maibot_qq_speaker.monitor（对端声音 -> 我们）
              maibot_qq_mic（我们的 TTS -> QQ 麦克风）

EOF

if [[ $failures -gt 0 ]]; then
    die "预检未通过（$failures 项），先解决上面的问题再安装"
fi

if [[ $APPLY -eq 0 ]]; then
    info "预检通过。确认无误后加 --apply 执行安装。"
    exit 0
fi

# --- 安装 ------------------------------------------------------------------

if [[ $SKIP_DEPS -eq 0 && ${#missing_commands[@]} -gt 0 ]]; then
    if [[ $ASSUME_YES -eq 1 ]]; then
        answer=y
    else
        printf '安装系统依赖 %s 需要 apt-get，继续？[y/N] ' "${missing_commands[*]}"
        read -r answer || answer=n
    fi
    if [[ $answer == [yY] ]]; then
        sudo apt-get update
        sudo apt-get install -y --no-install-recommends \
            pulseaudio pulseaudio-utils xvfb curl
    else
        die "缺少系统依赖且未安装"
    fi
fi

require_command git
if [[ -d "$BRIDGE_SRC_DIR/.git" ]]; then
    info "复用已有桥源码: $BRIDGE_SRC_DIR"
    git -C "$BRIDGE_SRC_DIR" fetch --quiet origin "$BRIDGE_REF" 2>/dev/null || true
    git -C "$BRIDGE_SRC_DIR" checkout --quiet "$BRIDGE_REF" 2>/dev/null \
        || warn "无法切换到 $BRIDGE_REF，继续使用当前检出"
else
    install -d -m 0750 "$(dirname -- "$BRIDGE_SRC_DIR")"
    info "拉取桥源码: $BRIDGE_REPO"
    git clone --quiet "$BRIDGE_REPO" "$BRIDGE_SRC_DIR"
    git -C "$BRIDGE_SRC_DIR" checkout --quiet "$BRIDGE_REF" 2>/dev/null \
        || warn "无法切换到 $BRIDGE_REF，继续使用默认分支"
fi
ok "桥源码就绪: $(git -C "$BRIDGE_SRC_DIR" rev-parse --short HEAD 2>/dev/null || printf 'unknown')"

installer="$BRIDGE_SRC_DIR/bridge/scripts/install.sh"
[[ -x $installer ]] || die "桥安装器不可执行: $installer"

info "运行桥官方安装器（预检）"
"$installer" --qq-dir "$QQ_DIR" --napcat-dir "$NAPCAT_DIR" \
    --install-dir "$INSTALL_DIR" --check

# NapCat 常常装在只有一个用户可以写的目录里，必要时用 sudo 重跑。
if [[ -w "$NAPCAT_DIR/plugins" ]]; then
    "$installer" --qq-dir "$QQ_DIR" --napcat-dir "$NAPCAT_DIR" --install-dir "$INSTALL_DIR"
else
    warn "NapCat 插件目录当前用户不可写，改用 sudo"
    sudo "$installer" --qq-dir "$QQ_DIR" --napcat-dir "$NAPCAT_DIR" --install-dir "$INSTALL_DIR"
fi

cat <<EOF

桥安装完成。接下来：

1. 用桥的启动脚本重启机器人 QQ（会自动拉起隔离 PulseAudio 与 AV Host）：
     MAIBOT_QQ_CALL_BOT_UIN="<机器人QQ号>" $INSTALL_DIR/scripts/run-napcat.sh

   生产环境建议改用 systemd 分别守护这两个进程，而不是留在前台终端。

2. 自检：
     $INSTALL_DIR/scripts/doctor.sh

3. 部署本机语音服务（ASR + TTS），见 tools/speech-service/README.md。

4. 在 bot.conf.toml 的 [qq_call] 里填：
     bridge_token_file = "$INSTALL_DIR/runtime/control.token"
     pulse_server      = "unix:$INSTALL_DIR/runtime/pulse/native"
     capture_device    = "maibot_qq_speaker.monitor"
     playback_device   = "maibot_qq_mic"
   然后把 enabled 改成 true 并重启 kovi-bot。

5. 先用测试账号给机器人打一次语音电话，确认接听、双向音频和挂断都正常，
   再切回日常使用。

注意：QQ 或 NapCat 升级会覆盖 Loader，升级后请重新运行本脚本的 --apply
     与 doctor.sh，并用测试账号复验一次来电。
EOF
