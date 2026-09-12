# 日志巡检与已知问题（2026-09-12）

巡检对象：线上 `kovi-bot.service`（systemd，`ubuntu@139.155.156.152`，`REMOTE_APP_DIR=/home/ubuntu/kovi-bot`）。

- 日志窗口：`journalctl -u kovi-bot.service --since '7 days ago'` —— 72,350 行，2026-09-06 ~ 2026-09-12
- 结构化 `[WARN]`/`[ERROR]`：157 条（WARN 72 / ERROR 85）
- 巡检时线上 revision：`4e6636d4e7c71be8461c2a6430be794543a57a9e`（本地 HEAD `5bbae7b` 仅多一个 docs commit）
- 服务运行时长：4h52m，`[HEALTH] 系统运行正常` 每 5 分钟稳定输出

结论：**没有单点致命故障，但存在一组相互耦合的资源与协议层问题**。最值得先修的是视觉能力整体失效（占全部 WARN/ERROR 的 20%）、embed 服务周期性 OOM，以及一个由重启风暴放大的对话并发锁竞争。

---

## 一、问题清单（按严重度）

### P0-1 embed 服务周期性 OOM，回忆链路降级

`yunxi-embed.service` cgroup `MemoryMax=1572864`（1.5G），7 天内被 OOM kill 至少 6 次：

```
Sep 12 15:39:06 systemd[1]: yunxi-embed.service: Failed with result 'oom-kill'.
Sep 12 15:49:40 kernel: oom-kill:constraint=CONSTRAINT_MEMCG ... cpuset=yunxi-embed.service
Sep 12 15:49:40 kernel: Memory cgroup out of memory: Killed process 2083156 (python)
                     total-vm:4633008kB, anon-rss:1035764kB
```

当前 PID 2112665 常驻 859M，峰值已顶到 1.4G。被杀的时间点与下列失败严格对应：

```
15:14:10 [ERROR] 记忆向量回填失败（连续 1 次）: 嵌入服务请求失败: error sending request for url (http://127.0.0.1:6112/v1/embed)
15:24:39 ... 15:25:38 ... 15:39:06 ... 15:49:41 （共 5 次）
```

原因：单进程同时常驻 embed 模型（BGE-small-zh ONNX）+ reranker 模型（bge-reranker-base ONNX）+ tokenizer，内存本就贴着上限；进程被换出后请求超过客户端超时窗口，表现为 “error sending request” 而不是明确的服务不可用。

影响：记忆向量回填中断 → 语义召回降级；每次 OOM 还会连带拖慢整机（见 P0-3）。

### P0-2 视觉得分：整体不可用，占全部 WARN/ERROR 的 20%

32 条 / 157 条。拆开看是四种不同病因，混在一条 “视觉模型不可用” 日志里：

| 次数 | 日志 | 病因 |
| --- | --- | --- |
| 13 | `[WARN] Intrinsic 视觉模型不可用（supports_vision=false，health=Degraded）` | 本地模型能力/健康度不满足 |
| 2 | `[WARN] Intrinsic 视觉推理失败: image has N pixels, above maximum 4000000` | 输入未预缩放（出现过 83997225 像素 = 8400 万像素） |
| 1 | `[WARN] Intrinsic 视觉模型仅支持单图分析，收到 3 张` | 多图未做拆分或降级 |
| 16 | `[ERROR] 截图分析失败: 本地 Intrinsic 视觉模型不可用（需要单一图片且模型支持 vision）` | 上面三种情况的统一对外错误 |

关键点：**图片已经在进链路，但一次都没成功过**——上游没有做尺寸预缩放和多图拆分，所以每次都在同一个地方失败，只留一条笼统 ERROR。

### P0-3 整机内存紧张 → DB 连接池饥饿

机器 3.7G RAM / 1.9G swap / 4 vCPU，三个常驻服务：

| 进程 | RSS |
| --- | --- |
| yunxi-embed (python) | 873M |
| kovi-bot | 692M |
| yunxi-speech (python) | 516M |

`free -m`：available 仅 880M，**swap 已用 1048M**，其中 python 进程被换出 222M + 55M。09-12 15:12 起内存压力把 PG 拖垮，持续数分钟：

```
[Warn] acquired connection, but time to acquire exceeded slow threshold aquired_after_secs=113.93 (threshold 2.0)
[Warn] slow statement: SELECT MIN(candidate_at) FROM ( … kovi_bot_agent_runs … ) elapsed=41.90s
[Warn] slow statement: WITH stale AS ( … yunxi_open_loops … FOR UPDATE SKIP LOCKED ) elapsed=31.68s
[ERROR] Agent Run 调度失败: pool timed out while waiting for an open connection
[ERROR] 提醒调度失败: pool timed out while waiting for an open connection
[ERROR] 跨群问答任务调度失败: 开启跨群问答状态收敛事务
```

**这不是缺索引**。逐表核对过：`kovi_bot_agent_runs`、`kovi_bot_reminders`、`yunxi_open_loops` 的调度索引齐全（含 partial index），全库最大表 `kovi_bot_memory_embeddings` 只有 3496 kB / 1159 行。慢的是等连接和等内存，不是扫表。

真正卡住吞吐的是应用侧连接池配置 `plugins/model/src/memory/mod.rs:497`：

```rust
let pool = PgPoolOptions::new()
    .max_connections(5)          // 全进程共 5 条
    .connect(&database_url)
```

PG 侧 `max_connections=100`，`pg_stat_activity` 实际只有 4 条在用——**瓶颈完全在应用侧的那 5 条**，而 `acquire_timeout` 用的是 sqlx 默认 30s，于是慢查询期间所有后台任务（提醒、agent run、跨群问答、executive 持久化）排队到超时。`Yunxi Executive persistence exceeded 2s; latest state remains dirty` 就是这条链的下游症状。

另外 PG 自身是默认配置：`shared_buffers=16MB`、`work_mem=4096kB`（4MB，偏小）、`autovacuum_max_workers=3`。

### P1-1 重启风暴（09-12 共 354 次）

按小时统计 09-12 的 `Started kovi-bot.service`：

```
00时 41   01时 43   02时 32   03时 19   07时 16   08时 19
09时 54   10时 54   11时 38   12时  5   13时  7   14时 10
15时  6   16时  3   17时  4   18时  3
```

09-11 也有尾部的 23 时 32 次。这段高频重启是 P1-2 并发锁竞争的放大器和触发条件。

### P1-2 对话并发槽位竞争：`conversation already has an active reply`

16 条，全部集中在单个进程 PID 934979，且呈 **4 次重试 / 每次间隔 200ms→500ms→1000ms** 的固定节奏：

```
2026-09-11T03:06:41 群聊 641996763  ×4   (03:06:41, 41, 42, 43)
2026-09-11T03:06:52 私聊 3052405886 ×4   (03:06:52, 52, 52, 53)
2026-09-11T03:08:38 群聊 784469488  ×4   (03:08:38, 38, 39, 40)
2026-09-11T03:08:50 群聊 784469488  ×4   (03:08:50, 50, 51, 52)
```

这**不是缺陷，是设计**：`plugins/model/src/model/recall.rs:226,247` 的直发路径在检测到在途回复时返回 `ConversationBusy`，`group.rs:1144` / `private.rs` 为重试 3 次（`[200, 500, 1000]`ms）+ 首次共 4 次，注释明确写着控制命令（`#禁言` / `#结束禁言`）必须拿到回执。观察到的 200/500/1000ms 间隔与常量完全吻合。

但有两个真实问题：

1. `recall.rs:226,247` 对**每一次尝试**都打 `eprintln!("[ERROR] …")`。一次正常的直发最多产生 4 行 ERROR，把“重试中”渲染成“故障”，污染监控基线。
2. 重试预算耗尽后（本例中 4 次都没拿到槽位），控制命令的回执**静默丢失**，日志里看不出“最终失败”。

配合 P1-1 的高频重启看：进程刚起来时残留的 in-flight 状态会让槽位在较长时间里保持 busy，4 次重试（约 1.7s 总跨度）不足以消化，于是集中爆发。

### P2-1 model 输出合法静默标记，host 仍判定为“不可执行计划”

16 条，预览内容完全相同：

```
[WARN] 回复协议未形成可执行计划 (场景: 群聊 N, 阶段: 首次回复, 字符数: 37,
       动作开始标记: 1, 动作结束标记: 1, 当前发送者字段: false,
       预览: "[[REPLY_ACTION]]{\"disposition\":\"silent\"}[[/REPLY_ACTION]]")
```

这正是 `plugins/model/src/model/reply_disposition.rs:5` 定义的 `SILENT_REPLY_OUTPUT` 常量本身，也正是 `utils.rs:90` 的 `EMPTY_REPLY_REPAIR_PROMPT` 明确要求模型输出的合法静默：

> 若确实不应回应，只输出完整的 `[[REPLY_ACTION]]{"disposition":"silent"}[[/REPLY_ACTION]]`

同时 `should_repair_empty_reply`（`utils.rs:852`）有条件 `!plan.is_silent()`，静默计划本不该触发修复。**当前代码静态阅读无法解释这条日志**——需要在部署 revision `4e6636d` 上补一个回归测试确认（见第三节第 5 项）。

消耗是实在的：每次多花一轮 `repair_empty_reply` 模型调用，且把“按协议正确静默”误报成异常。

### P2-2 记忆向量回填外键失败

```
2026-09-12T14:36:09 [ERROR] 记忆向量回填失败: error returned from database:
  insert or update on table "kovi_bot_memory_embeddings" violates foreign key constraint
  "kovi_bot_memory_embeddings_memory_id_fkey"
```

表定义为 `memory_id TEXT PRIMARY KEY REFERENCES kovi_bot_memories(id) ON DELETE CASCADE`（`memory/mod.rs:937`），说明回填时内存列表里的记忆已被淘汰/删除，DB 行已不存在。触发点是 `memory/mod.rs:2880` 的 `INSERT INTO kovi_bot_memory_embeddings`。

这属于“内存态与 DB 态不同步”一类问题：淘汰只删 DB 行、没有同步内存列表，回填就会拿着已消失的 id 反复重试。

### P2-3 记忆条数触顶失控保护

```
[WARN] 记忆条数 N 超过失控保护阈值 N，按重要性淘汰到阈值内——正常情况不该走到这里，请检查写入速率
```
出现 2 次。日志自己已经写明“正常情况不该走到这里”，属于需要看容量的信号。

### P2-4 QQ 通话稳定性与身份识别

- 4 次 `QQ 通话未能解析出来电者 QQ 号，按不在白名单处理` —— 静默拒接，且无法从日志还原是谁打的
- 4 次 `QQ 通话采集结束: parec 已退出（无 stderr 输出）` + 2 次 `（Connection failure: Connection terminated` —— 把正常挂断也记成 ERROR
- 4 次 `QQ 语音通话无法读取桥状态: 通话桥请求失败: http://127.0.0.1:6110/v1/calls/current`
- 2 次 `QQ 通话播报失败: 通话打断通道已关闭` / 1 次 `写入通话音频失败: Broken pipe`
- 1 次 `通话授权名单写入桥失败 (/home/ubuntu/napcat-qq-call/bridge/runtime/allowed-callers.json): Read-only file system (os error 30)` —— 授权名单**根本没写进去**
- 1 次 `QQ 通话回复链未在限定时间内收尾` / 1 次 `QQ 语音通话漏接（有过邀请但从未进房…）`

### P3 观察项

- `主动群聊消息发送失败 … retcode=N` 9 次：主动发送没走 `send_guard` 退避（对比 `[MUTE] 群 641996763 发送处于被拒退避,跳过可见消息` 已有 118 次跳过，说明被动路径退避生效良好）
- `记忆向量回填` 在 5 秒超时内失败（`API_RESPONSE_TIMEOUT = 5s`），embed 服务被换出后必然超时
- `本机 Intrinsic 视觉模型仅支持单图` / 像素超限，上游缺预缩放（已并入 P0-2）
- 09-12 15:14 的池饥饿与 16:59/17:02 的 `retcode 1200`（QQ 侧 `EventChecker Failed`）是两件事，后者已被 Core 正确归类为 `qq_send_denied: retryable: false`

---

## 二、优化策略

### 立刻做（当天，改配置为主）

1. **给 embed 服务抬内存上限**：`MemoryMax` 1.5G → 2G，并确认 `MemoryHigh` 设一个 1.6G 的软阀让内核先回收而不是直接 OOM kill。
2. **同时常驻两个模型是浪费**：reranker 只在重排时用，考虑按需 load 或拆成独立 service，让 embed 常驻进程回到 ~500M。
3. **PG 连接池扩容 + 快速失败**（`plugins/model/src/memory/mod.rs:497`）：`max_connections(5)` → 15~20，并显式 `.acquire_timeout(Duration::from_secs(5))`。用 5s 快速失败替代 30s 排队，让后台任务失败得早、可观测，而不是把 executive 持久化一起拖垮。
4. **PG 参数调优**：`shared_buffers` 16MB → 512MB；`work_mem` 4MB → 16MB；`autovacuum_max_workers` 3 → 4。全库数据量只有几 MB，加大 buffer 是低风险高收益。
5. **视觉链路先止血**：入口加尺寸预缩放（长边压到模型上限内）+ 多图拆分/取首图策略，让 8400 万像素和 3 张图这两类不再直接走到失败分支。
6. **给热表补 ANALYZE**：`kovi_bot_agent_runs` 从未被 analyze 过（`last_analyze` / `last_autoanalyze` 均为空），其余热表也停在 09-12 12:56~17:20 之间。

### 短期（本周，改代码）

7. **静默标记的差异化处理**：在 host 侧显式识别 `SILENT_REPLY_OUTPUT`，与“真空回复”走不同分支——合法静默不触发 `repair_empty_reply`、不打 WARN。当前这条 WARN 已经把“按协议正确静默”误报成异常 16 次。
8. **回填加存在性防御**：`memory/mod.rs:2880` 改为带回写目标存在性判断，或 `ON CONFLICT (memory_id) DO NOTHING`；更彻底的是让淘汰逻辑同时更新内存列表，这样失败重试会自然收敛而不是无限重复。
9. **直发重试日志降噪**：`recall.rs:226,247` 把每次尝试的 `[ERROR]` 改成单条汇总日志（尝试次数 + 最终结果），只在重试预算耗尽时打 ERROR。重试预算耗尽应上报而不是静默丢弃。
10. **主动发送接入退避**：9 次 `retcode=N` 的主动群聊发送同样走 `send_guard::record_rejection`，与被动路径对齐。

### 中期（本迭代）

11. **重启风暴收口**：把 09-12 那 354 次重启的触发源钉死（部署脚本 / `rollback.conf` drop-in / panic 退出码），并在 `systemd` 加 `StartLimitIntervalSec` + `StartLimitBurst` 快速熔断，避免崩溃循环反复冲刷对话状态。
12. **机器层面要么升配要么隔离**：3.7G 跑 embed + bot + speech + NapCat + PG，峰值必然换页。优先加内存，其次把 PG 或 speech 挪走。给 `kovi-bot.service` 与两个 python 服务设 `MemoryHigh`，让压力下先回收而不是硬 OOM。
13. **把“池饥饿”变成指标**：`acquired_after_secs` 超阈值目前只打 WARN。建议按小时聚合 `acquire>2s` 次数与 `pool timed out` 次数并告警——15:12 那次已经持续数分钟，本可在第一时间发现。
14. **通话桥加固**：`calls/current` 拉取失败、`parec` 正常退出、`Broken pipe` 都不该是 ERROR；来电者 QQ 号解析失败应记录原始 event payload 以便白名单规则能定位，并在解析失败时给出明确的“无法判定、已按非白名单处理”回执而不是静默拒接。
15. **修 `Read-only file system`**：`napcat-qq-call/bridge/runtime/` 的挂载/属主，否则授权名单写入一直失败（现在只是一条 WARN，实际后果是白名单不生效）。
16. **模型能力边界写进 System Prompt**：本轮日志里 bot 说出“我这边看不到自己的内存占用”后用户继续追问。把“能查什么、不能查什么”作为事实写进提示词，减少无效来回。

### 验收方式

- P0-1/P0-3：连跑 48h，`journalctl -u kovi-bot.service | grep -c "池\|pool timed out"` 归零，`yunxi-embed` 无 `oom-kill`
- P0-2：`截图分析失败` 归零，且至少出现一次成功识图
- P1-1：`systemctl show kovi-bot.service -p NRestarts` 增速 < 1/天
- P1-2/P2-1：`错误日志 4 连发` 与 `回复协议未形成可执行计划` 归零
- 全程：WARN/ERROR 日均 < 5 条（当前 157/7 ≈ 22 条/天）

---

## 三、需要先确认的两件事

1. **P2-1 的复现路径**：建议在部署 revision 上给 `parse_reply_output` + `should_repair_empty_reply` 补一个针对 `SILENT_REPLY_OUTPUT` 的回归测试。当前代码静态阅读应当不触发这条 WARN，日志与代码不一致，需要先钉死是解析分支还是修复分支在报。
2. **目标容量基线**：`记忆条数超过失控保护阈值` 与 1.5G embed 上限都指向“容量按现状设、按增长撞墙”。建议先定一版预期（群数 / 日均消息量 / 记忆条数上限），再回填阈值，否则只会周期性复发。

---

## 附：本轮巡检用到的命令

```bash
# 服务与资源
systemctl --no-pager --full -n 0 status kovi-bot.service
systemctl status yunxi-embed.service
free -m; ps aux --sort=-%mem | head -8

# 日志聚类（把数字归一后按模板计数）
journalctl -u kovi-bot.service --since '7 days ago' --no-pager -o short-iso > /tmp/kovi-7d.log
grep -E '\[(WARN|ERROR)\]' /tmp/kovi-7d.log \
  | sed -E 's/[0-9]+/N/g' | sort | uniq -c | sort -rn | head -40

# 重启分布
grep -E 'systemd\[1\]: Started kovi-bot' /tmp/kovi-7d.log | awk '{print $1}' | cut -c1-13 | uniq -c

# DB 侧
sudo -u postgres psql -d postgres -tAc "select count(*), state from pg_stat_activity group by state;"
sudo -u postgres psql -d postgres -tAc "select relname, n_live_tup, n_dead_tup,
  pg_size_pretty(pg_total_relation_size(relid)) from pg_stat_user_tables order by 4 desc limit 15;"

# OOM
sudo journalctl --since '7 days ago' | grep -iE 'oom-kill|Killed process'
```
