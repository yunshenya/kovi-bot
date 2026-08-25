# Yunxi Runtime Foundation v6：持续 Agent 运行时、任务监督与多通道执行基础设施开发文档

**文档状态：** 最终设计稿  
**版本：** V6  
**定位：** Yunxi V1～V5 之上的运行时基础设施强化层  
**目标：** 将 Yunxi 从“具备持续认知能力的 Agent”进一步升级为“可长期稳定运行、可并行处理任务与交流、可接入游戏/语音/直播/桌面/移动端的通用 Agent Runtime”。

---

# 1. V6 的定位

V6 不是新的“思想层”。

```text
V1 Core
→ 我如何持续存在、接收事件、执行动作

V2 Mind
→ 我是谁、我相信什么、我在意什么

V3 Executive
→ 我如何分配注意力、管理目标、计划与冲突

V4 World Model
→ 外部世界现在是什么状态、可能发生什么

V5 Model Fabric
→ 这些计算由哪些模型完成

V6 Runtime Foundation
→ 上述能力如何在真实环境中长期、并行、可靠地运行
```

V6 的目标不是“更聪明”，而是：

```text
更稳定
更可并行
更可恢复
更可追踪
更可扩展
更适合实时环境
```

核心原则：

> **Mind 可以复杂，Executive 可以聪明，World Model 可以推测，但 Runtime Foundation 必须尽可能 deterministic。**

---

# 2. V6 要解决的问题

V6 必须解决：

```text
1. 长任务运行时 Conversation 不被阻塞
2. 用户可随时查询任务真实状态
3. 多任务并存但并发有界
4. QQ / Voice / Game / Audience / Desktop 都能成为 Interaction Channel
5. Core 不硬编码平台能力
6. 所有 Action 统一生命周期
7. 旧 Decision 在状态变化后必须 revalidate
8. 时间相关功能可测试
9. 事件具备因果链
10. 系统过载时可优雅降级
11. Host restart 后不重复副作用
12. Model / Tool await 不持有 Runtime 全局锁
13. Critical 工作不被 Reflection 等后台任务饿死
14. Game / Voice / Audience Runtime 可直接接入
```

---

# 3. V6 非目标

V6 不重新设计：

```text
SelfModel
Belief
Preference
Interest
Curiosity
OpenQuestion
InnerAgenda
Relation
Affect
World Simulation
LLM Routing
Embedding
Prompt Persona
Dialogue Style
```

也不实现：

```text
FPS 瞄准算法
ASR / TTS 声学模型
Live2D
QQ API
Steam / 游戏 SDK
桌面自动化细节
```

这些属于 V2～V5 或具体 Adapter / Runtime Module。

---

# 4. 总体架构

```text
                           YUNXI
                             │
        ┌────────────────────┼────────────────────┐
        ▼                    ▼                    ▼
     V2 Mind            V4 World Model       V5 Model Fabric
        │                    │                    │
        └──────────┬─────────┘                    │
                   ▼                              │
              V3 Executive                        │
                   │                              │
                   ▼                              │
                V1 Core ◄─────────────────────────┘
                   │
                   ▼
          V6 Runtime Foundation
                   │
      ┌────────────┼────────────┬────────────┐
      ▼            ▼            ▼            ▼
TaskSupervisor  Channels   Capabilities  ActionLifecycle
      │            │            │            │
      └────────────┴─────┬──────┴────────────┘
                         ▼
                       Hosts
          ┌──────────┬───┼───┬──────────┐
          ▼          ▼       ▼          ▼
         QQ       Desktop   Game      Voice/Audience
```

---

# 5. 正式模块

V6 包含：

```text
TaskSupervisor
TaskProgressSnapshot
CapabilityRegistry
InteractionChannel
RuntimeEventBus
Clock
Scheduler
ActionLifecycle
ActionArbiter
EventCausality
EventJournal
StateVersion
DecisionBasis
PreActionRevalidation
CancellationGraph
HostLifecycle
RuntimeDegradationMode
RuntimeBudget
RecoveryManager
RuntimeSnapshot
Observability
```

---

# 6. 目录建议

```text
crates/
└── yunxi-runtime/
    ├── src/
    │   ├── lib.rs
    │   ├── task/
    │   ├── capability/
    │   ├── channel/
    │   ├── action/
    │   ├── event/
    │   ├── time/
    │   ├── version/
    │   ├── cancellation/
    │   ├── degradation/
    │   ├── recovery/
    │   ├── host/
    │   └── observability/
    └── tests/
```

`yunxi-runtime` 不直接依赖：

```text
Kovi / NapCat / OneBot
QQ types
Tauri
Bilibili
Steam
Windows API
具体 ASR / TTS
具体 PostgreSQL pool
具体 Redis client
具体模型后端
```

允许依赖平台无关 domain types、抽象 ports、serde、uuid、tokio primitives、tracing/metrics。

# 7. TaskSupervisor

TaskSupervisor 是 V6 最重要的模块之一。

它统一管理所有：

> **持续一段时间、可能跨多个步骤、可查询状态、可暂停/取消/恢复的工作。**

核心规则：

```text
Task Running
!=
Conversation Locked
```

用户可以：

```text
开始任务 A
↓
A 后台运行
↓
继续聊天
↓
询问 A 状态
↓
A 继续运行
```

建议：

```rust
pub struct RuntimeTaskId(pub Uuid);

pub enum TaskState {
    Queued,
    Starting,
    Running,
    Waiting,
    Paused,
    Completing,
    Completed,
    Failed,
    Cancelled,
}
```

`Waiting` 表示：

```text
等待 ToolResult
等待用户信息
等待网络
等待时间
等待外部 GameEvent
```

不等于卡死。

---

# 8. TaskRecord

```rust
pub struct TaskRecord {
    pub id: RuntimeTaskId,
    pub goal_id: Option<GoalId>,
    pub owner: TaskOwner,
    pub kind: TaskKind,
    pub state: TaskState,
    pub priority: TaskPriority,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub current_step: Option<TaskStepId>,
    pub revision: u64,
    pub cancellation: CancellationState,
    pub lease: Option<TaskLease>,
}
```

```rust
pub enum TaskOwner {
    Person(PersonId),
    Conversation(ConversationId),
    Global,
    System,
}
```

```rust
pub enum TaskPriority {
    Critical,
    Interactive,
    Normal,
    Background,
    Maintenance,
}
```

---

# 9. TaskKind

建议：

```rust
pub enum TaskKind {
    UserRequest,
    ToolWorkflow,
    AgentGoal,
    ReminderDelivery,
    Maintenance,
    Reflection,
    WorldRefresh,
    GameActivity,
    AudienceInteraction,
    Custom(String),
}
```

不要绑定 QQ 业务。

---

# 10. TaskProgressSnapshot

用户问：

```text
“完成了吗？”
“做到哪了？”
“卡在哪里？”
```

必须读取真实状态，不允许模型猜。

```rust
pub struct TaskProgressSnapshot {
    pub task_id: RuntimeTaskId,
    pub state: TaskState,
    pub current_step: Option<String>,
    pub completed_steps: usize,
    pub total_steps: Option<usize>,
    pub progress_fraction: Option<f32>,
    pub last_update_at: DateTime<Utc>,
    pub last_result_summary: Option<String>,
    pub waiting_reason: Option<String>,
    pub failure_summary: Option<String>,
}
```

不知道百分比时：

```text
progress_fraction = None
```

允许回答：

```text
“还在跑测试。”
```

禁止虚构：

```text
“已经 73%。”
```

---

# 11. Task Query

```rust
pub trait TaskQuery {
    async fn get_task(
        &self,
        id: RuntimeTaskId,
    ) -> Result<TaskProgressSnapshot>;

    async fn list_active(
        &self,
        owner: TaskOwner,
    ) -> Result<Vec<TaskProgressSnapshot>>;
}
```

用户：

```text
“刚才那个做完了吗？”
```

流程：

```text
Semantic TaskStatusIntent
→ resolve task
→ TaskProgressSnapshot
→ reply formulation
```

不是让 LLM 回忆自己“好像做到哪了”。

---

# 12. Task Pause / Resume / Cancel

不是所有 Task 都支持暂停。

Provider / Capability 应声明：

```text
supports_pause
supports_resume
supports_cancel
```

不支持时必须返回：

```text
Unsupported
```

取消状态：

```rust
pub enum CancellationState {
    None,
    Requested,
    Acknowledged,
    Completed,
}
```

取消不意味着撤销已经 `Committed` 的现实副作用。

---

# 13. Task Recovery

Host restart 后：

```text
Queued
Running
Waiting
```

必须根据策略恢复。

```text
Idempotent Tool Workflow
→ Resume

Non-idempotent Action uncertainty
→ Reconcile first

Reflection
→ Drop / Reschedule

Old proactive draft
→ Drop
```

现有 `agent_tasks` 已有 lease/retry/idempotency/restart recovery，应优先 bridge，而不是重写。

# 14. CapabilityRegistry

未来 Yunxi 会运行在多个环境。

Core 不应：

```rust
if qq { ... }
if game { ... }
if desktop { ... }
```

而应问：

```text
当前 Host 提供哪些能力？
```

建议：

```rust
pub struct CapabilityId(pub String);

pub enum CapabilityKind {
    SendText,
    SendImage,
    ReceiveText,
    ReceiveVoice,
    Speak,
    Listen,
    ObserveScreen,
    ObserveGame,
    ControlGame,
    UseTool,
    Notify,
    FileRead,
    FileWrite,
    NetworkFetch,
    Custom(String),
}
```

---

# 15. CapabilityDescriptor

```rust
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub kind: CapabilityKind,
    pub host_id: HostId,
    pub scope: CapabilityScope,
    pub availability: CapabilityAvailability,
    pub constraints: CapabilityConstraints,
    pub metadata: BTreeMap<String, String>,
}
```

```rust
pub enum CapabilityAvailability {
    Available,
    Degraded,
    Busy,
    Offline,
    PermissionDenied,
}
```

Capability 描述：

```text
“能不能做”
```

Mind 描述：

```text
“想不想做”
```

Executive 描述：

```text
“现在该不该做”
```

三者必须分离。

---

# 16. Capability Constraints

可包含：

```text
max message length
rate limit
supports streaming
supports cancellation
requires foreground
requires consent
supports image
supports voice
```

Registry API：

```rust
pub trait CapabilityRegistry {
    fn list(&self) -> Vec<CapabilityDescriptor>;
    fn find(&self, query: CapabilityQuery) -> Vec<CapabilityDescriptor>;
    fn get(&self, id: &CapabilityId) -> Option<CapabilityDescriptor>;
}
```

---

# 17. Capability Hot Reload

能力可以运行时变化：

```text
麦克风拔出
游戏关闭
Voice permission revoked
Tool backend offline
```

变化：

```text
capability_version + 1
```

旧 Prepared Action commit 前必须 revalidate。

# 18. InteractionChannel

`ConversationId` 保留，但未来很多交互并不是传统 Conversation。

增加：

```rust
pub enum InteractionChannelKind {
    DirectChat,
    GroupChat,
    AudienceStream,
    Voice,
    Game,
    Desktop,
    Mobile,
    System,
    Custom(String),
}
```

```rust
pub struct ChannelId(pub Uuid);

pub struct InteractionChannel {
    pub id: ChannelId,
    pub kind: InteractionChannelKind,
    pub host_id: HostId,
    pub conversation_id: Option<ConversationId>,
    pub activity: ChannelActivity,
    pub capabilities: Vec<CapabilityId>,
}
```

---

# 19. ChannelActivity

```rust
pub struct ChannelActivity {
    pub last_event_at: Option<DateTime<Utc>>,
    pub activity_level: f32,
    pub interruption_cost: f32,
    pub realtime_criticality: f32,
}
```

示例：

```text
Channel A = QQ Private
Channel B = Bilibili Audience
Channel C = Game Runtime
Channel D = Voice
```

V3 Executive 可读取 ChannelActivity：

```text
Game critical
→ audience reply Defer

Game idle
→ audience priority rises
```

Channel 不替代 Conversation，只是更上层的运行容器。

# 20. RuntimeEventBus

V1 已定义 WorldEvent。

V6 需要稳定、bounded 的 runtime event transport。

```rust
pub struct RuntimeEvent {
    pub meta: EventMeta,
    pub scope: EventScope,
    pub priority: EventPriority,
    pub payload: WorldEvent,
}
```

```rust
pub enum EventPriority {
    Critical,
    Interactive,
    Normal,
    Background,
    Maintenance,
}
```

关键原则：

```text
bounded
backpressure-aware
priority-aware
observable
```

至少：

```text
Critical
→ 不允许静默丢弃

Interactive
→ 强保留

Background
→ 可 coalesce / drop stale

Maintenance
→ 可 defer
```

禁止无限 queue。

# 21. Clock

Core 内不要到处：

```rust
Utc::now()
```

统一：

```rust
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
```

生产：

```text
SystemClock
```

测试：

```text
FakeClock
```

可：

```text
advance 2h
```

然后立即测试：

```text
OpenLoop due
Reminder due
Expectation expired
Interest decay
Task deadline
```

---

# 22. Scheduler

不要让 Runtime 逻辑散落：

```rust
tokio::time::sleep(...)
```

建议：

```rust
pub trait RuntimeScheduler {
    async fn schedule(
        &self,
        job: ScheduledJob,
    ) -> Result<ScheduledJobId>;

    async fn cancel(
        &self,
        id: ScheduledJobId,
    ) -> Result<()>;
}
```

Scheduler 是基础设施。

```text
Reminder
OpenLoop
ReflectionTick
MaintenanceTick
```

都可使用它，但它们的业务语义仍属于原层。

# 23. ActionLifecycle

V6 将 V1 的 Intent / Action 正式提升为统一生命周期。

```rust
pub enum ActionState {
    Proposed,
    Validating,
    Validated,
    Prepared,
    Committed,
    Executing,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}
```

`Committed` 的定义：

> 已经跨过不可安全撤销的现实副作用边界。

例如：

```text
message handed to platform send API
game command injected
notification delivered
external write submitted
```

---

# 24. RuntimeAction

```rust
pub struct RuntimeAction {
    pub id: ActionId,
    pub intent_id: IntentId,
    pub kind: ActionKind,
    pub state: ActionState,
    pub owner_task_id: Option<RuntimeTaskId>,
    pub basis: DecisionBasis,
    pub capability_id: CapabilityId,
    pub idempotency_key: IdempotencyKey,
}
```

---

# 25. ActionArbiter

ActionArbiter 负责：

```text
permission
capability
rate limit
priority
conflict
duplicate
current state
commit eligibility
```

它不负责判断：

```text
“这句话好不好听”
```

只负责：

```text
“这个 Action 现在是否允许进入现实副作用边界”
```

---

# 26. 通用 Commit 规则

消息：

```text
Prepared
→ Revalidate
→ Committed
→ Sent
```

Tool：

```text
Proposed
→ Permission
→ Prepared
→ Committed
→ Executing
→ Result
```

Game：

```text
TakeCover
→ Prepared
→ Committed to GameSkillLayer
→ Executing
→ Result
```

统一语义，减少每个 Adapter 自建状态机。

# 27. Pre-Action Revalidation

所有尚未 `Committed` 的非 trivial Action，在 commit 前检查：

```text
DecisionBasis still valid?
Capability still available?
Permission still valid?
Task still active?
Target still valid?
Stop/cancel arrived?
Conversation materially changed?
World materially changed?
```

无竞争：

```text
fast deterministic path
```

不应增加额外 LLM。

只有语义灰区才进入 Executive。

---

# 28. Action Idempotency

所有可 retry 的副作用 Action 必须：

```text
IdempotencyKey
```

Adapter timeout：

```text
timeout
→ reconcile
→ retry only if not already committed/succeeded
```

禁止出现：

```text
一次用户动作
→ 两次真实发送
```

# 29. EventCausality

系统必须能够回答：

```text
“为什么刚才发生了这件事？”
```

但不保存 hidden chain-of-thought。

```rust
pub struct EventMeta {
    pub event_id: EventId,
    pub root_event_id: EventId,
    pub parent_event_id: Option<EventId>,
    pub trace_id: TraceId,
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
}
```

示例：

```text
MessageReceived #1
→ GoalCreated #2
→ TaskStarted #3
→ ToolAction #4
→ ToolCompleted #5
→ Decision #6
→ MessageAction #7
→ MessageSent #8
```

---

# 30. EventJournal

保存 bounded runtime journal。

保存：

```text
event metadata
state transition
reason tags
trace relation
```

不保存：

```text
system prompt
API key
token
private secret
hidden model reasoning
```

V3 DecisionRecord 与 V6 EventJournal 分开：
- DecisionRecord：结构化决策元数据
- EventJournal：运行事实

# 31. StateVersion

V2 / V3 / V4 / Conversation / Task / Capability 会同时变化。

建议：

```rust
pub struct StateVersionSet {
    pub conversation: u64,
    pub mind: u64,
    pub world: u64,
    pub executive: u64,
    pub relation: u64,
    pub task: u64,
    pub capability: u64,
}
```

```rust
pub struct DecisionBasis {
    pub versions: StateVersionSet,
    pub source_event_id: EventId,
    pub created_at: DateTime<Utc>,
}
```

---

# 32. 为什么需要版本

```text
15:00:00
Task = Running

Planner:
准备回答“还没完成”

15:00:01
Task = Completed
task_version + 1

15:00:02
旧 reply 尝试 commit
```

结果：

```text
version changed
→ revalidate
→ stale
→ rewrite
```

最终：

```text
“刚刚完成了。”
```

---

# 33. Version Changed 不等于必取消

变化可能无关。

建议：

```rust
pub enum ChangeImpact {
    None,
    Low,
    Relevant,
    Invalidating,
}
```

策略：

```text
same version
→ commit

known irrelevant
→ commit

known invalidating
→ cancel / supersede

gray zone
→ Executive
```

# 34. CancellationGraph

长期 Agent 的工作链：

```text
Goal
→ Task
→ Plan
→ Action
→ ModelGeneration
→ ToolCall
```

取消必须传播。

```rust
pub enum CancellationNode {
    Goal(GoalId),
    Task(RuntimeTaskId),
    Action(ActionId),
    Generation(GenerationId),
    ToolCall(ToolCallId),
}
```

例如：

```text
cancel Task A
→ cancel future actions
→ cancel model generations
→ request tool cancellation
→ preserve already committed effects
```

取消不得误伤同 Conversation 下的 Task B。

# 35. HostLifecycle

未来 Host：

```text
QQ
Desktop
Server
Game
Voice
Audience
```

统一状态：

```rust
pub enum HostState {
    Starting,
    Ready,
    Degraded,
    Stopping,
    Stopped,
    Failed,
}
```

启动：

```text
load runtime state
recover tasks
reconcile committed actions
register capabilities
start scheduler
emit HostStarted
```

停止：

```text
stop new background work
persist task state
release leases
flush critical journal
cancel safe generations
reconcile committed actions
emit HostStopping
```

# 36. RuntimeDegradationMode

系统过载不能所有能力一起崩。

```rust
pub enum CoreMode {
    Full,
    Reduced,
    CriticalOnly,
}
```

Full：

```text
Direct Reply
Proactive
Reflection
Simulation
World Refresh
Maintenance
```

Reduced：

```text
保留 Direct Reply
保留 Reminder
保留 Task Result
降低 proactive
暂停 deep reflection
降低 simulation
延后 reembedding
```

CriticalOnly：

```text
Direct MustHandle
Reminder
Stop
Security
Task Result Delivery
Admin / Data operations
```

进入降级可参考：

```text
event backlog
model backlog
CPU / memory
GPU queue
DB health
tool health
host health
```

可靠义务不得因降级静默丢失。

# 37. RuntimeBudget

建议：

```rust
pub struct RuntimeBudget {
    pub max_active_tasks: usize,
    pub max_background_tasks: usize,
    pub max_pending_actions: usize,
    pub max_event_backlog: usize,
    pub max_model_generations: usize,
}
```

必须支持：

```text
per conversation quota
per person quota
per channel quota
```

避免某一群或某个直播事件流吃完所有资源。

低优先级任务可 aging，但 Background 不得最终压过 Critical MustExecute。

# 38. RecoveryManager

异常重启必须被视为正常生产场景。

分类：

### Safe Resume

```text
read-only query
idempotent computation
persistent agent_tasks
```

### Reconcile First

```text
message send uncertain
external write uncertain
game command uncertain
```

### Drop

```text
old proactive prepared draft
stale reflection
temporary thought
```

`Prepared` 自然语言输出重启后默认 Drop / Replan。

`Committed` 但结果未知：

```text
reconcile
```

禁止直接重发。

# 39. RuntimeSnapshot

为 Debug 提供：

```rust
pub struct RuntimeSnapshot {
    pub mode: CoreMode,
    pub active_tasks: usize,
    pub queued_tasks: usize,
    pub pending_actions: usize,
    pub channels: Vec<ChannelSnapshot>,
    pub capability_health: Vec<CapabilityHealth>,
    pub event_backlog: usize,
    pub last_event_at: Option<DateTime<Utc>>,
}
```

默认不包含完整私聊正文。

---

# 40. Observability

至少能回答：

```text
现在多少任务？
哪个任务在跑？
哪个 Channel 最忙？
为什么某 Action 被取消？
为什么进入 Reduced？
为什么 generation 被丢弃？
```

建议 metrics：

```text
yunxi_runtime_active_tasks
yunxi_runtime_queued_tasks
yunxi_runtime_task_completed_total
yunxi_runtime_task_failed_total
yunxi_runtime_task_cancelled_total
yunxi_runtime_pending_actions
yunxi_runtime_action_committed_total
yunxi_runtime_action_superseded_total
yunxi_runtime_action_revalidation_total
yunxi_runtime_event_backlog
yunxi_runtime_event_dropped_total
yunxi_runtime_mode
yunxi_runtime_recovery_total
```

Reason Tags：

```text
TASK_COMPLETED
TASK_FAILED
TASK_CANCELLED
TASK_WAITING
CAPABILITY_UNAVAILABLE
PERMISSION_DENIED
STATE_VERSION_CHANGED
ACTION_STALE
ACTION_SUPERSEDED
USER_STOP
USER_MESSAGE_NEWER
DIRECT_PREEMPTS_BACKGROUND
HOST_STOPPING
RUNTIME_OVERLOADED
RECOVERY_RECONCILE
```

# 41. 与 V1～V5 的关系

## V1 Core

负责：

```text
Identity
WorldEvent
OpenLoop
Intent
ConversationCoordinator
平台无关 Agent Core
```

V6 强化：

```text
Task lifecycle
Action lifecycle
Scheduling
Capability
Channel
Recovery
Versioning
Degradation
```

## V2 Mind

Runtime 不修改 Belief 语义，只读取稳定 Snapshot。

## V3 Executive

Executive 决定：

```text
priority
candidate
defer
goal arbitration
gray-zone revalidation
```

Runtime 负责：

```text
真正排队
真正取消
真正 commit
真正执行
```

## V4 World Model

World Model 可观察：

```text
Host health
Capability health
Channel activity
Task status
Environment state
```

但不直接执行。

## V5 Model Fabric

V6 向 V5 提供：

```text
priority
cancellation token
task id
trace id
deadline
```

V5 返回模型计算能力，不拥有现实副作用。

# 42. 并发与锁原则

禁止：

```rust
let guard = runtime.lock().await;
call_llm().await;
```

正确：

```text
lock
→ snapshot
→ unlock
→ await model/tool/network
→ re-lock
→ version check
→ apply
```

Definition of Done：

> **Runtime 全局锁不得跨 model/tool/network await。**

建议 structured concurrency。

禁止到处裸：

```rust
tokio::spawn(...)
```

Host shutdown 时必须知道哪些 runtime tasks 仍然存活。

# 43. Error Taxonomy

```rust
pub enum RuntimeErrorKind {
    Transient,
    Timeout,
    Cancelled,
    Permission,
    CapabilityUnavailable,
    InvalidState,
    Conflict,
    Permanent,
    Unknown,
}
```

Task failure：

```text
TaskFailed Event
```

不应导致 Runtime crash。

Capability failure：

```text
Capability → Degraded / Offline
```

不必整体退出。

# 44. Persistence Boundary

Runtime domain 不依赖 SQLx。

建议：

```rust
pub trait RuntimeStateStore {
    async fn save_task(&self, task: &TaskRecord) -> Result<()>;
    async fn load_active_tasks(&self) -> Result<Vec<TaskRecord>>;
    async fn append_action(&self, action: &RuntimeAction) -> Result<()>;
    async fn append_event_meta(&self, meta: &EventMeta) -> Result<()>;
}
```

V6 不要求全面 Event Sourcing。

只要求：

```text
关键状态可恢复
关键动作可追踪
关键因果链可解释
```

# 45. Game Runtime Readiness

V6 不实现游戏控制，但必须为未来 Game Runtime 提供：

```text
InteractionChannel::Game
Capability::ObserveGame
Capability::ControlGame
TaskKind::GameActivity
high-priority RuntimeEvent
ActionLifecycle
CancellationGraph
StateVersion
RuntimeBudget
```

这使得：

```text
Game Channel
+
Audience Channel
+
Voice Channel
```

可以同时存在。

游戏不因观众回复被锁住，观众也不因游戏永远饿死。

真正“Boss 战少说、死亡等待多回复”由 V3 Executive 决定。

# 46. Voice / Audience / Desktop Readiness

Voice Host 可注册：

```text
ReceiveVoice
Speak
```

TTS 播放时：

```text
不能持有 Conversation lock
不能持有 Executive lock
不能持有 Game control lock
```

Audience：

```text
大量弹幕
→ batch / cluster / coalesce
→ AudienceSignal
```

不能：

```text
1000 messages → 1000 planner calls
```

Desktop：

```text
ObserveScreen
FileRead
FileWrite
Notify
```

Core 不知道具体 Tauri / Win32 / Cocoa 实现。

# 47. Security Boundary

CapabilityRegistry 不是权限绕过器。

真实执行仍必须经过：

```text
Permission
ToolAccess
ActionArbiter
```

敏感 Capability：

```text
Camera
Microphone
FileWrite
GameControl
DesktopControl
```

必须显式反映 Host permission。

权限在 Prepared 后被撤销：

```text
commit revalidation
→ reject
```

# 48. Migration Strategy

V6 不允许 Big Bang Rewrite。

迁移顺序：

```text
Phase 0  Runtime Domain Types
Phase 1  Clock / Scheduler
Phase 2  TaskSupervisor
Phase 3  agent_tasks Bridge
Phase 4  CapabilityRegistry
Phase 5  InteractionChannel
Phase 6  ActionLifecycle
Phase 7  StateVersion / DecisionBasis
Phase 8  PreActionRevalidation
Phase 9  EventCausality / Journal
Phase 10 CancellationGraph
Phase 11 RecoveryManager
Phase 12 DegradationMode
Phase 13 RuntimeBudget / Fairness
Phase 14 Observability
Phase 15 Game Readiness
Phase 16 Voice Readiness
Phase 17 Audience Readiness
```

优先：

```text
Reuse
→ Wrap
→ Bridge
→ Shadow
→ Migrate
```

而不是：

```text
Delete
→ Rewrite Everything
```

# 49. Phase 0：Runtime Domain Types

实现：

```text
RuntimeTaskId
TaskState
TaskRecord
TaskProgressSnapshot
ActionState
RuntimeAction
ChannelId
InteractionChannel
CapabilityDescriptor
EventMeta
StateVersionSet
DecisionBasis
```

DoD：

```text
no platform dependency
serde
unit tests
```

---

# 50. Phase 1：Clock / Scheduler

实现：

```text
Clock
SystemClock
FakeClock
RuntimeScheduler
```

所有新 Runtime 时间逻辑必须走抽象。

---

# 51. Phase 2：TaskSupervisor

实现：

```text
create
start
query
pause
resume
cancel
complete
fail
```

---

# 52. Phase 3：agent_tasks Bridge

把现有成熟 `agent_tasks`：

```text
映射为 RuntimeTask provider
```

先 shadow，再迁移。

---

# 53. Phase 4：CapabilityRegistry

首先注册现有：

```text
QQ SendText
QQ ReceiveText
Tools
```

---

# 54. Phase 5：InteractionChannel

先实现：

```text
QQ Private
QQ Group
System
```

为未来预留：

```text
Game
Voice
Audience
Desktop
Mobile
```

# 55. Phase 6：ActionLifecycle

将现有：

```text
send message
tool execution
```

接入统一 ActionLifecycle。

---

# 56. Phase 7：State Versioning

先 version：

```text
Conversation
Task
Capability
```

再接：

```text
Mind
World
Executive
```

---

# 57. Phase 8：PreActionRevalidation

支持：

```text
Message
Tool
Task Control
```

不默认增加 LLM。

---

# 58. Phase 9：EventCausality

加入：

```text
trace_id
root_event_id
parent_event_id
sequence
```

---

# 59. Phase 10：CancellationGraph

连接：

```text
Task
Action
Generation
ToolCall
```

---

# 60. Phase 11：RecoveryManager

支持：

```text
task recovery
action reconcile
scheduler recovery
```

---

# 61. Phase 12：DegradationMode

实现：

```text
Full
Reduced
CriticalOnly
```

---

# 62. Phase 13：RuntimeBudget

支持：

```text
priority
owner
channel
bounded concurrency
```

---

# 63. Phase 14：Observability

实现：

```text
metrics
trace
reason tags
debug snapshot
```

---

# 64. Phase 15：Game Readiness

不实现具体游戏控制。

验收：

```text
Game Channel can register
ObserveGame capability can register
ControlGame capability can register
GameActivity task can run
GameEvents coexist with Conversation
```

---

# 65. Phase 16：Voice Readiness

验收：

```text
Voice Channel
Speak Capability
ReceiveVoice Capability
TTS Action lifecycle
```

---

# 66. Phase 17：Audience Readiness

验收：

```text
Audience Channel
event burst
batch/coalesce
interactive priority
```

# 67. Testing Strategy

必须覆盖：

```text
unit
integration
race
restart
load
security
```

Task：

```text
start
wait
pause
resume
cancel
complete
fail
illegal transition
```

Capability：

```text
register
update
offline
permission denied
version bump
```

Action：

```text
prepare
revalidate
commit
cancel before commit
cannot supersede after commit
idempotency
```

Clock：

```text
FakeClock advance
→ due event
```

Version：

```text
same → commit
invalidating change → reject
irrelevant change → allow
```

# 68. Race Tests

### Task 完成 vs 状态回复

```text
Prepared:
“还没”

TaskCompleted
```

必须：

```text
rewrite / supersede
```

### Cancel vs Commit

```text
cancel before commit
→ no side effect

commit first
→ preserve committed side effect
→ stop future steps
```

### Host Stop vs Task Start

HostStopping 后：

```text
no new Background Task
```

### Stale Generation

generation 被取消但模型仍返回：

```text
discard
```

# 69. Restart Tests

模拟 process crash。

验证：

```text
active task recovery
committed action reconciliation
scheduler recovery
lease recovery
```

Prepared natural-language output：

```text
drop
```

不允许重启后突然发送几分钟前的草稿。

---

# 70. Load Tests

Audience storm：

```text
10k events
```

必须：

```text
bounded memory
Critical not dropped
background coalesced
```

Model backlog：

```text
Reflection 堵塞
```

Direct Reply 仍可运行。

Task burst：

```text
100 tasks
```

不能变成：

```text
100 expensive workers simultaneously
```

# 71. State Transition Rules

Task 合法转换：

```text
Queued → Starting
Starting → Running
Running → Waiting
Running → Paused
Running → Completing
Running → Failed
Running → Cancelled
Waiting → Running
Paused → Running
Completing → Completed
```

例如：

```text
Completed → Running
```

默认非法。

Action 合法转换：

```text
Proposed → Validating
Validating → Validated
Validated → Prepared
Prepared → Committed
Prepared → Superseded
Prepared → Cancelled
Committed → Executing
Executing → Succeeded
Executing → Failed
```

`Committed` 后通常不能 `Superseded`。

# 72. 最终验收场景：长任务 + 对话

```text
User:
“帮我跑完整测试。”

Yunxi:
“好。”

Task:
Running

User:
“Rust trait object 那个问题怎么回事？”

Yunxi:
正常回答

Task:
仍然 Running
```

通过标准：

```text
Conversation 未被 Task 锁死
```

---

# 73. 最终验收场景：询问进度

```text
User:
“测试做完了吗？”
```

必须：

```text
TaskProgressSnapshot
```

驱动回答。

禁止模型猜。

---

# 74. 最终验收场景：状态刚好变化

```text
Prepared reply:
“还没。”

TaskCompleted
```

必须：

```text
PreActionRevalidation
→ task_version changed
→ rewrite
```

输出类似：

```text
“刚刚完成了。”
```

# 75. 最终验收场景：多通道

同时注册：

```text
QQ Channel
Game Channel
Audience Channel
Voice Channel
```

均可产生 RuntimeEvent。

---

# 76. 最终验收场景：边打游戏边交流

```text
GameActivity Task
→ Running continuously

AudienceEvent
→ enters Attention / Executive

Speech Action
→ TTS

Game Task
→ keeps running
```

TTS 播放期间：

```text
Game Runtime 不暂停
```

---

# 77. 最终验收场景：系统过载

Model queue 爆满：

```text
Full → Reduced
```

暂停：

```text
Deep Reflection
Simulation
Low-value Proactive
```

保留：

```text
Direct
Reminder
Stop
Task Result
```

---

# 78. 最终验收场景：重启

Task 正在运行时 crash。

启动：

```text
Recover
Reconcile
Resume / Fail safely
```

不能重复现实副作用。

---

# 79. 最终验收场景：Capability 变化

游戏关闭：

```text
ControlGame → Offline
capability_version + 1
```

旧 Prepared Game Action：

```text
revalidate
→ reject
```

---

# 80. 最终验收场景：取消

User：

```text
“别跑测试了。”
```

Task：

```text
Cancellation Requested
```

未 Committed 后续 Action：

```text
cancel
```

已 Committed 的现实副作用：

```text
不假装撤销
```

# 81. 最终验收场景：因果追踪

管理员问：

```text
“为什么芸汐刚才主动发了这句话？”
```

可追踪：

```text
MessageSent
← ActionCommitted
← ExecutiveDecision
← OpenLoopDue
← original user event
```

不暴露 hidden chain-of-thought。

---

# 82. Definition of Done

V6 完成必须满足：

```text
[ ] 长任务不阻塞 Conversation
[ ] 用户可查询真实任务状态
[ ] Task 可暂停/取消/恢复/失败
[ ] 多 Task bounded concurrency
[ ] CapabilityRegistry 平台无关
[ ] InteractionChannel 支持 chat/game/voice/audience
[ ] ActionLifecycle 统一
[ ] Prepared / Committed 边界明确
[ ] 关键 Action 有 idempotency
[ ] DecisionBasis 可检测 stale state
[ ] PreActionRevalidation 阻止过时 Action
[ ] Clock 可替换 FakeClock
[ ] Scheduler 不散落 sleep
[ ] Event 有 trace / parent / root
[ ] Cancellation 正确传播
[ ] Host restart 可恢复关键状态
[ ] Prepared 自然语言重启后不乱发
[ ] Runtime 可降级
[ ] Critical 义务不因降级丢失
[ ] Event queue bounded
[ ] 不在 global lock 下 await model/tool/network
[ ] Background 不饿死 Direct Reply
[ ] Debug snapshot 不泄露完整私聊正文
[ ] V1～V5 行为保持兼容
```

# 83. V1～V6 最终分工

```text
V1 Core
= Agent 的平台无关核心生命循环

V2 Mind
= 长期心智、自我、信念、兴趣与内部议程

V3 Executive
= 注意力、冲突、目标、计划与决策控制

V4 World Model
= 对外部世界的状态估计、预测与模拟

V5 Model Fabric
= 本地/云端/多模型计算基础设施

V6 Runtime Foundation
= 任务监督、多通道、动作生命周期、版本一致性、恢复、降级与长期运行
```

---

# 84. 最终设计原则

> **持续，不等于死循环调用模型。**

> **并行，不等于无限 spawn。**

> **任务在运行，不等于聊天被锁住。**

> **模型生成完成，不等于 Action 可以执行。**

> **状态改变，不等于旧 Decision 仍然有效。**

> **Host 能做什么，由 Capability 描述，而不是 Core 硬编码。**

> **时间是依赖，不是全局常量。**

> **副作用必须有 Commit 边界。**

> **重启是正常生产状态，不是异常世界。**

> **过载时要降级，而不是失去可靠义务。**

> **Runtime 的工作不是让芸汐“更像人”，而是让她真的能够长期稳定地活在系统里。**

---

# 85. 结论

V6 完成后，Yunxi 将拥有：

```text
持续运行
+
并行任务
+
多通道交互
+
真实任务状态
+
统一动作生命周期
+
状态版本一致性
+
取消传播
+
因果追踪
+
故障恢复
+
资源降级
```

这将直接为：

```text
Game Runtime
Voice Runtime
Audience Runtime
Desktop Runtime
Mobile Runtime
Embodiment
```

提供稳定底座。

V6 的成功标准不是：

```text
“芸汐说得更像真人”
```

而是：

> **无论她正在聊天、跑任务、打游戏、说话还是处理直播弹幕，Runtime 都能保证这些活动彼此协作，而不是互相卡死。**
