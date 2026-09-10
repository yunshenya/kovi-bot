# QQ 语音通话

对方给机器人 QQ 打语音电话时，芸汐自动接听，用她自己的私聊人设、长期记忆和
同一个模型跟对方说话，挂断后把这次通话写回对方的私聊记忆。

这个功能默认**关闭**（`qq_call.enabled = false`）。它需要在部署服务器上做两件
本仓库之外的事，所以不能随发布流程自动生效：

1. 安装 **NapCat AV 桥**——接通 QQ 通话信令并把音频接进一套隔离的
   PulseAudio 虚拟声卡；
2. 部署**本机语音服务**——中文识别与合成，见
   [`tools/speech-service/README.md`](../tools/speech-service/README.md)。

## 为什么需要桥接

QQ 的语音通话**不在 OneBot 11 协议里**，NapCat 也没有对应的公开接口。（官方接口
兼容表里有 `get_record`、`can_send_record`、`get_ai_record` 这类"语音消息"能力，
但那是发语音条，不是通话。）真正能打通话的是一个社区维护的独立组件：

- [ClaudiaGardner/maibot-qq-voice-call](https://github.com/ClaudiaGardner/maibot-qq-voice-call)（GPL-3.0）
- 上游文档：[QQ 语音通话适配器](https://docs.mai-mai.org/manual/adapters/qq-voice-call)

它由两部分组成：

- 一个**独立 NapCat 插件**，监听 QQ 的 AVSDK 事件、自动接听，并在
  `127.0.0.1:6110` 暴露一个最小的通话状态接口；
- 一个 **AV Host**：第二个 QQ 进程，加载 QQ 自带的 `libAVSDKPlugin.so` 承担真正的
  媒体收发，并在 `127.0.0.1:6111` 与插件通信。

音频**不走 HTTP**，而是走一套桥自己拉起来的隔离 PulseAudio 服务，它提供三个
虚拟设备：

| 设备 | 方向 | 用途 |
|---|---|---|
| `maibot_qq_speaker.monitor` | 采集 | QQ 把对端声音播到这里，我们读它的 monitor |
| `maibot_qq_mic` | 播放 | 我们把芸汐的声音播到这里 |
| `maibot_qq_mic_source` | 采集 | QQ 把它当作默认麦克风 |

设备名带 `maibot_` 前缀是因为桥来自 MaiBot 生态。**我们不改上游代码**，配置里
直接沿用这些名字，这样上游升级时不会跟我们的补丁冲突。

## 数据流

```
QQ 来电
  └─ NapCat 插件（自动接听，无插件可干预的余地）
       └─ 桥状态: GET /v1/calls/current  →  phase = "connected"
            └─ kovi-bot 的 qq_call 模块开始一次通话会话

  对端说话 ─► maibot_qq_speaker.monitor
                └─ parec（16 kHz 单声道 S16LE）
                     └─ 能量 VAD 切句（30 ms 帧、自适应噪声底）
                          └─ POST /v1/asr  →  文本
                               └─ 芸汐的私聊人设 + 记忆 + 同一个模型  →  一句回复
                                    └─ POST /v1/tts  →  逐句 PCM
                                         └─ pacat → maibot_qq_mic  ─► 对端听到

挂断 ─► 通话记录写入对方私聊记忆（context = private）
```

## 机器人侧的实现

代码在 `plugins/model/src/qq_call/`：

| 文件 | 职责 |
|---|---|
| `mod.rs` | 轮询桥状态，把"接通"翻译成一次会话；桥离线时限流报错 |
| `bridge.rs` | 桥控制接口的客户端与通话阶段解析 |
| `audio.rs` | `parec` / `pacat` 子进程，采集与播放 |
| `vad.rs` | 纯状态机的能量 VAD：自适应噪声底 + 迟滞阈值 + 预滚缓冲 |
| `speech.rs` | 本机语音服务客户端（识别用 WAV，合成读取流式 PCM） |
| `session.rs` | 单次通话的编排：两条并发链路 + 打断 + 挂断归档 |

两条链路是刻意的设计：

- **采集链**永不阻塞——每 30 ms 读一帧、判一次语音，顺带检测插话与挂断；
- **回复链**做识别、模型和合成，天然是秒级的。

两者之间只有两个有界通道（语音片段队列、打断信号）。识别变慢只会让回复滞后，
不会让通话"听不见"。

### 打断（barge-in）

对方在芸汐说话时开口，采集链立刻通过打断通道让回复链停掉正在播放的 `pacat`。
被打断的回复**不写进电话上下文**——对方没听完的话不应该被当成"说过了"。

### 采样率

合成输出的采样率是模型属性（VITS 中文常见 16 kHz、Matcha 22.05 kHz），
所以机器人始终以 `/v1/tts` 响应头 `X-Sample-Rate` 为准来启动 `pacat`，
配置里的 `tts_sample_rate` 只是一个请求提示。

## 开通步骤

```bash
# 1. 服务器条件预检（只读，不落盘）
scripts/install-qq-call.sh

# 2. 预检通过后安装桥（apt 依赖 + 上游桥官方安装器）
scripts/install-qq-call.sh --apply

# 3. 部署本机语音服务，见 tools/speech-service/README.md

# 4. 桥自检
~/.local/share/maibot-qq-voice-call/scripts/doctor.sh
```

然后把 `bot.conf.toml` 的对应项填上并打开开关：

```toml
[qq_call]
enabled = true
bridge_token_file = "/home/ubuntu/.local/share/maibot-qq-voice-call/runtime/control.token"
pulse_server = "unix:/home/ubuntu/.local/share/maibot-qq-voice-call/runtime/pulse/native"
capture_device = "maibot_qq_speaker.monitor"
playback_device = "maibot_qq_mic"
asr_url = "http://127.0.0.1:6120/v1/asr"
tts_url = "http://127.0.0.1:6120/v1/tts"
allowed_callers = [你的QQ号]
```

重启 kovi-bot 后，日志里应出现：

```
[INFO] QQ 语音通话已启用（桥 http://127.0.0.1:6110，轮询 250 毫秒）
```

**先用测试账号打一次电话**，确认接听、双向音频、打断和挂断都正常，再切回日常使用。

### 启动顺序

桥的启动脚本会拉起隔离 PulseAudio 和 AV Host，再启动 QQ（NapCat 在 QQ 进程内）：

```bash
MAIBOT_QQ_CALL_BOT_UIN="<机器人QQ号>" \
  ~/.local/share/maibot-qq-voice-call/scripts/run-napcat.sh
```

生产环境建议用 systemd 分别守护 AV Host 与 NapCat，而不要留在前台终端。
机器人自己的 `kovi-bot.service` 不需要改：PulseAudio 地址是机器人通过
`PULSE_SERVER` 环境变量传给 `parec`/`pacat` 子进程的，不依赖 systemd 环境。

## 权限与安全

- 桥与语音服务都**只监听回环**；配置层强制校验 `bridge_url`、`asr_url`、
  `tts_url` 必须是 `127.0.0.1` / `localhost` / `[::1]`，填公网地址会直接启动失败。
- 桥接 Token 优先读环境变量 `KOVI_QQ_CALL_BRIDGE_TOKEN`，否则读安装器生成的
  0600 文件；Token 不会出现在任何日志里。
- **桥会在来电后自动接听，插件无法阻止接通。** 因此白名单（`allowed_callers`，
  留空表示只允许主管理员）只能在接通后生效：白名单外只播报一句
  `refuse_message` 然后保持静音，不会进入对话。
- 挂断归档把通话内容写进对方的私聊记忆，`archive_to_memory = false` 可以关掉。

## 调参

| 配置 | 默认 | 说明 |
|---|---|---|
| `end_of_speech_frames` | 18（540 ms 静音） | 调小更灵敏，但中文句内停顿多，太小会把人话切断 |
| `barge_in_speech_frames` | 18 | 判定对方插话所需的语音帧数 |
| `max_reply_chars` | 40 | 电话里说话要短；超过会截断在句末标点上 |
| `history_turns` | 8 | 通话内保留的历史轮数 |
| `max_call_seconds` | 1800 | 通话时长上限，防止忘记挂断 |
| `tts_playback_latency_ms` | 80 | PulseAudio 播放缓冲；卡顿就调高 |

端到端延迟大致是：句尾静音判定（540 ms）+ 识别（0.2–0.5 s）+ 模型（0.5–1.5 s）
+ 首句合成（0.2–0.4 s）≈ **1.5–2.5 秒**。想更快，优先降 `end_of_speech_frames`，
其次换低延迟合成模型。

## 已知限制

- **只支持 Linux。** 桥依赖 Linux QQ 自带的 `libAVSDKPlugin.so` 与 PulseAudio。
- **QQ 或 NapCat 升级会覆盖 Loader。** 升级后必须重新运行
  `scripts/install-qq-call.sh --apply` 与 `doctor.sh`，并用测试账号复验一次来电。
- **不能主动打电话**，也不能挂断对方的电话；桥只暴露了读状态的能力。
- **通话没有接入 World Model / Mind 的实时状态**，只复用私聊人设、记忆和模型。
  电话里的情绪与情境暂时不会回流到核心的其它子系统。
- 白名单外只能"接通后婉拒"，原因见上面的安全一节。

## 排错

| 现象 | 先看 |
|---|---|
| 日志一直报"无法读取桥状态" | 桥没起来：`doctor.sh`、`run-napcat.sh` 的输出 |
| 接通了但芸汐不说话 | `curl http://127.0.0.1:6120/healthz`；再看机器人日志里"采集链路正常"有没有出现 |
| 芸汐听不见对方 | `capture_device` 是否为 `maibot_qq_speaker.monitor`；QQ 是否以 `PULSE_SERVER` 指向桥的 socket 启动 |
| 对方听不见芸汐 | `playback_device` 是否为 `maibot_qq_mic`；`pacat` 是否报错（日志里会带 stderr 尾部） |
| 识别全是乱码 | `capture_sample_rate` 与桥约定不一致；应为 16000 |
| 声音变调 | 合成模型输出采样率被写死了；应以 `X-Sample-Rate` 为准 |

## NapCat 跑在 Docker 里时的部署要点

我们的生产环境 NapCat 是 `mlikiowa/napcat-docker` 容器（NapCat 4.18.19 + Linux QQ
`9.9.22-40990`）。桥原本假设 NapCat 与 QQ 直接装在宿主机上，因此容器环境需要下面
这些额外处理。**这些都已经做进服务器上的脚本，本节是给日后升级/排障看的。**

### 1. 容器网络必须是 host

桥刻意只监听 `127.0.0.1:6110`（`parseBridgeSettings` 会拒绝非回环地址）。而
Docker 的 `-p` 端口发布是把流量转发到**容器网卡 IP**，回环上的服务收不到。

所以容器用 `--network host` 运行：容器与宿主机共享回环，机器人才能直接访问桥。
用 `-p 6110:6110` 是**行不通**的，这一点很反直觉，但已验证。

### 2. 需要一条共享挂载

PulseAudio 是 unix socket，跨容器边界必须是同一份文件。容器多挂了一个：

```
-v /home/ubuntu/napcat-qq-call:/app/qq-call
```

桥的安装目录就放在这里（`--install-dir /app/qq-call/bridge`），所以 AV Host、
启动脚本、Token、cookie 和 PulseAudio socket 在宿主机和容器里都能看到。

### 3. 容器入口包装脚本

镜像的 `entrypoint.sh` 只会启动普通 QQ，不会拉起 AV Host。`/app/qq-call/bridge-entry.sh`
是容器的启动命令，在把控制权交回 `entrypoint.sh` 之前做四件事（全部幂等）：

| 动作 | 原因 |
|---|---|
| `ensure_audio_packages` | 容器重建会丢掉 apt 装的 `pulseaudio` / `pulseaudio-utils` / `libpulse-mainloop-glib0`。**`libpulse-mainloop-glib.so.0` 缺了 AVSDK 插件会加载失败**（`Failed to load Pepper module`）。另外镜像里 `linuxqq` 有个 `xdg-utils` 依赖缺口，会让 apt 拒绝安装任何包，必须先单独装它。 |
| `patch-napcat-whitelist.py` | 见下一节 |
| 导出 `ACCOUNT` | 镜像只在 `ACCOUNT` 非空时才给 QQ 传 `-q`；不传就退回扫码登录。包装脚本从 `config/napcat_<uin>.json` 推断 QQ 号。 |
| 启动 AV Host | 桥没装好时这步是纯透传 |

另外还有两个常驻的小循环：**权限看护**（把 `/app/qq-call` 到 pulse socket 整条
链 chmod 0777，因为镜像会对 `/app/**` 做 chown root，而宿主机上的机器人是 ubuntu）
和 **cookie 同步**（见下）。

### 4. NapCat 4.18.x 的插件白名单

NapCat 4.18.x 在 `napcat.mjs` 里硬编码了一份官方插件白名单：

```js
const _me = new Set([
  "napcat-plugin-builtin", "napcat-plugin-cleaner",
  "napcat-plugin-ssqq", "napcat-plugin-qce"
]);
```

不在这个集合里的插件一律被 `[PluginLoader] Rejected ...: not in official plugin
whitelist` 拒绝，第三方插件（包括桥，也包括社区的 `napcat-plugin-debug`）根本
加载不了。

`patch-napcat-whitelist.py` 只做一处最小追加，把桥的插件名加进这个 Set。它同时
覆盖两个位置，因为容器首次启动时运行时文件还不存在：

- `/app/NapCat.Shell.zip` 里的模板（entrypoint 从这里解压）；
- `/app/napcat/napcat.mjs` 运行时文件。

幂等、可逆（首次修改前留 `*.kovi-orig` 备份）。锚点匹配不上时会明确报
"锚点未匹配"，升级 NapCat 后如果出现这句，说明内联结构变了，需要人工看一眼。

### 5. 隔离 PulseAudio 的 cookie

桥生成的 `pulse.pa` 写了 `auth-anonymous=1`，但**实测在该版本上不生效**：服务端
仍然做 cookie 校验，日志里是 `Denied access to client with invalid authentication
data.`。容器里的 QQ/AV Host 以 root 运行没有 cookie，反而被匿名认证放行；宿主机上
以 ubuntu 运行、带了自己 cookie 的机器人则被拒绝。

解决办法是把服务端 cookie 同步到共享目录，机器人的 `parec`/`pacat` 通过
`PULSE_COOKIE` 使用它（对应配置项 `qq_call.pulse_cookie`）。

### 6. 重启容器必须给足优雅退出时间

`docker stop` 默认 10 秒后 SIGKILL。QQ 来不及落盘会话，**会导致登录态失效、必须
重新扫码**（这个坑踩过一次）。现在容器用 `--stop-timeout 60` 创建，手动重启请用：

```bash
sudo docker restart -t 60 napcat
```

### 7. 重启 NapCat 之后要重启机器人

kovi 在 WebSocket 断开时会直接退出（不会自动重连），所以每次重启容器后都要：

```bash
sudo systemctl restart kovi-bot
```

建议给 `kovi-bot.service` 加 `Restart=on-failure` 之类的自愈策略，否则 NapCat 抖动
一次机器人就一直离线。

### 8. 这套东西的位置

| 内容 | 位置 | 容器重建后 |
|---|---|---|
| 桥源码（上游 GPL-3.0，固定提交 `22f30c0`） | `/home/ubuntu/napcat-qq-call/src` | 保留 |
| 容器入口包装 + 补丁脚本 | `/home/ubuntu/napcat-qq-call/` | 保留 |
| 桥运行时（AV Host、脚本、Token、cookie、pulse socket） | `/home/ubuntu/napcat-qq-call/bridge` | 保留 |
| NapCat 插件、配置 | `/root/napcat/{plugins,config}` | 保留 |
| 入口包装、补丁、apt 依赖、QQ Loader Hook | 容器层 | **自动重建**（包装脚本负责） |
| 本机语音服务 | `/home/ubuntu/yunxi-speech` + `~/.local/share/yunxi-speech/models` | 保留 |

回滚点：`/root/napcat-rollback/`（容器 inspect 快照 + `napcat-data-*.tar.gz` 数据备份），
以及旧容器 `napcat-bridge-v1`。

## 实测验证与根因（2026-09-11）

**2026-09-11 01:50 完成首次真实来电验证，全链路打通。** 实测记录：

```
01:50:18  QQ 语音通话已接通: 小猫ᓚᘏᗢ(3052405886)
01:50:28  QQ 通话已收到对端语音，采集链路正常
01:50:30  QQ 通话识别: 你好你好。
01:50:30  QQ 通话回复: 嗯，你好呀，听起来你今天心情不错？
01:50:42  QQ 通话识别: 嗯，OO采了采了。
01:50:43  QQ 通话回复: 采了了？是说你那边忙完了，还是刚才信号有点飘呀？
01:50:58  QQ 通话识别: 操，没事没事。
01:50:58  QQ 通话回复: 好，那我不追问了。你现在是想随便聊聊，还是有什么事想和我说？
01:51:09  QQ 语音通话结束（对方挂断，时长 51 秒）
```

挂断后私聊记忆里写入了一条 `context = private`、`tags = ["qq_call"]` 的记录：

```
[QQ语音通话记录] 结束原因：对方挂断
芸汐：喂，我在的，怎么啦？
小猫ᓚᘏᗢ：你好你好。
芸汐：嗯，你好呀，听起来你今天心情不错？
...
```

端到端时延（句尾到开口）约 2–3 秒。

### 根因：上游插件误判了 AVSDK 的 `20050`

问题**从来不在环境或版本**，而在插件对一条命令的语义误判。给插件加上命令直方图后，
AVSDK 回传的全部命令第一次变得可见：

| 命令 | 真实含义 | 上游的理解 |
|---|---|---|
| `cmd 1` → `[0, ""]` | 登录**成功**（0 = 成功） | — |
| `cmd 103` → `[1, ""]` | 状态通知 | — |
| `cmd 20061` | 音频设备变更上报（内容里就是我们桥的虚拟声卡） | — |
| `cmd 20050` | **周期性通知，与登录成败无关** | **误判为「掉线需重登」** |
| `cmd 20006` | 来电回调 | 等它，但之前永远等不到 |

上游在收到 `20050` 后每 100ms 重登一次，于是**亲手把自己刚刚建立的健康会话反复踢掉**
（实测累计 521 次，`16594 / 521 ≈ 31.9`）。会话永远无法稳定，`cmd 55`（内核转发的来电
动作）因此永远得不到处理，来电就一直停在 `ringing`。

修复只需一行语义修正：**`20050`/`120043` 不再触发重登**。效果：

| | 修复前 | 修复后 |
|---|---|---|
| 登录次数 | 521 次（循环） | **1 次** |
| `cmd 20006` 来电回调 | 从未出现 | **出现** |
| 自动接听 | 不触发 | **200ms 内完成** |
| `20004` 进房 | 从未出现 | **20004 出现** |
| 网络媒体输出 | 0 | **出现** |
| 通话阶段 | 永远 `ringing` | **`connected`** |

### 为了走到这一步，先排除掉的原因

这些排查虽然没有定位到根因，但把范围逼到了插件逻辑上，值得记录以免重复：

| 假设 | 验证方式 | 结论 |
|---|---|---|
| AVSDK 缺动态库 | 补齐 `libpulse-mainloop-glib.so.0`、`libEGL.so.1`、`libOpenGL.so.0`、`libGLESv2`、`libGLX`、`libGLdispatch` | 补齐后 Pepper 模块加载成功，问题不变（**但这是必需的**，缺了 AVSDK 根本加载不了） |
| 容器 `/dev/shm` 过小（Docker 默认 64 MB） | `--shm-size=1g` | 无效（**但保留**，Electron 需要） |
| 容器沙箱限制 | `--privileged --security-opt seccomp=unconfined` | 完全无变化，容器被排除 |
| 容器网络 | `--network host`（回环服务才能被宿主机访问） | 必需项，但非根因 |
| NapCat / QQ 版本漂移 | 桥写于 2026-08-03，当时 NapCat 为 4.18.12–4.18.14；我们是 4.18.19 | 仅差几个小版本，不是变量 |
| 音频设备 | AVSDK 正确枚举 `MaiBot_QQ_Speaker` / `MaiBot_QQ_Microphone` | 正常 |

### 三处已固化为自愈的补丁

容器入口包装 `bridge-entry.sh` 每次启动都会幂等地重打这三个补丁，因此**重建容器、
重装桥之后都会自动恢复**，不需要人工介入：

| 补丁 | 作用 |
|---|---|
| `patch-plugin-account-path.py` | NapCat 4.18.x 已删除 `session.getAccountPath()`，插件的回退值 `ctx.core.dataPath` 是 QQ 数据**根目录**而非账号目录（`nt_qq_<hash>`），需要自行解析 |
| `patch-20050-backoff.py` | 加入命令直方图（暴露在 `/v1/status` 的 `avHost.commandHistogram`） |
| `patch-ignore-20050.py` | **根因修复**：`20050`/`120043` 不再触发重登 |

已验证的插件整份备份在
`/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs.kovi-verified`。

### 仍然需要你知道的运维约束

- 重启 NapCat **必须**用 `sudo docker restart -t 60 napcat`。默认 10 秒就 SIGKILL，
  QQ 来不及落盘会话，会导致登录态失效、需要重新扫码。
- `kovi-bot.service` 已配置 `Restart=always`（kovi 断连是**正常退出**，退出码 0，
  所以 `on-failure` 不会触发），NapCat 抖动后机器人会自己回来。
- 完整诊断报告（含全部证据）见
  [`upstream-issue-qq-call-avsdk-20050.md`](upstream-issue-qq-call-avsdk-20050.md)。
