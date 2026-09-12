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

**写法约定**：补丁脚本必须**幂等**，而且要能收敛历史状态——比如 `patch-plugin-hangup.py`
是"归一化"写法：白名单那一行用正则匹配后改写成目标值 `{1, 5, 10, 55}`，所以上游原版
`{1,5,55}`、实验期间留下的 `{1,5,8,9,10,11,55}` 都能收敛到同一状态。不要写成"只认某一
版内容"的字符串替换，否则容器重建后补丁会静默失败。

## 其它文件

- `bridge-entry.sh`：容器入口包装的参考副本（真正生效的是部署机上的同名文件，它负责拉起
  PulseAudio、QQ、AV Host，并调用上面的补丁）。
- `try_close_variant.py`：诊断脚本。等来电接通后，用指定变体（A=uid 留空、B=roomId 取
  `invite[3]`、C=uid 传机器人自己、D=`clearRoom`）调挂断接口，8 秒内没结束就自动用已知可用
  的参数兜底，不会把对方悬在静音通话里。定位"挂断到底要什么参数"时用它。
