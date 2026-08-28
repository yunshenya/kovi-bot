# Yunxi Executive v3：执行控制、元认知与内生认知模型开发文档

**文档状态：** 3.1 实施就绪优化版  

**文档版本：** 3.1  
**适用项目：** Yunxi Core / Yunxi Mind / Yunxi Executive / Yunxi Intrinsic Model  
**主要语言：** Rust  
**代码审计基线：** `yunshenya/kovi-bot` `main@5fb787129bb369f04c5ef4a30defa4fdab775d2e`（2026-08-27 审阅时）  
**前置依赖：** Yunxi Core v1、Yunxi Mind v2 已落地；实现时以最新 `main` 为唯一代码事实源  
**模型参考：** `jingyaogong/minimind-o`，第一代 Intrinsic Model 参考 `minimind-3o` 约 0.1B 架构，V3 仅采用文字 + 视觉路径  
**文档定位：** 在不推翻 v1/v2 的前提下，为 Yunxi 增加 Executive Control，并正式引入一个与 `mind/` 同级的 `model/` 模块，使 Yunxi 在没有任何外部大模型、外部服务故障或预算不足时仍保留基础认知能力；同时严格限制 V3 只实现 Intrinsic Model 基线、最小认知等级选择与有界降级链，不把完整多供应商模型平台或自训练系统一并塞入本版本。

> 本文档中的类型名、字段名与接线方案优先对齐上述审计基线。开始每个 Phase 前必须重新读取当前 `main`，若代码已演进，则以代码为准做最小增量适配，禁止为了让代码迁就旧文档而回退现有能力。

---

# 1. 文档目标

Yunxi Core v1 解决：

> 系统如何持续存在、观察世界、产生行为、接收行为结果。

Yunxi Mind v2 解决：

> 系统长期保留哪些心智内容，例如 SelfModel、Beliefs、Preferences、Interests、OpenQuestions、InnerAgenda、Episode 与 Reflection。

Yunxi Executive v3 进一步解决两个互相关联的问题：

1. 当内部存在多个目标、多个观点、多个关注点、多个候选行为与不同置信度时，系统如何决定“现在应该优先处理什么、相信到什么程度、是否需要改变计划、是否值得继续思考”。
2. 当外部强模型可用性、预算或能力发生变化时，系统如何选择合适认知等级，并在强模型失效时降级到 Yunxi 自带的 Intrinsic Model，而不是让整套 Agent 失去基本可用性。

v3 不新增一个“第二个聊天脑”，也不把 MiniMind 等同于 Yunxi 本体。

v3 的核心是：

```text
Conflict Detection
Priority Control
Confidence Calibration
Plan Revision
Attention / Cognitive Budgeting
Expectation Tracking
Decision Comparison
Reflection Scheduling
Self-Consistency Monitoring
Cognitive Tier Selection
Intrinsic Model Availability
Bounded Fallback / Degradation
```

完成 v3 后必须满足一个新的产品级不变量：

```text
No external model configured
!=
Yunxi unavailable
```

外部强模型是高级认知增强；Intrinsic Model 是 Yunxi 产品运行时自带的最低生成式认知能力；Rust deterministic path 是最终 Reflex 层。

---

# 2. Core / Mind / Model / Executive 关系

本版不再使用容易误解的“严格三层”描述。

`yunxi-core` 是平台无关的认知运行时 crate；其中逐步形成三个并列的核心认知域：

```text
crates/yunxi-core/src/
├── mind/          # v2：长期心智内容
├── model/         # v3：内生认知能力与最低模型运行时
├── executive/     # v3：执行控制、预算、冲突、计划与认知等级选择
├── planner.rs
├── runtime.rs
└── ...            # v1 既有能力
```

概念关系：

```text
                         YUNXI CORE RUNTIME
                                │
             ┌──────────────────┼──────────────────┐
             │                  │                  │
             ▼                  ▼                  ▼
         YUNXI MIND         YUNXI MODEL       YUNXI EXECUTIVE
     “我长期保留什么”     “我现在能用什么思考”   “现在该怎么取舍”
             │                  │                  │
             └──────────────┬───┴──────────────┬──┘
                            ▼                  ▼
                         PLANNER          DETERMINISTIC POLICY
                            │                  │
                            └──────────┬───────┘
                                       ▼
                                  ACTION ARBITER
                                       │
                                       ▼
                                    ADAPTERS
```

职责必须分离：

- **Mind** 持久化“内容”，不拥有模型进程，也不直接执行 Action。
- **Model** 提供计算能力，不拥有身份、关系、Belief、Goal 或长期人格。
- **Executive** 决定是否值得调用哪一级认知能力，但不变成另一个自然语言生成器。
- **Planner** 消费有界 Snapshot，产生声明式计划。
- **ActionArbiter / Rust hard policy** 继续拥有权限、安全、MustExecute 与副作用边界。

因此：

```text
Yunxi != MiniMind
Yunxi != GPT-5.x
Yunxi != 任意单个权重
```

模型可以变化，Mind 与 Core 连续性不能因此丢失。

---

# 3. v1 / v2 / v3 职责边界

## 3.1 Yunxi Core v1

负责现有、已经落地的基础域：

- WorldEvent / Event Bus；
- Attention 基础入口；
- WorkingState；
- Identity；
- Memory 接口；
- OpenLoop；
- Goal 基础模型；
- Intent；
- Action / ActionResult；
- Platform-neutral Attachment reference；
- ActionArbiter 基础权限边界；
- Runtime data-erasure FIFO barrier；
- Planner / `ModelBackend` 现有抽象。

Core 回答：

> “发生了什么，我能做什么？”

## 3.2 Yunxi Mind v2

当前 `main` 已经真实存在 `crates/yunxi-core/src/mind/`，并实现 SelfModel、Belief、Preference、Interest、Curiosity、OpenQuestion、InnerAgenda、Episode、Consolidation、Reflection、MindSnapshotProvider、MindServices 与 MindDataErasure 等边界。

Mind 回答：

> “我是谁，我相信什么，我喜欢什么，我在意什么，还有什么没想明白？”

V3 必须复用这些类型，不得创建第二套 Belief / Reflection / MindSnapshot。

## 3.3 Yunxi Model v3

新增 `crates/yunxi-core/src/model/`，负责最小但真实的内生认知能力：

- Intrinsic Model 生命周期；
- 模型资产 manifest / version / hash；
- 文字推理；
- 视觉推理；
- 最小 ModelHealth；
- CognitiveTier；
- Strong → Intrinsic → Reflex 的有界降级；
- Intrinsic 资源预算与并发限制；
- host-supplied media resolution port；
- 模型版本与未来 Adapter 版本标识。

Model 回答：

> “我当前拥有哪些认知计算能力，它们是否健康？”

V3 的 `model/` 只负责 Yunxi 的内生最低认知能力与最小降级链。V3 不实现完整多供应商 Registry、Privacy Router、成本路由、Prompt Registry、Embedding/Reranker 生态、A/B 与任意 Backend 编排。

## 3.4 Yunxi Executive v3

负责：

- ConflictMonitor；
- ConfidenceCalibration；
- GoalArbitration；
- AttentionBudget / CognitiveBudget；
- PlanState / PlanRevision；
- ExpectationState；
- CandidateEvaluation；
- ReflectionController；
- SelfConsistencyMonitor；
- DecisionRecord；
- CognitiveTier selection；
- ExecutiveSnapshot。

Executive 回答：

> “这些东西现在应该怎么取舍，以及值得动用哪一级认知能力？”

## 3.5 V3 的硬边界

V3 只做：

```text
Intrinsic Model baseline
+ minimal strong/intrinsic fallback
+ tier-aware Executive
```

以下能力明确不属于 V3：

```text
general multi-provider model fabric
role-based provider routing
privacy / cost / latency routing policy
full registry / circuit breaker
prompt / context infrastructure
embedding / reranking platform
training candidate pipeline
LoRA / Adapter training
distillation
model promotion / canary / rollback governance
```

这些能力可以在独立工程阶段实现，但不得为了接入 Intrinsic Model 而一次性塞入 V3。

---

# 4. 最高级原则

## 4.1 不推翻 v1/v2

不得为了实现 Executive 或 Intrinsic Model：

- 重写 WorldEvent / Event Bus；
- 重写 PersonId / ConversationId；
- 重写 Kovi Adapter；
- 重写 Mind 模块；
- 重写 Memory / OpenLoop / Relation / Affect；
- 重写 ReplyTicket / ConversationCoordinator；
- 重写 agent_tasks / Reminder / Tool Runtime；
- 删除已经落地的数据删除 barrier、Mind outgoing fence 或 InteractionCues 语义链。

如果缺接口，优先增加：

```text
Port
Snapshot
Proposal
Adapter
Extension Point
Compatibility Re-export
```

而不是整体重构。

## 4.2 Zero External Model 是合法运行模式

必须支持：

```text
No API key
No remote provider
No llama.cpp external server
→ Intrinsic tier remains available
```

如果连 Intrinsic Model 也不可用：

```text
→ Reflex tier
```

Runtime、Memory、Reminder、Stop、Data Erasure、Action idempotency 等 deterministic 能力仍然存活。

## 4.3 正常 direct message 禁止默认“双模型串行”

Strong model 健康时，不允许把每条普通 direct message 变成：

```text
Intrinsic pre-pass
→ Strong model
```

否则会直接违反现有 V2/V3 的延迟与成本原则。

默认应是：

```text
Executive deterministic gate
→ choose one tier
→ one planner/model call
```

只有以下情况才允许同一 turn 发生第二次模型调用：

- 已选 Strong backend 明确失败并触发 bounded Intrinsic fallback；
- 高价值灰区且现有 V3 规则明确允许额外 evaluator；
- 非 direct foreground 的低频 background work。

## 4.4 Intrinsic Model 不是权限系统

下列事项继续由 Rust 确定性逻辑拥有最终权力：

- Stop；
- data deletion；
- Reminder；
- permission / security；
- ActionArbiter；
- idempotency；
- committed delivery；
- destructive / high-consequence action。

0.1B 模型不得直接放宽权限，也不得绕过 ActionArbiter。

## 4.5 V3 不做在线自训练

严禁：

```text
message
→ response
→ 立即反向传播修改 Intrinsic weights
```

原因包括自我污染、恶意数据投毒、灾难性遗忘与不可回滚行为漂移。

V3 只留下：

```text
base model version
adapter version
manifest hash
evaluation metadata hook
```

真正的参数训练不属于 V3；V3 只保留版本、评测与受治理升级所需的稳定接口。

## 4.6 模型资产不等于 Rust 源码

逻辑上 Intrinsic Model 属于 Yunxi Core；物理上模型权重不得用 `include_bytes!` 硬塞进可执行文件。

推荐作为产品运行时 bundle：

```text
models/yunxi-intrinsic/
```

启动时做 manifest、hash、兼容版本与 self-test。

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

# 6. V3 模块建议

当前 `yunxi-core/src/lib.rs` 已有 `mind`，尚无 `model` / `executive`。V3 按同级模块增量加入：

```text
crates/yunxi-core/src/
├── mind/                       # v2 existing
├── model/                      # v3 new
│   ├── mod.rs
│   ├── tier.rs
│   ├── health.rs
│   ├── capability.rs
│   ├── manifest.rs
│   ├── media.rs
│   ├── fallback.rs
│   └── intrinsic/
│       ├── mod.rs
│       ├── runtime.rs
│       ├── loader.rs
│       ├── tokenizer.rs
│       ├── text.rs
│       ├── vision.rs
│       ├── generation.rs
│       ├── cache.rs
│       └── config.rs
│
├── executive/                  # v3 new
│   ├── mod.rs
│   ├── conflict.rs
│   ├── confidence.rs
│   ├── priority.rs
│   ├── attention_budget.rs
│   ├── plan.rs
│   ├── expectation.rs
│   ├── candidate.rs
│   ├── reflection_controller.rs
│   ├── consistency.rs
│   ├── decision_record.rs
│   ├── snapshot.rs
│   └── policy.rs
│
├── planner.rs                  # existing
└── runtime.rs                  # existing
```

### 6.1 `ModelBackend` 迁移策略

当前 `ModelBackend` 已经位于 `planner.rs`，并且插件 `KoviModelBackend` 正在实现它。V3 不应为了目录漂亮就一次性破坏所有调用点。

第一阶段建议：

```text
core::model
→ re-export / wrap current planner::ModelBackend contract
```

当新 `model/` API 稳定后，再把 canonical definition 移入 `model/`；`planner` 与 crate root 保留兼容 re-export 至少一个稳定窗口。

禁止在 Phase 0 做全仓机械 rename。

### 6.2 产品语义与 Cargo feature 分离

产品层：Intrinsic Model 是 Yunxi 标准发行包的必备组件。

工程层：`yunxi-core` 仍应允许 domain-only test build，不要求每次单测都编译/加载大模型。

可以采用 feature：

```toml
[features]
intrinsic-model = []
```

标准 Yunxi 产品构建必须启用；纯 domain 单测可关闭。

这不代表 Intrinsic Model 是“可有可无的插件”，只是避免让模型 runtime 污染全部单测和 CI。

---

# 7. 平台无关约束

Executive 与 Intrinsic Model 继续严格平台无关。

Core 中禁止出现：

- QQ user_id / group_id；
- OneBot / Kovi / RuntimeBot；
- PgPool / SQLx / Redis client；
- OpenAI / Anthropic / provider API key；
- provider URL；
- `reqwest::Client` 用于模型或媒体下载；
- 直接理解平台 message segment。

Executive 只处理 canonical IDs 与 Core domain types。

## 7.1 图片的关键边界

当前 `Attachment` 已经是平台无关的 opaque reference；Core 不拥有 URL 语义，也不应该自己联网下载图片。

因此 Intrinsic vision 不能写成：

```text
Attachment.reference
→ reqwest GET inside yunxi-core
```

必须增加 host-supplied runtime port，例如：

```rust
pub trait ModelMediaResolver: Send + Sync {
    fn resolve_image<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> ModelMediaFuture<'a>;
}
```

返回 runtime-only、bounded 的图片数据：

```rust
pub struct ResolvedImage {
    pub bytes: Arc<[u8]>,
    pub media_type: Option<String>,
}
```

Host 负责：

```text
opaque attachment reference
→ authorized fetch / cache lookup
→ bounded bytes
```

Core 负责：

```text
validate bytes / dimensions
→ decode / preprocess
→ intrinsic vision inference
```

这样 `model/` 可以真实位于 Core 内部，同时不破坏平台无关性。

## 7.2 媒体安全上限

第一版至少限制：

- 单次最多处理 1 张图片；
- 输入字节上限；
- 解码后像素上限；
- 仅允许受支持 MIME / decode format；
- timeout；
- 不追随 Core 内部网络重定向，因为 Core 根本不做网络请求。

超限时返回可诊断错误，不触发 OOM。

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

# 30. Attention Budget 与 Cognitive Budget

## 30.1 目标

Attention v1 回答：

> “这个事件值不值得关注？”

Executive v3 继续回答：

> “当前系统还有多少认知预算可以分给它？”

加入 Intrinsic Model 后，还需要一个额外但仍然简单的问题：

> “这个事件值得使用哪一级认知能力？”

注意：**Budget 与 Tier 相关，但不是同一个东西。**

- Budget：逻辑资源额度。
- Tier：本轮允许选择的认知能力等级。
- Semaphore / queue：物理并发。

三者必须分离。

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

# 36. Budget、CognitiveTier 与模型 Semaphore

Executive Budget 是逻辑资源控制；现有 model semaphore / queue 仍属于物理并发控制。

V3 新增最小认知等级：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitiveTier {
    Reflex,
    Intrinsic,
    Standard,
    Enhanced,
}
```

语义：

```text
Reflex
→ Rust deterministic only
→ 不保证生成式聊天

Intrinsic
→ Yunxi 内置文字 + 视觉小模型
→ 基础对话、简单视觉、简单语义

Standard
→ host 配置的常规模型能力

Enhanced
→ 高质量 / 高成本 / 强推理模型能力
```

`Standard` / `Enhanced` 是能力等级，不是供应商名；完整 provider routing 不属于 V3 的实现范围。

### 36.1 默认选择原则

```text
Hard deterministic task
→ Reflex path

Low complexity / background / simple vision
→ Intrinsic

Normal conversation requiring stronger quality
→ Standard / Enhanced（如果可用）

High ambiguity / conflict / consequence
→ prefer Enhanced（如果可用且 policy 允许）
```

### 36.2 降级链

```text
Enhanced failure
→ Standard if distinct and healthy
→ Intrinsic
→ Reflex
```

第一版如果只有“Strong + Intrinsic”：

```text
Strong
→ Intrinsic
→ Reflex
```

最多一次模型 fallback；禁止 fallback loop。

### 36.3 强模型健康时 Intrinsic 也不是摆设

Intrinsic 可承担：

- 低复杂度 direct turn（由 Executive 直接选中）；
- 简单图片理解；
- background lightweight summary；
- 低成本分类 / tagging；
- health self-test；
- strong failure fallback。

但**不得**因此让所有强模型 direct turn强制增加一次 Intrinsic pre-pass。

### 36.4 Intrinsic v1 能力白名单

第一版 0.1B 模型默认只允许：

```text
ShortTextReply
SimpleSemanticClassification
SimpleStructuredExtraction
ImageDescription
SimpleVisualQuestionAnswering
LowCostSummarization
```

默认禁止直接拥有：

```text
DestructiveToolPlanning
PermissionDecision
SecurityDecision
DeepReflectionFinalAuthority
HighConsequenceAction
ComplexMultiStepToolAutonomy
```

即使模型输出相关内容，也必须被 Planner validation / ActionArbiter 拒绝或降级。

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

Mind v2 已经真实实现 Reflection domain、`ReflectionTrigger`、`ReflectionDepth`、`ReflectionInput`、`ReflectionProposal` 与 `ReflectionQueue`。

Executive v3 不创建第二套 Reflection；它决定：

> “现有 Reflection 候选什么时候真的值得执行、使用 Light 还是 Deep、是否应 defer，以及值得使用哪一级模型？”

---

# 60. 复用 V2 ReflectionTrigger，禁止新增 ReflectTick

当前 V2 已定义：

```rust
pub enum ReflectionTrigger {
    Idle,
    Maintenance,
    ConversationLikelyEnded,
    HighSalienceEvent,
    MemoryPressure,
    AgendaPressure,
    DayBoundary,
}
```

因此 V3 **不得**再新增或“重新定义” `WorldEvent::ReflectTick`。

现有 `IdleTick` / `MaintenanceTick`、conversation-end heuristic、Mind pressure signal 或 host scheduler 可以映射为上面的 `ReflectionTrigger`。

目标是：

```text
existing trigger
→ Executive gate
→ NoReflection / Light / Deep / Defer
```

而不是：

```text
new ReflectTick
→ mandatory model call
```

---

# 61. Reflection Trigger 与 Executive Gate

Executive 输入可以包括：

```text
ReflectionTrigger
conflict_count
important episode ended
agenda pressure
significant belief change
goal failure
repeated expectation violation
current load
current CognitiveTier
```

建议映射：

```text
IdleTick                  → ReflectionTrigger::Idle
MaintenanceTick           → ReflectionTrigger::Maintenance
conversation-end heuristic→ ReflectionTrigger::ConversationLikelyEnded
high salience event       → ReflectionTrigger::HighSalienceEvent
agenda near bound         → ReflectionTrigger::AgendaPressure
scheduled day boundary    → ReflectionTrigger::DayBoundary
```

Mind v2 的 `ReflectionInput::should_reflect()` 可以作为已有 baseline signal；Executive 在此基础上增加预算、冲突、负载与模型等级 gate，不应复制其内部规则。

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

DeepReflection 默认偏好 `Standard / Enhanced`。

如果当前只有 Intrinsic：

```text
high-value deep reflection
→ Defer when possible
```

只有配置明确允许且场景可降级时，才使用 Intrinsic 做 bounded light substitute；不能因为强模型离线就让 0.1B 模型独自承担高冲突 Belief 重写。

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

建立有界、可序列化 Snapshot：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ExecutiveSnapshot {
    pub active_conflicts: Vec<ConflictSnapshot>,
    pub prioritized_goals: Vec<GoalPrioritySnapshot>,
    pub attention_budget: AttentionBudgetSnapshot,
    pub active_plan: Option<PlanSnapshot>,
    pub pending_expectations: Vec<ExpectationSnapshot>,
    pub recent_decisions: Vec<DecisionRecordSnapshot>,
    pub cognitive_capability: CognitiveCapabilitySnapshot,
    pub version: u64,
}
```

建议：

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CognitiveCapabilitySnapshot {
    pub current_tier: CognitiveTier,
    pub preferred_tier: CognitiveTier,
    pub intrinsic_health: ModelHealth,
    pub strong_available: bool,
    pub text_available: bool,
    pub vision_available: bool,
    pub intrinsic_version: Option<IntrinsicModelVersion>,
}
```

Snapshot 只暴露“能力事实”，不暴露：

```text
API key
provider URL
QQ media URL
CUDA device handle
raw model pointer
```

Planner 可以知道“现在能力下降了”，但不需要知道供应商实现细节。

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

当前 `main` 的 `PlannerInput` 已经包含 V1 context 与 V2 `MindSnapshot`。V3 只能做**加法**，不得用旧文档里的 `WorkingStateSnapshot` 重写现有字段。

对齐当前代码后的目标形态：

```rust
pub struct PlannerInput {
    pub event: WorldEvent,
    pub state: PlannerStateSnapshot,
    pub memories: Vec<Memory>,
    pub open_loops: Vec<OpenLoop>,
    #[serde(default)]
    pub goals: Vec<Goal>,
    pub relation: Option<RelationState>,
    pub affect: AffectState,
    pub capabilities: Vec<ActionDescriptor>,
    #[serde(default)]
    pub mind: MindSnapshot,

    // v3 additive
    #[serde(default)]
    pub executive: ExecutiveSnapshot,
}
```

### 84.1 不把 Model Provider 塞进 PlannerInput

不要加入：

```text
provider_name
api_key
endpoint
model_path
```

这些不是认知输入。

Planner 只消费 `executive.cognitive_capability` 和有界 domain snapshot。

### 84.2 ModelBackend 最小组合层

V3 可以增加一个很薄的 `CognitiveModelStack`：

```rust
pub struct CognitiveModelStack {
    pub intrinsic: Arc<dyn ModelBackend>,
    pub strong: Option<Arc<dyn ModelBackend>>,
    pub policy: ModelFallbackPolicy,
}
```

它仍实现现有 `ModelBackend`，因此当前 `Planner::new(Arc<dyn ModelBackend>)` 不必重写。

职责只有：

```text
read preferred CognitiveTier
→ choose Strong or Intrinsic
→ bounded fallback on retryable failure
→ return PlannerOutput
```

完整角色路由、供应商 Registry、PrivacyClass 等不属于 V3 的实现范围。

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

低价值 deterministic：

```text
event
→ deterministic policy
→ direct decision
```

低复杂度生成式：

```text
event
→ Executive chooses Intrinsic
→ one Intrinsic Planner call
```

普通 / 中价值：

```text
event
→ Executive chooses current normal tier
→ one Planner call
```

高复杂度：

```text
event
→ strong Planner candidates
→ Executive compare
→ selected action
```

强模型不可用：

```text
retryable strong failure
→ at most one Intrinsic fallback
```

Intrinsic 不足以安全处理时：

```text
Defer / simplify / explicit capability-limited response
```

不要让小模型为了“保持在线”而伪装成拥有强推理能力。

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

一天内出现：

- 1 个高 salience Goal 完成；
- 2 个 Belief conflict；
- Agenda 接近上限。

Host / scheduler 产生：

```text
ReflectionTrigger::DayBoundary
```

Executive：

```text
DeepReflection
preferred tier = Enhanced
```

如果 Enhanced/Standard 不可用：优先 defer，而不是强迫 Intrinsic 深度改写 Belief。

---

# 110. Reflection Suppression Example

当前：

```text
direct conversation active
model queue saturated
```

收到：

```text
ReflectionTrigger::Maintenance
```

Executive：

```text
Defer
```

不产生额外 foreground model call。

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
COGNITIVE_TIER_INTRINSIC
COGNITIVE_TIER_DOWNGRADED
STRONG_MODEL_UNAVAILABLE
INTRINSIC_MODEL_UNAVAILABLE
INTRINSIC_FALLBACK_USED
REFLEX_ONLY
```

ReasonTag 必须是结构化元数据，不是隐藏思维链。

---

# 112. Metrics

建议至少：

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

yunxi_cognitive_tier_current
yunxi_cognitive_tier_downshifts_total
yunxi_intrinsic_load_failures_total
yunxi_intrinsic_inferences_total
yunxi_intrinsic_vision_inferences_total
yunxi_intrinsic_fallbacks_total
yunxi_intrinsic_inference_latency_ms
yunxi_intrinsic_peak_rss_bytes
yunxi_intrinsic_queue_depth
yunxi_strong_to_intrinsic_fallback_total
```

Model metrics 不记录 prompt / private image 内容。

---

# 113. Debug Interface

管理员可查看：

```text
#executive-status
```

输出：

- attention / cognitive budget；
- current CognitiveTier；
- Intrinsic model health / version；
- strong availability；
- top active goals；
- active conflicts；
- current plan；
- pending expectations；
- recent decision tags；
- reflection state。

另外可以提供：

```text
#intrinsic-status
```

仅输出：

- loaded / unavailable；
- model / adapter version；
- manifest hash；
- text / vision capability；
- queue / latency / last self-test；
- memory budget state。

不要输出：

- hidden chain-of-thought；
- secrets；
- full private memory；
- raw image bytes；
- API keys。

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
- AttentionBudget 重置合理 baseline；
- Intrinsic manifest 重新校验；
- Intrinsic weights 重新加载并 self-test；
- 不持久化 raw KV cache；
- 不把“上次 Strong 健康”当成本次启动事实，重新 probe。

Intrinsic 加载失败：

```text
Runtime stays alive
→ CognitiveTier::Reflex
→ health reports Unavailable
```

不得因为一个损坏的模型文件导致 data deletion / Reminder / runtime recovery 一起无法启动。

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

Executive 不得让普通 direct message 默认增加多个串行模型调用。

优先：

```text
one deterministic tier decision
+
one planner/model call
+
deterministic executive validation
```

允许的异常路径：

```text
Strong call fails
→ one Intrinsic fallback
```

这属于 failure recovery，不是正常 steady-state 双调用。

Intrinsic 本身应常驻并限制并发，避免每轮 load/unload。

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

所有 Executive 模块与 Model tiering 建议先 Shadow。

例如：

```text
would_preempt_goal
would_revise_plan
would_defer_reflection
would_select_candidate B
would_select_tier Intrinsic
would_fallback_to_intrinsic
```

Intrinsic Model 可以在 Phase 1/2 做离线 self-test 与显式管理员测试，但在 Shadow 阶段不得悄悄替换生产回复。

特别要求：

```text
shadow tier evaluation
!=
extra model inference
```

Shadow 默认只记录 deterministic routing decision；不要为了“看看小模型会说什么”给每条消息额外跑一次模型。

---

# 127. Rollout

推荐：

```text
Domain-only
→ Intrinsic explicit admin test
→ Intrinsic shadow routing
→ zero-external-model test environment
→ selected private low-risk turns
→ strong failure fallback
→ selected group
→ general
```

Executive 原有模块仍使用：

```text
Shadow
→ main admin
→ selected private
→ selected group
→ general
```

任何阶段都必须保留 previous release rollback；不要把“内置 fallback”当作部署回滚替代品。

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

[model.intrinsic]
enabled = true
shadow_routing = true
asset_dir = "models/yunxi-intrinsic"
max_parallel = 1

[model.fallback]
strong_to_intrinsic = false
max_model_attempts = 2
```

`model.intrinsic.enabled=false` 只用于开发 / CI /故障诊断；标准产品发行配置应启用。

不要增加 provider-specific flag 到 `yunxi-core`。

---

# 129. Phase 顺序

V3 继续按小步、可回滚方式落地。Intrinsic Model 加入后，Phase 仍保持 0–15，避免无限扩张版本。

```text
Phase 0   Current-main audit + Executive/Model domain skeleton
Phase 1   Intrinsic model loader + text inference + health (explicit test only)
Phase 2   Intrinsic vision + media resolver (explicit test only)
Phase 3   CognitiveTier + zero-external-mode + fallback shadow
Phase 4   ConflictMonitor + ConfidenceCalibration
Phase 5   GoalArbitration + Attention/CognitiveBudget
Phase 6   DecisionRecord
Phase 7   ExpectationState
Phase 8   PlanState + PlanRevision
Phase 9   CandidateEvaluation + tier-aware model choice
Phase 10  SelfConsistencyMonitor
Phase 11  ReflectionController using existing V2 ReflectionTrigger
Phase 12  Planner v3 / ExecutiveSnapshot integration
Phase 13  Real behavior + strong→intrinsic fallback activation
Phase 14  Persistence / erasure / asset / restart hardening
Phase 15  Behavior + availability + 2C4G evaluation / rollout
```

## Phase 0：Current-main Audit + Domain Skeleton

开始前重新读取：

```text
crates/yunxi-core/src/lib.rs
crates/yunxi-core/src/planner.rs
crates/yunxi-core/src/runtime.rs
crates/yunxi-core/src/event.rs
crates/yunxi-core/src/mind/*
plugins/model/src/yunxi/core_model.rs
plugins/model/src/yunxi/mind_runtime.rs
```

新增：

- executive domain IDs / snapshots；
- `model/` module skeleton；
- `CognitiveTier`；
- `ModelHealth`；
- `IntrinsicModelVersion`；
- `CognitiveCapabilitySnapshot`；
- compatibility re-export plan。

**不改变生产行为。**

验收：

```text
cargo test -p yunxi-core
```

仍不需要 Kovi、PostgreSQL、network、模型权重。

## Phase 1：Intrinsic Text

目标：把 MiniMind 0.1B language backbone 变成 Yunxi Core 内部的 inference-only text runtime。

要求：

- 不启动 Python sidecar 作为标准运行方式；
- 不加载 SenseVoice / Mimi / Talker / CAMPPlus；
- 权重外置 runtime bundle；
- startup manifest + hash + self-test；
- max_parallel = 1 默认；
- bounded context / max_new_tokens；
- explicit admin test only；
- 失败不影响 Core 启动。

先做一个短 feasibility spike，在 Rust-native/embedded engine 候选中只选**一个**生产实现。V3 domain API 不绑定 Candle / ORT 等具体 runtime 名称。

## Phase 2：Intrinsic Vision

只接：

```text
AttachmentKind::Image
→ host ModelMediaResolver
→ bounded bytes
→ decode / resize / normalize
→ SigLIP2 vision encoder
→ vision projector
→ MiniMind language backbone
```

第一版：

- batch 1；
- 每 turn 最多 1 张图；
- 目标输入 256×256（以最终导出模型 config 为准）；
- 无音频路径；
- 图片失败不得破坏文字回复路径。

## Phase 3：CognitiveTier / Zero External / Fallback Shadow

实现：

```text
Reflex
Intrinsic
Standard
Enhanced
```

增加 `CognitiveModelStack` 最小组合层。

Shadow 只记录：

```text
would_select_tier
would_fallback
```

不额外执行模型。

测试：

```text
no strong backend + intrinsic healthy
→ valid planner path exists
```

## Phase 4：Conflict + Confidence

先支持：

- Belief contradiction；
- Goal competition；
- capability conflict；
- duplicate intent；
- confidence clamp / source weight / max delta。

继续复用 Mind proposal / consolidation，不直接改 Mind store。

## Phase 5：Goal Arbitration + Budget

先做 Shadow rank。

Attention/CognitiveBudget 第一版只影响：

- low-priority background cognition；
- tier preference；
- optional deep work。

不得影响 direct MustExecute / Reminder / Stop。

## Phase 6：DecisionRecord

新增模型相关元数据但保持 bounded：

```text
selected disposition
reason tags
selected cognitive tier
fallback used?
intrinsic model version (when used)
confidence
```

不要保存完整 prompt 或隐藏推理。

## Phase 7：ExpectationState

先接：

- AskQuestion；
- Tool Action；
- Proactive follow-up。

## Phase 8：PlanState + PlanRevision

先用于：

- AgentTask-like multi-step action；
- Tool sequences。

支持 tool failure / stale / cancelled goal / expectation violation；revision bounded。

Intrinsic tier 默认不获得复杂多步 autonomous tool planning 权限。

## Phase 9：CandidateEvaluation + Tier-aware Choice

只用于灰区：

```text
Reply
Silent
ResumeAgenda
Defer
```

候选评分可以加入：

```text
required_cognitive_tier
intrinsic_suitability
strong_model_value
fallback_risk
```

仍然优先 deterministic weighted score。

## Phase 10：SelfConsistency

先 Shadow warning；高 severity 再 replan。

Intrinsic 生成的回复同样必须经过 SelfConsistency 与现有 protocol validation，不能因为“内置”而跳过。

## Phase 11：ReflectionController

只使用现有 V2：

```text
ReflectionTrigger
ReflectionInput
ReflectionDepth
ReflectionQueue
```

将 fixed/background opportunities 改成 condition-gated；不得新增 ReflectTick。

Deep Reflection 优先强模型；只有 Intrinsic 时优先 defer。

## Phase 12：Planner v3

给当前 `PlannerInput` **只增加** `ExecutiveSnapshot`。

保持：

```text
one normal planner model call
```

`CognitiveModelStack` 仍实现现有 `ModelBackend`，降低改动面。

## Phase 13：真实行为与 Fallback Activation

逐步让：

- proactive；
- goal priority；
- tool recovery；
- agenda resume；
- cognitive tier；
- strong→intrinsic retryable fallback；

真实受 Executive 影响。

先 private low-risk，再 group。

## Phase 14：Persistence / Erasure / Asset Hardening

补：

- Executive store indexes / TTL；
- restart / recovery；
- migration idempotency；
- Intrinsic manifest / hash / version；
- corrupted asset handling；
- model cache bounds；
- media cache invalidation；
- data erasure coordination；
- no raw KV cache persistence；
- adapter version slot only，不训练。

Data erasure 期间：

```text
begin Core/Mind barrier
→ invalidate matching model/media/learning-candidate caches
→ erase persistent state
→ verify
→ release barrier
```

任何 cache purge 失败必须 fail closed。

## Phase 15：Behavior / Availability / Resource Eval

固定评估包含：

```text
normal strong path
zero external model
strong outage fallback
intrinsic outage → Reflex
text-only
image+text
high load
restart
corrupt weights
data erasure race
no mandatory double model call
```

并在真实 2C4G CPU profile 上记录：

- startup peak RSS；
- steady RSS；
- text P50/P95 latency；
- vision P50/P95 latency；
- queue depth；
- OOM count；
- swap activity；
- throughput batch=1。

发布门槛不是某个拍脑袋的延迟数字，而是：

> **4GB RAM 目标机在限定 context / concurrency 下长时间运行不 OOM，并且 direct reply 不因模型重复调用产生不可接受退化。**

---

# 130. Behavioral Scenario A：Goal Priority

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

# 131. Scenario B：No Starvation

低优先级长 Goal：

等待很久。

预期：

priority 可以缓慢上升。

但不能压过安全/可靠任务。

---

# 132. Scenario C：Budget Under Load

群聊爆发 200 条消息。

预期：

- low priority mostly ObserveOnly；
- no model explosion；
- direct message still served。

---

# 133. Scenario D：Plan Revision

Tool A 失败。

预期：

Tool B fallback。

超过 revision limit：

PlanFailed。

---

# 134. Scenario E：Expectation

主动问：

```text
“面试怎么样？”
```

用户没回答。

预期：

不要立刻重复追问。

---

# 135. Scenario F：Self Consistency

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

# 136. Scenario G：Change Mind

强新证据出现。

预期：

Consistency 不阻止合理更新。

---

# 137. Scenario H：Reflection

无显著事件。

输入：

```text
ReflectionTrigger::Idle
```

预期：

```text
NoReflection
```

不得产生模型调用。

---

# 138. Scenario I：Deep Reflection

大量 conflict + high-salience episode。

输入：

```text
ReflectionTrigger::HighSalienceEvent
```

预期：

```text
DeepReflection candidate
```

如果只有 Intrinsic：默认 defer；不得自动执行高风险 belief rewrite。

---

# 139. Scenario J：Silent Candidate

群聊：

Mind 有兴趣。

Executive：

interruption cost high。

预期：

Silent。

---

# 140. Scenario K：Defer

Agenda 很重要。

当前 direct conversation 更重要。

预期：

Defer agenda。

---

# 141. Scenario L：Duplicate Event

同一个 root event 被 retry。

预期：

no duplicate side effect。

---

## Scenario M：Zero External Model

环境：

```text
strong backend = None
intrinsic = Healthy
```

预期：

- Runtime 正常启动；
- CognitiveTier 至少为 Intrinsic；
- 简单 direct text 可以产生可见回复；
- 不要求任何远程网络。

## Scenario N：Strong Outage Fallback

环境：

```text
selected Strong
→ retryable unavailable / timeout
```

预期：

```text
one Intrinsic fallback
```

并记录 reason tag / metric。

不得无限 retry。

## Scenario O：Intrinsic Outage

环境：

```text
strong unavailable
intrinsic weights corrupt
```

预期：

```text
CognitiveTier::Reflex
Runtime alive
Reminder / Stop / data erasure remain available
```

普通生成式聊天允许明确能力不可用，而不是伪造回答。

## Scenario P：Text + Vision Only

启动 Intrinsic：

预期：

- language loaded；
- vision loaded；
- vision projector loaded；
- SenseVoice 未加载；
- Mimi 未加载；
- Talker 未加载；
- CAMPPlus 未加载；
- audio input 返回 unsupported，而不是偷偷拉起音频栈。

## Scenario Q：Strong Healthy Without Double Call

普通 direct message 被 Executive 选择为 Standard/Enhanced。

预期：

```text
strong_calls = 1
intrinsic_calls = 0
```

只有 Strong 真正失败后才允许 `intrinsic_calls = 1`。

## Scenario R：Model Upgrade / Rollback Boundary

从：

```text
base v1 + adapter None
```

升级到：

```text
base v1 + adapter v2
```

必须：

- manifest version 可追踪；
- eval 结果可绑定版本；
- rollback 不修改 Mind state；
- V3 runtime 不执行训练。

---

# 142. Unit Tests

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
- snapshot version；
- CognitiveTier ordering / policy；
- ModelHealth transitions；
- max fallback attempts；
- Intrinsic capability whitelist；
- manifest validation；
- model version serialization；
- media size/dimension bounds；
- audio capability rejected in v1 Intrinsic。

---

# 143. Concurrency Tests

至少：

- concurrent goal updates；
- plan version race；
- expectation satisfy + expire race；
- direct reply + reflection；
- budget update race；
- no lock across await；
- no deadlock；
- intrinsic max_parallel=1 backpressure；
- strong failure + concurrent intrinsic fallback bounded；
- image resolve + data erasure race；
- model reload 不与 in-flight inference 破坏内存安全；
- fallback 不重复产生 side effect。

---

# 144. Persistence / Asset Tests

至少：

- plan restart；
- expectation restart；
- migration idempotency；
- duplicate action dedupe；
- decision record cleanup；
- conflict TTL；
- manifest hash mismatch；
- missing intrinsic asset；
- corrupt tokenizer / weights；
- version upgrade / rollback；
- raw KV cache not persisted；
- data erasure purges model/media scoped cache；
- erasure failure keeps barrier closed。

---

# 145. Performance Tests

确保普通 direct message：

```text
no mandatory second model call
```

目标 2C4G profile 必须单独跑 soak：

- batch = 1；
- bounded context；
- bounded output；
- Intrinsic max_parallel = 1；
- text-only steady run；
- periodic image run；
- 24h 或等价压力周期无 OOM；
- 记录 peak / steady RSS；
- 不把 steady-state 性能建立在持续 swap 上。

具体 P95 数字应以 Phase 1/2 benchmark 定基线后写入 CI threshold，本文不伪造硬件性能。

---

# 146. Cost / Model Call Tests

统计：

```text
candidate evaluation calls
reflection calls
plan revision calls
intrinsic calls
strong calls
strong→intrinsic fallbacks
calls avoided by Reflex/deterministic path
```

必须有测试证明：

```text
strong healthy normal direct turn
→ intrinsic extra calls = 0
```

以及：

```text
strong unavailable
→ intrinsic still serves eligible basic turns
```

---

# 147. Budget Metrics

记录：

```text
budget usage per minute
budget denial
critical reserve usage
```

---

# 148. No Chain-of-Thought

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

# 149. 不做人类意识宣称

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

# 150. Meta ≠ 无限套娃

Yunxi Executive 就是最高内部认知控制层。

不要再加：

```text
MetaExecutive
SuperExecutive
MetaMetaReasoner
```

---

# 151. v3 内部停止继续套 Meta 层

v3 完成后，不再增加：

```text
MetaExecutive
SuperExecutive
MetaMetaReasoner
```

本文件只定义 V3 内部边界，不继续向上叠加新的 Meta 控制层，也不在此扩展其他独立架构域。

V3 完成后优先优化：

- latency；
- intrinsic model quality；
- memory quality；
- behavioral tuning；
- availability；
- UX；
- observability。

---

# 152. 成功指标

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
10. 过去 Decision 会影响未来重复行为。
11. 不配置任何外部模型时，Yunxi 仍能进入 Intrinsic 模式并完成基础文字交互。
12. 强模型故障时可 bounded fallback 到 Intrinsic，而不是整个 Agent 失效。
13. Intrinsic 故障时 Core 仍存活于 Reflex 模式。
14. 普通 Strong direct turn 不增加强制 Intrinsic 串行调用。
15. 图片可通过 host media port 进入 Intrinsic vision，Core 不自行联网。
16. Intrinsic v1 只加载文字 + 视觉组件，不加载 Omni 音频栈。
17. 模型升级/回滚不改变 Mind 身份与长期状态。
18. V3 没有实现在线权重自修改。

---

# 153. 失败指标

如果变成：

```text
每条消息都多一个 Executive LLM call
每条 Strong 消息都先跑一次 Intrinsic
每个小冲突都深度反思
每个 Goal 都复杂计划
每次工具失败都无限修订
每次 disagreement 都触发 consistency alarm
Intrinsic 直接决定权限
Core 自己 reqwest 下载 QQ 图片
加载 MiniMind-O 时顺便拉起 SenseVoice/Mimi/Talker
外部模型没配就启动失败
每轮聊天都拿自己的回答在线训练自己
把完整多供应商模型平台全部提前塞进 v3
```

说明 v3 设计失败。

---

# 154. 配置建议

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

[model.intrinsic]
enabled = true
asset_dir = "models/yunxi-intrinsic"
max_parallel = 1
max_context_tokens = 2048
max_new_tokens = 256
max_images_per_turn = 1
startup_self_test = true

[model.fallback]
strong_to_intrinsic = true
max_model_attempts = 2
```

数值只是第一版上限建议，必须由 2C4G benchmark 校正。

不要把 provider key / URL 放进这组 Core 配置；provider 配置继续由 host / infrastructure 管理。

---

# 155. Codex 执行原则

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

# 156. Codex 禁止

不得：

- 重写 v1；
- 重写 v2；
- 删除 Mind 数据；
- 把 Executive 变成第二聊天模型；
- 在 Core 中引入 QQ / OneBot / Kovi；
- 在 Core 中引入 SQLx / Redis client；
- 在 Core 中硬编码 OpenAI / provider endpoint；
- 为了图片在 Core 内直接做网络下载；
- 把 MiniMind 当成 Yunxi identity；
- 默认加载 SenseVoice / Mimi / Talker / CAMPPlus；
- 每条 direct message 强制 Intrinsic + Strong 双调用；
- 增加无限 background thinking；
- 保存 hidden chain-of-thought；
- 在线直接修改模型权重；
- 未经 eval 自动 promote adapter；
- 自动生产部署。

---

# 157. 兼容性测试

每个 Phase 继续保证：

- direct reply；
- group reply；
- ReplyTicket / outgoing revalidation；
- Stop；
- Reminder；
- AgentTask；
- Tool；
- proactive；
- OpenLoop；
- Mind v2；
- Mind data erasure / outgoing fence；
- InteractionCues；
- CLI host；
- provider repair/fallback current behavior；

不回归。

新增兼容矩阵：

```text
Strong only (migration test)
Intrinsic only
Strong + Intrinsic
Neither → Reflex
Text only
Text + image
```

其中标准产品目标是 `Strong + Intrinsic` 或 `Intrinsic only`；`Neither` 只保证 deterministic survival。

---

# 158. 最终 Architecture

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
                  ┌────────────────┼────────────────┐
                  ▼                ▼                ▼
              YUNXI MIND       MEMORY/GOALS     MODEL HEALTH
                  │                                 │
      ┌───────────┼───────────┐                     │
      ▼           ▼           ▼                     │
   BELIEF     PREFERENCE    AGENDA                   │
      │           │           │                     │
      └───────────┴─────┬─────┘                     │
                        ▼                           │
                   YUNXI EXECUTIVE ◄───────────────┘
                        │
        ┌───────────────┼────────────────┐
        ▼               ▼                ▼
     CONFLICT         BUDGET       COGNITIVE TIER
        │               │                │
        ├───────┬───────┴───────┬────────┤
        ▼       ▼               ▼        ▼
      PLAN  EXPECTATION     CONSISTENCY  CANDIDATE
        │       │               │        │
        └───────┴────────┬──────┴────────┘
                         ▼
                      PLANNER
                         │
                         ▼
                COGNITIVE MODEL STACK
                  │                │
          ┌───────┘                └─────────┐
          ▼                                  ▼
   INTRINSIC MODEL                    HOST STRONG MODEL
   Text + Vision                       Optional
          │                                  │
          └──────────────┬───────────────────┘
                         ▼
                      INTENT
                         │
                         ▼
                  ACTION ARBITER
                         │
                         ▼
                       ACTION
                         │
                         ▼
                        WORLD
```

当 Strong 不存在：

```text
Planner
→ Intrinsic
```

当 Strong 失败：

```text
Strong
→ bounded Intrinsic fallback
```

当两者都失败：

```text
Reflex deterministic Core remains alive
```

这才是 V3 定义的“能力降级”，而不是“模型服务中断 = Yunxi 死亡”。

---

# 159. 最终 Definition of Done

## Conflict / Confidence / Goal

- 可检测关键内部冲突；
- bounded，不泛滥；
- confidence 有来源、有 max delta、可因证据变化；
- hard priority 正确；
- soft goal 可排序、preempt / resume、避免 starvation。

## Budget / Plan / Expectation

- 高负载可降级；
- critical reserve 存在；
- 不影响 MustExecute；
- 长任务 Plan 可持久化与 bounded revision；
- stale 可检测；
- Expectation 可 satisfied / expired，不演化成过度心理推断。

## Candidate / Reflection / Consistency

- Silent / Defer 是一等候选；
- 不每条消息调用额外模型；
- Reflection 使用 V2 既有 `ReflectionTrigger`，condition-gated，可 defer、有 budget；
- 保护高稳定 SelfModel，同时允许证据驱动 ChangeMind；
- DecisionRecord 结构化、bounded、不存 CoT。

## Intrinsic Model

- `crates/yunxi-core/src/model/` 与 `mind/` 同级存在；
- Intrinsic v1 支持文字；
- Intrinsic v1 支持图片；
- 不加载 SenseVoice / Mimi / Talker / CAMPPlus；
- 模型权重不 `include_bytes!`；
- runtime bundle 有 manifest / hash / version / self-test；
- 模型常驻，默认 max_parallel=1；
- 目标 2C4G profile 长时不 OOM；
- Core 不自行联网解析 Attachment；
- host media resolver 有 bounded bytes / timeout；
- corrupted assets 不使 Core 整体启动失败。

## Availability

- 没有任何 external model 是合法配置；
- Intrinsic healthy 时可完成基础 direct text reply；
- Strong retryable failure 最多一次 fallback 到 Intrinsic；
- Intrinsic 也失败时进入 Reflex，deterministic runtime 继续；
- normal Strong direct turn 不强制调用 Intrinsic。

## Safety

- Intrinsic 不决定 permission / security；
- MustExecute 继续由 deterministic policy 保护；
- ActionArbiter 仍是副作用边界；
- Intrinsic v1 默认不获得复杂 autonomous tool planning 权限。

## Growth Boundary

- 记录 base / adapter / manifest version；
- V3 不在线训练；
- V3 不自动修改 weights；
- learning candidate 只能作为受治理、默认关闭的 hook；
- LoRA / distillation / promotion / rollback training pipeline 不属于 V3。

## Platform Independence

全部认知 domain 位于 Yunxi Core，不依赖：

- QQ；
- Kovi；
- OneBot；
- PostgreSQL client；
- GUI；
- 具体云供应商。

## Regression Gate

必须：

```text
V1 tests green
V2 Mind tests green
V3 tests green
zero-external-mode tests green
no-double-call tests green
data-erasure race tests green
2C4G soak gate green
```

---

# 160. 最终行为验收

完成 v3 后，系统不应该只是：

```text
我有什么想法
→ 直接说出来
```

而应该进一步表现为：

```text
“这个现在不重要。”
“这个判断我没有那么确定。”
“当前信息变了，旧计划不该继续。”
“现在不适合插话。”
“这个问题值得调用更强的认知能力。”
“强模型不可用，我仍然可以用基础能力处理简单任务。”
“这个超出我当前 Intrinsic 能力，先 defer / 简化，而不是乱猜。”
```

从系统行为上看，外部强模型切换或短暂失效应该表现为：

```text
capability changes
```

而不是：

```text
identity reset
or
runtime death
```

Mind continuity、Goal、OpenLoop、Relation、Episode 与 deterministic runtime 继续保持。

---

# 161. 最重要的一句话

> **Yunxi Executive v3 的目标，是让 Yunxi 不仅拥有持续的 Mind，还能根据冲突、预算、风险与模型健康状态管理自己的认知能力；外部强模型是增强，Intrinsic Model 是最低生成式认知，Rust Core 是最终确定性生存层。**

当：

```text
当前事件
+ WorkingState
+ Mind
+ Memory / Goal / OpenLoop
+ Executive state
+ Cognitive capability
+ 过去决策
```

能够真实改变未来行为，并且：

```text
strong service disappears
→ Yunxi degrades
→ Yunxi does not disappear
```

Yunxi Executive v3 才算完成。

---

# Appendix A：Intrinsic Cognitive Model 实施规格

本 Appendix 是 V3 新增的模型实施约束，目标是让 Codex/开发者可以直接按阶段落地，而不是只知道“加一个小模型”。

## A.1 上游基线

第一代 Intrinsic Model 参考：

```text
jingyaogong/minimind-o
minimind-3o main backbone ≈ 0.1B / 115M
```

上游完整 Omni 支持：

```text
text / audio / image input
text / streaming audio output
```

Yunxi V3 **不完整嵌入 Omni**。

只保留：

```text
MiniMind language backbone
Tokenizer
SigLIP2 vision encoder
Vision projector
Text generation
```

明确删除 / 不加载：

```text
SenseVoice-Small
Audio projector
Talker
Mimi codec
CAMPPlus
TTS / voice cloning
barge-in audio path
```

原因不是这些能力永远不要，而是第一代 2C4G Intrinsic 目标只解决：

> **最低文字 + 视觉认知可用性。**

语音以后由专门版本/能力层接入，不应拖垮 V3 最低模型。

## A.2 不直接运行上游 `eval_omni.py`

上游 Demo 面向完整 Omni，会初始化完整链路。Yunxi 不能把它作为生产 Intrinsic runtime。

需要做的是：

```text
extract / port inference-relevant architecture
→ convert verified weights
→ Rust embedded inference
→ Yunxi-specific bounded wrapper
```

生产默认不得依赖：

```text
python child process
PyTorch service sidecar
remote inference server
```

否则它不再是 Yunxi “随本体存在”的 Intrinsic 能力。

开发期可以用 Python 上游实现做数值对照 oracle，但不能成为最终必需运行依赖。

## A.3 Runtime Engine 不写进 Domain Contract

V3 可以对 Candle、ONNX Runtime 或其他 Rust 可嵌入 inference engine 做 feasibility benchmark，但只选一个 production implementation。

Domain API 只看到：

```rust
pub trait IntrinsicInferenceEngine: Send + Sync {
    fn health(&self) -> ModelHealth;
    fn version(&self) -> IntrinsicModelVersion;
    // bounded text / vision inference methods
}
```

不要让：

```text
CandleTensor
OrtSession
CUDA handle
```

泄漏到 Planner / Executive / Mind public types。

## A.4 IntrinsicModelVersion

建议：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntrinsicModelVersion {
    pub model_id: String,
    pub base_version: String,
    pub adapter_version: Option<String>,
    pub manifest_hash: String,
}
```

`adapter_version` 在 V3 可以始终为 `None`，但字段应保留，以支持未来受治理的离线 Adapter 升级而不破坏 manifest 兼容性。

## A.5 Manifest

运行目录建议：

```text
models/
└── yunxi-intrinsic/
    ├── manifest.toml
    ├── tokenizer.json
    ├── language/
    │   └── weights.*
    ├── vision/
    │   └── weights.*
    ├── projector/
    │   └── weights.*
    ├── LICENSES/
    └── THIRD_PARTY_NOTICES
```

manifest 至少记录：

```text
manifest_version
model_id
model_version
architecture
upstream_repository
upstream_revision
supports_text
supports_vision
supports_audio = false
context_limit
image_size
weight files
sha256 per asset
adapter_version(optional)
```

启动：

```text
read manifest
→ validate schema
→ validate files
→ validate hashes
→ load tokenizer
→ load language
→ load vision/projector
→ self-test
→ Healthy
```

失败：

```text
Unavailable
→ Core continues in lower tier
```

## A.6 License / Redistribution Gate

上游代码仓库使用 Apache-2.0，但**打包模型权重、Tokenizer、SigLIP2 等资产前必须分别核对其实际许可证与 NOTICE 要求**。

发布流程必须生成：

```text
THIRD_PARTY_NOTICES
asset provenance
upstream revision
license record
```

不得因为 repo 代码是 Apache-2.0 就自动假设所有下载权重都具有同一再分发条件。

## A.7 Text Inference Profile

第一版推荐 bounded profile：

```text
batch = 1
parallel = 1
context default <= 2048
max_new_tokens default <= 256
streaming optional
```

理论模型 context 能更长不代表 2C4G 应该使用更长 context。

ContextBuilder 应优先保留：

1. hard system constraints；
2. current event；
3. relevant Mind；
4. active Goal / OpenLoop；
5. recent bounded conversation；
6. relevant Memory。

不要简单从头截断。

## A.8 Intrinsic Reply Profile

0.1B 第一版不必被迫生成完整复杂 Planner JSON。

更可靠的方式：

```text
Intrinsic raw text generation
→ bounded wrapper
→ validated simple PlannerOutput
```

默认支持：

```text
Reply
Silent
Ask simple clarification
Simple semantic sidecar
```

默认不支持：

```text
multi-tool autonomous plan
permission-changing actions
destructive action planning
complex admin command execution
```

这可以显著降低“模型很小但协议很复杂”造成的失败率。

## A.9 Vision Path

当前 Core `Attachment` 只保存 opaque reference，因此流程必须是：

```text
WorldEvent
→ AttachmentKind::Image
→ ModelMediaResolver(host)
→ ResolvedImage bytes
→ Core size validation
→ decoder
→ RGB
→ resize / normalize
→ SigLIP2
→ Vision Projector
→ language hidden space
→ generation
```

禁止：

```text
Core interprets QQ file id
Core parses OneBot segment
Core performs arbitrary URL fetch
```

第一版只处理 1 张图片；多图以后再扩展。

## A.10 Model Health

建议：

```rust
pub enum ModelHealth {
    Loading,
    Healthy,
    Degraded,
    Unavailable,
}
```

Health 来源至少包括：

- loaded state；
- last self-test；
- recent OOM / inference error；
- queue saturation；
- media path health；
- asset mismatch。

V3 不需要完整 circuit breaker 状态机；只保留最小 health 与 bounded fallback 即可。

## A.11 CognitiveModelStack

第一版只需要两类生成式 Backend：

```text
Strong (host supplied, optional)
Intrinsic (Core supplied)
```

最小策略：

```text
preferred tier = Intrinsic
→ Intrinsic

preferred tier >= Standard + strong healthy
→ Strong

Strong retryable failure
→ Intrinsic once

Intrinsic failure
→ return unavailable to Reflex policy
```

不能：

```text
Strong ↔ Intrinsic ↔ Strong loop
```

## A.12 Reflex Tier

Reflex 不是小模型。

Reflex = Rust deterministic survival path：

- event admission；
- WorkingState；
- Stop；
- Reminder；
- data erasure；
- idempotency；
- bounded scheduling；
- hard permission；
- selected deterministic responses if explicitly coded。

Reflex 不承诺开放式聊天质量。

因此：

```text
all generative models unavailable
```

时，系统可以明确表示生成能力暂不可用，但不能让可靠任务、删除请求与运行时一起停止。

## A.13 2C4G Profile

目标部署：

```text
2 CPU cores
4 GiB RAM
no GPU required
```

V3 不在文档里虚构绝对 token/s。

必须实际测：

- cold start；
- warm start；
- text 64 / 128 / 256 output；
- context 512 / 1024 / 2048；
- one 256px image；
- 1h / 24h soak；
- strong fallback burst。

第一版原则：

```text
bounded > clever
single concurrency > OOM
stable > maximum context
```

Swap 可作为操作系统安全垫，但 release gate 不能依赖持续 swap 才能稳定工作。

## A.14 Intrinsic 的“成长”边界

Yunxi 的成长分四级：

```text
1. Mind Growth
   Belief / Preference / Interest / Episode / Agenda
   → V2 已提供基础

2. Experience Learning
   retrieval + Reflection + Consolidation
   → V2/V3 runtime behavior

3. Adapter Learning
   LoRA / adapter update
   → 不属于 V3

4. Base Model Evolution
   distillation / replacement
   → 不属于 V3
```

V3 只确保同一个 `IntrinsicModelVersion` 能被追踪。

严禁：

```text
raw user turn
→ immediate SGD
```

未来可以：

```text
strong model high-quality result
→ evaluation
→ curated LearningCandidate
→ offline GPU training
→ eval
→ canary
→ promote
→ rollback if needed
```

但这条训练流水线不属于 V3。

## A.15 Data Erasure 与 Model Cache

Intrinsic Model 本身不应持久化用户身份数据到权重。

任何 runtime cache 若按 Person / Conversation 建索引：

- prompt cache；
- media cache；
- semantic result cache；
- future learning candidate queue；

必须接入现有 data-erasure barrier。

删除顺序：

```text
barrier begins
→ block new matching inference/cache writes
→ drain or invalidate in-flight scoped work
→ purge model caches
→ erase Mind/Core persistent data
→ verify
→ barrier ends
```

不能只删数据库而留下用户图片/语义结果在模型缓存中。

## A.16 非 V3 范围的模型基础设施

以下能力不在 V3 内实现：

- Strong provider registry；
- arbitrary backend routing；
- model role routing；
- cost / privacy / latency / quality policy；
- full circuit breaker；
- prompt registry；
- embeddings / reranking。

无论这些能力以后如何实现，`core::model::intrinsic` 都应继续保持 Yunxi 的标准最低能力实现，而不是退化成“某个可选第三方 provider”。

---

# Appendix B：Outgoing Revalidation & Collision Arbitration


## 1. 新职责：Outgoing Revalidation

Executive v3 新增正式职责：

> 当 PendingOutgoing 尚未 `Committed` 且 Conversation 在生成期间发生变化时，判断旧内容是否仍值得发送。

---

## 2. 输入

建议：

```text
PendingOutgoing
New WorldEvent(s)
Latest MindSnapshot
Latest ConversationState
ExecutiveSnapshot
Optional bounded external-state snapshot
```

---

## 3. 输出

建议：

```rust
pub enum OutgoingRevalidation {
    CommitAsIs,
    Cancel,
    Supersede,
    Rewrite(RewriteRequest),
    Merge(MergeRequest),
    Defer(DeferUntil),
}
```

---

## 4. Deterministic Fast Path 优先

以下情况不需要额外模型：

- ReplyTicket stale
- generation stale
- Stop intent
- target invalid
- permission invalid
- capability invalid
- exact duplicate
- OpenLoop 已明确 resolved
- user already answered exact pending question
- direct reply preempts unrelated proactive

只有：

```text
conversation changed
+
semantic ambiguity high
```

才进入 Executive / lightweight model 仲裁。

---

## 5. 典型场景

### Pending

```text
“你今天面试怎么样？”
```

### New Message A

```text
“我面试过了！”
```

结果：

```text
Supersede / Rewrite
```

### New Message B

```text
“我刚去吃火锅了。”
```

结果：

```text
CommitAsIs / Defer
```

不能 deterministic 全取消。

---

## 6. Direct Reply vs Proactive

默认：

```text
Direct Reply
>
Prepared Proactive
```

如果 proactive 尚未 `Committed`：

可以：

```text
Cancel
Merge
Defer
Rewrite
```

---

## 7. 多 Proactive Motive

如果两个 motive 同时准备：

```text
FollowUp
+
Share
```

Executive 应：

- 选择主 motive；
- 或自然 Merge；
- 或 Defer 一个。

禁止短时间机械连续发两条独立主动消息。

---

## 8. CandidateScore 新增维度

建议加入：

```text
semantic_staleness
duplicate_question_cost
conversation_change_cost
user_already_answered
direct_preempts_proactive
collision_risk
rewrite_value
```

---

## 9. MustExecute 边界

Executive 不得取消：

- Reminder
- data deletion
- Stop handling
- committed task delivery
- security / permission operation

但可以调整：

```text
自然语言包装
合法 delivery timing
```

因此：

```text
MustExecute
!=
MustSendExactOldSentence
```

---

## 10. Rewrite / Merge 次数限制

单个 PendingOutgoing：

```text
max rewrite / merge count
```

必须 bounded，例如：

```text
2
```

用户连续快速发送时，超过限制：

```text
Cancel / re-enter normal planning
```

避免永远生成、永远发不出去。

---

## 11. 测试补充

至少：

- user already answered → Supersede
- unrelated message → Keep/Defer
- direct preempts proactive
- proactive + proactive merge
- rewrite bounded
- MustExecute not silently cancelled
- no extra LLM on unchanged conversation
- semantic gray-zone only calls optional evaluator
- optional evaluator may use Intrinsic when policy allows, but unchanged conversations trigger no call

---

## 12. 性能原则

普通 direct message：

```text
conversation_version unchanged
```

不得因为 V3 默认增加额外模型调用。

尤其禁止在 conversation_version unchanged 时同时调用 Intrinsic + Strong。

---

## 13. 核心结论

Executive v3 负责的不是：

> “阻止双方同时说话。”

而是：

> **“在仍有机会修改输出时，判断旧输出是否已经失去语义价值。”**
