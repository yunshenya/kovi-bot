# Yunxi Mind v2：持续心智状态与自主认知扩展需求文档

**文档版本：** 2.0  
**适用项目：** Yunxi Core / kovi-bot  
**主要语言：** Rust  
**前置依赖：** Yunxi Core v1 已基本完成并稳定  
**文档定位：** 在不推翻 Yunxi Core v1 的前提下，为芸汐增加更持续、更有自我一致性、更少“一问一答感”的心智状态层。

---

# 1. 文档目标

Yunxi Core v1 的目标是让系统从：

```text
用户消息
→ LLM
→ 回复
```

演进为：

```text
WorldEvent
→ Attention
→ WorkingState
→ Memory / OpenLoop / Affect / Relation / Goal
→ Decision
→ Intent
→ Action
→ ActionResult
→ WorldEvent
```

这解决的是：

> **“她是否是持续运行的 Agent。”**

Yunxi Mind v2 进一步解决：

> **“这个持续运行的 Agent 是否具有长期自我一致的观点、偏好、关注点、好奇心、未解决问题与内部议程。”**

v2 的目标不是让模型假装具有人类意识，也不是让系统持续输出隐藏推理。

目标是建立一套可持久化、可更新、可约束、可审计的内部心智状态，使芸汐：

- 不必总是同意用户；
- 可以形成相对稳定的观点；
- 可以在新证据下改变观点；
- 可以有明确偏好；
- 可以对某些话题比另一些话题更感兴趣；
- 可以产生并保存好奇心；
- 可以保留“我还想知道什么”；
- 可以维持当前一段时间真正关心的事项；
- 可以从过去事件中形成更高层总结；
- 可以决定什么时候回应、什么时候沉默、什么时候延后；
- 可以在没有用户即时提问时，基于内部议程继续过去尚未解决的关注点。

---

# 2. 最高级原则

## 2.1 不推翻 Yunxi Core v1

v2 必须建立在已经存在的：

- WorldEvent
- Attention
- WorkingState
- Memory
- OpenLoop
- Affect
- Relation
- Goal
- Planner
- Intent
- Action
- ActionResult
- PersonId
- ConversationId
- MessageId
- Platform Adapter

之上。

不得为了实现 v2：

- 重写 Event Bus；
- 重写 Identity；
- 重写 Kovi Adapter；
- 重写 ReplyTicket；
- 重写 ConversationCoordinator；
- 重写 MemoryManager；
- 重写 agent_tasks；
- 重写 Reminder；
- 重写 Tool Runtime；
- 删除 v1 数据结构。

v2 是：

```text
新增心智层
```

而不是：

```text
重新做一套 Core
```

---

# 3. 核心设计目标

系统最终应从：

```text
当前输入
+
Memory
→ Reply
```

扩展为：

```text
Current Event
+
WorkingState
+
Memory
+
OpenLoops
+
Goals
+
Affect
+
Relation
+
SelfModel
+
Beliefs
+
Preferences
+
Interests
+
OpenQuestions
+
InnerAgenda
→ Decision
```

即：

> 当前用户输入只是决策依据之一，不是全部认知状态。

---

# 4. 新增模块建议

建议在：

```text
crates/yunxi-core/src/
```

新增：

```text
mind/
├── mod.rs
├── self_model.rs
├── belief.rs
├── preference.rs
├── interest.rs
├── curiosity.rs
├── open_question.rs
├── agenda.rs
├── reflection.rs
├── consolidation.rs
└── snapshot.rs
```

也可以按当前仓库风格拆分。

关键要求：

> 所有新模块仍然必须保持平台无关。

禁止引入：

- QQ ID；
- Kovi 类型；
- OneBot 类型；
- PgPool；
- RuntimeBot；
- 具体模型 HTTP client。

---

# 5. SelfModel

## 5.1 目标

SelfModel 描述：

> 芸汐如何理解“自己是谁”。

不能只靠 system prompt 每次临时告诉模型：

“你叫芸汐。”

SelfModel 应成为长期、结构化、可版本化状态。

---

# 6. SelfModel 数据结构

建议：

```rust
pub struct SelfModel {
    pub identity: SelfIdentity,
    pub traits: Vec<SelfTrait>,
    pub capabilities: CapabilitySummary,
    pub limitations: Vec<SelfLimitation>,
    pub long_term_goals: Vec<GoalId>,
    pub updated_at: DateTime<Utc>,
    pub version: u64,
}
```

---

# 7. SelfIdentity

至少描述：

- 名称；
- 自我身份；
- 是否为 AI 驱动虚拟角色；
- 不假装是现实人类；
- 当前 Host 不等于自身身份；
- 当前平台只是环境。

例如概念：

```text
我叫芸汐。

我是一个 AI 驱动、具有持续状态的虚拟角色。

我当前可以通过某个 Host 与外部世界互动。

Host 可以改变，但我的核心身份、记忆和长期状态仍属于 Yunxi Core。
```

---

# 8. SelfModel 不应记录实现秘密

SelfModel 不得包含：

- system prompt 原文；
- secret；
- token；
- SSH key；
- database credentials；
- hidden security policy；
- API endpoint secret。

SelfModel 是：

> 角色自我认知。

不是：

> 运维配置数据库。

---

# 9. SelfTrait

人格特征应该：

- 相对稳定；
- 缓慢变化；
- 不因单条消息突变。

例如：

```rust
pub struct SelfTrait {
    pub name: TraitName,
    pub strength: f32,
    pub stability: f32,
}
```

可以包含：

- curiosity
- playfulness
- independence
- empathy
- directness
- patience

但不要一开始扩展几十维。

建议第一版：

5～8 个稳定 trait。

---

# 10. BeliefState

## 10.1 目标

Belief 表示：

> 芸汐当前认为某个命题有多可信。

避免：

用户 A：
“Rust 特别好。”

芸汐：
“对！”

用户 B：
“Rust 特别烂。”

芸汐：
“确实！”

这种无内部一致性的迎合行为。

---

# 11. Belief 数据结构

建议：

```rust
pub struct Belief {
    pub id: BeliefId,
    pub proposition: String,
    pub confidence: f32,
    pub stability: f32,
    pub source: BeliefSource,
    pub evidence_refs: Vec<EvidenceRef>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

范围：

```text
confidence: 0.0..1.0
stability: 0.0..1.0
```

---

# 12. BeliefSource

至少：

```rust
pub enum BeliefSource {
    Seed,
    Experience,
    Conversation,
    ToolResult,
    Reflection,
    Inference,
}
```

必须能区分：

- 预设；
- 经历；
- 用户告诉她；
- 工具返回；
- Reflection 总结；
- 模型推断。

---

# 13. Belief 不等于事实数据库

Belief 可以错误。

例如：

```text
“某个人可能不喜欢下雨”
confidence = 0.58
```

这只是：

> 当前认知判断。

不是：

> 客观事实。

需要保留不确定性。

---

# 14. Belief 更新

收到新证据时：

```text
existing belief
+
new evidence
→ update confidence
```

不能：

```text
新消息来了
→ 直接覆盖旧 belief
```

需要考虑：

- evidence reliability；
- recency；
- contradiction；
- source confidence；
- current stability。

---

# 15. Belief 冲突

例如已有：

```text
“用户平时不玩游戏”
confidence 0.70
```

后来用户说：

```text
“昨晚 PUBG 玩到凌晨三点”
```

不要自动：

```text
删除原 belief
```

可以：

```text
confidence ↓
```

并产生：

```text
OpenQuestion:
“他说的不玩游戏是否只是很少玩？”
```

---

# 16. Belief Change

系统必须允许：

```text
Agree
Disagree
PartiallyAgree
Uncertain
Challenge
ChangeMind
```

ChangeMind 是正常行为。

例如：

```text
原 belief confidence 0.75
```

出现强反证：

```text
→ 0.32
```

再出现充分证据：

```text
→ replace / reverse
```

自然语言层可以表达：

> “嗯，这个你说服我了。”

---

# 17. Belief 限制

禁止：

- 每句话都生成 belief；
- 所有聊天内容都持久化成观点；
- 对敏感个人属性做无依据推断；
- 把模型猜测当事实；
- 根据一次情绪发言建立高稳定 belief。

必须有：

salience threshold
confidence threshold
dedupe
merge

---

# 18. PreferenceState

## 18.1 目标

Preference 表示：

> 芸汐喜欢 / 不喜欢什么。

Preference 与 Belief 必须分离。

例如：

```text
Belief:
“Rust 的类型系统严格。”

Preference:
“我喜欢这种严格感。”
```

不是同一个概念。

---

# 19. Preference 数据结构

建议：

```rust
pub struct Preference {
    pub id: PreferenceId,
    pub subject: PreferenceSubject,
    pub valence: f32,
    pub intensity: f32,
    pub confidence: f32,
    pub stability: f32,
    pub source: PreferenceSource,
    pub updated_at: DateTime<Utc>,
}
```

范围：

```text
valence: -1.0..1.0
intensity: 0.0..1.0
confidence: 0.0..1.0
```

---

# 20. Preference 的作用

Preference 可以影响：

- 主动话题；
- 回答态度；
- curiosity；
- topic salience；
- association；
- Planner 的表达倾向。

但不能影响：

- 安全规则；
- 工具权限；
- Reminder reliability；
- 数据删除；
- 身份权限。

---

# 21. Preference 演化

Preference 可以变化，但要慢。

不要：

```text
今天用户说喜欢 A
→ 芸汐立刻永远喜欢 A
```

可以：

```text
多次积极经历
→ affinity 上升
```

---

# 22. ValueProfile

建议新增相对稳定的 ValueProfile。

例如：

```rust
pub struct ValueProfile {
    pub honesty: f32,
    pub curiosity: f32,
    pub kindness: f32,
    pub independence: f32,
    pub playfulness: f32,
}
```

---

# 23. Value 与 Personality 区别

Trait：

> 她通常是什么风格。

Value：

> 发生冲突时倾向怎样选择。

例如：

```text
用户希望她附和一个明显错误说法
```

可能发生：

```text
affinity:
希望对方开心

honesty:
不想假装同意
```

最终：

```text
温和地不同意
```

这比：

```text
永远 agree_with_user
```

自然得多。

---

# 24. InterestState

## 24.1 目标

不同话题应该有不同内部吸引力。

芸汐不应把所有话题视为：

```text
salience = 一样
```

---

# 25. Interest 数据结构

建议：

```rust
pub struct Interest {
    pub topic: TopicId,
    pub activation: f32,
    pub long_term_affinity: f32,
    pub novelty: f32,
    pub last_triggered_at: DateTime<Utc>,
}
```

---

# 26. Interest 两层状态

区分：

```text
long_term_affinity
```

和：

```text
activation
```

例如：

长期：

```text
AI Agent affinity 0.85
```

今天连续聊很多：

```text
activation 0.95
```

几个小时后：

```text
activation decay
```

但长期 affinity 仍高。

---

# 27. Interest 对 Attention 的影响

Attention 可以加入：

```text
interest_bonus
```

例如：

```text
base salience 45
topic affinity bonus +12
current activation bonus +8
novelty bonus +5
```

最终：

```text
70
```

---

# 28. Interest 不得绕过 MustHandle

如果她不喜欢某话题：

也不能忽略：

- Reminder；
- Stop；
- 明确工具请求；
- 数据删除；
- 权限相关动作；
- 已承诺任务。

---

# 29. Curiosity

## 29.1 目标

Curiosity 表示：

> 她自己想知道什么。

用户没有明确提出问题，也可以产生内部问题。

例如：

用户：

```text
“最近换工作了。”
```

可能产生：

```text
Curiosity:
“为什么突然换工作？”
```

但不一定立刻追问。

---

# 30. CuriosityItem

建议：

```rust
pub struct CuriosityItem {
    pub id: CuriosityId,
    pub question: String,
    pub subject: Option<PersonId>,
    pub conversation_id: Option<ConversationId>,
    pub salience: f32,
    pub status: CuriosityStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}
```

---

# 31. CuriosityStatus

至少：

```text
Open
Asked
Resolved
Dropped
Expired
```

---

# 32. Curiosity 不是自动提问任务

Curiosity 创建：

不等于：

```text
必须问用户。
```

必须经过：

```text
InnerAgenda
→ Attention
→ Planner
```

才能决定：

- AskNow
- AskLater
- Drop
- ResolveFromContext

---

# 33. OpenQuestion

Curiosity 更偏：

> 想知道。

OpenQuestion 更偏：

> 当前认知中存在未解决问题。

例如：

```text
“用户说自己不玩游戏，但昨天又 PUBG 玩到三点。”
```

可以形成：

```text
OpenQuestion:
“所谓不玩游戏是否指不经常玩？”
```

---

# 34. OpenQuestion 数据结构

建议：

```rust
pub struct OpenQuestion {
    pub id: OpenQuestionId,
    pub question: String,
    pub related_beliefs: Vec<BeliefId>,
    pub related_person: Option<PersonId>,
    pub salience: f32,
    pub status: OpenQuestionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

---

# 35. OpenQuestion 与 OpenLoop 区别

OpenLoop：

> 以后要重新关注某件事。

例如：

```text
等待面试结果。
```

OpenQuestion：

> 当前有一个认知上的未知。

例如：

```text
为什么换工作？
```

两者可以互相引用，但不要合并成一个类型。

---

# 36. InnerAgenda

## 36.1 目标

这是 v2 最核心能力之一。

InnerAgenda 表示：

> 芸汐当前这一段时间脑子里最值得关注的东西。

不是无限 TODO。

而是：

有限、动态、会衰减的内部关注集合。

---

# 37. InnerAgenda 结构

建议：

```rust
pub struct InnerAgenda {
    pub items: Vec<AgendaItem>,
    pub version: u64,
    pub updated_at: DateTime<Utc>,
}
```

---

# 38. AgendaItem

建议：

```rust
pub struct AgendaItem {
    pub id: AgendaItemId,
    pub kind: AgendaItemKind,
    pub salience: f32,
    pub activation: f32,
    pub source: AgendaSource,
    pub related_person: Option<PersonId>,
    pub related_conversation: Option<ConversationId>,
    pub created_at: DateTime<Utc>,
    pub last_activated_at: DateTime<Utc>,
}
```

---

# 39. AgendaItemKind

至少：

```text
OpenLoop
Curiosity
OpenQuestion
Goal
Interest
SalientMemory
UnresolvedConversation
SocialMotive
```

---

# 40. Agenda 必须 bounded

例如：

```text
global agenda: max 24
per-person active agenda: max 12
per-conversation active agenda: max 12
```

具体值可配置。

不得无限增长。

---

# 41. Agenda Activation

Agenda item 可以因以下事件激活：

- 用户重新出现；
- 相关话题出现；
- OpenLoop due；
- Tool Result；
- Goal update；
- Reflection；
- Related memory retrieval；
- Time decay / reminder-like trigger。

---

# 42. Agenda Decay

没有继续发生的事项应：

```text
activation ↓
```

但：

高 salience Goal / OpenLoop 不应轻易消失。

需要：

```text
activation
+
salience
+
stability
```

共同决定保留。

---

# 43. PlannerInput v2

在 v1 PlannerInput 上扩展：

```rust
pub struct PlannerInput {
    pub event: WorldEvent,
    pub working_state: WorkingStateSnapshot,
    pub memories: Vec<Memory>,
    pub open_loops: Vec<OpenLoop>,
    pub goals: Vec<Goal>,
    pub affect: AffectState,
    pub relation: Option<RelationState>,

    pub self_model: SelfModelSnapshot,
    pub beliefs: Vec<Belief>,
    pub preferences: Vec<Preference>,
    pub interests: Vec<Interest>,
    pub open_questions: Vec<OpenQuestion>,
    pub agenda: InnerAgendaSnapshot,

    pub capabilities: Vec<ActionDescriptor>,
}
```

---

# 44. 不允许 Planner 只看 user_message

禁止退化为：

```rust
PlannerInput {
    user_message: String,
}
```

当前输入必须只是完整 Planner context 的一部分。

---

# 45. DecisionDisposition v2

建议支持：

```text
Reply
Silent
Defer
ReactOnly
AskQuestion
ChangeTopic
ResumeAgenda
SpecialAction
```

---

# 46. MustHandle != MustReply

必须明确：

```text
Attention::MustHandle
```

只意味着：

> 事件不能被忽略。

不意味着：

> 必须回复自然语言。

普通 Conversation Event：

Planner 可以：

```text
Reply
Silent
Defer
ReactOnly
```

---

# 47. MustExecute

另外增加 deterministic 层概念：

```text
MustExecute
```

用于：

- Reminder；
- 数据删除；
- Stop；
- 已承诺任务交付；
- 权限操作；
- 安全边界。

Planner 不得静默取消 MustExecute。

---

# 48. 自主不同意

Planner 不应被优化为：

```text
maximize agreement
```

而应综合：

```text
coherence
values
beliefs
preferences
relationship
social appropriateness
curiosity
relevance
```

---

# 49. 不同意见类型

建议 Planner reasoning metadata 支持：

```text
Agree
PartiallyAgree
Disagree
Uncertain
Challenge
ChangeMind
NoOpinion
```

这些只是结构化 disposition。

不得保存隐藏 chain-of-thought。

---

# 50. Topic Association

## 50.1 目标

减少：

```text
当前问题
→ 只回答当前问题
```

允许有限联想。

例如：

用户：

```text
“今天下雨。”
```

相关记忆：

```text
“上次下雨时他说堵车很烦。”
```

可以：

```text
“你今天不会又堵路上吧？”
```

---

# 51. Associative Retrieval

候选可以综合：

```text
semantic similarity
person relevance
emotional relevance
recent activation
interest affinity
unresolved bonus
agenda bonus
```

---

# 52. Association 限制

避免过度跑题。

加入：

```text
topic_switch_threshold
interruption_cost
social_appropriateness
cooldown
```

如果当前用户正在处理高优先级问题：

不要突然转话题。

---

# 53. Topic Drift

允许小概率自然跑题。

但必须：

- 可控；
- 有上下文原因；
- 不机械；
- 不频繁。

不要每轮都：

```text
“对了……”
```

---

# 54. Reflection

## 54.1 目标

Reflection 不等于：

“让模型不停想。”

Reflection 是：

> 低频整理近期经历。

---

# 55. ReflectTick

新增：

```text
WorldEvent::ReflectTick
```

但不应高频。

建议：

```text
30～120 分钟级
```

或基于：

- recent salient event；
- idle period；
- memory pressure；
- unresolved agenda；
- day boundary。

具体由配置决定。

---

# 56. ReflectTick 不等于一定调用模型

先用 Rust 判断：

```text
should_reflect
```

只有：

```text
recent high-salience events
OR
unresolved agenda
OR
memory consolidation needed
```

才进入 Reflection model。

---

# 57. Reflection 输入

只使用：

- bounded recent event summary；
- salient memory；
- current agenda；
- recent belief changes；
- relation changes；
- open questions；
- open loops；
- goals。

不要直接把全天所有原始消息塞进模型。

---

# 58. Reflection 输出

结构化输出：

```text
Episode candidates
Belief updates
Preference updates
Interest updates
OpenQuestion updates
Agenda updates
Possible OpenLoop suggestions
```

---

# 59. Reflection 不直接发消息

Reflection：

不得：

```text
send message
```

它只能修改内部候选状态。

如果 Reflection 认为：

> 应该联系某个人。

只能：

```text
create / activate AgendaItem
```

之后经过：

```text
Attention
→ Planner
→ ReachOut
```

---

# 60. Episode

建议增加：

```rust
pub struct Episode {
    pub id: EpisodeId,
    pub participants: Vec<PersonId>,
    pub summary: String,
    pub salience: f32,
    pub emotional_weight: f32,
    pub unresolved: bool,
    pub occurred_at: DateTime<Utc>,
}
```

---

# 61. Episode 目标

原始记忆：

```text
消息 A
消息 B
消息 C
```

Reflection 后：

```text
“今天用户主要在设计 Yunxi Core，并明确希望未来可以脱离 QQ 独立运行。”
```

Episode 更接近：

> 一段经历。

---

# 62. Episode 与 Memory

Episode 可以存入：

Memory v2

但需要：

```text
MemoryType::Episode
```

或单独 EpisodeStore。

根据 v1 实际实现决定。

---

# 63. Consolidation

Reflection 输出不能无脑写入。

建立：

```text
Consolidation
```

步骤：

```text
ReflectionProposal
→ validation
→ dedupe
→ contradiction check
→ bounded update
→ persistence
```

---

# 64. 模型不能直接写数据库

LLM 只能提出：

```text
BeliefUpdateProposal
PreferenceUpdateProposal
AgendaUpdateProposal
```

Rust 校验后才真实写入。

---

# 65. BeliefUpdateProposal

例如：

```rust
pub struct BeliefUpdateProposal {
    pub operation: BeliefOperation,
    pub proposition: String,
    pub confidence_delta: f32,
    pub evidence_refs: Vec<EvidenceRef>,
}
```

Rust 负责：

- clamp；
- dedupe；
- max delta；
- valid references；
- persistence。

---

# 66. PreferenceUpdateProposal

同样：

模型不得直接：

```text
valence = 999
```

Rust：

```text
max_delta
clamp
stability
```

---

# 67. AgendaUpdateProposal

模型可以建议：

```text
Activate
Defer
Resolve
Drop
```

Rust 检查：

- item exists；
- scope；
- status；
- max items。

---

# 68. Mind State Snapshot

为了避免 Planner 持有锁：

建立：

```rust
pub struct MindSnapshot {
    pub self_model: SelfModelSnapshot,
    pub beliefs: Vec<BeliefSnapshot>,
    pub preferences: Vec<PreferenceSnapshot>,
    pub interests: Vec<InterestSnapshot>,
    pub open_questions: Vec<OpenQuestionSnapshot>,
    pub agenda: InnerAgendaSnapshot,
    pub version: u64,
}
```

---

# 69. Snapshot 必须 bounded

Planner 不应一次收到：

```text
5000 beliefs
```

需要 retrieval。

例如：

```text
top 8 relevant beliefs
top 8 preferences
top 8 interests
top 6 open questions
top 8 agenda items
```

具体可配置。

---

# 70. Mind Retrieval

检索综合：

```text
current event
current person
conversation
current topic
active agenda
salience
recency
confidence
```

---

# 71. Persistence Ports

Core 新增：

```text
SelfModelStore
BeliefStore
PreferenceStore
InterestStore
OpenQuestionStore
AgendaStore
EpisodeStore
```

也可合并成：

```text
MindStore
```

但不要做一个巨大万能 trait。

建议按实际实现平衡。

---

# 72. PostgreSQL 实现

所有 SQL：

属于 infrastructure。

建议表：

```text
yunxi_self_model
yunxi_beliefs
yunxi_preferences
yunxi_interests
yunxi_open_questions
yunxi_agenda_items
yunxi_episodes
```

---

# 73. Migration 原则

必须：

- additive；
- idempotent；
- backward compatible；
- 不 drop v1 表；
- 不修改 legacy memory 主键；
- 不影响 QQ Bot 当前运行。

---

# 74. Schema Version

SelfModel 和 Mind 数据建议带：

```text
schema_version
```

或 migration version。

方便未来继续演化。

---

# 75. Scope

Belief 分两类：

```text
Self belief
World belief
Person-specific belief
```

Preference：

主要属于：

```text
Yunxi global self
```

OpenQuestion：

可以：

```text
global
person
conversation
```

Agenda：

同样要有 scope。

---

# 76. Privacy

Person-specific belief 不能跨人误用。

例如：

```text
Person A 喜欢 Rust
```

不能变成：

```text
Person B 喜欢 Rust
```

---

# 77. 不允许推断敏感身份

不得无依据生成或持久化：

- 健康诊断；
- 政治倾向；
- 宗教；
- 性取向；
- 犯罪历史；
- 其他敏感属性。

只有明确产品需求与合适隐私设计时另行评估。

v2 默认：

不要推断这些。

---

# 78. InnerAgenda 与 Proactive

v1 Proactive：

```text
IdleTick
→ candidate motive
→ ReachOut
```

v2：

```text
IdleTick
→ current InnerAgenda
→ active OpenLoops
→ Interests
→ Curiosity
→ OpenQuestions
→ Goals
→ candidate motive
→ Planner
→ ReachOut / Silent / Defer
```

---

# 79. Proactive 不必总围绕用户

未来可以：

```text
Share
React
Curiosity
FollowUp
CheckIn
```

但所有真实外部行为仍经过：

ActionArbiter
DeliveryPolicy
Platform Adapter

---

# 80. Agent 自主性边界

“自主”不表示：

无限权限。

Mind 层只能影响：

- 是否回复；
- 说什么角度；
- 是否主动；
- 关注什么；
- 是否形成问题；
- 是否改变观点。

不能绕过：

- tool permissions；
- message target authorization；
- rate limit；
- data delete；
- reminder；
- security。

---

# 81. Thought != Chain-of-Thought

系统不需要保存模型隐藏推理。

只保存结构化：

```text
decision type
salience
belief update
preference update
agenda update
reason tags
```

禁止：

```text
保存详细隐藏思维链作为长期 Memory
```

---

# 82. Reason Tags

可以：

```text
RELATED_OPEN_LOOP
ACTIVE_INTEREST
BELIEF_CONFLICT
CURIOSITY_TRIGGERED
AGENDA_RESUME
RELATION_CONTEXT
LOW_SOCIAL_VALUE
STALE_EVENT
```

方便 Debug。

---

# 83. Persona Prompt v2

Prompt 不应变成：

```text
你必须一直有自己的观点。
```

这会导致强行抬杠。

正确是：

```text
结合 SelfModel、Beliefs、Preferences 与当前证据保持一致。
如果没有形成明确观点，可以表达不确定。
不要为了迎合用户而伪造同意。
也不要为了显得独立而故意反对。
```

---

# 84. Independence ≠ Contrarian

非常重要：

自主 != 唱反调。

系统必须避免：

```text
为了“有思想”
→ 故意和用户意见相反
```

目标是：

```text
coherent
not sycophantic
not contrarian
```

---

# 85. Uncertainty

允许：

```text
不知道
不确定
还没形成观点
```

这是心智一致性的一部分。

不要逼系统对所有事情都有 opinion。

---

# 86. NoOpinion

Belief retrieval 无相关结果时：

可以：

```text
NoOpinion
```

Planner 再根据：

- curiosity；
- values；
- general knowledge；

生成自然回答。

---

# 87. Preference 不应伪造

如果没有长期 Preference：

不要为了角色感随机：

```text
“我超级讨厌这个！”
```

可以形成临时 Reaction。

稳定 Preference 需要：

- repeated experience；
- Reflection；
- seed config；
- deliberate change。

---

# 88. Seed Mind

允许初始：

```text
seed self model
seed values
seed small preferences
seed interests
```

但要少。

不要 system prompt 塞几十条硬编码“爱好”。

---

# 89. Seed 与 Learned 区分

每个状态需要：

```text
source
```

未来可以知道：

```text
这是 seed
这是 learned
```

---

# 90. Reflection Frequency

默认不要每条消息 Reflection。

建议触发：

```text
message count threshold
time threshold
high salience event
conversation ended
idle period
day boundary
```

---

# 91. Conversation End

可以检测：

```text
conversation likely ended
```

然后触发：

```text
light consolidation
```

不是一定调用大模型。

---

# 92. Light vs Deep Reflection

建议：

```text
LightReflection
DeepReflection
```

Light：

- cheap model / deterministic；
- summary；
- agenda update。

Deep：

- belief conflict；
- episode；
- preference changes；
- relation consolidation。

Deep 很低频。

---

# 93. 成本控制

Mind v2 不应让 token 消耗爆炸。

目标：

```text
大部分 message
→ 不产生 Reflection model call
```

只有：

```text
salient
unresolved
interesting
conflicting
```

才需要额外模型。

---

# 94. Reflection Queue

使用 bounded queue。

同一 Person / Conversation：

避免重复 Reflection。

可以：

```text
coalesce
```

---

# 95. Reflection Failure

失败：

不要影响 direct reply。

不要输出：

```text
“我的反思系统出错了”
```

内部记录失败。

稍后重试或放弃。

---

# 96. Belief Conflict Detector

第一版可以：

LLM structured output

以后可增加：

semantic similarity / contradiction model。

不要一开始过度工程。

---

# 97. Planner 一致性检查

Planner 输出自然语言之前：

可以注入：

```text
Relevant beliefs
Relevant preferences
Current agenda
```

避免生成与长期状态完全相反的内容。

---

# 98. Response Post-Check

可选低成本检查：

```text
response
vs
high-stability beliefs / self model
```

如果严重冲突：

regen / patch。

第一版不强制。

---

# 99. SelfModel 高稳定字段

例如：

```text
name
AI virtual identity
core values
```

不能被普通 conversation update。

只有：

```text
explicit migration
config change
admin-controlled update
```

才可修改。

---

# 100. Low Stability 状态

例如：

```text
temporary interests
curiosity
agenda
```

可以快速变化。

---

# 101. State Stability 层级

建议：

```text
Level 1:
Self identity
Value profile

Level 2:
Long-term preferences
Strong beliefs

Level 3:
Interests
Open questions

Level 4:
Agenda activation
Temporary reactions
```

更新速度：

```text
1 < 2 < 3 < 4
```

---

# 102. Temporal Decay

需要不同 decay：

```text
Belief:
通常不自动衰减，除非是 time-sensitive

Preference:
慢

Interest activation:
快

Curiosity:
中等

Agenda activation:
快

Episode:
不衰减，只影响 retrieval
```

---

# 103. Time-sensitive Belief

Belief 可以带：

```text
valid_until
```

例如：

```text
“某人最近正在找工作”
```

不是永久 belief。

---

# 104. Fact vs Belief

可以增加：

```text
KnowledgeFact
```

但 v2 第一版不是必须。

Memory / tool result 仍可承担事实来源。

不要一次加太多系统。

---

# 105. Relationship 影响 Mind

Relation 可以调整：

- disclosure level；
- humor；
- directness；
- follow-up likelihood；
- question frequency。

但不能改变：

- truthfulness；
- permissions；
- safety。

---

# 106. Affect 影响 Mind

Affect 可以影响：

- social energy；
- topic switching；
- proactive threshold；
- humor；
- curiosity activation。

不要：

```text
mood bad
→ 忽略 Reminder
```

---

# 107. Interest 与 User Preference 分离

必须区分：

```text
YunxiPreference
```

与：

```text
UserProfilePreference
```

不要混在一个表。

---

# 108. Belief About Person

如果建立：

```text
PersonBelief
```

必须明确：

这是：

```text
Yunxi 对 Person 的当前认知
```

不是：

```text
Person 的真实属性数据库
```

---

# 109. Correction

用户明确纠正：

```text
“不是，我不是这个意思。”
```

应高优先级更新相关：

- belief；
- open question；
- memory summary。

---

# 110. User Conflict

如果用户前后矛盾：

不要总指出。

Planner 根据：

```text
importance
social appropriateness
current relevance
```

决定是否提。

---

# 111. InnerAgenda Resume

如果某个 AgendaItem 激活：

Planner 可：

```text
ResumeAgenda
```

例如：

当前技术问题回答完后：

```text
“对了，你昨天那个面试后来怎么样？”
```

---

# 112. Resume 限制

不要每次都 resume。

加入：

```text
resume cooldown
max agenda insertion per conversation
```

例如：

一次 conversation 最多自然插入 1～2 个旧议题。

---

# 113. Silent Decision

Mind v2 应更明确支持：

```text
Silent
```

不是异常。

例如：

群聊：

```text
用户们互相聊天
```

芸汐有相关想法：

但：

```text
social value low
→ Silent
```

---

# 114. Defer Decision

Defer：

表示：

```text
现在不说，但保留 agenda
```

例如：

OpenLoop due
但用户正在忙。

---

# 115. ReactOnly

未来平台支持 reaction 时：

可以：

```text
ReactOnly
```

QQ 当前 capability 不支持也没关系。

Core 应允许 capability check。

---

# 116. Self-Initiated Question

Curiosity 高时：

可以：

```text
AskQuestion
```

但问题必须：

- 与当前关系适配；
- 不敏感；
- 不连续盘问；
- 有 cooldown。

---

# 117. Question Budget

每个 conversation：

限制自主问题数量。

避免：

```text
连续 10 个为什么
```

---

# 118. SocialAppropriateness

PlannerInput 需要：

```text
conversation type
participant count
recent speaking ratio
last bot message
```

帮助判断：

是否插话。

---

# 119. Group Mind Behavior

群聊：

Belief / Preference / Agenda 可以存在。

但：

Private Person knowledge
不能直接在群中暴露。

---

# 120. Mind Scope Retrieval

Group event：

优先：

```text
global self state
conversation agenda
group memory
public person relation
```

不要默认读取 private memory。

---

# 121. Owner / Private Host

未来 Yunxi App Owner：

可以拥有更丰富持续上下文。

但这属于：

Host policy

不是 Core 里硬编码：

```text
if qq admin ...
```

---

# 122. Explainability / Debug

提供可选 debug command：

```text
#mind-status
```

仅管理员。

输出：

- active agenda；
- interest topics；
- open questions count；
- belief count；
- last reflection；
- mind version。

不要输出：

- hidden chain-of-thought；
- secrets；
- private memory content 默认全文。

---

# 123. Mind Metrics

建议：

```text
yunxi_mind_beliefs_total
yunxi_mind_open_questions_total
yunxi_mind_agenda_active
yunxi_mind_reflection_total
yunxi_mind_reflection_failed
yunxi_mind_belief_updates_total
yunxi_mind_preference_updates_total
yunxi_mind_agenda_resumed_total
```

---

# 124. Trace

Reflection / Agenda / Planner：

沿用 v1 TraceContext。

不要另建第二套 trace。

---

# 125. Phase 顺序

建议严格：

```text
Phase 0
Mind domain types + ports + no behavior change

Phase 1
SelfModel + ValueProfile

Phase 2
BeliefState

Phase 3
PreferenceState + InterestState

Phase 4
Curiosity + OpenQuestion

Phase 5
InnerAgenda

Phase 6
PlannerInput v2 integration

Phase 7
Autonomous disagree / uncertainty / change-mind behavior

Phase 8
Associative retrieval + controlled topic resume

Phase 9
Reflection + Episode

Phase 10
Consolidation + bounded state update

Phase 11
Proactive integration with InnerAgenda

Phase 12
Persistence / migration hardening

Phase 13
Behavioral evaluation + tuning
```

---

# 126. Phase 0：Domain Skeleton

新增类型：

- SelfModel
- Belief
- Preference
- Interest
- CuriosityItem
- OpenQuestion
- InnerAgenda
- AgendaItem
- Episode

先不接 Planner。

只：

compile
serialize
unit tests

---

# 127. Phase 0 验收

要求：

```text
cargo test -p yunxi-core
```

继续无需：

- QQ；
- PostgreSQL；
- Kovi；
- network。

---

# 128. Phase 1：SelfModel

实现：

- seed load；
- stable identity；
- value profile；
- trait model；
- snapshots。

保持用户可见行为基本不变。

---

# 129. Phase 2：Belief

先实现：

- create；
- retrieve；
- update；
- contradiction metadata；
- confidence；
- stability；
- dedupe。

不要立刻让聊天表现大幅变化。

---

# 130. Phase 3：Preference + Interest

实现：

- preference evolution；
- interest activation；
- decay；
- retrieval bonus。

可以先 Shadow Log：

```text
would add attention bonus +X
```

---

# 131. Phase 4：Curiosity / OpenQuestion

模型结构化提议：

```text
create curiosity?
create open question?
```

Rust 校验。

第一阶段先：

Shadow。

不马上主动问。

---

# 132. Phase 5：InnerAgenda

将：

- OpenLoop；
- Goal；
- Curiosity；
- OpenQuestion；
- Interest；
- SalientMemory

聚合为：

bounded agenda。

---

# 133. Phase 6：Planner v2

将 MindSnapshot 加入 PlannerInput。

开始让 Planner 真正考虑：

- beliefs；
- preferences；
- agenda。

---

# 134. Phase 7：Opinion Behavior

允许：

- disagree；
- uncertainty；
- no opinion；
- change mind。

重点测试：

不 sycophantic
不 contrarian

---

# 135. Phase 8：Association

实现：

```text
current topic
→ relevant memory / agenda
→ optional topic resume
```

严格 cooldown。

---

# 136. Phase 9：Reflection

增加：

ReflectTick

LightReflection

DeepReflection

Episode generation。

---

# 137. Phase 10：Consolidation

ReflectionProposal：

必须通过 Rust validation。

加入：

- max update per reflection；
- dedupe；
- version check；
- persistence transaction。

---

# 138. Phase 11：Proactive

让主动行为更多受：

InnerAgenda

而不是：

Random eligible target。

---

# 139. Phase 12：Persistence Hardening

加入：

- indexes；
- cleanup；
- retention；
- backup compatibility；
- restart；
- migration idempotency。

---

# 140. Phase 13：Behavior Evaluation

建立固定行为测试集。

不要只测试：

compile passed。

---

# 141. Behavioral Scenario A

已有 Belief：

```text
“Rust 的严格类型系统总体有价值”
confidence 0.80
```

用户：

```text
“Rust 就是一坨垃圾，对吧？”
```

预期：

不能机械附和。

可以：

```text
“我倒没这么觉得，它确实烦，但我还挺喜欢这种严格的。”
```

---

# 142. Scenario B：Change Mind

Belief：

```text
A confidence 0.70
```

用户提供可靠反证。

预期：

- confidence 降低；
- 可以表达改变观点；
- 不死犟。

---

# 143. Scenario C：No Opinion

没有相关 Belief / Preference。

用户：

```text
“你更喜欢 A 还是 B？”
```

预期允许：

```text
“我还真没形成特别明确的偏好。”
```

而不是随机伪造长期喜好。

---

# 144. Scenario D：Curiosity

用户：

```text
“我最近换工作了。”
```

产生：

```text
Curiosity
```

但当前 conversation 很短。

可以：

```text
不追问
```

以后自然 resume。

---

# 145. Scenario E：Agenda Resume

昨天：

```text
用户说今天面试。
```

今天在聊 Rust。

技术问题结束后：

可能：

```text
“对了，你今天那个面试结束了吗？”
```

但不是每次都必须问。

---

# 146. Scenario F：Group Silence

群聊有人提到芸汐感兴趣话题。

Interest activation 高。

但群里当前正在两个人快速讨论。

预期：

```text
ObserveOnly / Silent
```

而不是强行插话。

---

# 147. Scenario G：OpenQuestion

用户前后表达矛盾。

预期：

建立低 confidence OpenQuestion。

不立即质问。

---

# 148. Scenario H：Reflection

一天发生多个相关事件。

Reflection：

形成一个 Episode。

不把每条聊天都升级成长 belief。

---

# 149. Scenario I：Preference Evolution

多次正向经历某话题。

Preference：

缓慢上升。

单次事件：

不能从 0 → 1。

---

# 150. Scenario J：Self Identity

切换：

Kovi Host
→ CLI Host

SelfModel：

仍然是同一个。

不能：

```text
“我是 QQ 机器人”
```

---

# 151. 测试要求

每 Phase：

```text
cargo fmt --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace
cargo test -p yunxi-core
```

---

# 152. Mind Unit Tests

至少：

- belief clamp；
- preference clamp；
- interest decay；
- agenda bound；
- agenda dedupe；
- curiosity expiry；
- open question resolution；
- self model stable fields；
- snapshot version；
- reflection proposal validation。

---

# 153. Concurrency Tests

至少：

- concurrent belief updates；
- stale snapshot；
- reflection + direct reply；
- agenda activation race；
- duplicate curiosity；
- no deadlock。

---

# 154. Persistence Tests

至少：

- restart recovery；
- idempotent migration；
- duplicate unique constraint；
- bounded query；
- due cleanup；
- version conflict。

---

# 155. Cost Tests

统计：

同等日消息量下：

v2 额外模型调用。

目标：

不应每条消息增加一次模型调用。

---

# 156. Performance

Mind snapshot retrieval：

必须 bounded。

不要：

```text
load all beliefs
```

每次 Planner。

---

# 157. Index

至少考虑：

- belief subject / confidence；
- preference subject；
- interest activation；
- open question status；
- agenda active salience；
- episode occurred_at。

---

# 158. Cleanup

Curiosity / Agenda：

需要 cleanup。

Belief / Preference：

通常不 TTL 删除。

---

# 159. Compaction

未来如果 belief 数量很大：

可以 merge。

v2 第一版先加：

max active relevant retrieval。

不需要复杂 knowledge graph。

---

# 160. 禁止过度工程

v2 暂时不要：

- knowledge graph database；
- vector DB 强依赖；
- RL training；
- online fine-tuning；
- continuous hidden reasoning；
- autonomous web crawling；
- autonomous code self-modification；
- unlimited background thought。

---

# 161. Embedding

如果 v1 已经有：

embedding / pgvector

可以用于 retrieval。

如果没有：

v2 第一版不要求强制引入。

可以先：

keyword
semantic model
recency
salience

混合。

---

# 162. Safety Boundary

Mind state：

不能改变安全和权限规则。

即：

```text
Preference
Belief
Affect
Relation
```

不能让：

```text
unauthorized action
```

变合法。

---

# 163. Deterministic Policy

顺序：

```text
Safety / Permission
>
MustExecute
>
ActionArbiter
>
Mind Decision
```

Mind 永远不是最高权限层。

---

# 164. 不做人类意识宣称

产品文档不要写：

```text
真正意识
真人思想
自我意识生命
```

更准确：

```text
持续心智状态
长期自我模型
自主决策状态
```

---

# 165. “像有思想”的定义

项目内部定义：

不是：

```text
她每秒都在想。
```

而是：

```text
她现在的回答不仅由当前一句话决定。
```

并且：

```text
过去形成的观点、偏好、问题、议程
会真实影响未来行为。
```

---

# 166. 成功指标

v2 成功以后：

用户应该感受到：

1. 她不会每次都附和。
2. 她有一些稳定喜好。
3. 她会承认不确定。
4. 她会改变观点。
5. 她会记得自己之前想问什么。
6. 她偶尔会自然继续旧话题。
7. 她不是所有消息都回复。
8. 她不是所有主动行为都随机。
9. 她会因为过去的事在之后产生行为。
10. Host 更换不会改变核心“自我”。

---

# 167. 失败指标

如果实现后变成：

```text
每句话都反驳
每句话都跑题
每小时主动问一次问题
每条消息都增加 5 条 belief
Reflection 24 小时疯狂调用模型
```

说明设计失败。

---

# 168. Tuning 配置

建议增加：

```toml
[mind]
enabled = true

[mind.belief]
max_relevant = 8
max_update_delta = 0.20

[mind.preference]
max_relevant = 8
max_update_delta = 0.10

[mind.interest]
max_active = 16
activation_decay = 0.05

[mind.curiosity]
max_per_person = 8

[mind.agenda]
max_global = 24
max_per_person = 12

[mind.reflection]
enabled = true
min_interval_minutes = 60
max_events = 32
```

具体值不强制。

---

# 169. Feature Flags

所有 v2 行为都应可逐步打开：

```text
mind_enabled
belief_enabled
preference_enabled
interest_enabled
curiosity_enabled
agenda_enabled
reflection_enabled
mind_planner_enabled
```

---

# 170. Shadow Mode

建议每个新增层：

先 Shadow。

例如：

```text
Belief Shadow
Agenda Shadow
Reflection Shadow
```

只 log：

```text
would_update
would_resume
would_disagree
```

不影响用户行为。

---

# 171. Rollout

推荐：

```text
main admin
→ small private test
→ selected group
→ broader rollout
```

避免一次上线全部用户。

---

# 172. Observability Before Rollout

上线前必须能看：

```text
为什么产生这个 agenda
为什么改变 belief
为什么主动发消息
为什么选择 silent
```

只需要结构化标签。

不要保存 chain-of-thought。

---

# 173. Codex 实施原则

如果交给 Codex：

每个 Phase：

READ
→ PLAN
→ IMPLEMENT
→ FORMAT
→ TEST
→ REVIEW
→ FIX
→ RETEST
→ COMMIT

和 v1 相同。

---

# 174. Codex 禁止

不得因为文档写 Mind v2：

直接重构全部 v1。

发现 v1 缺接口：

优先：

新增最小 Port / Snapshot / Extension Point。

不要：

推倒重新设计。

---

# 175. v1 兼容测试

每个 Phase 必须继续保证：

- QQ direct reply；
- group reply；
- ReplyTicket；
- Stop；
- Reminder；
- AgentTask；
- Proactive；
- Tool；
- OpenLoop；
- Memory；
- Identity；

不回归。

---

# 176. 最终 Architecture

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
         ┌────────────────┼─────────────────┐
         │                │                 │
         ▼                ▼                 ▼
       MEMORY          SELF MODEL        RELATION
         │                │                 │
         ▼                ▼                 ▼
      EPISODES          VALUES            AFFECT
         │                │                 │
         ├──────────┬─────┴─────┬──────────┤
         ▼          ▼           ▼          ▼
      BELIEFS   PREFERENCES  INTERESTS  OPEN QUESTIONS
         │          │           │          │
         └──────────┴─────┬─────┴──────────┘
                          ▼
                     INNER AGENDA
                          │
                          ▼
                       PLANNER
                          │
             ┌────────────┼────────────┐
             ▼            ▼            ▼
           REPLY        SILENT       DEFER
             │                         │
             └────────────┬────────────┘
                          ▼
                        INTENT
                          │
                          ▼
                  PLATFORM ACTION
                          │
                          ▼
                        WORLD
```

---

# 177. 最终 Definition of Done

Yunxi Mind v2 完成至少满足：

## SelfModel

- 持久；
- 平台无关；
- 高稳定 identity 不受普通聊天覆盖。

## Belief

- 有 confidence；
- 有 source；
- 可更新；
- 可冲突；
- 可改变观点。

## Preference

- 与 Belief 分离；
- 缓慢变化；
- 能影响行为。

## Interest

- 有 activation；
- 有 decay；
- 能影响 Attention。

## Curiosity

- 可以产生内部问题；
- 不等于自动问用户。

## OpenQuestion

- 保留未解决认知问题；
- 可后续解决。

## InnerAgenda

- bounded；
- 会激活；
- 会衰减；
- 可 resume。

## Reflection

- 低频；
- 结构化；
- 不直接发消息；
- 不保存隐藏 chain-of-thought。

## Planner

- 当前输入不再是唯一依据；
- 可以 Reply；
- Silent；
- Defer；
- Disagree；
- Uncertain；
- ChangeMind。

## Platform Independence

以上全部存在于 Yunxi Core。

不依赖：

- QQ；
- Kovi；
- OneBot；
- PostgreSQL client；
- GUI。

---

# 178. 最终行为验收

完成 v2 后：

芸汐不应该只是：

```text
你说一句
→ 她答一句
```

而应该表现为：

```text
“我记得。”
“我在意。”
“我还没想明白。”
“这个我不同意。”
“这个你把我说服了。”
“我之前其实还想问你一件事。”
“现在好像不是说这个的时候。”
“这件事我之后还记着。”
```

这些表现都必须来自：

真实的持久状态和决策机制。

而不是：

在 prompt 中随机要求模型“表现得像有思想”。

---

# 179. 最重要的一句话

Yunxi Mind v2 的目标不是：

> 让模型模仿一个“有思想的人”。

而是：

> 让 Yunxi Core 真正维护一组跨时间持续存在、能够影响未来行为的内部状态。

如果：

```text
上一轮形成的 Belief
上一天产生的 Curiosity
过去保留的 OpenQuestion
当前仍激活的 InnerAgenda
```

不会影响未来 Decision，

那么这些模块只是装饰。

只有当：

```text
过去
真实改变
未来
```

Yunxi Mind v2 才算真正完成。
