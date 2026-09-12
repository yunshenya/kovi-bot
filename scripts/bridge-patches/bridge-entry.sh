#!/usr/bin/env bash
# NapCat 容器的入口包装：准备 QQ 通话桥的音频环境、拉起 AV Host，然后把控制权
# 交回镜像自己的 entrypoint.sh，保证 NapCat 的启动方式与官方镜像完全一致。
#
# 桥没装好时这个脚本基本是纯透传，不会影响任何现有功能。
set -uo pipefail

BRIDGE_DIR="${QQ_CALL_BRIDGE_DIR:-/app/qq-call/bridge}"
RUNTIME_DIR="$BRIDGE_DIR/runtime"
PULSE_DIR="$RUNTIME_DIR/pulse"
PULSE_SOCKET="$PULSE_DIR/native"
AV_HOST_SCRIPT="$BRIDGE_DIR/scripts/run-av-host.sh"

# 桥自带的脚本会把 runtime 目录 chmod 成 0700 且属主是 root，而宿主机上的机器人
# 必须能连到这个 PulseAudio socket。用一个极轻量的看护循环把权限保持在"可穿越 +
# 可连接"，这样就不必依赖宿主机侧的文件属主（容器 entrypoint 会把 /app/** 改成 root）。
install -d -m 0777 "$RUNTIME_DIR" "$PULSE_DIR" 2>/dev/null
chmod 0777 /app/qq-call "$BRIDGE_DIR" 2>/dev/null
(
  while :; do
    # 整条祖先链都要可穿越，否则宿主机侧的 parec/pacat 连不到 socket。
    chmod 0777 /app/qq-call "$BRIDGE_DIR" "$RUNTIME_DIR" "$PULSE_DIR" 2>/dev/null
    # 芸汐发语音时机器人往这里写 WAV，NapCat 读同一份文件；容器 entrypoint
    # 每次启动会把 /app/** chown 成 root，所以这里要一直把权限放回来。
    install -d -m 0777 /app/qq-call/voice 2>/dev/null
    chmod 0777 /app/qq-call/voice 2>/dev/null
    if [ -S "$PULSE_SOCKET" ]; then chmod 0777 "$PULSE_SOCKET" 2>/dev/null; fi
    # 桥接 Token 由安装器建成 0600 root，宿主机上的机器人需要能读它。
    chmod 0644 "$BRIDGE_DIR/runtime/control.token" 2>/dev/null
    chmod 0644 "$BRIDGE_DIR/runtime/pulse-cookie" 2>/dev/null
    sleep 2
  done
) &
echo "[qq-call] 音频权限看护已启动 (pid $!)"

# 桥插件在挂载卷里会保留，但重新安装桥会覆盖它，所以每次启动幂等重打补丁。
#
# 三个补丁缺一不可，它们共同修掉了「来电能检测到但永远无人接听」：
#   account-path : NapCat 4.18 删了 session.getAccountPath，回退值拿到的是 QQ 数据
#                  根目录而不是账号目录；
#   ignore-20050 : 上游把周期性通知 20050 误判为掉线并每 100ms 重登，亲手把健康的
#                  会话反复踢掉（实测累计 521 次），cmd 55 因此永远得不到处理；
#   histogram    : 记录 AVSDK 各 cmd 的出现次数，便于日后判断协议是否又变了。
for patcher in patch-plugin-account-path.py patch-20050-backoff.py patch-ignore-20050.py patch-plugin-login-refresh.py patch-plugin-caller-allowlist.py patch-plugin-hangup.py; do
  patch_path="$BRIDGE_DIR/../$patcher"
  if [ -f "$patch_path" ]; then
    python3 "$patch_path" || echo "[qq-call] 补丁 $patcher 未应用" >&2
  fi
done

# Loader Hook 同样在镜像层里，重建容器会丢，这里每次启动幂等补一次。
LOADER_HOOK="$BRIDGE_DIR/../ensure-loader-hook.sh"
if [ -f "$LOADER_HOOK" ]; then
  bash "$LOADER_HOOK" || echo "[qq-call] Loader Hook 未能应用，AV Host 将不可用" >&2
fi

# 容器的 apt 包在镜像层之外，重建容器就会丢。这里按需补齐通话所需的音频依赖，
# 让整套设置在容器重建/镜像升级后能自愈。（xdg-utils 必须先装：镜像里
# linuxqq 的依赖缺口会让 apt 拒绝安装任何其它包。）
ensure_audio_packages() {
  local missing=0
  for c in pulseaudio pactl parec pacat; do
    command -v "$c" >/dev/null 2>&1 || missing=1
  done
  [ -f /usr/lib/x86_64-linux-gnu/libpulse-mainloop-glib.so.0 ] || missing=1
  # AVSDK 是 Pepper 插件，进程里还要能解析到 libEGL/libGLESv2，否则会以
  # "Failed to load Pepper module ... libEGL.so.1" 失败，登录接口随即 500。
  ldconfig -p 2>/dev/null | grep -q "libEGL.so.1" || missing=1
  if [ "$missing" = "0" ]; then
    return 0
  fi
  echo "[qq-call] 缺少通话音频依赖，正在安装（首次重建容器后需要一次）..."
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq >/dev/null 2>&1 || true
  apt-get install -y --no-install-recommends xdg-utils >/dev/null 2>&1 || true
  if apt-get install -y --no-install-recommends pulseaudio pulseaudio-utils libpulse-mainloop-glib0 libegl1 libgl1 libgles2 libopengl0 libglx0 libglvnd0 libglx-mesa0 libgl1-mesa-dri >/dev/null 2>&1; then
    ldconfig 2>/dev/null || true
    echo "[qq-call] 音频依赖安装完成"
  else
    echo "[qq-call] 音频依赖安装失败，通话功能不可用" >&2
  fi
}
ensure_audio_packages

# 隔离 PulseAudio 的 auth-anonymous 在本版本上不生效，客户端必须带匹配 cookie。
# 容器内的 QQ/AV Host 以 root 运行本就免认证；宿主机上的机器人是 ubuntu，
# 需要一个 cookie 副本，这里持续把它同步到共享目录给宿主机用。
sync_pulse_cookie() {
  while :; do
    if [ -f /root/.config/pulse/cookie ]; then
      chmod 0644 /root/.config/pulse/cookie 2>/dev/null
      if ! cmp -s /root/.config/pulse/cookie "$BRIDGE_DIR/runtime/pulse-cookie" 2>/dev/null; then
        cp -f /root/.config/pulse/cookie "$BRIDGE_DIR/runtime/pulse-cookie" 2>/dev/null && \
          chmod 0644 "$BRIDGE_DIR/runtime/pulse-cookie" 2>/dev/null
      fi
    fi
    sleep 5
  done
}
sync_pulse_cookie &
echo "[qq-call] PulseAudio cookie 同步已启动 (pid $!)"

# NapCat 4.18.x 把官方插件白名单硬编码在 napcat.mjs 里，第三方插件会被
# "not in official plugin whitelist" 拒绝。补丁脚本是幂等的，容器每次启动
# 都会自动重新应用（napcat.mjs 在镜像层里，重建容器会丢）。
PATCHER="$BRIDGE_DIR/../patch-napcat-whitelist.py"
if [ -f "$PATCHER" ]; then
  python3 "$PATCHER" || echo "[qq-call] 白名单补丁未应用（可能 NapCat 版本已变）" >&2
fi

# 镜像的 entrypoint 只有在 ACCOUNT 非空时才会给 QQ 传 -q（快速登录）。
# 不传就会退回扫码登录，所以这里从 NapCat 的账号配置里推出 QQ 号并导出。
if [ -z "${ACCOUNT:-}" ]; then
  cfg=$(ls /app/napcat/config/napcat_[0-9]*.json 2>/dev/null | head -1)
  if [ -n "$cfg" ]; then
    ACCOUNT=$(basename "$cfg" | sed -E 's/^napcat_([0-9]+)\.json$/\1/')
    export ACCOUNT
  fi
fi
if [ -n "${ACCOUNT:-}" ]; then
  echo "[qq-call] 使用快速登录账号: $ACCOUNT"
fi

if [ -x "$AV_HOST_SCRIPT" ]; then
  export PULSE_SERVER="unix:$PULSE_SOCKET"
  echo "[qq-call] PULSE_SERVER=$PULSE_SERVER"
  if [ "${QQ_CALL_AV_HOST:-1}" = "1" ]; then
    install -d -m 0750 "$BRIDGE_DIR/logs"
    nohup "$AV_HOST_SCRIPT" >>"$BRIDGE_DIR/logs/av-host.log" 2>&1 &
    av_pid=$!
    echo "[qq-call] AV Host 已拉起 (pid $av_pid)"

    # 严格照上游 run-napcat.sh 的顺序：AV Host 先就绪，再启动主 QQ。
    # 上游在 AV Host 未就绪时直接报错退出，这里保持同样的语义并留足时间
    # （第二个 Electron 进程冷启动约 15–30 秒）。
    av_ready=0
    for _ in $(seq 1 240); do
      if curl -fsS "http://127.0.0.1:${QQ_CALL_AV_HOST_PORT:-6111}/healthz" >/dev/null 2>&1; then
        av_ready=1
        break
      fi
      sleep 1
    done
    if [ "$av_ready" = "1" ]; then
      echo "[qq-call] AV Host 已就绪，开始启动主 QQ"
    else
      echo "[qq-call] AV Host 未在 240 秒内就绪，重试一次" >&2
      bash "$BRIDGE_DIR/../restart-av-host.sh" || true
      for _ in $(seq 1 60); do
        if curl -fsS "http://127.0.0.1:${QQ_CALL_AV_HOST_PORT:-6111}/healthz" >/dev/null 2>&1; then
          av_ready=1
          break
        fi
        sleep 1
      done
      if [ "$av_ready" = "1" ]; then
        echo "[qq-call] AV Host 重试后就绪"
      else
        echo "[qq-call] AV Host 仍不可用；继续启动 NapCat，通话功能会不可用" >&2
      fi
    fi
  fi
else
  echo "[qq-call] 桥尚未安装，纯透传启动 NapCat"
fi

exec bash entrypoint.sh
