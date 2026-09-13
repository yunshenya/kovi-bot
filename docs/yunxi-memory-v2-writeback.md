# Core 回合的记忆写入（Memory v2 双写的下半截）

## 问题

`docs/yunxi-core-v1.md` §63 写的双写策略是「写 new memory + 保留 legacy write」。落地时只做了
**一半**：Core→legacy 的兼容投影有了（`memory_store.rs::remember`），但聊天记忆的**写入仍全在
V1**（`model/utils.rs` 的 `MEMORY_REPOSITORY`，写 `kovi_bot_memories`）。Core 接管回复之后：

| 事实 | 数据 |
|---|---|
| Core 是线上回复主力 | 今天群聊发出 80 条，V1 只处理 4 个回合（Core 决策 88 次） |
| Core 读 legacy 是**前缀**匹配 | 群只读 `group_chat%`、私聊只读 `private_chat%`（`memory/mod.rs:1618`） |
| 读不到的有 | `group_observation` 656、旧格式 `private` 32、`proactive_group_chat` 5——比读得到的（`group_chat` 574 + `private_chat` 99 = 673）还多 |
| `group_chat` 只有 V1 回合会写 | 主群最近 3 天只新增 9 条；最近 32 条跨 8/29–9/13 |
| Core 不写任何长期记忆 | `yunxi_memories` 至今 1 条（global 世界事实），conversation/person 作用域 0 行 |
| 短期兜底很薄 | Core 提示词只带最近 8 条群消息（`MAX_CORE_RECENT_GROUP_MESSAGES`）≈ 1 小时 28 分 |

后果：**1.5 小时以前、且不是 V1 回过的对话，对 Core 等于没发生过**；新对话不进长期记忆
（只有 Mind 情节进，423 条且是活的）。记忆页那 1367 条里她能读到的只有 673 条
（`group_chat` + `private_chat`），而且这部分还在停止增长。

## 目标 / 非目标

**目标**：Core 回合重新产出长期记忆，且新旧两表同时增长——召回立刻生效，记忆页/人物页不再是
"只有旧数据"。

**非目标**（本轮不做，避免把一次修复变成一次重构）：

- 不改召回策略：前缀匹配保持原样（放宽到作用域会把 1222 条短噪音灌进上下文）；
- 不做历史迁移：那是 `yunxi-memory-migrate`（离线、按批次、可回滚）；
- 不让模型自己决定"记什么"：那要给 Core 加 `StateUpdateProposal::Memory`，见「分步」第 3 步。

## 设计

### 1. 写什么：一轮 = 两条，且只有真的回了才写

与 V1 的 `group_chat` 语料同形，保持语料同质：

```text
[HH:MM:SS] 称呼: 对方正文     ← 收到的
芸汐: 她说出去的话            ← 发出的
```

- **只有投递成功（`ActionPortOutcome::Delivered`）才写**。Core 决定沉默的回合什么都不写——
  保持"她参与过的对话"这个语义，与旧 `group_chat` 完全一致。
- **不写未回复的观察流**：`group_observation` 继续由 Host 那条分支写（今天的量：主群 579 条）。
  它们读不到不是本轮要解决的问题，硬塞进召回只会让上下文变吵（文档自己写着"她的记忆语料
  大多是短群聊噪音"）。
- **主动消息（autonomous tick）本轮不写**：它没有"入站行"，语义上不是"她回的"。
  V1 时代它写的是 `proactive_group_chat` / `proactive_private_*` 语料（全库目前只有 5 条），
  量极小；补它要额外解析私聊会话 → person，留作下一步。

### 2. 作用域与兼容投影

写入走 `PostgresMemoryStore::remember`，适配器会自动生成 legacy 投影（这是现成的那半截）：

| 会话 | v2 作用域 | legacy 投影（自动） | Core 召回 |
|---|---|---|---|
| 群聊 | `Conversation(conversation_id)` | `context='group_chat'`, `subject=群号` | ✅ 前缀 `group_chat` |
| 私聊 | `Person(person_id)` | `context='private_chat'`, `subject=QQ` | ✅ 前缀 `private_chat`；人物页也能数到 |

私聊选 **Person** 而不是 Conversation：与旧语料同形（V1 的私聊记忆就是 subject=QQ）、人物页
计数口径不用改。适配器在"这个人的 QQ 身份不唯一"时会自己退化成只写 v2（`UnsupportedScope`
分支），不需要我们加判断。

### 3. 落点：ingress 暂存 + 投递成功落库

两个位置各写一半，各自都有完整信息，不需要跨层传参：

1. **入站暂存** —— `bridge.rs::resolve_and_submit_inner`（那里已有 `InboundMessage`：正文、
   称呼、`person_id`、`conversation_id`）。事件是宿主自己构造的（`WorldEvent::message_received`），
   所以 `event.id()` 已知；把渲染好的入站行按 **`EventId`** 放进一个有界的进程内暂存
   （`Arc<StdMutex<...>>`，沿用 `FamiliarityCache` 那种做法，容量 256，LRU 淘汰）。
2. **落库** —— `bridge.rs::run_runtime` 的 `Planned` 分支：现有代码已经在
   `CognitiveIntent::SendMessage` 对应 `ActionResult::Executed { outcome: Delivered }` 时做
   conversation continuation 登记，在同一处取出暂存行 + `intent.content`（正文在意图里，
   `ActionPortOutcome` 不带正文），调写入模块。`RuntimeObservation.event_id` 就是暂存的键。

暂存天然做了"所有权"过滤：只有交给 Core 的事件才会进暂存，Host 回合不受影响。

**为什么不用 Core 的 `StateUpdateProposal`**：那要给 Core 契约加 `Memory` 提案——planner schema、
校验、快照、序列化兼容、一大批测试全要动。宿主侧写入能以最小改动先恢复数据流；让模型自己
决定记什么是第 3 步的事。

### 4. 重要度与保留

- v2 `importance = 40`（legacy 投影 = 4，与历史 `group_chat` 平均 5.5 同量级）。
- 低于保护阈值 70 → 30 天后随 `memory.retention_days` 自然老去；`max_entries = 50000` 只是
  失控保护，不是管理手段。
- 体量：按今天的量（群 76 条回复 + 私聊 19 条）× 2 ≈ **190 行/天**，30 天约 5700 行。

### 5. 幂等

- 进程内按 `EventId` 去重：一条入站只写一次，写成功即从暂存移除。
- legacy 侧本来就有（subject + context + 归一化正文）去重，重放只会合并。
- **跨重启可能重复一条 v2 行**——接受，并记进已知取舍：v2 是 append-only 的事实来源，重复
  一条的代价远小于"漏记一轮"；真要收口得给 `MemoryStore::remember` 加外部 id，那是契约变更。

### 6. 失败处理

- 写记忆失败**绝不阻塞回复**：`kovi::log::warn!` + 丢弃这一条（与 Core 其它 fail-soft 一致）。
- 暂存有界；进程重启丢暂存 = 那一轮不记（可接受，不补写）。

### 7. 开关与回滚

- 新配置 `memory.core_writeback_enabled`（`MemoryConfig` 加字段 + 默认值 +
  `bot.conf.example.toml` 补一行 + 跑 `tools/admin-docs/extract_config_docs.py` 重生成
  `config_docs.json`）。
- **回滚 = 关开关**：立刻停写；已经写入的行可按 scope/时间删（不碰旧表，也不影响召回正确性）。

## 测试

- 单元测试（`memory_writeback.rs::tests`）：两种正文渲染与旧语料同形、超长正文按字符截断
  （含"全是待转义字符"的最坏情况仍落在记忆限额内）、空回复不写、暂存有界且淘汰最旧。
- 落库那半截（`remember` → v2 + legacy 投影）由 `memory_store` 既有测试覆盖；本层没有再加
  DB 集成测试——CI 的 `#[ignore]` 清单要手工登记，同样的证据用发布后的线上验证更直接。
- 线上验证口径：发布后打一场对话，`yunxi_memories` 应出现对应作用域的新行，
  `kovi_bot_memories` 应出现同正文的 `group_chat` / `private_chat` 行；记忆页与人物页的数字
  跟着动；召回日志里的记忆条数不再是 0。

## 分步

1. **本次**：宿主侧写入 + 开关 + 测试（就是上面这套）。
2. **观察一周**：召回有没有变好、上下文有没有变吵（看 journal 的召回 token 与人工体感）。
3. **之后**：把"写什么"交给模型——Core 加 `Memory` 提案，实现 §63 真正的双写；那时再决定
   观察流要不要也进 v2、以及要不要让模型写"他喜欢爵士"这类事实记忆（现在写的只是对话流水）。

## 已定的取值（2026-09-13 确认）

1. **私聊写 `Person` 作用域**（而非 `Conversation`）：与旧语料同形、人物页可数。
2. **重要度固定 40**（而非跟随 V1 的长度规则 2~5）：召回排序上与新语料同量级，且规则简单可解释。
3. **开关默认 `true`**，随下次发布生效：写入是纯增量、可随时关、可按时间删；先观察再开会让
   "数据继续丢"多持续一轮发布。
