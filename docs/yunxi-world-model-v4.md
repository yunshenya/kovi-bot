# Yunxi World Model v4：世界状态、因果建模、预测与反事实模拟开发文档

**文档状态：** 最终整合版  

**文档版本：** 4.0  
**适用项目：** Yunxi Core / Yunxi Mind / Yunxi Executive  
**主要语言：** Rust  
**前置依赖：** Yunxi Core v1、Yunxi Mind v2、Yunxi Executive v3 已基本稳定  
**文档定位：** 为 Yunxi 增加平台无关的外部世界模型（World Model），使系统能够维护“当前世界可能是什么状态”的结构化估计，区分事实、观察、假设与未知；在高价值决策中对候选行为进行有限预测与反事实模拟，并将结果交给 Executive 层进行仲裁。

---

# 0. 文档导览与实现状态

> **定位**：本文档是 Yunxi World Model v4 的**权威设计蓝图**（266+ 小节，含 Phase 0–16 建设计划、分组目录与行为场景）。**它描述的是目标状态**——当前代码只实现了其中最小的一层（见 §0.1），其余实现路径按 §175 的 Phase 顺序推进。阅读时请先看本节，避免把“设计”误读为“已上线”。

## 0.1 实现状态对照（截至当前代码）

| 设计块 | 状态 | 对应代码 |
|---|---|---|
| **世界“事实”最小闭环**（Observation 的极简近似：scope/summary/importance → `Fact` 记忆 + watch 开环 + 可选 Mind interest） | ✅ 已实现 | `crates/yunxi-core/src/memory.rs::world_fact_draft`、`MemoryKind::Fact`；`plugins/model/src/yunxi/mod.rs::observe_world_fact`、`world_loop_draft` |
| **世界采样/传感器**（`url_status`、`command` 两种 kind，状态变化才回喂核心） | ✅ 已实现 | `plugins/model/src/world_sensors.rs`、`config/world_sensors.rs`；`agent_runs.rs` 完成时回喂 |
| **Observation 结构化**（§10–14：ObservationSource / Reliability / TTL / 数据结构） | 🔶 部分（仅以 Fact 记忆近似；无结构化字段、可靠性、TTL） | 未实现结构化 Observation |
| **Entity Model**（§15–20） | ⬜ 仅设计，未实现 | — |
| **Situation Model**（§21–26） | ⬜ 仅设计，未实现 | — |
| **Hypothesis**（§27–33） | ⬜ 仅设计，未实现 | — |
| **Temporal / Causal / Prediction**（§34–52） | ⬜ 仅设计，未实现 | — |
| **Simulation / Counterfactual / Snapshot**（§53–67） | ⬜ 仅设计，未实现 | — |
| **SocialScene / Environment / Uncertainty**（§68–83） | ⬜ 仅设计，未实现 | — |
| **Update Pipeline / Merge / Extraction**（§84–92） | ⬜ 仅设计，未实现 | — |
| **WorldModel Store Ports / Persistence / 表结构**（§125–131，含 `yunxi_world_observations`） | ⬜ 设计表**尚未创建**（代码中无该表） | 现有持久化为 Memory/OpenLoop/Mind store（`plugins/model/src/yunxi/schema.rs`） |
| 与 OpenLoop/Mind 的接入 | 🔶 部分（watch 开环已接；§93–95、§112 其余未接） | `observe_world_fact` 的 `world_loop_draft` 分支 |
| **Phase 0–16 建设计划**（§175–193） | ⬜ 尚未按 Phase 推进（当前处于“世界事实近似层”） | — |

> **一句话现状**：v4 目前只落地了「世界事实最小闭环」（传感器/事件 → `observe_world_fact` → Fact 记忆 + 开环 + 兴趣），其余均为设计蓝图；下文设计节引用的表（如 `yunxi_world_observations`）都是**目标 schema，尚未建表**。

## 0.2 分组目录

| 主题 | 章节 |
|---|---|
| 定位 / 原则 / 四层架构 / 职责边界 | §1–5 |
| 实现约束 / 平台无关 / 模块建议 | §6–8 |
| WorldModel 总结构 | §9 |
| Observation | §10–14 |
| Entity Model | §15–20 |
| Situation Model | §21–26 |
| Hypothesis | §27–33 |
| Temporal Model | §34–38 |
| Causal Model | §39–45 |
| Prediction | §46–52 |
| Simulation / Counterfactual / Snapshot | §53–67 |
| SocialScene / Environment | §68–79 |
| Uncertainty / 更新流水线 | §80–92 |
| 与 Mind / OpenLoop / Goal / Executive 集成 | §93–95、§112–115 |
| 示例场景 | §96–106 |
| 隐私 / 边界 / 存储接口 | §107–127 |
| 容量 / 预算 / 冲突 / 观测扩源 | §128–152 |
| 指标 / 可解释 / 安全 / 降级 | §153–165、§246–256 |
| 宿主与未来（CLI / 桌面 / 移动 / 视觉 / 语音 / 游戏） | §166–171 |
| 定位边界（≠知识库 / 人格 / 全能） | §172–174、§227–230 |
| 建设阶段与验收 | §175–193 |
| 行为场景（A–J） | §194–206 |
| 测试 / 索引 / 特性开关 / 灰度 | §204–222 |
| 校准 / 成功与失败指标 | §223–226、§260–261 |
| Neuro-like Agent / 边界红线 | §235–239、§257–258 |
| 删数据 / 管理命令 / 安全 | §241–245 |
| 最终架构 / DoD / 行为验收 / 后续原则 | §262–266 |

---

# 1. 文档目标

Yunxi Core v1 解决：

> 系统如何持续存在、接收事件、执行动作、观察动作结果。

Yunxi Mind v2 解决：

> 系统如何维护长期 SelfModel、Beliefs、Preferences、Interests、OpenQuestions、InnerAgenda 等内部心智状态。

Yunxi Executive v3 解决：

> 系统如何管理冲突、优先级、不确定性、计划、候选动作、注意力预算和反思调度。

Yunxi World Model v4 进一步解决：

> 系统如何结构化表示“外部世界当前可能是什么样”，如何理解事件之间的时间与局部因果关系，以及在执行高价值动作之前，有限地估计不同候选行为可能产生什么后果。

v4 的核心不是：

```text
让模型幻想未来。
```

而是：

```text
Observed Evidence
+
Known State
+
Uncertainty
+
Local Causal Knowledge
→ Possible World States
→ Predicted Outcomes
→ Executive Decision
```

---

# 2. 最重要的原则

World Model 中保存的内容是：

> **芸汐对世界的当前估计。**

不是：

> **现实世界的绝对事实。**

所有非直接、非高可靠来源的信息必须保留：

```text
confidence
source
uncertainty
freshness
```

禁止：

```text
推测
→ 自动升级成事实
```

禁止：

```text
没有回复
→ 用户生气
```

禁止：

```text
一句模糊表达
→ 建立高置信长期世界状态
```

---

# 3. 四层总架构

建议最终逻辑关系：

```text
                       WORLD
                         │
                         ▼
                      ADAPTER
                         │
                         ▼
                    WORLD EVENT
                         │
                         ▼
                      OBSERVE
                         │
           ┌─────────────┴─────────────┐
           ▼                           ▼
       YUNXI MIND                WORLD MODEL
   “我的内部状态”             “外部世界的估计”
           │                           │
           └─────────────┬─────────────┘
                         ▼
                 YUNXI EXECUTIVE
                “现在如何取舍”
                         │
                         ▼
                      PLANNER
                         │
                         ▼
                       INTENT
                         │
                         ▼
                       ACTION
                         │
                         ▼
                        WORLD
```

---

# 4. World Model 不是第四层管理器

v4 不应变成：

```text
Executive
→ MetaExecutive
→ WorldModelManager
→ SuperPlanner
```

World Model 是：

> 外部环境认知模型。

它与 Mind 属于不同方向：

```text
Mind:
内部状态

World Model:
外部状态

Executive:
对内部需求与外部现实进行取舍
```

---

# 5. 职责边界

## 5.1 Yunxi Mind

保存：

- SelfModel
- Beliefs
- Preferences
- Interests
- Curiosity
- OpenQuestions
- InnerAgenda
- Affect
- Relation

回答：

> “我是谁、我在意什么、我相信什么。”

---

## 5.2 Yunxi World Model

保存：

- Entities
- Situations
- Observations
- Hypotheses
- TemporalRelations
- CausalRelations
- Predictions
- EnvironmentState
- SocialScene
- Uncertainty

回答：

> “外部世界现在可能是什么样。”

---

## 5.3 Yunxi Executive

负责：

- 冲突；
- 优先级；
- 预算；
- Goal；
- Plan；
- Candidate；
- Reflection；
- Action selection。

回答：

> “基于我的目标和世界状态，现在应该怎么做。”

---

# 6. 最高级实现约束

不得为了实现 v4：

- 重写 v1 Event Bus；
- 重写 v2 Mind；
- 重写 v3 Executive；
- 重写 Identity；
- 重写 Memory；
- 重写 OpenLoop；
- 重写 Goal；
- 重写 Adapter；
- 重写 Reminder；
- 重写 agent_tasks；
- 重写 Tool Runtime。

缺接口时优先：

```text
Port
Snapshot
Proposal
Adapter
Bridge
Extension Point
```

---

# 7. 平台无关约束

建议模块位于：

```text
crates/yunxi-core/src/world_model/
```

或同等平台无关 crate。

禁止依赖：

- QQ；
- Kovi；
- OneBot；
- NapCat；
- RuntimeBot；
- SQLx；
- PgPool；
- Redis Client；
- Tauri；
- Android SDK；
- Web UI；
- 具体模型 HTTP API。

World Model 只能使用：

- PersonId；
- ConversationId；
- MessageId；
- GoalId；
- OpenLoopId；
- EventId；
- EntityId；
- SituationId；
- ObservationId；
- HypothesisId；
- PredictionId。

---

# 8. 模块建议

建议：

```text
world_model/
├── mod.rs
├── entity.rs
├── observation.rs
├── situation.rs
├── hypothesis.rs
├── temporal.rs
├── causal.rs
├── prediction.rs
├── simulation.rs
├── social_scene.rs
├── environment.rs
├── uncertainty.rs
├── snapshot.rs
├── update.rs
├── policy.rs
└── metrics.rs
```

实际目录可按项目风格调整。

---

# 9. WorldModel 总结构

建议：

```rust
pub struct WorldModel {
    pub entities: EntityStateIndex,
    pub situations: SituationIndex,
    pub hypotheses: HypothesisIndex,
    pub causal_knowledge: CausalKnowledge,
    pub temporal_state: TemporalState,
    pub social_scene: SocialSceneState,
    pub environment: EnvironmentState,
    pub version: u64,
}
```

不要将所有对象塞进一个巨大 HashMap。

应按用途分层并 bounded。

---

# 10. Observation

> 📌 **实现状态：** 🔶 部分——目前以“世界事实记忆”近似（`world_fact_draft`/`observe_world_fact`），无结构化字段、Reliability 与 TTL。

## 10.1 目标

Observation 表示：

> 系统实际观察到的外部证据。

例如：

```text
用户发来：
“面试过了。”
```

这是 Observation。

而：

```text
“用户很开心。”
```

通常只是推断。

---

# 11. Observation 数据结构

建议：

```rust
pub struct Observation {
    pub id: ObservationId,
    pub source_event_id: EventId,
    pub scope: WorldScope,
    pub kind: ObservationKind,
    pub payload: ObservationPayload,
    pub confidence: f32,
    pub observed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

---

# 12. ObservationSource Reliability

Observation 必须保留来源可靠性。

例如：

```text
DirectUserStatement
ToolResult
PlatformEvent
SystemState
ModelExtraction
DerivedObservation
```

不同来源权重不同。

---

# 13. Observation ≠ Belief

World Observation：

```text
用户说“我下午三点面试”
```

Mind Belief：

```text
“用户近期正在求职”
```

两者可以关联，但不能混为同一张表。

---

# 14. Observation TTL

很多 Observation 是短期状态。

例如：

```text
Desktop foreground
Voice connected
User active recently
```

应有 TTL。

不要永久保存为长期事实。

---

# 15. Entity Model

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 15.1 目标

建立外部实体抽象。

---

# 16. EntityKind

建议：

```rust
pub enum EntityKind {
    Person,
    Conversation,
    Host,
    Tool,
    GoalContext,
    Place,
    Topic,
    Resource,
    ExternalService,
    Unknown,
}
```

第一版只实现真正需要的类型。

不要过度泛化。

---

# 17. EntityId

WorldModel 内使用：

```rust
pub struct EntityId(Uuid);
```

Person 可关联：

```text
EntityId
↔
PersonId
```

Conversation 可关联：

```text
EntityId
↔
ConversationId
```

不要复制新的身份系统。

---

# 18. EntityState

建议：

```rust
pub struct EntityState {
    pub id: EntityId,
    pub kind: EntityKind,
    pub properties: Vec<StateProperty>,
    pub confidence: f32,
    pub last_observed_at: DateTime<Utc>,
    pub version: u64,
}
```

---

# 19. StateProperty

每个属性都应有：

```text
value
confidence
source
valid_from
valid_until
```

例如：

```text
Person A
employment_state = interviewing
confidence 0.78
```

---

# 20. 禁止人格数据库化

EntityState 不能演化成：

> 用户全部心理画像数据库。

只保存当前行为真正需要的世界状态。

---

# 21. Situation Model

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 21.1 目标

将多个 Observation / EntityState 组织成：

> 一个持续变化的局面。

---

# 22. Situation 示例

```text
Situation:
JobSearch

participant:
Person A

state:
InterviewScheduled

next_known_time:
2026-08-25T15:00

uncertainty:
medium
```

---

# 23. Situation 数据结构

建议：

```rust
pub struct Situation {
    pub id: SituationId,
    pub kind: SituationKind,
    pub participants: Vec<EntityId>,
    pub state: SituationState,
    pub confidence: f32,
    pub related_goals: Vec<GoalId>,
    pub related_open_loops: Vec<OpenLoopId>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}
```

---

# 24. Situation 生命周期

至少：

```text
Active
Dormant
Resolved
Failed
Expired
Unknown
```

---

# 25. Situation Transition

例如：

```text
InterviewScheduled
→ InterviewInProgress
→ OutcomeUnknown
→ InterviewPassed
```

必须由：

```text
Observation
+
validated transition
```

触发。

---

# 26. SituationTransition Proposal

模型可以提出：

```text
SituationTransitionProposal
```

Rust 验证：

- source；
- current version；
- valid transition；
- confidence；
- contradiction。

---

# 27. Hypothesis

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 27.1 目标

明确区分：

```text
Known
Suspected
Unknown
```

---

# 28. Hypothesis 数据结构

建议：

```rust
pub struct Hypothesis {
    pub id: HypothesisId,
    pub proposition: WorldProposition,
    pub confidence: f32,
    pub evidence_for: Vec<ObservationId>,
    pub evidence_against: Vec<ObservationId>,
    pub status: HypothesisStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

---

# 29. HypothesisStatus

至少：

```text
Active
Supported
Rejected
Superseded
Expired
Unknown
```

---

# 30. Hypothesis 不自动升级

例如：

```text
用户两小时没回复
```

不能直接变成：

```text
“用户不想理我”
```

最多：

```text
Hypothesis:
可能暂时忙
confidence 0.25
```

甚至通常无需创建。

---

# 31. Hypothesis 创建阈值

只在：

- 与 Goal 相关；
- 与 OpenLoop 相关；
- 与 Action decision 相关；
- 存在明显不确定；
- 高 salience；
- 可能影响行为；

时创建。

---

# 32. Unknown 是合法状态

WorldModel 必须能明确：

```text
Unknown
```

不要逼模型填空。

---

# 33. Unknown 优于错误假设

如果证据不足：

```text
unknown
```

比：

```text
low-quality hypothesis spam
```

更好。

---

# 34. Temporal Model

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 34.1 目标

时间不仅是 timestamp。

还需要：

- before；
- after；
- during；
- expected_at；
- due；
- duration；
- recency；
- staleness。

---

# 35. TemporalRelation

建议：

```rust
pub enum TemporalRelation {
    Before,
    After,
    During,
    Overlaps,
    Starts,
    Ends,
    ExpectedAt,
}
```

---

# 36. TimelineEntry

建议：

```rust
pub struct TimelineEntry {
    pub entity_or_situation: WorldRef,
    pub interval: TimeInterval,
    pub confidence: f32,
}
```

---

# 37. 时间表达解析

例如：

```text
“明天下午”
“今晚”
“过两天”
```

解析必须优先使用：

- deterministic date/time parser；
- current timezone；
- known conversation time。

LLM 可帮助语义抽取。

最终具体时间由 Rust 校验。

---

# 38. Temporal Uncertainty

“下午”不是：

```text
15:00:00 exact
```

应允许：

```text
time range
```

例如：

```text
13:00–18:00
```

或按产品约定。

---

# 39. Causal Model

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 39.1 目标

维护局部、有限、可追踪的因果倾向。

不是构建“世界真理”。

---

# 40. CausalRelation

建议：

```rust
pub struct CausalRelation {
    pub id: CausalRelationId,
    pub cause_pattern: WorldPattern,
    pub effect_pattern: WorldPattern,
    pub strength: f32,
    pub confidence: f32,
    pub source: CausalSource,
    pub scope: CausalScope,
}
```

---

# 41. CausalSource

例如：

```text
Seed
ObservedRepeatedPattern
ToolBehavior
Reflection
DomainRule
```

---

# 42. Causal Scope

区分：

```text
Global
ToolSpecific
PersonSpecific
ConversationSpecific
HostSpecific
```

Person-specific causal relation 要非常克制。

---

# 43. 禁止心理因果过拟合

例如：

```text
用户一次没回复
→ “因为我说错话”
```

禁止。

需要大量证据才可形成低置信局部模式。

---

# 44. Tool Causal Knowledge

这是 v4 第一版最适合应用的地方。

例如：

```text
API rate limit
→ immediate retry likely fail
```

这类因果关系：

- 低隐私风险；
- 高实用性；
- 可验证。

优先实现。

---

# 45. Environment Causal Knowledge

例如：

```text
Host offline
→ cannot deliver through that Host
```

属于 deterministic rule。

不需要 LLM。

---

# 46. Prediction

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 46.1 目标

在执行 Action 之前：

估计可能结果。

---

# 47. Prediction 数据结构

建议：

```rust
pub struct Prediction {
    pub id: PredictionId,
    pub source_candidate: CandidateId,
    pub possible_outcomes: Vec<PredictedOutcome>,
    pub confidence: f32,
    pub generated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

---

# 48. PredictedOutcome

建议：

```rust
pub struct PredictedOutcome {
    pub description: OutcomeKind,
    pub probability: f32,
    pub utility: f32,
    pub social_cost: f32,
    pub risk: f32,
    pub goal_progress: f32,
}
```

---

# 49. Probability 不要求伪精确

如果模型无法合理给精确概率：

可以使用：

```text
Low
Medium
High
```

再映射为区间。

不要制造：

```text
73.42%
```

这种虚假精度。

---

# 50. Prediction ≠ Expectation

v3 Expectation：

> 行动以后期待观察到什么。

v4 Prediction：

> 行动以前估计可能发生什么。

---

# 51. Prediction 生命周期

Prediction：

短期。

Action 执行后：

可以比较：

```text
predicted
vs
observed
```

用于：

- calibration；
- causal update；
- plan quality。

---

# 52. Prediction Error

定义：

```text
PredictionError
```

只作为：

```text
模型校准信号
```

不要拟人化。

---

# 53. Counterfactual Simulation

> 📌 **实现状态：** ⬜ 仅设计，未实现。

## 53.1 目标

对少量候选动作做：

```text
What if A?
What if B?
What if C?
```

---

# 54. Simulation 输入

建议：

```rust
pub struct SimulationInput {
    pub world_snapshot: WorldModelSnapshot,
    pub mind_snapshot: MindSnapshot,
    pub executive_snapshot: ExecutiveSnapshot,
    pub candidate: ActionCandidate,
    pub horizon: SimulationHorizon,
}
```

---

# 55. Simulation 输出

```rust
pub struct SimulationResult {
    pub candidate_id: CandidateId,
    pub predicted_outcomes: Vec<PredictedOutcome>,
    pub uncertainty: f32,
    pub model_version: u64,
}
```

---

# 56. Simulation 不是真实执行

必须建立硬边界。

禁止：

```text
simulate()
→ ActionPort.execute()
```

Simulation 中：

- 不发送消息；
- 不调用真实 Tool 副作用；
- 不改数据库业务状态；
- 不修改 Goal；
- 不 resolve OpenLoop；
- 不发通知。

---

# 57. ExecutionMode

可加入：

```rust
pub enum ExecutionMode {
    Simulated,
    Real,
}
```

任何 infrastructure side effect：

必须只接受：

```text
Real
```

---

# 58. Simulation Sandbox

如果需要模拟 Tool：

只能使用：

- cached capability description；
- historical success statistics；
- mock result model；
- read-only metadata。

不能：

```text
真正调用付费 / destructive tool
```

---

# 59. Simulation 使用条件

只在：

- 多个合理 candidate；
- high consequence；
- long-running Goal；
- proactive gray zone；
- tool recovery；
- social interruption gray zone；
- plan revision；

时启用。

---

# 60. 普通聊天不模拟

以下默认不需要：

```text
“你好”
“谢谢”
简单知识问答
普通 direct reply
```

---

# 61. Simulation Budget

应有：

```text
max simulations per root event
```

建议：

```text
1～3
```

---

# 62. Simulation Depth

只做：

```text
shallow horizon
```

例如：

```text
下一步
或
接下来少量状态
```

不要模拟：

```text
10 步未来人生。
```

---

# 63. WorldModelSnapshot

Planner / Executive 不直接持有 WorldModel 锁。

建议：

```rust
pub struct WorldModelSnapshot {
    pub entities: Vec<EntityStateSnapshot>,
    pub situations: Vec<SituationSnapshot>,
    pub hypotheses: Vec<HypothesisSnapshot>,
    pub relevant_causal: Vec<CausalRelationSnapshot>,
    pub social_scene: SocialSceneSnapshot,
    pub environment: EnvironmentSnapshot,
    pub version: u64,
}
```

---

# 64. Snapshot 必须相关检索

不能：

```text
把整个世界模型塞给 Planner。
```

需要：

```text
current event
current conversation
related persons
active goals
open loops
agenda
```

进行检索。

---

# 65. Snapshot Bound

建议：

```text
entities ≤ 16
situations ≤ 8
hypotheses ≤ 8
causal relations ≤ 8
temporal entries ≤ 12
```

配置化。

---

# 66. WorldModel Version

每次关键状态变更：

```text
version += 1
```

Simulation / Planner 返回后：

重新检查 version。

---

# 67. Stale Simulation

如果：

```text
simulation based on version 20
```

执行前：

```text
world version = 28
```

且关键 state 已变化：

必须：

```text
discard / re-evaluate
```

---

# 68. Social Scene Model

## 68.1 目标

理解：

> 当前会话的社交局面。

尤其群聊。

---

# 69. SocialSceneState

建议：

```rust
pub struct SocialSceneState {
    pub conversation_id: ConversationId,
    pub active_participants: Vec<PersonId>,
    pub current_floor: Vec<PersonId>,
    pub bot_addressed: bool,
    pub activity_level: f32,
    pub interruption_cost: f32,
    pub scene_kind: SocialSceneKind,
}
```

---

# 70. SocialSceneKind

例如：

```text
DirectConversation
GroupDiscussion
RapidGroupChat
IdleGroup
TaskConversation
Unknown
```

---

# 71. Current Floor

表示：

> 现在主要是谁在说。

例如：

```text
A ↔ B
```

如果芸汐未被叫：

```text
interruption cost ↑
```

---

# 72. Social Scene 不推断私人心理

只描述：

- 对话结构；
- 活跃度；
- 说话轮次；
- 是否被点名；
- 是否快速刷屏。

不要推断：

```text
谁讨厌谁
谁暗恋谁
谁生气
```

除非有明确证据并另走 Mind/Relation。

---

# 73. Environment Model

## 73.1 目标

描述：

> 当前可用环境。

---

# 74. EnvironmentState

例如：

```rust
pub struct EnvironmentState {
    pub hosts: Vec<HostState>,
    pub available_capabilities: Vec<ActionDescriptor>,
    pub model_health: ServiceHealth,
    pub tool_health: Vec<ToolHealth>,
    pub load: RuntimeLoad,
}
```

---

# 75. HostState

例如：

```text
Kovi:
online

Desktop:
offline

Mobile:
available
```

---

# 76. HostState 属于环境

不是 SelfModel。

Host 可以变化。

---

# 77. Delivery Context

v4 可以给 Executive：

```text
当前哪个 Host 可达
哪个 Host 活跃
```

但具体 Delivery selection 仍属于：

```text
Executive / Delivery Router / Host
```

---

# 78. Runtime Load

WorldModel 可以提供：

```text
model queue depth
tool latency
host availability
```

帮助 Executive 判断：

是否 defer 低价值动作。

---

# 79. Service Health

如果 Tool A 当前故障：

Prediction：

```text
Tool A success probability low
```

PlanRevision：

优先 Tool B。

---

# 80. World Uncertainty

## 80.1 目标

全局跟踪：

> 当前哪些地方不知道。

---

# 81. Uncertainty 类型

```text
StateUnknown
TemporalUnknown
SourceConflict
StaleState
InsufficientEvidence
PredictionUncertain
```

---

# 82. Uncertainty 传播

如果输入 hypothesis confidence 低：

Prediction 也不能变高。

例如：

```text
world state certainty 0.4
```

则：

```text
prediction confidence <= bounded value
```

---

# 83. 不确定性优先保守

高不确定：

更倾向：

```text
Ask
Defer
Observe
NoAction
```

而不是：

```text
强行执行高后果动作
```

---

# 84. World Update Pipeline

建议：

```text
WorldEvent
→ Observation Extraction
→ Source Validation
→ Entity Update Proposal
→ Situation Update Proposal
→ Hypothesis Update
→ Temporal Update
→ WorldModel Version++
```

---

# 85. Observation Extraction

尽可能：

- deterministic；
- reuse MessageUnderstanding；
- reuse ToolResult metadata。

不要每个 event 都多一次模型调用。

---

# 86. Structured Extraction

LLM 可输出：

```text
ObservationProposal
SituationProposal
HypothesisProposal
```

Rust 校验。

---

# 87. 模型禁止直接写 WorldModel

必须：

```text
Proposal
→ Validator
→ Merge / Reject
→ Store
```

---

# 88. Merge Policy

重复 observation：

dedupe。

相似 situation：

merge。

冲突 state：

保留：

```text
multiple hypotheses
```

而不是强行覆盖。

---

# 89. WorldState Freshness

每个动态状态需要：

```text
last_observed_at
```

例如：

```text
user_active = true
```

10 小时后不能仍当真。

---

# 90. Stale Policy

状态可：

```text
Fresh
Stale
Expired
Unknown
```

---

# 91. Entity Property TTL

不同属性不同 TTL。

例如：

```text
host online:
seconds/minutes

user recent activity:
minutes

scheduled interview:
until event window ends

tool availability:
minutes

stable external fact:
longer
```

---

# 92. Situation Expiry

例如：

```text
InterviewScheduled
```

到时间后：

不能永远保持 scheduled。

应：

```text
OutcomeUnknown
```

或：

```text
ExpiredUnknown
```

---

# 93. Situation + OpenLoop Integration

Situation：

```text
InterviewOutcomeUnknown
```

可以关联：

```text
OpenLoop:
FollowUp interview
```

---

# 94. Situation + Expectation

如果系统问：

```text
“面试怎么样？”
```

Expectation：

```text
等待 outcome response
```

Situation：

仍然：

```text
OutcomeUnknown
```

直到新 Observation 到达。

---

# 95. Situation + Goal

Goal：

```text
帮助用户完成某项目
```

World Situation：

```text
BuildCurrentlyFailing
```

Tool result：

```text
BuildPassed
```

Situation transition：

```text
BuildCurrentlyFailing
→ BuildPassing
```

Goal 可以完成。

---

# 96. Causal Learning

第一版只允许非常受控的学习。

不要：

```text
模型随便发明因果关系
```

---

# 97. Causal Proposal

建议：

```rust
pub struct CausalRelationProposal {
    pub cause: WorldPattern,
    pub effect: WorldPattern,
    pub confidence: f32,
    pub evidence_refs: Vec<ObservationId>,
}
```

---

# 98. Causal Promotion

从 temporary pattern：

```text
candidate
```

升级为 active relation：

需要：

- repeated evidence；
- domain rule；
- admin seed；
- reliable tool behavior。

---

# 99. Person-specific Causal Learning

默认：

限制很严。

例如：

```text
“这个人晚回消息意味着生气”
```

不应建立。

---

# 100. Social Causal Policy

优先使用通用成本：

```text
interruption cost
repetition cost
recent unanswered message
```

而不是心理因果。

---

# 101. Counterfactual Candidate Example

当前：

```text
OpenLoop due:
面试
```

候选：

```text
A:
现在问

B:
晚点问

C:
不问
```

WorldModel 模拟：

```text
A:
timeliness high
interruption risk medium

B:
timeliness medium
interruption risk low

C:
interruption zero
follow-up value zero
```

Executive 最终选。

---

# 102. Tool Recovery Example

Tool A：

```text
currently rate limited
```

Candidate：

```text
A retry now
B wait
C use fallback
```

Prediction：

```text
A likely fail
B likely recover
C medium success
```

Executive：

```text
C
```

---

# 103. Group Example

SocialScene：

```text
rapid discussion
bot not addressed
current floor A/B
```

Mind：

```text
interest high
```

WorldModel：

```text
interruption cost high
```

Executive：

```text
Silent
```

---

# 104. Direct Example

用户：

```text
“我现在马上要出门。”
```

World Observation：

```text
user busy / leaving soon
```

confidence：

取决于明确程度。

主动问题候选：

```text
interruption cost high
```

---

# 105. Temporal Example

用户：

```text
“明晚八点左右结束。”
```

WorldModel：

```text
time window:
19:30–20:30
```

不要精确到：

```text
20:00:00
```

除非用户明确。

---

# 106. Unknown Outcome Example

用户：

```text
“我去医院检查一下。”
```

不能建立：

```text
diagnosis
```

只能：

```text
Situation:
medical visit planned
Outcome:
Unknown
```

不保存敏感推测。

---

# 107. Privacy

WorldModel 也必须遵守 scope。

Private A：

不得泄露给 Private B。

Group：

不得自动加载 Private Person state。

---

# 108. Sensitive State

默认禁止模型主动推断并持久化：

- 医疗诊断；
- 政治倾向；
- 宗教；
- 性取向；
- 犯罪历史；
- 其他敏感属性。

---

# 109. Person State 最小化

只保存：

当前任务真正需要的状态。

---

# 110. World Model 与 Memory 区别

Memory：

> 过去发生过什么。

WorldModel：

> 根据过去与现在，我认为当前世界是什么状态。

---

# 111. World Model 与 Belief 区别

Belief：

> 芸汐认为某个命题可信。

WorldState：

> 当前环境状态估计。

例如：

```text
Belief:
“Rust 的严格类型系统有价值”

WorldState:
“当前项目 build 正在失败”
```

---

# 112. World Model 与 OpenLoop 区别

OpenLoop：

> 未来需要重新关注。

WorldModel：

> 当前状态。

---

# 113. World Model 与 Goal 区别

Goal：

> 想达到什么。

WorldModel：

> 当前情况是什么。

---

# 114. World Model 与 Executive 区别

Executive：

> 选择怎么做。

WorldModel：

> 提供对外部世界的估计与可能后果。

---

# 115. World Model 与 Planner

Planner：

可以读取：

```text
MindSnapshot
WorldModelSnapshot
ExecutiveSnapshot
```

但不要把三者全部原始数据塞进去。

必须 retrieval。

---

# 116. PlannerInput v4

建议：

```rust
pub struct PlannerInput {
    pub event: WorldEvent,
    pub working_state: WorkingStateSnapshot,
    pub mind: MindSnapshot,
    pub world: WorldModelSnapshot,
    pub executive: ExecutiveSnapshot,
    pub capabilities: Vec<ActionDescriptor>,
}
```

---

# 117. Planner 不直接修改 WorldModel

Planner 只能：

```text
WorldUpdateProposal
```

Rust 校验。

---

# 118. Simulation 与 Planner

候选 Action：

可以由 Planner 生成。

Simulation：

评估。

Executive：

选择。

最终 Natural Language：

仍尽量一次生成。

---

# 119. 避免三次 LLM

错误：

```text
Planner LLM
→ Simulation LLM
→ Executive LLM
→ Reply LLM
```

普通路径必须避免。

---

# 120. 推荐路径

普通消息：

```text
single Planner call
+
deterministic validation
```

复杂高价值：

```text
Planner candidates
→ optional simulation
→ deterministic / cheap Executive compare
```

---

# 121. Simulation Model Tier

如果必须使用模型：

优先：

```text
cheap / fast model
```

不要所有 simulation 都用主模型。

---

# 122. Simulation Cache

相同：

```text
world version
candidate
```

可以短 TTL cache。

---

# 123. Prediction Calibration

记录：

```text
predicted outcome
observed result
```

用于统计：

```text
calibration error
```

---

# 124. 不做在线强化学习

v4 第一版：

不要自动调整模型权重。

只：

```text
metrics
rule tuning
confidence calibration
```

---

# 125. WorldModel Store Ports

> 📌 **实现状态：** ⬜ 设计——下列 Store Ports 与 `yunxi_world_observations` 等表**尚未实现/建表**；当前世界事实走 Memory/OpenLoop store。

建议：

```text
EntityStateStore
SituationStore
HypothesisStore
CausalStore
PredictionStore
```

也可合理合并。

---

# 126. Persistence

SQL 属于 infrastructure。

建议表：

```text
yunxi_world_entities
yunxi_world_entity_properties
yunxi_world_situations
yunxi_world_observations
yunxi_world_hypotheses
yunxi_world_causal_relations
yunxi_world_predictions
```

---

# 127. 不全部持久化

以下可仅 runtime：

- active SocialScene；
- transient Environment load；
- short prediction；
- simulation results。

---

# 128. 持久化原则

长期保存：

- 有价值 Situation；
- important Observation；
- unresolved Hypothesis；
- validated CausalRelation。

短期状态：

TTL。

---

# 129. Schema Migration

必须：

- additive；
- idempotent；
- backward compatible；
- 不 drop v1/v2/v3 表；
- 不要求停机 destructive migration。

---

# 130. Restart Recovery

重启后：

- active Situation 恢复；
- unresolved Hypothesis 恢复；
- persistent causal knowledge 恢复；
- short transient scene 可重建；
- simulation cache 不必恢复。

---

# 131. Cleanup

必须有：

- stale observation cleanup；
- expired hypothesis cleanup；
- resolved situation retention policy；
- prediction TTL；
- simulation cache eviction。

---

# 132. Bound

所有 runtime index 必须 bounded 或 TTL。

---

# 133. Entity Cap

如果 Entity 数量巨大：

使用：

```text
active set
+
persistent store
```

不要全部常驻内存。

---

# 134. Situation Cap

每个 Person / Conversation：

active situation 数量 bounded。

---

# 135. Hypothesis Cap

例如：

```text
max active hypotheses per person = 16
max per conversation = 16
```

防止猜测爆炸。

---

# 136. Causal Cap

只保留：

```text
high confidence
high utility
```

的关系进入 active retrieval。

---

# 137. Simulation Cap

每个 root trace：

```text
max simulations
```

必须有限。

---

# 138. Time Horizon

Simulation horizon：

```text
Immediate
Short
TaskStep
```

不要：

```text
LongTermLife
```

---

# 139. WorldModel Update Budget

群聊大量消息：

不能每条都做复杂世界更新。

使用：

- semantic snapshot reuse；
- coalescing；
- low-value observation sampling。

---

# 140. Group Observation

普通群聊：

可以只更新：

```text
SocialScene
activity
current floor
```

不必每条建 Situation。

---

# 141. Tool Observation

ToolResult：

高价值。

可更新：

- Entity；
- Situation；
- Environment；
- Causal evidence。

---

# 142. ActionResult

ActionResult：

必须重新成为 WorldEvent。

WorldModel：

观察：

```text
success
failure
latency
delivery
```

---

# 143. Action Success

例如：

```text
message delivered
```

WorldState：

```text
delivery successful
```

Expectation：

进入 pending response。

---

# 144. Action Failure

例如：

```text
Host unavailable
```

Environment：

```text
host health ↓
```

PlanRevision：

可触发。

---

# 145. SocialScene Input

可以复用：

- conversation coordinator；
- message timestamps；
- participant activity；
- bot addressed flag；
- reply relationships。

---

# 146. SocialScene 不需要大模型

大部分可以 deterministic。

---

# 147. Causal Inference 预算

只有：

```text
repeated pattern
```

或：

```text
important failure
```

才触发。

---

# 148. Hypothesis Dedup

相同 proposition：

merge evidence。

---

# 149. Hypothesis Contradiction

相反 hypothesis：

可以并存。

例如：

```text
A
confidence 0.4

not A
confidence 0.35
```

直到证据增加。

---

# 150. Hypothesis Resolution

新 Observation：

支持：

```text
Supported
Rejected
Superseded
```

---

# 151. Situation Conflict

两个 Situation 状态互斥：

必须触发：

```text
WorldConflict
```

交给 Executive / Mind ConflictMonitor。

---

# 152. Cross-layer Conflict

例如：

WorldModel：

```text
Tool A unavailable
```

Executive Plan：

```text
Use Tool A
```

→ Capability / State conflict。

---

# 153. WorldModel Reason Tags

建议：

```text
STATE_STALE
STATE_UNKNOWN
HYPOTHESIS_LOW_CONFIDENCE
SITUATION_TRANSITION
SOCIAL_INTERRUPT_HIGH
HOST_UNAVAILABLE
TOOL_DEGRADED
PREDICTION_UNCERTAIN
CAUSAL_RULE_MATCH
SIMULATION_SKIPPED
SIMULATION_USED
WORLD_VERSION_STALE
```

---

# 154. Metrics

建议：

```text
yunxi_world_observations_total
yunxi_world_situations_active
yunxi_world_hypotheses_active
yunxi_world_hypotheses_resolved
yunxi_world_predictions_total
yunxi_world_simulations_total
yunxi_world_simulations_skipped
yunxi_world_prediction_error
yunxi_world_stale_state_total
yunxi_world_social_scene_updates_total
yunxi_world_causal_relations_active
```

---

# 155. Debug Interface

管理员可：

```text
#world-status
```

显示：

- active situations；
- active hypotheses count；
- stale state count；
- environment health；
- current social scene；
- recent prediction summary；
- world version。

不要：

- 输出隐藏 chain-of-thought；
- 默认输出敏感 private state；
- 泄露 secrets。

---

# 156. Explainability

需要能回答调试问题：

```text
为什么认为 Tool A 不可用？
```

通过：

```text
source observations
confidence
freshness
```

---

# 157. Observation Lineage

每个 derived state：

最好能追溯：

```text
source event
observation
proposal
state update
```

---

# 158. No Hidden Reasoning Storage

v4 同样不存：

```text
详细思维链
```

只存：

- observation；
- state；
- hypothesis；
- confidence；
- prediction；
- reason tags。

---

# 159. Simulation Trace

Simulation 有：

```text
simulation_id
candidate_id
world_version
```

但不包含：

完整 chain-of-thought。

---

# 160. Security Boundary

WorldModel 不能绕过：

- Safety；
- Permission；
- ActionArbiter；
- MustExecute。

---

# 161. Simulated Action Security

即使只是 Simulation：

也不能生成并执行：

真实 side effect。

---

# 162. Tool Capability

WorldModel 可以预测：

```text
Tool likely unavailable
```

但最终能否调用：

由 Tool Runtime / Arbiter 决定。

---

# 163. Data Delete

用户删除数据时：

对应：

- Person-linked world state；
- Situations；
- Hypotheses；
- Predictions；
- Observation references；

必须进入删除策略。

---

# 164. Identity Unlink

解绑 QQ：

不一定删除：

Person world state。

但 QQ-specific external state：

应清理或 unlink。

---

# 165. Host Portability

WorldModel 不应绑定：

```text
QQ online/offline
```

而应：

```text
HostState
```

Host metadata 可：

```text
provider = qq
```

但 Core 只视作 opaque capability source。

---

# 166. CLI Host

yunxi-cli：

WorldModel 仍应工作。

至少：

- Person；
- Conversation；
- Situation；
- Hypothesis；
- Prediction；

可运行。

---

# 167. Desktop Future

未来 Desktop 可提供：

```text
Foreground
Idle
Notification state
Voice availability
```

WorldModel 只新增 Observation source。

Core 结构不变。

---

# 168. Mobile Future

Mobile：

```text
AppBackgrounded
NotificationOpened
```

同样进入：

WorldEvent / Observation。

---

# 169. Vision Future

未来视觉：

```text
VisualObservation
```

可进入 WorldModel。

但 v4 当前文档：

不要求实现 Camera / Screenshot。

---

# 170. Voice Future

Voice activity：

可影响 SocialScene。

当前不要求 STT/TTS。

---

# 171. Game Future

GameState：

可作为：

```text
Situation / EnvironmentState
```

当前不实现游戏控制。

---

# 172. World Model 不做全能知识库

公共知识问答：

仍使用：

Model / Tool / Web。

WorldModel 只维护：

当前 Agent 行为需要的世界状态。

---

# 173. Knowledge Base 区分

例如：

```text
“东京是日本首都”
```

不必进 WorldModel。

---

# 174. 当前 Context 才进 WorldModel

例如：

```text
“用户现在人在东京”
```

若业务需要且有明确来源，才是 WorldState。

---

# 175. Phase 顺序

> 📌 **实现状态：** ⬜ 尚未按 Phase 推进——当前代码仅处于“世界事实近似层”（对应 Phase 1 的极简子集）。

建议：

```text
Phase 0
WorldModel domain types + snapshots + no behavior change

Phase 1
Observation model + source reliability

Phase 2
EntityState

Phase 3
Situation model + transitions

Phase 4
Temporal model + freshness

Phase 5
Hypothesis + uncertainty

Phase 6
EnvironmentState

Phase 7
SocialScene

Phase 8
WorldModel retrieval + PlannerInput v4

Phase 9
Prediction

Phase 10
Counterfactual Simulation

Phase 11
Causal relations

Phase 12
Executive integration

Phase 13
Plan / Tool recovery integration

Phase 14
Proactive / Social integration

Phase 15
Persistence hardening

Phase 16
Calibration + behavioral evaluation
```

---

# 176. Phase 0：Domain Skeleton

新增：

- EntityId；
- SituationId；
- ObservationId；
- HypothesisId；
- PredictionId；
- WorldModel；
- WorldModelSnapshot；
- WorldScope；
- WorldUncertainty。

不改变用户行为。

---

# 177. Phase 0 验收

```text
cargo test -p yunxi-core
```

无需：

- Kovi；
- PostgreSQL；
- network。

---

# 178. Phase 1：Observation

支持：

- MessageReceived；
- ToolResult；
- ActionResult；
- Host state。

先 Shadow。

---

# 179. Phase 2：EntityState

先实现：

- Person；
- Conversation；
- Host；
- Tool。

不要一开始支持所有 EntityKind。

---

# 180. Phase 3：Situation

优先真实业务：

- FutureEvent；
- ToolTask；
- AgentTask；
- Build/Task state；
- Conversation state。

---

# 181. Phase 4：Temporal

实现：

- timestamp；
- time window；
- stale；
- expiry；
- timeline。

---

# 182. Phase 5：Hypothesis

先支持：

- task outcome；
- tool availability；
- situation ambiguity。

Person psychology 默认不做。

---

# 183. Phase 6：Environment

实现：

- Host availability；
- Tool health；
- Runtime load；
- model health。

---

# 184. Phase 7：SocialScene

实现：

- addressed；
- current floor；
- activity；
- interruption cost。

Shadow 观察。

---

# 185. Phase 8：PlannerInput v4

Planner 开始读取：

WorldModelSnapshot。

先不 Simulation。

---

# 186. Phase 9：Prediction

先做：

deterministic / structured prediction。

主要用于：

- Tool；
- Delivery；
- OpenLoop proactive。

---

# 187. Phase 10：Simulation

只给：

高价值 gray zone。

先 Shadow 比较。

---

# 188. Phase 11：Causal

优先：

Tool / Host / Runtime。

不要先做人类心理因果。

---

# 189. Phase 12：Executive

Executive Candidate score：

加入：

- predicted utility；
- world uncertainty；
- interruption cost；
- environment availability。

---

# 190. Phase 13：Plan Integration

PlanRevision：

使用：

WorldState / Prediction。

---

# 191. Phase 14：Proactive

ReachOut：

加入：

- SocialScene；
- Current world situation；
- predicted interruption cost。

---

# 192. Phase 15：Persistence

补：

- indexes；
- TTL；
- restart；
- cleanup；
- migration。

---

# 193. Phase 16：Calibration

评估：

prediction vs actual。

调：

confidence。

---

# 194. Behavioral Scenario A：Interview Follow-up

Observation：

```text
用户昨天说今天面试。
```

Situation：

```text
InterviewScheduled
```

时间过去：

```text
OutcomeUnknown
```

OpenLoop due。

当前：

```text
user recently active elsewhere
conversation idle
```

Candidates：

```text
AskNow
AskLater
Drop
```

Prediction：

```text
AskNow interruption medium
AskLater lower
```

Executive 选择。

---

# 195. Scenario B：No Reply

芸汐问了问题。

用户没回复。

预期：

```text
Expectation Pending / Expired
```

WorldModel 不生成：

```text
“用户生气”
```

---

# 196. Scenario C：Tool Rate Limit

ToolResult：

```text
429
```

Environment：

```text
Tool A degraded
```

Causal：

```text
immediate retry low utility
```

Plan：

fallback。

---

# 197. Scenario D：Group Chat

Rapid group chat。

bot not addressed。

Interest high。

SocialScene：

```text
interruption cost high
```

Executive：

Silent。

---

# 198. Scenario E：Unknown

用户说：

```text
“我明天可能出去。”
```

WorldModel：

```text
possible outing
low confidence
```

不要：

```text
calendar fact
```

---

# 199. Scenario F：Contradictory Observations

Observation A：

```text
event starts 15:00
```

Observation B：

```text
后来用户说改到 16:00
```

WorldModel：

newer explicit correction wins。

---

# 200. Scenario G：Stale State

Host：

10 分钟前 online。

TTL 到期。

WorldModel：

```text
Unknown
```

不是继续：

```text
online
```

---

# 201. Scenario H：Simulation

候选：

```text
Use Tool A
Use Tool B
Ask User
```

WorldModel：

Tool A degraded。

Simulation：

Tool B best utility。

---

# 202. Scenario I：Prediction Error

Prediction：

Tool B likely succeed。

实际：

失败。

记录：

```text
PredictionError
```

未来：

confidence 调低。

---

# 203. Scenario J：CLI

无 QQ。

用户：

```text
“我明天下午去开会”
```

Yunxi CLI：

仍可以：

- Observation；
- Situation；
- Temporal；
- OpenLoop；
- WorldModel。

---

# 204. Unit Tests

至少：

- observation source；
- confidence clamp；
- entity version；
- situation transition；
- stale property；
- temporal range；
- hypothesis merge；
- hypothesis contradiction；
- world snapshot bound；
- simulation side-effect isolation；
- prediction probability normalization；
- environment TTL；
- social interruption calculation。

---

# 205. Concurrency Tests

至少：

- concurrent observation update；
- stale world version；
- situation transition race；
- hypothesis update race；
- environment update race；
- simulation + live update；
- no lock across await；
- no deadlock。

---

# 206. Persistence Tests

至少：

- active situation restart；
- hypothesis restart；
- migration idempotency；
- expired observation cleanup；
- prediction TTL；
- causal dedupe。

---

# 207. Performance Tests

普通 direct message：

v4 不应默认增加一次 Simulation LLM。

---

# 208. Cost Tests

统计：

```text
world extraction calls
prediction calls
simulation calls
causal inference calls
```

---

# 209. Memory Tests

WorldModel active state：

不得随消息量无限增长。

---

# 210. Group Load Test

大量 group event：

只维护 bounded SocialScene。

不得大量生成 Hypothesis。

---

# 211. World Snapshot Latency

检索：

应有明确上限。

---

# 212. Index 建议

PostgreSQL：

```text
entity_id
situation status
participant
hypothesis status
expires_at
observation observed_at
prediction expires_at
causal scope
```

---

# 213. Query 要求

禁止：

```text
每个 Event
→ SELECT * all situations
```

---

# 214. Snapshot Retrieval

查询：

```text
current scope
related entity
active only
ORDER BY relevance / recency
LIMIT
```

---

# 215. Feature Flags

建议：

```toml
[world_model]
enabled = true
shadow_mode = true

[world_model.observation]
enabled = true

[world_model.entity]
enabled = true

[world_model.situation]
enabled = true

[world_model.hypothesis]
enabled = false

[world_model.social_scene]
enabled = true

[world_model.environment]
enabled = true

[world_model.prediction]
enabled = false

[world_model.simulation]
enabled = false

[world_model.causal]
enabled = false
```

---

# 216. Configuration

建议：

```toml
[world_model.limits]
max_entities_per_snapshot = 16
max_situations_per_snapshot = 8
max_hypotheses_per_snapshot = 8
max_causal_per_snapshot = 8

[world_model.hypothesis]
max_active_per_person = 16
min_create_confidence = 0.20

[world_model.prediction]
max_outcomes = 4

[world_model.simulation]
max_candidates = 3
max_per_root_trace = 2
```

---

# 217. Shadow Mode

所有高风险能力先：

```text
would_transition
would_predict
would_simulate
would_mark_stale
```

不改用户行为。

---

# 218. Rollout

推荐：

```text
Phase 0-8 Shadow
→ admin
→ selected private
→ selected group
→ broader rollout
```

---

# 219. Simulation Rollout

Simulation 最后开。

---

# 220. Observability Before Rollout

上线前必须能看到：

```text
state source
confidence
freshness
prediction source
simulation reason
```

---

# 221. Causal Rollout

Causal 最慢。

优先：

Tool / Host。

---

# 222. No Over-Simulation

如果 90% 消息都走 Simulation：

设计失败。

---

# 223. No Over-Hypothesis

如果一个用户几天内产生数百 Hypothesis：

设计失败。

---

# 224. No Fake Precision

禁止：

```text
probability = 0.734219
```

除非来自真实统计模型。

---

# 225. Calibration 优先

宁可：

```text
uncertain
```

不要：

```text
confidently wrong
```

---

# 226. Prediction Quality

v4 成功不是：

“预测得神”。

而是：

> 系统知道自己什么时候不确定。

---

# 227. World Model 不负责人格

不要把：

```text
Preference
Value
SelfModel
```

搬进 v4。

---

# 228. Mind 不负责世界状态

不要把：

```text
Host online
Tool degraded
Current group floor
```

塞进 Mind。

---

# 229. Executive 不保存完整世界

Executive 只拿：

WorldModelSnapshot。

---

# 230. Clean Boundaries

理想：

```text
Mind:
internal

WorldModel:
external

Executive:
control

Core:
runtime
```

---

# 231. Decision Formula

概念：

```text
Desired State
(Mind / Goals)

+

Estimated External State
(World Model)

+

Control Constraints
(Executive)

=

Action
```

---

# 232. Prediction Feedback

ActionResult：

回到：

WorldModel。

形成：

```text
Predict
→ Act
→ Observe
→ Compare
→ Update
```

---

# 233. World Model 闭环

最终：

```text
OBSERVE
→ MODEL
→ PREDICT
→ ACT
→ OBSERVE RESULT
→ UPDATE MODEL
```

---

# 234. 但不要持续预测

只有需要。

---

# 235. World Model 与 Neuro-like Agent

本项目不假设任何外部系统的内部实现。

v4 仅实现：

通用 Agent 世界模型能力。

---

# 236. 不模仿人格

WorldModel 与任何角色口癖无关。

---

# 237. Safety-first Simulation

Simulation 不能用来规避权限。

---

# 238. Dangerous Candidate

如果 Candidate 本身：

ActionArbiter 不允许。

无需 Simulation。

直接拒绝。

---

# 239. Simulation Ordering

顺序：

```text
Safety Filter
→ Capability Filter
→ Candidate
→ Simulation
```

不是：

```text
Simulation
→ 看能不能绕过限制
```

---

# 240. MustExecute

Reminder：

不需要 Simulation 来判断：

```text
要不要执行
```

只可以：

```text
选择合适 delivery
```

---

# 241. Data Deletion

同理：

不得 Prediction 后决定不删。

---

# 242. WorldModel Store Delete

Person data delete：

必须清理：

- entity properties；
- person situations；
- hypotheses；
- person-linked observations；
- predictions；
- causal person-specific relations。

---

# 243. Aggregate Causal Data

如果因 anonymized aggregate 保留：

必须明确独立隐私策略。

v4 默认：

不实现。

---

# 244. Admin Commands

建议：

```text
#world-status
#world-situations
#world-hypotheses
```

只管理员。

---

# 245. 不输出敏感私聊

Debug：

默认只显示：

IDs / summary / counts。

---

# 246. Metrics Privacy

Metrics 不带：

消息正文。

---

# 247. Structured Logs

例如：

```text
[YUNXI_WORLD]
event=...
situation_transition=...
confidence=...
reason=...
```

---

# 248. WorldModel Error

失败：

不影响 direct reply。

---

# 249. WorldModel Failure Fallback

如果 World snapshot 不可用：

Planner 退化为：

v3 path。

---

# 250. Simulation Failure

失败：

选择：

```text
unsimulated candidate path
```

不要对用户显示系统错误。

---

# 251. Prediction Failure

Prediction 不可用：

Executive 仍可用：

base candidate scores。

---

# 252. Database Failure

World persistence failure：

根据 severity：

- log；
- degrade；
- no destructive retry loop。

---

# 253. Circuit Breaker

WorldModel 后台模块连续失败：

可以暂时 disable expensive stages。

---

# 254. Load Shedding

高负载：

优先禁用：

```text
simulation
causal inference
deep prediction
```

保留：

```text
Observation
critical Situation update
```

---

# 255. Service Degradation

层级：

```text
Full
NoSimulation
NoPrediction
ObservationOnly
Disabled
```

---

# 256. Health Metrics

可查看：

当前 WorldModel mode。

---

# 257. Codex 实施原则

每 Phase：

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
```

---

# 258. Codex 禁止

不得：

- 重写 v1/v2/v3；
- 新增平台耦合；
- 把 WorldModel 做成知识图谱大工程；
- 引入 Neo4j；
- 强制引入 vector DB；
- 持续 background simulation；
- 保存 hidden chain-of-thought；
- 自动执行 simulation action；
- 做人类心理猜测系统；
- 自动生产部署。

---

# 259. 兼容性测试

每 Phase 继续保证：

- direct reply；
- group reply；
- ReplyTicket；
- Stop；
- Reminder；
- AgentTask；
- Tool；
- proactive；
- OpenLoop；
- Mind v2；
- Executive v3；
- CLI host；

不回归。

---

# 260. v4 成功指标

完成后应表现：

1. 能明确 Unknown。
2. 不把猜测当事实。
3. 能维护当前 Situation。
4. 能识别状态过期。
5. 能理解简单时间关系。
6. 能感知 Host / Tool 可用性。
7. 群聊插话考虑 SocialScene。
8. 高价值候选行为可以做有限预测。
9. Tool / Plan 失败后利用 WorldState 改计划。
10. Prediction 结果可以被真实 Observation 校准。
11. Simulation 永远不产生真实副作用。
12. QQ 移除后 WorldModel 仍能在 CLI / App Host 运行。

---

# 261. v4 失败指标

如果实现后：

```text
用户不回复
→ 生成 8 个心理假设

每条消息
→ simulation

每个状态
→ 永久存数据库

所有概率
→ 精确到小数点四位

WorldModel
→ 直接发消息

Simulation
→ 调真实 Tool

QQ ID
→ 进入 Core world entity 主键
```

说明设计失败。

---

# 262. 最终 Architecture

```text
                           WORLD
                             │
                             ▼
                          ADAPTER
                             │
                             ▼
                        WORLD EVENT
                             │
                             ▼
                          OBSERVE
                             │
             ┌───────────────┴───────────────┐
             ▼                               ▼
          YUNXI MIND                   WORLD MODEL
      “内部持续状态”                 “外部状态估计”
             │                               │
   ┌─────────┼─────────┐           ┌─────────┼─────────┐
   ▼         ▼         ▼           ▼         ▼         ▼
 Belief   Interest   Agenda      Entity   Situation Hypothesis
   │         │         │           │         │         │
   └─────────┴────┬────┘           └─────────┴────┬────┘
                  │                               │
                  └──────────────┬────────────────┘
                                 ▼
                         YUNXI EXECUTIVE
                                 │
                 ┌───────────────┼───────────────┐
                 ▼               ▼               ▼
              Priority         Plan          Candidate
                 │               │               │
                 └───────────────┼───────────────┘
                                 ▼
                           PREDICT / SIMULATE
                                 │
                                 ▼
                              PLANNER
                                 │
                                 ▼
                               ACTION
                                 │
                                 ▼
                                WORLD
```

---

# 263. 最终 Definition of Done

## Observation

- 有来源；
- 有时间；
- 有 confidence；
- 可追踪。

## EntityState

- 平台无关；
- 有 freshness；
- bounded。

## Situation

- 有生命周期；
- 可 transition；
- 可 resolve；
- 可关联 Goal / OpenLoop。

## Hypothesis

- 明确非事实；
- 有证据；
- 有 confidence；
- 可 reject；
- 可 expire。

## Temporal

- 支持 time point / range；
- 支持 stale；
- 支持 timeline。

## SocialScene

- 能判断 addressed / floor / interruption；
- 不做过度心理推断。

## Environment

- 能表示 Host / Tool / Runtime 可用性；
- TTL 正确。

## Prediction

- 候选 Action 前可有限预测；
- 有 uncertainty；
- 可和实际结果比较。

## Simulation

- 只用于少量高价值 candidate；
- shallow；
- bounded；
- 无真实副作用。

## Causal

- 局部；
- 有证据；
- 有 scope；
- 不做心理过拟合。

## Integration

- Mind 提供内部目标；
- WorldModel 提供外部状态；
- Executive 做取舍；
- Planner 生成行为；
- ActionResult 回到 WorldModel。

## Portability

WorldModel 不依赖：

- QQ；
- Kovi；
- OneBot；
- PostgreSQL Client；
- GUI。

---

# 264. 最终行为验收

完成 v4 后：

系统不仅能：

```text
“我想做什么？”
```

还应该能在高价值情况下考虑：

```text
“外部世界现在是什么状态？”
“哪些东西我其实不知道？”
“这个判断有多可靠？”
“如果现在做 A，可能发生什么？”
“如果做 B 呢？”
“这个环境现在允许我做什么？”
“我之前的预测和实际结果一致吗？”
```

然后再：

```text
Decision
→ Action
```

---

# 265. 最重要的一句话

Yunxi World Model v4 的目标不是：

> 让模型更会幻想未来。

而是：

> 让 Yunxi Core 在行动之前拥有一个受约束、可校准、带不确定性的外部世界估计。

v1：

```text
我能够持续存在与行动。
```

v2：

```text
我有持续的内部心智状态。
```

v3：

```text
我能管理这些状态、优先级与计划。
```

v4：

```text
我能区分自己内部想法与外部世界，并在高价值行动前有限估计可能后果。
```

如果 v4 最终只是：

```text
多调用一个模型问“接下来会发生什么？”
```

那么它没有实现目标。

只有当：

```text
Observation
State
Unknown
Hypothesis
Freshness
Situation
Prediction
Simulation
Prediction Error
```

能够真实影响未来 Decision，

Yunxi World Model v4 才算完成。

---

# 266. v4 之后的架构原则

v4 完成后：

停止继续增加抽象认知层。

后续优先投入：

- 模型质量；
- 延迟；
- Token 成本；
- Memory retrieval；
- WorldModel calibration；
- 行为评测；
- 长期运行稳定性；
- Voice；
- TTS；
- Live2D；
- Desktop / Mobile；
- Vision；
- Game integration；
- UX；
- observability。

不要继续创建：

```text
V5 MetaWorld
V6 SuperMind
V7 HyperExecutive
```

四层已经足够形成完整 Agent 闭环。

接下来最重要的是：

> **让这四层真实工作得好。**


# Message Collision as World State


## 1. 新增 Conversation Concurrency Observation

World Model v4 可接收：

```text
MessageCollisionDetected
ConversationFloorChanged
PendingQuestionAnswered
OutgoingCommitted
```

---

## 2. MessageCollisionDetected

当：

```text
Outgoing 已 Committed
+
Incoming message 几乎同时发生
```

产生：

```text
WorldEvent::MessageCollisionDetected
```

建议：

```rust
pub struct MessageCollision {
    pub conversation_id: ConversationId,
    pub incoming_message_id: MessageId,
    pub outgoing_message_id: MessageId,
    pub delta_ms: i64,
    pub outgoing_was_committed: bool,
}
```

---

## 3. Collision 是正常 World Event

禁止将 Message Collision：

- 当成系统异常；
- 每次自动撤回；
- 每次自动道歉；
- 每次固定输出“撞消息了”。

它只是：

```text
Conversation State changed
```

的一种形式。

---

## 4. SocialScene 更新

Collision 可以短期影响：

```text
current_floor
recent_speaking_order
conversation_version
interruption_cost
```

但不自动产生心理判断。

---

## 5. 禁止心理过拟合

碰撞不能自动推断：

```text
用户生气
用户着急
用户不耐烦
用户故意打断
```

除非有独立明确证据。

---

## 6. PendingQuestionAnswered

如果新用户消息已经回答 PendingOutgoing 的核心问题：

World Model 可以产生：

```text
PendingQuestionAnswered
```

供 Executive 快速 revalidate。

例如：

```text
Pending:
“面试怎么样？”

Incoming:
“我面试过了！”
```

---

## 7. WorldModel 不负责 Cancel / Send

WorldModel 只负责：

```text
观察外部世界
更新 SocialScene / Situation / Observation
```

不负责：

```text
Cancel PendingOutgoing
Commit Outgoing
Send Message
```

这些分别属于 Executive / Core。

---

## 8. Collision 与 Prediction

高价值主动消息在 commit 前可将：

```text
conversation activity
recent incoming rate
social floor
```

纳入 interruption prediction。

但普通 direct reply 不需要额外 Simulation。

---

## 9. 时间窗口

near-simultaneous threshold 可配置，例如：

```text
<= 1500 ms
```

只作为 Conversation / SocialScene 信号。

不能作为强语义事实。

---

## 10. 测试补充

至少：

- committed outgoing + incoming → collision event
- prepared but not committed → no collision event, use revalidation
- collision updates SocialScene
- collision 不生成心理 Hypothesis
- PendingQuestionAnswered detected
- collision event does not trigger automatic reply
- world snapshot exposes relevant concurrency state

---

## 11. 核心结论

World Model v4 的新增职责是：

> **让系统知道“刚刚双方几乎同时说了话”这个外部事实。**

而不是：

> **替 Executive 决定下一句说什么。**
