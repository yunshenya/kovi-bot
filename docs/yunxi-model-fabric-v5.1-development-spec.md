# Yunxi Model Fabric v5：本地模型、多模型路由与可替换推理基础设施需求文档

**文档版本：** 5.0  
**适用项目：** Yunxi Core / Yunxi Mind / Yunxi Executive / Yunxi World Model  
**主要语言：** Rust  
**前置依赖：** Yunxi Core v1、Yunxi Mind v2、Yunxi Executive v3、Yunxi World Model v4 已基本稳定  
**文档定位：** 将模型能力从具体云 API、具体供应商和具体推理框架中彻底解耦，建立统一 Model Fabric，使 Yunxi 可以在云模型、本地模型、自有微调模型、Embedding 模型和轻量分类模型之间按角色路由，并为未来完全离线运行提供基础。

---

# 1. 总目标

v1：持续存在与行动。  
v2：持续的内部心智状态。  
v3：执行控制、优先级、不确定性与计划。  
v4：外部世界状态、预测与有限模拟。  
v5：

> **让模型成为 Yunxi 可替换的计算资源，而不是 Yunxi 本身。**

最终：

```text
Core / Mind / Executive / World Model
                 │
                 ▼
            Model Fabric
       ┌─────────┼─────────┐
       ▼         ▼         ▼
     Cloud      Local     Custom
```

模型更换不能导致人格、记忆、关系、Goal、OpenLoop、Belief、World Model 或 Executive State 丢失。

---

# 2. v5 不是第五层认知系统

v5 是横向基础设施，而不是：

```text
Core → Mind → Executive → World Model → SuperMind
```

正确：

```text
             Mind ─────┐
                       │
World Model ───────────┼──→ Executive / Planner
                       │
             Core ─────┘
                       │
                       ▼
                  Model Fabric
```

Model Fabric 为所有认知模块提供推理能力。

---

# 3. 必须支持的长期运行模式

## Cloud Mode

主要使用云模型。

## Hybrid Mode

例如：

```text
Semantic      → Local Small
WorldExtract  → Local Small
Embedding     → Local
Reflection    → Local Medium
Planner       → Local Strong / Cloud fallback
Dialogue      → Local Strong / Cloud
```

## Fully Local Mode

所有核心模型请求都在本机或可信私有服务器运行。

断开公网后，核心功能仍可工作。

---

# 4. 最高架构边界

以下模块不得知道具体供应商：

- Yunxi Core
- Yunxi Mind
- Yunxi Executive
- Yunxi World Model

禁止在这些层硬编码：

```text
OpenAI
Anthropic
Gemini
DeepSeek
Ollama
llama.cpp
vLLM
SGLang
MLX
```

也禁止认知层直接：

```rust
reqwest::Client
```

调用 `/v1/...`。

所有具体协议必须封装在 Model Backend / Adapter。

---

# 5. 建议 crate

```text
crates/
├── yunxi-model-api/
├── yunxi-model-router/
├── yunxi-model-openai-compatible/
├── yunxi-model-local/
├── yunxi-embedding/
└── yunxi-model-eval/
```

实际可根据现有 workspace 合并，但职责必须保留。

---

# 6. ModelRole

至少：

```rust
pub enum ModelRole {
    SemanticUnderstanding,
    AttentionAssist,
    Planner,
    Dialogue,
    Reflection,
    WorldExtraction,
    Simulation,
    Summarization,
    Embedding,
    Reranking,
}
```

Role 不等于 Model Name。

必须：

```text
Planner
→ Router
→ Backend
```

而不是：

```text
Planner = 某个具体模型
```

---

# 7. Model Backend 抽象

建议：

```rust
#[async_trait]
pub trait TextGenerationBackend: Send + Sync {
    async fn infer(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, ModelError>;

    fn capabilities(&self) -> ModelCapabilities;
    fn backend_id(&self) -> BackendId;
}
```

Embedding / Reranker / Vision 如果接口差异明显，应拆 trait。

不要做 God Trait。

---

# 8. InferenceRequest

建议至少：

```rust
pub struct InferenceRequest {
    pub role: ModelRole,
    pub messages: Vec<ModelMessage>,
    pub structured_output: Option<SchemaSpec>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub timeout: Duration,
    pub trace: TraceContext,
    pub privacy: PrivacyClass,
}
```

---

# 9. InferenceResponse

```rust
pub struct InferenceResponse {
    pub content: ModelOutput,
    pub usage: ModelUsage,
    pub backend_id: BackendId,
    pub model_id: ModelId,
    pub latency_ms: u64,
    pub finish_reason: FinishReason,
}
```

统一记录：

- input tokens
- output tokens
- cached tokens（如可用）
- latency
- backend
- model
- role

本地模型也要记录。

---

# 10. PrivacyClass

建议：

```rust
pub enum PrivacyClass {
    Public,
    Internal,
    Personal,
    Sensitive,
    LocalOnly,
}
```

其中：

```text
LocalOnly
```

必须是硬约束。

如果本地模型不可用：

返回 `NoEligibleBackend`。

禁止偷偷 fallback 到云端。

---

# 11. Model Router

Router 根据：

```text
Role
Capability
Privacy
Latency
Quality
Budget
Backend Health
```

选择模型。

建议：

```rust
pub struct RoutingContext {
    pub role: ModelRole,
    pub required_capabilities: ModelCapabilities,
    pub privacy: PrivacyClass,
    pub latency_class: LatencyClass,
    pub quality_class: QualityClass,
    pub budget_class: BudgetClass,
}
```

---

# 12. Latency / Quality / Budget

可定义：

```text
Latency:
Realtime
Interactive
Background
Batch

Quality:
Fast
Balanced
High
Critical

Budget:
FreePreferred
LowCost
Balanced
QualityFirst
```

---

# 13. 示例 Routing Policy

```text
SemanticUnderstanding
→ Local Small

Planner
→ Local Strong
→ Cloud Strong fallback

Dialogue
→ configured Main Model

Reflection
→ Local Background

Embedding
→ Local Only

WorldExtraction
→ Local Small

Simulation
→ Local Fast
→ optional remote fallback
```

Router 必须由 Rust Policy 控制。

不能让 LLM 自己决定“我要调用哪个模型”。

---

# 14. Backend Health

状态至少：

```text
Healthy
Degraded
Unavailable
CoolingDown
```

依据：

- timeout rate
- error rate
- queue depth
- latency
- OOM
- model loaded state
- VRAM pressure（如果 Runtime 可提供）

---

# 15. Circuit Breaker

连续失败：

```text
Healthy
→ Degraded
→ CoolingDown
```

防止所有请求反复撞坏后端。

---

# 16. Fallback

Fallback 必须：

- 明确配置；
- 尊重 PrivacyClass；
- 有最大层数；
- 有 trace。

例如：

```text
Local Strong
→ Cloud Strong
→ Local Small
```

但：

```text
LocalOnly
→ Cloud
```

绝对禁止。

---

# 17. Role-specific Failure Policy

Direct Dialogue：
可 fallback。

Reflection：
可以 defer。

Embedding：
可以暂时退化为 keyword retrieval。

Simulation：
失败直接 skip。

World Extraction：
使用 deterministic / existing semantic fallback。

---

# 18. 第一版本地模型接入

第一版优先实现：

```text
LocalOpenAICompatibleBackend
```

连接：

```text
http://127.0.0.1:<port>/v1
```

这样未来可替换多个本地推理 runtime，而不绑定一个产品。

Core 不知道本地运行器是什么。

---

# 19. 后续 Runtime Adapter

未来可选增加：

```text
Ollama Adapter
llama.cpp Adapter
vLLM Adapter
SGLang Adapter
MLX Adapter
```

这些都属于 infrastructure。

---

# 20. Model Registry

维护：

```text
BackendId
ModelId
Capabilities
ContextWindow
MaxOutputTokens
Locality
Quantization
Health
Version
```

建议：

```rust
pub struct ModelDescriptor {
    pub id: ModelId,
    pub backend_id: BackendId,
    pub capabilities: ModelCapabilities,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub locality: ModelLocality,
}
```

---

# 21. ModelLocality

```text
Local
PrivateNetwork
RemoteCloud
```

未来局域网 GPU 服务器属于：

```text
PrivateNetwork
```

是否可信由配置策略决定。

---

# 22. Capability

至少考虑：

```text
TextGeneration
StructuredJson
ToolCalling
Vision
LongContext
Streaming
Embedding
Reranking
```

Router 必须先做 capability filter。

需要 `ToolCalling + StructuredJson` 时，不能选择不支持的后端。

---

# 23. 多模型角色拆分

不要默认一个大模型包打天下。

推荐：

```text
Semantic / Extraction → 小模型
Planner               → 中/大模型
Dialogue              → 中/大模型
Reflection            → 中模型/后台模型
Embedding             → 专用模型
Reranker              → 专用模型
```

---

# 24. 哪些任务适合本地小模型

- stop intent
- future-event extraction
- topic classification
- MessageUnderstanding
- image intent
- memory classification
- World Observation extraction
- simple Attention assist
- structured tagging

这些任务格式稳定、容易评测，优先本地化。

---

# 25. 哪些任务需要更强模型

- nuanced dialogue
- complex planning
- high-conflict belief revision
- difficult reflection
- high-value simulation
- complex multi-goal decision

---

# 26. Embedding Backend

独立抽象：

```rust
pub trait EmbeddingBackend {
    async fn embed(
        &self,
        texts: &[String],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingError>;
}
```

---

# 27. Embedding Version

必须记录：

```text
embedding_model_id
embedding_version
```

不同模型向量空间禁止混用。

---

# 28. 更换 Embedding Model

必须支持：

```text
background re-embedding
versioned index
incremental migration
optional dual-index read
```

不能直接把旧向量当新向量使用。

---

# 29. Vector Store 不写死

v5 不强制：

```text
pgvector
```

可以由 Storage 层选择。

---

# 30. Reranker

可增加 `RerankerBackend`。

用于：

- Memory retrieval
- Belief retrieval
- WorldModel retrieval

第一版非必须。

---

# 31. Prompt Registry

Prompt 不应散落在大量 Rust 字符串里。

建立：

```text
PromptRegistry
```

至少记录：

```text
prompt_id
prompt_version
role
```

例如：

```text
semantic_v1
planner_v3
world_extract_v2
reflection_v1
dialogue_v4
```

---

# 32. Persona 与 Prompt

SelfModel / Values / Mind State：

来自 v2。

不要把 Yunxi 的全部人格只存在静态 system prompt。

否则换 Prompt 就像换人格。

---

# 33. ContextBuilder

为每个 Role 构建最小必要 Context。

例如：

Semantic：

不需要完整 SelfModel。

Embedding：

不需要 Persona。

WorldExtraction：

不需要完整 Relation。

Dialogue：

才使用更多 Mind / World / Executive context。

---

# 34. Context Budget

每 Role 设置：

```text
max_context_tokens
```

并定义：

```text
MustInclude
Relevant
Optional
DropFirst
```

---

# 35. Context Overflow

禁止：

```text
直接截掉最前面的消息
```

应优先：

1. 保留系统硬约束；
2. 保留 current event；
3. 保留 active goal / open loop；
4. 保留 relevant memory；
5. 压缩低优先上下文。

---

# 36. Summarization Role

长期 conversation 可使用：

```text
ModelRole::Summarization
```

由低成本/本地后台模型处理。

---

# 37. Structured Output

v2-v4 大量依赖结构化输出。

必须统一：

```text
SchemaSpec
```

模型返回：

```text
parse
→ validate
→ accept / repair / reject
```

---

# 38. Schema Repair

invalid JSON：

允许有限 repair。

建议：

```text
max repair = 1
```

禁止无限修复循环。

---

# 39. Tool Calling

内部统一为：

```text
ToolCallProposal
```

模型永远不能直接执行 Tool。

仍经过：

```text
Action / Tool Runtime / Arbiter
```

---

# 40. Streaming

Dialogue Backend 可支持 streaming。

但 Core 不能依赖 streaming 才能正常工作。

未来 Voice / Desktop 可以利用 streaming 降低体感延迟。

---

# 41. Cancellation

必须支持尽可能完整的取消传播。

例如：

```text
ReplyTicket stale
→ cancel model generation
```

避免 stale reply 继续占 GPU。

建议使用：

```text
CancellationToken
```

或当前项目等价机制。

---

# 42. Inference Scheduler

本地 GPU 是共享稀缺资源。

需要统一：

```text
InferenceScheduler
```

而不是各模块各自无限调用模型。

---

# 43. Scheduler Priority

建议：

```text
Critical:
direct reply / committed delivery

High:
Planner

Medium:
Semantic / World Extraction

Low:
Reflection / Simulation / Re-embedding
```

---

# 44. Background 不得阻塞前台

100 个 Reflection 任务：

不能让一条 direct reply 等几十秒。

---

# 45. Scheduler 必须 bounded

模型队列：

不得无限增长。

低优先请求允许：

- defer
- drop
- coalesce

---

# 46. 并发

不同 Backend：

可以独立 semaphore。

同一 Backend：

受：

- max concurrency
- queue capacity
- VRAM

约束。

---

# 47. Fairness

单一 Conversation：

不能占满全部 inference slots。

可设置：

```text
per-scope concurrent limit
```

---

# 48. Model Residency

如果本地显存无法同时装多个大模型：

需要：

```text
Model Residency Policy
```

例如：

```text
Small Semantic always resident
Embedding resident
Main Model on-demand
```

---

# 49. 模型切换成本

Router 应考虑：

```text
model load time
```

不要频繁：

```text
A → B → A → B
```

---

# 50. Quantization

ModelDescriptor 可记录：

```text
bf16
fp16
int8
int4
GGUF Q4
```

但 Core 不关注具体量化。

---

# 51. Role 与量化

允许：

Semantic：

更激进量化。

Planner：

更高质量。

这属于 Router / deployment policy。

---

# 52. Hardware Abstraction

Core 不得写：

```text
CUDA
ROCm
Metal
```

由本地 Runtime backend 处理。

---

# 53. Device Descriptor

基础设施可暴露：

```text
CPU
CUDA
ROCm
AppleSilicon
```

供 Scheduler 参考。

---

# 54. Offline Mode

必须有硬配置：

```text
allow_remote_models = false
```

或：

```text
offline = true
```

此时 Router 禁止任何 RemoteCloud Backend。

不是“尽量不用”。

是：

```text
绝对不能用。
```

---

# 55. Fully Local 验收

断开公网后至少应能：

- CLI direct dialogue
- Semantic
- Planner
- Memory retrieval
- Mind snapshot usage
- basic World extraction
- Reflection（若已配置本地后端）

---

# 56. Degradation Ladder

本地模式示例：

```text
Local Strong
↓
Local Small
↓
Deterministic / template fallback
```

Hybrid 示例：

```text
Local Strong
↓
Cloud Strong
↓
Local Small
```

始终遵守 Privacy Policy。

---

# 57. Remote Cost Budget

云 Backend 可以定义：

```text
input_cost
output_cost
```

支持：

```text
daily remote token budget
hourly call budget
background budget
simulation budget
```

预算耗尽：

优先 Local。

---

# 58. 本地模型的价值

不要只以“省钱”评估。

还包括：

- 持续运行；
- 隐私；
- 离线；
- 低公网依赖；
- 后台 Reflection 成本可控；
- 更低网络抖动；
- 可微调；
- 可自定义推理策略。

---

# 59. Model Evaluation Framework

必须建立：

```text
yunxi-model-eval
```

不能只看通用 benchmark。

必须按 Yunxi Role 评测。

---

# 60. Semantic Eval

至少：

- wants_stop
- future event
- image intent
- conversation relevance
- interjection worthiness
- topic extraction
- structured validity

---

# 61. Planner Eval

至少：

- Reply vs Silent
- Defer
- Goal selection
- OpenLoop handling
- permission awareness
- tool/action selection
- stale state
- non-sycophancy

---

# 62. Dialogue Eval

至少：

- SelfModel consistency
- context following
- naturalness
- hallucination
- verbosity control
- not blindly agreeing
- style stability

---

# 63. Mind Eval

Reflection / Consolidation：

- 不过度创建 Belief
- 不过度更新 Preference
- 不推断敏感属性
- Agenda quality
- Episode quality

---

# 64. World Model Eval

至少：

- observation vs hypothesis
- Unknown handling
- temporal extraction
- stale state
- situation transitions
- uncertainty calibration

---

# 65. Simulation Eval

至少：

- 不假装精确
- candidate discrimination
- no side effect
- uncertainty awareness

---

# 66. Embedding Eval

至少：

- Memory recall
- person scope
- conversation scope
- unresolved item retrieval
- semantic relevance

---

# 67. Golden Dataset

建立受版本控制：

```text
eval/
```

包含：

```text
input
expected structured result
acceptable variants
must-not conditions
```

使用脱敏或人工数据。

---

# 68. Regression Gate

模型升级前：

必须跑 Role-specific regression。

---

# 69. Model Promotion

流程建议：

```text
Candidate Model
→ Offline Eval
→ Shadow
→ Admin Test
→ Small Rollout
→ Promote
```

---

# 70. Shadow Model

允许：

```text
Primary Model
+
Shadow Model
```

Shadow：

只产生对比结果。

不得：

- 发消息
- 写 Mind
- 执行 Tool
- 修改 Goal

---

# 71. A/B Test

允许少量 A/B。

但 A/B 模型不能同时修改共享长期状态。

长期 Mind update 应经过统一 Consolidation Policy。

---

# 72. Model Versioning

每次请求记录：

```text
backend_id
model_id
model_version
prompt_version
```

便于重现问题。

---

# 73. Local Model Artifact Registry

未来记录：

```text
base_model
quantization
adapter
model_hash
license
eval_score
```

---

# 74. SFT / LoRA 预留

v5 允许未来：

```text
SFT
LoRA
QLoRA
Adapters
```

但不要求第一版训练。

---

# 75. 推荐微调顺序

第一批最适合：

```text
Semantic / Structured Extraction
```

然后：

```text
World Extraction
```

再：

```text
Planner
```

最后才考虑：

```text
Dialogue
```

---

# 76. 为什么先微调结构化小任务

因为：

- 目标明确；
- 数据容易构造；
- 自动评测容易；
- 小模型收益明显；
- 风格副作用较小。

---

# 77. 不从零训练基础大模型

v5 不要求从零预训练 foundation model。

个人项目优先：

```text
开源基础模型
→ 本地 inference
→ Eval
→ LoRA / SFT
```

---

# 78. 长期记忆不进模型权重

不要把用户长期记忆“烤进模型”。

用户长期信息继续属于：

- Memory
- Mind
- Relation
- WorldModel

模型权重主要学习：

- 推理格式
- 角色表达
- schema
- task behavior

---

# 79. 不做在线自动训练

禁止：

```text
聊天过程中模型自动 fine-tune 自己
```

权重更新：

离线、显式、版本化、可回滚。

---

# 80. Training Data Export

可以定义：

```rust
pub struct TrainingExample {
    pub role: ModelRole,
    pub input: TrainingInput,
    pub target: TrainingTarget,
    pub quality: f32,
    pub provenance: DataProvenance,
}
```

---

# 81. Provenance

例如：

```text
Synthetic
Manual
Distilled
ProductionOptIn
```

默认禁止：

所有真实聊天自动进入训练集。

---

# 82. Training Privacy

训练数据需要：

- 明确来源；
- 去重；
- 脱敏；
- retention policy；
- 用户数据策略。

---

# 83. Distillation

未来：

```text
Strong Model
→ High-quality structured output
→ Filter
→ Eval
→ Small Model Training
```

不要直接蒸馏未经评测的错误输出。

---

# 84. Prompt Injection 边界

用户消息不能修改：

- model routing
- backend config
- privacy class
- local/remote policy

除非经过明确 Admin Command。

---

# 85. Model Output 不可信

所有模型输出依然：

```text
untrusted proposal
```

尤其：

- Action
- Tool
- Belief update
- World update
- Plan update

都需 Rust 验证。

---

# 86. ModelError

建议统一：

```rust
pub enum ModelError {
    Timeout,
    RateLimited,
    Unavailable,
    InvalidResponse,
    ContextTooLong,
    CapabilityMissing,
    Cancelled,
    OutOfMemory,
    NoEligibleBackend,
    BackendInternal,
}
```

供应商错误映射到统一类型。

---

# 87. Retry

必须 bounded。

例如：

RateLimit：
backoff。

Invalid JSON：
一次 repair。

OOM：
不要立即重复同模型配置。

---

# 88. ContextTooLong

交给 ContextBuilder：

压缩 / trim。

只允许有限重试。

---

# 89. Local Runtime Manager

后期可增加：

```text
ModelRuntimeManager
```

负责：

- inference process lifecycle
- health
- model load/unload
- graceful shutdown

---

# 90. Core 不管理 GPU 进程

RuntimeManager 属于 infrastructure。

---

# 91. LAN Model Server

本地模型可以部署在：

```text
家庭服务器 / 局域网 GPU 主机
```

ModelLocality：

```text
PrivateNetwork
```

仍可以有：

- auth
- TLS
- health checks

---

# 92. Model Cache

可以缓存：

- embedding
- semantic classification
- deterministic-equivalent extraction
- static prompt prefix

Cache Key 至少：

```text
model_id
model_version
prompt_version
schema_version
input_hash
```

---

# 93. Dialogue Cache

普通自然语言回复默认不缓存。

---

# 94. Prefix Cache

如果本地 Runtime 支持：

可利用：

- stable system prefix
- stable SelfModel
- tool schema

提升吞吐。

---

# 95. Batch

适合 batch：

- embedding
- re-embedding
- background classification
- reflection batch

Direct Dialogue：

不要为了 batch 故意增加明显等待。

---

# 96. Metrics

建议：

```text
yunxi_model_requests_total
yunxi_model_errors_total
yunxi_model_latency_seconds
yunxi_model_tokens_total
yunxi_model_queue_depth
yunxi_model_queue_wait_seconds
yunxi_model_fallback_total
yunxi_model_circuit_breaker_total
yunxi_model_local_oom_total
yunxi_model_cancelled_total
yunxi_model_cache_hit_total
```

标签：

```text
role
backend
model
result
```

不要包含消息正文 / 用户 ID。

---

# 97. Debug

管理员可：

```text
#model-status
```

显示：

- backend health
- role mapping
- local/remote
- queue
- offline mode
- fallback
- recent error categories

不显示 secrets。

---

# 98. Config Reload

支持安全热更新：

```text
Role → Backend mapping
```

正在执行请求继续使用旧配置。

新请求使用新配置。

---

# 99. Model Switch

理想情况下：

```text
改配置
```

即可：

```text
Cloud Planner
→ Local Planner
```

Core 无代码修改。

---

# 100. Phase 顺序

建议：

```text
Phase 0  Model API / Role / Capability / Privacy types
Phase 1  将当前 ModelGateway 包装成 Backend
Phase 2  Model Router
Phase 3  Health / Fallback / Circuit Breaker
Phase 4  Local OpenAI-compatible Backend
Phase 5  Inference Scheduler / Priority / Cancellation
Phase 6  Role-specific routing
Phase 7  Embedding Backend
Phase 8  Prompt Registry / Context Builder
Phase 9  Offline / LocalOnly hard policy
Phase 10 Metrics / Debug / Config Reload
Phase 11 Role-specific Eval
Phase 12 Shadow / A-B
Phase 13 Model Artifact Registry / Version
Phase 14 Local Runtime Manager
Phase 15 Training Export / LoRA-SFT Hooks
Phase 16 Fully Local Acceptance
```

---

# 101. Phase 0

新增纯抽象：

- ModelRole
- BackendId
- ModelId
- ModelCapabilities
- InferenceRequest
- InferenceResponse
- ModelUsage
- ModelError
- PrivacyClass

不得改变现有生产行为。

---

# 102. Phase 1

把当前云 ModelGateway：

包装成 Backend。

只做抽象迁移。

不要同时大改 Prompt。

---

# 103. Phase 2

Router：

先按静态 config 路由。

---

# 104. Phase 3

增加：

- health
- timeout
- fallback
- circuit breaker

---

# 105. Phase 4

接入：

```text
Local OpenAI-compatible endpoint
```

关键验收：

只改配置即可把某 Role 从云切本地。

---

# 106. Phase 5

统一模型调度：

- bounded queue
- priority
- cancellation
- per-backend semaphore
- fairness

---

# 107. Phase 6

正式拆：

- Semantic
- Planner
- Dialogue
- Reflection
- WorldExtraction
- Simulation

---

# 108. Phase 7

Embedding：

独立接口与版本化。

---

# 109. Phase 8

Prompt Registry：

版本化。

ContextBuilder：

按 Role 最小化上下文。

---

# 110. Phase 9

实现真正 Offline：

```text
allow_remote_models = false
```

必须是硬限制。

---

# 111. Phase 10

增加：

- metrics
- admin debug
- runtime config reload

---

# 112. Phase 11

建立：

Role-specific golden eval。

---

# 113. Phase 12

支持：

Shadow model / A-B。

---

# 114. Phase 13

Model Artifact Registry：

记录：

- model version
- hash
- capabilities
- quantization
- eval status

---

# 115. Phase 14

可选实现：

Local Runtime Manager。

---

# 116. Phase 15

建立训练数据 export schema。

不自动训练。

---

# 117. Phase 16

完全断网验收。

---

# 118. 关键测试矩阵

至少覆盖：

1. Role routing 正确
2. Capability filter 正确
3. LocalOnly 不走云
4. Offline mode remote request = 0
5. Backend timeout fallback
6. Circuit breaker
7. Scheduler priority
8. Reflection 不阻塞 direct reply
9. stale Reply cancellation
10. queue bounded
11. local OOM degrade
12. config reload race
13. invalid structured output repair bounded
14. embedding version 不混用
15. prompt version 可追踪
16. model switch 不改变 Core state
17. Shadow model 不产生 side effect
18. A/B 不双写 Mind
19. no secrets in logs
20. fully local CLI works

---

# 119. Behavioral Scenario A

当前：

```text
Semantic → Cloud
```

切配置：

```text
Semantic → Local 7B
```

用户行为保持基本一致。

---

# 120. Scenario B

Planner Local backend down。

Hybrid：

```text
→ Cloud Planner
```

LocalOnly：

```text
→ fail closed / degrade
```

绝不能偷偷云 fallback。

---

# 121. Scenario C

后台 50 个 Reflection。

用户发 direct message。

Scheduler：

direct 优先。

---

# 122. Scenario D

ReplyTicket stale。

本地模型还在生成。

系统：

取消该 request。

---

# 123. Scenario E

Embedding Model A：

已有向量。

更换 Model B。

旧 A 与新 B：

不混用。

---

# 124. Scenario F

Cloud cost budget 用尽。

Router：

优先 Local。

---

# 125. Scenario G

新本地 Planner Eval 表现差。

不能 promotion。

---

# 126. Scenario H

新 Dialogue 模型风格漂移。

Shadow / regression 检出。

回滚配置。

---

# 127. Scenario I

完全断网。

Yunxi CLI：

仍能对话并读取 Memory。

---

# 128. Scenario J

本地大模型 OOM。

Backend：

Degraded。

Router：

选择其他 eligible backend。

---

# 129. 单元测试

至少：

- routing order
- privacy enforcement
- capability matching
- error mapping
- circuit breaker state
- budget
- config validation
- model registry
- cache key
- prompt version

---

# 130. 并发测试

至少：

- high vs low priority
- cancellation race
- backend health race
- hot reload race
- per-scope fairness
- no lock across await
- no deadlock

---

# 131. 性能测试

记录：

```text
p50 / p95 / p99
TTFT
tokens/sec
queue wait
```

按 Role。

---

# 132. 本地性能重点

关注：

- load time
- VRAM
- TTFT
- tokens/s
- context length
- concurrent sessions

---

# 133. Cost Eval

Hybrid 模式记录：

```text
remote tokens saved
local request share
fallback share
```

---

# 134. Privacy Eval

构造 `LocalOnly` 请求。

确保：

任何 remote backend：

都未收到。

---

# 135. 模型升级原则

不能只因为：

```text
benchmark 更高
```

就替换生产模型。

必须：

```text
Role Eval
+
Shadow
+
Regression
```

---

# 136. Persona Consistency

Dialogue Model 必须跑稳定性测试。

换模型不能让：

SelfModel / Values

被模型临场风格覆盖。

---

# 137. Decision Consistency

Planner 模型必须测试：

- Reply
- Silent
- Defer
- Tool
- Goal
- Proactive

---

# 138. Fine-tune 数据边界

模型微调数据不要成为 Memory 替代品。

长期个人状态：

仍在数据库 / Core 状态。

---

# 139. 训练版本

每个 Adapter：

记录：

```text
base_model_version
adapter_version
dataset_version
eval_version
```

---

# 140. 回滚

模型和 LoRA：

必须可回滚。

最好仅修改：

```text
Model Registry / Config
```

即可。

---

# 141. 供应链

本地模型文件：

建议校验 hash。

不要默认执行不受信任 remote code。

---

# 142. 运行隔离

本地 inference server：

建议独立进程。

Core crash / model crash：

互相隔离。

---

# 143. Model Fabric Degradation Modes

建议：

```text
Full
NoSimulation
NoReflection
SmallModelsOnly
OfflineLocal
DeterministicFallback
```

---

# 144. 高负载降级顺序

优先关闭 / 延后：

```text
Simulation
Deep Reflection
Background Re-embedding
Low-value World Extraction
```

保留：

```text
Direct Reply
MustExecute
Planner
Critical Semantic
```

---

# 145. v1-v4 兼容性

每个 Phase 必须继续保证：

- WorldEvent
- ReplyTicket
- Reminder
- AgentTask
- Memory
- OpenLoop
- Mind v2
- Executive v3
- World Model v4
- Kovi Host
- CLI Host

不回归。

---

# 146. Codex 实施原则

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

# 147. Codex 禁止

不得：

- 重写 v1-v4
- 把具体模型供应商写进 Core
- 把本地 URL 硬编码进 Mind / Planner
- 自动下载任意模型
- 自动 fine-tune 生产聊天
- 在线修改自身权重
- 无限 retry
- 无界 inference queue
- Shadow 模型执行 Action
- LocalOnly 请求走云
- 打印 API Key
- 自动 production deploy

---

# 148. Definition of Done

## Model Abstraction

Core / Mind / Executive / WorldModel 不知道 Provider。

## Router

能按：

Role / Capability / Privacy / Health / Budget

选择 Backend。

## Local

至少支持一个通用本地文本生成 Backend。

## Scheduler

有：

- bounded queue
- priority
- cancellation
- concurrency control

## Health

有：

- health state
- fallback
- circuit breaker

## Offline

真正禁止远程请求。

## Privacy

LocalOnly fail closed。

## Embedding

独立 Backend + version。

## Prompt

有 Registry + Version。

## Eval

有 Role-specific regression。

## Training

只提供离线、受控、版本化 hooks。

## Portability

更换模型不重写 Yunxi。

---

# 149. 最终架构

```text
                           YUNXI CORE
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
            MIND          WORLD MODEL       EXECUTIVE
              │                │                │
              └────────────────┼────────────────┘
                               ▼
                            PLANNER
                               │
                               ▼
                         MODEL FABRIC
                               │
         ┌───────────────┬─────┼─────┬────────────────┐
         ▼               ▼           ▼                ▼
  LOCAL SEMANTIC   LOCAL STRONG   EMBEDDING       CLOUD
         │               │           │                │
         ▼               ▼           ▼                ▼
   Extraction      Planner/Dialog   Retrieval       Fallback
```

---

# 150. Fully Local 最终状态

```text
Yunxi
 │
 ├── Semantic      → Local
 ├── Planner       → Local
 ├── Dialogue      → Local
 ├── Reflection    → Local
 ├── WorldExtract  → Local
 ├── Simulation    → Local
 ├── Embedding     → Local
 └── Reranking     → Local
```

公网模型请求：

```text
0
```

---

# 151. 最终产品验收

今天：

```text
Planner
→ Cloud Backend A
```

明天：

```text
Planner
→ Local Model B
```

以后：

```text
Planner
→ 自己微调的 Yunxi Planner Model
```

而：

```text
PersonId
Memory
SelfModel
Beliefs
Preferences
Relations
Goals
OpenLoops
InnerAgenda
Executive Plans
World Situations
```

全部保持不变。

---

# 152. 最重要的一句话

Yunxi Model Fabric v5 的目标不是：

> **“让 Yunxi 使用更多模型。”**

而是：

> **“让任何一个具体模型都不再等于 Yunxi。”**

模型只是推理资源。

Yunxi 的连续性存在于：

```text
Core
Mind
Executive
World Model
Memory
Identity
Persistent State
```

因此真正成功的验收标准是：

```text
Cloud Model
→ Local Model
→ Fine-tuned Local Model
```

可以不断替换，

而同一个 Yunxi：

继续记得过去、
保持关系、
保持 Goal、
保持 SelfModel、
保持未完成事项。

如果换模型仍然需要重写前四层，

v5 就没有完成。

---

# 153. v5 之后

v5 完成后，不再继续堆认知架构版本。

后续真正值得投入的是：

- 选择合适本地基础模型
- GPU / Apple Silicon / 私有服务器部署
- 量化
- KV Cache
- Prompt Cache
- Speculative Decoding
- 模型评测
- SFT / LoRA
- Voice
- TTS
- Live2D
- Vision
- Desktop / Mobile
- Game Integration
- 长期行为调参
- 延迟与稳定性

最终体系：

```text
V1 Core
V2 Mind
V3 Executive
V4 World Model
V5 Model Fabric
```

已经足够构成一个平台无关、模型可替换、可云端、可混合、也可完全本地运行的持续 Agent 架构。

---

# 154. Stale Generation Cancellation 与 Pre-Commit 支持

Model Fabric v5 必须把会话竞争视为一等取消场景。每个 text generation request 应尽量绑定：`GenerationId`、`OutgoingId`、ReplyTicket/generation token、`CancellationToken`。

当 V1 ConversationCoordinator 判定 PendingOutgoing stale / superseded 时，应尽早 cancel backend generation。如果具体云/本地 Backend 无法真正取消，允许请求结束，但结果必须丢弃，不得重新进入 send path。

Scheduler 优先级进一步明确：

```text
Direct Reply
> Prepared Proactive rewrite
> Background Reflection / Simulation
```

Streaming 第一阶段建议：

```text
Generate / Buffer
→ Prepared
→ Revalidate
→ Commit
```

不要把尚未 revalidate 的 token 直接视作已提交的用户可见副作用。未来若实现真正 live token streaming，应单独定义 `PartialCommit`。

新增测试：stale ReplyTicket cancels generation；cancelled backend result cannot commit；proactive generation 被 direct user message supersede；取消 race 不产生重复发送；cancellation 不跨 Conversation 误杀其他 generation。
