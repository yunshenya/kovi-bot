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
| `mod.rs` | 轮询桥状态，把"接通"翻译成一次会话；阶段变化写日志；桥离线时限流报错 |
| `bridge.rs` | 桥控制接口的客户端与通话阶段解析 |
| `diagnostics.rs` | 最近一次通话摘要与 `#通话状态` 自检报告（桥、来电者、授权、语音服务） |
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

## 谁能给芸汐打电话

放行规则是「主管理员 ∪ 副管理员 ∪ 通话授权名单」，其中授权名单同时来自两处：
数据库（`kovi_bot_authorized_callers`，用命令维护）和静态配置
`qq_call.allowed_callers`（首次初始化会迁移进数据库，保留是为了授权体系尚未
初始化时仍能工作）。

在私聊里用这些命令维护（**仅机器人管理员可执行**，普通用户发这些命令会被静默忽略）：

| 命令 | 作用 |
|---|---|
| `#授权通话 QQ号` | 允许该 QQ 给芸汐打电话 |
| `#取消授权通话 QQ号`（或 `#移除授权通话 QQ号`） | 取消授权 |
| `#通话名单`（或 `#授权通话列表`） | 查看当前可通话名单（含主/副管理员） |
| `#通话帮助` | 用法说明 |

主管理员与副管理员**默认就可以通话，不需要授权**；`#通话名单` 会把他们的身份标出来。

几点要注意：

- 授权状态持久化在 PostgreSQL，重启与重新部署都不丢；每条变更都会写
  `[INFO] 通话授权名单已更新` 日志。
- **授权只决定"她接起来之后说不说话"，不决定"接不接"**：接听由桥完成，插件拦不住
  也催不动。授权对了却一直没人接，属于桥/QQ 侧的问题，用 `#通话状态` 与日志里的
  "来电振铃"那一行区分（见[排错](#排错)）。
- 默认行为仍是**接通后婉拒**（和上游一致）：谁打进来都会被接通，名单外听到一句
  `refuse_message` 后静音。注意 QQ 的 1v1 通话没有对插件开放"离开房间"，这通电话会
  一直连着，直到对方挂断——9 月 11 日晚上就是这样卡了一整夜。
- 想改成**名单外不接**，把 `qq_call.caller_allowlist_enabled` 打开：机器人会把
  「授权名单 ∪ 副管理员 ∪ 主管理员」写进 `caller_allowlist_file`，桥在接听前读它，
  名单外直接不接听，来电自然结束（对方听到"无人接听"）。名单文件读不到时（例如机器人
  没在跑）桥**回退成接听任何来电**，避免谁都打不进来；`#通话状态` 会显示开关状态、
  名单人数与最近同步时间。
- 实际能打进来的上限是机器人 QQ 的好友——QQ 语音通话本身要求双方是好友。
- 数据库如果不可用，授权判定会回退到静态配置 `qq_call.allowed_callers`，
  不会出现"谁都打不进来"。
- 机器人私聊里发 `#通话状态` 可以现场核对：桥当前阶段、这位来电者算不算已授权、
  最近一次通话有没有进房、语音服务是否正常。

## 芸汐主动发语音消息

两条链路都能把一轮回复用声音说出来，标记方式各自跟着自己的契约走：

**Core 链路**（私聊、群内点名等日常对话，即 2026-09 起的默认链路）：她在正文最前面
单独一行写 `[[VOICE]]`，程序把这一轮的气泡合成成语音发出；标记不会展示给用户：

```
[[VOICE]]
我没事呀
```

**Host 链路**（插话、控制命令回执与回退路径）：她通过 `reply_action` 工具的 `voice`
字段把这一轮标成语音：

```json
{"voice": true, "messages": ["我没事呀"]}
```

字段契约随工具 schema 下发（不再有常驻的 `<回复协议>` 文本、也不再在正文里写动作标记）。

什么时候用是**她自己判断**的——字段说明里写的是「想用声音说这一条就填 true」。
语音消息承载不了 @ 和引用，所以填了语音就不要同时使用它们，字段说明里写明了；
合成成功时引用段会被丢掉。

**语音选项只在 `[qq_voice] enabled = true` 时才写进提示词**。通道关掉时不该让模型
以为自己有一个当下用不了的出口——提示词里说能发、投递时静默退化成文字，是最难查的
那种不一致。Core 侧还多一道门：只有真的下发过语音选项的回合才执行标记，明确条数的
批次、工具回合与自主续聊不会因为模型多写一个标记就变成语音。

### 实现要点

- **两条链路共用同一份合成实现**：`plugins/model/src/voice_reply.rs` 合成 PCM、写
  WAV、算 NapCat 侧路径；Core 的投递在 `yunxi/delivery.rs`（标记语音且合成成功时只
  发 `record` 段），旧协议的投递在 `model/message_actions.rs`。
- **标记解析**：Core 侧是 `core_model.rs` 的 `split_core_voice_marker`，只认"正文最
  前面单独一行"的 `[[VOICE]]`，写在别处一律当普通正文，所以让芸汐复述这个标记不会
  被当成指令。
- **复用通话那套 TTS**：同一个只监听回环的本机语音服务，`plugins/model/src/speech.rs`
  现在被通话与语音消息共用。
- **任何一步失败都回退成文字**。语音只是表达方式，不该因为 TTS 抖动把回复弄丢。
- **音频必须落在 NapCat 能打开的路径上**。NapCat 常常与机器人不在同一个文件系统
  命名空间里（本部署里 NapCat 跑在容器内），所以配置里有 `staging_dir`（机器人写）
  与 `napcat_staging_dir`（NapCat 读）两个路径，指向同一个共享目录。非容器部署时
  两者填成一样即可。
- NapCat 会把非 silk 音频**自动转成 silk**（内部 `convertToNTSilkTct`），所以直接给
  WAV 就行，不需要我们自己转码。
- 暂存文件按 `keep_files` 保留最近的若干个，不会无限增长。
- 记忆里记的仍然是**文字**（她"说"的内容），不是音频路径。

## 调参

| 配置 | 默认 | 说明 |
|---|---|---|
| `end_of_speech_frames` | 18（540 ms 静音） | 调小更灵敏，但中文句内停顿多，太小会把人话切断 |
| `barge_in_speech_frames` | 18 | 判定对方插话所需的语音帧数 |
| `max_reply_chars` | 40 | 电话里说话要短；超过会截断在句末标点上 |
| `history_turns` | 8 | 通话内保留的历史轮数 |
| `max_call_seconds` | 1800 | 通话时长上限，防止忘记挂断 |
| `tts_playback_latency_ms` | 80 | PulseAudio 播放缓冲；卡顿就调高 |
| `max_reply_chars` | 120 | **单段**上限；对方让她"一直说"时一段可以这么长 |
| `monologue_max_chunks` | 30 | 一次"一直说"最多连讲几段（0 = 不连讲） |
| `tool_max_rounds` | 3 | 一次回复里最多几轮工具调用 |
| `claim_retry_rounds` | 2 | 空承诺补跑次数；0 = 只告警不补跑 |
| `idle_prompt_secs` | 20 | 对方安静多久她主动出声；0 = 关闭 |
| `idle_prompt_max` | 3 | 连续主动出声的次数上限（每次间隔翻倍） |

端到端延迟大致是：句尾静音判定（540 ms）+ 识别（0.2–0.5 s）+ 模型（0.5–1.5 s）
+ 首句合成（0.2–0.4 s）≈ **1.5–2.5 秒**。想更快，优先降 `end_of_speech_frames`，
其次换低延迟合成模型。

## 已知限制

- **只支持 Linux。** 桥依赖 Linux QQ 自带的 `libAVSDKPlugin.so` 与 PulseAudio。
- **QQ 或 NapCat 升级会覆盖 Loader。** 升级后必须重新运行
  `scripts/install-qq-call.sh --apply` 与 `doctor.sh`，并用测试账号复验一次来电。
- **可以主动挂断，也可以主动外呼**（均已于 2026-09-12 真机验证）。
  - 主动挂断用 `Close`（cmd 10；早前试过的 `Quit`/cmd 8 挂不断）：对方要求挂断、
    名单外婉拒、通话到点、采集中断时，机器人会请桥真的挂断这通电话；
  - 主动外呼用 `StartCall`（cmd 4）+ JSON payload，私聊 `#打给我` 触发，
    见「外呼（她主动打给我）」一节。
- **通话没有接入 World Model / Mind 的实时状态**，只复用私聊人设、记忆和模型。
  电话里的情绪与情境暂时不会回流到核心的其它子系统。**但工具是通的**——电话里
  可以让她发消息、建提醒，见「通话中的工具调用」一节。通话**结束**这件事本身已经
  进认知层（`CallEnded` 事件），只是通话**过程中**的实时状态还没有。
- 白名单外默认仍是"接通后婉拒"（可选改成"不接"，见上面的安全一节）。

### "主动挂断"排查记录（2026-09-12，已解决）

背景：2026-09-11 晚上一通未授权来电接通后卡在 `connected` 一整夜，机器人每次重启都
重新起一次幽灵会话，直到重启容器才清掉。为了给芸汐加上"主动挂断"，先把客户端侧所有
"看起来像"的入口试穿（下表）；**真正的入口一直是桥控制接口的 cmd 8，只是白名单只放行了
1/5/55。**

| 入口 | 试了什么 | 结果 |
|---|---|---|
| AVSDK 消息通道 `postMessage({cmd})` | 命令号 2/3/4/6/7/8/9/10/11/12/13/14/15/20/21/30/40 | 当时全部"无效果"——因为**桥的白名单把它们拦在 AV Host 之外**（`ALLOWED_COMMANDS = {1,5,55}`） |
| `NodeIKernelAVSDKService.startGroupVideoCmdRequestFromAVSDK(a, b)` | 数字 1..40/55/100（空载荷与会话参数）、字符串命令名 `hangup`/`endCall`/`closeRoom`/`leaveRoom`/`quit`/`reject`/`cancel` 等 24 个 × 两种载荷 | 无效果（这条确实不是 1v1 通话的路径） |
| `NodeIKernelAVSDKService.setActionFromAVSDK(type, payload)` | 数字 1..25/30/40/55/100 + 会话参数 | 无效果（它属于主 QQ 的 AVSDK 服务，不是 AV Host 里那通电话） |
| `NodeIKernelAVSDKService.sendGroupVideoJsonBuffer(a, b)` | 数字 1..30/40/55/100/20001/20004/20006 + 会话参数 | 无效果（同上，群视频通道） |
| 枚举 AVSDK 服务全部方法 | `getOwnPropertyNames` 沿原型链 | 只有 7 个方法；真正的通话控制在 AV Host 的 **PPAPI 插件**里，不在这套内核 API 里 |
| 杀掉 AV Host 进程 | 通话中断后由守护脚本拉起 | **对方通话不会结束**：房间由服务器保留，必须由客户端显式"离开房间" |
| 上游仓库 `ClaudiaGardner/maibot-qq-voice-call` | 全仓库搜索 + commits + issues | 没有任何挂断实现或说明；我们固定的 `22f30c0` 就是上游 HEAD |

补充（同日 02:20–02:40，抓包 + 内核 API 二次排查）：

| 尝试 | 结果 |
|---|---|
| 内核接口类型码 291 / 344 | 无效果（这两个数其实是**帧长**的误读，见下） |
| AV Host 的 X 显示截图（通话中） | 全黑：headless 下它根本不渲染通话窗口，`xdotool` 无从点击 |
| 线协议抓包 | 帧头是 `5b 00 | 0x01 | 总长(u8)`，之后才是消息体；对端挂断时本端先收到一个 35 字节控制帧，回一帧确认，随后服务器销毁房间（`onS2CActionToAVSDK {destroyReason:1}`）。**之前记的"类型 291/344"是误读**：0x0123 = 291 恰好是 35 字节帧的总长 |
| 手机 ↔ 服务器（中继模式） | 媒体经腾讯中转（`…:8000`），走 protobuf；与"挂断 API"无关，挂断不经过这里 |

**真正的入口**（03:00 逆向 `libAVSDKPlugin.so` 得到）：

- `libAVSDKPlugin.so` 是 PPAPI 插件，`host.html` 用 `plugin.postMessage({cmd, id, param})` 调它；
  `PPP_InitializeModule` 把消息回调注册成 **`CallCpp(cmd, id, param)`**。
- `CallCpp` 里 **`cmd` 就是 `QRTCServiceInterfaceWrapper` 的方法表下标**（`g_api_funcs`，
  `cmd > 111` 走群视频的 `od_api_funcs`，即 cmd 100000+）：

| cmd | 方法签名 | 说明 |
|---|---|---|
| 1 | `Login(id, uid, uin, uin, accountPath, "")` | 上游桥在用 |
| 5 | `Accept(id, uinType, uid, uids[], …)` | 上游桥在用（自动接听） |
| 8 | `Quit(id, uint roomId, int reason)` | 方法表里叫 Quit，但实测**不能挂断**（桥的事件计数照涨、`endReason` 一直为空）——**已从桥的白名单里去掉** |
| 9 | `Reject(id, uint roomId, uid, int reason)` | 拒接（未验证，同样不在白名单里） |
| **10** | **`Close(id, uint roomId, uid, int reason)`** | **真正结束通话**（实测有效，见下） |
| 11 | `ClearRoom(id, uint roomId, uid)` | 清房间（未验证，同样不在白名单里） |
| 55 | `OnPenetrateEvent(id, type, payload)` | 上游桥的 kernel-forward |

- 本仓库的服务端补丁 `patch-plugin-hangup.py` 只把 **10** 加进 AV Host 白名单
  （`{1, 5, 10, 55}`），并新增 `POST /v1/calls/hangup`
  （`{"method":"close","roomId":0,"reason":1}`）；机器人侧对应
  `qq_call.hangup_enabled`/`hangup_method`（只接受 `close`）/`hangup_reason`。
  8/9/11 已从白名单移除：8 实测挂不断，另外两个没验证过。

  关于"对方界面显示要请多人通话"：**与 cmd 8 无关**。2026-09-12 早上重启容器后
  （白名单只放行 10）再测一通，对方界面仍然出现这个提示，而命令直方图显示这一通
  只发过 `10`。来电事件本身就叫 `OnInviteActionToAVSDK`（`inviteType=1`，还带
  `relation_id`），所以这更像是 QQ 把 1v1 通话实现成"两人群视频房间"的固有表现；
  该提示不影响通话正常建立与结束（`endReason=4`）。
- **2026-09-12 03:20 真机实测**（对方手机打进来、通话中逐个候选试）：单发 `quit`
  （roomId 取 0 / 1 / 来电元组里的数 / reason 0/1/2/3）全部"受理但无效"——桥的 AVSDK
  事件计数照涨、`endReason` 始终为空，说明本端没被移出房间；紧接着发 `close` 后
  桥立刻变成 `ended` + `endReason=4`，事件计数停止增长，对方手机上的通话结束。
  因此默认方法选 `close`（`quit` 仍可用 `method` 显式指定），参数固定为
  `close + roomId=0 + 来电者 uid + reason=1`——就是实测成功的那一组。
- 结论修正：**挂断 API 一直都在**，只是既不在 OneBot、也不在内核 AVSDK 服务里，
  而在 AV Host 的 PPAPI 插件方法表里。

### 挂断参数实验（2026-09-12 早上，逐通真机测试）

`Close` 的类型是从符号表读出来的（`Close(unsigned int id, unsigned int roomId, char const* uid, int reason)`），
参数含义只能按位置推断，所以逐个组合在真机上试：

| 方法 / 参数 | 能否结束通话 | 对方界面 |
|---|---|---|
| `close` + `roomId=0` + 来电者 uid + `reason=1`（当前默认） | ✅ `phase=ended`、`endReason=4` | 出现"对方邀请其他人加入，将转为多人电话…" |
| `close` + `roomId=1`（来电元组 `invite[3]`）+ 来电者 uid | ✅ 同上 | 同上 |
| `close` + uid **留空** | ❌ 受理后阶段卡在 `ending`、`endReason` 始终为空 | — |
| `close` + **机器人自己的 uid** | ❌ 同上 | — |
| `clearRoom`(cmd 11) + 来电者 uid | ❌ 同上 | — |

**读状态时的坑**：`ending` 不是 AVSDK 报的，而是桥插件在**受理任何一次挂断请求**时自己先写的
（`POST /v1/calls/hangup` 一进来就置位），所以 `close`/`quit`/`clearRoom`/uid 留空/uid 传自己
全都显示 `ending`——它只说明"请求被桥收下了"，不代表电话在挂。判断是否真的挂了要看
**`ended` + `endReason` 非空**，以及 AVSDK 事件计数是否停止增长：cmd 8 那几次 `ending` 期间
事件计数从 601 一路涨到 721、`endReason` 始终为空，就是"没挂"的铁证。

结论：

- **唯一能真正结束通话的是 `close` + 来电者 uid**（`roomId` 填 0 或 1 都行；uid 不能省，
  也不能换成自己）。因此桥只放行 10、方法表只留 `close`，机器人侧 `hangup_method`
  也只接受 `close`。
- 对方那句"对方邀请其他人加入，将转为多人电话…"**与 cmd 8、与 roomId 都无关**：白名单
  只放行 10 的那几通照样出现，换 roomId、换方法也消不掉。它是 QQ 收到"这通两人房间被
  对方结束"时的固定文案（来电事件本身就叫 `OnInviteActionToAVSDK`、`inviteType=1`，
  协议里 1v1 通话就是两人群视频房间），客户端侧改不了。通话本身是正常结束的
  （道别 → `ended` → `endReason`），只是对方界面会闪这一句。
- 唯一没试的变体：把来电事件里的 `relation_id` 字符串当 uid 传（需要插件多记录一个
  字段）。这是唯一可能绕过"uid 被当成邀请对象"的思路，先记在这里备查。
- 复现这些实验的脚本留在服务器：`/home/ubuntu/napcat-qq-call/try_close_variant.py`
  （等来电接通 → 用指定变体挂断 → 8 秒内没结束就自动用已知可用参数兜底，不会把对方
  悬在静音通话里）。

### 外呼（她主动打给我）—— 已真机验证 2026-09-12

私聊发 `#打给我`（别名 `#打电话给我`；**仅主管理员与副管理员**，见下）→ `POST /v1/calls/dial` → 桥把 QQ 号解析成 AVSDK
uid → cmd 4 `StartCall` 带 JSON payload。**真机结果：手机正常响铃，通话可用。**

判定成败**只看 AVSDK 回执，不能看阶段**：

- 拨出去后 AVSDK 回 `20021`，内容形如
  `{"is_peer_online":true,"is_pc_online":false,"is_phone_online":true,...}`；
  收到它就说明邀请真的发出去了（真机上手机随即响铃，Rust 侧 6 秒窗口内拿到，实测不到 1 秒）；
- **呼出的通话不会让桥进入 `ringing` / `connected`**：`state.call` 会一直是
  `idle`，然后直接跳到 `ended`，`peer` 始终是 `null`。所以"阶段没变成 ringing"
  不等于失败，按阶段判会误报；被叫存在 `state.call.dialedUin` 里。
- 收到回执后机器人回「好，我打给你啦，接一下～」；6 秒内没回执则回
  「我让桥拨了，但没等到 AVSDK 的回执，多半是没拨出去——这个我还在查。」，
  不会假装成功。
- 呼出的通话用桥的 `dialedUin` 认领对端，不会把被叫当成未知来电婉拒；日志里也按
  "我方外呼（被叫 N）"命名，不会印成"未能解析来电者 QQ 号"。

> **谁能让她打出去**：只有管理员。规则是**带 `#` 的命令只由管理员下达**（私聊与群聊
> 走同一道闸：`is_restricted_command && !sender_is_admin` → 静默），`#打给我` 只是
> 这条规则的一个实例——`#打给我` 与
> `#系统信息`、`#授权群` 是同一条规则，不是特例：授权好友能和她聊天、能打进来，
> 但**不能指挥她做事**。通话授权名单管的是"谁能打进来"，不是"谁能下命令"。
> 所以非管理员发这条命令会被静默丢弃（2026-09-12 真机上静默过一次，本人不知道
> 发生了什么——静默是有意的，没必要向授权好友解释"你的权限不够"）。
> 实现里另外那道"通话名单"判据与它语义不同，串起来用，长得像矛盾而已。

定标过程中查清的关键事实：

- `StartCall`（cmd 4）的字符串参数**必须是 JSON**：传裸 uid 会被回 `[3,"json parse error"]`；
- JSON 字段名取自 QQ 自己的 JS↔原生绑定属性表（在 `/opt/QQ/resources/app/major.node` 里）：
  `self_uid / invite_count / invite_uids / sub_business_type / invite_reason /
  invite_original / audio_scene / use_ntrtc_dsp / ntrtc_ai_denoise_update_model /
  c2c_extend_params / opensdk_enter_room_params`；
- 判据在 `.so` 的日志串里：**`GetRoomId skip: invite_uids empty, scene=%d`** —— `invite_uids`
  为空就直接跳过、不拨号（之前所有尝试都命中这条）；
- **手工 JSON 有几率把插件打成 segfault**（Bugly `signo: 11`），所以每次尝试之间要重启
  AV Host；命令回复内容读 `lastRawValuePreview`（别信那个块缓冲日志）；
- 定标工具：`try_startcall.py`（自动重启 → 探活 → 发送 → 守卫式读回复），
  最终验证通过的参数组合已固化进 `patch-plugin-dial.py` v5。
- 取证套路：AV Host 的 `/v1/status`（6111，带 token）里有 `invocationCount` /
  `lastInvocationCommand` / `lastRawCommand` / `lastRawValuePreview`，是真·实时读数；
  桥的 `/v1/status`（6110）里有 `eventCount` 与 `call.dialedUin`。两者定时轮询就能
  在事后逐秒复盘一次外呼，不必依赖块缓冲日志。

下一步（可选）：外呼没有"对方拒接/无人接听"的回执分支，桥侧一律是 `ended`，
所以暂时无法区分"没人接"和"接通后挂断"。**认知层因此也只分三种结局**（见下）。

### 三条触发路径（2026-09-16 加）

外呼现在有三个入口，共用同一道闸门 `qq_call::dial_peer`（"打给谁"必须在进来之前绑定好）：

| 谁发起 | 入口 | 目标 | 说明 |
|---|---|---|---|
| 管理员 | 私聊 `#打给我` | 发起者本人 | 手动兜底，随时可用 |
| **她**（应要求） | 工具 `call.start` | **本轮跟她说话的人** | 群里就是发言者本人；schema 里没有任何"打给谁"的参数 |
| **她**（自己起意） | 主动接触走电话媒介 | 机会里那个人 | 定时器触发，见下面的媒介策略 |
| **她**（续聊时改主意） | 自主回合里的 `call.start` | 当前会话的人 | 那一轮**只**带通话这一个工具，不是整套工具清单 |

`call.start` 归 `EffectScope::Outbound`，所以"任务读过外人文字之后档位收窄到
`UserScoped`"那条既有防线自动覆盖它——**别拨号由档位保证，不靠模型自觉**。
它也**不进定时任务**：到点自己拨号、现场没人看着，是另一种东西。

它只对**明确的、把电话打给说话人**的请求下发。路由判据（`requests_call_to_speaker`）
刻意只认"打给我/给我打个电话/打电话给我"这类说法，并把能力询问、用法讨论、否定、
假设、完成态排除掉——判错的代价是**凭空拨一通电话**，比该打的时候打不出去严重得多。
已知边界：第三人称（"打电话给我妈"）在文本层判不出来，子串上和"打电话给我"同源；
兜底不靠文本判据，而靠目标绑定——就算判错，响的也是**提问者本人**的手机。

**媒介策略（主动接触）**：这条路是**定时器**触发的，人已经不在对话里，所以媒介不由
模型在生成话题时挑，而由配置里的动机白名单决定：

```toml
[proactive]
# 哪些主动理由可以用电话打出去；留空 = 从不主动打电话
call_motives = ["check_in"]
```

默认只有 `check_in`（"担心他最近怎么样"，打电话贴切）；分享/好奇/回应/跟进一条消息就够。
动机名拼错**在加载时**报错并列出可用取值——静默忽略的话，"配置了不生效"只会表现为
"她从来不打电话"，运行时什么都看不出来。

决定媒介时会先问 `qq_call::can_dial`（开关 + 名单 + 没在通话 + 桥读得到），不满足就
**退回消息**。反过来（选了电话再失败）只能要么放弃这次接触、要么把一句"喂，是我"当
正文发出去，两种都不好。退回时记一条日志。

主动打电话时会带上**开场白**：话题生成的内容就是接通后她说的第一句，不再是配置里那句
通用问候语。跨任务传话用带 TTL 的待用区（拨号在投递侧、接通在调度器轮询侧是两条链），
拨号前存下、拨不出去就撤销——一通没接的电话不该把这句话留到之后那通**来电**上用。

**自主回合为什么只带一个工具**：`AutonomousConversationTick` 是"她自己想说话"的回合，
原本一个工具都没有，所以她想"这次改用电话而不是消息"都做不到。放开工具协议会让它带出
**整套**清单（建提醒、发消息、改群状态……）——那是在没人要求她的回合里，比她需要的多得多。
所以那一轮收窄到通话一个（`autonomous_call_only`）。宿主没声明通话工具时维持原样：
一个工具都不带。

注意判据问的是**工具声明**（`PlannerInput::declares_tool("call.start")`），不是某个能力
变体：所有工具一律挂在 `ActionCapability::UseTool` 下、靠名字区分，Core 不为单个工具设
能力。所以"宿主能不能打电话"这件事在能力快照里就是"有没有 `call.start` 这条声明"，
只有一种说法。

### 外呼的三道闸门（默认全放开）

外呼原语里已经内置了三道闸门，**默认全部放开**——这是明确的选择（先把功能做起来），
但每一道都真的生效，要收紧改配置即可，不必改代码：

```toml
[qq_call]
outgoing_quiet_hours = ""    # 例如 "23:00-08:00"（支持跨午夜）；留空 = 不限时段
outgoing_daily_limit = 0     # 每人每天最多被主动拨几次；0 = 不限
outgoing_min_interval_secs = 0  # 两次外呼同一人的最短间隔；0 = 不限
```

两道判据的细节值得记住：

- **静默时段支持跨午夜**，而且**起止相同 = 整天静默**。写错的方向很危险：把
  `23:00-08:00` 判成空区间等于这道闸门完全不生效，而失败表现是"半夜打了一通电话"。
  非法写法在**加载时**报错，不会静默忽略。
- **频次账本记在进程内，重启即清**。它是**频次闸门**，不是配额账本：作用是配置开启后
  拦住"短时间内反复拨同一个人"，不保证跨重启的精确日上限。要精确计数应该落
  Redis/Postgres，那是另一个决定。只对**真的拨出去了**的记账，没拨出去的不占额度。

撞上闸门时：私聊命令回一句说明；工具回"电话没拨出去 + 原因"（照实转述，不记成工具故障）；
主动接触则报 `Deferred`——静默时段和频次都是"此刻不行、以后可以"，报成永久失败会让这次
机会直接作废。

**这个决定不新增风险面**：主动接触这条路本来就有 `cooldown_secs`、`daily_limit`、
`target_cooldown_secs`、`recent_interaction_cooldown_secs` 四道闸门在媒介决定之前，
电话继承它们。真正无上限的只有"有人当场要她打"那条路径。

### 通话结束会进认知层（2026-09-16 加）

挂断后除了写进对方私聊记忆，还会给 Core 发一个 `CallEnded` 事件（对端、是不是自己拨的、
结局、时长）。分三种结局，**不编更细的**：

| 结局 | 判据 |
|---|---|
| `completed` | 进过房 |
| `unanswered` | 没进房：外呼没接 / 来电漏接 |
| `refused` | 名单外婉拒 |

判据的先后顺序有讲究：**授权判据排在"进过房"前面**。名单外的来电**也会进房**
（桥自动接听，名单外只是播报一句婉拒然后静音），所以"进过房"单独用会把每一通被婉拒的
来电都记成"她跟这个人说过话"。

注意力只对**自己拨出去、没接通**这一种要一轮规划——那是唯一需要她收尾的结局（改发一条
消息是自然的收尾，或者判断此刻不适合打扰而什么都不做）。别的一律 `ObserveOnly`：
仍然记进有界工作状态（"刚打过"她记得住），但不为它花钱跑模型。刚说完话的回合没必要再补。

事件发不出去**不影响通话收尾**：`record_call_ended` 吞掉所有失败只记日志——电话已经
结束了，认知层漏记一次不该变成一个可见的故障。

### 通话中的工具调用（电话里也能办事）—— 2026-09-12

以前电话是一条封闭回路：`语音 → ASR → 查挂断关键词 → 模型 → TTS`。模型走的是
`params_model_with_plain_style_context`（**不带工具**），所以她在电话里只能说话——
让她"给群里发一声"她可能嘴上答应，实际什么都不会发生。现在电话模型走原生
function-calling（`params_model_with_native_tools`），和私聊同一套受控工具。

**开关**：`qq_call.phone_tools_enabled`（默认开）。关掉即回到旧行为，并且提示词会
换成"这通电话里你没有工具可用"，让她如实说做不了、而不是口头答应。

**打电话也能发文字**：通话中的她可以给**群**（`group.message.send`）和**好友**
（`private.message.send`）发消息。两条路都只对主管理员开放，且只在私聊/通话场景可用
（群聊里不给这两个工具）。名字不是 QQ 号时先走 `private.contacts.search` 解析，
只有唯一匹配才允许发送；非好友一律拒绝——机器人不该变成"给任意 QQ 号发消息"的工具。

> **通话里没有"来源消息编号"**，而 `group.message.send` 原本是挂在聊天消息上的：
> 它要 `source_message_id` 做幂等键、并写进 `kovi_bot_agent_goals` 台账（跨群问答
> 就是靠它追踪回复）。电话里没有消息，所以那条路在通话中会直接报"缺少来源消息编号"。
> 现在按来源分流：**有来源消息**走原来的 goal 台账（支持收集回复）；**没有**（通话）
> 退化成一次性直发 `send_tracked_plain_text`，并且明确拒绝 `collect_replies_minutes`
> ——如实告诉她"消息能发，但等待回复要等挂断后在私聊里再说"。这是有意的不对称：
> 宁可少一个能力，也不要为了凑幂等键去编一个不存在的消息编号，把审计台账污染掉。

**权限与私聊完全一致**：工具清单由 `native_tool_specs` 按来电者身份过滤，
上下文是 `destination = Private(来电者)`。也就是说电话本身不带来任何额外权限——
主管理员在电话里能跨群发消息、建提醒、启动持续任务；普通授权来电者只有
时间/天气/网页/新闻/算数/记忆检索这类。几个会自动落空的门：
`group.members.search` 要求群聊场景；`sticker_memory.teach` 要求带表情教学上下文。

**独立回复作用域**：本通话绑到 `ReplyScope::Call(对端 QQ)`。工具执行要用
`ReplyTicket` 证明"这一轮还是当前轮"，若借用 `Private(对方)`，对方在通话期间发一条
私聊就会推进那个作用域的代数，通话里的工具调用会整批失败。

**实时性上的三个取舍**（电话是对话，不是异步任务）：

1. **填充语按需出现**：工具超过 700 毫秒还没回来，才先说一句 `tool_filler`
   （默认"嗯……我看一下。"）。`time.now` 这种瞬时工具不会白白多一句话。
   填充语与工具**并发**：说话的同时工具已经在跑，说完正好接结果；被打断也不影响
   工具跑完。
2. **轮次有上限**：一次回复最多 `tool_max_rounds`（默认 3）轮工具，覆盖
   "先查时间→再算日期→再建提醒"这类串联。到顶还想要工具，就把已拿到的结果折成
   一条资料，强制她用一两句口语收尾。
3. **审计**：每次调用记 `QQ 通话工具调用: <工具> <参数摘要>` 与
   `QQ 通话工具结果: <工具> 成功/失败（N 字）`，参数截断到 160 字符——电话里说的
   话可能涉及私事，日志只留够排查的片段。

**空承诺补跑：说了要查就必须真的查（2026-09-13）**。线上实测：对方问"你能不能查到群聊记忆"，
她回"那我先看看有哪些群，等我一下"，然后**整通电话再无下文**。查日志才看清：那一轮
`tool_calls=0`——她一个工具都没调，所以**根本没有东西在跑**，对方等多久都不会有结果；
更早那句"我看了下，能查到的是待过的群的一些信息"同样是没查就编出来的。

根因不是模型不会用工具（同一通电话里 `memory.search` 她刚用成功过），而是
**"说出口的承诺"和"真的调用"之间没有任何连接**：`generate_reply` 一看到模型回了纯文本就
结束这一轮。现在的做法：

- **判据**（`ACTION_CLAIM_MARKERS`）覆盖三类原话：发送类（"我再发一次"）、
  **查询类**（"我去找找""等我一下""我看看有哪些"）、谎称已完成类（"我看了下""查到了"）。
  命中就当她许了一个诺。
- **动作**：模型这一轮回纯文本且命中判据时**不结束**，把 [`commitment_nudge`] 作为系统
  纠正塞回上下文，让她重来一轮——最多 `claim_retry_rounds`（默认 2）次。日志会写
  `QQ 通话空承诺补跑 1/2：回复里出现「等我一下」但本轮没有工具调用，要求她真的执行`。
- **纠正是三条出路，不是"必须调工具"**：① 有对应工具就现在调用、拿结果再说话；② 已经
  真做完了就直接说结果；③ 确实没有工具能做就**如实说做不到**。少了第三条，模型会被逼着
  编一个结果出来——那比空承诺更糟。
- 补跑次数用尽仍无工具调用时，保留 `[WARN] QQ 通话疑似只承诺未执行` 告警。
  `#通话自检` 的报告里会多一行"空承诺补跑：N 次"。

**一直说、不要停：连讲（2026-09-13）**。线上实测对方说"连续说一直说不要停"，她讲两句
就停——不是模型不肯讲，是**三层天花板一起卡住**：

1. `sanitize_reply` 只取模型输出的**第一行**（历史遗留："电话回复就是一句口语"），
   而模型组织长内容天然会分行分段——只留第一行等于把她后面讲的整段丢掉；
2. `max_reply_chars` 默认 40（线上配置也是 40）：中文 40 字就是一句；
3. `PHONE_MAX_TOKENS` 256：一次生成的总预算，写到一半就会被掐断。

现在三层都松开（并接成一条链）：

- **多行读成一段**：`sanitize_reply` 把各行正文按顺序接起来（上一行没有句末标点就补逗号），
  只丢掉纯舞台指示的空壳行（`[轻声]`）。
- **单段上限 120 字**、生成预算 768 token；日常回话仍由提示词压在一两句，这只是安全上限。
- **`[[继续]]` 标记 + 宿主接续**：她在回复末尾（单独一行）写 `[[继续]]` 表示"还有下文"，
  宿主**不等对方开口**就立刻让她接着讲下一段（`continue_monologue`），最多
  `monologue_max_chunks`（默认 30）段。每一段都走同一条生成+播报路径，所以对方随时
  可以插话——`speak()` 被打断就立刻停，剩下的不再讲。
- **安静的含义跟着变**：对方说过"不要停"之后（`wants_monologue` 判据），20 秒安静触发的
  主动出声不再问"你还在听吗"，而是"接着上一段往下讲"。
- 日志：`QQ 通话连讲第 N 段: …`、`QQ 通话连讲：对方开口了，先停下听他说`。
  提示词里同时写明"被打断是正常的（说明他在听），不用道歉也不用重新开头"。

**换气要像人（2026-09-13 二次调）**。第一版连讲虽然接上了，但两次日志里她的开场白
**一字不差**（01:26:44 与 01:26:49 都是"好，那我接着讲。"），停顿也每次一样长——
听起来就是机器在念稿。三处一起改：

- **禁掉过渡语**：连讲提示词直接把她的原话点出来（"不要用'好，那我接着讲''那我接着说'
  这类过渡语——连讲时每段都这么开场最假"），并把**上一段的结尾原样贴回提示词**
  （`last_spoken_tail`，取最后 30 字），让她接在那句话后面，而不是重新起个头；
  系统提示词里也加了同一条，管住"对方说'继续讲'"这种直接回合。
- **停顿抽签**（`speech_pause`）：一半左右轻轻一顿（150–400 毫秒），三成 450–900 毫秒，
  一成多 0.9–1.6 秒，剩下偶尔拉到 1.6–2.8 秒——真人讲故事就是这个分布。换气期间
  对方一开口就让位。
- **连讲模式的沉默阈值**（`MONOLOGUE_BREATH`，4 秒）：对方说过"不要停"之后，她要是
  自己收了尾（没带 `[[继续]]`），4 秒就接上，而不是等满 20 秒——线上那次就是靠 20 秒
  的兜底才接上的，故事听着像断了。

**句间也该有停顿：分句播出（2026-09-13 三次调）**。上一版只把**段与段之间**的换气
做了抽签，句子内部仍然由 TTS 固定的韵律决定——于是段界那一停（真实等待：模型调用 +
TTS 首包 ≈ 1–2 秒）比句间停顿长出一大截，"连讲"听起来还是一顿一顿的。现在整条
播报路径改成**分句播出**：

- `speech_segments` 把回复按 。！？…；~ 切成"一口气能念完"的短句，只有"好。""嗯……"
  这种一两字的语气词并进邻句（否则像打了个嗝）；切分不丢字（有测试保证拼回等于原文）。
- 逐句合成、逐句播；播第 N 句时后台已经**预取**第 N+1 句（`synthesize_segment` 丢进
  tokio 任务），所以停顿里不含 TTS 首包时间，打断时预取任务立刻 abort。
- 句与句之间插入**真正的静音帧**（`write_silence`，按 100 毫秒一片写，期间对方插话
  立刻停），长度由 `speech_pause` 抽签决定——**连讲的段间换气用的是同一个函数**，
  同分布才不会显得突兀：听起来就是她在想下一句，而不是机器卡了一下。

**主动出声：对方不说话时她先开口（2026-09-13）**。以前回复链是纯被动的
`while let Some(job) = jobs.recv()`——**她只在对端出声时才说话**，对方一安静，两边就一起
静音，听起来像掉线；这也正是"她说去找了、然后一直没回复"最直观的那一面。

现在回复链在"她说完一句之后"起一个安静计时器（`idle_prompt_secs`，默认 20 秒，0 = 关闭）：

- 到点她**主动说一句**：现场说明（[`idle_prompt`]）要求她先自然地问一句对方怎么了、
  还在不在、是不是没听清，**并顺手把没做完的事做完**（"刚才说要查的东西"这一轮真的调用
  工具、把结果报出来），且不许重复上一条。
- **退避**：连续主动出声按 20/40/80/160 秒翻倍（上限 8 倍），次数上限
  `idle_prompt_max`（默认 3）；对方一开口计数归零。到顶就回到安静等待，不催问。
- **不抢话**：计时器用 `biased` 的 `select!`，队里已经有对端语音就先处理对方；两次
  检查（wake 时和开口前）都过了才出声。被打断同样不计入通话上下文。
- **不给挂断权**：主动出声这一路即使模型带了 `[[挂断]]` 也忽略——对方只是没说话，
  不该因此被挂电话。挂断仍只由"对方明说 / 她回应时判断 / 到点"触发。

两个能力是配套的：补跑让"我去找找"当场变成结果，主动出声让**任何**沉默都不再无限延续。

**不通话也能验：`#通话自检 [问题]`**（**仅管理员**——它也是命令，别名 `#电话自检`）。它复用
**同一个** `generate_reply` 跑一遍工具链路——同样的模型调用、**同样的工具清单**、
同样的结果回灌——只换两样：音频出口换成记录（不出声），执行换成试跑。
试跑的分寸（`PhoneToolMode::Rehearsal`）是特意这么切的：

- 清单**必须和真通话一致**。第一版把自检做成"只给只读工具"，结果
  `#通话自检 给我发个消息` 报的是"0 次工具调用"——因为 `group.message.send`
  根本不在清单里，她无工具可选。这验不出"她本来会不会调"，只看得出她会追问。
- 执行**分两种**：只读工具真跑（报告里的结果是真结果），有副作用的只记下
  "本来会调用 XX"、绝不执行，报告里标 🟡 并写明"换成真通话她会真的执行这个动作"。
  判据用注册表自己的 `available_read_only_for_context`，不另维护一张表，两边不可能对不上。

授权门槛与打电话一致；报告会说清：可用几个工具、调了哪些、成功没有、本来会说什么、
以及工具慢到会不会触发填充语。

存在的理由：验证"电话里能不能办事"原本必须真打一通电话，成本高、还得有人正好有空接。
`#通话自检 现在几点` 在私聊里发一句，就能把除音频与桥以外的整条链路验穿。

**ASR 会听错，所以有确认话术**：实测"挂了吧"能被识别成"过了吧"。提示词因此要求：
凡是会改变外部世界的动作（发消息、创建/取消提醒、启动持续任务），她先把要做的事
用一句话复述并**等对方明确答应**，对方没说"好/对/可以/发吧"之前不许调用；关键信息
（发给谁、发什么、什么时候）不确定时宁可追问，不许自己补齐。这是提示词层面的约束，
不是代码强制——`tool_max_rounds` 和审计日志是代码层面的兜底。

**已落地的机制**（2026-09-12 凌晨）：

1. **主动挂断（默认开启）**：`qq_call.hangup_enabled = true` 时，机器人结束会话
   （对方要求挂断 / 名单外婉拒 / 到 `max_call_seconds` / 采集链路中断）后会调用
   `POST /v1/calls/hangup`，请 AVSDK 执行 `Close`（cmd 10）。关掉即回到旧行为：
   "只能停止参与，等对方挂断"。
2. **可选的"未授权来电不接"**：`qq_call.caller_allowlist_enabled = true` 时，机器人把
   「授权名单 ∪ 副管理员 ∪ 主管理员」写进 `qq_call.caller_allowlist_file`（默认
   `/home/ubuntu/napcat-qq-call/bridge/runtime/allowed-callers.json`），桥在接听前读它，
   名单外直接不接听。**默认关闭**（保持上游的"接通后婉拒"），因为这会改变来电者
   听到的结果。
3. **对方说"挂了吧/先挂/挂电话"**：两条路径都会挂断。
   - 关键词：`hangup_keywords` 命中就播一句 `farewell` 然后挂断（最快，但不抗识别错误）；
   - 模型判断：电话提示里约定"对方要结束通话时在回复末尾加 `[[挂断]]`"，模型自己判断
     并道别，代码见标记就挂断。2026-09-12 早上实测对方说"挂了吧"被 ASR 识别成
     "过了吧"，关键词没命中、电话一直挂着，所以补上了这条语义路径（标记会随
     `[[...]]` 协议标记一起从朗读文本里去掉）。
4. **到 `max_call_seconds`**：同样先道别，再挂断。
5. **名单外婉拒（回退路径）**：播完婉拒即挂断，不再无限静音挂着。
6. `#通话状态` 增加一行"接听授权名单：N 人（机器人同步于 …）"。


## 排错

先在机器人私聊里发 `#通话状态`（管理员，别名 `#通话诊断`）。它不登录服务器就能回答
大部分"为什么不接电话"：

```
QQ 语音通话：已启用（桥 http://127.0.0.1:6110，轮询 250 毫秒）
当前：桥状态 idle（无通话）
最近一次通话：小猫ᓚᘏᗢ(3052405886)，通话授权: 已授权，来电 3 分钟前，从未进房（桥没有建立音频会话），结束原因: 未进房就结束
语音服务：http://127.0.0.1:6120/v1/tts 正常
```

需要服务器视角时，手动触发 GitHub Actions 里的
[`Diagnose production`](../.github/workflows/diagnose-production.yml)（Actions →
选择该工作流 → Run workflow）。它**只读**：服务状态、最近 500 行日志里各类通话标记的
次数与最后时间、桥的 `/v1/calls/current` 与 `/v1/status`（含 `avHost.commandHistogram`）、
容器与插件目录、`doctor.sh`、语音服务 `/healthz`、隔离 PulseAudio。因为仓库是公开的、
Actions 日志谁都能看，它的输出已经脱敏：QQ 号/群号掩码成 `<num>`，昵称与群名不打印，
桥的 JSON 只保留白名单字段。要看原文请登录服务器 `journalctl -u kovi-bot -f`。

三种典型结论：

| 报告里看到 | 说明 | 下一步 |
|---|---|---|
| **某个号打不通（"无人接听"），换个号一打就通** | **不是桥的问题**，是来电方账号/设备侧（QQ 对该号的通话限制或它的通话设备状态）。2026-09-12 上午一台好好的桥被这个现象查了两个小时 | 先看 `/v1/status` 的 `eventCount` 与命令直方图**有没有增长**：完全不增长＝信令根本没到我们这侧，别再顺着桥查；让来电方换个号、或在那台设备上重新登录 QQ 再试 |
| `最近一次通话：桥还没有上报过任何来电` | 桥根本没收到这次来电（QQ 掉登录态、AV Host 没起来、插件没加载） | `doctor.sh`、`run-napcat.sh`；容器重建后确认补丁与插件都在 |
| `从未进房`，或当前停在 `ringing` / `accepted` | 桥接听了信令，但 AVSDK 的会话没建立（就是 20050 那类问题） | 确认 `patch-ignore-20050.py` 生效，看 `/v1/status` 的命令直方图 |
| `已授权` 且 `已进房`，但对方听不到声音 | 通话建立过，问题在音频或语音服务 | `curl http://127.0.0.1:6120/healthz`，再按下面几行排查 |
| 收到了来电（`20006` 增长）但没有 `5`/`20004` | 桥没接或没进房 | 看插件日志里的 `native auto-accept failed`；确认 `patch-plugin-login-refresh.py` 的两个前提（`lastOutputAt` 字段与安静期常量）都在 |
| 来电响了但一直没人接，`20006` 计数为 0 | **AV Host 的 AVSDK 没拿到来电**（信令被路由到主 QQ 那台设备），接听参数（12 元组）只有它回报的 `20006` 里才有，监听那条路给不出（只有 `relation_id`/`invite_type`/`from_uid`，后者还是空的） | `patch-plugin-accept-retry.py` 会重投 payload（最多 2 次）；看 `avHost.acceptRetryCount` / `acceptRetryGaveUp` 与 `outputTrail` 判断是路由问题还是别的问题 |

阶段变化也会写进机器人日志，一次通话的完整时间线是这样的：

```
[INFO] QQ 语音通话来电振铃: 小猫ᓚᘏᗢ(3052405886)；通话授权: 已授权（桥会自动接听，名单外只播报婉拒）
[INFO] QQ 语音通话桥正在接听: 小猫ᓚᘏᗢ(3052405886)（阶段 accepting）
[INFO] QQ 语音通话已进房: 小猫ᓚᘏᗢ(3052405886)（桥已接好音频设备）
[INFO] QQ 语音通话已接通: 小猫ᓚᘏᗢ(3052405886)
...
[INFO] QQ 语音通话结束（对方挂断，时长 51 秒）
```

**没有"来电振铃"这一行，就说明桥没把这次来电上报给机器人**——问题在桥或 QQ，不在机器人，
也不需要去改通话授权。

| 现象 | 先看 |
|---|---|
| 日志一直报"无法读取桥状态" | 桥没起来：`doctor.sh`、`run-napcat.sh` 的输出 |
| 有"来电振铃"但一直没有"已进房" | 桥接了信令但 AV 会话没建立；按上面的 20050 补丁与命令直方图排查 |
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

### 六处已固化为自愈的补丁

容器入口包装 `bridge-entry.sh` 每次启动都会幂等地重打这些补丁，因此**重建容器、
重装桥之后都会自动恢复**，不需要人工介入：

| 补丁 | 作用 |
|---|---|
| `patch-plugin-account-path.py` | NapCat 4.18.x 已删除 `session.getAccountPath()`，插件的回退值 `ctx.core.dataPath` 是 QQ 数据**根目录**而非账号目录（`nt_qq_<hash>`），需要自行解析 |
| `patch-20050-backoff.py` | 加入命令直方图（暴露在 `/v1/status` 的 `avHost.commandHistogram`） |
| `patch-ignore-20050.py` | **根因修复**：`20050`/`120043` 不再触发重登 |
| `patch-plugin-login-refresh.py` | **登录自愈**：AV Host 进程重启后拿不到登录参数（上游只在插件启动时投一次），插件空闲时每 60 秒补投一次，结果见 `/v1/status` 的 `avHost.loginRefreshCount` |
| `patch-plugin-caller-allowlist.py` | **接听授权**：接听前读 `runtime/allowed-callers.json`（`enabled != true` 时保持上游"谁打进来都接"），并把来电者写进状态 |
| `patch-plugin-dial.py` | **主动外呼通道（v5，已真机验证）**：插件新增 `POST /v1/calls/dial`（QQ 号 → AVSDK uid，再用 cmd 4 `StartCall` 带 JSON payload），把目标记进 `state.call.dialed*`，并把 AVSDK 的 `20021` 回执翻译成 `dialReachedAt` / `dialPeerOnline`。AV Host 白名单里的 `4`/`20` 由 hangup 补丁统一归一化 |
| `patch-avhost-raw-preview.py` | 在 AV Host 里记录"插件最近一条原始消息"的截断预览（`lastRawCommand` / `lastRawValuePreview`，仅本地 token 可见）。AV Host 的日志是块缓冲的、看不到最新内容，这个字段是实时的——定标外呼时正是靠它读出 `[3,"json parse error"]` |
| `patch-plugin-avsdk-trace.py` | **AVSDK 输出追踪**：把最近 20 条非心跳输出记进 `state.avHost.outputTrail`（命令号 + 类型/长度摘要），用来定位"来电时 AV Host 到底回报了什么" |
| `patch-plugin-accept-retry.py` | **来电回调重投**：邀请 payload 转给 AV Host 的 AVSDK 后等 `20006`，按 1.5 → 3 → 6 秒退避重投（共 3 次投递，覆盖约 10 秒铃声窗口）；彻底失败时打一条带**诊断快照**的 WARN（阶段、inviteAt、callerUin、重试次数、直方图、输出轨迹、监听事件），计数见 `avHost.acceptRetryCount` / `acceptRetryGaveUp`。背景：真机对照发现 `20006` 偶尔不来，此时电话会一直响到对方放弃 |
| `patch-plugin-hangup.py` | **主动挂断**：AV Host 的 cmd 白名单只加入 `10`（`Close`，唯一实测有效的方法），`invokeAVHost` 返回响应体，并新增 `POST /v1/calls/hangup`；见上面的"主动挂断"一节。写法是归一化（正则改写整行），能收敛上游原版与实验期间的各种历史状态 |

已验证的插件整份备份在
`/root/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs.kovi-verified`。

这些补丁的版本化副本在仓库的 `scripts/bridge-patches/`（连同 `bridge-entry.sh` 参考副本
和诊断脚本 `try_close_variant.py`），线上真身在部署机的 `/home/ubuntu/napcat-qq-call/`；
改完看那个目录的 `README.md` 里的同步命令。

### 仍然需要你知道的运维约束

- 重启 NapCat **必须**用 `sudo docker restart -t 60 napcat`。默认 10 秒就 SIGKILL，
  QQ 来不及落盘会话，会导致登录态失效、需要重新扫码。
- `kovi-bot.service` 已配置 `Restart=always`（kovi 断连是**正常退出**，退出码 0，
  所以 `on-failure` 不会触发），NapCat 抖动后机器人会自己回来。
- 完整诊断报告（含全部证据）见
  [`upstream-issue-qq-call-avsdk-20050.md`](upstream-issue-qq-call-avsdk-20050.md)。
