# 来电能被检测到，但 AV Host 的 AVSDK 每次登录都被 `20050` 顶掉，接听永远不会触发

## 环境

| 项 | 值 |
|---|---|
| 桥版本 | 0.3.4（提交 `22f30c021cd3170f75af9dff66cc959a07ebda4b`） |
| NapCat | **4.18.19**（`mlikiowa/napcat-docker` 镜像，2026-08-14 构建） |
| Linux QQ | **9.9.22-40990**（`linuxVersion: 3.2.20-40990`，二进制构建于 2026-07-03） |
| Electron | 40.0.0 / Chrome 144.0.7559.60（来自 AV Host 的 `/v1/status`） |
| 系统 | Ubuntu 24.04，x86_64，4 核 / 3.6 GB |
| QQ 运行方式 | Docker 容器，`--network host`，`--shm-size=1g` |
| `libAVSDKPlugin.so` | 存在（33,643,064 字节） |

`doctor.sh` 全部通过（`all bridge checks passed`）。

## 现象

真实来电（同一账号的另一台设备拨打机器人 QQ）**能被正确检测到**，但**接听永远不触发**，对方一直听到无人接听。

### 桥侧观测

```json
{
  "eventCount": 1,
  "recentEvents": [{ "name": "OnInviteActionToAVSDK" }],
  "listenerRegistered": true,
  "serviceAvailable": true,
  "serviceNull": null,
  "listenerError": null,
  "avHost": {
    "inviteCallbackSeen": false,
    "autoAcceptAttemptedAt": null,
    "autoAcceptPostedAt": null,
    "acceptOutputAt": null,
    "enterRoomOutputAt": null,
    "kernelActionCount": 1,
    "networkOutputCount": 0,
    "lastError": null
  },
  "call": { "phase": "ringing", "inviteType": 1 }
}
```

事件参数摘要（`summarizeValue` 输出）：

```
OnInviteActionToAVSDK
  args: [
    { type: "object", keys: [
        { key: "relation_id", value: { type: "string", length: 10 } },
        { key: "invite_type", value: { type: "number", value: 1 } },
        { key: "from_uid",    value: { type: "string", length: 0 } } ] },
    { type: "number", value: 5 },
    { type: "string", length: 852 }
  ]
```

即 `args[1] = 5`、`args[2]` 是 852 字节的 payload，`forwardKernelAction` 正常走到了
`invokeAVHost(55, [5, payload])`（`kernelActionCount` 从 0 变 1）。

### AV Host 侧观测

来电期间 AV Host 确实收到了转发（`/v1/status`）：

```json
{
  "ready": true,
  "pluginFound": true,
  "methods": ["postMessage"],     // 只有 postMessage，无 login/acceptInvite 等方法
  "invocationCount": 523,
  "lastInvocationCommand": 55,     // ← cmd 55 确实送达
  "lastInvocationAt": "2026-09-10T16:44:41.685Z",   // 来电时间 16:43:56
  "messageCount": 16605,
  "forwardedCount": 16600,
  "lastForwardedCommand": 20050,   // ← 之后再无任何回传
  "forwardError": null,
  "error": null
}
```

**关键点：收到 `cmd 55` 之后，AVSDK 一条消息都没有回传**（计数器冻结）。

## 核心现象：AVSDK 每次登录都被 `20050` 顶掉

观察 AV Host 的计数器可以看到一个稳定的模式：

```
messages=16594  invocations=521  lastFwdCmd=20050
messages=16598  invocations=521  lastFwdCmd=20050   ← 冻结
```

`16594 / 521 ≈ 31.9`，也就是**每次登录 AVSDK 回大约 32 条消息，最后一条固定是 `20050`**，
随后插件的重登逻辑再次登录，如此往复，直到 AVSDK 彻底停止回传。

因此 `networkOutputCount` 始终为 **0** —— AVSDK 从未产生过 `20001`（网络数据），
也就是从未真正建立过媒体会话。

`host.cjs` 的 `forwardError` 为 `null`，`rendererState.error` 为 `null`，
AV Host 进程本身健康（`/healthz` 200，`pluginFound: true`）。

### AVSDK 自身的日志

`[TRAE]` 前缀的 AVSDK 内部日志显示音频子系统**工作正常**，并且正确识别到了桥的虚拟设备：

```
[TRAE] [INFO] [pulse_audio_wrapper.cc:782] PaSinkInfoCallbackHandler, render display name: MaiBot_QQ_Speaker
[TRAE] [INFO] [audio_device_linux.cc:89]  UpdateAudioDeviceHardwaresInfoWithNotification, output: 0, MaiBot_QQ_Speaker, 0, is_default:1
[TRAE] [INFO] [pulse_audio_wrapper.cc:808] PaSourceInfoCallbackHandler, capture display name: MaiBot_QQ_Microphone
[TRAE] [INFO] [audio_device_linux.cc:68]  UpdateAudioDeviceHardwaresInfoWithNotification, default input: MaiBot_QQ_Microphone, -1
```

除此之外，日志中**没有任何与会话/网络/登录相关的 AVSDK 输出**。

## 我这边已经做的两处修复（可能对上游也有价值）

### 1. `accountPath` 解析（建议上游考虑合并）

NapCat 4.18.x **已经不再提供 `session.getAccountPath()`**（我在 `napcat.mjs` 里
grep `getAccountPath` 命中 0 次），因此插件的回退分支会用 `ctx.core.dataPath`：

```js
const accountPath = String(
  session?.getAccountPath?.(Number.parseInt(selfUin, 10)) || ctx.core?.dataPath || "",
);
```

实测拿到的 `ctx.core.dataPath` 是 **QQ 数据根目录**，而不是账号自己的目录：

```
修复前: accountPath=/app/.config/QQ
修复后: accountPath=/app/.config/QQ/nt_qq_d66d7f85239b08c71a88cd18bce50fb9
```

修正后，重登次数从**无上限**收敛到 521 次（不再无限增长），但 `20050` 依然存在。
修改方式是在 `dataPath` 下查找 `nt_qq_*` 目录：

```js
function resolveAccountPath(dataPath, uin) {
  try {
    if (!dataPath) return "";
    const dirs = fs.readdirSync(dataPath, { withFileTypes: true })
      .filter((e) => e.isDirectory() && e.name.startsWith("nt_qq_"))
      .map((e) => path.join(dataPath, e.name));
    return dirs[0] || "";
  } catch { return ""; }
}
```

### 2. 启动顺序

按 `run-napcat.sh` 的语义，改为**先启动 AV Host 并等待 `/healthz` 就绪，再启动主 QQ**
（原来的容器集成是不等待就起主 QQ）。改完 `20050` 现象不变，但顺序现在与上游一致。

## 已经排除的因素

为了节省你的时间，下面这些我都实测排除了：

| 假设 | 验证方式 | 结果 |
|---|---|---|
| AVSDK 缺少动态库 | 逐个补齐 `libpulse-mainloop-glib.so.0`、`libEGL.so.1`、`libOpenGL.so.0`、`libGLESv2`、`libGLX`、`libGLdispatch` | 补全后 Pepper 模块加载成功（不再报 `Failed to load Pepper module`），但 `20050` 不变 |
| 容器 `/dev/shm` 过小（Docker 默认 64 MB） | 改为 `--shm-size=1g` | 不变 |
| 容器沙箱限制 | `--privileged --security-opt seccomp=unconfined` 重建 | **完全不变**，可直接排除 |
| 容器网络 | 改为 `--network host`（回环服务才能被宿主机访问） | 桥/AV Host 均可达，`20050` 不变 |
| NapCat 版本漂移 | 核对 docker tag 时间线：桥写于 2026-08-03，当时 NapCat 为 4.18.12–4.18.14；我们是 4.18.19 | 仅差几个小版本，认为不是变量 |
| QQ 版本漂移 | 我们的 QQ 构建于 2026-07-03，桥提交于 2026-08-03 | 相差不到一个月，认为不是变量 |
| 音频设备不可用 | AVSDK 成功枚举 `MaiBot_QQ_Speaker` / `MaiBot_QQ_Microphone` | 正常 |
| 桥自身健康 | `doctor.sh` → `all bridge checks passed` | 正常 |
| 主 QQ 身份读取 | 插件日志：`uid=u_UpnQKnstesxt81AAjWLzAA uin=3115024431` | 正常 |

## 我的问题

1. **你验证过的 QQ / NapCat 具体版本是什么？** README 只写了「NapCat ≥ 4.14.0」，
   没有写明验证过的 QQ 构建。如果方便，能贴一下 `qqnt.json` 里的 `version` 和
   NapCat 版本吗？我想先对齐到你验证过的组合。

2. **`20050` 在你这边的语义是什么？** 插件把它当作「掉线需重登」，但我们的场景里它
   **每次登录都会出现**，于是形成 521 次的重登循环。在你的部署里它是一次性的吗？

3. **AV Host 的 QQ 是否需要自己先登录过一次？** `run-av-host.sh` 用的是独立的
   `--user-data-dir=$runtime_dir/av-host-profile`，这个 profile 在全新部署时并没有 QQ
   登录态，登录完全依赖 `cmd 1` 注入。你的部署里 `av-host-profile` 是保持一个曾经
   扫码登录过的状态，还是全新未登录也能工作？

4. **是否有可能 `cmd 55` 的 payload 格式与 QQ 9.9.22 不再匹配？** 即内核数据转发
   需要额外的封装（例如某个 `SetPenetrateActionFromAVSDK` 变体）。如果你手上有当时
   抓到的 `cmd 55` 成功案例日志，对比一下就能确认。

如果你认为这是已知限制或需要特定 QQ 版本，也请直接说明——我会按你的建议调整环境再试。

## 补充：复现与诊断命令

```bash
# 桥状态
TOKEN=$(cat ~/.local/share/maibot-qq-voice-call/runtime/control.token)
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:6110/v1/status | python3 -m json.tool

# AV Host 计数器（判断 AVSDK 是否在回传）
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:6111/v1/status | python3 -m json.tool
```

来电后重点看三个字段：AV Host 的 `lastInvocationCommand` 是否变成 `55`、
`lastForwardedCommand` 是否在 `55` 之后有新值、桥的 `networkOutputCount` 是否 > 0。
