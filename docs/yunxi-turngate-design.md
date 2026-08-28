# Yunxi TurnGate 小分类器设计

**状态：** 修订设计稿 v0.2

**目标：** 在 2 核 4GB 服务器上，用一个常驻、低延迟、纯本地的小型判别模型，辅助完成两个不同的决定：

1. 这一条消息连同当前已经积累的片段，现在是否适合提交为一个 turn；
2. turn 提交后，在当前私聊或群聊上下文中，芸汐现在是否应该发一条可见消息，以及应当是正常回答、继续聊天、简短确认、等待还是静默。

这里的“是否说完”不是对用户未来行为的证明。TurnGate 只能预测“现在 flush 是否合适”，不能知道用户几秒后是否还会发送下一条。等待时间只允许作为失联时的 liveness watchdog，不能作为语义标签。

TurnGate 只负责输入门控和回复候选判断。它不生成可见回复，不执行工具，不拥有权限，也不替代 `ConversationState`、Executive、ActionArbiter 或 MiniMind 生成模型。

## 1. 结论

不要再部署两个独立的 Transformer，也不要让 MiniMind 0.1B 为每一条消息生成一个“是否接话”的答案。

推荐部署一个共享特征、两个输出头的线性分类器。两个 head 可以在一次轻量推理中同时得到结果，但路由上仍然分成“先决定是否提交、再决定是否发言”两个阶段：

```text
TurnGate
├── completion head: flush_now / hold_for_more
└── response head: answer / continue / ack / ignore / wait
```

`abstain` 不是一种语言类别，而是校准后的运行时结果：当 logits 置信度或 margin 不够时，runtime 返回 `abstain`。低置信度时由程序按 scope 和硬策略决定等待或静默，禁止分类器强行推动一条消息。

`must_reply`、`command`、`stop`、`erase` 等是程序策略覆盖，不是模型类别。普通私聊不能因为 scope 是 `private` 就跳过 response head，也不能把私聊的 response 标签写成 `null`。

模型形态采用中文字符 n-gram + 结构化上下文特征的逻辑回归或 softmax 线性模型。训练在开发机或 CI 离线完成，生产服务器只加载推理权重。

MiniMind 0.1B 的职责保持为：

- Intrinsic 文字回复生成；
- Intrinsic 视觉回复生成；
- TurnGate 未加载、版本不可用或灰区需要更强语义时的有限兜底；
- 可选的离线伪标签教师，伪标签必须经过抽样人工复核。

## 2. 为什么不直接使用 MiniMind

即使只要求 MiniMind 输出一个 token，也需要执行完整 decoder forward。`max_parallel = 1` 可以限制内存和 CPU 并发，但不能消除以下问题：

- 普通群消息一多，完成度判断会和正常回复争抢唯一的推理槽位；
- 生成模型可能输出解释、内部标记或错误格式；
- “是否接话”需要群聊上下文、当前话题和预算，不是单条文本生成任务；
- 2 核 CPU 上的排队延迟会直接影响连续对话的响应性。

当前代码中的 `classify_input_completion` 可以作为过渡路径，但最终应由 TurnGate 取代模糊输入上的 MiniMind 分类调用。当前群聊的 `reserve_interjection_decision`、频率预算和硬规则仍然保留，它们是安全边界，不应被模型替换。

## 3. 决策边界

### 3.1 硬规则优先

以下情况由程序先识别；它们不应依赖模型决定权限或执行控制命令：

- 停止、删除、数据擦除、提醒、任务和权限相关命令；
- 已经提交的 MustExecute 行为；
- 平台协议必须立即消费的事件。

以下情况**不能**作为绕过 TurnGate 的理由：

- 普通私聊消息；
- 普通群聊消息；
- 只因为明确 `@` 芸汐或回复芸汐而认为用户已经说完。

明确 `@` 芸汐或回复芸汐的消息可以设置 `policy_override = must_reply`，表示 turn 一旦提交就不能被 response head 选为 `ignore`；它们仍然要经过 completion 判断，必要时继续等待用户把句子发完。普通私聊不设置这个 override，必须让 response head 在 `answer/continue/ack/ignore/wait` 中做选择。这样既不会漏掉直接请求，也不会把“@芸汐，我想问你一件事”误当成完整问题。

硬命令可以使用 `policy_override = command | stop | erase`，直接进入现有命令、FIFO barrier 或取消路径。硬规则只决定进入哪条处理链，不直接生成正文；停止和数据擦除仍由现有 Core FIFO barrier 与取消逻辑处理。

### 3.2 completion：现在是否提交这一轮

completion 的语义是：

> 给模型看到当前片段和已经等待中的片段，在不知道未来消息的前提下，判断现在把它们提交给语义理解和回复决策是否合适。

它不是“用户是否永远说完”的分类，也不是“之后是否真的来了下一条”的回看标签。典型行为如下：

```text
入站片段 + pending_user_fragments + 有界上下文
    -> completion head
hold_for_more
    -> 留在该 scope 的 pending FIFO，不生成可见回复
flush_now
    -> 冻结当前 batch，进入 response head
abstain
    -> 沿用保守的 normal hold 行为
失联保护
    -> max_wait 只负责最终释放队列，并单独记录 flush_reason=watchdog
```

判断依据包括当前句法、语义完整性、待合并片段之间的关系、芸汐上一轮是否正在等待补充，以及图片/表情等附件是否可能在下一条文字中补全。示例：

- “我想问你一件事”通常是 `hold_for_more`；随后收到“你明天有空吗”时，合并候选通常变成 `flush_now`；
- “今天吃饭了吗”即使没有句号，也通常是 `flush_now`；
- “因为”“然后还有”“如果可以的话”通常是 `hold_for_more`，但完整上下文可以改变判断；
- “嗯”“好”“知道了”可能是 `flush_now`，之后由 response head 决定 `ack` 还是 `ignore`。

标点、消息间隔和短语词表只能是特征或快速路径，不能单独定义真值。`max_wait` 是 liveness watchdog，不是 completion 的语义答案。

### 3.3 response：现在是否应该发言

response head 的问题是：

> 假设当前候选 turn 现在已经提交，芸汐是否应该在这个 scope 发一条可见消息？如果应该，属于哪种回复行为？

response 使用统一标签，私聊和群聊都必须标注：

- `answer`：应产生一条正常内容回复；私聊中的直接问题、群聊中有自然交流价值的未点名消息都可以属于此类；
- `continue`：应继续已经存在的连续对话，通常依赖 `conversation_active` 和最近几轮；
- `ack`：只需要短确认或情绪承接，不需要完整回答；
- `ignore`：不发言更自然，例如旁聊、复读、纯噪声、已明确结束的礼貌收尾；
- `wait`：当前 turn 暂时不发言，等待用户补全、任务结果、上一条发送完成或明确的外部事件；
- `abstain`：模型无法可靠判断，由程序的 scope-specific fallback 处理。

这套标签不把“群聊接话”单独做成和私聊不同的模型。群聊未点名时，`answer` 就表示一次候选 interjection，但仍必须通过群聊频率预算、冷却、Executive 和 `ConversationState`；私聊中的 `answer/continue/ack/ignore/wait/abstain` 都是合法结果。

因此，普通私聊也不再是“一收到就必答”：

```text
我想问你一件事       -> completion=hold_for_more, response=wait
你明天有空吗         -> completion=flush_now, response=answer
嗯，原来如此         -> completion=flush_now, response=ack 或 ignore
谢谢，先这样         -> completion=flush_now, response=ignore
（已有进行中的话题）对，就是这个 -> completion=flush_now, response=continue
```

`must_reply` 只表示不能接受 `ignore`，不要求模型对每条私聊都生成长回答。命令、停止和擦除由 `policy_override` 直接处理，不把它们混入普通 response 学习目标。

### 3.4 两阶段状态机

```text
入站消息
    -> 硬策略识别
    -> 构造上下文快照
    -> completion
       ├─ hold_for_more -> pending FIFO，response 的有效结果为 wait
       └─ flush_now     -> 冻结 batch，使用冻结后的候选上下文进入 response
    -> response
       ├─ answer/continue/ack -> 申请 ReplyTicket，交给生成模型
       ├─ ignore              -> 丢弃可见回复候选，更新会话观察
       ├─ wait                -> 等待明确事件或受限 watchdog
       └─ abstain             -> 按 scope fallback，不把 abstain 当 ignore
```

TurnGate 不保存无限历史，也不通过提示词把很多消息拼成一大段。每次只读取 bounded snapshot；真正的连续对话由 `MessageCoalescer`、`ConversationState`、`ConversationCoordinator`、`ReplyTicket` 和 pending FIFO 维护。生成期间的新消息作为新的 turn 排队，不能覆盖或拼接进正在生成的 turn。

### 3.5 模型怎样做决定

TurnGate 不需要生成解释文本。训练器和 Rust runtime 对同一个输入快照生成同一个特征向量 `x`，两个 head 分别计算 logits：

```text
p_completion = softmax(W_completion * x + b_completion)
p_response   = softmax(W_response * x + b_response)
```

然后使用验证集校准过的阈值执行路由：

```text
policy_override in command/stop/erase -> 走程序硬路径
completion=hold_for_more             -> 有效 response 强制为 wait
completion=flush_now + response=...  -> 按 response 和置信度执行
任一 head 低于阈值                   -> abstain，走 scope-specific fallback
```

所以模型“判断说没说完”依赖的是语言片段、pending 片段和上下文的统计规律；模型“判断该不该接话”依赖的是当前 scope、是否正在对话、最近几轮、芸汐是否刚提问、是否有任务/发送在途，以及消息本身。它不会读取未来消息，也不会通过自己的解释来改变权限或调度。

## 4. Core API 草案

建议将 API 明确拆成“共享输入 + 两个 head 的结果”，避免用一个 `complete` 字段暗示模型掌握了未来：

```rust
pub enum TurnCompletion {
    FlushNow,
    HoldForMore,
    Abstain,
}

pub enum TurnResponseDecision {
    Answer,
    Continue,
    Ack,
    Ignore,
    Wait,
    Abstain,
}

pub enum TurnPolicyOverride {
    None,
    MustReply,
    Command,
    Stop,
    Erase,
}

pub enum TurnScope {
    Private,
    Group,
}

pub enum RecentTurnRole {
    User,
    OtherMember,
    Assistant,
}

pub struct RecentTurn {
    pub role: RecentTurnRole,
    pub text: String,
}

pub struct TurnGateInput {
    pub current_text: String,
    pub pending_user_fragments: Vec<String>,
    pub recent_turns: Vec<RecentTurn>,
    pub scope: TurnScope,
    pub conversation_active: bool,
    pub bot_last_asked_question: Option<String>,
    pub pending_outgoing: bool,
    pub pending_task: bool,
    pub addressed_to_agent: bool,
    pub replies_to_agent: bool,
    pub has_image: bool,
    pub has_sticker: bool,
    pub policy_override: TurnPolicyOverride,
}

pub struct TurnGateOutput {
    pub completion: TurnCompletion,
    pub completion_confidence: f32,
    pub response: TurnResponseDecision,
    pub response_confidence: f32,
    pub model_version: String,
}

pub struct CompletionOutput {
    pub decision: TurnCompletion,
    pub confidence: f32,
}

pub struct ResponseOutput {
    pub decision: TurnResponseDecision,
    pub confidence: f32,
}
```

具体实现可以使用 `TurnGateRuntime` 和 `TurnGateEngine` 两层：

- `TurnGateEngine` 只做同步、无副作用的特征计算和分类；
- `TurnGateRuntime` 负责模型加载、manifest 校验、健康状态、指标和不可用降级。

建议提供两个显式方法，虽然底层可共享一次特征计算：

```rust
fn classify_completion(&self, input: &TurnGateInput) -> CompletionOutput;
fn classify_response(&self, input: &TurnGateInput) -> ResponseOutput;
```

当 completion 为 `HoldForMore` 时，路由层强制有效 response 为 `Wait`，不允许 response head 的偶然高分提前触发回复。`FlushNow` 后，response head 使用冻结的 batch 作为 `current_text`，并清空本轮已消费的 pending fragments。`MustReply` 只约束最终路由不能选择 `Ignore`，不改变 completion 的判断。

现有 `InputCompletion` 可以先保留兼容：`FlushNow` 映射为旧的 complete，`HoldForMore` 映射为旧的 incomplete，`Abstain` 映射为 coalescer 的普通等待路径。兼容类型稳定后再删除。

所有字段必须有界：

- `current_text` 最多 512 个 Unicode 字符；
- `pending_user_fragments` 最多 4 条，每条最多 160 个 Unicode 字符；
- `recent_turns` 最多 4 条，每条最多 160 个 Unicode 字符；
- `bot_last_asked_question` 最多 160 个字符；
- 不进入模型的字段包括 QQ 号、数据库主键、URL、Token 和原始完整聊天记录；
- 输入只作为数据，不允许分类器输出任何可执行协议。

## 5. 特征与模型

### 5.1 文本特征

输入预处理：

1. 当前片段截断到 512 个字符和 2048 个字节以内；
2. ASCII 字母转小写；
3. 合并空白；
4. 保留中文标点、emoji 的稳定表示；
5. 为当前片段、pending 片段和最近 turn 添加稳定的字段边界标记；
6. 不把 future message、消息到达间隔或“最终是否发了回复”编码进特征。

对 current text、pending fragments 和 recent turns 提取字符 2-gram 到 5-gram，经过固定的稳定 hash 映射到特征桶。每个字段有独立边界标记，每次推理最多保留 512 个非零特征，重复特征计数上限为 2。这样模型看到的是“谁说了什么、它处于哪一段上下文”，而不是把上下文无边界地拼成提示词。

建议初始参数：

```text
hash buckets = 65536
ngram range = 2..=5
text feature count <= 512
context feature count <= 64
pending fragments <= 4
recent turns <= 4
```

不用 `DefaultHasher`，因为跨版本不保证稳定。需要固定的 FNV-1a 或等价稳定 hash，并在训练器和 Rust runtime 中共享测试向量。

### 5.2 结构化特征

额外输入使用固定位置的数值或 one-hot 特征：

- private / group；
- `pending_user_fragments` 是否为空、数量和每段的字段位置；
- `recent_turns` 中 user/other_member/assistant 的角色边界；
- addressed_to_agent；
- replies_to_agent；
- conversation_active；
- `bot_last_asked_question` 是否存在及其有限字符特征；
- `pending_outgoing`；
- `pending_task`；
- has_image；
- has_sticker；
- 当前输入与 recent turns 的有限词面关系。

不把“距离上次发言多少秒”作为 completion 或 response 的语义特征，也不把“下一条消息是否已经到达”作为训练标签。冷却时间、频率窗口、每日额度、正在发送状态和任务锁仍由程序执行。

### 5.3 输出和置信度

completion head 学习两个 logits：`flush_now` 和 `hold_for_more`；response head 学习五个 logits：`answer`、`continue`、`ack`、`ignore` 和 `wait`。通过独立验证集做温度校准或 Platt scaling，再用类别阈值和 top-1/top-2 margin 产生运行时 `abstain`。`abstain` 不作为“用户真实意图”的伪类别。

初始策略应保守：

```text
completion:
  flush_now / hold_for_more 只有达到验证集阈值才采用
  否则 abstain

response:
  answer/continue/ack 需要达到各自阈值后才允许生成
  ignore 也必须达到阈值，不能把低置信度自动当成 ignore
  wait 用于明确的补全、事件或任务等待
  不确定返回 abstain，由 scope-specific fallback 处理

policy:
  must_reply 禁止最终选 ignore，但不跳过 completion
  command/stop/erase 由程序直接处理
```

阈值必须配置在模型 manifest 或受校验的配置中，不能散落在群聊处理代码里。

### 5.4 资源预算

以 65536 个 hash 桶、2 个 completion 类别和 5 个 response 类别、`f32` 权重估算：

```text
65536 * (2 + 5) * 4 bytes ~= 1.75 MiB
```

加上特征表、manifest、上下文权重和运行时对象，目标常驻内存小于 20MB。后续确认精度后可导出 int8 权重进一步降低体积，但不在第一版同时引入量化误差和模型结构变化。

推理目标是单次 P95 小于 5ms。这个目标需要在实际 2 核机器上基准测试，不以开发机结果代替。

## 6. 模型资产格式

建议目录：

```text
models/
└── yunxi-turngate/
    ├── manifest.toml
    ├── turn_gate.bin
    └── THIRD_PARTY_NOTICES
```

manifest 至少包含：

```toml
manifest_version = 1
model_id = "yunxi-turngate"
model_version = "v0.2.0"
algorithm = "hashed-char-ngram-logistic"
feature_version = "char-2-5-v2"
hash_buckets = 65536
max_text_chars = 512
max_pending_fragments = 4
max_pending_fragment_chars = 160
max_recent_turns = 4
max_recent_turn_chars = 160
max_question_chars = 160
completion_labels = ["flush_now", "hold_for_more"]
response_labels = ["answer", "continue", "ack", "ignore", "wait"]
abstain = "calibrated_threshold"
training_data_version = "local-dataset-v2"

[[assets]]
path = "turn_gate.bin"
sha256 = "..."
size_bytes = 0
```

加载顺序：

```text
read manifest
-> validate bounds and feature version
-> validate path, size and sha256
-> validate binary dimensions and finite weights
-> load immutable Arc<TurnGateEngine>
-> run fixed-vector self-test
-> publish Healthy
```

模型缺失、损坏或版本不兼容时，Core 不启动失败：

- completion 回退到现有 lexical + MiniMind/normal hold 路径；
- response 回退到现有私聊处理链，以及群聊的保守预算和语义模型路径；模型缺失时群聊可以直接静默；
- 群聊限流、停止、任务、提醒和数据擦除不受影响。

模型替换使用新文件写入临时路径、校验完成后原子 rename。运行中的 `Arc` 保持不变，禁止在推理过程中原地修改权重。

## 7. 离线训练方案

### 7.1 数据格式

训练输入使用脱敏 JSONL，不存平台 ID。协议版本升级为 `2`，明确区分当前片段、尚未提交的用户片段和已经发生的有限上下文：

```json
{"schema_version":2,"sample_id":"tg_01HXYZ","current_text":"我想问你一件事","context":{"scope":"private","pending_user_fragments":[],"recent_turns":[{"role":"user","text":"最近准备去哪里玩"},{"role":"assistant","text":"还没有决定"}],"conversation_active":true,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"hold_for_more","response":"wait","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":1.0},"feature_version":"char-2-5-v2"}
{"schema_version":2,"sample_id":"tg_01HABC","current_text":"你明天有空吗","context":{"scope":"private","pending_user_fragments":["我想问你一件事"],"recent_turns":[{"role":"assistant","text":"还没有决定"}],"conversation_active":true,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"flush_now","response":"answer","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":1.0},"feature_version":"char-2-5-v2"}
{"schema_version":2,"sample_id":"tg_01HDEF","current_text":"有人今晚打游戏吗","context":{"scope":"group","pending_user_fragments":[],"recent_turns":[{"role":"other_member","text":"今晚好无聊"}],"conversation_active":false,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"flush_now","response":"ignore","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":0.9},"feature_version":"char-2-5-v2"}
```

示例中的 `recent_turns` 必须来自消息到达前真实存在的上下文；如果没有前置 turn，`conversation_active` 应为 `false`，`recent_turns` 应为空。采集器不得为了让样本看起来合理而编造上下文。

数据必须满足：

- 明确授权或使用已经允许用于训练的样本；
- 移除 QQ 号、昵称、URL、Token 和平台消息 ID；
- 数据擦除请求能够反向定位并删除训练样本；
- 不把芸汐自己的历史决策直接当作真值；
- 不把“几秒内有没有下一条消息”作为 completion 的唯一标签。

MiniMind 或 Strong 可以提出伪标签，但伪标签只能作为待复核样本来源，不能闭环自我训练。

### 7.2 标注定义

completion 标注看当前输入、尚未提交的 pending fragments 和允许展示的有限上下文，但必须隐藏未来消息。标注问题是：

> 如果现在不知道用户未来会不会继续发送，把当前候选提交给对话管线是否合适？

- `flush_now`：当前候选已经足够形成一个可独立处理的 turn；
- `hold_for_more`：明显是半句、列举开头、因果未完、等待后续对象，或 pending fragments 仍很可能属于同一意图；
- `abstain`：人工也无法稳定判断，放入校准/回访集，不当成强监督语义类别。

response 标注必须包含同一时刻可获得的上下文，且私聊和群聊一视同仁。标注问题是：

> 假设当前候选现在已经提交，芸汐是否应该在这个 scope 发一条可见消息？

- `answer`：应该正常回答；
- `continue`：应该接续已有对话；
- `ack`：只需简短确认或情绪承接；
- `ignore`：不发言更自然；私聊也允许此标签；
- `wait`：需要等用户补全、任务/工具结果、上一条发送完成或明确事件；
- `abstain`：上下文不足或人工无法稳定判断。

`policy_override = must_reply` 的样本仍然要标注 response 类型，因为它只禁止 `ignore`，不替模型决定是 `answer`、`continue` 还是 `ack`。`command`、`stop`、`erase` 等硬策略样本可以保留用于路由回归，但不进入普通 response loss；它们的 `labels.response` 使用 `abstain`，而不是 `null`。

### 7.3 训练和切分

第一版建议目标：

- completion：至少 5,000 条，覆盖中英文、无标点短句、半句、括号、列表、emoji 和连续发送；
- response：至少 10,000 个“上下文 + 消息”样本，必须同时包含私聊和群聊，并为 `answer`、`continue`、`ack`、`ignore`、`wait` 准备 hard negative；
- 按 conversation/group 切分 train、validation、test，禁止同一连续对话被随机拆到不同集合；
- 对私聊的 `ignore`、群聊的 `answer` 和所有 `ack/wait` 使用分层采样，避免模型只学会“私聊必答”或“群聊必静默”；
- 记录 calibration、每个 scope 的混淆矩阵、PR 曲线和不同场景的分层结果。

训练器可以先使用 Python `scikit-learn` 或等价离线工具，但生产仓库只需要导出的二进制权重和固定特征协议。训练环境不进入 Kovi 插件启动依赖。

### 7.4 数据来源和采集闭环

第一版数据按四类来源获得，再加一条最小闭环，优先级从高到低如下：

#### A. 人工种子集

先由开发者和运营者编写一批脱离真实用户身份的样本，覆盖决策边界，而不是追求聊天语料规模。

completion 至少覆盖：

- 完整但没有句号的中文短句；
- 以“因为、然后、还有、如果、我想问”结尾的半句；
- 括号、引号、列表和代码未闭合；
- 连续发送时常见的“第一段 + 第二段”；
- emoji、图片说明、口语省略和中英文混排。

response 至少覆盖：

- 正在进行的话题中的自然追问；
- 私聊中的正常问题、补充、简短应答、礼貌收尾和明确要求暂缓回复；
- 群聊中没有点名但有明确交流价值的陈述；
- 群成员之间的旁聊、复读、纯表情和公告；
- 机器人刚刚回复后，其他成员是否自然加入；
- 相同文本在有连续会话上下文和无连续会话上下文时的不同标签；
- 同一文本在 `pending_task`、`pending_outgoing` 和普通空闲状态下的不同标签。

种子集可以用模板和受控改写扩充，但必须由人复核。模板扩充只能增加覆盖面，不能代替真实分布。

#### B. 线上主动学习样本

TurnGate 接入前先以 shadow 模式运行，只保存有价值的样本：

- completion 置信度低的样本；
- 新旧 completion 结果不一致的样本；
- 当前私聊/群聊逻辑与 response head 结果不一致的样本；
- response 的 `answer/continue/ack/ignore/wait` 候选及其附近的 hard negative；
- 用户显式纠正“我还没说完”“不用接话”“先别回复”等反馈对应的样本。

不保存所有群消息。默认关闭采集；开启后使用采样率、最大队列、保留天数和磁盘上限。采集文件只在本机保存，不自动上传。

标注页面或管理员命令应展示：

- 当前消息；
- 最近有限条上下文；
- 最近有限上下文和会话状态；
- 不暴露 QQ 号、数据库 ID 和 Token。

completion 标注时隐藏“之后实际发生了什么”，防止标注员把时间间隔当成答案。response 标注则必须提供足够上下文，并明确问题是“现在是否应该让芸汐发一条可见消息”。私聊样本不能因为天然存在一问一答预期而自动标为 `answer`。

#### C. 弱标签和数据增强

以下只能作为候选数据，不能直接作为最终真值：

- 高精度 lexical 规则产生的 flush_now/hold_for_more；
- MiniMind 或 Strong 对灰区样本给出的分类；
- 用户后续发送的消息、撤回和显式反馈；
- 从完整句子按自然边界截断得到的半句。

弱标签进入人工复核队列。至少经过一次人工确认，或在独立测试集上证明其与人工标签一致，才可以进入训练集。

#### D. 隐私和删除

训练数据采集必须和 Core 的数据擦除保持同一生命周期：

- 默认不采集，启用时必须是明确的本地配置；
- 原文只存在于有界、短 TTL 的待标注区；
- 导出训练集前去除 QQ 号、昵称、URL、平台 ID 和可识别元数据；
- 维护一个仅供本机删除屏障使用的 opaque source key，不把它导出到模型；
- 用户或群组发起数据擦除时，删除待标注样本和未训练样本；
- 已训练模型不能声称可从权重中逐条删除个人内容，因此训练集更新必须生成新版本并重新评估。

#### E. 最小闭环

没有现成数据时按以下顺序开始：

```text
人工写 2,000 条 completion + 3,000 条 response 种子（私聊和群聊都要有）
    -> 线上 shadow 采集灰区和分歧样本
    -> 每周人工复核 500 条
    -> 达到目标后离线训练 v0.1
    -> 新旧逻辑 shadow 对比
    -> 只先启用 completion，再启用 response（先私聊，再谨慎开启群聊 ambient）
```

线上运行产生的“芸汐实际发了/没发”只能作为回访和采样依据，不能直接当作标签。否则模型会把旧的概率、冷却和时间窗口策略复制下来。

### 7.5 采集文件和训练文件格式

统一使用 UTF-8 JSONL，每行一个样本，每个字符串中的换行必须以 JSON 转义保存。线上采集和离线训练使用不同目录：

```text
data/turngate/inbox/*.jsonl      # 本机待标注，可能含脱敏后的原文
data/turngate/labeled/*.jsonl    # 已标注，可供离线训练
models/yunxi-turngate/           # 训练导出的模型，不放原始数据
```

#### 待标注样本

待标注样本记录“为什么被采集”和当前模型建议，但不带人工真值：

```json
{"schema_version":2,"sample_id":"tg_01HXYZ","captured_at":"2026-08-28T12:30:15Z","capture_reason":"completion_disagreement","current_text":"我想问你一件事","context":{"scope":"private","pending_user_fragments":[],"recent_turns":[{"role":"user","text":"最近准备去哪里玩"},{"role":"assistant","text":"还没有决定"}],"conversation_active":true,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"review_context":[{"role":"user","text":"最近准备去哪里玩"},{"role":"assistant","text":"还没有决定"}],"predictions":{"completion":"hold_for_more","completion_confidence":0.61,"response":"wait","response_confidence":0.88,"model_version":"turngate-shadow-v0"},"local_source_key":"hmac:v1:..."}
```

字段约束：

- `schema_version` 当前为 `2`；未知版本拒绝进入训练器；
- `sample_id` 为随机 opaque ID，不编码 QQ 号、群号或时间；
- `current_text` 最多 512 个 Unicode 字符；
- `context.pending_user_fragments` 最多 4 条，每条最多 160 个 Unicode 字符；
- `context.recent_turns` 最多 4 条，每条最多 160 个 Unicode 字符；
- `review_context` 最多 4 条，每条最多 160 个字符，仅供人工标注；
- `capture_reason` 只能是受控枚举，例如 `low_confidence`、`completion_disagreement`、`response_disagreement`、`user_feedback`；
- `local_source_key` 只用于本机数据擦除，不导出到训练集，也不送入模型特征。

`predictions` 是观测值，不是真值。`context` 必须是消息到达当时的快照；`review_context` 只能展示这个快照的同一范围，不能包含未来消息、未来的回复结果或标注员事后补写的上下文。训练器只能使用已经写入 `feature_version` 的字段。

#### 已标注训练样本

人工复核后，转换为稳定的训练格式：

```json
{"schema_version":2,"sample_id":"tg_01HXYZ","current_text":"我想问你一件事","context":{"scope":"private","pending_user_fragments":[],"recent_turns":[{"role":"user","text":"最近准备去哪里玩"},{"role":"assistant","text":"还没有决定"}],"conversation_active":true,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"hold_for_more","response":"wait","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":1.0},"feature_version":"char-2-5-v2"}
{"schema_version":2,"sample_id":"tg_01HABC","current_text":"你明天有空吗","context":{"scope":"private","pending_user_fragments":["我想问你一件事"],"recent_turns":[{"role":"assistant","text":"还没有决定"}],"conversation_active":true,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"flush_now","response":"answer","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":1.0},"feature_version":"char-2-5-v2"}
{"schema_version":2,"sample_id":"tg_01HDEF","current_text":"有人今晚打游戏吗","context":{"scope":"group","pending_user_fragments":[],"recent_turns":[{"role":"other_member","text":"今晚好无聊"}],"conversation_active":false,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"flush_now","response":"ignore","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":0.9},"feature_version":"char-2-5-v2"}
{"schema_version":2,"sample_id":"tg_01HIJK","current_text":"谢谢，先这样","context":{"scope":"private","pending_user_fragments":[],"recent_turns":[{"role":"assistant","text":"那就按这个方案试试"}],"conversation_active":true,"bot_last_asked_question":null,"pending_outgoing":false,"pending_task":false,"addressed_to_agent":false,"replies_to_agent":false,"has_image":false,"has_sticker":false,"policy_override":"none"},"labels":{"completion":"flush_now","response":"ignore","policy_override":"none"},"label_provenance":{"source":"human_consensus","annotator_count":2,"agreement":0.9},"feature_version":"char-2-5-v2"}
```

标签枚举：

```text
labels.completion      = flush_now | hold_for_more | abstain
labels.response        = answer | continue | ack | ignore | wait | abstain
labels.policy_override = none | must_reply | command | stop | erase
```

普通私聊绝不使用 `response = null`。`must_reply` 样本也必须有 response 标签，因为它只禁止最终路由选择 `ignore`；只有 `command`、`stop`、`erase` 这类硬策略样本可以将 response 标为 `abstain`，表示该样本不由普通 response head 决定。`abstain` 样本可以留在校准集，但默认不参与强监督分类损失。只有 `human_consensus` 或经过人工确认的 `user_feedback` 才能进入正式训练集；`weak_lexical`、`teacher_minimind` 和 `observed_outcome` 必须保留为候选来源。

训练器不读取 `captured_at`、`sample_id`、`predictions`、`label_provenance` 和 `local_source_key` 作为模型特征。训练集切分使用本机未导出的 conversation/group split key，最终导出的模型不包含这些标识。

## 8. 与现有代码接线

### 8.1 Core

新增建议：

```text
crates/yunxi-core/src/model/turn_gate.rs
crates/yunxi-core/src/model/turn_gate_manifest.rs  # 必要时拆分
```

负责：

- public contract；
- stable feature extraction；
- linear inference；
- manifest and asset validation；
- model health and metrics；
- deterministic fallback。

### 8.2 Plugin

新增建议：

```text
plugins/model/src/yunxi/turn_gate_runtime.rs
plugins/model/src/config/cognitive_model.rs  # 增加 TurnGate 配置段
```

接线点：

1. `private.rs` 和 `group.rs` 在进入 `MessageCoalescer` 时构造同一份 `TurnGateInput`，普通私聊和群聊都请求 completion head；
2. `coalesce.rs` 必须提供原子性的 bounded snapshot + append，避免并发消息在读取 pending fragments 和写入 FIFO 之间产生竞态；
3. `coalesce.rs` 接受 `FlushNow / HoldForMore / Abstain`，不再自行用另一套 lexical 规则重复猜测；`max_wait` 只作为 watchdog；
4. 批次形成后，私聊和群聊共同经过 response head。completion 仍为 `HoldForMore` 时，强制有效 response 为 `Wait`；
5. `answer`、`continue`、`ack` 才能创建回复候选；`ignore` 不创建可见回复；`wait` 进入 pending/event 等待；`abstain` 使用按 scope 的 fallback；
6. 群聊未点名的 `answer` 仍需经过 `reserve_interjection_decision`、频率预算、冷却、Executive 和 `ConversationState`，TurnGate 不能绕过任何一个；
7. `ConversationState` 必须按 `ReplyScope` 同时维护 private 和 group 的有界 recent turns；仅有 `ConversationCoordinator::is_active` 这样的布尔值不足以支持私聊 `continue`；
8. `ConversationCoordinator`、`ConversationState`、`ReplyTicket` 和 pending FIFO 继续负责真实连续任务；
9. Executive 继续决定候选优先级、预算和是否 defer，TurnGate 不能绕过 Executive 或 ActionArbiter。

### 8.3 过渡期接线

第一阶段不能立即删除现有逻辑，且 shadow 必须覆盖私聊和群聊：

```text
TurnGate shadow result
    + existing completion/response result
    -> metrics and disagreement record
```

完成度先切 active，response 在私聊和群聊分别 shadow。私聊先以较低风险的 `answer/continue/ack/ignore` 进行 active 灰度，再开启群聊未点名的 `answer`；群聊误接话成本更高，需要更高 precision 阈值和更充足的 hard negative。

## 9. 分阶段实施

### Phase 0：协议和基准

- 固定文本归一化、hash 和特征向量测试样例；
- 实现 `TurnGateInput`、`CompletionOutput`、`ResponseOutput`、阈值和 `abstain`；
- 加入 `TurnGateMetrics`；
- 不改变生产路由。

验收：Rust 与离线训练器对相同样本得到完全一致的非零特征索引。

### Phase 1：线性 runtime 和模型 bundle

- 实现二进制读取、manifest、SHA-256 和有限值校验；
- 实现缺失/损坏模型的 fail-soft；
- 添加内存、吞吐和并发基准；
- 使用固定 fixture 验证分类结果。

验收：2 核机器上 P95、小于 20MB 常驻内存、错误资产不阻断 Core。

### Phase 2：完成度 active

- 将现有 lexical + MiniMind 分类路径替换为 TurnGate completion 优先；
- completion 在 private/group 两个 scope 统一使用 `flush_now/hold_for_more`；
- `abstain` 进入 normal hold；
- `max_wait` 只保留 watchdog；
- 保留 MiniMind 作为模型缺失或灰区兜底。

验收：半句提前提交率、完整句不必要等待率、连续多条合并正确率。

### Phase 3：接话 shadow

- 为私聊和群聊运行 response head，但不改变发送行为；
- 记录与现有私聊处理结果、群聊 `interjection_worthy` 和最终人工反馈的分歧；
- 观察误接话、漏接话和 CPU 争抢。

验收：按 scope 分层的 response precision、private unnecessary-reply rate、group ambient false-positive rate、连续会话漏接率。

### Phase 4：接话 active

- 私聊和群聊共同接受高置信度的 `answer/continue/ack`；
- 群聊未点名的 `answer` 只有在高置信度和预算允许时接受；
- `continue` 必须经过 ConversationState；
- `wait` 不发送消息，`abstain` 走按 scope 的 fallback，不能一律静默；
- 保留紧急停止、限流和 Executive defer。

验收：不增加直接消息延迟，不发送重复主动消息，不影响 MustExecute。

### Phase 5：持续校准

- 只添加经过授权的新标注数据；
- 新模型版本绑定 manifest 和评估结果；
- 新旧模型 shadow 对比后再切换；
- 不在线修改权重，不让模型使用自己的输出作为唯一训练信号。

## 10. 验收指标

完成度优先关注漏合并和提前截断：

```text
hold_for_more recall >= 0.95
flush_now precision >= 0.98
abstain rate separately reported
```

response 按 scope 关注“该说时说、不该说时不说”：

```text
private answer/continue precision measured separately
private unnecessary-reply rate reported separately
group ambient answer precision >= 0.90
group ambient false-positive rate <= 1%
continue recall measured separately for private/group
```

资源指标：

```text
TurnGate P95 < 5ms on 2-core production-like host
additional RSS < 20MB
no extra process
no model call for command/stop/erase paths
```

行为验收：

- TurnGate 不发送消息，不执行工具，不改变权限；
- completion 不使用时间窗口作为语义真值；
- response 同时覆盖 private 和 group，私聊不使用 `null` 作为“不判断”；
- active conversation 由状态机保持，不由 prompt 伪造；
- 每个新 turn 独立进入队列和 ReplyTicket，不把一大段生成结果拆成假连续消息；
- TurnGate、MiniMind、Strong 任一不可用时，Core、停止、任务和数据擦除仍可用。

## 11. 最终建议

先做一个**共享字符特征、双 head、带 abstain 的 TurnGate 线性分类器**，而不是再引入一个小型生成模型。

短期保留当前 MiniMind 完成度兜底；接话先 shadow；有足够标注数据后再 active。这样 0.1B 模型可以专注于真正的对话生成，TurnGate 则以很小的 CPU 和内存成本承担高频门控任务，适合 2 核 4GB 的长期运行环境。
