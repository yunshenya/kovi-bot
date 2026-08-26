# Yunxi Core v1：平台无关持续 Agent 核心架构与迁移开发文档

**文档状态：** 最终整合版  

**项目：** `yunshenya/kovi-bot`  
**文档版本：** 2.0  
**主要语言：** Rust  
**当前宿主：** Kovi + OneBot 11 + QQ  
**当前存储：** PostgreSQL，可选 Redis  
**长期目标：** Yunxi Desktop / Mobile / Web / Server / Kovi 等多个宿主共享同一个芸汐核心

**当前交付范围（2026-08-27）：** `yunxi-core` 与现有 Kovi/QQ Host 的渐进迁移。
Desktop、Mobile、Web、Server 产品 Host、协议层和 UI 均是未来方向，不属于当前阶段的
交付物或完成条件。Live2D、语音、游戏、桌面控制等高级能力也只保留架构扩展点，不代表
已经实现或已进入本阶段排期。`yunxi-cli` 是 Fake Host 边界测试，不等同于真实第二宿主迁移。

---

# 1. 文档定位

本文档取代此前以：

> “在 QQ Bot 内部构建 Neuro-sama-like Cognitive Runtime”

为核心的设计。

新的最高架构目标是：

> **构建一个完全不依赖 QQ、Kovi、OneBot 或具体 UI 的 `Yunxi Core`。**

当前 `kovi-bot` 应逐步变成：

> **Yunxi Core 的第一个 Platform Adapter / Host。**

未来即使：

- 不再使用 QQ；
- NapCat 停止维护；
- Kovi 不再使用；
- OneBot 被替换；
- 项目改成独立桌面 App；
- 改成手机 App；
- 改成 Web；
- 接入 Live2D；
- 接入语音；
- 接入游戏；

都不应该要求重新实现：

- 人格；
- 长期记忆；
- 关系；
- 情绪；
- OpenLoop；
- Goal；
- Attention；
- Cognitive Runtime；
- Planner；
- 主动行为逻辑。

---

# 2. 当前实现状态

本节用于记录 `kovi-bot` 在 Yunxi Core 拆分开始前已经存在的成熟能力。

**本节属于迁移基线，不得在后续架构优化中删除。**

后续 V1～V5 的目标不是“重新发明这些能力”，而是逐步把已经成熟的实现迁移到更清晰的平台无关边界中。

当前项目已经具备：

```text
长期记忆
滚动摘要
用户 / 群档案
Bot Personality / Mood
主动聊天
群聊插话
视觉 / 图片语义入口
工具调用
提醒
跨群任务
可打断回复
消息合并 / batching / coalescing
ConversationCoordinator
ReplyTicket
PostgreSQL 持久化
可选 Redis
```

因此，新架构必须优先：

```text
Reuse
→ Wrap
→ Bridge
→ Migrate
```

而不是：

```text
Delete
→ Rewrite Everything
```

---

## 2.1 当前运行时入口

`plugins/model/src/lib.rs` 已经承担大量运行时初始化工作，包括但不限于：

- Memory；
- Reminders；
- Agent Runtime / Goals；
- Agent Tasks；
- Sticker Memory；
- Redis；
- Tool Registry；
- Group Access；
- Message Handlers；
- Proactive Manager；
- 多个 Tokio 后台循环。

因此：

> `plugins/model` 当前仍然是生产 Host，不应在建立 `yunxi-core` 时一次性拆空。

第一阶段应保持：

```text
plugins/model
    │
    ├── existing production runtime
    │
    └── Yunxi Core bridge / shadow integration
```

然后再逐步迁移。

---

## 2.2 当前后台运行循环

现有系统不是一个纯粹的：

```text
Message → LLM → Reply
```

程序。

当前已经存在多个长期后台循环，例如：

```text
proactive_chat loop
→ 主动聊天

reminders loop
→ 提醒

agent_tasks loop
→ 跨群任务

mood drift loop
→ 情绪漂移

maintenance loop
→ Memory / runtime maintenance

health loop
→ 健康监控
```

所以 Yunxi Core 的 Event Runtime 必须吸收这些“长期运行”能力，而不是把系统退化回 request-response chatbot。

---

## 2.3 ConversationCoordinator / ReplyTicket

当前 `ConversationCoordinator` 已经提供非常重要的会话生命周期基础，包括：

- bounded shared lifecycle；
- FIFO / queue semantics；
- ReplyTicket；
- interruption；
- generation validity；
- 消息竞争协调。

这套机制必须复用。

V1 新增的：

```text
PendingOutgoing
ConversationVersion
Thinking
Prepared
Committed
Sent
Superseded
```

应建立在现有：

```text
ConversationCoordinator
+
ReplyTicket
```

之上。

不得平行重新实现第二套 conversation generation coordinator。

---

## 2.4 Message batching / coalescing

当前系统已经存在消息合并、batching / coalescing 相关能力。

因此：

用户短时间连续发送：

```text
A
B
C
```

不应重新设计成：

```text
A → generation 1
B → generation 2
C → generation 3
```

三个彼此竞争的完整回复流程。

V1 Conversation Concurrency 应优先复用现有 batching / coalescing 逻辑，并只补充：

- conversation version；
- pending outgoing；
- pre-commit revalidation；
- collision handling。

---

## 2.5 agent_runtime

当前 `agent_runtime.rs` 已经不是简单聊天回复代码。

它已经具有：

- persistent goals；
- action executor；
- 模型提出 Action；
- Rust 控制真实副作用；
- 权限校验；
- ID；
- persistence；
- idempotency；
- side-effect execution。

现有原则本身与 Yunxi Core 目标一致：

```text
LLM proposes
↓
Rust validates
↓
Rust executes
```

因此 V1/V3 不得把这一层改回：

```text
LLM 直接执行副作用
```

未来应逐步把其平台相关 Action 映射为：

```text
Core Intent / Action
→ Action Arbiter
→ Host / Adapter
```

---

## 2.6 agent_tasks

当前 `agent_tasks.rs` 已经实现较成熟的持久化跨群任务状态机。

已有能力包括：

```text
ask
→ collect events
→ quiet period / deadline
→ summarize
→ reply
```

并且已有：

- lease；
- retry；
- idempotency；
- persistence；
- restart recovery。

这是非常重要的生产级能力。

后续 Goal / Plan / Executive 设计必须：

```text
integrate
```

而不是：

```text
replace with a new toy state machine
```

尤其 V3 `PlanState` 不得在没有迁移方案的情况下与 `agent_tasks` 重复实现同一套可靠任务状态机。

---

## 2.7 proactive_chat

当前 `proactive_chat` 已经能执行主动消息，但主要机制仍偏向：

```text
周期检查
→ 条件判断
→ eligible target
→ random / heuristic target selection
→ topic generation
→ send
```

因此它已经证明：

> 系统可以在没有即时用户消息的情况下主动行动。

当前交付范围已经在 Core 建立 motive/candidate/`ReachOut` 边界；成熟的调度、画像/冷却策略
和真实投递继续由 Kovi Host/Adapter 承担，符合当前 DoD。

以下是长期可选的跨宿主策略迁移目标，不是当前未完成项：

```text
IdleTick / WorldEvent
→ OpenLoop
→ Memory
→ Relation
→ Affect
→ Interests
→ InnerAgenda
→ Executive
→ ReachOut(PersonId)
```

现有 proactive 作为 Host 行为继续保留。只有当其中的平台无关决策需要被多个真实 Host
共享时，才逐步迁移相应策略；调度器生命周期和平台投递仍属于 Host/Adapter。

---

## 2.8 topic_generator

当前主动话题生成已经有相对成熟的 Memory-aware prompt，并存在类似 motive：

```text
follow_up
share
check_in
react
```

且支持：

```text
[[NONE]]
```

这意味着当前系统已经具备：

> “即使有主动机会，也可以决定不说。”

后续 `Silent / Defer / NoAction` 应复用这一产品语义，而不是把所有主动 trigger 变成必发消息。

---

## 2.9 semantic.rs

当前已有统一 Semantic Understanding。

其结构化理解已经覆盖多类信息，例如：

- mood；
- stop / no-reply intent；
- image intent；
- conversation relevance；
- interjection worthiness；
- interests；
- topics；
- group atmosphere。

这部分是未来：

```text
WorldEvent Observation Extraction
Attention
World Model extraction
```

的重要输入。

不得为了 v2/v4 再平行建立完全重复的 message-understanding 模型链路。

优先：

```text
reuse existing semantic result
→ map into new domain snapshot
```

---

## 2.10 mood_system

当前 Mood System 已存在 Bot Personality / Mood 漂移逻辑。

同时需要特别注意：

> 当前部分 user semantic mood 可能影响 global bot mood。

Core v2 Affect 的平台无关状态必须明确拆分：

```text
PerceivedUserAffect
→ BotAppraisal / Reaction
→ Slow Global Affect
```

避免不同 Person 的情绪信息通过 global state 无意串扰。

迁移阶段必须保持现有行为兼容，并通过 Shadow / metrics 验证后再改变更新规则。成熟的
global `BotPersonality`/mood 兼容策略当前可以留在 Host；将其中可跨宿主复用的领域规则迁入
Core 是长期可选工作，不是当前 DoD 缺口。

---

## 2.11 Memory

当前 Memory 已具备：

- `MemoryEntry`；
- `UserProfile`；
- `GroupProfile`；
- `BotPersonality`；
- rolling summaries；
- proactive state；
- PostgreSQL persistence。

当前 Memory 已经是生产系统的重要基础。

现阶段主要缺少更明确的：

```text
Episodic Memory
Prospective / OpenLoop Memory
Platform-independent PersonId / ConversationId scope
```

因此后续 Memory v2 应：

```text
bridge / dual-read / dual-write / migrate
```

而不是 destructive rewrite。

旧 Memory 数据不得自动删除。

---

## 2.12 当前 Memory Scope 的平台耦合

现有 Memory 仍较多依赖类似：

```text
subject_id: i64
```

的 QQ-era scope。

长期目标是：

```rust
pub enum MemoryScope {
    Person(PersonId),
    Conversation(ConversationId),
    Global,
}
```

迁移时：

```text
QQ private subject
→ ExternalIdentity
→ PersonId

QQ group subject
→ ExternalConversation
→ ConversationId
```

---

## 2.13 Reminder

当前 Reminder 已经存在可靠提醒能力。

因此必须明确：

```text
Reminder
≠ OpenLoop
```

Reminder：

```text
用户明确要求在某时交付
→ reliable MustExecute
```

OpenLoop：

```text
系统认为未来值得重新关注
→ Planner 可 Reply / Silent / Defer
```

不得用 v2/v3 的自主行为机制削弱 Reminder reliability。

---

## 2.14 tool_access

当前 `tool_access.rs` 已经提供较强 Capability / Security Boundary，包括：

- ToolExecutionContext；
- authorization；
- admin control；
- capability restrictions；
- tool whitelist / scope。

这部分必须作为 v1 ActionPort / v3 Executive 的下层硬边界继续保留。

原则：

```text
Safety / Permission
>
MustExecute
>
Executive
>
Mind Preference
```

Mind / World Model / Planner 永远不能绕过现有工具权限。

---

## 2.15 ModelGateway / 当前模型调用

当前模型调用已经具备：

- OpenAI-compatible model 接入；
- per-scope conversation history；
- model concurrency；
- generation pipeline。

v5 应将它：

```text
wrap as Backend
```

而不是直接删除。

第一步应完成：

```text
Existing ModelGateway
→ ModelBackend Adapter
```

并保持生产行为不变。

之后才增加：

```text
ModelRouter
LocalBackend
Role-specific routing
```

---

## 2.16 当前角色提示约束

现有模型提示中存在较强 human-roleplay / real-person style guard。

长期更推荐：

> 保持自然角色表现，但被直接询问身份时不需要虚构现实人类身份。

该问题属于 Persona / Product Policy 调整，不应和 Core 拆分同时大范围修改。

迁移阶段优先保持行为稳定。

---

## 2.17 当前 Embedding 状态

在此前仓库检查中，没有发现一个已经成为核心依赖的完整 embedding retrieval layer。

因此：

v1～v4 不应假设：

```text
vector DB 已经存在
```

可以先使用：

- existing memory search；
- recency；
- salience；
- structured scope；
- semantic model。

v5 再抽象 `EmbeddingBackend`，需要时逐步引入 hybrid retrieval。

---

## 2.18 Group Authorization / Rate Limit / Transport

当前仓库已经有：

- group access；
- message transport；
- rate limiting；
- authorization；
- existing reply generation pipeline。

这些都属于已经成熟的生产机制。

新架构必须复用。

---

## 2.19 CI / Test / Security / Deployment

当前项目已经具有较好的工程纪律，包括：

- `cargo fmt`；
- `cargo clippy`；
- PostgreSQL integration tests；
- release build；
- RustSec；
- secret scanning；
- deploy / rollback 相关流程。

因此所有 V1～V5 Phase 都必须延续：

```text
READ
→ PLAN
→ IMPLEMENT
→ FORMAT
→ TEST
→ REVIEW
→ FIX
→ RETEST
```

不得以“架构大改”为理由绕过现有测试与安全流程。

---

## 2.20 当前实现状态的迁移结论

当前 `kovi-bot` 已经拥有大量 Agent 所需的局部能力。

真正缺少的不是：

> “从零写一个 Agent。”

而是：

> “把这些已经存在的能力统一到平台无关、事件驱动、长期可演进的 Yunxi Core 边界中。”

因此总体迁移策略必须是：

```text
现有生产机制
      │
      ▼
Adapter / Port / Bridge
      │
      ▼
Shadow Runtime
      │
      ▼
逐模块迁移
      │
      ▼
Yunxi Core 成为真正核心
```

而不是：

```text
停止现有系统
→ 新建一套完全不同实现
→ 一次性切换
```

这也是后续所有开发文档必须遵守的基线。

## 2.21 当前实现状态（2026-08-27）

以下状态以仓库代码和测试为准；“部分完成”表示 Core 边界已经存在，但部分专用行为仍由
Kovi/QQ Host 或人工验收承载，不能误读为迁移已经结束。
本次审计的非数据库自动测试已通过；需要 `DATABASE_URL` 的 PostgreSQL fixture 在当前环境中
未执行，自动测试也不能替代下文列出的生产数据、真实宿主和长期运行人工验收。

| Phase | 状态 | 已落地与剩余 |
| --- | --- | --- |
| 0 Core crate | 已完成 | `yunxi-core` 无 Kovi/OneBot/PostgreSQL 依赖，可离线测试。 |
| 1 Identity | 已完成 | Core ID、QQ mapping、ConversationMember、附件归一化和 identity unlink 已有；Person import、direct route 与删除固定使用 `Person -> route -> Conversation` 锁序，删除按 canonical membership 枚举跨平台 direct conversation、保留 Group，并拒绝误删其他 Person 的 direct route；versioned Person snapshot 会一致性携带 Memory/Relation/Affect/OpenLoop/Goal，超限、外部身份归属冲突及异内容 ID 冲突均 fail-closed。真实跨平台 host 携带性仍需验收。 |
| 2 Core Runtime | 已完成 | QQ ingress 进入 bounded `WorldEvent` runtime、Attention 和 WorkingState。 |
| 3 OpenLoop | 基础完成 | PostgreSQL store、去重、容量、atomic claim、lease recovery 已实现。到期 owner 合同已接线：Person 走 `ReachOut`，Conversation 走同会话非引用 `SendMessage`，Global 无安全路由时恢复为无 `due_at` 的 `Open`；真实 QQ 投递、跨重启和长时间运行仍需人工验收。 |
| 4 Memory Bridge | 已完成 | Core `MemoryStore` 已接入规范化 PostgreSQL memory，并保留 legacy 双读/双写。 |
| 5 Proactive | 当前范围完成 | Core motive/candidate/`ReachOut` 和 canonical owner 路由已接通；真实调度、画像/冷却策略和投递属于 Host/Adapter，保留现有实现符合当前边界。将可跨宿主复用的选择策略进一步迁入 Core 是长期可选工作。 |
| 6 Intent / Action | 当前范围完成 | `SendMessage`、`ReachOut`、`UseTool`、Goal/OpenLoop 管理 Action、arbiter、QQ adapter 已接通，权限和路由缺失时 fail-closed。普通 action 使用 event-local deterministic idempotency key；`UseTool` 由有界 one-shot Host capability 将 exact key + tool envelope 指纹绑定原 `ReplyTicket`，副作用 builtin/MCP 在 dispatch 前重验 ticket、actor route、membership/route、管理员、群授权与 context，不会让旧计划借新 ticket。`SendMessage`/`ReachOut` 使用 PostgreSQL delivery ledger，以业务 key 和完整 envelope 指纹持久化 `Committed`/`Sent`/`Unknown`/`Failed`，跨重启阻止重放；不可确认的投递返回非成功终态 `DeliveryIndeterminate`，不会伪造成功事件。Person 数据删除在同一事务内清理可归属账本，snapshot 不携带 Host 投递证据；详细合同见并发规范。 |
| 7 Conversation | 当前范围完成 | 私聊安全纯文本/普通图片及群聊普通文本/普通图片默认由 Yunxi Core 接管。普通群聊先作为有界观察写入 WorkingState，不申请回复 admission，也不会打断正在生成的回复；明确 `@`、文本点名和本地限流抽中的未点名消息才进入 Planner。群聊近期上下文保留匿名成员 `speaker_id` 与 Core 自身成功回复，图片在 Host 侧安全物化后作为同一轮视觉输入交给主模型或独立视觉 Provider。Core 是支持事件的唯一可见回复 owner，入队失败时丢弃并释放 admission，不回退到 Host。Core ingress admission 绑定 exact `MessageId`，同一次模型回复会读取与 exact frozen ticket/reservation 绑定、最多 4096 字符的非可信 Prepared preview，并以受限 `incoming_impact` sidecar 将新消息精化为 `Keep / Rewrite / Merge / Defer`。可见直聊遇到模型取消、协议异常或空计划时走固定保底回复并记录原因。需要直接回复时，`Unrelated` 对 proactive 使用 `Defer`、对 reactive 使用 `Rewrite`，不会吞掉新问题；缺失或非法分类按 `Unknown` fail-closed，全程不增加第二次模型调用。runtime 拒绝 collision 时会原序恢复未提交尾部，`RejectedState` 会释放 exact admission。命令、提醒、Agent Run、引用回复、音视频/文件、表情协议及 coalescing/queue 保留在 Kovi Host/Adapter。 |
| 8 Affect / 9 Relation | 部分完成 | Core store 与平台无关的有界演化已接通，严格绑定当前 `PersonId`。legacy `MessageUnderstanding` 的 mood/confidence/gratitude 已归一化为有界 `InteractionCues` 并 best-effort 投射；Core 直聊也可从同一次模型回复读取受限 cue sidecar，不增加一次模型调用，sidecar 缺失、重复、越界或非法时按 `Unknown` fail-closed。Core 已提供确定性时间衰减，PostgreSQL store 在读取时按 `updated_at` 应用，并忽略负时间差和 60 秒内抖动。legacy profile/mood 只填充缺失行；global `BotPersonality`/mood 兼容策略留在 Host 不阻塞当前 DoD，剩余项是跨天、跨重启和跨平台人工验收。 |
| 10 Memory v2 | 基础完成 | 双读/双写、backfill、数量/哈希校验、审计、rollback、migration CLI 已完成；生产数据抽样、生产回滚演练和真实第二宿主导入仍需人工验收。 |
| 11 Goal Event Integration | 已完成 | Reminder、`agent_tasks`、tools 已投射通用事件，runtime 加载有界 Goal context；legacy 状态保持权威。 |
| 12 CLI Host | 当前范围完成 | Fake host 已跑通 Core 回路；`YUNXI_CLI_STATE` 以单个有界 JSON snapshot 接入 Memory/Affect/Relation/OpenLoop ports 并恢复稳定 Core ID/context，FakeModel 会读取 context 并提交有界 Affect/Relation 更新；`YUNXI_CLI_JOURNAL` 可独立提供同步 turn 审计。它只证明离线边界，不算真实第二宿主或生产携带性验收。 |
| 13 App 预留 | 未来 / 非目标 | 通用 Event/Action 边界可供新 host 接入；Desktop/Mobile/Web/App 产品、协议层和 App 专属能力均未实现，也不属于当前阶段完成条件。 |

Mind v2 的默认影响模式现为 `active`：有界 SelfModel、Belief、Preference、Interest、
OpenQuestion 与 Agenda 会进入 Core 决策上下文；`shadow` 仅保留为显式诊断模式。

因此当前迁移真正未完成的是：历史 Memory migration 的生产抽样与回滚演练；使用真实
第二宿主完成 snapshot 导入和身份携带性验收；以及 Affect/Relation、主动行为及 OpenLoop
调度的跨天、跨重启长期行为验收。QQ 到未来 App 的同一 Person/Memory/Relation/OpenLoop 连续性只能在真实 App Host
存在后人工验收，当前没有完成。Desktop/Mobile/Web/App 产品及 Live2D、TTS、STT、桌面
控制等高级能力属于未来 / 非目标，不计作当前迁移的缺陷。

现有 Kovi 命令、管理员控制面、引用回复、音视频/文件、视觉命令与视觉 Provider 适配、
表情协议、coalescing/queue 和主动投递可以继续作为 Host/Adapter 能力。普通群聊文本与图片
已由 Core 观察并选择性接话，Host 侧只负责安全取图和平台适配。把剩余兼容层进一步变薄是
可选后续，不是当前 DoD 未完成项。

## 2.22 当前 Core / Adapter 边界

前文大量章节描述的是目标边界；仓库当前的实际所有权如下：

| 层 | 当前职责 | 边界说明 |
| --- | --- | --- |
| `crates/yunxi-core` | 平台无关 ID、Event、Runtime、Planner、Affect/Relation/OpenLoop/Goal、Intent/Action、Ports 和确定性领域规则；不依赖 Kovi、OneBot、QQ 或 SQL 实现。 | 不负责 provider 配置、SQL、QQ 路由、权限和真实发送。 |
| `plugins/model/src/yunxi` | 当前 Kovi Host 的 bridge/adapter：QQ identity 与 conversation mapping、PostgreSQL stores、到期 scheduler、旧 ModelGateway 适配、Action 仲裁与 QQ delivery。 | 它是宿主集成代码，不是第二套 Core；其中出现 QQ/SQL 是预期边界。 |
| `plugins/model` Host 兼容层 | 仍承载命令、管理员控制面、引用回复、音视频/文件、视觉命令与 Provider 适配、表情协议、提醒、Agent Task、主动调度、部分画像/冷却和 global `BotPersonality`/mood 兼容策略；普通群聊文本/图片由 Core 观察并选择性接话，Host 仅提供安全取图和 Provider 适配。 | 这些成熟 Host/Adapter 能力可按当前范围保留；其中可跨宿主复用的领域规则可在长期迁移，进一步变薄不是当前 DoD 缺口。 |
| Desktop/Mobile/Web/App | 尚无真实 Host。 | 产品、协议、同步、认证、通知和 UI 全部是未来工作。 |

`KoviModelBackend` 复用现有 ModelGateway 的 provider 与工具策略，并把结果翻译成声明式 Core
plan；这仍是 Host adapter，而不是 Core 直接依赖旧模型模块。当前 Core 接管私聊安全纯文本/
普通图片以及群聊普通文本/普通图片；未点名群聊采用本地抽样后由 Core 决定回复或沉默。命令、
引用回复、音视频/文件和表情协议等专用入口继续由 Host/Adapter 处理。

## 2.23 OpenLoop 到期 owner 路由合同

当前生产接线必须保持：

- `Person` owner 提交 person-scoped `ProspectiveMemoryDue`，成功生成 `ReachOut(PersonId)`；
- `Conversation` owner 提交 conversation-scoped 事件，成功生成同一 `ConversationId` 的
  非引用 `SendMessage`，即 `reply_to = None`；
- Conversation 路由优先使用持久化唯一映射；只有持久层临时不可用时才可回退有界内存
  缓存，权威查询无映射或有歧义时必须 fail-closed；
- `Global` owner 在当前 Host 没有安全投递路由，不提交到 runtime；scheduler 调用
  `defer(id, None, now)`，把记录恢复为 `Open` 并清除 `due_at`/`triggered_at`，避免反复 claim；
- 已越过不可逆边界但平台是否接收无法确认时，adapter 返回 `DeliveryIndeterminate`；runtime
  只生成 `ActionFailed(delivery_indeterminate:*)`，不生成 `ActionSucceeded`/`MessageSent`；
- `DeliveryIndeterminate`、`ToolFailed`、non-retryable `ActionPortError`、
  `TargetUnavailable` 和 `DeliveryResolutionFailed` 都是 terminal
  non-success；本次到期 OpenLoop 必须调用 `defer(id, None, now)`，既不宣称 resolve，也不再
  进入 lease recovery 循环；retryable `ActionPortError` 仍走有界重试；
- runtime 关闭或容量丢弃是临时失败，使用有界延迟重试，不能按不支持 owner 处理。

以上是当前代码合同和自动测试覆盖的行为，不代表真实 QQ 投递、进程重启、lease recovery
和多日运行已经完成人工验收。

## 2.24 必须保留的人工验收

以下项目不能由单元测试、Fake Host 或本地空数据表替代，目前不得标为“已验收”：

| 项目 | 人工验收门槛 |
| --- | --- |
| 生产 Memory migration | 先备份，在生产数据上 dry-run/抽样核对数量、内容与 hash；选择受控批次执行 rollback，确认只删除该批次新写记录且 legacy 数据不变。 |
| 真实第二宿主 | 用非 Kovi、非 QQ 的真实 Host 导出/导入同一 versioned Person snapshot，核对稳定 `PersonId`、Memory、Affect、Relation、OpenLoop 和 Goal；`yunxi-cli` Fake Host 不计。 |
| 长期行为 | 跨天和跨进程重启观察时间衰减、主动冷却、到期投递、失败重试、Global 停止重占 lease，以及无重复/错投。 |
| QQ 到未来 App 连续性 | App Host 存在后，以同一用户完成 identity link，核对 QQ 历史数据在 App 中归属同一 Person，并验证后续双向更新；当前只能保留场景，不能宣称通过。 |

# 3. 最终产品定义

不要把芸汐理解成：

```text
QQ机器人
  +
LLM
```

而应该理解成：

```text
                    Yunxi Core
                 “芸汐本身”
                      │
       ┌──────────────┼──────────────┐
       │              │              │
       ▼              ▼              ▼
    Kovi Host      Desktop App    Mobile App
       │              │              │
       ▼              ▼              ▼
      QQ           GUI/Voice      GUI/Voice
```

甚至未来：

```text
                    Yunxi Core
                         │
      ┌────────────┬─────┼─────┬────────────┐
      ▼            ▼     ▼     ▼            ▼
     QQ          Desktop Web  Live2D      Game
```

各个平台只是：

> 芸汐观察世界和影响世界的不同“身体”。

---

# 4. 最高级架构原则

## 4.1 Core 不认识 QQ

`yunxi-core` 中禁止直接出现：

```rust
qq_number: i64
group_id: i64
RuntimeBot
PrivateMsgEvent
GroupMsgEvent
OneBot
Kovi
NapCat
```

这些全部属于：

```text
Platform Adapter
```

Core 只能认识：

```text
Person
Conversation
Message
WorldEvent
Intent
Action
Memory
Goal
```

---

# 5. 核心与平台关系

输入：

```text
QQ消息
   ↓
Kovi
   ↓
Kovi Adapter
   ↓
通用 WorldEvent
   ↓
Yunxi Core
```

未来 App：

```text
App文本输入
   ↓
App Adapter
   ↓
通用 WorldEvent
   ↓
Yunxi Core
```

输出：

```text
Yunxi Core
    ↓
Intent / Action
    ↓
Platform Adapter
```

QQ：

```text
SendMessage
↓
OneBot
```

App：

```text
SendMessage
↓
GUI message bubble
```

Mobile：

```text
ReachOut
↓
Push Notification
```

---

# 6. 关键产品要求

应该能够做到：

今天：

```text
用户通过 QQ 和芸汐聊天
```

几年以后：

```text
QQ 接口彻底关闭
```

然后用户启动：

```text
Yunxi Desktop
```

芸汐仍然拥有：

- 原来的长期记忆；
- 原来的用户身份；
- 原来的关系状态；
- 原来的 OpenLoop；
- 原来的未完成 Goal；
- 原来的情绪慢状态；
- 原来的人格；
- 原来的偏好。

从芸汐自己的认知角度：

> **不是换了一个 AI，而只是换了一个身体。**

---

# 7. Workspace 最终结构

长期建议：

```text
kovi-bot/
│
├── Cargo.toml
│
├── crates/
│   │
│   ├── yunxi-core/
│   │   └── src/
│   │       ├── cognitive/
│   │       ├── identity/
│   │       ├── memory/
│   │       ├── goals/
│   │       ├── affect/
│   │       ├── relation/
│   │       ├── ports/
│   │       └── lib.rs
│   │
│   ├── yunxi-storage-postgres/
│   │
│   ├── yunxi-model/
│   │
│   ├── yunxi-protocol/
│   │
│   └── yunxi-adapter-kovi/
│
├── apps/
│   ├── kovi-bot/
│   ├── yunxi-cli/
│   ├── yunxi-desktop/
│   └── yunxi-server/
│
└── plugins/
    └── model/       # 迁移期间保留
```

但：

> **不要第一轮就把整个仓库移动成这个样子。**

应渐进迁移。

---

# 8. 第一阶段实际物理结构

初期只强制增加：

```text
crates/
└── yunxi-core/
```

当前：

```text
plugins/model
```

继续工作。

然后：

```text
plugins/model
     │
     ▼
depends on
     │
     ▼
yunxi-core
```

随着迁移：

```text
plugins/model
```

逐步变薄。

---

# 9. Yunxi Core 的依赖边界

`yunxi-core`：

允许依赖：

- serde；
- chrono / time；
- uuid；
- tokio 中必要的 async primitives；
- thiserror / anyhow 中适合 library 的部分；
- 小型通用工具 crate。

原则上不得依赖：

- kovi；
- OneBot；
- NapCat；
- sqlx；
- PostgreSQL；
- Redis；
- GUI；
- Tauri；
- Android；
- QQ SDK；
- 特定 OpenAI HTTP client。

尤其建议：

> **Core 不直接依赖 SQL 数据库。**

---

# 10. Ports and Adapters

Core 通过 trait 使用外部世界。

例如：

```rust
pub trait MemoryStore {
    // ...
}

pub trait IdentityStore {
    // ...
}

pub trait OpenLoopStore {
    // ...
}

pub trait RelationStore {
    // ...
}

pub trait GoalStore {
    // ...
}

pub trait ModelBackend {
    // ...
}

pub trait ActionPort {
    // ...
}
```

具体实现：

```text
yunxi-storage-postgres
yunxi-model
yunxi-adapter-kovi
```

这样以后可以增加：

```text
yunxi-storage-sqlite
yunxi-adapter-desktop
```

无需修改 Core。

---

# 11. Core 必须可独立测试

必须能够：

```bash
cargo test -p yunxi-core
```

并且：

- 不需要 QQ；
- 不需要 NapCat；
- 不需要 Kovi；
- 不需要真实 PostgreSQL；
- 不需要真实模型 API；
- 不需要网络。

通过：

```text
FakeMemoryStore
FakeModel
FakeActionPort
```

测试完整 Cognitive Runtime。

这是平台解耦是否真正成功的重要验收标准。

---

# 12. 内部身份系统

这是本次架构调整最重要的内容之一。

当前大量业务使用：

```rust
user_id: i64
group_id: i64
```

作为领域 ID。

长期必须逐步废除。

---

# 13. PersonId

芸汐认识的是：

```rust
pub struct PersonId(Uuid);
```

而不是：

```text
QQ号
```

例如：

```text
PersonId:
550e8400-e29b-41d4-a716-446655440000
```

代表：

> 某个人。

---

# 14. ExternalIdentity

平台身份：

```rust
pub struct ExternalIdentity {
    pub platform: PlatformId,
    pub external_id: String,
}
```

例如：

```text
platform = "qq"
external_id = "123456789"
```

映射：

```text
QQ 123456789
       ↓
ExternalIdentity
       ↓
PersonId abc
```

未来：

```text
platform = "yunxi_app"
external_id = "owner"
```

也可以：

```text
↓
PersonId abc
```

于是两个平台身份属于：

> 同一个人。

---

# 15. 禁止自动合并身份

绝不允许根据：

```text
昵称相同
头像相同
名字相似
```

自动判断：

```text
这是同一个人。
```

跨平台身份关联必须：

- 显式配置；
- 用户认证；
- 管理员确认；
- 安全迁移流程。

---

# 16. Person 数据库

新增平台无关身份表。

建议：

```sql
CREATE TABLE yunxi_persons (
    id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

以及：

```sql
CREATE TABLE yunxi_external_identities (
    platform TEXT NOT NULL,
    external_id TEXT NOT NULL,
    person_id UUID NOT NULL
        REFERENCES yunxi_persons(id),

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    PRIMARY KEY (platform, external_id)
);
```

---

# 17. QQ Identity Resolver

当前 Kovi Adapter：

收到：

```text
user_id = 123456
```

执行：

```text
resolve_external_identity(
    platform="qq",
    external_id="123456"
)
```

得到：

```text
PersonId
```

再送给 Core。


# 18. ConversationId

同样：

Core 不认识：

```text
QQ群 98765
```

而认识：

```rust
pub struct ConversationId(Uuid);
```


# 19. ConversationKind

```rust
pub enum ConversationKind {
    Direct,
    Group,
    System,
}
```

以后可扩展：

```text
VoiceSession
GameSession
```

不要现在过度扩展。


# 20. Conversation 表

建议：

```sql
CREATE TABLE yunxi_conversations (
    id UUID PRIMARY KEY,

    kind TEXT NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

平台映射：

```sql
CREATE TABLE yunxi_external_conversations (
    platform TEXT NOT NULL,
    external_id TEXT NOT NULL,

    conversation_id UUID NOT NULL
        REFERENCES yunxi_conversations(id),

    PRIMARY KEY (platform, external_id)
);
```

例如：

```text
QQ group 98765
    ↓
ConversationId xyz
```


# 21. ConversationMember

建议：

```sql
CREATE TABLE yunxi_conversation_members (
    conversation_id UUID NOT NULL,
    person_id UUID NOT NULL,

    role TEXT,

    PRIMARY KEY (
        conversation_id,
        person_id
    )
);
```

但大型 QQ 群不要求实时同步所有成员。

可以：

> 按实际互动懒加载。

---

# 22. Core 的 Message 模型

定义内部：

```rust
pub struct Message {
    pub id: MessageId,

    pub conversation_id: ConversationId,

    pub sender: PersonId,

    pub content: MessageContent,

    pub timestamp: DateTime<Utc>,

    pub reply_to: Option<MessageId>,
}
```

---

# 23. MessageId

使用：

```rust
pub struct MessageId(Uuid);
```

不要 Core 中暴露：

```text
OneBot message_id: i32
```

---

# 24. 外部消息 ID

Adapter 保存：

```text
MessageId
↔
OneBot message_id
```

映射。

这样 Core 只说：

```text
ReplyTo(MessageId)
```

QQ Adapter 再翻译：

```text
OneBot message_id
```

---

# 25. WorldEvent

系统统一事件：

```rust
pub struct WorldEvent {
    pub id: EventId,

    pub occurred_at: DateTime<Utc>,

    pub scope: EventScope,

    pub priority: EventPriority,

    pub trace: TraceContext,

    pub kind: WorldEventKind,
}
```

---

# 26. EventScope

禁止使用：

```text
Private(user_id)
Group(group_id)
```

改成：

```rust
pub enum EventScope {
    Global,

    Conversation {
        conversation_id: ConversationId,
    },

    Person {
        person_id: PersonId,
    },

    Goal {
        goal_id: GoalId,
    },
}
```

---

# 27. WorldEventKind

至少：

```rust
pub enum WorldEventKind {
    MessageReceived(MessageReceivedEvent),

    MessageSent(MessageSentEvent),

    ToolCompleted(ToolCompletedEvent),

    ToolFailed(ToolFailedEvent),

    ReminderDue(ReminderDueEvent),

    GoalUpdated(GoalUpdatedEvent),

    GoalCompleted(GoalCompletedEvent),

    ProspectiveMemoryDue(
        ProspectiveMemoryEvent
    ),

    ActionSucceeded(
        ActionSucceededEvent
    ),

    ActionFailed(
        ActionFailedEvent
    ),

    IdleTick,

    MaintenanceTick,

    HostStarted,

    HostStopping,
}
```

未来：

```text
VoiceReceived
VisionObserved
LocationChanged
GameStateChanged
NotificationOpened
```

无需改架构。

---

# 28. Adapter 的唯一输入职责

例如 `KoviAdapter`：

```text
PrivateMsgEvent
↓
resolve PersonId
↓
resolve ConversationId
↓
normalize attachments
↓
create internal Message
↓
WorldEvent
```

Adapter 不负责：

```text
长期人格
情绪
长期动机
全局 Attention
OpenLoop
```

---

# 29. Environment Capability

不同平台能力不同。

QQ：

可能支持：

- send message；
- reply；
- @；
- recall；
- group member query。

Desktop：

可能支持：

- display text；
- notification；
- TTS；
- Live2D。

因此 Adapter 启动时应暴露：

```rust
pub struct EnvironmentCapabilities {
    pub actions: Vec<ActionDescriptor>,
}
```

---

# 30. Action 也必须平台无关

禁止 Core 输出：

```rust
SendQqPrivateMessage
SendQqGroupMessage
```

应该：

```rust
pub enum ProposedAction {
    SendMessage(SendMessageAction),

    ReachOut(ReachOutAction),

    UseTool(ToolAction),

    CreateOpenLoop(...),

    ResolveOpenLoop(...),

    StartGoal(...),

    CancelGoal(...),

    Noop,
}
```

---

# 31. SendMessage

```rust
pub struct SendMessageAction {
    pub conversation_id: ConversationId,

    pub content: MessageContent,

    pub reply_to: Option<MessageId>,
}
```

---

# 32. ReachOut

这个抽象尤其重要。

主动聊天真正表达的不是：

```text
给 QQ 123456 发消息
```

而是：

```rust
pub struct ReachOutAction {
    pub person_id: PersonId,

    pub message: MessageContent,

    pub motive: ReachOutMotive,
}
```

---

# 33. DeliveryResolver

平台层负责：

```text
PersonId
↓
当前有哪些可达渠道？
```

比如：

```text
QQ direct
Yunxi App
Desktop
```

然后根据策略选择。

---

# 34. 当前只有 QQ 时

```text
ReachOut(PersonId)
       ↓
resolve QQ identity
       ↓
QQ private conversation
       ↓
send
```

---

# 35. 未来只有 App 时

```text
ReachOut(PersonId)
       ↓
Yunxi App
       ↓
Push Notification / UI
```

Core 完全不变。

---

# 36. 多平台同时在线

以后可能：

```text
QQ
+
Desktop
+
Mobile
```

不能让芸汐同时：

```text
三个平台发同一句主动消息
```

需要：

```text
DeliveryPolicy
```

例如：

1. 当前活跃平台；
2. 用户指定首选平台；
3. 最近互动平台；
4. fallback。

该逻辑属于：

```text
Host / Delivery Router
```

不是 Persona。

---

# 37. Cognitive Runtime

Core 的核心：

```text
WORLD
  ↓
EVENT
  ↓
ATTENTION
  ↓
WORKING STATE
  ↓
MEMORY / AFFECT / RELATION
  ↓
DECISION
  ↓
INTENT
  ↓
ACTION
  ↓
ADAPTER
  ↓
WORLD
```

---

# 38. CognitiveRuntime

建议：

```rust
pub struct CognitiveRuntime {
    rx: Receiver<WorldEvent>,

    state: WorkingState,

    attention: AttentionSystem,

    planner: Planner,

    services: CoreServices,
}
```

注意：

这里不能有：

```rust
RuntimeBot
PgPool
```

---

# 39. CoreServices

例如：

```rust
pub struct CoreServices {
    pub memory: Arc<dyn MemoryStore>,

    pub identity: Arc<dyn IdentityStore>,

    pub open_loops: Arc<dyn OpenLoopStore>,

    pub relations: Arc<dyn RelationStore>,

    pub goals: Arc<dyn GoalStore>,

    pub model: Arc<dyn ModelBackend>,
}
```

具体 trait 设计由实际 Rust toolchain 决定。

---

# 40. Event Bus

继续使用：

```text
bounded mpsc
```

即可。

不要一开始引入：

- Kafka；
- RabbitMQ；
- actor framework。

---

# 41. Backpressure

队列必须 bounded。

Critical：

```text
等待容量
```

Low：

```text
try_send
```

队列满不能内存无限增长。

---

# 42. Attention

继续采用：

```text
高频感知
低频思考
```

大部分事件：

```text
Rust filter
```

结束。

只有重要事件：

```text
LLM
```

---

# 43. AttentionDisposition

```rust
pub enum AttentionDisposition {
    Ignore,
    ObserveOnly,
    Attend,
    MustHandle,
}
```

---

# 44. MustHandle

以下始终 MustHandle：

- 用户直接私聊；
- 用户明确叫芸汐；
- 回复芸汐；
- Stop；
- Reminder；
- 用户明确请求的操作；
- Goal completion；
- 必须交付的任务结果。

---

# 45. WorkingState

平台无关：

```rust
pub struct WorkingState {
    pub global: GlobalWorkingState,

    pub conversations:
        HashMap<
            ConversationId,
            ConversationWorkingState
        >,
}
```


# 46. ConversationWorkingState

```rust
pub struct ConversationWorkingState {
    pub current_topic: Option<String>,

    pub active_people: Vec<PersonId>,

    pub recent_events: VecDeque<CompactEvent>,

    pub last_message_at:
        Option<DateTime<Utc>>,

    pub last_bot_action_at:
        Option<DateTime<Utc>>,

    pub open_loops: Vec<OpenLoopId>,

    pub version: u64,
}
```

---

# 47. 所有 State 必须 bounded

例如：

```text
global events: 64
conversation events: 32
open loops refs: 32
active participants: 32
```

不能无限增长。

---

# 48. OpenLoop

继续作为非常重要的能力。

例如：

用户：

> 明天下午面试。

形成：

```text
OpenLoop:
等待面试结果
```

---

# 49. OpenLoop 不属于 QQ

必须：

```rust
pub struct OpenLoop {
    pub id: OpenLoopId,

    pub owner: OpenLoopOwner,

    pub kind: OpenLoopKind,

    pub summary: String,

    pub due_at: Option<DateTime<Utc>>,

    pub status: OpenLoopStatus,
}
```

---

# 50. OpenLoopOwner

```rust
pub enum OpenLoopOwner {
    Person(PersonId),

    Conversation(ConversationId),

    Global,
}
```

禁止：

```text
QQ user_id
```

---

# 51. OpenLoop Store

Core：

```text
OpenLoopStore trait
```

PostgreSQL：

```text
yunxi-storage-postgres
```

实现。

以后 Desktop offline 可以：

```text
SQLite implementation
```

---

# 52. OpenLoop PostgreSQL

建议：

```sql
CREATE TABLE yunxi_open_loops (
    id UUID PRIMARY KEY,

    owner_kind TEXT NOT NULL,

    owner_id UUID,

    kind TEXT NOT NULL,

    summary TEXT NOT NULL,

    source_message_id UUID,

    due_at TIMESTAMPTZ,

    expires_at TIMESTAMPTZ,

    salience SMALLINT NOT NULL,

    status TEXT NOT NULL,

    created_at TIMESTAMPTZ NOT NULL,

    updated_at TIMESTAMPTZ NOT NULL,

    resolved_at TIMESTAMPTZ
);
```

---

# 53. Reminder 和 OpenLoop 必须继续分开

Reminder：

> 用户要求系统必须做。

OpenLoop：

> 芸汐自己觉得以后值得想起。

---

# 54. Affect

也必须属于：

```text
Yunxi Core
```

而不是：

```text
QQ Bot mood
```

---

# 55. AffectState

```rust
pub struct AffectState {
    pub valence: f32,

    pub arousal: f32,

    pub social_energy: f32,

    pub curiosity: f32,
}
```

---

# 56. Relation

关系必须与：

```text
PersonId
```

绑定。

```rust
pub struct RelationState {
    pub person_id: PersonId,

    pub familiarity: f32,

    pub affinity: f32,

    pub trust: f32,

    pub comfort: f32,

    pub tension: f32,
}
```

---

# 57. 关系不能属于某个平台

错误：

```text
QQ用户 123 与芸汐关系 8/10
```

正确：

```text
Person abc 与芸汐关系
```

QQ 只是：

```text
Person abc 的一个外部身份。
```

---

# 58. Memory v2

这是长期必须解决的问题。

当前历史 Memory 很多依赖：

```text
subject_id: i64
```

这对未来 App 不够。

最终平台无关 Memory 应使用：

```rust
pub enum MemoryScope {
    Person(PersonId),

    Conversation(ConversationId),

    Global,
}
```

---

# 59. 不要立刻破坏旧 Memory

必须：

```text
legacy memory
+
memory v2
```

渐进迁移。

不能：

```text
ALTER 旧表然后一次把所有生产数据改掉
```

---

# 60. 新 Memory 表建议

长期：

```sql
CREATE TABLE yunxi_memories (
    id UUID PRIMARY KEY,

    scope_kind TEXT NOT NULL,

    scope_id UUID,

    memory_type TEXT NOT NULL,

    content TEXT NOT NULL,

    importance SMALLINT NOT NULL,

    tags JSONB NOT NULL,

    occurred_at TIMESTAMPTZ NOT NULL,

    created_at TIMESTAMPTZ NOT NULL
);
```

具体 schema 可根据当前数据库风格调整。

---

# 61. Legacy Memory Migration

需要单独 migration service。

例如：

```text
旧 private memory
subject_id = QQ USER
        ↓
qq identity map
        ↓
PersonId
        ↓
new Yunxi Memory
```

旧 group：

```text
subject_id = QQ GROUP
        ↓
external conversation map
        ↓
ConversationId
```

---

# 62. 双读策略

迁移期：

```text
new memory
   +
legacy memory fallback
```

生成 Context 时合并。

---

# 63. 双写策略

在稳定迁移阶段：

```text
写 new memory
+
保留必要 legacy write
```

等确认：

```text
所有 Host 都能读新 Memory
```

再逐步停止 legacy 写。

---

# 64. 不得自动删除旧 Memory

迁移成功也先保留。

需要：

- migration count；
- hash / comparison；
- manual validation。

---

# 65. Planner

Core Planner 输入必须平台无关。

不能：

```text
group_id=123
user_id=456
```

而应该：

```text
person
conversation
relation
memory
open loops
available capabilities
```

---

# 66. PlannerInput

```rust
pub struct PlannerInput {
    pub event: WorldEvent,

    pub state: PlannerStateSnapshot,

    pub memories: Vec<Memory>,

    pub open_loops: Vec<OpenLoop>,

    pub relation: Option<RelationState>,

    pub affect: AffectState,

    pub capabilities:
        Vec<ActionDescriptor>,
}
```

---

# 67. PlannerOutput

只输出：

```text
高层决策
```

不能直接副作用。

```rust
pub struct DecisionPlan {
    pub disposition: DecisionDisposition,

    pub intents: Vec<CognitiveIntent>,

    pub state_updates:
        Vec<StateUpdateProposal>,
}
```

---

# 68. Intent 与 Action 分开

推荐：

```text
Intent
=
芸汐想干什么
```

例如：

```text
想联系某个人
想回答消息
想查天气
想继续一个 Goal
```

Action：

```text
环境具体如何做
```

---

# 69. 示例

Core：

```text
Intent:
ReachOut(Person abc)
```

Host：

```text
QQ:
Send private message
```

未来 App：

```text
Push notification
```

---

# 70. Action Arbiter

必须存在。

负责：

- 权限；
- cooldown；
- stale；
- rate limit；
- scope；
- capability；
- delivery；
- dedupe。

---

# 71. 平台权限仍由 Adapter 强制

例如 QQ 跨群：

Core 可以表达：

```text
SendMessage(
    Conversation xyz
)
```

但 Kovi Adapter 必须再次判断：

```text
这个 Conversation 是否对应授权 QQ 群？
```

不能因为 Core 通过了就绕过。

---

# 72. Goal

Goal 也必须平台无关。

```rust
pub struct Goal {
    pub id: GoalId,

    pub owner: GoalOwner,

    pub kind: GoalKind,

    pub state: GoalState,
}
```

---

# 73. 当前 agent_tasks

当前跨群问答状态机不要重写。

第一阶段：

```text
agent_tasks
↓
Adapter/Bridge
↓
GoalUpdated WorldEvent
```

以后再泛化。

---

# 74. Proactive 行为

当前 DoD 要求主动意图进入平台无关边界：

```text
Core motive
```

而不是：

```text
QQ push
```

成熟的主动调度、画像/冷却和真实投递可以继续由 Kovi Host/Adapter 承担。把其中需要由多个
真实 Host 共享的平台无关选择策略迁入 Core，是长期可选演进，不是当前完成条件。

---

# 75. ProactiveMotive

```rust
pub enum ProactiveMotive {
    FollowUp,

    CheckIn,

    Share,

    React,

    Curiosity,
}
```

---

# 76. Proactive Candidate

目标是：

```text
PersonId
```

而不是：

```text
QQ user id
```

---

# 77. Proactive Flow

```text
IdleTick
↓
open loops
↓
memory
↓
relation
↓
affect
↓
candidate motives
↓
Attention
↓
Planner
↓
ReachOut(PersonId)
↓
DeliveryResolver
↓
Adapter
```

---

# 78. 主动行为失败

如果当前：

```text
没有任何可达平台
```

例如 QQ 下线、App 不在线：

不能把 Goal/Memory 丢掉。

可以：

```text
DeliveryUnavailable
↓
ActionFailed WorldEvent
```

之后 defer。

---

# 79. ModelBackend

Core 不应该硬编码：

```text
OpenAI Responses API
```

定义平台无关接口。

当前：

```text
ModelGateway
```

可以作为第一版 Adapter。

---

# 80. Tool

工具也不应该全部绑在 Kovi plugin。

长期：

```text
Core Tool Intent
↓
Tool Runtime
```

---

# 81. 环境专属 Tool

有些工具：

```text
group.members.search
```

明显属于 QQ。

因此需要：

```text
capability-based exposure
```

只有 Kovi Host 提供时：

Planner 才能看到。

---

# 82. Standalone App 架构

未来至少支持两种方式。

### 模式 A：本地嵌入

```text
Desktop App
     │
     └── Yunxi Core Library
```

### 模式 B：Core Server

```text
Yunxi Server
   │
   ├── Core
   └── PostgreSQL

Desktop/Mobile/Web
       ↓
 WebSocket / HTTP
```

当前架构不得排除任一种。

---

# 83. 推荐未来 Desktop

例如：

```text
Tauri
+
Rust Yunxi Core
```

但本 PRD 不要求现在开发 Tauri。

---

# 84. yunxi-protocol

长期如果使用 Server 模式，可以增加：

```text
crates/yunxi-protocol
```

定义：

- ClientEvent；
- ServerEvent；
- Message DTO；
- stream event；
- action update。

本阶段不强制。

---

# 85. CLI Host

强烈建议较早实现一个：

```text
yunxi-cli
```

原因不是产品价值。

而是：

> **证明 Core 真的已经脱离 QQ。**

---

# 86. CLI 验收

执行：

```bash
cargo run -p yunxi-cli
```

然后：

```text
You: 你好
Yunxi: ...
```

它不使用：

- Kovi；
- OneBot；
- QQ。

如果 CLI 能使用：

- Core；
- Memory；
- Affect；
- Relation；
- OpenLoop；
- Planner；

说明 Core 可以在 Fake Host 中脱离 QQ 运行，是依赖边界的自动验收。它不能证明生产数据
可迁移、真实第二宿主可用，或 QQ 到未来 App 的用户连续性；这些仍按 2.24 人工验收。

CLI 可通过环境变量启用 host-owned Core state 和独立的 durable turn journal：

```bash
YUNXI_CLI_STATE=/path/to/yunxi-cli-state.json \
YUNXI_CLI_JOURNAL=/path/to/yunxi-cli-turns.jsonl \
cargo run -p yunxi-cli
```

`YUNXI_CLI_STATE` 使用单个有界 JSON snapshot，持久化稳定 Person/Conversation ID 以及
Memory、Affect、Relation、OpenLoop；反序列化时会重新检查领域约束、总量和 scope/owner
边界。FakeModel 会读取这些恢复后的 context，并通过 Core proposal 更新 Affect/Relation。
JSONL journal 则在 Core 处理前同步记录输入，在处理后记录完成或失败，并能忽略崩溃留下的
最后半行；两者可以单独启用，且都属于 host，不给 Core crate 增加文件系统依赖。

---

# 87. Phase 0：建立 Core crate

新增：

```text
crates/yunxi-core/
```

第一轮只放：

```text
identity
event
working_state
attention
runtime skeleton
ports
```

不改变生产行为。

---

# 88. Phase 0 关键验收

运行：

```bash
cargo tree -p yunxi-core
```

不得出现：

```text
kovi
```

最好也不出现：

```text
sqlx
```

---

# 89. Phase 1：Identity

实现：

- PersonId；
- ConversationId；
- MessageId；
- ExternalIdentity；
- ExternalConversation；
- IdentityStore。

PostgreSQL 增加：

```text
yunxi_persons
yunxi_external_identities
yunxi_conversations
yunxi_external_conversations
```

---

# 90. Phase 1 Kovi Bridge

Kovi message：

```text
QQ user
↓
PersonId
```

QQ group/private：

```text
↓
ConversationId
```

但原业务逻辑仍继续使用旧：

```text
i64
```

暂时允许：

```text
old ID
+
new ID
```

同时存在。

---

# 91. Phase 2：WorldEvent Shadow Runtime

Kovi 消息额外发送：

```text
generic WorldEvent
```

进入 Core。

Core：

```text
Observe
↓
Attention
↓
WorkingState
↓
log
```

不产生真实副作用。

---

# 92. Phase 3：OpenLoop

OpenLoop 必须从第一天就使用：

```text
PersonId / ConversationId
```

不要再做一套：

```text
QQ subject_id
```

版本。

---

# 93. Phase 4：Memory Bridge

新增：

```text
MemoryStore port
```

第一版实现可以包装：

```text
现有 MemoryManager
```

这样不需要马上重写全部 memory。

---

# 94. Phase 5：Proactive

当前 DoD 在 Core 建立主动意图边界：

```text
QQ-oriented proactive
```

到：

```text
Core proactive motive
```

实际发送仍由 Kovi Adapter。

现有 Kovi 主动调度、画像和冷却策略可作为 Host 策略保留。把其中与平台无关、需要被多个
真实 Host 共享的选择规则迁入 Core，是长期可选演进，不阻塞本 Phase 的当前范围完成。

---

# 95. Phase 6：Intent / Action

增加：

```text
CognitiveIntent
ProposedAction
ActionArbiter
DeliveryResolver
```

Core 不执行 Kovi API。


# 96. Phase 7：Direct Conversation

逐步让：

```text
MessageReceived
```

成为直接聊天的统一入口。

但是：

当前成熟：

- coalescing；
- ReplyTicket；
- Vision；
- sticker；
- queue；

暂时仍可以留在 Kovi Adapter / host。

---

# 97. 长期这些功能归属

推荐：

### Platform Adapter

- OneBot parsing；
- QQ message segments；
- group member query；
- external message ID；
- recall implementation；
- @ implementation。

### Yunxi Core

- conversation relevance；
- attention；
- memory；
- relation；
- affect；
- motivation；
- planner；
- goal。

---

# 98. Phase 8：Affect

长期目标是把可跨宿主复用的 affect 领域规则从 QQ 插件搬入 Core；这不要求当前把所有
global mood 兼容策略都搬走。

当前已完成 Core state/port、PostgreSQL store、`InteractionCues` 和读取时的确定性时间衰减。
legacy `MessageUnderstanding` 的 mood/confidence/gratitude 会归一化后 best-effort 投射；Core
直聊可复用同一次模型回复的受限 cue sidecar。旧 global `BotPersonality`/mood 策略可继续
作为 Host 兼容策略；进一步迁移是长期可选项，不是当前 DoD 缺口。本 Phase 的“部分完成”
指跨天、跨重启和真实第二宿主的长期行为尚待人工验收。

---

# 99. Phase 9：Relation

将关系状态与：

```text
PersonId
```

绑定。

当前 Relation 已绑定 canonical `PersonId`，接受有界 gratitude/sentiment cue，并在 PostgreSQL
读取时按 `updated_at` 做确定性衰减。仍需验证跨天、重启和真实第二宿主中的长期行为。

---

# 100. Phase 10：Memory v2

实现真正：

```text
platform-independent memory
```

迁移旧 QQ memory。

---

# 101. Phase 11：Goal Event Integration

Reminder / agent_tasks / tools：

逐步转成：

```text
generic WorldEvent
```

---

# 102. Phase 12：CLI Host

实现一个最小：

```text
apps/yunxi-cli
```

作为离线平台边界验收；当前 host 提供可选 `YUNXI_CLI_STATE` Core store snapshot 和
`YUNXI_CLI_JOURNAL` durable turn journal，二者均保持在 Core crate 边界之外。
它不是最终的真实第二宿主迁移验收。

---

# 103. Phase 13：App 预留

这一阶段不要求开发完整 App。

Desktop/Mobile/Web/App Host、协议、认证、同步、通知和 UI 均为未来 / 当前非目标；本阶段只
检查通用 API 没有把这些可能性封死。

但 Core API 必须已经可以：

```text
App input
↓
WorldEvent
↓
Core
↓
Action
↓
App output
```

---

# 104. App 所需未来事件

本节仅是未来扩展词汇表，不是当前已实现 API 或交付清单。

架构要允许：

```text
AppForegrounded
AppBackgrounded

VoiceReceived
VisionObservation
NotificationOpened
```

但现在不要实现。

---

# 105. App 所需未来 Action

本节仅是未来扩展词汇表，不是当前已实现 API 或交付清单。

架构要允许：

```text
Speak
ShowExpression
SendNotification
PlayAnimation
OpenPanel
```

但现在不要写死。

---

# 106. 数据可携带性

PostgreSQL adapter 当前已经可以导出、导入 versioned Person snapshot：

```text
external identities
+ Person memories
+ relation / affect
+ Person-owned open loops / goals
```

导出在一致性只读事务内完成，并以“上限 + 1”探测所有有界集合；超限时拒绝生成残缺
snapshot。导入对全部 Core state、scope 与 owner 做验证，在同一事务内检测 external identity
归属和 Memory/OpenLoop/Goal ID 冲突，任何异内容或跨 Person 冲突都会回滚。新增字段采用
`serde(default)`，兼容缺少这些字段的旧 version 1 JSON。真实跨平台 host 携带性仍需单独
验收；生产数据抽样、生产 rollback 演练也仍是人工门槛，不能因 adapter API 和本地测试存在
而标为已通过。

QQ-specific metadata 可作为：

```text
optional external identity metadata
```

而不能成为主数据。

---

# 107. 数据删除

用户删除数据时必须删除：

- Person memories；
- relation；
- private conversation data；
- open loops；
- goals；
- linked external identities（视策略）；

当前 Kovi Host 的彻底删除按以下安全合同执行：

- Person import、direct route 创建和 Person 删除统一使用
  `Person -> external route -> Conversation` advisory-lock 顺序；
- Person 删除以 `ConversationMember` 为 canonical 枚举源，覆盖跨平台 direct
  conversation，保留共享 Group；即使 external identity mapping 已不存在，也不得删除已属于
  其他 Person 的 direct route；
- Group/Conversation 删除先通过 Core FIFO erasure command 阻断并清空
  `WorkingState`，再清 reference/route cache，并以 group handler epoch/write gate 排空 legacy
  handler；PostgreSQL 事务持 Conversation owner lock，原子删除 group domain、canonical
  Conversation 和 attributable delivery ledger；
- 数据库成功或失败都必须结束 erasure window；若 barrier cleanup 本身失败则保持 blocked，
  不能重新放行旧 scope。成功/失败 receipt 均走 tracked-but-unrecorded 发送，晚到结果不得重建
  recall 或 ledger。

不能只删除：

```text
QQ subject_id
```

---

# 108. Identity unlink

未来用户可能要求：

```text
解绑 QQ
```

应该：

```text
删除 ExternalIdentity mapping
```

但：

```text
Person
Memory
Relation
```

不一定删除。

这就是身份与人格数据分离的价值。

---

# 109. Identity delete

而：

```text
彻底删除我的数据
```

才删除：

```text
Person domain data
```

---

# 110. 数据迁移必须可回滚

所有新表：

```text
additive
```

禁止：

```text
直接 drop legacy table
```

---

# 111. 不要一次“大搬家”

尤其禁止 Codex 第一轮：

```text
把 plugins/model 全部移动到 crates/yunxi-core
```

这种 massive diff。

正确：

```text
抽象
→ Bridge
→ 迁移一个模块
→ 测试
→ 下一个模块
```

---

# 112. Commit 原则

每个阶段独立 commit。

例如：

```text
feat(core): add platform-neutral identity model

feat(core): add world events and bounded runtime

feat(kovi): bridge qq identities into yunxi core

feat(core): add prospective open loops

feat(core): add proactive reach-out intents
```

---

# 113. Platform Boundary Test

建议专门写一个 lint/test。

例如 CI：

```text
grep / cargo tree
```

保证：

```text
yunxi-core
```

不能依赖：

```text
kovi
```

---

# 114. 代码级禁止项

在：

```text
crates/yunxi-core
```

禁止：

```rust
use kovi::*;
```

禁止：

```rust
pub qq_user_id: i64
```

禁止：

```rust
pub qq_group_id: i64
```

禁止：

```rust
send_group_msg()
```

禁止：

```rust
PgPool
```

---

# 115. 可接受的外部 ID

Core 某些诊断 DTO 可以存在：

```rust
pub struct ExternalReference {
    pub provider: String,
    pub opaque_id: String,
}
```

但只能：

> opaque。

Core 不允许解释它。

---

# 116. PersonId 不暴露平台

不要：

```text
person_id = "qq:123456"
```

应该使用真正平台无关 UUID。


# 117. ConversationId 同理

不要：

```text
conversation_id = "qq-group-9988"
```

---

# 118. Owner 身份

当前主管理员最终应该映射到：

```text
canonical owner PersonId
```

例如配置可以：

```toml
[identity]
owner_person_id = "..."
```

当前：

```text
main_admin QQ
```

作为：

```text
owner Person 的 QQ ExternalIdentity。
```

当前迁移实现已提供：

```toml
[identity]
owner_person_id = "<canonical PersonId UUID>"
```

配置后 Core Tool、主动聊天、管理员入口和故障通知优先使用该 canonical `PersonId`
对应的唯一 QQ 路由；配置了 owner 但没有唯一 QQ 路由时 fail-closed。未配置时才回退到
`main_admin` 的 QQ 映射。`main_admin` 仅作为兼容字段保留。

---

# 119. 未来 App Owner

Yunxi App 登录后：

```text
app owner identity
```

也映射到同一个：

```text
owner PersonId
```

于是关系和记忆连续。

---

# 120. Persona 本身

Persona 属于：

```text
Yunxi Core global state
```

绝对不能属于：

```text
QQ plugin config
```

配置最终应逐渐迁移：

```text
Yunxi persona config
```

---

# 121. 身份透明性

芸汐是：

> 一个 AI 驱动的虚拟角色。

不能把“是真实人类”写成 Core 约束。

平台不同，也不能改变这一点。

---

# 122. 最终认知模型

芸汐不是：

```text
一个 QQ Bot
```

而是：

```text
                  ┌──────────────┐
                  │   Identity   │
                  └──────┬───────┘
                         │
                  ┌──────▼───────┐
                  │    Memory    │
                  └──────┬───────┘
                         │
       ┌─────────────┬───┴────┬─────────────┐
       ▼             ▼        ▼             ▼
    Affect        Relation   Goals       OpenLoops
       │             │        │             │
       └─────────────┴───┬────┴─────────────┘
                         ▼
                   WorkingState
                         │
                         ▼
                     Attention
                         │
                         ▼
                      Planner
                         │
                         ▼
                       Intent
                         │
                         ▼
                       Action
                         │
                         ▼
                 Platform Adapter
```

---

# 123. 最终宿主模型

```text
                     YUNXI CORE

                          │
        ┌─────────────────┼─────────────────┐
        │                 │                 │
        ▼                 ▼                 ▼
 Kovi Adapter       Desktop Adapter    Mobile Adapter
        │                 │                 │
        ▼                 ▼                 ▼
       QQ                GUI            Notification
```

---

# 124. 最终最重要的边界

下面这一句话作为整个项目的架构铁律：

> **Yunxi Core 决定芸汐“想做什么”；Platform Adapter 决定这个环境里“具体怎么做”。**

例如：

Core：

```text
我想联系这个人。
```

QQ Adapter：

```text
发 QQ 私聊。
```

Desktop：

```text
弹出消息。
```

Mobile：

```text
发通知。
```

这三者对 Core 来说：

> 是同一个 Intent。

---

# 125. 第一轮 Codex 实施范围

第一轮不要实现整个 PRD。

只实现：

```text
crates/yunxi-core

identity/
event/
working_state/
attention/
runtime/
ports/
```

加：

```text
Identity mapping PostgreSQL tables
```

再给现有 Kovi Plugin 做一个：

```text
Bridge
```

将收到的 QQ 用户和群：

```text
QQ ID
↓
PersonId / ConversationId
↓
WorldEvent
```

送入：

```text
shadow Yunxi Core
```

---

# 126. 第一轮严格禁止

第一轮不得：

- 改现有回复行为；
- 改主动聊天行为；
- 删除旧 Memory；
- 改 agent_tasks 状态机；
- 改 Reminder 可靠执行；
- 搬整个 plugin；
- 开发 App；
- 开发 TTS；
- 开发 Live2D。

---

# 127. 第一轮完成标准

必须能够证明：

```text
QQ PrivateMsgEvent
      ↓
Kovi Adapter/Bridge
      ↓
PersonId
ConversationId
      ↓
WorldEvent
      ↓
Yunxi Core
      ↓
Attention
      ↓
WorkingState
```

同时：

```text
旧 QQ Bot 行为完全正常。
```

---

# 128. 第二轮

实现：

```text
OpenLoop
```

必须使用新的：

```text
PersonId / ConversationId
```

绝不允许再绑定 QQ `i64`。

---

# 129. 第三轮

主动行为：

```text
ProactiveMotive
↓
ReachOut(PersonId)
```

然后 Kovi Adapter：

```text
PersonId
↓
QQ
```

真正发送。

---

# 130. 第四轮

实现 Action / Delivery Router。

---

# 131. 第五轮

实现：

```text
Affect
Relation
```

全部 Core 化。

---

# 132. 第六轮

Memory v2。

迁移 legacy QQ memory。

---

# 133. 第七轮

增加：

```text
yunxi-cli
```

作为 Core 可脱离 QQ 运行的依赖边界证明；真实第二宿主仍需人工验收。

---

# 134. 目标 Definition of Done（不是当前完成状态）

平台无关架构完成必须满足：

## Core

`yunxi-core`：

- 无 Kovi dependency；
- 无 OneBot dependency；
- 无 QQ domain ID；
- 无 PostgreSQL dependency；
- 可 Fake Store 测试；
- 可 Fake Model 测试；
- 可 Fake Environment 测试。

## Identity

存在：

```text
PersonId
ConversationId
MessageId
ExternalIdentity
```

QQ 只作为 external identity。

## Cognitive

存在：

```text
WorldEvent
Attention
WorkingState
OpenLoop
Affect
Relation
Goal
Planner
Intent
Action
```

## Host

Kovi：

```text
QQ
↔
Yunxi domain
```

当前 DoD 中，Kovi 负责 QQ 与 Yunxi domain 的 Adapter 集成，也可保留成熟的主动调度、
画像/冷却和 global mood 兼容策略；平台无关的身份、状态、意图和 Action 合同必须留在 Core。
把可跨宿主复用的 Host 策略进一步迁入 Core 是长期可选目标，不是当前完成条件。

## Portability

至少有：

```text
yunxi-cli
```

可以在没有：

```text
Kovi
OneBot
NapCat
QQ
```

的情况下运行 Yunxi Core。这只构成 Core 依赖边界门槛；产品级 portability 还必须通过
2.24 的真实第二宿主和数据连续性人工验收。

---

# 135. 未来产品人工验收场景（当前未完成）

以下是 QQ 到未来 App 连续性的最终人工场景，不是当前实现事实。只有真实 App Host、identity
link 和数据迁移均存在后才能执行并判定通过。

今天：

```text
QQ：

用户：
我明天下午面试。

芸汐：
那今晚早点休息呀……
```

Core：

```text
PersonId A
↓
OpenLoop:
等待面试结果
```

明天：

```text
ReachOut(Person A)
```

Kovi：

```text
→ QQ
```

几年后 QQ 不存在。

同一个：

```text
OpenLoop
Relation
Memory
Person A
```

App：

```text
→ 手机通知
```

> “对了……那个面试后来怎么样啦？”

这时才说明：

> **芸汐不是 QQ 机器人，而是真正拥有跨平台连续性的 Agent。**

---

# 136. 给 Codex 的最高优先级说明

将下面这段放在任务提示词最前面：

> 本文档取代此前的 Cognitive Runtime PRD。新的最高架构目标是平台无关性。
>
> `kovi-bot` 只是 Yunxi Core 的当前宿主，不是最终领域边界。
>
> 所有新 Cognitive Runtime 领域类型必须避免直接依赖 Kovi、OneBot、QQ、PostgreSQL 或具体 UI。
>
> 平台消息必须先由 Adapter 转换为 `PersonId`、`ConversationId`、`MessageId` 和通用 `WorldEvent`。
>
> Yunxi Core 只能生成平台无关的 Intent / Action；Kovi Adapter 负责将其转换成 QQ / OneBot 操作。
>
> 不得为了快速完成而在新的 Core 类型中继续使用 QQ user_id/group_id 作为领域主键。
>
> 当前成熟代码必须渐进迁移，不允许 massive rewrite。
>
> 如果“短期开发方便”和“未来 Core 可脱离 QQ”发生冲突，应优先选择清晰的平台边界，但同时必须保证现有生产功能不回归。

---

# 137. Codex 无人值守实施规则

## 137.1 总原则

这是一次允许长时间无人值守执行的开发任务。在没有真正阻塞的情况下：

- 不要等待用户确认；
- 不要反复询问是否继续；
- 不要因为任务较大而只输出建议；
- 需要实际阅读代码、修改、测试、Review、修复并逐阶段提交。

正确优先级：

```text
correctness
> safety
> compatibility
> maintainability
> amount of completed code
```

不要为了“全部完成”牺牲正确性。

---

## 137.2 开始编码之前

先阅读至少：

```text
README.md
Cargo.toml
AGENTS.md（若存在）
CONTRIBUTING.md（若存在）
.github/workflows/*
plugins/model/src/lib.rs
plugins/model/src/model/*
plugins/model/src/memory/*
plugins/model/src/proactive_chat/*
plugins/model/src/agent_runtime.rs
plugins/model/src/agent_tasks.rs
plugins/model/src/reminders.rs
plugins/model/src/mood_system/*
plugins/model/src/config/*
相关 tests
```

理解当前真实架构后再编码。

PRD 中伪代码是设计目标，不要求机械照抄。

如果现有代码已经有更合适的可靠抽象，应复用。

---

## 137.3 Git 安全

开始前检查：

```bash
git status
```

若存在用户已有未提交修改：

- 不覆盖；
- 不删除；
- 不 reset；
- 不 force checkout；
- 尽量绕开；
- 若无法安全继续，则停止并记录具体原因。

允许创建本任务 commit。

禁止：

- push；
- merge main；
- force reset；
- production deploy。

---

## 137.4 基线验证

编码前至少尝试：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace
```

以及仓库已有 PostgreSQL integration tests / CI 等价检查。

如果基线已有失败：

- 记录；
- 判断是否与当前任务无关；
- 不要为“变绿”随意改不相关代码；
- 后续必须确保不新增额外回归。

---

## 137.5 每个 Phase 的固定流程

严格：

```text
READ
→ PLAN
→ IMPLEMENT
→ FORMAT
→ TEST
→ REVIEW
→ FIX
→ RETEST
→ COMMIT
→ NEXT PHASE
```

每个 Phase 独立 commit。

建议 commit：

```text
feat(core): add platform-neutral identity model
feat(core): add world events and bounded runtime
feat(kovi): bridge qq identities into yunxi core
feat(core): add prospective open loops
feat(core): add proactive reach-out intents
```

---

## 137.6 每阶段测试

至少运行适用的：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace
```

涉及 PostgreSQL 时必须运行对应 integration tests。

不得：

- 删除测试来通过 CI；
- 降低断言严格度掩盖问题；
- 无理由大量使用 `#[allow(...)]`；
- 修改 CI 以跳过失败测试。

---

## 137.7 Review 要求

每个 Phase 完成后，在 commit 前必须检查：

- race condition；
- lock across await；
- stale decision；
- unbounded channel / Vec / HashMap；
- DB full scan；
- missing index；
- scope leak；
- cross-user data leak；
- incorrect identity mapping；
- permission bypass；
- duplicate side effects；
- restart replay；
- retry loop；
- panic；
- blocking IO in async；
- model-call multiplication；
- broken cooldown；
- broken ReplyTicket；
- destructive migration。

发现问题先修，再重新测试。

---

## 137.8 自动停止条件

无人值守不等于无条件继续。

遇到以下情况必须停止后续 Phase：

1. 当前 Phase 在合理修复后仍无法通过测试；
2. 需要 destructive database migration；
3. 需要生产 Secret / production DB / deploy 权限；
4. 用户未提交修改导致无法安全继续；
5. 必须大规模重写成熟模块才能继续；
6. 基础架构与现有代码严重冲突，需要重新设计；
7. 持续编译失败且无法确定根因；
8. 存在跨用户数据泄漏、权限越权、发错消息对象等高风险；
9. 出现明显“修一个坏两个”的退化。

停止时：

- 保留已经验证通过的 Phase commits；
- 不回滚独立且已通过的阶段；
- 不提交失败中的半成品 Phase，除非明确 WIP 且确有必要；
- 最终报告阻塞原因。

---

# 138. 非目标 / 当前阶段禁止事项

当前架构重构阶段不要优先实现：

- Desktop / Mobile / Web / Server 产品 Host、协议层和 UI；
- App 登录、跨设备同步、推送通知产品链路；
- Live2D；
- TTS；
- STT；
- 游戏控制；
- 桌面控制；
- 摄像头；
- 持续截图；
- 强化学习；
- 自训练模型；
- 大型 actor framework；
- Kafka；
- RabbitMQ；
- pgvector 强制迁移；
- Tauri 产品开发；
- Mobile App 产品开发。

上述项目均是未来方向，不是当前 Phase 0～12 的缺陷或完成阻塞项。App 事件/Action 名称和
架构图只表达可扩展性，不构成实现承诺。当前阶段也不以高级能力演示替代生产 Memory、真实
第二宿主、长期行为和跨平台连续性的人工验收。

这些未来都应该成为：

```text
Sensor
或
Effector / Adapter
```

而不是修改 Yunxi Core 的基础认知模型。

---

# 139. 最终实施哲学

不要把芸汐实现成：

```text
收到消息
↓
调用大模型
↓
回复
↓
结束
```

而要逐步实现：

```text
芸汐持续存在
        │
        ├── 世界不断产生事件
        ├── 她只能注意其中少数
        ├── 她记得重要的事情
        ├── 她会忘记不重要的事情
        ├── 她有当前情绪
        ├── 她与不同人的关系不同
        ├── 她有没做完的事情
        ├── 她会在未来重新想起一些事情
        ├── 她可以选择沉默
        ├── 她可以主动说话
        ├── 她可以使用工具
        └── 她会看到自己行为造成的新结果
```

最终闭环：

```text
WORLD
  ↓
EVENT
  ↓
ATTENTION
  ↓
WORKING STATE
  ↓
MEMORY / AFFECT / RELATION / GOALS / OPEN LOOPS
  ↓
DECISION
  ↓
INTENT
  ↓
ACTION
  ↓
PLATFORM ADAPTER
  ↓
WORLD
```

---

# 140. 一句话架构铁律

> **Yunxi Core 决定芸汐想做什么；Platform Adapter 决定这个环境里具体怎么做。**

如果一个新的认知领域类型必须知道“QQ 号”“QQ群号”“Kovi RuntimeBot”才能工作，那么这个抽象大概率放错了层。


# 141. Conversation Concurrency v1：双向同时发言与发送前重校验

本架构增加跨层规范：`yunxi-conversation-concurrency-v1-optimization-spec.md`。

V1 Core 必须为普通自然语言输出维护：

```text
Thinking
  ├→ Cancelled
  └→ Prepared
        ├→ Cancelled
        ├→ Superseded
        └→ Committed
              ├→ Sent
              ├→ Cancelled（明确拒绝）
              └→ Unknown（仅在 Committed 后，平台接收不确定）
```

**生成完成不等于允许发送。** `Prepared` 消息在进入 `Committed` 前必须基于最新
`ReplyTicket`、`scope_epoch`、`conversation_version`、Stop 状态、目标有效性和权限执行
pre-commit revalidation。数据删除会更换 scope epoch，旧 epoch 的晚到发送完成不得重建
recall 或其他已删除数据。

入站发现现有 `Prepared` 时先建立单槽、带 lease 的 pending-inbound reservation，冻结 commit
但不持会话锁等待模型；`Keep` 释放原 token，`Rewrite / Merge / Defer` 以及缺失或非法语义结果
才 fail-closed 终结它。没有现有 `Prepared` 时，新入站 generation 在 handler active 前也必须
保留 reservation，防止主动 fallback 抢占已经进入异步队列的用户消息。发送方只可在等待
reservation 完成后、尚未取得 route/authorization guard 时领取 bounded precommit permit；permit
之后到达的新入站直接使 token stale，因此最终 commit 不会持安全 guard 等待语义模型。

同一次语义模型调用会收到与 exact frozen ticket/reservation 绑定、最多 4096 字符的 Prepared
正文预览；它只作为非可信 JSON Data，reservation 或 token 不匹配时不得读取其他 envelope。
需要直接回复的新事件若被分类为 `Unrelated`，proactive Prepared 使用 `Defer`，reactive
Prepared 使用 `Rewrite`；只有无需回复的观察才允许 `Keep`。runtime 关闭或容量拒绝 collision
提交时，当前项与未提交尾部按原序恢复；scope 已被删除时不得重建。Core 返回
`RejectedState` 时必须释放 exact `MessageId` admission reservation。

新用户事件在 `Committed` 前可以使待发送消息：`Keep / Cancel / Rewrite / Merge / Defer`。
`Committed` 是副作用 Point of No Return；之后默认不强制撤回。明确拒绝可进入 `Cancelled`，
平台是否接收无法确认或发送 future 丢失必须进入 `Unknown`，并继续阻止重复 key 和异 envelope
碰撞。若双方已经分别进入不可撤销边界并近乎同时发言，应允许自然消息碰撞，并产生
`WorldEvent::MessageCollisionDetected`。

`ConversationCoordinator` 负责 generation、ReplyTicket、conversation version、PendingOutgoing、commit serialization、stale detection、cancellation propagation 与 collision detection。语义上是否 `Keep / Rewrite / Merge / Defer` 交由 Executive。

主动消息可有 300–1000ms、可配置且可关闭的短 `Prepared` grace window，仅用于捕获“刚准备主动发言时用户恰好发来消息”，禁止用固定长延迟伪装真人打字。

同一 Conversation 的真实 send commit 必须串行化，但禁止持锁等待 LLM 或平台网络。
Core `SendMessage`/`ReachOut` 还必须在网络调用前写入 PostgreSQL delivery ledger；仅明确
`Failed` 且 envelope 完全相同的记录允许重试。

delivery ledger 的 `Unknown` 只能由 `Committed` 转入，不能从 `Prepared` 产生。对于到期
OpenLoop，`DeliveryIndeterminate`、`ToolFailed`、non-retryable `ActionPortError`、
`TargetUnavailable` 和 `DeliveryResolutionFailed` 都属于 terminal non-success，必须
`defer(id, None, now)`；retryable `ActionPortError` 才继续有界重试。

`UseTool` 不使用消息 delivery ledger，但不能脱离原 turn：runtime 为普通事件 action 写入
`event:{EventId}:intent:{index}`，到期 action 继续使用 open-loop key；Host 以该 key 和
scope/name/input 指纹一次性领取原 `ReplyTicket`，重复、伪造、丢失或淘汰的 capability 均
fail-closed。每个副作用 builtin 与 MCP 在实际 dispatch 前重新验证 ticket、canonical actor
route、conversation membership/route、管理员、群授权、暂停状态与当前工具可用性；guard 在
外部 I/O 前释放。

验收标准：

> 在 `Committed` 之前，新事件可以阻止明显过时的待发送内容；进入 `Committed` 后允许自然碰撞，并把碰撞本身作为新的 WorldEvent。
