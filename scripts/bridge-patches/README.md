# 桥补丁（NapCat AV 桥的服务端改造）

QQ 语音通话的媒体链路跑在一套独立的 NapCat AV 桥上（第二个 QQ + AVSDK PPAPI 插件）。
上游桥有些行为在本部署里必须改掉，而这些改动**在容器镜像层之外**：重建容器、重装桥
都会丢。所以容器入口包装 `bridge-entry.sh` 每次启动都会把这里的脚本**幂等地重打一遍**。

线上真身放在部署机的 `/home/ubuntu/napcat-qq-call/`（容器内是 `/app/qq-call/`），本目录
只是版本化副本；改完要同步过去并在容器里执行一次（或重启容器）。

```bash
# 同步（把 <host> 换成部署机）
scp scripts/bridge-patches/*.py <host>:/tmp/ && \
  ssh <host> 'sudo cp /tmp/*.py /home/ubuntu/napcat-qq-call/ && sudo docker exec napcat python3 /app/qq-call/patch-plugin-hangup.py'
```

## 补丁清单

| 文件 | 作用 |
|---|---|
| `patch-plugin-account-path.py` | NapCat 4.18 删了 `session.getAccountPath()`，插件的回退值拿到的是 QQ 数据**根目录**而不是账号目录（`nt_qq_<hash>`），需要自行解析 |
| `patch-20050-backoff.py` | 加入 AVSDK 命令直方图（暴露在 `/v1/status` 的 `avHost.commandHistogram`），便于判断协议是否又变了 |
| `patch-ignore-20050.py` | **根因修复**：`20050`/`120043` 是普通通知，上游误判为掉线并每 100ms 重登，亲手把健康会话踢掉 |
| `patch-plugin-login-refresh.py` | **登录自愈**：AV Host 是独立 QQ 进程，登录态由插件投递；上游只在启动时投一次，进程重启后就再也没人投。这里补一个 60 秒的空闲重投 |
| `patch-plugin-caller-allowlist.py` | **接听授权**：接听前读 `runtime/allowed-callers.json`（`enabled != true` 时保持上游"谁打进来都接"），并把来电者写进状态 |
| `patch-plugin-hangup.py` | **主动挂断**：AV Host 的 cmd 白名单加上 `10`（`Close`），插件新增 `POST /v1/calls/hangup`。见 `docs/qq-call.md` 的「挂断参数实验」 |
| `patch-capture-rare-cmds.py` | 记录少见的 AVSDK 命令（排查协议变化用） |
| `patch-napcat-whitelist.py` | 把桥插件名加进 NapCat 的官方插件白名单 |

前六个都挂在 `bridge-entry.sh` 的启动循环里；后两个是排查用的，按需单独执行。

**写法约定**（都是踩过坑换来的，2026-09-12）：

1. **幂等判据要用独立的版本标记**，不要拿"整段插入文本"比对——内容改一版，整段比对
   就失配，会把同一段代码插第二遍。`index.mjs` 是 ESM，**重复函数声明是语法错误**，
   插件会直接加载失败、整个桥下线（当天真的这么下线过两次）。
2. **每个插入点各自一个标记**：三点共用一个标记时，第一个插入成功后，后面的插入点会
   被误判成"已是最新"而跳过（`dial` 路由就这么被跳过过一次）。
3. **写盘前自检**：把关键片段数一遍，不等于期望值就整份放弃、不落盘
   （见 `patch-plugin-dial.py` 的 `EXPECTED_ONCE` / `NEVER_TWICE`）。
4. **发现旧标记就拒绝**，提示"从 `index.mjs.upstream` 重建 + 跑全套补丁"，不要"猜着补"。
   重建路径可靠：还原 `.upstream` → 重启容器，`bridge-entry.sh` 会按顺序重打全套。
5. **能归一化就归一化**：例如 `patch-plugin-hangup.py` 用正则把白名单整行改写成目标值
   `{1, 4, 5, 10, 20, 55}`，上游原版与实验期间的 `{1,5,8,9,10,11,55}` /
   `{1,5,10,11,55}` 都能收敛到同一状态。
6. **别拿 AV Host 的日志当实时证据**：它是块缓冲的，最新几行可能几十秒后才落盘
   （当天因此误判过两次"命令没到"）。判断命令是否送达请看 AV Host 的
   `invocationCount` / `lastInvocationCommand`（`/v1/status`，端口 6111）。
7. **改完补丁一定跑一次自检**：补丁在仓库和线上各有一份，只在服务器上改、或往仓库
   加了新补丁却忘了加进 `bridge-entry.sh` 的 patcher 列表，都会造成"功能在仓库里、
   线上根本没打"的静默漂移（2026-09-12 就真漏过一次：列表少了最后四个 patcher）。
   ```bash
   scripts/bridge-patches/verify-deployed.sh    # 仓库 vs 线上逐字节比对，exit 0 才算齐
   ```
   往 `bridge-entry.sh` 加新补丁时，记得连同服务器上那份一起更新，再重启容器重打全套。

## 其它文件

- `bridge-entry.sh`：容器入口包装的参考副本（真正生效的是部署机上的同名文件，它负责拉起
  PulseAudio、QQ、AV Host，并调用上面的补丁）。`verify-deployed.sh` 会比对这两份。
- `verify-deployed.sh`：核对仓库与线上补丁是否逐字节一致，并检查 patcher 列表里的每个
  文件都真实存在。防止"仓库有、线上没跑"的漂移。
- `try_close_variant.py`：诊断脚本。等来电接通后，用指定变体（A=uid 留空、B=roomId 取
  `invite[3]`、C=uid 传机器人自己、D=`clearRoom`）调挂断接口，8 秒内没结束就自动用已知可用
  的参数兜底，不会把对方悬在静音通话里。定位"挂断到底要什么参数"时用它。
- `try_startcall.py`：外呼定标脚本（已定标完成，留作协议万一又变了时的复测工具）。
  自动重启 AV Host → 探活 → 依次试多组候选参数 → 守卫式读回复；手工发 JSON 有几率
  把插件打成 segfault，所以每次尝试之间必须重启 AV Host。
- `patch-*` / `try_*`：都是**参考副本**，真正生效的是容器里的 `/app/qq-call/`。
