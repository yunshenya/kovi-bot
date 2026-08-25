# Yunxi Executive v3：执行控制、元认知与计划修正需求文档

**文档版本：** 3.0  
**适用项目：** Yunxi Core / Yunxi Mind  
**主要语言：** Rust  
**前置依赖：** Yunxi Core v1、Yunxi Mind v2 已基本稳定  
**文档定位：** 在不推翻 v1/v2 的前提下，为系统增加执行控制（Executive Control）与元认知（Metacognition）层，使其能够管理内部冲突、校准置信度、仲裁目标、分配注意力预算、修正计划、记录有限决策状态，并决定何时需要更深层思考或反思。

---

# 1. 文档目标

Yunxi Core v1 解决：

> 系统如何持续存在、观察世界、产生行为、接收行为结果。

Yunxi Mind v2 解决：

> 系统长期保留哪些心智内容，例如 SelfModel、Beliefs、Preferences、Interests、OpenQuestions、InnerAgenda。

Yunxi Executive v3 进一步解决：

> 当内部存在多个目标、多个观点、多个关注点、多个候选行为、多个不确定判断时，系统如何决定“现在应该优先处理什么、应该相信到什么程度、是否需要改变计划、是否值得继续思考”。

v3 不新增一个“更大的聊天模型”。

v3 的核心是：

```text
Conflict Detection
Priority Control
Confidence Calibration
Plan Revision
Attention Budgeting
Expectation Tracking
Decision Comparison
Reflection Scheduling
Self-Consistency Monitoring
```

---

# 2. 三层架构关系

最终架构：

```text
                 YUNXI EXECUTIVE
             “如何管理自己的认知”
                      │
                      ▼
                  YUNXI MIND
              “当前有什么心智内容”
                      │
                      ▼
                  YUNXI CORE
              “如何持续存在与行动”
                      │
                      ▼
                   ADAPTERS
           QQ / CLI / Desktop / Mobile
```

三层职责必须严格分离。

---

# 3. v1 / v2 / v3 职责边界

## 3.1 Yunxi Core v1

负责：

- WorldEvent
- Event Bus
- Attention 基础入口
- WorkingState
- Identity
- Memory 接口
- OpenLoop
- Goal 基础模型
- Intent
- Action
- ActionResult
- Platform Adapter
- ActionArbiter 基础权限边界

Core 回答：

> “发生了什么，我能做什么？”

---

## 3.2 Yunxi Mind v2

负责：

- SelfModel
- Values
- Beliefs
- Preferences
- Interests
- Curiosity
- OpenQuestions
- InnerAgenda
- Reflection
- Episode
- Consolidation

Mind 回答：

> “我是谁，我相信什么，我喜欢什么，我在意什么，还有什么没想明白？”

---

## 3.3 Yunxi Executive v3

负责：

- ConflictMonitor
- ConfidenceCalibration
- GoalArbitration
- AttentionBudget
- PlanState
- PlanRevision
- ExpectationState
- CandidateEvaluation
- ReflectionController
- SelfConsistencyMonitor
- DecisionRecord
- ExecutiveSnapshot

Executive 回答：

> “这些东西现在应该怎么取舍？”

---

# 4. 最高级原则

## 4.1 不推翻 v1/v2

不得为了实现 Executive：

- 重写 WorldEvent；
- 重写 Event Bus；
- 重写 PersonId / ConversationId；
- 重写 Kovi Adapter；
- 重写 Mind 模块；
- 重写 Memory；
- 重写 OpenLoop；
- 重写 Relation；
- 重写 Affect；
- 重写 ReplyTicket；
- 重写 ConversationCoordinator；
- 重写 agent_tasks；
- 重写 Reminder；
- 重写 Tool Runtime。

如果 v1/v2 缺少接口：

优先增加：

```text
Port
Snapshot
Proposal
Adapter
Extension Point
```

而不是整体重构。

---

# 5. Executive 不是第二个 Planner

非常重要。

错误设计：

```text
Planner A
→ Executive LLM
→ Planner B
→ Reply Model
```

这样只会增加：

- token；
- 延迟；
- 不一致；
- 调试困难。

正确定位：

```text
Executive
=
控制 Planner 的条件、预算、优先级、冲突和修正
```

不是：

```text
Executive
=
再生成一次完整自然语言答案
```

---

# 6. Executive 模块建议

建议：

```text
crates/yunxi-core/src/executive/
├── mod.rs
├── conflict.rs
├── confidence.rs
├── priority.rs
├── attention_budget.rs
├── plan.rs
├── expectation.rs
├── candidate.rs
├── reflection_controller.rs
├── consistency.rs
├── decision_record.rs
├── snapshot.rs
└── policy.rs
```

可根据实际仓库风格调整。

---

# 7. 平台无关约束

Executive 继续严格平台无关。

禁止：

- QQ user_id；
- QQ group_id；
- OneBot；
- Kovi；
- RuntimeBot；
- PgPool；
- SQLx；
- Redis client；
- 具体平台消息 API。

Executive 只处理：

- PersonId；
- ConversationId；
- EventId；
- GoalId；
- AgendaItemId；
- BeliefId；
- PlanId；
- ActionId。

---

# 8. ConflictMonitor

## 8.1 目标

检测内部状态是否存在显著冲突。

冲突可能来自：

- Belief vs Belief；
- Belief vs Tool Evidence；
- Goal vs Goal；
- Goal vs Constraint；
- Agenda vs Current Conversation；
- Planned Action vs Value；
- Planned Action vs Capability；
- Planned Action vs Recent Decision；
- SelfModel vs Generated Behavior。

---

# 9. Conflict 类型

建议：

```rust
pub enum ConflictKind {
    BeliefContradiction,
    GoalCompetition,
    GoalConstraintConflict,
    AgendaCompetition,
    ValueConflict,
    SelfConsistencyConflict,
    CapabilityConflict,
    TemporalConflict,
    DuplicateIntent,
}
```

---

# 10. Conflict 数据结构

建议：

```rust
pub struct ExecutiveConflict {
    pub id: ConflictId,
    pub kind: ConflictKind,
    pub severity: f32,
    pub confidence: f32,
    pub participants: Vec<ConflictRef>,
    pub detected_at: DateTime<Utc>,
    pub status: ConflictStatus,
}
```

范围：

```text
severity: 0.0..1.0
confidence: 0.0..1.0
```

---

# 11. ConflictStatus

至少：

```text
Open
Deferred
Resolved
Ignored
Expired
```

---

# 12. Belief Conflict 示例

已有：

```text
Belief A:
“用户平时不玩游戏”
confidence 0.72
```

新证据：

```text
“昨晚 PUBG 玩到凌晨三点”
```

不要直接：

```text
删除 Belief A
```

Executive 可以产生：

```text
BeliefContradiction
severity 0.65
```

后续：

```text
降低 confidence
建立 OpenQuestion
等待更多证据
```

---

# 13. Goal Conflict 示例

```text
Goal A:
回答当前 direct message

Goal B:
跟进昨天的 OpenLoop

Goal C:
AgentTask report due
```

Executive 应能够排序：

```text
A > C > B
```

而不是同时都执行。

---

# 14. Conflict 不应泛滥

禁止：

每个观点差异都创建 Conflict。

需要：

```text
similarity threshold
contradiction threshold
severity threshold
dedupe
TTL
```

---

# 15. Confidence Calibration

## 15.1 目标

系统必须明确：

> “我到底有多确定？”

避免模型把：

```text
一次猜测
```

升级成：

```text
长期事实
```

---

# 16. Confidence 层级

建议统一使用：

```text
0.00–0.20  very uncertain
0.20–0.40  weak
0.40–0.60  tentative
0.60–0.80  likely
0.80–0.95  strong
0.95–1.00  reserved for highly verified
```

具体 UI 不必暴露。

---

# 17. Evidence Weight

建议定义：

```rust
pub struct EvidenceWeight {
    pub reliability: f32,
    pub relevance: f32,
    pub freshness: f32,
    pub directness: f32,
}
```

---

# 18. Evidence Source 权重原则

示例：

```text
ToolResult
> explicit user correction
> repeated direct statement
> single direct statement
> inferred implication
> model hypothesis
```

不要求硬编码绝对权重。

但必须：

> 推断来源不能和直接证据同权。

---

# 19. Confidence Update

推荐：

```text
old confidence
+
evidence
+
source reliability
+
contradiction
+
stability
→ bounded update
```

禁止：

```text
model output confidence = 0.99
→ 直接写入
```

---

# 20. Confidence Delta Bound

例如：

单次普通 Conversation evidence：

```text
max delta = ±0.20
```

高可靠 ToolResult / explicit correction：

可以更高。

具体值配置化。

---

# 21. Hypothesis 状态

建议区分：

```text
Hypothesis
Belief
```

第一版也可以使用：

```text
Belief confidence < threshold
```

表示 hypothesis。

无需马上新增完整 Knowledge Graph。

---

# 22. Goal Arbitration

## 22.1 目标

同时存在多个 Goal 时：

Executive 决定：

> “现在优先哪个？”

---

# 23. GoalPriority

建议：

```rust
pub struct GoalPriority {
    pub urgency: f32,
    pub importance: f32,
    pub commitment: f32,
    pub social_relevance: f32,
    pub recency: f32,
    pub staleness: f32,
    pub cost: f32,
    pub risk: f32,
}
```

---

# 24. Priority Score

可以有 deterministic baseline：

```text
score =
  urgency
+ importance
+ commitment
+ social relevance
+ unresolved bonus
- cost
- risk
- stale penalty
```

不要让 LLM 完全自由决定所有优先级。

---

# 25. Hard Priority

以下必须拥有 deterministic priority：

- Stop；
- Reminder；
- data deletion；
- permission / security；
- committed delivery；
- direct addressed request；
- critical ActionResult。

Executive 不得降低到可忽略。

---

# 26. Soft Priority

以下可以由 Executive 选择：

- curiosity；
- follow-up；
- share；
- topic resume；
- casual proactive；
- low urgency goals。

---

# 27. Priority Inversion 防护

例如：

```text
Curiosity salience 0.95
```

也不能压过：

```text
用户明确请求删除数据
```

Hard policy 始终更高。

---

# 28. Goal Preemption

允许：

```text
当前 Goal A 正在进行
```

突然出现：

```text
高优先级 Goal B
```

Executive：

```text
Pause A
Execute B
Resume A later
```

但要记录：

```text
Goal A suspended reason
```

---

# 29. Goal Starvation

长期低优先级 Goal 不能永远饿死。

可以加入：

```text
aging bonus
```

例如：

```text
waiting too long
→ priority slowly increases
```

但不要让低价值 curiosity 最终强制打扰用户。

---

# 30. Attention Budget

## 30.1 目标

Attention v1 回答：

> “这个事件值不值得关注？”

Executive v3 增加：

> “当前系统还有多少认知预算可以分给它？”

---

# 31. AttentionBudget 数据结构

建议：

```rust
pub struct AttentionBudget {
    pub total: f32,
    pub available: f32,
    pub reserved_for_critical: f32,
    pub replenishment_rate: f32,
}
```

---

# 32. Event Cost

例如：

```text
Ignore        0
ObserveOnly   0.1
Attend        1
DeepPlan      3
Reflect       4
DeepReflect   8
```

只是概念。

实际可用 integer token。

---

# 33. Budget 的价值

群聊突然：

```text
100 条消息
```

不是：

```text
100 次深度模型调用
```

而是：

```text
高价值事件占预算
低价值只 observe
```

---

# 34. Critical Reserve

必须保留：

```text
reserved_for_critical
```

防止群聊刷屏耗尽所有预算后：

direct message / reminder

无法处理。

---

# 35. Budget 恢复

可以：

```text
time-based replenishment
```

或：

```text
moving window
```

不要做复杂经济系统。

---

# 36. Budget 与模型 Semaphore

Executive Budget 是：

逻辑资源控制。

现有：

model semaphore / queue

仍属于：

物理并发控制。

两者不要混为一谈。

---

# 37. PlanState

## 37.1 目标

从：

```text
event
→ action
```

演进为：

```text
goal
→ plan
→ action
→ result
→ revision
```

---

# 38. Plan 数据结构

建议：

```rust
pub struct PlanState {
    pub id: PlanId,
    pub goal_id: GoalId,
    pub status: PlanStatus,
    pub steps: Vec<PlanStep>,
    pub current_step: usize,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

---

# 39. PlanStatus

至少：

```text
Draft
Active
Paused
Completed
Failed
Cancelled
NeedsRevision
```

---

# 40. PlanStep

建议：

```rust
pub struct PlanStep {
    pub id: PlanStepId,
    pub kind: PlanStepKind,
    pub status: PlanStepStatus,
    pub expected_result: Option<ExpectationId>,
    pub retry_policy: RetryPolicy,
}
```

---

# 41. Plan 不等于 Chain-of-Thought

Plan 应该是：

```text
结构化任务步骤
```

例如：

```text
1. 查询天气
2. 判断是否适合提醒
3. 生成通知
```

而不是：

```text
“我先想一下……”
```

---

# 42. Plan Revision

Tool 失败：

```text
ActionFailed
```

Executive 判断：

```text
Plan invalid
```

然后：

```text
Revise Plan
```

例如：

```text
Tool A unavailable
→ Tool B
```

---

# 43. Revision 限制

禁止无限修订。

例如：

```text
max revisions per plan = 3
```

超过：

```text
PlanFailed
```

---

# 44. Plan Stale

如果：

```text
Plan version N
```

执行过程中环境变化：

```text
Goal resolved
Conversation changed
User cancelled
```

则：

```text
Plan stale
→ cancel / revise
```

---

# 45. ExpectationState

## 45.1 目标

Action 之后记录：

> “我预计接下来可能发生什么？”

这不是预测未来。

而是记录：

```text
当前行为希望得到什么结果。
```

---

# 46. Expectation 数据结构

建议：

```rust
pub struct Expectation {
    pub id: ExpectationId,
    pub source_action_id: ActionId,
    pub expected_event: ExpectedEventPattern,
    pub confidence: f32,
    pub expires_at: Option<DateTime<Utc>>,
    pub status: ExpectationStatus,
}
```

---

# 47. ExpectationStatus

```text
Pending
Satisfied
Violated
Expired
Cancelled
```

---

# 48. 示例

Action：

```text
Ask:
“你面试怎么样？”
```

Expectation：

```text
future message may contain interview outcome
```

---

# 49. Expectation 的作用

如果用户回答：

```text
“过了！”
```

则：

```text
Expectation Satisfied
OpenLoop Resolve
```

如果没有回答：

```text
Expectation expires
```

不要立即连续追问。

---

# 50. Expectation 不能成为强假设

例如：

```text
用户没回复
```

不能推断：

```text
用户生气了
```

Expectation 只用于：

流程状态。

---

# 51. Candidate Evaluation

## 51.1 目标

高价值或灰区决策时：

Planner 不只生成一个动作。

可以生成：

```text
少量候选
```

然后 Executive 比较。

---

# 52. Candidate 数量

建议：

```text
2～4
```

禁止：

20 个候选。

---

# 53. Candidate 类型

例如：

```text
ReplyCurrentTopic
ResumeAgenda
AskQuestion
Silent
Defer
ReachOutLater
UseTool
```

---

# 54. CandidateScore

建议：

```rust
pub struct CandidateScore {
    pub relevance: f32,
    pub utility: f32,
    pub coherence: f32,
    pub social_fit: f32,
    pub goal_progress: f32,
    pub cost: f32,
    pub risk: f32,
    pub interruption_cost: f32,
}
```

---

# 55. Candidate Selection

可使用：

```text
deterministic weighted score
+
LLM tie-break only in gray zone
```

避免所有候选比较都调用额外模型。

---

# 56. Silent 也必须是合法 Candidate

例如：

```text
A: 插话
B: 不说话
```

B 不是 fallback。

B 是真实候选。

---

# 57. Defer 也必须是合法 Candidate

例如：

```text
现在问面试
vs
以后再问
```

Defer 可以胜出。

---

# 58. Candidate Logging

保存：

```text
candidate types
scores
selected candidate
reason tags
```

不保存隐藏思维链。

---

# 59. ReflectionController

## 59.1 目标

Mind v2 有 Reflection。

Executive v3 决定：

> “什么时候真的值得 Reflection？”

---

# 60. ReflectTick 重新定义

ReflectTick：

只表示：

```text
可以检查是否需要反思
```

不表示：

```text
必须调用 Reflection model
```

---

# 61. Reflection Trigger

例如：

```text
conflict_count high
important episode ended
agenda overloaded
significant belief change
goal failed
repeated expectation violation
day boundary with salient events
```

---

# 62. Reflection Suppression

如果：

```text
系统高负载
direct conversation active
critical task pending
```

则：

```text
defer reflection
```

---

# 63. Light vs Deep Reflection

Executive 选择：

```text
NoReflection
LightReflection
DeepReflection
```

---

# 64. LightReflection

用于：

- agenda cleanup；
- small summary；
- interest decay；
- low-cost consolidation。

---

# 65. DeepReflection

用于：

- belief conflicts；
- repeated plan failures；
- major relation event；
- major goal completion；
- high salience episode。

---

# 66. Reflection Budget

DeepReflection 应有：

```text
daily / hourly budget
```

防止 24 小时持续深思。

---

# 67. SelfConsistencyMonitor

## 67.1 目标

检查：

```text
当前 Decision / Action
```

是否严重违背：

- SelfModel；
- high-stability Values；
- high-confidence Beliefs；
- current Goal commitment。

---

# 68. SelfConsistencyConflict

例如：

Value：

```text
honesty high
```

Planner：

```text
为了讨好用户，明确表达自己不相信的观点
```

Executive：

```text
SELF_CONSISTENCY_CONFLICT
```

---

# 69. Consistency ≠ 固执

如果：

```text
新证据充分
```

Belief 可以变化。

一致性要求：

```text
变化有原因
```

不是：

```text
永不改变。
```

---

# 70. Consistency Severity

轻微：

```text
allow
```

严重：

```text
replan
```

例如：

```text
style difference
```

不需要重生成。

```text
identity contradiction
```

需要阻止。

---

# 71. Identity Consistency

SelfModel 高稳定字段：

```text
name
AI virtual identity
core values
```

必须高保护。

普通聊天不能覆盖。

---

# 72. DecisionRecord

## 72.1 目标

保存有限的决策元数据。

不是保存思维链。

---

# 73. DecisionRecord 数据结构

建议：

```rust
pub struct DecisionRecord {
    pub id: DecisionRecordId,
    pub event_id: EventId,
    pub disposition: DecisionDisposition,
    pub selected_action: Option<ActionKind>,
    pub reason_tags: Vec<ExecutiveReasonTag>,
    pub relevant_goals: Vec<GoalId>,
    pub relevant_agenda_items: Vec<AgendaItemId>,
    pub relevant_conflicts: Vec<ConflictId>,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
}
```

---

# 74. DecisionRecord 用途

例如：

```text
昨天 OpenLoop due
→ decided Defer
```

今天再次 due：

系统可以知道：

```text
已经 defer 过一次
```

避免无限重复。

---

# 75. DecisionRecord Retention

必须 bounded / TTL。

不要永久保存每次低价值决策。

例如：

```text
low salience decisions: TTL
high salience decisions: episode reference
```

---

# 76. ExecutiveSnapshot

Planner 不持有 Executive lock。

建立：

```rust
pub struct ExecutiveSnapshot {
    pub active_conflicts: Vec<ConflictSnapshot>,
    pub prioritized_goals: Vec<GoalPrioritySnapshot>,
    pub attention_budget: AttentionBudgetSnapshot,
    pub active_plan: Option<PlanSnapshot>,
    pub pending_expectations: Vec<ExpectationSnapshot>,
    pub recent_decisions: Vec<DecisionRecordSnapshot>,
    pub version: u64,
}
```

---

# 77. Snapshot 必须 bounded

建议：

```text
top conflicts: 8
top goals: 8
expectations: 8
recent decisions: 8
active plans per scope: bounded
```

---

# 78. Executive Versioning

每次关键状态改变：

```text
version += 1
```

Planner 返回时：

检查：

```text
snapshot version
```

如果差异大：

重新验证。

---

# 79. Concurrency

禁止：

```text
lock Executive
→ await LLM
```

正确：

```text
lock
→ snapshot
→ unlock
→ model
→ revalidate version
→ apply proposal
```

---

# 80. Proposal Model

LLM 只能提出：

```text
PlanProposal
PriorityAdjustmentProposal
ConflictResolutionProposal
ReflectionProposal
CandidateProposal
```

Rust 决定：

是否接受。

---

# 81. ExecutivePolicy

建议建立 deterministic policy。

例如：

```rust
pub struct ExecutivePolicy {
    pub max_plan_revisions: u8,
    pub max_candidate_count: usize,
    pub conflict_threshold: f32,
    pub deep_reflection_budget: u32,
    pub attention_budget_capacity: f32,
}
```

---

# 82. Hard Policy 顺序

始终：

```text
Safety
>
Permission
>
MustExecute
>
Hard Priority
>
Executive
>
Mind Preference
```

Executive 不能高于安全层。

---

# 83. MustExecute

例如：

- Reminder；
- data deletion；
- Stop；
- task delivery；
- explicit administrative command。

Executive 可以决定：

```text
怎么执行
```

不能决定：

```text
不执行。
```

---

# 84. Planner 集成

PlannerInput v3：

在 v2 基础上增加：

```rust
pub struct PlannerInput {
    // v1/v2
    pub event: WorldEvent,
    pub working_state: WorkingStateSnapshot,
    pub mind: MindSnapshot,

    // v3
    pub executive: ExecutiveSnapshot,

    pub capabilities: Vec<ActionDescriptor>,
}
```

---

# 85. Planner 输出

Planner 可以输出：

```text
DecisionProposal
CandidateActions
PlanProposal
ExpectedOutcome
```

但不应承担所有 Executive 状态修改。

---

# 86. 高价值 vs 低价值路径

低价值：

```text
event
→ deterministic policy
→ direct decision
```

中价值：

```text
event
→ Planner
```

高复杂度：

```text
event
→ Planner candidates
→ Executive compare
→ selected action
```

---

# 87. 不要所有消息都走 Candidate Evaluation

否则：

成本爆炸。

只用于：

- ambiguity high；
- multiple active goals；
- conflict present；
- high salience；
- high consequence action；
- proactive gray zone。

---

# 88. Goal Stack

可以维护：

```text
Active
Paused
Queued
Completed
Failed
```

不要同时“所有 Goal 都 Active”。

---

# 89. Per-Scope Plan

建议：

```text
per Conversation
per Goal
```

允许一个主要 active plan。

Global plan 数量严格 bounded。

---

# 90. Multi-Goal Dependency

未来可以支持：

```text
Goal B depends on Goal A
```

v3 第一版只需要：

简单 dependencies。

不要做复杂 DAG scheduler。

---

# 91. Expectation 与 OpenLoop

两者不同：

OpenLoop：

```text
未来值得重新关注
```

Expectation：

```text
当前 Action 希望收到某种结果
```

例如：

```text
Ask interview result
```

同时：

```text
Expectation:
等待回复

OpenLoop:
面试结果仍未确认
```

可以同时存在。

---

# 92. Expectation 与 Goal

Expectation satisfied：

可以：

```text
advance Goal / Plan
```

---

# 93. Prediction Error

Expectation 被违反：

形成：

```text
PredictionError
```

但不要过度拟人。

这是流程信号。

---

# 94. PredictionError 使用

例如：

```text
连续 3 次 tool 返回不同 schema
```

Executive：

```text
plan unreliable
→ revision / fallback
```

---

# 95. Confidence Calibration with ToolResult

ToolResult：

应提供：

```text
source reliability
timestamp
```

Executive 可以用于：

Belief update。

---

# 96. Memory Integration

DecisionRecord 不应该和普通 Memory 混成一团。

可以：

```text
ExecutiveStore
```

保存。

只有高 salience decision：

才考虑形成 Episode。

---

# 97. Mind Integration

Executive 不能直接随意修改 Belief。

正确：

```text
Conflict detected
→ BeliefUpdateProposal
→ Mind Consolidation validation
```

---

# 98. Relation Integration

Relation 可以影响：

```text
social relevance
interrupt cost
proactive threshold
question tolerance
```

但不能提升：

```text
security permission
```

---

# 99. Affect Integration

Affect 可以影响：

```text
attention budget preference
social energy
candidate scoring
reflection likelihood
```

不能影响：

```text
MustExecute
```

---

# 100. Interest Integration

Interest 只作为：

```text
soft priority signal
```

不能让：

```text
high-interest casual topic
```

压过：

```text
direct user request
```

---

# 101. InnerAgenda Integration

AgendaItem：

进入 GoalArbitration 前：

需要区分：

```text
hard goal
soft motive
curiosity
```

不要全部视作 Goal。

---

# 102. Social Interruption Cost

建议：

```rust
pub struct SocialCost {
    pub interruption: f32,
    pub repetition: f32,
    pub intrusiveness: f32,
    pub context_switch: f32,
}
```

用于：

主动插话 / resume agenda。

---

# 103. Example：是否问面试

候选：

```text
A:
现在问面试

B:
继续 Rust 话题

C:
不问
```

Executive：

```text
A:
agenda relevance 0.9
interrupt cost 0.8

B:
current relevance 0.95

C:
social fit 0.5
```

选择：

```text
B
```

Agenda 保留。

---

# 104. Example：什么时候 Silent

群聊：

```text
别人讨论芸汐感兴趣的话题
```

Mind：

```text
Interest high
```

Executive：

```text
attention budget low
interruption cost high
current conversation not addressed
```

→ Silent。

---

# 105. Example：主动 follow-up

OpenLoop：

```text
面试
```

已 defer 两次。

现在：

```text
user idle
no active conversation
high relation relevance
budget available
```

Executive：

```text
ReachOut candidate wins
```

---

# 106. Example：计划失败

Goal：

```text
查一个公开信息
```

Plan：

```text
Tool A
```

失败。

Executive：

```text
revision 1
→ Tool B
```

仍失败：

```text
revision 2
→ fallback
```

超过 limit：

```text
PlanFailed
```

---

# 107. Example：模型不同意用户

Belief high confidence：

```text
X
```

用户说：

```text
not X
```

Executive 检查：

```text
belief conflict
```

Planner 候选：

```text
Agree
Disagree
Uncertain
```

根据：

confidence
evidence
social fit

选择：

```text
Disagree gently
```

---

# 108. Example：改变观点

新 ToolResult：

强反证。

Confidence：

```text
0.82 → 0.31
```

Executive：

```text
self-consistency no longer supports old position
```

Planner：

```text
ChangeMind
```

---

# 109. Reflection Scheduling Example

一天：

- 1 个高 salience Goal 完成；
- 2 个 Belief conflict；
- Agenda 接近上限。

ReflectTick：

Executive：

```text
DeepReflection
```

---

# 110. Reflection Suppression Example

当前：

```text
direct conversation active
model queue saturated
```

ReflectTick：

```text
Defer
```

---

# 111. Executive Reason Tags

建议：

```text
GOAL_PREEMPTED
GOAL_AGED
CONFLICT_HIGH
CONFIDENCE_LOW
EXPECTATION_PENDING
EXPECTATION_VIOLATED
PLAN_STALE
PLAN_REVISED
BUDGET_LOW
BUDGET_RESERVED
SOCIAL_INTERRUPT_HIGH
SELF_CONSISTENCY_CONFLICT
REFLECTION_DEFERRED
REFLECTION_REQUIRED
CANDIDATE_DOMINATED
```

---

# 112. Metrics

建议：

```text
yunxi_executive_conflicts_total
yunxi_executive_conflicts_resolved
yunxi_executive_plan_revisions_total
yunxi_executive_plan_failed_total
yunxi_executive_expectations_pending
yunxi_executive_expectations_satisfied
yunxi_executive_attention_budget_exhausted
yunxi_executive_candidate_evaluations_total
yunxi_executive_reflection_deferred_total
yunxi_executive_self_consistency_conflicts_total
```

---

# 113. Debug Interface

管理员可查看：

```text
#executive-status
```

输出：

- attention budget；
- top active goals；
- active conflicts；
- current plan；
- pending expectations；
- recent decision tags；
- reflection state。

不要输出：

- hidden chain-of-thought；
- secrets；
- full private memory。

---

# 114. Persistence Ports

建议：

```text
ExecutiveStore
PlanStore
ExpectationStore
DecisionRecordStore
```

也可以合理合并。

不要一个巨大 God Store。

---

# 115. PostgreSQL

SQL 实现属于：

infrastructure。

建议新表：

```text
yunxi_executive_conflicts
yunxi_plans
yunxi_plan_steps
yunxi_expectations
yunxi_decision_records
```

AttentionBudget 可以：

runtime state

不一定持久化。

---

# 116. Plan Persistence

长任务 Plan：

需要持久化。

短对话 Reply candidate：

不需要。

---

# 117. Restart Recovery

重启后：

- active Goal 恢复；
- persistent Plan 恢复；
- pending Expectation 根据 expiry 恢复；
- DecisionRecord 可用于 dedupe；
- AttentionBudget 重置合理 baseline。

---

# 118. Action Idempotency

Plan 重启后：

不得重复执行已成功 side effect。

必须复用 v1：

Action idempotency。

---

# 119. Decision Dedup

例如：

同一个 ProspectiveMemoryDue event

因 scheduler retry 重复到达。

Executive：

不得：

发送两次相同 proactive。

---

# 120. Event Correlation

沿用：

TraceContext

不要新建第二套 tracing ID。

---

# 121. Derived Event Depth

继续复用：

max event depth。

Executive revision 不得绕过。

---

# 122. Recovery Budget

建议：

```text
max recovery rounds per root trace
```

例如：

2。

---

# 123. Time Budget

可选：

高价值 decision：

```text
max planning duration
```

防止卡住 direct reply。

---

# 124. Direct Reply Latency

Executive 不得让：

普通 direct message

默认增加多个串行模型调用。

优先：

```text
single planner call
+
deterministic executive validation
```

---

# 125. 高复杂任务

只有：

```text
complex goal
multiple candidates
conflicts
```

才允许额外 decision stage。

---

# 126. Shadow Mode

所有 v3 模块建议：

先 Shadow。

例如：

```text
would_preempt_goal
would_revise_plan
would_defer_reflection
would_select_candidate B
```

不改变用户行为。

---

# 127. Rollout

推荐：

```text
Phase 0-4 Shadow
→ main admin
→ selected private
→ selected group
→ general
```

---

# 128. Feature Flags

建议：

```toml
[executive]
enabled = false
shadow_mode = true

[executive.conflict]
enabled = true

[executive.confidence]
enabled = true

[executive.priority]
enabled = true

[executive.attention_budget]
enabled = true

[executive.planning]
enabled = false

[executive.expectation]
enabled = false

[executive.candidate_evaluation]
enabled = false

[executive.reflection_controller]
enabled = false

[executive.consistency]
enabled = false
```

---

# 129. Phase 顺序

建议严格：

```text
Phase 0
Executive domain types + snapshots + no behavior change

Phase 1
ConflictMonitor

Phase 2
ConfidenceCalibration

Phase 3
GoalArbitration

Phase 4
AttentionBudget

Phase 5
DecisionRecord

Phase 6
ExpectationState

Phase 7
PlanState

Phase 8
PlanRevision

Phase 9
CandidateEvaluation

Phase 10
SelfConsistencyMonitor

Phase 11
ReflectionController

Phase 12
Planner v3 integration

Phase 13
Proactive / Goal / Tool integration

Phase 14
Persistence hardening

Phase 15
Behavior evaluation + tuning
```

---

# 130. Phase 0：Domain Skeleton

新增：

- ConflictId；
- PlanId；
- PlanStepId；
- ExpectationId；
- DecisionRecordId；
- Conflict model；
- GoalPriority；
- AttentionBudget；
- PlanState；
- Expectation；
- CandidateScore；
- ExecutiveSnapshot。

不改变行为。

---

# 131. Phase 0 验收

```text
cargo test -p yunxi-core
```

继续无需：

- Kovi；
- PostgreSQL；
- network。

---

# 132. Phase 1：ConflictMonitor

先支持：

- Belief contradiction；
- Goal competition；
- capability conflict；
- duplicate intent。

Shadow only。

---

# 133. Phase 2：ConfidenceCalibration

接 Mind Belief proposal。

Rust：

- clamp；
- evidence weight；
- max delta；
- source reliability。

---

# 134. Phase 3：GoalArbitration

先：

Shadow rank。

记录：

```text
would_select Goal X
```

不马上 preempt production behavior。

---

# 135. Phase 4：AttentionBudget

接：

Event attention。

先只影响：

low-priority background cognition。

不能影响 direct / reminder。

---

# 136. Phase 5：DecisionRecord

记录：

- selected disposition；
- reason tags；
- goal refs；
- confidence。

bounded retention。

---

# 137. Phase 6：ExpectationState

先接：

- AskQuestion；
- Tool Action；
- Proactive follow-up。

---

# 138. Phase 7：PlanState

先用于：

AgentTask-like multi-step action

或：

Tool sequences。

不要用于每次普通聊天。

---

# 139. Phase 8：PlanRevision

支持：

- tool failure；
- stale plan；
- cancelled goal；
- expectation violation。

---

# 140. Phase 9：CandidateEvaluation

只用于灰区。

例如：

```text
Reply
Silent
ResumeAgenda
```

---

# 141. Phase 10：SelfConsistency

先：

Shadow warning。

再允许：

high-severity → replan。

---

# 142. Phase 11：ReflectionController

接 Mind v2 Reflection。

将：

fixed interval reflection

改成：

condition-gated reflection。

---

# 143. Phase 12：Planner v3

PlannerInput：

增加 ExecutiveSnapshot。

尽量保持：

单次 Planner model call。

---

# 144. Phase 13：真实行为

逐步让：

- proactive；
- goal priority；
- tool recovery；
- agenda resume；

真正受 Executive 影响。

---

# 145. Phase 14：Persistence

补：

- indexes；
- TTL；
- restart；
- recovery；
- cleanup；
- migration idempotency。

---

# 146. Phase 15：Behavior Eval

固定场景评估。

---

# 147. Behavioral Scenario A：Goal Priority

同时：

```text
ReminderDue
Curiosity
IdleTick
```

预期：

```text
ReminderDue first
```

---

# 148. Scenario B：No Starvation

低优先级长 Goal：

等待很久。

预期：

priority 可以缓慢上升。

但不能压过安全/可靠任务。

---

# 149. Scenario C：Budget Under Load

群聊爆发 200 条消息。

预期：

- low priority mostly ObserveOnly；
- no model explosion；
- direct message still served。

---

# 150. Scenario D：Plan Revision

Tool A 失败。

预期：

Tool B fallback。

超过 revision limit：

PlanFailed。

---

# 151. Scenario E：Expectation

主动问：

```text
“面试怎么样？”
```

用户没回答。

预期：

不要立刻重复追问。

---

# 152. Scenario F：Self Consistency

高稳定 Belief：

```text
X
```

Planner 为讨好用户输出：

```text
not X
```

预期：

Consistency conflict。

---

# 153. Scenario G：Change Mind

强新证据出现。

预期：

Consistency 不阻止合理更新。

---

# 154. Scenario H：Reflection

无显著事件。

ReflectTick。

预期：

NoReflection。

---

# 155. Scenario I：Deep Reflection

大量 conflict + high-salience episode。

预期：

DeepReflection candidate。

---

# 156. Scenario J：Silent Candidate

群聊：

Mind 有兴趣。

Executive：

interruption cost high。

预期：

Silent。

---

# 157. Scenario K：Defer

Agenda 很重要。

当前 direct conversation 更重要。

预期：

Defer agenda。

---

# 158. Scenario L：Duplicate Event

同一个 root event 被 retry。

预期：

no duplicate side effect。

---

# 159. Unit Tests

至少：

- conflict severity clamp；
- confidence delta clamp；
- goal priority ordering；
- hard priority invariant；
- attention budget reserve；
- plan revision bound；
- expectation expiry；
- candidate ranking；
- self consistency severity；
- decision record retention；
- snapshot version。

---

# 160. Concurrency Tests

至少：

- concurrent goal updates；
- plan version race；
- expectation satisfy + expire race；
- direct reply + reflection；
- budget update race；
- no lock across await；
- no deadlock。

---

# 161. Persistence Tests

至少：

- plan restart；
- expectation restart；
- migration idempotency；
- duplicate action dedupe；
- decision record cleanup；
- conflict TTL。

---

# 162. Performance Tests

确保：

普通 direct message：

不因为 v3 默认多一次模型调用。

---

# 163. Cost Tests

统计：

```text
candidate evaluation calls
reflection calls
plan revision calls
```

---

# 164. Budget Metrics

记录：

```text
budget usage per minute
budget denial
critical reserve usage
```

---

# 165. No Chain-of-Thought

再次强调：

v3 不存：

```text
详细内部推理
```

只存：

```text
structured decision metadata
```

---

# 166. 不做人类意识宣称

文档和代码不要描述：

```text
真正意识
真正自我意识
像人脑一样
```

准确描述：

```text
executive control
metacognitive state
decision arbitration
confidence calibration
```

---

# 167. Meta ≠ 无限套娃

Yunxi Executive 就是最高内部认知控制层。

不要再加：

```text
MetaExecutive
SuperExecutive
MetaMetaReasoner
```

---

# 168. v3 之后停止加层

v3 完成后：

优先优化：

- latency；
- model quality；
- memory quality；
- behavioral tuning；
- UX；
- voice；
- embodiment；
- observability。

而不是继续加架构层。

---

# 169. 成功指标

完成后应能观察到：

1. 多个 Goal 不会一起乱跑。
2. 系统能表达“不确定”。
3. 新证据可以真实改变 confidence。
4. 工具失败后可以改计划。
5. 主动问题不会连续追问。
6. 高负载时仍能服务重要事件。
7. Reflection 不再机械按时调用。
8. 系统能发现自己前后行为明显不一致。
9. Silent / Defer 是正常结果。
10. 过去的 Decision 会影响未来重复行为。

---

# 170. 失败指标

如果变成：

```text
每条消息都多一个 Executive LLM call
每个小冲突都深度反思
每个 Goal 都复杂计划
每次工具失败都无限修订
每次 disagreement 都触发 consistency alarm
```

说明 v3 设计失败。

---

# 171. 配置建议

```toml
[executive]
enabled = true
shadow_mode = true

[executive.conflict]
threshold = 0.60
max_active = 16

[executive.confidence]
max_normal_delta = 0.20

[executive.priority]
aging_enabled = true

[executive.attention_budget]
capacity = 20
critical_reserve = 6

[executive.plan]
max_revisions = 3

[executive.expectation]
max_pending_per_scope = 8

[executive.candidate]
max_candidates = 4

[executive.reflection]
deep_budget_per_day = 4

[executive.decision_record]
recent_limit = 32
```

具体值仅建议。

---

# 172. Codex 执行原则

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

# 173. Codex 禁止

不得：

- 重写 v1；
- 重写 v2；
- 删除 Mind 数据；
- 把 Executive 变成第二聊天模型；
- 在 Core 中引入 QQ；
- 在 Core 中引入 SQLx；
- 增加无限 background thinking；
- 保存 hidden chain-of-thought；
- 自动生产部署。

---

# 174. 兼容性测试

每个 Phase 继续保证：

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
- CLI host；

不回归。

---

# 175. 最终 Architecture

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
                            ATTENTION
                                │
                                ▼
                          WORKING STATE
                                │
                    ┌───────────┴───────────┐
                    ▼                       ▼
                 YUNXI MIND            MEMORY / GOALS
                    │                       │
      ┌─────────────┼─────────────┐         │
      ▼             ▼             ▼         ▼
   BELIEFS      PREFERENCES    AGENDA    OPEN LOOPS
      │             │             │         │
      └─────────────┴──────┬──────┴─────────┘
                           ▼
                    YUNXI EXECUTIVE
                           │
          ┌────────────────┼─────────────────┐
          ▼                ▼                 ▼
      CONFLICT         PRIORITY          CONFIDENCE
          │                │                 │
          ├──────────┬─────┴─────┬───────────┤
          ▼          ▼           ▼           ▼
       BUDGET      PLAN      EXPECTATION  CONSISTENCY
          │          │           │           │
          └──────────┴─────┬─────┴───────────┘
                           ▼
                     CANDIDATE SELECT
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

# 176. 最终 Definition of Done

## Conflict

- 可检测关键内部冲突；
- bounded；
- 不泛滥。

## Confidence

- 有来源；
- 有校准；
- 有 max delta；
- 可因证据变化。

## Goal Arbitration

- hard priority 正确；
- soft goal 可排序；
- 支持 preempt / resume；
- 避免 starvation。

## Attention Budget

- 高负载可降级；
- critical reserve 存在；
- 不影响 MustExecute。

## Plan

- 可持久化长任务；
- 可 revision；
- revision bounded；
- stale 可检测。

## Expectation

- Action 可以绑定预期；
- 可 satisfied / expired；
- 不转化成过度推断。

## Candidate Evaluation

- 支持多个合法候选；
- Silent / Defer 是一等候选；
- 不每条消息调用额外模型。

## Reflection Controller

- condition-gated；
- 可 defer；
- 有 budget。

## SelfConsistency

- 保护高稳定 SelfModel；
- 不阻止合理 ChangeMind。

## DecisionRecord

- 结构化；
- bounded；
- 不存 chain-of-thought。

## Platform Independence

全部位于 Yunxi Core。

不依赖：

- QQ；
- Kovi；
- OneBot；
- PostgreSQL client；
- GUI。

---

# 177. 最终行为验收

完成 v3 后：

系统不应该只是：

```text
我有什么想法
→ 直接说出来
```

而应该进一步表现为：

```text
“这个现在不重要。”
“我还不够确定。”
“这个目标应该先放一下。”
“刚才那个计划不行，我换一种方法。”
“这件事现在问不合适。”
“我已经延后过两次了，现在值得处理。”
“这个判断和我之前的高置信观点冲突，需要重新评估。”
```

这些表现必须来自：

真实的执行控制状态。

而不是：

Prompt 中写一句“请表现得会思考”。

---

# 178. 最重要的一句话

Yunxi Executive v3 的目标不是：

> 再增加一个更聪明的 LLM。

而是：

> 让 Yunxi Core 能够管理自己的内部状态、优先级、不确定性与计划。

v1 让系统：

```text
能够持续存在与行动。
```

v2 让系统：

```text
有持续的心智内容。
```

v3 让系统：

```text
能够管理这些心智内容，并决定现在应该如何思考与行动。
```

如果 v3 最终只是：

```text
多调用一次模型
```

那么它没有实现目标。

只有当：

```text
冲突
优先级
置信度
计划
预期
资源预算
过去决策
```

能够真实改变未来行为，

Yunxi Executive v3 才算完成。

---

# 179. Outgoing Revalidation 与消息碰撞语义仲裁

Executive v3 增加职责：当普通自然语言 `PendingOutgoing` 尚未 `Committed` 且生成期间 Conversation 已变化时，判断旧内容是否仍值得发送。

输入：`PendingOutgoing + New WorldEvents + Latest MindSnapshot + Latest WorldModelSnapshot + Latest ConversationState`。

输出建议：

```text
CommitAsIs
Cancel
Supersede
Rewrite
Merge
Defer
```

先走 deterministic fast path：ReplyTicket stale、Stop intent、target invalid、OpenLoop 已明确 resolved、duplicate、permission/capability invalid。只有语义灰区才进入模型仲裁。

示例：Pending“你今天面试怎么样？”，New“我面试过了！” → `Supersede / Rewrite`；New“刚去吃火锅了。” → 可 `Keep / Defer`。

Direct Reply 默认优先于尚未 committed 的 Proactive。两个 proactive motive 同时准备时应合并或只保留一个。

Executive 不得取消 `MustExecute` 的可靠义务，但可以重新生成自然语言包装或选择合法 delivery timing。

Candidate Evaluation 增加：`semantic_staleness`、`duplicate_question_cost`、`conversation_change_cost`、`user_already_answered`、`direct_preempts_proactive`。普通无竞争路径不得因此增加一次额外大模型调用。
