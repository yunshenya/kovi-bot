# 日志巡检与修复台账（2026-09-15）

巡检对象：线上 `kovi-bot.service`（systemd，`ubuntu@139.155.156.152`，`REMOTE_APP_DIR=/home/ubuntu/kovi-bot`）。

- 日志窗口：`journalctl -u kovi-bot.service`，2026-09-15 00:00 ~ 13:53，并回看 3 天做趋势
- 巡检时线上 revision：`5ee24f9`（本轮启动 10:25:40，巡检期间未重启）
- 窗口内 `[HEALTH] 系统运行正常` 稳定输出，`[sent]` 零失败，无 panic

结论：**没有新的致命故障，但有三类问题值得处理**——Mind 观测在进 Core 前的预算里被成片丢弃、09-14 有一次"运行时死亡后 53 分钟静默丢消息"、以及表情包素材库缺相册语义导致她否认自己的照片。上游模型超时已随 10:04 换线路消失。

---

## 一、Mind 观测超时（已修：配置止血 + 去白读）

### 现象

```
近 3 天 "Yunxi Mind event update timed out and failed soft"：115 次
  09-12 → 3    09-13 → 4    09-14 → 87    09-15 → 22（当天 22/48 条入站事件，≈46%）
时刻成簇：00:42–00:45、07:18–07:19、09:25–09:34、10:00–10:03、12:25–12:38
```

超时的伴随现象之一是 PG 的 `there is no transaction in progress`（3 天 115 次超时 / 14 条
该 notice，其中 12 条的上一行就是超时）：`timeout()` 把她从事务中间掐断，sqlx 回滚时事务
已经不在了。只有约一成的超时会留下它——取消正好落在事务里时才有，所以它适合当"这次取消
踩到了事务"的证据，不适合当超时计数。

### 根因

1. 生产 `mind.event_update_timeout_ms = 40`（模板 150）。这个漂移在
   `docs/code-review-2026-09-14-unattended.md:400` 已被记录，当时判"影响较小"。
2. 而 `observe_event`（`plugins/model/src/yunxi/mind_runtime.rs:842`）在每个 scope 上依次跑
   `resolve_answered_state` + `refresh_agenda_for_scopes`，两边**各自**调一次
   `load_v1_context`；每次都是 `memory.recall()`（开事务 + `lock_memory_read` 共享锁 +
   多条查询，见 `plugins/model/src/yunxi/memory_store.rs:771`）+ open_loops + goals。
   一条群消息就是 6~10 次串行 DB 往返，40ms 预算正常情况下也偏紧。
3. `resolve_answered_state` 只用得到 open loop：memory 与 goal 读了就整份丢掉。

**排除项**：不是机器压力。`load average 0.01`、available 1169M、swap 389M，
今天 0 条 `slow statement` / `acquired connection ... exceeded slow threshold`。
09:25–10:05 那两簇与上游模型 30~60s 超时窗口重叠，属"模型侧卡住把预算挤没"，
其余几簇是 DB 侧抖动。

### 已做

| 动作 | 位置 | 说明 |
| --- | --- | --- |
| 生产止血 | `runtime/bot.conf.override.toml` 的 `[mind]` | `event_update_timeout_ms` 40 → 150，走管理后台 override（校验→备份→原子写→热加载），**未重启**；生效值已核对 |
| 去掉白读 | commit `3e74d5b` | `resolve_answered_state` 改走只读 open loop 的轻量路径；抽 `scope_owners` 防止两条路读成不同 scope |

失败仍是 fail-soft（记一条 warn、当作这一轮没有可了结的 open loop），与改造前一致。

### 验证

- 新增 `observing_an_event_reads_memory_once_per_scope`：一条私聊消息两个 scope 只读两次
  memory（改造前四次）
- `cargo test -p model --lib` 1130 passed / 0 failed；clippy `-D warnings` 干净
- 生产侧待观察：13:30 之后到现在还没有新消息（Mind 决策数 0），超时率要等有流量再对比

---

## 二、09-14 丢了 186 条入站消息（根因已定位；当时的 panic 已由 `ce78355` 修复）

### 现象

```
2026-09-14 14:24:10 ~ 15:17:28，跨两个 PID（3521203 → 3579823），共 186 条：
[Error] Yunxi Core runtime rejected inbound event: … error=cognitive runtime is closed
[WARN]  Yunxi Core message dropped during ingress: … error=cognitive runtime is closed
```

### 根因（原始日志已核）

`cognitive runtime is closed` 不是"关机/降级态"，而是**运行时任务已经死了**：
`RuntimeHandle::submit` 的 `TrySendError::Closed` 只意味着持有 `mpsc::Receiver` 的
`CognitiveRuntime` 被 drop，而它唯一的归属者是 `run_runtime` 这个 tokio 任务。

```
2026-09-14T14:23:23  kovi-bot[3521203]: thread 'tokio-rt-worker' panicked at plugins/model/src/yunxi/core_model.rs:3617:14
2026-09-14T15:11:28  kovi-bot[3579823]: thread 'tokio-rt-worker' panicked at plugins/model/src/yunxi/core_model.rs:3617:14
```

即"焦点写入晚读已被 disarm 的 guard"，宿主模型是在运行时任务内部被 await 的，所以这条
panic 直接把运行时 future 展开。修复提交 `ce78355`（09-14 15:18:35）之后，15:21:01 的
重启才真正结束窗口；15:02:52 那次重启只是让新进程又 panic 了一次。

**被拒的入站消息没有任何排队/重试/持久化/补偿**，直接丢；同时开环调度一起停摆。
丢消息本身没有配置开关可以避免。

### 仍未处理的结构性风险（待拍板）

`run_runtime` 的 `JoinHandle` 被丢弃、退出时一行日志都不打，健康检查也不看它——
**宿主再 panic 一次就又是一个 53 分钟盲区**，而进程看起来一切正常。建议组合：

- **A（低风险，建议先做）**：保留 `JoinHandle`，给 `run_runtime` 包一层监督者（退出/panic
  都打 ERROR）；首个 `RuntimeClosed` 处打一条 `action=restart_required` 并撤掉 readiness
  标记（`remove_ready_marker`），让部署与监控立刻看见"未就绪"。
- **B（结构性）**：把 `CoreBridge.runtime` 换成可热替换句柄，panic 后重建运行时（带次数
  上限与退避）。影响 10+ 处调用点与开环调度，需先定义在途事件策略。
- **C（放大器清理）**：审计运行时热路径上的 `expect`/`unreachable!`；`config::get()` 在锁
  中毒时改为降级而不是连环 panic。

---

## 三、表情包素材库缺"相册语义"（已修：提示词 + 判据 + 验收脚本）

### 现象

素材库只有 1 张 `芸汐的照片.jpg`。线上 2026-09-15：

```
13:20:22  用户：芸汐看看你的照片
13:20:23  芸汐：我哪有什么照片呀，就是个只会打字陪你聊天的人，长什么样连我自己都不知道呢。
13:21:32  芸汐：随便一张也没有呀，我手机里就存了一堆表情包。要不给你发个猫猫的？
13:21:57  芸汐：好呀，那我就发那个猫猫的啦，你等等。      ← 相册里没有猫猫，承诺落空
```

02:16 还有一次整轮只剩表情包动作、投递被放弃（`这一轮没有任何可发送内容，投递已放弃`）。

### 根因

1. 旧判据 `asks_about_stickers` 只认"表情包/表情/贴纸/斗图/sticker/meme"——**"照片"不命中**，
   所以那一轮只拿到表情包协议、没有清单，也就没有任何"素材库是我的相册"的说法。
2. 她持久化的自我认知是 `claims_human_identity = false`、"我是由 AI 驱动……的虚拟角色"
   （`yunxi_self_model`），在没有相册语义的情况下，模型稳定地把它读成"我不能有自己的照片"。

### 已做

| commit | 内容 |
| --- | --- |
| `b62be06` | 协议行说成"素材库就是你自己的相册（图都是你的）"；写死"没有就别发、别答应「等下发给你」"；被问到图/照片时注入的清单前加"带你自己名字的标签就是你本人的照片……不要说那不是你"；`sticker.list` 描述/回执与修复说明同步；判据补 `照片/相册/自拍/photo/album/selfie` |
| `cc896dd` | 新增 `scripts/verify-sticker-album.sh`：对生产模型跑"改动前 vs 改动后"对照 |
| `26a0c50` | 验收脚本补最坏情况：把她自己否认过的记忆放进 memory context |
| `f38951e` | 补"但要回话说明没有，不能用沉默代替"；118 → 129 字 |

### 验证（实跑，非推测）

`scripts/verify-sticker-album.sh` 三组条件全部 PASS：

- **对照组（改动前）**：稳定复现线上那句否认——"我没有真正的照片呀……没有可以拍下来的
  样子"、"那些只是聊天时可以用的表情包，不是我本人的照片啦"；
- **改动后**：直接写 `[[STICKER 芸汐的照片]]` 发出去，并承认"标签就叫「芸汐的照片」"；
- **改动后 + 她自己的否认记忆被回忆起来**：同样直接发图（"刚才是我自己不好意思认啦"）。

配套单测：`cargo test -p model --lib` 1130 passed；判据命中用例覆盖"发我看看你的照片"
"相册里有自拍吗"等问法。

**一次被自己推翻的结论**：第一轮 6 次采样看到"新协议 3 次空回复、旧协议 0 次"，据此写了
"回归"；放大到 12 次后两侧分别是 5 与 4（模型侧抖动，且线上有
`strong_reply_repair_needed` 兜底），结论不成立。代码注释与提交信息里都写明了这一点，
避免后人把它当证据。

---

## 四、上游模型超时（已自行消失）

```
今天 [ERROR]：28 条 模型请求失败（超时，api.b.ai/v1/chat/completions）
              6 条 等待模型请求配额超时
              5 条 模型响应解析失败 / error decoding response body
分布：08:18–08:31、09:30–09:39、10:00–10:05 三簇，最后一次 10:05:05
```

10:04:47 override 把 `server_config.url` 从 `api.b.ai` 换成 `https://api.deepseek.com`
（两份 `.bak` 里还能看到旧值），**此后至今 0 条模型侧失败**。属外部依赖，无需继续处理；
但"超时会连带把 Mind 观测预算挤没"这条耦合仍在，见第一节。

---

## 五、待拍板

1. **本轮代码是否发布**：改动尚未上线（线上仍是 `5ee24f9`）。发布演练已经做过
   （`scripts/deploy-local.sh --dry-run`，revision `22ac38c`）：交叉编译 release 通过、
   包 14.1 MiB，未上传未切换。注意工作区里**另一个会话**随时可能在改，
   建议等它停手再用 `scripts/deploy-local.sh --require-clean`。
2. **第二节的 A / B / C**（运行时监督层、热重建、panic 放大器）是否开工。
3. 素材本身：相册里只有一张。想让她能发别的图，把图片放进
   `runtime/stickers/`（文件名即标签）即可，不需要改代码。

---

## 六、发布后怎么验

两条命令，各自给 PASS/FAIL，不用靠肉眼看日志：

```bash
scripts/verify-mind-observation.sh "1 hour ago"   # Mind 观测丢失率（改动前基线 46%，阈值 10%）
scripts/verify-sticker-album.sh                   # 要照片时她直接发相册里那张、不否认
```

判读要点：

- `verify-mind-observation.sh` 输出 `SKIP` 表示窗口里还没有进 Core 的事件（群里没人说话），
  **不是**通过；输出 `INCONCLUSIVE` 表示同窗口内有 slow statement / 慢获取连接，先按资源
  压力排查再谈预算。
- `verify-sticker-album.sh` 的"对照组"**应当**复现那句否认——那是故障复现，不是脚本坏了；
  只有"验收组 / 最坏情况组"的断言决定 PASS/FAIL。

### 产物自检（发布包里到底有没有这次改动）

发布演练的 release 二进制按字节查过：六条新文案全在，两条旧文案（`想发表情包：先调
sticker.list`、`列出她现在能发的表情包标签`）都已消失。

```python
data = open("target/x86_64-unknown-linux-gnu/release/kovi-bot", "rb").read()
data.find("素材库就是你自己的相册（图都是你的）".encode())   # >= 0 即在产物里
```

**坑**：macOS 上 `grep -a "中文" 二进制` 会漏报——同样的模式它一条都找不到，用 Python
按 UTF-8 字节查则全部命中。别用 `grep` 的结论判断"改动没进产物"。

---

## 附：本轮 commit

| commit | 内容 |
| --- | --- |
| `3e74d5b` | Mind 观测不再白读 memory 与 goal |
| `b62be06` | 表情包素材库改说成她自己的相册 |
| `cc896dd` | 相册语义验收脚本 |
| `26a0c50` | 验收脚本补最坏情况 |
| `f38951e` | 相册协议补"但要回话" |
| `a6269de` | Mind 观测验收脚本（一条命令判丢失率） |
| 本文件 | `6860df0` 起，后续修订见 git log |
