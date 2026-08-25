# Yunxi Perception–Action Loop v7：持续感知、行动反馈与闭环世界交互开发文档

**文档状态：** 最终设计稿  
**版本：** V7  
**定位：** Yunxi V1～V6 之上的感知—行动闭环基础设施  
**目标：** 将 Yunxi 从“能接收事件、做出决策、执行动作”的持续 Agent，进一步升级为“能够持续观察外部世界、接收行动反馈、通过时间和任务事件主动重新观察，并形成完整 Observe → Model → Decide → Act → Observe 闭环”的 Agent。

---

# 1. V7 的定位

V7 不是新的认知层。

V1～V6 已经分别负责：

```text
V1 Core
→ 平台无关的 Agent 核心生命循环

V2 Mind
→ 自我、信念、兴趣、Curiosity、InnerAgenda

V3 Executive
→ 注意力、目标、计划、冲突和决策控制

V4 World Model
→ 对外部世界的状态估计、预测和模拟

V5 Model Fabric
→ 本地 / 云端 / 多模型推理基础设施

V6 Runtime Foundation
→ 多任务、多通道、Action 生命周期、恢复、降级和长期运行
```

V7 负责：

```text
外部世界如何持续进入 Yunxi
+
Yunxi 的行动结果如何重新成为新的世界信息
+
时间 / Task 如何驱动主动重新观察世界
```

---

# 2. V7 一句话原则

> **Yunxi 不是“收到消息才醒一次”，而是由世界变化、时间变化、任务变化和自己的行动反馈持续驱动。**

---

# 3. 核心闭环

V7 的核心不变量：

```text
Observe
→ Model
→ Decide
→ Act
→ Observe
```

更完整地：

```text
World
↓
Observation
↓
WorldEvent
↓
EventBus
↓
Attention
↓
WorkingState
↓
Mind + WorldModel + Executive
↓
Intent
↓
CapabilityRegistry
↓
ActionArbiter
↓
ActionLifecycle
↓
Adapter
↓
ActionResult
+
World Changed
↓
Observation
↓
WorldEvent
↓
EventBus
```

---

# 4. V7 要解决的问题

V7 必须解决：

```text
1. World → WorldEvent / EventBus 的感知闭环
2. ActionResult 如何回流系统
3. ActionResult 与真实 World Observation 的区别
4. Adapter 不得直接修改 WorldModel / Mind / WorkingState
5. Clock / Scheduler 如何驱动主动检查世界
6. TaskSupervisor 如何产生 TaskEvent
7. 不同 Observation Source 如何统一进入 EventBus
8. WorldEvent 如何避免重复和风暴
9. 状态更新必须有明确 Reducer / Updater 边界
10. 主动轮询不能演化成 while true + LLM
11. Observation 必须带 freshness / confidence / source
12. 行动结果和世界结果必须可区分
13. 对外部世界重新观察必须通过 Capability
14. 高实时场景下必须支持不同感知频率
15. WorldModel 不直接等同于 World 真相
16. Perception Loop 必须可暂停、降级、恢复和调试
```

---

# 5. V7 非目标

V7 不负责：

```text
新的 SelfModel
新的 Belief 系统
新的 Personality
新的 Goal Planner
新的 Reflection 系统
新的 World Simulation 算法
新的 Model Router
具体 FPS Vision Model
具体 ASR / OCR / CV 模型
具体游戏输入实现
具体 QQ API
```

这些仍属于 V2～V6 或具体 Adapter。

---

# 6. 总体架构

```text
                         YUNXI PERCEPTION–ACTION LOOP
                                      │
        ┌─────────────────────────────┼─────────────────────────────┐
        │                             │                             │
        ▼                             ▼                             ▼
      World                         Clock                    TaskSupervisor
        │                             │                             │
        ▼                             ▼                             ▼
   Observation                    TimeEvent                     TaskEvent
        │                             │                             │
        └─────────────────────────────┼─────────────────────────────┘
                                      ▼
                                  EventBus
                                      ↓
                                  Attention
                                      ↓
                               WorkingState
                                      │
                     ┌────────────────┼────────────────┐
                     ▼                ▼                ▼
                   Mind           WorldModel       Executive
                     │                │                │
                     └────────────────┼────────────────┘
                                      ▼
                                    Intent
                                      ↓
                             CapabilityRegistry
                                      ↓
                                ActionArbiter
                                      ↓
                              ActionLifecycle
                                      ↓
                                   Adapter
                                      │
                       ┌──────────────┴──────────────┐
                       ▼                             ▼
                  ActionResult                     World
                       │                             │
                       │                             ▼
                       │                        Observation
                       │                             │
                       └──────────────┬──────────────┘
                                      ▼
                                  WorldEvent
                                      │
                                      └──────────────→ EventBus
```

---

# 7. V7 正式模块

建议新增：

```text
Observation
ObservationEnvelope
ObservationSource
ObservationAdapter
ObservationNormalizer
ObservationQuality
ObservationFreshness
ObservationScheduler
WorldEventFactory
PerceptionPolicy
PerceptionBudget
PerceptionLoop
StateReducer
ActionFeedbackBridge
TemporalEventSource
TaskEventSource
EventDeduplicator
EventCoalescer
PerceptionHealth
PerceptionSnapshot
```

---

# 8. 目录建议

```text
crates/
└── yunxi-perception/
    ├── src/
    │   ├── lib.rs
    │   ├── observation/
    │   │   ├── mod.rs
    │   │   ├── envelope.rs
    │   │   ├── source.rs
    │   │   ├── quality.rs
    │   │   └── normalizer.rs
    │   ├── event/
    │   │   ├── mod.rs
    │   │   ├── world_event_factory.rs
    │   │   ├── dedupe.rs
    │   │   └── coalesce.rs
    │   ├── loop_runtime/
    │   │   ├── mod.rs
    │   │   ├── perception_loop.rs
    │   │   ├── budget.rs
    │   │   └── policy.rs
    │   ├── feedback/
    │   │   ├── mod.rs
    │   │   └── action_feedback.rs
    │   ├── reducer/
    │   │   ├── mod.rs
    │   │   └── state_reducer.rs
    │   ├── temporal/
    │   │   ├── mod.rs
    │   │   └── event_source.rs
    │   ├── task/
    │   │   ├── mod.rs
    │   │   └── event_source.rs
    │   └── observability/
    │       ├── mod.rs
    │       ├── metrics.rs
    │       └── snapshot.rs
    └── tests/
```

---

# 9. 依赖边界

`yunxi-perception` 不直接依赖：

```text
Kovi
NapCat
OneBot
QQ types
具体 Game SDK
具体 CV SDK
具体 TTS / ASR
具体 SQLx Pool
具体 Model Backend
```

只依赖：

```text
yunxi-core domain types
yunxi-runtime domain types
abstract capability / observation ports
serde
uuid
chrono/time abstraction
tracing / metrics
```

---

# 10. Observation

Observation 表示：

> **系统实际观察到的外部或运行时事实。**

它不是推测。

---

# 11. Observation 与 Hypothesis 区别

例如：

```text
用户说：
“我面试通过了。”
```

Observation：

```text
UserReportedInterviewPassed
```

可以直接成立。

但：

```text
“用户现在应该很开心。”
```

不是 Observation。

它可能是：

```text
Hypothesis / Appraisal
```

属于 V4 / V2。

---

# 12. ObservationEnvelope

建议：

```rust
pub struct ObservationEnvelope {
    pub id: ObservationId,
    pub source: ObservationSource,
    pub scope: ObservationScope,
    pub observed_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub freshness: ObservationFreshness,
    pub quality: ObservationQuality,
    pub payload: ObservationPayload,
    pub trace_id: TraceId,
}
```

---

# 13. ObservationSource

```rust
pub enum ObservationSource {
    UserMessage,
    GroupMessage,
    AudienceStream,
    Tool,
    Adapter,
    GameRuntime,
    VoiceRuntime,
    DesktopRuntime,
    TaskSupervisor,
    Clock,
    Scheduler,
    Host,
    Custom(String),
}
```

---

# 14. ObservationScope

```rust
pub enum ObservationScope {
    Person(PersonId),
    Conversation(ConversationId),
    Channel(ChannelId),
    Task(RuntimeTaskId),
    Global,
}
```

---

# 15. ObservationQuality

建议：

```rust
pub struct ObservationQuality {
    pub confidence: f32,
    pub directness: f32,
    pub reliability: f32,
}
```

注意：

```text
confidence
```

表示：

```text
“这份 observation 本身可靠到什么程度”
```

不代表：

```text
“整个世界状态一定如此”
```

---

# 16. ObservationFreshness

```rust
pub enum ObservationFreshness {
    Fresh,
    Aging,
    Stale,
    Unknown,
}
```

---

# 17. Freshness 必须与 World Model 分离

Observation 进入 World Model 后：

```text
WorldModel property
```

仍应能知道：

```text
source
observed_at
freshness
confidence
```

避免：

```text
3 小时前的游戏状态
```

继续被当作当前事实。

---

# 18. ObservationNormalizer

不同 Adapter 的输入不能直接污染 Core。

例如：

```text
QQ Message
Bilibili Danmaku
Game Telemetry
Desktop Window Event
Tool JSON
```

统一经过：

```text
ObservationNormalizer
```

变成平台无关 Observation。

---

# 19. Adapter 不得直接写 State

必须明确禁止：

```text
Adapter
→ Mind.write(...)
```

禁止：

```text
Adapter
→ WorldModel.set(...)
```

禁止：

```text
Adapter
→ WorkingState.patch(...)
```

正确：

```text
Adapter
→ Observation / ActionResult
→ WorldEvent
→ EventBus
→ Reducer / StateUpdater
```

---

# 20. 为什么必须禁止直接写 State

否则未来会出现：

```text
Game Adapter 改 WorldModel
TaskSupervisor 又改一次
Executive 又自己推断一次
Tool handler 再 patch 一次
```

状态来源会失控。

V7 要坚持：

> **外部输入只通过事件进入状态。**

---

# 21. WorldEventFactory

Observation 不能直接等于所有 WorldEvent。

建议：

```rust
pub trait WorldEventFactory {
    fn from_observation(
        &self,
        observation: ObservationEnvelope,
    ) -> Vec<WorldEvent>;
}
```

---

# 22. 一个 Observation 可产生多个 Event

例如：

```text
Game Observation:
health dropped to 10%
enemy visible
round active
```

可映射：

```text
HealthChanged
EnemyVisible
DangerStateChanged
```

---

# 23. 一个 Event 也可由多个 Observation 支撑

例如：

```text
Audience:
“后面！”
“后面有人！”
“看身后！”
```

经过 coalesce 后产生：

```text
AudienceSignal:
RearThreatWarning
```

---

# 24. World → Observation

这是 V7 最核心的第一条闭环。

所有 Host / Adapter 必须最终能够：

```text
World
→ Observation
```

---

# 25. Push Observation

理想情况：

```text
外部世界主动推事件
```

例如：

```text
QQ message arrived
Tool result arrived
Game telemetry changed
File watcher event
```

优先采用 push。

---

# 26. Pull Observation

有些环境只能：

```text
主动检查
```

例如：

```text
check process state
poll game state
poll remote API
check task progress
```

这类使用 ObservationScheduler。

---

# 27. Push 优先于 Poll

原则：

```text
有可靠 push
→ 不额外高频 poll
```

避免：

```text
一边 webhook
一边每秒查询一次
```

造成重复。

---

# 28. ObservationScheduler

建议：

```rust
pub trait ObservationScheduler {
    async fn schedule_check(
        &self,
        request: ObservationCheckRequest,
    ) -> Result<ObservationCheckId>;
}
```

---

# 29. ObservationCheckRequest

```rust
pub struct ObservationCheckRequest {
    pub capability: CapabilityId,
    pub scope: ObservationScope,
    pub reason: ObservationReason,
    pub due_at: DateTime<Utc>,
    pub priority: EventPriority,
}
```

---

# 30. Clock 不直接检查 World

V7 正式规定：

```text
Clock
X → 直接检查 World
```

而应该：

```text
Clock / Scheduler
→ TimeEvent
→ Executive / Policy
→ ObservationIntent
→ Capability
→ Adapter
→ Observation
```

---

# 31. 时间本身是一种事件来源

例如：

```text
14:00
```

不是“世界事实已经变化”。

它只是：

```text
TimeEvent
```

然后系统决定：

```text
是否值得重新观察某件事
```

---

# 32. 时间驱动闭环

示例：

```text
User:
“我下午三点面试。”

OpenLoop:
follow-up later

15:30:
Clock
→ OpenLoopDue
→ EventBus
→ Executive
→ CheckContext / Decide

如果需要：
→ Observation Action / ReachOut
```

---

# 33. 不允许 Clock 直接强制发消息

`Due` 只表示：

```text
现在值得重新考虑
```

OpenLoop：

```text
Due
→ Planner
```

Reminder：

```text
Due
→ MustExecute
```

语义必须保持 V1 的区别。

---

# 34. TaskSupervisor 是 Event Producer

TaskSupervisor 必须产生：

```text
TaskQueued
TaskStarted
TaskProgressed
TaskWaiting
TaskPaused
TaskResumed
TaskCompleted
TaskFailed
TaskCancelled
TaskDeadlineExceeded
```

---

# 35. Task 状态不可只存在数据库里

Task state change：

必须进入 EventBus。

这样：

```text
TaskCompleted
```

才能让：

```text
Prepared “还没完成”
```

失效。

---

# 36. Task 驱动闭环

```text
TaskSupervisor
↓
TaskProgressed
↓
EventBus
↓
WorkingState
↓
Executive
↓
可能通知用户 / 继续步骤 / Silent
```

---

# 37. ActionResult

ActionResult 表示：

> **Runtime / Adapter 对“动作执行过程”的结果。**

不是：

> **外部世界最终真实状态。**

---

# 38. ActionResult 类型

```rust
pub enum ActionResult {
    Accepted,
    Started,
    Progress(ActionProgress),
    Succeeded(ActionOutput),
    Failed(ActionFailure),
    Cancelled,
    Unknown,
}
```

---

# 39. ActionResult ≠ World Result

例：

```text
Action:
SendMessage
```

Adapter：

```text
API accepted
```

只能说明：

```text
ActionSucceeded / Accepted
```

不能说明：

```text
对方已读
```

---

# 40. 游戏例子

```text
Action:
TakeCover
```

GameSkillLayer：

```text
command executed
```

不代表：

```text
agent is now safe
```

必须靠后续 Observation：

```text
player_position
line_of_fire
cover_state
```

确认。

---

# 41. 网络例子

```text
Action:
CreateIssue
```

HTTP 200：

可能说明：

```text
server accepted
```

如果返回 issue id，可以认为 create succeeded。

但后续：

```text
issue still exists
```

仍属于世界状态。

---

# 42. Action Outcome Evaluation

V7 必须明确区分：

```text
ActionResult
World Change
Goal Progress
```

三者不是同一件事。

一个 Action 可以：

```text
执行成功
+
世界没有变化
```

也可以：

```text
世界发生变化
+
Goal 没有推进
```

还可以：

```text
观察能力失效
+
根本不知道世界有没有变化
```

因此不能只用：

```text
changed / unchanged
```

两个状态描述行动后结果。

---

# 43. ChangeAssessment

建议：

```rust
pub enum ChangeAssessment {
    Changed,
    ExpectedStable,
    UnexpectedStable,
    Unknown,
}
```

语义：

```text
Changed
= 与行动前相比，观察到有意义变化

ExpectedStable
= 没变化，但这符合当前预期

UnexpectedStable
= 本来应该变化，但观察结果仍保持原状态

Unknown
= 无法可靠判断是否发生变化
```

---

# 44. ExpectedStable

例如：

```text
Task:
remote build running

10:00:
running

10:01:
running
```

如果预计构建可能需要数分钟：

```text
running → running
```

属于：

```text
ExpectedStable
```

这不是失败。

系统可以：

```text
update last_checked_at
→ schedule next check
→ Silent
```

---

# 45. UnexpectedStable

例如：

```text
Action:
OpenDoor

Before:
Closed

Expected:
Open within 2s

Observed:
Closed
```

结果：

```text
UnexpectedStable
```

这意味着：

```text
动作已执行
+
预期世界变化没有出现
```

它应进入 Executive，而不是自动重试。

---

# 46. Unknown 不等于 NoChange

例如：

```text
camera offline
telemetry stale
API timeout
parse failed
```

此时不能说：

```text
环境没有变化
```

正确是：

```text
Unknown
```

V7 必须严格避免：

```text
Observation unavailable
→ assume unchanged
```

---

# 47. OutcomeEvaluation

建议：

```rust
pub enum OutcomeEvaluation {
    Confirmed,
    PartialProgress,
    NoObservedChange,
    Mismatch,
    Unknown,
}
```

其中：

```text
Confirmed
= 观察结果确认预期效果

PartialProgress
= 世界有变化且朝目标推进，但尚未完成

NoObservedChange
= 观察到没有有意义变化

Mismatch
= 观察结果与预期方向明显冲突

Unknown
= 无足够证据判断
```

---

# 48. ProgressAssessment

除了 World 是否变化，还必须判断 Goal 是否推进。

建议：

```rust
pub enum ProgressKind {
    Progress,
    PartialProgress,
    NoProgress,
    Regression,
    Unknown,
}

pub struct ProgressAssessment {
    pub kind: ProgressKind,
    pub consecutive_no_progress: u32,
    pub evidence: Vec<ObservationId>,
}
```

---

# 49. World Changed ≠ Goal Progress

例如游戏：

```text
Goal:
进入安全掩体

Action:
移动到左侧障碍物

Observed:
position changed

Observed:
danger_level unchanged
```

这里：

```text
ChangeAssessment = Changed
```

但：

```text
ProgressAssessment = NoProgress
```

因此不能因为：

```text
角色移动成功
```

就认为：

```text
Goal completed
```

---

# 50. Before / Expected / Observed / Goal 四项比较

任何需要结果确认的重要 Action，都建议形成：

```rust
pub struct OutcomeEvaluationInput {
    pub before: Option<WorldStateSlice>,
    pub expected: Option<ExpectedOutcome>,
    pub observed: Option<WorldStateSlice>,
    pub goal: Option<GoalProgressTarget>,
}
```

评估顺序：

```text
Before State
+
Expected Outcome
+
Observed State
+
Goal Progress
        ↓
ChangeAssessment
+
OutcomeEvaluation
+
ProgressAssessment
```

---

# 51. ExpectedOutcome

建议：

```rust
pub struct ExpectedOutcome {
    pub description: String,
    pub expected_changes: Vec<ExpectedChange>,
    pub observation_window: Duration,
    pub confirmation_policy: ConfirmationPolicy,
}
```

不要保存模型隐藏推理。

只保存：

```text
结构化、可检查的预期结果
```

---

# 52. Observation Window

世界变化可能不是即时发生。

例如：

```text
StartProcess
→ 预计 3 秒内进入 running
```

因此不能：

```text
ActionSucceeded
→ 立即 observe
→ unchanged
→ fail
```

而应该允许：

```text
observation_window
```

在窗口内：

```text
PendingConfirmation
```

---

# 53. OutcomeEvaluation Timing

建议：

```text
Action Committed
→ ActionResult
→ wait / observe according to policy
→ evaluate outcome
```

不是所有 Action 都需要额外等待。

---

# 54. NoProgressGuard

V7 必须有防止“没推进就无限重试”的硬边界。

建议：

```rust
pub struct NoProgressGuard {
    pub max_confirmation_attempts: u32,
    pub max_retry_attempts: u32,
    pub max_no_progress_count: u32,
    pub deadline: Option<DateTime<Utc>>,
    pub backoff: BackoffPolicy,
}
```

---

# 55. 连续 NoProgress

例如：

```text
Goal:
让测试通过

Attempt 1:
12 failing tests

Attempt 2:
12 failing tests

Attempt 3:
12 failing tests
```

如果连续达到：

```text
max_no_progress_count
```

必须触发：

```text
PlanRevisionRequired
```

而不是继续相同策略。

---

# 56. Regression

如果：

```text
12 failing tests
→ 20 failing tests
```

这是：

```text
Regression
```

通常比 NoProgress 更应该触发：

```text
Replan / rollback / inspect
```

---

# 57. NoProgress Event

建议增加：

```text
GoalProgressed
GoalNoProgress
GoalRegressed
OutcomeConfirmed
OutcomeMismatch
OutcomeUnknown
UnexpectedStable
```

这些都进入 EventBus。

---

# 58. NoProgress 与 Executive

V7 只负责：

```text
测量 / 评估结果
```

V3 Executive 决定：

```text
Wait
Retry
UseDifferentCapability
Replan
AskUser
FailTask
Defer
Silent
```

---

# 59. 不允许 Perception 自动 Retry

这是 V7 的硬规则：

```text
UnexpectedStable
X → 自动重试相同 Action
```

正确：

```text
UnexpectedStable
→ EventBus
→ Executive
→ Retry / Replan / Wait / Fail
```

---

# 60. Retry 必须有理由

如果 Executive 选择 Retry，应至少满足：

```text
retry budget available
deadline not exceeded
capability healthy
same strategy still plausible
no Stop / Cancel
```

---

# 61. Retry 与 Reobserve 分离

有时不需要重新执行 Action，只需要再观察。

例如：

```text
StartProcess
→ command accepted
→ 500ms 后仍 not running
```

可能：

```text
Wait
→ Reobserve
```

而不是：

```text
再 StartProcess 一次
```

---

# 62. Reobserve Budget

重新观察同样必须 bounded。

```rust
pub struct ConfirmationBudget {
    pub attempts: u32,
    pub max_attempts: u32,
    pub next_delay: Duration,
    pub deadline: Option<DateTime<Utc>>,
}
```

---

# 63. GoalProgressTarget

建议 Goal / Plan 暴露最小可验证进度指标。

例如：

```text
Goal:
tests pass

Progress target:
failing_tests decreases
final target = 0
```

或者：

```text
Goal:
reach cover

Progress target:
distance_to_cover decreases
danger_level decreases
```

---

# 64. Progress Metric 不一定是数字

也可以：

```text
state transition
milestone completion
task terminal state
expected event observed
```

---

# 65. No False Precision

如果只能知道：

```text
step 2 finished
```

不要硬算：

```text
67%
```

ProgressAssessment 可以是定性状态。

---

# 66. Stable World 可以是成功

某些 Goal 就是：

```text
保持状态不变
```

例如：

```text
monitor service and keep it healthy
```

如果：

```text
service remains healthy
```

则：

```text
ExpectedStable
+
Goal Progress / Goal Maintained
```

不能把稳定误判为 NoProgress。

---

# 67. Maintenance Goal

建议允许：

```rust
pub enum GoalProgressMode {
    ReachTarget,
    MaintainState,
    AvoidState,
    ObserveUntil,
}
```

这样：

```text
“保持服务器在线”
```

不会被错误要求“世界必须变化”。

---

# 68. AvoidState

例如：

```text
Goal:
avoid dying in game
```

没有死亡：

可能就是成功的持续状态。

---

# 69. ObserveUntil

例如：

```text
等待 build completed
```

期间：

```text
running → running
```

可以持续 `ExpectedStable`。

直到：

```text
Completed
```

---

# 70. NoProgress 的语义必须依 GoalMode

```text
ReachTarget
→ 长期不变化可能 NoProgress

MaintainState
→ 不变化可能正是成功

AvoidState
→ 不进入禁止状态可能成功

ObserveUntil
→ 未出现目标事件前可 ExpectedStable
```

---

# 71. Outcome Evaluation Snapshot

建议 Debug 可查看：

```rust
pub struct OutcomeEvaluationSnapshot {
    pub action_id: ActionId,
    pub change: ChangeAssessment,
    pub outcome: OutcomeEvaluation,
    pub progress: ProgressKind,
    pub confirmation_attempts: u32,
    pub retry_attempts: u32,
    pub evaluated_at: DateTime<Utc>,
}
```

默认不包含敏感原始 Observation。

---

# 72. Metrics：Outcome

新增：

```text
yunxi_perception_outcome_confirmed_total
yunxi_perception_outcome_mismatch_total
yunxi_perception_unexpected_stable_total
yunxi_perception_no_progress_total
yunxi_perception_regression_total
yunxi_perception_outcome_unknown_total
yunxi_perception_plan_revision_required_total
```

---

# 73. Reason Tags：Outcome

新增：

```text
EXPECTED_STABLE
UNEXPECTED_STABLE
WORLD_CHANGED
WORLD_CHANGE_UNKNOWN
OUTCOME_CONFIRMED
OUTCOME_MISMATCH
PARTIAL_PROGRESS
NO_PROGRESS
REGRESSION
CONFIRMATION_BUDGET_EXHAUSTED
RETRY_BUDGET_EXHAUSTED
PLAN_REVISION_REQUIRED
```

# 74. ActionFeedbackBridge

建议：

```rust
pub trait ActionFeedbackBridge {
    fn to_world_events(
        &self,
        action: &RuntimeAction,
        result: ActionResult,
    ) -> Vec<WorldEvent>;
}
```

---

# 75. ActionResult 也进入 EventBus

例如：

```text
ToolCompleted
MessageSendAccepted
GameSkillCompleted
NotificationDelivered
```

都可以是 WorldEvent / RuntimeEvent。

---

# 76. 但不要自动改 WorldModel

```text
ActionSucceeded
```

不是：

```text
World property changed exactly as intended
```

WorldModel 应等待：

```text
Action feedback
+
Observation
```

综合更新。

---

# 77. 状态更新入口

建议正式引入：

```text
StateReducer
```

---

# 78. StateReducer

```rust
pub trait StateReducer {
    fn reduce(
        &self,
        current: &WorkingState,
        event: &WorldEvent,
    ) -> StateDelta;
}
```

---

# 79. Reducer 的职责

Reducer 负责：

```text
确定哪些运行时状态应该变化
```

例如：

```text
TaskCompleted
→ Task snapshot update

MessageReceived
→ Conversation version update

CapabilityOffline
→ Capability version update
```

---

# 80. Reducer 不做复杂人格推理

不要：

```text
TaskFailed
→ Bot mood = sad
```

这种应由 V2 Appraisal / Affect 逻辑决定。

---

# 81. StateDelta

建议：

```rust
pub struct StateDelta {
    pub working_state: Option<WorkingStatePatch>,
    pub version_bumps: VersionBumps,
    pub derived_events: Vec<WorldEvent>,
}
```

---

# 82. Derived Event 必须 bounded

Reducer 可以产生少量 derived events。

但禁止：

```text
Event A
→ Event B
→ Event C
→ Event A
```

无限循环。

---

# 83. EventDepth

建议：

```rust
pub struct EventMeta {
    ...
    pub derivation_depth: u16,
}
```

超过阈值：

```text
reject / alert
```

---

# 84. EventDeduplicator

闭环系统最容易出现重复事件。

例如：

```text
ToolCompleted from tool runtime
+
ActionSucceeded from adapter
+
TaskProgressed from supervisor
```

可能描述同一件事。

需要 dedupe。

---

# 85. Dedupe Key

建议：

```text
source
external_event_id
action_id
task_id
semantic kind
time window
```

---

# 86. Dedupe 不等于语义合并

完全相同外部事件：

```text
dedupe
```

大量相似弹幕：

```text
coalesce
```

两者不同。

---

# 87. EventCoalescer

适合：

```text
Audience
Game telemetry
Rapid UI changes
Sensor stream
```

例如：

```text
health 100
health 99
health 98
...
health 60
```

如果中间状态不重要：

可 coalesce 成：

```text
HealthChanged 100 → 60
```

---

# 88. 不能 Coalesce 的事件

以下通常不能随便合并：

```text
ReminderDue
Stop
PermissionRevoked
TaskCompleted
SecurityEvent
Committed Action Result
```

---

# 89. PerceptionPolicy

并不是所有世界状态都需要持续观察。

建议：

```rust
pub struct PerceptionPolicy {
    pub mode: PerceptionMode,
    pub min_interval: Duration,
    pub max_interval: Duration,
    pub priority: EventPriority,
    pub stale_after: Duration,
}
```

---

# 90. PerceptionMode

```rust
pub enum PerceptionMode {
    Push,
    Poll,
    Hybrid,
    OnDemand,
}
```

---

# 91. OnDemand

例如：

```text
用户问：
“那个任务完成了吗？”
```

可触发：

```text
OnDemand status observation
```

如果已有足够新的 TaskProgressSnapshot：

不需要重复检查。

---

# 92. Hybrid

例如 Game：

```text
telemetry push
+
低频 full snapshot
```

避免丢事件后永久漂移。

---

# 93. PerceptionBudget

必须防止：

```text
全世界所有状态每秒全量扫描
```

建议：

```rust
pub struct PerceptionBudget {
    pub max_checks_per_second: u32,
    pub max_background_checks: usize,
    pub max_high_cost_observations: usize,
}
```

---

# 94. Perception Priority

建议：

```text
Critical
Interactive
Foreground
Background
Maintenance
```

---

# 95. 感知频率与语义价值

例如：

```text
FPS enemy position
→ 高频

CPU temperature
→ 中频

weekly profile cleanup
→ 低频
```

不要统一频率。

---

# 96. 高频世界状态不应全部进 LLM

Game Runtime：

```text
60Hz observation
```

不能：

```text
60Hz Planner call
```

正确：

```text
high-frequency deterministic perception
→ local state
→ salient event extraction
→ low-frequency Executive
```

---

# 97. Salience Gate

建议：

```text
raw observations
→ local reducer
→ salience detector
→ WorldEvent
```

避免每个 frame 都成为高层认知事件。

---

# 98. Game 示例

```text
60Hz:
position / health / enemy visibility

5~10Hz:
local strategy state

salient:
EnemyAppeared
HealthCritical
RoundEnded
ObjectiveChanged
```

只有 salient 才高优先级进入 Executive。

---

# 99. Voice 示例

Audio stream 本身不进入 WorldEvent。

流程：

```text
audio frames
→ ASR
→ utterance / interrupt / speech end
→ Observation
→ WorldEvent
```

---

# 100. Audience 示例

```text
1000 danmaku
→ batch
→ cluster
→ AudienceSignal
→ EventBus
```

而不是：

```text
1000 raw events → 1000 LLM calls
```

---

# 101. Perception Loop

建议：

```rust
pub trait PerceptionLoop {
    async fn run(&self, ctx: PerceptionContext) -> Result<()>;
}
```

但：

> V7 不允许一个全局 `while true { ask LLM what to observe }`。

---

# 102. 正确 Perception Loop

```text
Push source
+
Scheduler
+
Task event
+
Action feedback
+
on-demand observation
```

共同驱动。

---

# 103. Idle 不等于 Poll Everything

空闲时可以：

```text
IdleTick
```

但 IdleTick 只触发：

```text
是否需要观察 / reflection / maintenance 的检查
```

而不是强制全量 world scan。

---

# 104. WorldEvent 来源分类

V7 建议正式定义四类来源：

```text
1. External Observation
2. Temporal Event
3. Task Event
4. Action Feedback
```

---

# 105. External Observation

例如：

```text
MessageReceived
GameEnemyVisible
WindowChanged
VoiceUtterance
AudienceSignal
```

---

# 106. Temporal Event

例如：

```text
ReminderDue
OpenLoopDue
DeadlineDue
MaintenanceTick
IdleTick
```

---

# 107. Task Event

例如：

```text
TaskStarted
TaskProgressed
TaskCompleted
TaskFailed
TaskWaiting
```

---

# 108. Action Feedback

例如：

```text
MessageSendAccepted
ToolCompleted
GameSkillFailed
NotificationDelivered
```

---

# 109. 统一进入 EventBus

四类事件最终：

```text
→ RuntimeEvent
→ EventBus
```

这样 Attention / WorkingState / Executive 不需要知道底层来源细节。

---

# 110. WorkingState

WorkingState 是：

> **当前决策所需的短期整合状态。**

不等于长期 Memory，也不等于完整 WorldModel。

---

# 111. WorkingState 内容建议

```text
recent relevant events
active tasks
current channel state
pending actions
current conversation state
recent observations
active open loops
runtime mode
```

---

# 112. WorkingState 必须 bounded

不能不断增长。

---

# 113. WorkingState Snapshot

Planner / Executive 读取：

```text
immutable snapshot
```

模型返回后：

```text
version revalidation
```

---

# 114. WorldModel 更新

WorldModel 从：

```text
Observation
+
Action feedback
+
existing model
```

更新。

但必须保留：

```text
Known
Suspected
Unknown
```

的区别。

---

# 115. Observation 不自动变永久事实

例如：

```text
health = 20%
```

五秒后可能失效。

因此 WorldModel property 应有：

```text
valid_at
freshness
source
confidence
```

---

# 116. Action Expected Outcome

V3/V4 可为 Action 生成：

```text
ExpectedOutcome
```

然后 V7 通过 Observation 验证。

---

# 117. Prediction Error

```text
Expected:
door opened

Observed:
door still closed
```

产生：

```text
PredictionMismatch
```

供 V4 World Model 校准。

---

# 118. 不做 Online RL

V7 记录：

```text
prediction vs observation
```

但不自动在线训练模型权重。

---

# 119. Observation Confirmation

某些 Action 需要确认。

例如：

```text
DeleteFile
```

ActionSucceeded：

可能已经足够。

而：

```text
NavigateToCover
```

需要：

```text
post-action observation
```

---

# 120. ConfirmationPolicy

建议：

```rust
pub enum ConfirmationPolicy {
    None,
    AdapterAck,
    ObserveAfter,
    ObserveUntil,
}
```

---

# 121. ObserveAfter

例如：

```text
Action committed
→ after 200ms
→ schedule observation
```

---

# 122. ObserveUntil

例如：

```text
等待 task reaches terminal state
```

但必须：

```text
deadline
max attempts
backoff
```

---

# 123. 禁止无限确认轮询

任何 `ObserveUntil`：

必须 bounded。

---

# 124. Backoff

建议：

```text
100ms
250ms
500ms
1s
2s
```

按具体 Capability 配置。

---

# 125. Event Causality

V7 继承 V6：

```text
trace_id
root_event_id
parent_event_id
```

Observation → WorldEvent：

保留 trace。

ActionResult → WorldEvent：

保留 action_id。

---

# 126. 感知因果链示例

```text
User Message #1
→ Intent #2
→ Action #3
→ Tool Call #4
→ ActionResult #5
→ ToolObservation #6
→ WorldEvent #7
→ WorldModelUpdate #8
→ ReplyAction #9
```

---

# 127. Time-driven 因果链

```text
ClockTick
→ OpenLoopDue
→ ExecutiveDecision
→ ObservationIntent
→ AdapterCheck
→ Observation
→ WorldEvent
→ ReachOut Decision
```

---

# 128. Task-driven 因果链

```text
TaskStarted
→ TaskProgressed
→ TaskCompleted
→ EventBus
→ pending reply invalidated
→ Rewrite
```

---

# 129. Event Priority

感知事件不能一律同优先级。

例如：

```text
User direct message
→ Interactive

Game player dying
→ Critical / Foreground

Background CPU stat
→ Maintenance
```

---

# 130. Perception Degradation

V6 Reduced 模式下：

```text
降低 background poll
降低 world refresh
保留 direct observation
保留 critical game state
保留 task completion
```

---

# 131. CriticalOnly

只保留：

```text
direct user input
Stop
Reminder
Task terminal state
security
critical host health
necessary confirmation
```

---

# 132. Observation Health

每个 source 必须有：

```text
Healthy
Degraded
Stale
Offline
PermissionDenied
```

---

# 133. PerceptionHealth

建议：

```rust
pub struct PerceptionHealth {
    pub source: ObservationSource,
    pub state: PerceptionHealthState,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub error_rate: f32,
}
```

---

# 134. Source Offline

例如 Game telemetry 掉线：

```text
ObservationSourceOffline
```

进入 EventBus。

WorldModel 对该 source 的状态：

逐步标记 stale。

---

# 135. 不允许 stale data 无限有效

例如：

```text
enemy_visible = true
```

如果 game source 断开：

不能永远保持：

```text
enemy visible
```

必须：

```text
fresh → aging → stale → unknown
```

---

# 136. Observation TTL

建议属性：

```text
stale_after
expire_after
```

---

# 137. Expire 后变 Unknown

不是自动变 False。

例如：

```text
enemy_visible
```

过期：

```text
Unknown
```

不是：

```text
false
```

---

# 138. WorldEvent Replay

V7 可以支持测试 replay。

输入：

```text
recorded observation sequence
```

重放：

```text
WorldEvent
→ Reducer
→ State
```

用于 deterministic test。

---

# 139. Replay 不触发真实副作用

测试 replay 默认：

```text
ActionPort = simulated
```

---

# 140. Simulation vs Real

继承 V4/V6：

```text
ExecutionMode::Simulated
ExecutionMode::Real
```

Simulation 产生：

```text
SimulatedObservation
```

不得混入真实世界事实。

---

# 141. Simulated Observation

必须明确标记：

```text
source = Simulation
```

WorldModel 不得把它自动当真实 Observation。

---

# 142. Privacy

Observation 可能包含：

```text
voice
screen
camera
private message
file content
location
```

必须遵循：

```text
scope
permission
retention
redaction
```

---

# 143. Raw Observation Retention

默认：

```text
最短必要
```

高敏原始数据不要长期保留。

---

# 144. Derived Observation

可以保留：

```text
structured summary
```

而不是原始 audio/video。

---

# 145. Camera / Mic

如果 Capability permission revoked：

```text
Observation source stops
capability_version bumps
pending checks fail closed
```

---

# 146. State Reducer 与 Privacy

Reducer 不能把：

```text
private raw content
```

复制到 global scope。

---

# 147. Person / Conversation Scope

Observation 必须保留 scope。

不要：

```text
Person A private observation
→ Global WorldModel
```

除非明确需要且符合权限设计。

---

# 148. Multi-host Observation

同一个 world entity 可能被多个 Host 观察。

例如：

```text
QQ
Desktop
Voice
```

需要 source-aware merge。

---

# 149. Conflict Observation

例如：

```text
Source A:
task running

Source B:
task completed
```

不能直接覆盖。

进入 V4：

```text
conflicting observations
→ Hypothesis / Unknown / freshness compare
```

---

# 150. Direct Source 优先

通常：

```text
authoritative task store
>
model inference
```

---

# 151. Fresh Source 优先

同可靠度下：

```text
newer observation
>
older observation
```

---

# 152. 但禁止粗暴 Last Write Wins

如果旧 source 权威、新 source 不可靠：

不能只看时间。

---

# 153. Observation Merge Policy

建议考虑：

```text
source authority
confidence
freshness
scope
directness
```

---

# 154. PerceptionSnapshot

Debug：

```rust
pub struct PerceptionSnapshot {
    pub active_sources: usize,
    pub degraded_sources: usize,
    pub pending_checks: usize,
    pub recent_observations: usize,
    pub event_backlog: usize,
    pub last_observation_at: Option<DateTime<Utc>>,
}
```

默认不显示完整敏感内容。

---

# 155. Metrics

建议：

```text
yunxi_perception_observation_total
yunxi_perception_observation_dropped_total
yunxi_perception_observation_deduped_total
yunxi_perception_observation_coalesced_total
yunxi_perception_check_total
yunxi_perception_check_failed_total
yunxi_perception_source_health
yunxi_perception_stale_state_total
yunxi_perception_action_feedback_total
yunxi_perception_loop_latency_ms
```

---

# 156. Reason Tags

建议：

```text
WORLD_OBSERVED
ACTION_RESULT
TIME_DUE
TASK_STATE_CHANGED
OBSERVATION_STALE
OBSERVATION_DUPLICATE
OBSERVATION_COALESCED
SOURCE_OFFLINE
SOURCE_RECOVERED
POST_ACTION_CONFIRMATION
PREDICTION_MISMATCH
PERCEPTION_BUDGET_EXCEEDED
```

---

# 157. 锁原则

禁止：

```text
hold WorldModel lock
→ await Adapter observation
```

正确：

```text
snapshot request
→ unlock
→ await observation
→ emit event
→ reducer
→ version check
```

---

# 158. Observation Adapter await

Adapter observation 可以很慢。

绝不能阻塞：

```text
EventBus
ConversationCoordinator
Game Control Loop
TaskSupervisor
```

---

# 159. Structured Concurrency

Observation checks：

必须挂在：

```text
TaskSupervisor / runtime-managed workers
```

避免无限 detached task。

---

# 160. Timeout

每个 pull observation：

必须有 timeout。

---

# 161. Retry

只读 observation：

可 retry。

但：

```text
bounded
backoff
```

---

# 162. Error Taxonomy

建议：

```rust
pub enum PerceptionErrorKind {
    Timeout,
    Permission,
    SourceOffline,
    Parse,
    InvalidObservation,
    RateLimited,
    Cancelled,
    Transient,
    Permanent,
}
```

---

# 163. Parse Error 不等于 World False

如果数据解析失败：

```text
Observation unavailable
```

不是：

```text
state = false
```

---

# 164. Integration with V1

V1 WorldEvent：

继续作为上层统一事件。

V7 增加：

```text
WorldEvent 的输入规范
```

而不是替换 V1。

---

# 165. Integration with V2

Mind 不直接消费 raw sensor stream。

只消费：

```text
salient normalized events
```

---

# 166. Integration with V3

Executive 决定：

```text
是否需要进一步观察
是否值得主动检查
是否需要 post-action confirmation
```

---

# 167. Integration with V4

V4 是 V7 最大消费者之一。

V7 提供：

```text
Observation
source
freshness
confidence
action feedback
prediction mismatch
```

V4 负责：

```text
world state estimation
hypothesis
causal relation
prediction
```

---

# 168. Integration with V5

某些 Observation extraction：

```text
vision
semantic
ASR
classification
```

可以调用 Model Fabric。

但：

```text
raw high-frequency stream
```

不能全部丢给大模型。

---

# 169. Integration with V6

V6 提供：

```text
TaskSupervisor
Clock
Scheduler
CapabilityRegistry
ActionLifecycle
RuntimeBudget
Recovery
```

V7 使用它们形成闭环。

---

# 170. Observation Capability

建议 CapabilityKind 增加或明确：

```text
Observe
ObserveGame
ObserveScreen
ObserveTask
ObserveEnvironment
```

---

# 171. ObservationIntent

V7 建议 Intent 层支持：

```rust
pub enum ObservationIntent {
    CheckTaskStatus(RuntimeTaskId),
    ObserveChannel(ChannelId),
    ObserveEnvironment(EnvironmentQuery),
    ObserveGame(GameQuery),
    ConfirmAction(ActionId),
}
```

---

# 172. ObservationIntent 不直接执行

仍然：

```text
Intent
→ Capability
→ ActionArbiter
→ Adapter
```

---

# 173. Polling Example

```text
Task waiting for remote completion
```

不要：

```text
while true:
  sleep 1s
  check
```

建议：

```text
Scheduler
→ CheckDue
→ ObservationIntent
→ Adapter
→ Observation
→ if still waiting
   schedule next check
```

---

# 174. Polling 可被取消

Task cancel：

```text
cancel future observation checks
```

---

# 175. Polling 可降级

Reduced：

```text
1s poll
→ 5s
```

如果业务允许。

---

# 176. Polling Deadline

必须有：

```text
max duration
```

---

# 177. World Check 不是永远必须

如果 push event 已经到达：

取消 pending poll。

---

# 178. Action Feedback Example：消息发送

```text
MessageAction
→ Adapter
→ Accepted
→ ActionResult Event
```

如果平台提供：

```text
delivered / read
```

再通过 Observation 进入。

---

# 179. Action Feedback Example：Tool

```text
ToolCall
→ Started
→ Progress
→ Succeeded
```

这些是 ActionFeedback。

Tool output 内容：

可以进一步产生：

```text
Observation
```

---

# 180. Action Feedback Example：Game

```text
GameIntent:
TakeCover

GameSkill:
Succeeded
```

然后：

```text
Observe position / threat
```

确认是否真正进入掩体。

---

# 181. Action Feedback Example：Desktop

```text
ClickButton
→ input injected
```

随后：

```text
Observe UI changed
```

才能确认目标是否达成。

---

# 182. Action Feedback Example：文件

```text
WriteFile
→ filesystem write succeeded
```

这个 ActionResult 本身通常足够确认文件写入。

但如果需要更强验证：

```text
read-back / checksum
```

---

# 183. Confirmation 应按 Action 类型配置

不是所有 Action 都额外 ObserveAfter。

避免：

```text
每次发 QQ 都再 poll 一次平台
```

---

# 184. WorldEvent Loop Safety

闭环最危险的问题：

```text
event → action → event → action → ...
```

必须防失控。

---

# 185. Loop Guard

建议：

```text
trace action count
derivation depth
same-action cooldown
same-intent dedupe
budget
```

---

# 186. Self-trigger Storm

例如：

```text
Bot sends message
→ sees own message
→ replies to own message
→ ...
```

必须有：

```text
self-origin marker
```

---

# 187. Origin

建议 EventMeta：

```rust
pub enum EventOrigin {
    External,
    Yunxi,
    System,
    Simulation,
}
```

---

# 188. Self-origin Event 仍可观察

但 AttentionPolicy：

```text
不要把自己的普通 outgoing 当成新的 user input
```

---

# 189. Echo Prevention

聊天平台可能把 Bot 自己消息作为 incoming event。

必须 dedupe / mark origin。

---

# 190. WorldEvent Feedback Delay

某些世界变化不是即时。

例如：

```text
remote job started
```

后续结果：

```text
minutes later
```

Task / Scheduler 处理，而不是同步等待。

---

# 191. Long-running Observation

例如：

```text
watch build
watch game session
watch voice session
```

应该作为 RuntimeTask。

---

# 192. Streaming Observation

支持：

```text
stream of observations
```

但高层只接收：

```text
normalized / salient events
```

---

# 193. Observation Cursor

对可续读流：

建议：

```text
cursor / sequence
```

防止 restart 后重复消费。

---

# 194. Restart Recovery

重启：

```text
restore observation subscriptions
restore scheduled checks
restore cursor
```

---

# 195. 不恢复旧 transient frame

例如：

```text
2 分钟前游戏画面
```

不应该 replay 成“现在”。

---

# 196. 恢复后重新 Snapshot

某些 source：

```text
Game
Desktop
```

restart 后应：

```text
request fresh full snapshot
```

---

# 197. Source Epoch

建议：

```text
source_epoch
```

Host reconnect 后：

```text
epoch + 1
```

旧 observation 不与新 session 混淆。

---

# 198. Conversation Collision 与 V7

V6 的 MessageCollision：

现在可视为：

```text
Conversation Observation
```

进入：

```text
WorldEvent
```

再由 V3 决定是否提及。

---

# 199. Task Completion Collision

```text
TaskCompleted
```

也是 V7 闭环的一部分。

它会：

```text
EventBus
→ version bump
→ pending action revalidation
```

---

# 200. Time Collision

例如：

```text
ReminderDue
+
User already did thing
```

Reminder reliable semantics 仍由 V1/V6 控制。

V7 只提供最新 Observation。

---

# 201. Current World Snapshot

对于复杂环境，可定期维护：

```text
WorldSnapshot
```

但必须：

```text
bounded
source-aware
freshness-aware
```

---

# 202. Snapshot 不是事件日志

EventJournal：

```text
发生过什么
```

WorldSnapshot：

```text
现在估计是什么
```

---

# 203. Reducer 与 WorldModel 分层

Reducer：

```text
deterministic runtime state
```

WorldModel：

```text
uncertain external state estimate
```

---

# 204. 例：Task

```text
TaskCompleted
```

Runtime Task state：

```text
Completed
```

这是 deterministic。

但：

```text
用户是否满意
```

仍然是 uncertain。

---

# 205. 例：Game

Telemetry：

```text
health = 12
```

可以 deterministic。

但：

```text
敌人下一步可能 rush
```

属于 V4 prediction。

---

# 206. Perception and Memory

不是所有 Observation 都写 Memory。

Memory write：

需要：

```text
salience
scope
privacy
importance
```

---

# 207. 高频 sensor data 不写长期 Memory

例如：

```text
frame 1
frame 2
frame 3
```

不持久化长期记忆。

---

# 208. Episode Candidate

只有：

```text
high-salience event
```

才可能进入 V2 Reflection / Episode。

---

# 209. OpenLoop 与 Perception

OpenLoop due：

可以触发：

```text
reconsider
```

必要时：

```text
ObservationIntent
```

不是自动 send。

---

# 210. Expectation 与 Perception

V3 Expectation：

```text
等待某个未来结果
```

V7 负责：

```text
真正检测相应 Observation 是否出现
```

---

# 211. Prediction 与 Perception

V4 Prediction：

```text
预计 Action 后会发生什么
```

V7：

```text
观察实际发生了什么
```

二者结合：

```text
PredictionError
```

---

# 212. Scheduler 与 Perception

V6 Scheduler：

负责：

```text
什么时候检查
```

V7 Perception：

负责：

```text
检查什么
如何形成 Observation
```

---

# 213. Capability 与 Perception

CapabilityRegistry：

负责：

```text
现在能不能观察
```

例如：

```text
ObserveGame = Offline
```

则：

```text
ObservationIntent
→ fail closed / degrade
```

---

# 214. Runtime Mode 与 Perception

Full：

```text
all configured perception
```

Reduced：

```text
critical + interactive + sparse background
```

CriticalOnly：

```text
critical only
```

---

# 215. Perception Priority Inversion

禁止：

```text
低价值 background scan
```

占满：

```text
direct user status query
```

的 observation worker。

---

# 216. Worker Pools

可按成本分池：

```text
cheap
network
vision
high-cost
```

---

# 217. Vision Budget

例如：

```text
screen vision
```

不能无限并发。

---

# 218. Model-backed Perception

如果 Observation extraction 需要模型：

```text
Role = WorldExtraction / SemanticUnderstanding
```

走 V5。

---

# 219. Local-first 可选

敏感 screen / voice：

可以标：

```text
PrivacyClass::LocalOnly
```

V5 必须 fail closed。

---

# 220. Observation Validation

模型产出的结构化 Observation：

必须 schema validate。

---

# 221. Invalid Observation

如果模型输出 malformed：

```text
discard / repair bounded
```

不能直接 patch WorldModel。

---

# 222. Evidence Link

每个模型派生 Observation：

建议保留：

```text
source observation ids
```

---

# 223. Human correction

用户直接纠正：

```text
“不是昨天，是今天。”
```

新的 Observation：

通常：

```text
higher directness
```

进入 V4 belief/world update。

---

# 224. No Silent Overwrite

冲突信息：

不要直接删除旧 observation。

保留：

```text
superseded / conflict relation
```

---

# 225. Testing：World → EventBus

输入 simulated Observation。

验证：

```text
WorldEvent emitted
EventBus receives
Reducer applies
```

---

# 226. Testing：Adapter 不直写 State

通过 architecture test / dependency test：

确保 Adapter 没有直接依赖：

```text
Mind mutable store
WorldModel mutable store
WorkingState mutable store
```

---

# 227. Testing：ActionResult ≠ Observation

```text
TakeCover succeeded
```

不能自动设置：

```text
is_safe = true
```

直到相关 Observation 到达。

---

# 228. Testing：Clock-driven check

FakeClock：

```text
advance
→ TimeEvent
→ ObservationIntent
→ mock adapter
→ Observation
→ WorldEvent
```

---

# 229. Testing：Task-driven loop

```text
TaskCompleted
→ EventBus
→ pending reply invalidated
```

---

# 230. Testing：Dedupe

同 external_event_id 两次：

只处理一次。

---

# 231. Testing：Coalesce

100 个相似 Audience message：

合并成少量 AudienceSignal。

---

# 232. Testing：Stale

Observation TTL 过期：

World state：

```text
Fresh → Aging → Stale → Unknown
```

---

# 233. Testing：Source offline

Source offline 后：

相关状态不能继续无限 Fresh。

---

# 234. Testing：Loop guard

self-message echo：

不能形成无限 reply loop。

---

# 235. Testing：Poll cancel

push result 到达后：

pending poll 被取消。

---

# 236. Testing：Reduced mode

进入 Reduced：

background polling frequency 降低。

Critical observation 保持。

---

# 237. Testing：Restart

restart 后：

subscription 恢复
cursor 恢复
stale transient frame 不 replay
fresh snapshot requested
```

---

# 238. Testing：Version race

Observation 到达导致 world_version bump。

旧 Prepared Action：

```text
revalidate
```

---

# 239. Testing：Privacy

private Observation：

不能流入 unrelated global state。

---

# 240. Load Test：Game Telemetry

模拟：

```text
60Hz × 10min
```

验证：

```text
bounded queue
bounded memory
salience extraction
no 60Hz LLM
```

---

# 241. Load Test：Audience Storm

```text
10k messages
```

验证：

```text
batch
coalesce
dedupe
interactive responsiveness
```

---

# 242. Load Test：Observation Workers

高成本 vision backlog：

不能阻塞 direct chat。

---

# 243. Phase 0：Observation Domain Types

实现：

```text
ObservationId
ObservationEnvelope
ObservationSource
ObservationScope
ObservationQuality
ObservationFreshness
ObservationPayload
```

DoD：

```text
platform-independent
serde
unit tests
```

---

# 244. Phase 1：Observation Normalizer

实现：

```text
Adapter raw input
→ Observation
```

先支持：

```text
QQ message
Tool result
Task event
Clock event
```

---

# 245. Phase 2：WorldEventFactory

实现：

```text
Observation
→ WorldEvent
```

---

# 246. Phase 3：StateReducer

建立：

```text
WorldEvent
→ deterministic StateDelta
```

---

# 247. Phase 4：ActionFeedbackBridge

所有 RuntimeAction result：

统一回流 EventBus。

---

# 248. Phase 5：TemporalEventSource

Clock / Scheduler：

产生：

```text
TimeEvent
```

---

# 249. Phase 6：TaskEventSource

TaskSupervisor：

产生：

```text
TaskEvent
```

---

# 250. Phase 7：ObservationScheduler

实现：

```text
OnDemand
Poll
Hybrid
```

---

# 251. Phase 8：Event Dedupe / Coalesce

支持：

```text
external id dedupe
semantic coalesce
```

---

# 252. Phase 9：Freshness / TTL

World observation：

支持：

```text
Fresh
Aging
Stale
Unknown
```

---

# 253. Phase 10：ConfirmationPolicy

支持：

```text
None
AdapterAck
ObserveAfter
ObserveUntil
```

---

# 254. Phase 11：Loop Guard

实现：

```text
origin
derivation depth
trace action budget
self echo prevention
```

---

# 255. Phase 12：PerceptionBudget

支持：

```text
per source
per channel
per cost class
```

---

# 256. Phase 13：Perception Degradation

接入 V6：

```text
Full
Reduced
CriticalOnly
```

---

# 257. Phase 14：World Model Integration

将：

```text
source
freshness
confidence
observation ids
```

正式提供给 V4。

---

# 258. Phase 15：Game Readiness

不实现具体游戏视觉。

验收：

```text
high-frequency raw observation
→ local reducer
→ salient GameEvent
→ EventBus
```

---

# 259. Phase 16：Voice Readiness

验收：

```text
audio stream
→ ASR result
→ utterance observation
→ WorldEvent
```

---

# 260. Phase 17：Audience Readiness

验收：

```text
burst
→ batch
→ cluster
→ AudienceSignal
```

---

# 261. Phase 18：Desktop Readiness

验收：

```text
screen / process / file observation
→ normalized Observation
→ WorldEvent
```

---

# 262. Definition of Done

V7 完成必须满足：

```text
[ ] World 可以通过 Observation 进入 EventBus
[ ] Adapter 不直接修改 Mind / WorldModel / WorkingState
[ ] ActionResult 统一回流为事件
[ ] ActionResult 与 World Observation 明确区分
[ ] Clock 只产生时间事件，不直接操作世界
[ ] Scheduler 可驱动 ObservationIntent
[ ] TaskSupervisor 产生 TaskEvent
[ ] 所有 Observation 有 source / time / freshness / quality
[ ] WorldModel 可识别 stale observation
[ ] stale 不自动变 false，而可变 unknown
[ ] Event dedupe 有效
[ ] Event coalesce 有效
[ ] high-frequency stream 不直接进入 LLM
[ ] self-origin echo 不形成循环
[ ] poll 有 deadline / backoff / cancellation
[ ] push 到达后可取消多余 poll
[ ] post-action confirmation 可配置
[ ] Event loop bounded
[ ] observation workers bounded
[ ] Reduced / CriticalOnly 能降低非关键感知
[ ] restart 后 observation subscriptions 可恢复
[ ] stale transient data 不在 restart 后当作 current
[ ] private observation 不泄漏跨 scope
[ ] ExpectedStable / UnexpectedStable / Unknown 明确区分
[ ] OutcomeEvaluation 区分 Confirmed / PartialProgress / NoObservedChange / Mismatch / Unknown
[ ] ProgressAssessment 区分 Progress / NoProgress / Regression / Unknown
[ ] World Changed 不自动等于 Goal Progress
[ ] Unknown 不被误判为 NoChange
[ ] 每个需要确认的 Action 有 bounded confirmation attempts
[ ] Retry 有 max attempts / deadline / backoff
[ ] 连续 NoProgress 可触发 PlanRevisionRequired
[ ] Perception 层不自动无限 Retry
[ ] MaintainState / AvoidState / ObserveUntil 类型 Goal 不被错误要求世界变化
[ ] V1～V6 行为保持兼容
```

---

# 263. 最终验收场景 A：外部世界驱动

```text
QQ user sends message
↓
Adapter
↓
Observation
↓
WorldEvent
↓
EventBus
↓
Attention
↓
Decision
```

通过。

---

# 264. 最终验收场景 B：行动反馈驱动

```text
ToolAction
↓
Adapter
↓
ActionResult::Succeeded
↓
WorldEvent
↓
TaskProgress
↓
Executive
```

通过。

---

# 265. 最终验收场景 C：世界确认

```text
Game Action:
TakeCover
↓
GameSkill succeeded
```

不能立即：

```text
WorldModel:
safe = true
```

必须：

```text
Observation
→ cover confirmed
```

---

# 266. 最终验收场景 D：时间驱动

```text
Clock
↓
OpenLoopDue
↓
Executive
↓
ObservationIntent
↓
Adapter
↓
Observation
↓
WorldEvent
```

通过。

---

# 267. 最终验收场景 E：任务驱动

```text
Task running
↓
TaskCompleted
↓
EventBus
↓
old status reply stale
↓
Rewrite
```

通过。

---

# 268. 最终验收场景 F：边打游戏边聊天

```text
Game raw telemetry
→ local perception
→ salient GameEvents

Audience stream
→ cluster
→ AudienceSignal

Voice
→ utterance events
```

三条事件流同时存在。

Executive 分配注意力。

---

# 269. 最终验收场景 G：世界状态过期

```text
Game source offline
```

旧：

```text
enemy_visible = true
```

必须逐步：

```text
Fresh
→ Aging
→ Stale
→ Unknown
```

不能永远保持 true。

---

# 270. 最终验收场景 H：自触发防环

```text
Yunxi sends QQ message
↓
platform echoes bot message
↓
Observation origin = Yunxi
```

系统不能：

```text
reply to itself forever
```

---

# 271. 最终验收场景 I：push + poll

已有 pending poll：

```text
check task after 5s
```

2 秒后 push：

```text
TaskCompleted
```

必须：

```text
consume push
cancel unnecessary poll
```

---

# 272. 最终验收场景 J：过载降级

Vision worker backlog 爆满：

```text
Full → Reduced
```

降低：

```text
background screen scan
world refresh
```

保留：

```text
direct message
task completion
critical game event
stop
reminder
```

---

# 273. 最终验收场景 K：ExpectedStable

```text
Task:
remote build running

Observe #1:
running

Observe #2:
running
```

在合理等待窗口内：

```text
ChangeAssessment = ExpectedStable
```

系统：

```text
Silent
→ schedule next check
```

不得误判失败。

---

# 274. 最终验收场景 L：UnexpectedStable

```text
Action:
OpenDoor

Before:
Closed

Expected:
Open

Observed:
Closed
```

结果：

```text
UnexpectedStable
→ OutcomeMismatch / NoObservedChange
→ Executive
```

不得自动无限重试。

---

# 275. 最终验收场景 M：Unknown

```text
Action:
StartProcess

Observation source:
Offline
```

结果：

```text
Outcome = Unknown
```

不能写：

```text
process_running = false
```

---

# 276. 最终验收场景 N：World Changed 但 Goal NoProgress

```text
Goal:
进入安全位置

Observed:
position changed
danger_level unchanged
```

结果：

```text
ChangeAssessment = Changed
ProgressAssessment = NoProgress
```

Executive 应考虑：

```text
Replan
```

---

# 277. 最终验收场景 O：连续 NoProgress

连续达到：

```text
max_no_progress_count
```

必须产生：

```text
PlanRevisionRequired
```

禁止继续同一 Action 无限循环。

---

# 278. V1～V7 最终分工

```text
V1 Core
= Agent 的平台无关生命循环

V2 Mind
= 自我、信念、兴趣与内部议程

V3 Executive
= 注意力、目标、冲突、计划与决策控制

V4 World Model
= 对外部世界的状态估计、预测与模拟

V5 Model Fabric
= 本地 / 云端 / 多模型计算基础设施

V6 Runtime Foundation
= 多任务、多通道、动作生命周期、恢复、降级和长期运行

V7 Perception–Action Loop
= 世界感知、行动反馈、时间驱动、任务驱动与持续闭环
```

---

# 279. 最终设计原则

> **World 不能只存在于 Action 的终点，它必须重新成为 Event 的起点。**

> **Adapter 负责观察和执行，不负责直接改 Yunxi 的内部世界。**

> **Action 成功，不代表世界一定已经变成预期状态。**

> **时间到了，不代表必须行动；时间只意味着需要重新评估。**

> **Task 状态变化，本身就是一种世界事件。**

> **高频感知必须先局部压缩，再进入高层认知。**

> **Observation 必须有来源、时间、新鲜度和可信度。**

> **过期信息应变 Unknown，而不是自动变 False。**

> **感知闭环必须 bounded，不能演化成无限轮询。**

> **V7 不要求世界必须变化，而要求系统判断“不变化是否符合预期”。**

> **环境没有变化、环境无法观察、Goal 没有推进，是三个不同状态。**

> **Action 成功、World Changed、Goal Progress 必须分别评估。**

> **连续 NoProgress 应推动 PlanRevision，而不是自动无限重试。**

> **真正持续的 Agent，不是不断调用 LLM，而是不断接收世界反馈并只在必要时思考。**

---

# 280. 结论

V7 完成后，Yunxi 的运行方式将从：

```text
用户消息
→ 思考
→ 回答
```

进一步升级为：

```text
世界变化
时间变化
任务变化
行动结果
        ↓
     EventBus
        ↓
   WorkingState
        ↓
Mind / WorldModel / Executive
        ↓
      Intent
        ↓
      Action
        ↓
      World
        ↓
   Observation
        ↓
     EventBus
```

也就是说：

> **Yunxi 不再只是“对输入做反应”，而是处在一个持续的感知—决策—行动—再感知循环中。**

这会成为后续 Game Runtime、Voice Runtime、Audience Runtime、Desktop Runtime 真正可靠接入的最后一块关键基础设施。

同时，V7 现在正式保证：

```text
World Changed
≠
Action Succeeded
≠
Goal Progress
```

以及：

```text
No Change
≠
Failure
```

系统必须结合 ExpectedOutcome、Observation 和 GoalProgress 判断“不变化是否符合预期”。
