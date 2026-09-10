# `20050` 被误判为「掉线需重登」，导致健康会话被反复踢掉、来电永远停在 ringing

## 结论先说

**`20050` / `120043` 不是会话结束通知，而是与登录成败无关的周期性通知。**

上游在收到它们后用固定 100ms 立刻重登，于是**亲手把自己刚刚建立的健康会话反复踢掉**，
形成 521 次重登循环。会话永远无法稳定，`cmd 55`（内核数据转发）因此永远得不到处理，
`20006`（来电回调）从未出现，来电一直停在 `ringing`，对方听到的是无人接听。

改成「只计数、不重登」之后，**首次真实来电即全链路打通**（详见下方实测记录）。

## 环境

| 项 | 值 |
|---|---|
| 桥 | 0.3.4（`22f30c021cd3170f75af9dff66cc959a07ebda4b`） |
| NapCat | 4.18.19（`mlikiowa/napcat-docker`，2026-08-14 构建） |
| Linux QQ | 9.9.22-40990（Linux 3.2.20-40990，构建于 2026-07-03） |
| 运行方式 | Docker，`--network host --shm-size=1g`，Ubuntu 24.04 x86_64 / 4 核 3.6 GB |

`doctor.sh` → `all bridge checks passed`。

## 根因证据

给插件加上命令直方图后，AVSDK 回传的命令第一次变得可见。一次完整通话结束后：

```
cmd 20040: 69     ← 通话中
cmd 20050: 49     ← 被误判的通知
cmd 20001:  2     ← 网络媒体数据
cmd 20023:  2
cmd 20043:  2
cmd 20046:  2
cmd     1:  1     ← 登录
cmd     5:  1     ← 接听
cmd   103:  1
cmd 20004:  1     ← 进房
cmd 20006:  1     ← 来电回调（修复前从未出现）
cmd 20037:  1
```

修复前（同一环境，只是没改 20050 的处理）：

```
直方图: { '1': 2, '103': 2, '20050': 72, '20061': 2 }
```

**有效信息只有 `1`/`103`/`20061` 各 2 条，其余 72 条全是 `20050`。**
`16594 / 521 ≈ 31.9`——每次登录 AVSDK 回约 32 条消息、末条恒为 `20050`，插件的重登逻辑
再登录一次，如此往复直到 AVSDK 停止回传。

### 这几条命令的真实内容（通过捕获 `value` 得到）

```
cmd 1     -> [0, ""]        ← 登录返回，0 = 成功
cmd 103   -> [1, ""]        ← 状态通知
cmd 20061 -> [[1,2], ["MaiBot_QQ_Speaker", "MaiBot_QQ_Microphone_Feed"],
              ["0","1"], [1], ["MaiBot_QQ_Microphone"], ["0"], 0, 0, 1, 1]
                            ← 音频设备变更上报，内容里就是桥的虚拟声卡
cmd 20050 -> 周期性通知，与登录成败无关
```

**关键点：`cmd 1` 返回 `0`，登录一直是成功的。** 之前看到的所有异常现象，都是插件
对 `20050` 的过度反应造成的。

## 建议的修改

`bridge/napcat-plugin/index.mjs` 的 `handleAVSDKOutput` 中：

```js
// 现在
if (
  (command === 20050 || command === 120043) &&
  state.avHost.loginPosted &&
  pluginContext
) {
  state.avHost.loginPosted = false;
  scheduleAVHostLogin(pluginContext, 100);
}

// 建议改为：只计数，不重登
if (command === 20050 || command === 120043) {
  state.avHost.sessionEndCount = (state.avHost.sessionEndCount || 0) + 1;
}
```

如果 `20050` 在别的 QQ 构建上确实是「掉线」，建议至少改成**退避**（例如 1s 起、
指数增长、上限 30s）而不是固定 100ms——100ms 的间隔会让刚建立的会话没有机会稳定。

## 修复后的实测结果

```
桥侧
  来电回调已见 : true
  自动接听     : 17:50:17.863Z → 发出 17:50:17.865Z → accept 输出 17:50:17.867Z
  进房 20004   : 17:50:18.064Z          （接通耗时约 200ms）
  网络输出     : 2
  通话阶段     : connected
  来电者       : 小猫ᓚᘏᗢ (3052405886)，身份解析正常

机器人侧（本地 ASR + 本地 TTS，非 DashScope）
  01:50:18  QQ 语音通话已接通
  01:50:28  QQ 通话已收到对端语音，采集链路正常
  01:50:30  识别: 你好你好。
  01:50:30  回复: 嗯，你好呀，听起来你今天心情不错？
  01:50:42  识别: 嗯，OO采了采了。
  01:50:43  回复: 采了了？是说你那边忙完了，还是刚才信号有点飘呀？
  01:50:58  识别: 操，没事没事。
  01:50:58  回复: 好，那我不追问了。你现在是想随便聊聊，还是有什么事想和我说？
  01:51:09  QQ 语音通话结束（对方挂断，时长 51 秒）
```

三轮对话全部正常，句尾到开口约 2–3 秒，挂断检测正常。

## 另一处修复：`accountPath`

NapCat 4.18.x **已不再提供 `session.getAccountPath()`**（`napcat.mjs` 里 grep 命中 0 次），
插件因此走到回退分支：

```js
const accountPath = String(
  session?.getAccountPath?.(Number.parseInt(selfUin, 10)) || ctx.core?.dataPath || "",
);
```

实测 `ctx.core.dataPath` 是 QQ 数据**根目录**，而不是账号自己的目录：

```
修复前: /app/.config/QQ
修复后: /app/.config/QQ/nt_qq_d66d7f85239b08c71a88cd18bce50fb9
```

建议在 `dataPath` 下解析 `nt_qq_*` 目录：

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

## 附：已排除的因素

为了节省时间，下面这些我都实测排除了（**其中前两项虽是必需项，但不是根因**）：

| 假设 | 验证方式 | 结论 |
|---|---|---|
| AVSDK 缺动态库 | 补齐 `libpulse-mainloop-glib.so.0`、`libEGL.so.1`、`libOpenGL.so.0`、`libGLESv2`、`libGLX`、`libGLdispatch` | 缺了 AVSDK 根本加载不了（`Failed to load Pepper module`），补全是必需项 |
| 容器 `/dev/shm` 仅 64 MB | `--shm-size=1g` | 建议保留，但非根因 |
| 容器沙箱 | `--privileged --security-opt seccomp=unconfined` | **完全无变化** |
| 容器网络 | `--network host` | 必需项（桥只监听回环，`-p` 发布访问不到） |
| NapCat / QQ 版本 | 桥写于 2026-08-03，当时 NapCat 为 4.18.12–4.18.14 | 与我们的 4.18.19 仅差几个小版本 |

## 附：本次用到的诊断手段

`host.cjs` 的 `rendererState` 里有 `messageCount` / `forwardedCount` / `lastForwardedCommand`
三个计数器，它们是判断「AVSDK 有没有在回传」最快的入口——建议在 README 的排错一节里
提一下，比只看桥的 `networkOutputCount` 直观得多：

```bash
TOKEN=$(cat "$INSTALL_DIR/runtime/control.token")
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:6111/v1/status | python3 -m json.tool
```

配合插件侧加一个 `commandHistogram`，协议层面的变化一眼就能看出来。
