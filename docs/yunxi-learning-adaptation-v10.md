# Yunxi Learning & Adaptation v10：持续学习、策略适应与受治理模型训练开发文档

**文档状态：** 最终设计稿  
**版本：** V10  
**定位：** Yunxi V1～V9 之上的持续学习、经验沉淀、策略适应与受治理离线训练层  
**目标：** 让 Yunxi 能够从长期交互、任务结果、用户纠正、评测结果和环境反馈中持续学习，同时严格区分“即时状态学习”“运行时策略适应”“模型权重学习”，确保所有学习过程可追溯、可评测、可回滚、可治理。

---

# 1. V10 的定位

V10 不是新的认知层。

```text
V1 Core
→ 平台无关 Agent 生命循环

V2 Mind
→ 自我、信念、兴趣与内部议程

V3 Executive
→ 注意力、目标、冲突、计划和决策控制

V4 World Model
→ 外部世界状态估计、预测和模拟

V5 Model Fabric
→ 本地 / 云端 / 多模型推理基础设施

V6 Runtime Foundation
→ 多任务、多通道、Action 生命周期、恢复和降级

V7 Perception–Action Loop
→ 世界感知、行动反馈、时间和任务驱动闭环

V8 Affordance & Cognitive I/O Protocol
→ 外部环境动态发布 Context、Affordance 和 DecisionRequest

V9 Evaluation & Autonomy Governance
→ 行为评测、自主权限、回归、Shadow、Canary 和发布治理

V10 Learning & Adaptation
→ 从经验中产生学习信号、构造训练数据、调整策略并训练候选模型
```

V10 不追求：

```text
每条消息后都训练一次
```

而追求：

```text
真正有价值的经验
→ 被识别
→ 被过滤
→ 被结构化
→ 被评测
→ 必要时改变策略或模型
```

---

# 2. 核心原则

> **自学习优先发生在 Memory、Belief、Preference、Policy 和 Runtime 参数层；只有积累到足够高质量、可追溯、可评测的数据后，才允许改变模型权重。**

> **线上交互负责产生经验，离线训练负责改变权重，V9 负责判断新权重能不能上线。**

> **用户纠正应先立即修正当前状态，而不是立即拿去训练模型。**

> **模型自己的输出不能自动成为“真值训练数据”。**

---

# 3. V10 三层学习模型

V10 正式把学习分为：

```text
Layer A
Immediate State Learning

Layer B
Runtime Policy Adaptation

Layer C
Offline Model Learning
```

## Layer A：Immediate State Learning

包括：

```text
Memory
Belief
Preference
Interest
Relation
OpenLoop
WorldModel
Task Knowledge
```

特点：

```text
立即生效
低成本
可回滚
不改模型权重
```

## Layer B：Runtime Policy Adaptation

包括：

```text
proactive threshold
attention threshold
retrieval top_k
ranking weight
reflection trigger
retry policy
routing preference
context selection weight
```

特点：

```text
参数化
受约束
可版本化
可评测
可回滚
```

## Layer C：Offline Model Learning

包括：

```text
SFT
LoRA
QLoRA
Preference Training
Embedding Fine-tune
Reranker Fine-tune
Classifier Fine-tune
```

特点：

```text
离线
有 Dataset
有 Model Artifact
有 V9 Evaluation
有 Shadow
有 Canary
可回滚
```

---

# 4. V10 要解决的问题

V10 必须解决：

```text
1. 什么叫“学习”
2. Memory 学习与权重学习如何区分
3. 用户纠正如何立即生效
4. 成功 / 失败任务如何形成学习信号
5. 什么时候生成 LearningCandidate
6. LearningCandidate 如何筛选
7. 训练数据如何记录 provenance
8. 私密数据能否进入训练集
9. 什么数据只能留在 Memory
10. 什么数据可以进入本地训练
11. 如何避免模型自我强化错误
12. 如何避免坏样本污染
13. 如何构造正样本 / 负样本 / 对比样本
14. 如何做 hard negative
15. 如何做 replay dataset
16. 如何防止 catastrophic forgetting
17. 如何进行 SFT / LoRA / QLoRA
18. 哪些 ModelRole 优先 fine-tune
19. 如何训练 Semantic / Extraction 小模型
20. 如何训练 Planner
21. Dialogue 是否值得训练
22. Embedding / Reranker 如何版本化
23. 如何评估 Candidate Model
24. 训练后如何 Shadow
25. 如何 Canary
26. 什么时候回滚
27. 是否允许自动触发训练
28. 自动训练如何受 V9 治理
29. 如何防止训练自己的错误输出
30. 如何防止用户隐私固化进共享权重
31. 如何构建 Learning Journal
32. 如何控制训练成本与频率
33. 如何恢复旧模型
34. 如何管理多个 LoRA / Adapter
35. 如何做 Role specialization
36. 如何把 Production Incident 转成学习样本
37. 如何证明新模型真的更好
```

---

# 5. V10 非目标

V10 不负责：

```text
直接管理 GPU 进程
替代 V5 Model Fabric
替代 V9 Evaluation
修改 Hard Safety Policy
在线实时 fine-tune foundation model
让模型自己决定并部署自己
把全部用户 Memory 训练进共享模型
```

V10 必须依赖：

```text
V5 Model Fabric
V9 Evaluation & Governance
```

---

# 6. 总体学习闭环

```text
Production Runtime
      │
      ▼
Interactions / Tasks / Feedback
      │
      ▼
Learning Signals
      │
      ▼
LearningCandidate
      │
      ▼
Candidate Filter
      │
      ├──────────────┐
      ▼              ▼
Reject          Experience Pool
                     │
                     ▼
                Dataset Curator
                     │
                     ▼
                Dataset Version
                     │
                     ▼
                  Training
                     │
                     ▼
               Candidate Model
                     │
                     ▼
                 V9 Evaluation
                     │
            ┌────────┴────────┐
            ▼                 ▼
          Reject            Shadow
                               │
                               ▼
                             Canary
                               │
                               ▼
                             Deploy
                               │
                               ▼
                      Production Signals
                               │
                               └────→ Learning Loop
```

---

# 7. 正式模块

建议：

```text
LearningSignal
LearningCandidate
LearningCandidateStore
ExperiencePool
CorrectionSignal
OutcomeSignal
PreferenceSignal
FailureSignal
SuccessSignal
PrivacyFilter
TrainingEligibility
ProvenanceTracker
DataQualityScorer
Deduplicator
CounterexampleBuilder
HardNegativeMiner
DatasetCurator
DatasetManifest
DatasetVersion
TrainingJob
TrainingRecipe
TrainingScheduler
TrainingBudget
ModelCandidate
AdapterArtifact
TrainingLineage
RuntimeTuner
AdaptationPolicy
LearningJournal
RollbackPlan
```

---

# 8. 目录建议

```text
crates/
└── yunxi-learning/
    ├── src/
    │   ├── lib.rs
    │   ├── signal/
    │   ├── candidate/
    │   ├── experience/
    │   ├── provenance/
    │   ├── privacy/
    │   ├── quality/
    │   ├── dataset/
    │   ├── training/
    │   ├── adaptation/
    │   ├── promotion/
    │   └── observability/
    └── tests/
```

---

# 9. LearningSignal

LearningSignal 表示：

> 某次真实交互中出现了值得系统学习的证据。

```rust
pub enum LearningSignalKind {
    UserCorrection,
    UserPreference,
    TaskSuccess,
    TaskFailure,
    ToolFailure,
    PlannerFailure,
    ActionFailure,
    RetrievalSuccess,
    RetrievalFailure,
    ProactiveAccepted,
    ProactiveIgnored,
    ProactiveRejected,
    WorldPredictionCorrect,
    WorldPredictionWrong,
    PolicyViolation,
    EvaluationFailure,
    EvaluationSuccess,
    ManualLabel,
}
```

---

# 10. LearningSignal 数据结构

```rust
pub struct LearningSignal {
    pub id: LearningSignalId,
    pub kind: LearningSignalKind,
    pub source_event_ids: Vec<EventId>,
    pub task_id: Option<RuntimeTaskId>,
    pub conversation_id: Option<ConversationId>,
    pub person_id: Option<PersonId>,
    pub role: Option<ModelRole>,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
}
```

LearningSignal 只表示：

```text
这里可能值得学
```

不表示：

```text
这条可以直接拿去训练
```

---

# 11. 用户纠正

例如：

```text
Yunxi:
“你的面试在周二。”

User:
“不是，是周三。”
```

正确顺序：

```text
Memory / Belief 立即修正
↓
LearningSignal::UserCorrection
↓
LearningCandidate
↓
ExperiencePool
↓
未来 Dataset
```

禁止：

```text
用户纠正
→ 立即 fine-tune
```

---

# 12. 强反馈与弱反馈

推荐强度：

```text
explicit user correction
>
explicit approve / reject
>
task objective result
>
tool objective result
>
implicit engagement
>
silence
```

用户没有回复：

```text
不能自动视为负反馈
```

只能作为弱信号。

---

# 13. 模型自评不能成为 Ground Truth

禁止：

```text
Model:
“我觉得我回答得很好。”

→ Positive Training Label
```

模型自己的输出：

只有经过：

```text
外部事实
用户纠正
Task Outcome
Tool Result
Human Label
Golden Scenario
```

等独立证据，才可成为高置信样本。

---

# 14. LearningCandidate

```rust
pub struct LearningCandidate {
    pub id: LearningCandidateId,
    pub source_signal_ids: Vec<LearningSignalId>,
    pub category: LearningCategory,
    pub target_role: Option<ModelRole>,
    pub input: CandidateInput,
    pub preferred_output: Option<CandidateOutput>,
    pub rejected_output: Option<CandidateOutput>,
    pub confidence: f32,
    pub quality_score: f32,
    pub privacy_class: PrivacyClass,
    pub provenance: DataProvenance,
    pub created_at: DateTime<Utc>,
}
```

---

# 15. LearningCategory

```rust
pub enum LearningCategory {
    Semantic,
    WorldExtraction,
    Planner,
    Dialogue,
    Summarization,
    Embedding,
    Reranking,
    ToolSelection,
    Preference,
    Safety,
}
```

---

# 16. Candidate 生命周期

```text
Proposed
→ Filtered
→ ApprovedForPool
→ IncludedInDataset
→ Archived
```

或：

```text
Rejected
```

---

# 17. DataProvenance

```rust
pub struct DataProvenance {
    pub origin: ProvenanceOrigin,
    pub source_ids: Vec<String>,
    pub model_generated: bool,
    pub user_corrected: bool,
    pub human_labeled: bool,
    pub synthetic: bool,
    pub created_at: DateTime<Utc>,
}
```

```rust
pub enum ProvenanceOrigin {
    ProductionInteraction,
    GoldenScenario,
    Replay,
    HumanLabel,
    SyntheticGeneration,
    ToolGroundTruth,
    TaskOutcome,
}
```

没有 provenance 的数据：

```text
不能进入正式训练集
```

---

# 18. PrivacyFilter

Candidate 进入 ExperiencePool 前：

必须经过：

```text
PrivacyFilter
```

沿用 V5：

```text
Public
Internal
Personal
Sensitive
LocalOnly
```

规则：

```text
Public
→ 可按 policy 进入共享训练

Internal
→ 受部署 policy 控制

Personal
→ 默认只用于 Memory / 用户级适应

Sensitive
→ 默认禁止进入模型训练

LocalOnly
→ 只能本地保存 / 本地训练
```

---

# 19. 用户 Memory 不等于训练集

强规则：

```text
“Yunxi 记住了用户的一件私事”
```

不等于：

```text
“把这件私事写入共享模型权重”
```

共享模型训练：

默认排除个人私密事实。

---

# 20. ExperiencePool

按 Role 分区：

```text
SemanticPool
WorldExtractionPool
PlannerPool
DialoguePool
RetrievalPool
SafetyPool
```

不要：

```text
Semantic 样本
```

直接拿去训练 Dialogue。

---

# 21. DataQualityScorer

```rust
pub struct DataQualityScore {
    pub correctness: f32,
    pub confidence: f32,
    pub novelty: f32,
    pub relevance: f32,
    pub grounding: f32,
    pub privacy_risk: f32,
}
```

优先高质量 Grounding：

```text
Tool Ground Truth
Task Objective Result
Explicit User Correction
Human Label
Golden Scenario
```

低质量：

```text
model self-generated opinion
unverified speculation
```

---

# 22. Deduplication

必须支持：

```text
exact dedupe
semantic dedupe
near-duplicate dedupe
```

同一错误出现 100 次：

不应保存 100 个完全相同样本。

更合理：

```text
one canonical sample
+
frequency metadata
```

---

# 23. CounterexampleBuilder

高质量学习不仅需要正确答案，还需要：

```text
Preferred
Rejected
ReasonTag
```

例如：

```text
Input:
用户关闭主动联系

Preferred:
Silent

Rejected:
Proactive message

Reason:
USER_DISABLED_PROACTIVE
```

不需要保存 Chain-of-Thought。

---

# 24. HardNegativeMiner

适用于：

```text
Semantic
Retrieval
Tool Selection
Affordance Selection
```

例如：

```text
看起来非常相似
但实际不相关
```

的 Memory，可作为 Retrieval Hard Negative。

---

# 25. DatasetCurator

负责：

```text
ExperiencePool
→ immutable DatasetVersion
```

---

# 26. DatasetManifest

```rust
pub struct DatasetManifest {
    pub id: DatasetId,
    pub role: ModelRole,
    pub version: DatasetVersion,
    pub source_pools: Vec<PoolId>,
    pub sample_count: usize,
    pub privacy_policy_version: u64,
    pub curator_version: String,
    pub created_at: DateTime<Utc>,
}
```

---

# 27. Dataset Split

至少：

```text
Train
Validation
Holdout
```

推荐：

```text
temporal split
```

避免同一事件近重复样本同时进入训练和评测。

---

# 28. Evaluation Leakage Prevention

V9 Golden / Holdout：

必须标：

```text
training_forbidden
```

不能为了训练效果：

```text
把评测答案喂回训练集
```

---

# 29. Production Incident

一次线上事故可以同时产生：

```text
LearningCandidate
+
RegressionScenario
```

但 RegressionScenario 本身：

```text
不能直接进入 Train Split
```

---

# 30. TrainingRecipe

```rust
pub struct TrainingRecipe {
    pub id: TrainingRecipeId,
    pub target_role: ModelRole,
    pub base_model: ModelArtifactId,
    pub method: TrainingMethod,
    pub dataset: DatasetId,
    pub hyperparameters: TrainingHyperparameters,
    pub budget: TrainingBudget,
}
```

---

# 31. TrainingMethod

```rust
pub enum TrainingMethod {
    Sft,
    Lora,
    QLora,
    Preference,
    EmbeddingFineTune,
    RerankerFineTune,
}
```

---

# 32. 推荐训练顺序

```text
1. Semantic Understanding
2. World Extraction
3. Embedding / Reranking
4. Planner
5. Dialogue
```

Semantic 最适合作为第一批：

```text
结构化
容易评测
训练成本低
边界清晰
能显著降低云 Token
```

Dialogue 最后，因为它对：

```text
人格
风格
工具调用习惯
长期一致性
```

影响最大。

---

# 33. TrainingJob

```rust
pub struct TrainingJob {
    pub id: TrainingJobId,
    pub recipe_id: TrainingRecipeId,
    pub state: TrainingJobState,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub output_artifact: Option<ModelArtifactId>,
}
```

```rust
pub enum TrainingJobState {
    Queued,
    Preparing,
    Running,
    EvaluatingTrainingMetrics,
    Completed,
    Failed,
    Cancelled,
}
```

TrainingJob：

```text
必须接 V6 TaskSupervisor
```

训练时 Production Conversation 继续运行。

---

# 34. TrainingScheduler

训练不是：

```text
1 条新数据
→ 训练一次
```

触发条件可以是：

```text
minimum sample count
minimum quality
scheduled interval
manual trigger
incident threshold
role-specific threshold
```

---

# 35. TrainingBudget

```rust
pub struct TrainingBudget {
    pub max_gpu_hours: f32,
    pub max_cost: Option<f64>,
    pub max_dataset_samples: usize,
    pub max_training_runs_per_day: u32,
}
```

必须避免：

```text
LearningSignal storm
→ Training storm
```

每个 Role 应有：

```text
cooldown
```

---

# 36. ModelCandidate

训练完成后：

```rust
pub struct ModelCandidate {
    pub artifact_id: ModelArtifactId,
    pub base_model: ModelArtifactId,
    pub dataset_id: DatasetId,
    pub recipe_id: TrainingRecipeId,
    pub training_lineage: TrainingLineage,
}
```

Candidate 不允许直接上线。

必须：

```text
Offline Eval
↓
V9 Golden Suite
↓
Safety Suite
↓
Autonomy Suite
↓
Cost / Latency
↓
Shadow
↓
Canary
↓
Production
```

---

# 37. TrainingLineage

必须记录：

```text
base model
dataset id/hash
recipe
parent adapter
hyperparameters
training code version
random seed
hardware metadata
```

目标：

```text
lineage reproducible
```

---

# 38. AdapterArtifact

推荐：

```text
Base Model
+
LoRA / QLoRA Adapter
```

而不是每次覆盖 Base Model。

优点：

```text
回滚快
多角色 specialization
artifact 小
版本清晰
```

---

# 39. Role-specific Adapter

未来可以：

```text
Semantic Adapter
WorldExtraction Adapter
Planner Adapter
Dialogue Adapter
```

而不是一个 Adapter 同时承担所有能力。

---

# 40. Catastrophic Forgetting

持续训练必须防：

```text
灾难性遗忘
```

需要：

```text
Replay Buffer
Base Skill Dataset
Regression Dataset
```

每次训练：

```text
new data
+
representative replay data
```

---

# 41. Base Capability Set

例如 Semantic 必须长期保持：

```text
Stop Intent
No Reply Intent
Task Status Intent
Image Intent
Conversation Relevance
```

新数据不能让这些旧能力退化。

---

# 42. Drift Detection

监控：

```text
personality drift
tool overuse
proactive spam
risk tolerance drift
silent rate drift
confirmation drift
```

建议指标：

```text
ActionRateDelta
ToolCallRateDelta
ProactiveRateDelta
SilentRateDelta
ConfirmationRateDelta
StyleEmbeddingDelta
SafetyRateDelta
```

超过阈值：

```text
reject candidate
```

---

# 43. Runtime Policy Adaptation

不是所有问题都需要训练模型。

例如：

```text
主动消息太频繁
```

优先：

```text
adjust proactive threshold
```

而不是：

```text
fine-tune Dialogue
```

---

# 44. RuntimeTuner

可调整：

```text
proactive threshold
memory top_k
semantic threshold
world refresh interval
reflection trigger threshold
router preference
```

所有自动调参必须：

```text
bounded
versioned
reversible
evaluated
```

---

# 45. AdaptationPolicy

例如：

```text
proactive_threshold ∈ [0.70, 0.95]
```

RuntimeTuner 不能自动调成：

```text
0.01
```

参数变化后：

```text
V9 eval / online metrics
```

若变差：

```text
rollback parameter set
```

---

# 46. Personal Adaptation

用户个性化优先使用：

```text
Memory
Preference
Relation
Policy
Retrieval
```

不推荐早期做：

```text
Per-user Model Fine-tune
```

原因：

```text
数据少
过拟合
成本高
隐私风险
版本管理复杂
```

---

# 47. Future Personal Adapter

未来如果确实需要：

```text
local personal LoRA
```

必须：

```text
LocalOnly
user-specific
never merged into shared model
```

---

# 48. Synthetic Data

允许生成：

```text
Golden variations
Semantic edge cases
Tool schemas
World state combinations
```

但必须：

```text
synthetic = true
```

且不能压过真实 Ground Truth。

---

# 49. Self-generated Data Risk

禁止：

```text
current model
→ 生成 100 万条答案
→ 下一版模型全拿这些训练
```

这容易：

```text
放大偏差
放大幻觉
降低多样性
产生 model collapse
```

---

# 50. Teacher / Distillation

可以：

```text
strong model
→ curated outputs
→ local small model
```

但 Teacher 必须：

```text
versioned
evaluated
filtered
```

Distillation Dataset：

仍然经过 V10 Data Quality 与 V9 Safety。

---

# 51. Preference Learning

用户明确：

```text
更喜欢 A
不喜欢 B
```

可以形成：

```text
Preferred / Rejected
```

但个人偏好默认：

```text
只更新 PreferenceState
```

不能自动训练共享 Dialogue Model。

---

# 52. Planner Learning

Planner Dataset：

```text
WorldState
Goal
Affordances
Chosen Action
Outcome
Failure Attribution
```

---

# 53. Failure Attribution

失败不一定是 Planner 错。

```rust
pub enum FailureAttribution {
    Planner,
    Tool,
    Environment,
    Permission,
    StaleState,
    Model,
    Unknown,
}
```

例如：

```text
Planner 选对 Tool
但网络断了
```

应：

```text
Environment
```

不能把正确 Planner Action 标成负样本。

---

# 54. Unknown

原因不明确：

```text
Unknown
```

不要强行构造错误标签。

---

# 55. Semantic Learning

第一阶段重点。

形式：

```text
Message
→ Structured Semantic JSON
```

Ground Truth：

```text
User Correction
Rule-derived label
Manual Label
Task Outcome
```

高置信 Candidate：

可自动进入 Pool。

低置信：

```text
Review Queue
```

---

# 56. World Extraction Learning

形式：

```text
Observation
→ Structured World Facts
```

必须保留：

```text
Known
Unknown
Hypothesis
```

训练不能奖励：

```text
无证据时强行猜测
```

---

# 57. Retrieval / Reranker Learning

从：

```text
retrieved memory
user correction
task success
```

生成：

```text
query
candidate memories
positive / negative
ranking
```

Hard Negative：

尤其重要。

---

# 58. Embedding Fine-tune

Embedding Model 更新：

```text
vector space changes
```

必须遵循 V5：

```text
embedding_version
reindex
dual-index migration
```

不同版本向量：

```text
不能直接混合比较
```

---

# 59. Dialogue Learning

最谨慎。

优先：

```text
high-quality edited samples
persona golden scenarios
style dataset
```

禁止：

```text
把用户 Memory 直接当 Dialogue SFT 数据
```

否则容易：

```text
把私人事实学成全局人格知识
```

---

# 60. LearningJournal

```rust
pub struct LearningJournalEntry {
    pub id: LearningJournalId,
    pub event: LearningJournalEvent,
    pub dataset_id: Option<DatasetId>,
    pub training_job_id: Option<TrainingJobId>,
    pub model_artifact_id: Option<ModelArtifactId>,
    pub created_at: DateTime<Utc>,
}
```

事件：

```text
SignalCreated
CandidateCreated
CandidateRejected
CandidateApproved
DatasetBuilt
TrainingStarted
TrainingFailed
TrainingCompleted
EvalFailed
ShadowStarted
CanaryStarted
CandidatePromoted
CandidateRolledBack
```

---

# 61. Dataset Governance

Dataset 自身也要有：

```text
owner
privacy policy
retention
allowed training role
export permission
hash
```

若某条数据以后需要删除：

```text
必须从未来 Dataset 排除
```

第一版不承诺：

```text
已经训练进权重的数据可以完整 machine-unlearn
```

因此训练前治理必须严格。

---

# 62. Auto-Learning Governance

V10 可以自动：

```text
收集 LearningSignal
生成 Candidate
Privacy Filter
Quality Filter
Dedupe
构建 Dataset
排队 TrainingJob
```

但默认不自动：

```text
Promote to Production
```

---

# 63. Strict Auto Promotion

未来如果开启：

必须满足：

```text
all V9 blocking suites pass
no safety regression
no autonomy regression
cost within budget
latency within budget
Shadow healthy
Canary healthy
```

---

# 64. V9 Kill Switch

V9 KillSwitch 应可以：

```text
stop new training
stop auto adaptation
stop candidate deployment
```

---

# 65. Training Environment Isolation

训练环境不得直接：

```text
访问 production secrets
访问未脱敏私聊
执行 production tools
```

训练前：

```text
materialize immutable dataset artifact
```

---

# 66. Model Registry Integration

V5 Model Registry 增加：

```text
training lineage
dataset id
evaluation artifact
promotion state
```

```rust
pub enum PromotionState {
    Candidate,
    Evaluated,
    Shadow,
    Canary,
    Production,
    Rejected,
    RolledBack,
}
```

---

# 67. Rollback

必须支持：

```text
Production Adapter A
→ rollback Adapter B
```

模型回滚：

```text
不回滚用户 Memory
```

除非 Memory schema 自身存在问题。

---

# 68. V10 与 V1～V9 的关系

## V1 Core

提供：

```text
Event / Memory / Action
```

V10 从中提取学习信号。

## V2 Mind

是即时学习主要位置。

## V3 Executive

Planner / Executive 可以成为训练目标。

## V4 World Model

Prediction 可以形成：

```text
PredictionCorrect
PredictionWrong
```

## V5 Model Fabric

负责：

```text
模型后端
模型注册
训练 Artifact
推理路由
```

V10 负责：

```text
为什么训练
拿什么训练
训练哪个 Role
如何生成 Candidate
```

## V6 Runtime

TrainingJob：

走 TaskSupervisor。

## V7 Perception

Observation：

提供真实 Outcome / Ground Truth。

## V8 Cognitive I/O

ActionSelection / Affordance：

提供 Planner 学习数据。

## V9 Governance

最重要关系：

```text
V10 负责“学”
V9 负责“能不能上线”
```

V10 永远不能绕过 V9。

---

# 69. Phase 0：Learning Domain Types

实现：

```text
LearningSignal
LearningCandidate
DataProvenance
DataQualityScore
DatasetManifest
DatasetVersion
TrainingRecipe
TrainingJob
ModelCandidate
TrainingLineage
```

---

# 70. Phase 1：Signal Collector

先接：

```text
UserCorrection
TaskSuccess
TaskFailure
EvaluationFailure
```

---

# 71. Phase 2：Candidate Builder

从 Signal：

```text
生成最小充分 LearningCandidate
```

---

# 72. Phase 3：PrivacyFilter

实现：

```text
Public
Internal
Personal
Sensitive
LocalOnly
```

---

# 73. Phase 4：ExperiencePool

按 Role 分池。

---

# 74. Phase 5：Dedupe / Quality

实现：

```text
exact dedupe
semantic dedupe
quality scoring
```

---

# 75. Phase 6：DatasetCurator

生成：

```text
immutable DatasetVersion
Train / Validation / Holdout
```

---

# 76. Phase 7：Semantic Dataset

建立第一批正式持续学习数据。

---

# 77. Phase 8：TrainingJob

接：

```text
V6 TaskSupervisor
```

---

# 78. Phase 9：LoRA / QLoRA Pipeline

优先：

```text
本地小模型
```

---

# 79. Phase 10：V9 Evaluation Integration

Candidate：

自动进入：

```text
RegressionSuite
```

---

# 80. Phase 11：Shadow

候选模型：

```text
zero real side effects
```

---

# 81. Phase 12：Canary

小比例生产流量。

---

# 82. Phase 13：Rollback

支持：

```text
model / adapter rollback
```

---

# 83. Phase 14：RuntimeTuner

先支持：

```text
proactive threshold
memory top_k
reflection threshold
```

---

# 84. Phase 15：World Extraction Training

建立结构化 Dataset。

---

# 85. Phase 16：Reranker Training

训练 Memory Reranker。

---

# 86. Phase 17：Planner Dataset

加入：

```text
Action
Outcome
Failure Attribution
```

---

# 87. Phase 18：Planner Fine-tune

必须在：

```text
Semantic / Extraction
```

稳定后进行。

---

# 88. Phase 19：Dialogue Candidate Pipeline

先建立：

```text
data
evaluation
drift metrics
```

不急着自动训练。

---

# 89. Phase 20：Auto Training Scheduler

可以自动：

```text
排队训练
```

但部署仍走 V9。

---

# 90. Golden Learning Scenario A：用户纠正

```text
Yunxi:
“你的面试是周二。”

User:
“不是，是周三。”
```

预期：

```text
Memory immediately corrected
LearningSignal created
Candidate created
No immediate model training
```

---

# 91. Scenario B：模型自己生成事实

没有外部验证。

预期：

```text
not strong training label
```

---

# 92. Scenario C：环境失败

Planner 选择正确 Tool。

网络断开。

预期：

```text
FailureAttribution = Environment
Planner not marked negative
```

---

# 93. Scenario D：Planner 选错动作

导致任务失败。

预期：

```text
FailureAttribution = Planner
negative candidate
```

---

# 94. Scenario E：Private Memory

私聊产生：

```text
Personal
```

预期：

```text
Memory allowed
Shared training forbidden
```

---

# 95. Scenario F：LocalOnly

预期：

```text
local training allowed by policy
remote export forbidden
```

---

# 96. Scenario G：Duplicate Corrections

同一 Semantic 错误 100 次。

预期：

```text
canonical sample
frequency metadata
```

而不是 100 份重复数据。

---

# 97. Scenario H：Golden Leakage

Golden Eval Case：

```text
training_forbidden
```

---

# 98. Scenario I：Semantic Regression

Candidate：

```text
intent accuracy +3%
Stop Intent -5%
```

预期：

```text
reject
```

---

# 99. Scenario J：Local Model Cost Win

```text
quality ≈ same
latency lower
cloud token -80%
```

预期：

```text
promotion favored
```

---

# 100. Scenario K：Dialogue Drift

```text
style improved
tool overuse +30%
```

预期：

```text
reject
```

---

# 101. Scenario L：Training Storm

大量 Signal 突发。

预期：

```text
batch
dedupe
bounded TrainingJobs
```

---

# 102. Scenario M：Runtime Adaptation

Proactive Precision 下降。

优先：

```text
adjust threshold
```

不是：

```text
train Dialogue
```

---

# 103. Scenario N：Adaptation Rollback

参数调整后质量变差。

预期：

```text
rollback parameter set
```

---

# 104. Testing Strategy

必须覆盖：

```text
unit
property
integration
privacy
dataset
training
replay
regression
drift
rollback
```

---

# 105. Property：No Golden Leakage

任何：

```text
training_forbidden
```

样本：

永远不能进入 Train Split。

---

# 106. Property：No Self-training Loop

未经外部验证的 Model Output：

不能自动成为强 Positive Label。

---

# 107. Property：V9 Promotion Required

任何 ModelCandidate：

不能直接：

```text
Candidate → Production
```

必须经过 V9 promotion。

---

# 108. Privacy Test

Personal / Sensitive Candidate：

必须：

```text
reject / local-only
```

符合 Policy。

---

# 109. Training Test

固定小 Dataset：

必须能完整产生：

```text
Model Artifact
Training Lineage
Metrics
Evaluation Request
```

---

# 110. Rollback Test

Candidate 上线后指标恶化：

```text
rollback
```

---

# 111. Drift Test

多轮持续训练后：

```text
base capabilities
```

不得持续下降。

---

# 112. Dataset Drift

监控：

```text
category distribution
source distribution
privacy distribution
role distribution
```

避免数据越来越偏。

---

# 113. LearningSnapshot

```rust
pub struct LearningSnapshot {
    pub pending_signals: usize,
    pub pending_candidates: usize,
    pub experience_pool_size: usize,
    pub active_training_jobs: usize,
    pub candidate_models: usize,
    pub last_promoted_model: Option<ModelArtifactId>,
    pub last_training_at: Option<DateTime<Utc>>,
}
```

---

# 114. Control Center

未来可展示：

```text
Learning Signals
Candidate Queue
Experience Pools
Dataset Versions
Training Jobs
Model Candidates
Evaluation Results
Shadow Status
Canary Status
Rollback History
Runtime Adaptation
```

---

# 115. Reason Tags

```text
USER_CORRECTION
TASK_SUCCESS
TASK_FAILURE
PLANNER_FAILURE
ENVIRONMENT_FAILURE
TOOL_FAILURE
PRIVACY_REJECT
PERSONAL_NOT_TRAINABLE
SENSITIVE_NOT_TRAINABLE
LOCAL_ONLY
DUPLICATE_SAMPLE
LOW_QUALITY
GOLDEN_TRAINING_FORBIDDEN
MODEL_SELF_LABEL_REJECT
TRAINING_BUDGET_EXCEEDED
CANDIDATE_EVAL_FAILED
CANDIDATE_DRIFT_REJECTED
SHADOW_FAILED
CANARY_FAILED
MODEL_PROMOTED
MODEL_ROLLED_BACK
RUNTIME_ADAPTATION_APPLIED
RUNTIME_ADAPTATION_ROLLED_BACK
```

---

# 116. Metrics

```text
yunxi_learning_signal_total
yunxi_learning_signal_user_correction_total
yunxi_learning_candidate_total
yunxi_learning_candidate_rejected_total
yunxi_learning_privacy_reject_total
yunxi_learning_dataset_samples
yunxi_learning_dataset_version_total
yunxi_learning_training_job_total
yunxi_learning_training_failed_total
yunxi_learning_gpu_hours
yunxi_learning_candidate_eval_pass_rate
yunxi_learning_model_promoted_total
yunxi_learning_model_rollback_total
yunxi_learning_runtime_adaptation_total
```

---

# 117. Definition of Done

V10 完成必须满足：

```text
[ ] 学习明确分成 State / Policy / Model 三层
[ ] 用户纠正先修当前状态，再产生 LearningSignal
[ ] Model Self Output 不自动成为强训练标签
[ ] LearningSignal 与 Dataset 分离
[ ] LearningCandidate 有 provenance
[ ] LearningCandidate 有 privacy_class
[ ] Personal 默认不进入 shared training
[ ] Sensitive 默认禁止训练
[ ] LocalOnly 不允许远端导出
[ ] ExperiencePool 按 ModelRole 分区
[ ] Dedupe 支持 exact / semantic
[ ] Dataset immutable / versioned
[ ] Train / Validation / Holdout 分离
[ ] Golden Eval 数据禁止进入训练
[ ] TrainingRecipe 可追踪
[ ] TrainingJob 走 TaskSupervisor
[ ] LoRA / QLoRA Pipeline 可运行
[ ] ModelCandidate 有完整 lineage
[ ] Candidate 必须经过 V9 Evaluation
[ ] Shadow 无真实副作用
[ ] Canary 可逐步上线
[ ] Rollback 可恢复旧模型 / Adapter
[ ] Catastrophic forgetting 有 Replay Set 防护
[ ] Drift metrics 存在
[ ] RuntimeTuner bounded / reversible
[ ] 自动训练有 budget / cooldown
[ ] 自动训练不能绕过 V9 promotion
[ ] Failure Attribution 不把环境错误错标成 Planner 错误
[ ] Embedding 升级遵守 V5 vector-space migration
[ ] LearningJournal 可完整追踪
[ ] Dataset 有 hash / provenance / privacy policy version
[ ] V1～V9 行为保持兼容
```

---

# 118. V1～V10 最终分工

```text
V1 Core
= 平台无关 Agent 生命循环

V2 Mind
= 自我、信念、兴趣与内部议程

V3 Executive
= 注意力、目标、冲突、计划和决策控制

V4 World Model
= 外部世界状态估计、预测和模拟

V5 Model Fabric
= 本地 / 云端 / 多模型推理基础设施

V6 Runtime Foundation
= 多任务、多通道、Action 生命周期、恢复和降级

V7 Perception–Action Loop
= 世界感知、行动反馈、时间和任务驱动闭环

V8 Affordance & Cognitive I/O Protocol
= 外部环境动态发布 Context、当前动作空间和 DecisionRequest

V9 Evaluation & Autonomy Governance
= 行为评测、自主权限、回归和发布治理

V10 Learning & Adaptation
= 从经验中沉淀状态、调整策略、构建训练数据并安全训练新模型
```

---

# 119. V10 最终学习闭环

```text
World / User / Task
↓
V7 Observation
↓
V1～V4 State
↓
V3 Decision
↓
V8 Action Selection
↓
V9 Governance
↓
V6 Execution
↓
Outcome
↓
LearningSignal
↓
V10 Candidate / Experience
↓
Dataset
↓
Training
↓
Candidate Model
↓
V9 Evaluation
↓
Shadow
↓
Canary
↓
Production
```

---

# 120. 最终设计原则

> **学到东西，不等于必须改权重。**

> **用户纠正优先修 Memory / Belief，而不是立刻 Fine-tune。**

> **模型自己的输出不能自动变成训练真值。**

> **训练数据必须有来源、有质量、有隐私等级。**

> **共享模型不应该记住某个用户的私人生活。**

> **低成本问题优先通过 Policy 和 Runtime 参数适应解决。**

> **结构化 ModelRole 比 Dialogue 更适合早期持续训练。**

> **训练出来的模型必须先成为 Candidate，而不是直接成为 Production。**

> **V10 负责学习，V9 负责决定新模型能不能上线。**

> **持续学习不是无限训练，而是持续积累高质量证据。**

> **如果一个新模型无法通过回归、Shadow 和 Canary，它就不应该因为“已经训练完了”而被部署。**

---

# 121. 结论

完成 V10 后，Yunxi 将拥有三种不同速度的学习：

```text
秒级
→ Memory / Belief / Preference 更新

小时～天级
→ Runtime Policy / Threshold 适应

天～周级
→ Dataset → Training → Candidate → Eval → Deploy
```

这比：

```text
每条消息后在线 Fine-tune
```

更加稳定，也更符合一个长期运行 Agent 的工程现实。

Yunxi 会从：

```text
能记住
能思考
能行动
能评测
```

进一步变成：

```text
能从长期经验中安全地变得更好
```

V10 的成功标准不是：

```text
“模型每天都在训练。”
```

而是：

> **真正有价值的经验能够被识别、保留、转化为高质量学习数据，并且只有经过 V9 治理验证的新能力才会进入生产系统。**
