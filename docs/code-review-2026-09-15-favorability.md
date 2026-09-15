# 全项目代码评审（2026-09-15）：好感度系统专项

评审范围：整个工作区（`crates/yunxi-core`、`plugins/model`、`apps/yunxi-cli`、`tools/`、
`scripts/`）。本次的重点问题是"好感度系统好像有问题"，因此第一部分是对
`RelationState` 五维的端到端追踪，第二部分是全项目其它子系统的并行评审。

**行号基准：`HEAD = 72eed43`。** 本文所有 `file:line` 都以该提交为准（写本文期间有另一个
会话正在同一个工作区改 `sticker_library.rs` 与 `yunxi/core_model.rs`，工作区当时是红的；
本次评审只读该工作区，结论全部基于上面这个提交）。

基线（评审开始时，工作区干净）：`cargo fmt --check` 干净、`cargo clippy --workspace
--all-targets --all-features -- -D warnings` 干净、`cargo test --workspace` 全绿
（`model` 325 passed / 1 ignored，`yunxi-core` 325 passed / 1 ignored）。也就是说下面这些
都不是 lint 或单元测试能发现的表层问题。

> **2026-09-15 已按这份报告动手修了一轮**，明细见文末「九、修复台账」（每条都有 commit
> 与验证方式，多数带反向对照：先让测试在旧行为下失败，再修）。仍未修的三条也列在那一节。

方法：好感度部分是**穷举式的数据流追踪**（把 `RelationState` 五个字段的每一个写点与
读点在全仓 grep 出来逐条读），不是抽样阅读；每条结论都回到源码复核。其它子系统用
四路并行评审覆盖，其中标「✅ 已复核」的条目由我回到源码逐条重新验证过，标
「代理报告」的尚未逐条复核（每条都带 `file:line`，可单独验证）。

---

## 一、结论先行

好感度（`RelationState.affinity`，后台显示为"好感"）**不是一个坏掉的数字，而是一条断掉的线**。
把五个维度的写点与读点全部列出来之后，问题是四个各自独立、可以分别验证的断点：

| # | 断点 | 一句话 |
| --- | --- | --- |
| 1 | **好感与信任没有任何读点** | 全仓生产代码里 `relation.affinity` / `relation.trust` 只被写、被存、被展示，**从没有被读过**——它不改变任何行为 |
| 2 | **真正在跑的那条判据不碰好感** | 唯一高频生效的相处判据（模型判"友好/不友好"）只写 `tension`；好感唯一的输入是另一个几乎不触发的通道 |
| 3 | **好感唯一的输入是个没写说明的字段** | 好感的唯一来源是语义层 `gratitude` 布尔值，而它是提示词 JSON 模板里**唯一没有字段说明**的那个 |
| 4 | **初值只种一次，且取决于谁先看到她** | legacy 等级 → 好感的投影只在建行时插一次（`ON CONFLICT DO NOTHING`），新用户默认等级 1 投影出 **-0.8**；而另一条路从 0 起步 |

行为读点只有三个，全部与好感无关：

```text
core_model.rs:2507/2509/2511   tension / comfort+familiarity   → 语气提示
core_model.rs:2879             tension                          → 群聊静默门控
bridge.rs:1649                 familiarity                      → 未点名插话放行
```

于是后台"好感"条形图给用户的观感——**永远是 50%（或永远是 10%）**——是真实反映了系统的：
这个数字确实不会动，而且就算动了也没人看。

---

## 二、数据流全景（谁写、谁读、什么时候）

### 2.1 五个字段的全部写点

| 写点 | 位置 | 写哪些列 | 时机 |
| --- | --- | --- | --- |
| Core 回合收尾整行回写 | `relation_store.rs:154-187` | familiarity / affinity / trust / comfort | 每个回合结束（含静默回合），用**回合开始时的快照**算出来 |
| 相处证据 delta 通道 | `relation_store.rs:80-98` | **只有 tension** | 指向她的消息，模型判定后（后台任务） |
| legacy 投影建档 | `relation_store.rs:45-68` + `mod.rs:1253-1276` | 全五列 | 只在**没有行**时插入一次 |
| 便携导入 | `identity_store.rs:672-691` | 全五列 | `#导入` 类操作 |

### 2.2 affinity / trust / comfort / familiarity 的折算

```text
evolve_interaction_state_inner（结构演化，planner.rs:291-411）
  familiarity  ← 每条消息按会话类型 +0.005~0.018（递减步幅）
  comfort      ← 私聊/引用她/点名她 的固定目标混合
  trust        ← 只有 replies_to_agent 时 +0.003
  affinity     ← 只由 cues.gratitude_strength 决定（默认调用恒为 0）

apply_interaction_cues（语义 cues，planner.rs:223-289）
  affinity     ← 0.012 × gratitude × (1 - affinity)
  trust        ← 0.008 × gratitude × (1 - trust)
  comfort      ← 0.18 × gratitude，再被 0.025 混合率衰减一次（净 0.0045）
```

**结论**：`affinity` 与 `trust` 在生产里**只有 gratitude 一个自变量**。
`familiarity` 由每条消息驱动，`comfort` 由会话类型驱动，`tension` 由证据通道驱动。

### 2.3 gratitude 从哪来

两条通道，都要求模型主动给出：

1. **语义理解层**（生产主力）：`model/semantic.rs:141-170` 把
   `MessageUnderstanding.gratitude` 折成 `gratitude_strength = 0.75` →
   `private.rs:706` / `group.rs:994` 的 `project_interaction_cues` →
   `InteractionCuesObserved` 世界事件 → `core_model.rs:5607` 的 `pre_model_plan` →
   `apply_interaction_cues`。
2. **回复 sidecar**（几乎不触发）：模型在可见回复里输出
   `[[INTERACTION_CUES]]{"gratitude_milli":…}` → `core_model.rs:1416-1451`。
   `docs/memory-driven-silence-2026-09-14.md:278` 记录线上 24 小时 `cues=true` **0 次**。

---

## 三、逐条发现

### F1（高 · 确定）好感与信任在整条生产链路里没有读点

穷举 `relation.affinity` / `relation.trust` 的全部出现位置：写点（`planner.rs:248-251`、
`planner.rs:358-362`）、持久化（`relation_store.rs:60/165/173`、`identity_store.rs:678-687`）、
后台展示（`memory_api.rs:673/760-766`、`app.js:2630`）、以及测试断言。**行为路径上零读点。**

对照：`tension` 有 2 个读点、`familiarity` 有 2 个、`comfort` 有 1 个。也就是说
"关系五维"里有两个维度是**只写不读**的。

> 影响：好感度的涨跌目前不可能改变她的任何一句话、任何一个"回不回"的决定。
> 后台那个条形图是纯展示数据。

### F2（高 · 确定）真正在跑的"友好"判据不碰好感

`plugins/model/src/relation_evidence.rs` 是线上唯一高频生效的相处判据：指向她的消息
（结构化 `@` 她 / 正文叫她的名字）触发一次模型判定，给出 `unfriendly | warm | neutral`。

```rust
// relation_evidence.rs:65-74
let step = match self {
    Self::Unfriendly => UNFRIENDLY_TENSION_STEP,  // +0.15
    Self::Warm => WARM_TENSION_STEP,              // -0.05
    Self::Neutral => return None,
};
```

这个 delta 的唯一消费者是 `record_personal_evidence`（`relation_evidence.rs:265-303`），
它调用 `relations.nudge_tension(person_id, strength)`——而 `nudge_tension`
（`relation_store.rs:91`）**只写 tension 一列**。

> 影响：一个持续对她好的人（道谢之外的所有善意：关心、歉意、帮忙、维护她），
> 会让张力下降、comfort 上升，但**好感一点都不涨**。这正是"我对她好，好感度也不动"
> 的直接成因。

### F3（高 · 确定）好感的唯一输入是提示词里唯一没有说明的字段

`model/semantic.rs:264-305` 的语义提示词里，JSON 模板有 16 个字段，随后"字段含义"
逐条解释了其中 15 个（`wants_no_reply`、`wants_stop`、`cross_group_message_request`、
`cross_group_followup_request`、`reminder_request`、`agent_run_request`、`image_intent`、
`image_reference`、`conversation_relevant`、`conversation_end`、`topic_shift`、
`interjection_worthy`、`sticker_reaction`、`interests/personality_traits/topics`、
`group_atmosphere`）——**唯独 `gratitude` 一个字都没写**。

而 `gratitude` 恰好是驱动好感的那个字段（`semantic.rs:168`）。

> 影响：好感的增长完全押在一个**没有语义规格**的布尔字段上。模型只能从字段名猜它
> 什么时候该为 true，判据不可控、不可验收。

### F4（中高 · 确定）好感初值只种一次，且"谁先看到她"决定初值

```rust
// plugins/model/src/yunxi/mod.rs:1253-1276
let affinity = (f32::from(relationship_level) - 5.0) / 5.0;
let trust = (f32::from(relationship_level) - 1.0) / 9.0;
```

- `relationship_level = 1` 是 legacy 里新用户的默认值（`utils.rs:4297`）→ **affinity = -0.8**，
  后台条显示 `(-0.8+1)/2 = 10%`。
- 投影经由 `seed_if_absent` 落库，SQL 是 `ON CONFLICT (person_id) DO NOTHING`
  （`relation_store.rs:52-67`）——只在**没有行**时插入。
- 投影的调用点只有一个：`model/utils.rs:4353` ← `learn_user_profile_from_message`
  ← `model/group.rs:1057`（**Host 群聊路**）。
- 另一条路是 Core 回合收尾的 `set`，它是 `INSERT … ON CONFLICT DO UPDATE`
  （`relation_store.rs:159-170`），新行从**全零**起步。

于是同一个新人，两种初值：

```text
先在群里"路过"被 Host 路看到  → 建档 → 好感 -0.8（后台 10%）
第一条消息就 @ 她走 Core 路   → 建档 → 好感  0.0（后台 50%）
```

更关键的是：legacy 的 `relationship_level` **之后还会继续上涨**
（`utils.rs:4311-4319`：每 20 条互动 +1、每次 gratitude +1、主管理员恒为 10），
但 `seed_if_absent` 再也不会把它同步进 `yunxi_relations`。两套关系系统从此各走各的。

### F5（高 · 确定）关系行读失败被当成"没有关系"，回合收尾把整行写成 0

```rust
// crates/yunxi-core/src/runtime.rs:1544-1550
if let Some(person_id) = person_id {
    if let Ok(relation) = services.relations.get(person_id).await {
        input = input.with_relation(relation);
    }
    ...
}
```

`Err` 与 `Ok(None)` 在这里被合并成同一个 `None`。下游：

```rust
// planner.rs:299-301（evolve_interaction_state_inner 同形，apply_interaction_cues 在 230-232）
let mut relation = relation
    .filter(|state| state.person_id == message.sender && state.validate().is_ok())
    .unwrap_or_else(|| RelationState::new(message.sender));   // 全零
```

然后回合收尾 `apply_state_updates`（`runtime.rs:1024` / `1152`）拿着这个"全零 + 本轮增量"
去 `set`，而 `set` 是 `INSERT … ON CONFLICT DO UPDATE`（`relation_store.rs:159-170`）。

> 影响：**一次瞬时 DB 读失败**（连接池耗尽、超时、行数据校验失败导致的
> `RelationStoreError`）就会把这个人的 familiarity/affinity/trust/comfort **静默清零**，
> 而且是"看起来一切正常"的那种清零。
>
> `tension` 不受影响（单写者只写一列）；`affect` 是同形状的问题
> （`runtime.rs:1548-1550` + `affect_store.rs:108-138`）。
>
> 严重度高的理由是**不可逆**：清零后的值没有任何地方可以恢复。

### F6（中 · 确定）`nudge_tension` 重置了漂移时钟，却没有把漂移落库

```rust
// relation_store.rs:80-98
let Some(current) = self.get(person_id).await?;          // get 内部已按 updated_at 算过漂移
let adjusted = adjust_relation_tension(current, signed_strength);
query("UPDATE yunxi_relations SET tension = $2, updated_at = NOW() WHERE person_id = $1")
```

`get`（`relation_store.rs:139-141`）对**五个维度**都套了 `drift_relation_state`，
但这条 UPDATE 只写 `tension` 一列，把另外四个维度**已经算出来的漂移值丢掉**，
同时把 `updated_at` 推到 `NOW()`。

> 影响：`updated_at` 同时承担了"上次整行写"和"上次张力写"两种含义。只要有指向她的
> 消息（很频繁），这个时钟就不断被重置，于是 comfort（30 天半衰期）、familiarity
> （180 天）、affinity（365 天）、trust（730 天）的漂移**永远不落地**——
> "关系会随时间变淡"的设计对有互动的人完全失效。

### F7（中 · 确定）注释与代码互相矛盾，且测试把错的一侧钉住了

```rust
// planner.rs:248-252  ——仍在改 tension
relation.affinity = (relation.affinity + 0.012 * gratitude * (1.0 - relation.affinity))...;
relation.trust = (relation.trust + 0.008 * gratitude * (1.0 - relation.trust))...;
relation.tension = blend_bounded(relation.tension, 0.0, 0.08 * gratitude, -1.0, 1.0);   // ← 这里

// planner.rs:278-286  ——26 行之后
// **tension 不在这里动。** 它是"证据累积量"，只有 delta 通道能改
// （宿主侧 `relation_store::nudge_tension`）。
```

`docs/memory-driven-silence-2026-09-14.md:270-273`（第四处修正）也明确写着
"Core 侧所有'顺手改张力'的路径全部删除：语义 cues 的 valence → tension、
**gratitude → tension**"。第四处修正清掉了 `evolve_interaction_state_inner` 里那处
（`planner.rs:363-364` 的注释为证），**漏掉了 `apply_interaction_cues` 这处**。

而测试站在错的一侧：

```rust
// planner.rs:1389
assert!(evolved.relation.tension < relation.tension);   // 断言 gratitude 必须降温
```

对照 `planner.rs:1290-1293` 的 `assert_eq!(grateful.relation.tension, baseline.relation.tension, "语义 cues 不得改 tension")`
——同一个概念，两个入口，两条相反的断言。

> 影响：目前被 `set` 不写 tension 兜住，所以是**沉睡的坑**而非线上事故。
> 但 `apps/yunxi-cli/src/state.rs:758` 的 `RelationStore` 是整行写，那边这条是真生效的；
> 将来任何人给 `set` 加回 tension 列，"第四处修正"修的那个 bug 会立刻复活，
> 而测试不会拦住——它正断言着错误行为。

### F8（中 · 确定）后台画的是库里未漂移的值，她实际用的是漂移后的值

`memory_api.rs:673`（列表）与 `:814`（详情）直接 `SELECT` 原始列，**不跑 drift**；
Core 每次读都跑（`relation_store.rs:139-141`）。

> 影响：张力半衰期 3 天。库里 `tension = 0.6`、闲置 6 天 → 她实际按 0.15 行动
> （不会被静默），后台仍然显示 0.6，排查时会被误导成"门控没生效"。

### F9（中 · 确定）人物页只画四维，恰好漏掉唯一驱动行为的那一维

```javascript
// admin/assets/app.js:2630
for (const [key, label] of [['familiarity','熟悉'],['affinity','好感'],['trust','信任'],['comfort','自在']]) {
```

`tension` 在 API 里（`memory_api.rs:765`）但前端不渲染；`README.md:212` 写的是"关系五维"。
同一处的条形图把 `[-1,1]` 线性映射成 `[0,100%]`（`app.js:2636`），**没有零点刻度、没有数字**：

```text
affinity =  0.0  →  条宽 50%   （看起来像"好感度 50%"）
affinity = -0.8  →  条宽 10%
```

> 影响：用户看到的是"好感永远是 50%"，而这一维既不驱动行为、又恰好在视觉上最像
> "坏在中间"。真正该看的张力反而不显示。

### F10（中 · 确定）legacy 画像在 Core 接管的路径上不再更新

`learn_user_profile_from_message` 的**全部**调用点（全仓 grep）：

```text
model/group.rs:1057                        Host 群聊路
model/utils.rs:3939（legacy 私聊路）        private_chat_claimed ← private.rs:857
```

而 `private.rs:799-808` 决定 legacy 私聊路只在 `owner == MessageOwner::Host` 时执行；
普通私聊文本默认由 Core 接管（`lib.rs:787-798` + 架构文档第 7 行）。

> 影响：`relationship_level` / `interests` / `mood_history` 这三样东西对
> **真人对话**基本停更——私聊不再学，被 @ 的消息走 Core 也不学，只剩"群里没点名的闲聊"
> 还在写。而 `relationship_level` 正是 `utils.rs:4205-4215` 那段私聊语气档
> （8-10 亲密 / 5-7 友好 / 1-4 礼貌）与好感投影（F4）的唯一输入。
>
> 这与 `docs/memory-driven-silence-2026-09-14.md:118-119` 记的"同形状的遗留…需要单独
> 核对 Core 路是否另有覆盖"是同一件事：核对结果是 **Core 路没有覆盖**。

### F11（低 · 确定）死代码

- `evolve_interaction_state_with_cues`（`planner.rs:208`）在生产代码里**没有调用点**
  （只有 `planner.rs` 的测试）。它与 `apply_interaction_cues` 各写了一份 cue 折算，
  两份已经不一致（见 F7）——重复实现已经在漂移。
- `MessageReceivedEvent.stop_requested` 在两条入站构造里恒为 `false`
  （`bridge.rs:2217 / 2256 / 2311`）。于是这些分支都是死的：
  `planner.rs:336-337`（comfort 目标 -0.3）、`planner.rs:380`（arousal 下限 0.65）、
  `attention.rs:64`（`StopRequested` 高优先级）、`core_model.rs:2712 / 4971 / 5100 / 5115`
  的 `&& !message.stop_requested` 守卫。
  `docs/memory-driven-silence-2026-09-14.md:272` 已承认"该字段在群聊入站里恒为 false，
  本来就是死代码"，但只清了用到它的那一条路径，字段与其余分支还在。

### F12（低 · 确定）文档/config 注释与代码不符

- `docs/yunxi-core-architecture.md:3633`：说 cues 会更新
  "affinity/trust/comfort/**tension**"。tension 那部分已在第四处修正里删掉；
  而且 sentiment cue 本来也不碰 affinity/trust/comfort（只有 gratitude 碰）。
- `bot.conf.toml:365` 与 `bot.conf.example.toml:766` 首行写
  "（默认只影子观察，不改变任何可见行为）"，而同段第 376/776 行与 `enabled = true`
  都是"默认开启"——首行是改默认值那次留下的残句。
- `bot.conf.toml:371-372` 仍在描述"模型判定的敌意（语义通道）与字面辱骂/驱赶
  （确定性通道）都按同一刻度累加"：字面词表已在第三处修正里整表删除，
  语义通道的张力影响已在第四处修正里删除，这句是双重过期。

---

## 四、已验证为正确的部分

写下来是为了说明覆盖面，避免"没提到 = 没看"：

- **没有并发丢更新（这一条我原本以为有，验证后否定）**。Core 的 runtime 由
  `run_runtime` 单循环驱动（`bridge.rs:3459-3472`），`process_next_with_planner_and_actions…`
  一个事件一次、整个回合（含模型调用）await 到底，`planner_input_with_context`
  读关系（`runtime.rs:1544`）与 `apply_state_updates` 写关系（`runtime.rs:1024/1152`）
  在同一个回合内先后发生。因此 `InteractionCuesObserved` 事件与消息回合的两次整行写
  **不会交错**：cues 事件总是排在消息回合之后，读到的是已写入的状态。
  真正绕过队列的只有 `nudge_tension`，而它按设计只写一列。
- **tension 的单写者设计本身是自洽的**：`set` 完全不碰 tension 并 `RETURNING` 取回库里的真值
  （`relation_store.rs:159-186`），delta 通道只写一列，漂移在两侧都是同一个函数。
- **`adjust_relation_tension` 的刻度与文档一致**：0.15 锚点、`(1-tension) × 0.2 × 强度`、
  置信门槛 0.6，与 `docs/memory-driven-silence-2026-09-14.md:130-149` 的标定表对得上，
  并由 `relation_evidence.rs:344-359` 的测试守着。
- **关系漂移的数学**：`decay_toward`（`planner.rs:486-497`）用 `exp(-ln2·t/half_life)`
  朝目标衰减，时钟回拨与 60 秒内抖动都被 `elapsed_for_drift` 挡掉
  （`relation_store.rs:190-199`，有单测）。
- **`seed_if_absent` 不会覆盖已有的 Core 演化**（`ON CONFLICT DO NOTHING`，有 PG 集成测试）。
- **时区/取整/边界**：`InteractionCues` 的毫秒整数化（`event.rs:745-790`）与
  `[0,1000]`/`[-1000,1000]` 的边界校验是 fail-closed 的，越界整条丢弃而不是截断。

---

## 五、其它子系统

### 5.1 关键（✅ 已复核：我逐条回源码验证过）

#### O1（严重 · ✅）"改主意"必然失败，并把整批反思一起回滚

信念（belief）被退休时版本号会自增两次，而存储层要求"每次 upsert 恰好 +1"，
于是只要模型提出一次"改主意"，整个反思批次（信念 + 偏好 + 兴趣 + 未解问题 + 议程 + 情节）
全部 `VersionConflict` 回滚。

```text
mind_runtime.rs:3404          改主意路径给 Retract 提案写 valid_until = Some(now)
consolidation.rs:467          let expected = existing.version()          // V
belief.rs:244 (apply_delta)   version = V + 1
belief.rs:258 (retired_at)    version = V + 2                              // ← 第二次自增
consolidation.rs:485          MindUpsert { value(V+2), expected_version: Some(V) }
mind_store.rs:690             Some(expected) if record.version != expected + 1 → VersionConflict
```

- `retired_at`（`belief.rs:255-262`）是 `valid_until` 的唯一生产者，`consolidation.rs:481`
  是它唯一的调用点，`mind_runtime.rs:3404` 是它唯一的业务触发者。
- `postgres_mind_store::apply`（`mind_store.rs:1654-1742`）整批在**一个事务**里，
  `put_record_tx(...)?` 一失败事务即回滚；`consolidate_retry`（`mind_runtime.rs:2554-2573`）
  只重试 `StaleSnapshot`，版本冲突直接当硬错误返回。
- **影响**：她永远无法真正"改变看法"——旧的那条退休不掉，新的那条也立不起来，
  同一批里其它本来能落库的更新一起丢。`crates/yunxi-core/src/mind/in_memory.rs:779-787`
  是同一份契约的内存实现，同样拒绝。
- 评审代理用一个针对仓库自身 `libyunxi_core.rlib` 编译的临时程序复现过：
  `seeded belief version = 1 / planned version = 3 / RESULT: consolidate FAILED`。

**修法（三选一，推荐第一个）**：让 `retired_at` 不自增版本（退休不是一次新的证据更新）；
或把 `expected_version` 写成 `Some(expected + 1)`；或把退休折进 `apply_delta` 一次完成。

#### O2（高 · ✅ · 隐私）群聊回合拿到发送者的私聊状态

```text
runtime.rs:2019-2026   event_person_id: MessageReceived(_) => Some(message.sender)   // 无会话类型闸门
runtime.rs:1503-1519   无条件按 MemoryScope::Person / OpenLoopOwner::Person / GoalOwner::Person 取
runtime.rs:1544-1551   无条件注入该人的 relation + affect
core_model.rs:5987-6008 记忆渲染只 join memory.content()，take(32)，**不带作用域标签**
```

对照同一份状态在 Mind 那条路上的规矩：`mind/snapshot.rs:594` 用
`conversation_kind == ConversationKind::Direct` 决定要不要带私聊状态，注释写明
"…so the private Mind scope can be restored **without guessing in group chat**"。
而 person 作用域的记忆只在私聊写入（`memory_writeback.rs:508-521`）。

- **触发**：A 先和她在私聊里聊过（写入 person 记忆 / 私聊关系 / 私聊情绪），
  之后 A 在群里 @ 她 → 她的群聊回复提示词里带着 A 的私聊记忆、她私下对 A 的关系与情绪。
- **影响**：私聊内容在群里被说出来的风险；且这是 fail-open（没有配置开关）。
- **修法**：把 `runtime.rs:1503-1551` 与 `event_person_id` 一并按
  `conversation_kind == Direct`（或 `EventScope::Person`）收口，与 Mind 判定对齐。

#### O3（高 · ✅）后台"原始 TOML"保存一次就把密钥写成 `********`

```text
config_api.rs:241-247   非 typed 文件用 mask_generic 打码
config_api.rs:258       "raw": mask_raw_secrets(&raw, &masked)   ← 所有文件都打码
config_api.rs:288-294   write_raw: if file.typed { restore_masked_secrets(..) } else { body.raw.clone() }
config_api.rs:105-112   kovi.conf.toml / kovi.plugin.toml 的 typed: false
```

`kovi.conf.toml` 里有 `access_token`（连 NapCat/OneBot 的令牌）。
打开「Kovi 框架配置 → 原始 TOML」时编辑器里就是 `********`，一个字不改点保存，
落盘的就是 `********`——**下一次重启机器人就连不上 NapCat**。代码自己两行上的注释
（"否则管理员在原始编辑器里点一次保存，所有密钥就变成 `********` 落盘"）与弹窗提示
（`app.js:1368`）都承诺不会发生。同一根因的变体：还原清单只覆盖 `ModelConfig` 认识的键，
`#[serde(default)]` 且无 `deny_unknown_fields`，所以叫 `*_token` 但模型不认识的键
同样是打码后原样写回。

#### O4（高 · ✅）记忆页会 panic：`short_id` 按字节切中文

```rust
// memory_api.rs:1553-1559
fn short_id(id: &str) -> String {
    if id.len() > 8 { id[..8].to_string() } else { id.to_string() }
}
```

`readable_title`（`:1440-1450`）在标题是 `{"type":…,"value":…}` 时走
`format!("{kind} {}", short_id(value))`；议程行的 `subject` 正是这种 JSON
（`AgendaSubject::SocialMotive(String)`），值是中文时 8 字节切在字符中间 → panic。
`/api/memory/records`、`/graph`、`/overview`、`/person/{id}` 都走这条渲染。
同文件几行之下的 `truncate()` 是按 `chars()` 截的——只有这一处漏了。

### 5.2 高/中（代理报告，未逐条复核）

**World Model（会展 `world_model`）**

| # | 位置 | 问题 |
| --- | --- | --- |
| W1 | `world_model/mod.rs:859-876` + `plugins/model/src/yunxi/world_model.rs:552,648` | `prune_expired` 从不清理 `situations`，唯一的删除路径 `erase_person`/`erase_conversation` 全仓无调用点；而两条创建路径用 `world.situations().len() < 8` 按**全局总数**把关 ⇒ 历史上攒够 8 条情境后，任何会话都再也记不进新情境，且会被 `save_world` 反复持久化、重启后依旧 |
| W2 | `world_model/mod.rs:644-657` | `MAX_ACTIVE_SITUATIONS_PER_SCOPE` 实际按全局计数，且用 `status()==Active` 而别处用 `is_active()`（`Unknown` 状态两者不一致），命名与语义不符 |
| W3 | `world_model/mod.rs:878-915` | `erase_person`/`erase_conversation` 的文档写"Erase every world-model record linked to the person / Used by data deletion flows"，但 13 个字段只清了 5 个（`observations`/`predictions`/`prediction_errors`/`timeline`/`causal`/`environment` 没动，而 observation 里存着正文），且生产擦除走的是 SQL，随后 `save_world` 会把内存快照整表写回 |
| W4 | `hypothesis.rs:283-293`、`situation.rs:371-378`、`entity.rs:475-478` | 先赋值后 `validate()`：失败时对象停在非法状态；`WorldModel::validate()` 从此永远失败，宿主"拒绝持久化"（`world_model_store.rs:210-212`）——一处坏调用冻结整个世界模型的落库 |
| W5 | `social_scene.rs:15` / `environment.rs:15-16,361-372` | 场景/宿主/工具上限只拒不留（唯一删除路径无调用点）⇒ 超过 256 个会话、32 个宿主、128 个工具后，**新的**会话/工具永远记不进来 |
| W6 | `fallback.rs:189-203` vs `:138-143` | `select()` 会把 `strong_available` 按"强档是否真的存在"掩掉，而 Intrinsic 复检用的是未掩码的原始 capability ⇒ `strong: None` 时判成 `Strong ≠ Intrinsic` 直接 `Err(Unavailable)`，Intrinsic 永远不被调用（与自己上面的注释矛盾）。当前仅测试可达 |
| W7 | `world_model_store.rs:293-317` + `hypothesis.rs:132-144` | `Hypothesis` 没有 restore 构造器，`status`/`updated_at`/`version` 写了不读：重启后 `Supported/Refuted` 一律变回 `Active`，`freshness_at` 也随之偏移 |
| W8 | `executive/mod.rs:330-346` | Global 作用域的 setter 会 `state.goals = goals; state.goal_scopes.clear()`——把其它作用域的目标与归属索引一起清掉，与它上面一行的文档相反；跨作用域也没有总量上限（对比 `MAX_ACTIVE_PLANS`） |
| W9 | `arbiter.rs:927-931` | `last_by_scope` 只增不删，超过 4096 个作用域后**新会话**永久被拒，且报的是 `IdempotencyStateFull`（条件名不对）。当前仅测试可达（生产 cooldown 为 0） |

**Admin（后台）**

| # | 位置 | 问题 |
| --- | --- | --- |
| A1 | `app.js:4377-4383` | `boot()` 里任何一次渲染失败都被当成"没登录"：PostgreSQL 挂了 + `#/memory` 直接跳登录页，重登也进不去——控制台恰好在出事时不可用 |
| A2 | `config_api.rs:439` | `POST /api/config/patch` 的响应里 `"raw": candidate` 是**未打码**的整份文件（含 `admin.token`、`server_config.api_key`），与 GET 侧的刻意打码相反，明文进 devtools / 浏览器缓存 / 任何日志代理 |
| A3 | `memory_api.rs:479-508` | 标签过滤只 `retain` 了 `items`，没有重算 `total`/`counts` ⇒ 命中 3 条也显示"共 5000 条"、翻页越翻越空 |
| A4 | `memory_api.rs:474-475` | 每类抓取上限 200，`total` 却是真实总数 ⇒ 深页恒空，与头部数字矛盾 |
| A5 | `app.js:2673` | 相处结论的 `confidence_milli` 是千分比，却按 `/100` 渲染 ⇒ 0.13 显示成「置信 1.30」（另一个消费方 `mind_runtime.rs:3316` 是 `/1000`） |
| A6 | `app.js:2192-2200` | 星座图用 `occurred_at` 的极值去归一化 `mentioned_at`（而 `mentioned_at ≥ occurred_at`）⇒ 新节点全部顶到最暖色，图例描述的还是另一条序列 |
| A7 | `app.js:1574-1620` | 渲染没有并发保护（`goto` 不防重入）：双击"记忆"或点统计卡 ⇒ 两倍卡片/表格叠在一起 |

**Core runtime**

| # | 位置 | 问题 |
| --- | --- | --- |
| R1 | `runtime.rs:1250-1260` + `:2281-2293` + `:1365-1371` | `ToolCompleted` 被算作"投递成功"，且 `requires_delivery` 实际是"有任意非 Noop 意图"⇒ `[UseTool, ResolveOpenLoop]` 会在什么都没送到人面前时把到期的未完结线索结掉（失败的那条有测试，成功的没有） |
| R2 | `runtime.rs:1169-1179` | 回合中途 guard 失效时返回 `silent()` + 空 `actions`，把**已经执行并投递**的结果一起丢掉；宿主因此对一条已在网上的消息调用 `record_silent_turn`（`bridge.rs:3466/3496/3576-3588`）⇒ 长期记忆里记成"她没说话" |
| R3 | `runtime.rs:744-765` | 工具预算溢出分支丢掉最老的 follow-up 时没有 `release_tool_budget_root_if_terminal`（其它消费路径都调了）⇒ 孤儿项只增不减，攒满 1024 后所有新回合的工具能力被永久关闭，直到重启 |

### 5.3 低（代理报告）

- `runtime.rs`：person 记忆被尾插后又被消费方 `take(32)` 截掉（有 32 条会话记忆时私聊记忆全被挤掉）；
  `RuntimeConfig { max_trace_depth: 0 }` 被接受而所有派生事件都是 `.ok()?`（反馈被静默丢弃）；
  `tool_action_budget_order` 只写不读；`:2076-2082` 的去重注释描述了代码没做的事。
- `executive/attention_budget.rs:235-241` 用 `as_secs()/60` 丢余数（兄弟实现用 `as_secs_f32()`）；
  `executive/confidence.rs:194-201` 传 `NaN` 会 panic 而不是 fail-safe；
  `executive/mod.rs:465-476` 的 `Violated`/`Cancelled` 落进空分支后被删（`Expectation::violate()` 无调用点）；
  `plan.rs:276-282` `revise()` 失败后对象停在 `Failed`；`conflict.rs:328-330` 用集合比较参与者（`[A,A,B] == [A,B,B]`）；
  `policy.rs:60-66` 不校验 `critical_attention_reserve` 的有限性。
- `world_model`：`hypothesis.rs:291` `None`（永不过期）在 `max()` 下反被赋上 TTL；
  `mod.rs:861-871` `prune_expired` 返回值少算 uncertainties/predictions；
  `mod.rs:740-748` `add_uncertainty` 无上限；`simulation.rs:110-114` 的时间校验恒假；
  `snapshot.rs:1085-1094,1237` `is_empty` 漏 `causal`、causal 只取第一个 person；
  `mod.rs:8-15` 两处模块级文档声明与实现不符；`working_state.rs:312-323` 版本 ABA（会话被 LRU 淘汰后重建又回到 1）；
  `goal.rs:304-309` 不可达分支；`conversation.rs:362-370` 只可能在时钟回拨时命中。
- `turn_gate.rs:614-618` 资产 `path` 未做目录包含校验（兄弟实现 `model/manifest.rs:31-39` 做了）；
  `turn_gate.rs:64-81` 字段分隔符可被正文注入；`:188-189` 文档承诺的 160 字截断在 core 侧未实现（当前靠 `coalesce.rs:212-217` 兜住，latent）。
- Admin：`mod.rs:129` 常量时间比较的长度差被 `as u8` 截断；`/?token=` 登录路径没有失败限速（`auth.rs:318-341`）；
  `memory_api.rs:474` `offset+limit` 可溢出 panic；`kinds` 不去重；`mod.rs:371` `/api/memory/overview` 无调用方；
  图谱节点用裸 `id` 作键（跨类型会撞）；`app.js:2565` 详情打印 `undefined`；
  `app.js:1136` "已改动"高亮 CSS 选择器对不上；`:4082-4091` 回车会劫持按钮；时间线视图没有翻页；
  标签/文件列表缓存不刷新；标注"下一条"按文件序而非队列序；错误信息回显原始 DB 文本与绝对路径。

### 5.4 代理报告"已验证正确"的部分（用于界定覆盖面）

- **Core 并发**：`ExecutiveController` 全部方法同步、临界区有界、模型调用前先克隆快照，
  "不持锁等待模型"的不变量成立；`ActionArbiter` 所有状态转移在同一个 `Mutex` 下，
  `arbiter.rs` 内没有跨 `.await` 持锁；幂等键 `(key, action_id)` 的构造与重复成功处理正确。
- **`event.rs` / `working_state.rs` / `goal.rs` / `open_loop.rs` / `proactive.rs` / `delivery.rs`**：
  作用域校验、trace 深度、文本按字符与字节双重有界、删除时标识清理、终态不可复活等均与文档一致。
- **Admin**：所有 `/api/*` 都在 `require_session` 之后；静态资源是 `include_str!` 常量（无路径穿越）；
  challenge 先消费后比较（不可重放）；cookie HttpOnly + SameSite=Strict；
  `format!` 拼 SQL 的地方只拼 `&'static str` 白名单常量，用户输入一律 bind；
  前端唯一 HTML sink 是未被调用的 `h({html})` 分支，其余全走 `textContent`。
- **`bars()` 的映射本身是对的**：关系字段被 `[-1,1]` 约束（`planner.rs:106-110`），
  所以 `(v+1)/2*100` 是正确画法——问题在"没有零点标记"与"漏画 tension"，不在算术。

---

## 六、建议的修复顺序

好感度那条线（F1–F4）是产品语义问题，要先定方向；其它几条是纯正确性问题，可以直接修。
建议顺序：

1. **O1（改主意必失败）** —— 纯 bug，修法明确，影响面是"整个反思批次"，优先级最高。
2. **O2（群聊拿到私聊状态）、O3（密钥被写成 `********`）** —— 两个 fail-open 的隐私/凭据问题，
   改动都局限在单点（recall 闸门 / 还原分支）。
3. **F5（读失败清零关系）** —— 一行的判断，代价是"关系被静默清零"，建议立刻收口。
4. **O4、A1–A4** —— 后台的 panic 与"看到的不等于真的"，排查时会误导人。
5. **F2 + F1（让"友好"能加好感、并让好感真的影响行为）** —— 这是"好感度系统有问题"的正面回答，
   但需要先决定好感到底要不要驱动行为（见下）。
6. **F6（漂移时钟被重置）、F7（`apply_interaction_cues` 仍在改 tension）、F8/F9（后台展示）** ——
   正确性与可观测性。
7. **文档/config 注释（F12）与死代码（F11）** —— 顺手清。

## 七、需要你拍板的三个问题

1. **好感（affinity）要不要驱动行为？**
   - 方案 A（推荐）：接通——让"友好"证据也能加好感（新增一条**单列 delta 通道**，
     避免又变成整行读改写），并让 `affect_tone_guidance` 多一档好感语气。
   - 方案 B：不接通——那就把后台的"好感"标注成"仅展示、不参与行为"，
     并在架构文档里写清楚，免得下次又被当成坏的。
2. **tension 的语义边界**：`apply_interaction_cues` 里那行 `gratitude → tension`
   （`planner.rs:252`）留还是删？留 = 承认"语义通道也能降温"，
   删 = 兑现"tension 单写者"的文档与注释（同时要改 `planner.rs:1389` 那条反向断言）。
3. **legacy 画像与 Core 关系二选一**：`relationship_level` 现在只在 Host 群聊路更新，
   私聊与被 @ 的消息都不再学（F10）。是把学习搬进 Core 路，还是明确它只服务 Host 路并写进文档？

## 八、本次评审的覆盖边界

四路并行评审里，**Core runtime** 与 **Admin 后台** 两路跑完并已收录（5.1–5.4）；
**存储/mind** 一路只来得及交出头条结论（即上面 O1，已由我回源码复核），
**model 生成层**（`model/{private,group,reply,interrupt,conversation_coordinator,tool_access,
memory_query,utils}.rs`、`memory/mod.rs`）一路被中途叫停——原因是写本文期间有**另一个会话
正在同一个工作区**改 `sticker_library.rs` / `yunxi/core_model.rs` / `delivery.rs`
（在修"QQ 上发不出表情包"），工作区当时编译不过；继续让评审代理跑 `cargo`
既拿不到可信基线，也会和那边的构建抢 `target/` 锁。这两路都是可续跑的，
需要时再补。

---

## 九、修复台账（2026-09-15）

按这份报告动手修了一轮。每条都跑了测试；带「反向对照」的条目是**先把修复回退、
看着新测试失败**，再恢复——用来证明测试真的钉住了那个 bug，而不是恰好通过。

### 好感度主线

| 条目 | 改动 | Commit |
| --- | --- | --- |
| F1 好感不驱动行为 | 语气档新增三档好感措辞（喜欢 / 有好感 / 提不起劲，按"张力 → 好感 → 熟悉度"判）；群成员关系缓存从"熟悉度"扩成"熟悉度 + 好感"，任一过线都让未点名消息进语义评估（新配置 `group_interjection.affinity_threshold`，默认 0.5） | `f26bec3` |
| F2 友好证据不加好感 | Core 新增 `relation_evidence_nudge(signed_hostility)`：一条证据同时表达短期冷暖与长期好感/信任（符号沿用 `tension_delta` 的"正=不友好"）；相处证据通道与道谢通道都走它 | `0a0ab39` `f26bec3` |
| F3 gratitude 没有字段说明 | 语义提示词补上一段判据（客套、转述、反讽、谢第三方都不算，判断不了就是 false） | `f26bec3` |
| F4 新用户被投影成 -0.8 好感 | 等级 1..=4（礼貌/正式）一律中性起步，5..=10 线性给到 0.6 | `f26bec3` |
| F5 读失败把关系清零 | `PlannerInput` 新增 `PersonStateRead`：读失败时本轮的 affect/relation 回写被拒（按存储分别 fail-closed），并在插件侧留一行 `[WARN]` | `f26bec3` |
| F6 漂移时钟被重置 | `set` 与 `nudge` 都把别列的漂移放进同一条 UPDATE（半衰期由 `relation_half_lives` 单点提供）；真实 PostgreSQL 集成测试验证 | `0a0ab39` |
| F7 注释与代码矛盾 | `apply_interaction_cues` 不再改关系（只改情绪），tension 单写者的注释与实现一致；那条钉住错误行为的断言改成钉住新不变量 | `0a0ab39` |
| F8 后台显示未漂移的值 | 人物列表与详情都跑同一份 `drift_relation_state`，原值一并返回便于对账 | `7ad59dd` |
| F9 人物页漏张力、无零点、无数字 | 五维齐全、以 0 为中心（中线 + 负值 danger 色）、右侧给出数值 | `c64449f` |
| F11 死代码 | 删掉生产无调用点的 `evolve_interaction_state_with_cues`（它与 `apply_interaction_cues` 各写一份 cue 折算，已经漂移） | `0a0ab39` |
| F12 文档/config 过期 | 静默门控的判据段、架构文档的 Relation 行 | `742e49e` `e3fe502` |

### 其它子系统

| 条目 | 改动 | Commit |
| --- | --- | --- |
| O1 改主意必然失败 | 证据增量与退休收成一次 `Belief::apply_update`，版本只自增一次；**反向对照**复现 `VersionConflict` | `86c8686` `5b03613` |
| O2 群聊注入私聊状态 | person 作用域的记忆/未完结线索/目标只在私聊或 Person 作用域事件里展开；关系与情绪保留（语气档与静默门控要用） | `57129d5` |
| O3 原始 TOML 把密钥写成星号 | 读接口与写接口共用同一份打码清单（schema 标注 ∪ 名字像密钥），非类型化文件也还原 | `7ad59dd` |
| O4 记忆页按字节切中文 panic | `short_id` 按字符截 | `7ad59dd` |
| A1 渲染失败被当成未登录 | 会话检查与首屏渲染分开；渲染失败留在控制台并把原因写在当前页 | `c64449f` |
| A2 patch 回显明文密钥 | 响应里的 `raw` 与读接口一样打码 | `7ad59dd` |
| A3/A4 页码条承诺翻不到的页 | 过滤后的计数在窗口上重算，未过滤时也把总数收口到窗口大小，并写明"在最近 N 条里统计" | `7ad59dd` `c64449f` |
| A5 置信度放大 10 倍 | `confidence_milli` 按千分比渲染，时间戳改用 `fmtTime` | `c64449f` |
| A6 星座图配色错配 | 按**正在画的那条序列**归一化；接口同时返回 occurred/mentioned 两份范围 | `b0d6796` |
| A7 渲染可重入 | 渲染串行 + 只画最新一页 | `b0d6796` |
| A-lows | 常量时间比较不再丢长度信号、`kinds` 去重、`offset` 溢出、记录详情 `undefined`、"已改动"高亮选择器、标注页回车劫持 | `c5a2efb` `b0d6796` |
| R1 工具成功当投递成功 | 只有 `SendMessage`/`ReachOut` 算送达；没送到就走既有的"不排期"恢复路径；**反向对照**复现"线索被提前结掉" | `d0a1b3b` |
| R2 中途取消丢弃已执行结果 | 返回"已执行的前缀"，宿主不再把已送达的消息记成静默 | `93b86df` |
| R3 工具预算孤儿 | 溢出分支归还被挤掉那一根的预算；**反向对照**复现台账不释放 | `0338677` |
| W1 情境只增不减 | `prune_expired` 退役终态情境（24 小时宽限 + 256 条硬上限）；宿主侧那道"总数 < 8"的门拆掉，名额交给 Core | `b901270` `cf05dd7` |
| W2 名额按全局计数 | 改成按作用域计数，判据统一 `is_active()` | `b901270` |
| W3 擦除不完整 | 观察/预测/预测误差/因果知识一并按作用域清理，`CausalKnowledge` 补擦除方法 | `b901270` |
| W4 先改后验留半成品 | 三处（假设合并、情境转换、实体更新）改成副本上完成、验过再落；顺带修 `expires_at` 的 `None` 被 TTL 覆盖；**反向对照**复现"目标被改坏" | `0c98387` |
| W6 Intrinsic 永远不可用 | 复检用与选档同一份掩码后的能力快照；**反向对照**复现 `Unavailable` | `eff4452` |

### 第二轮补修（2026-09-15 下午）

| 条目 | 改动 | Commit |
| --- | --- | --- |
| W9 冷却名额只增不删 | 容量检查前按过期回收；真满时用新的 `CooldownStateFull`（原来的报错名和原因对不上）；**反向对照**复现"新会话被拒" | `0c33e67` |
| Executive 四小处 | `max_delta = NaN` 会 panic；`CognitiveBudget::replenish` 丢余数（每 30 秒补一次的路径永远补 0）；`critical_attention_reserve` 的 NaN 能通过校验；`same_participants` 是集合比较（`[A,A,B] == [A,B,B]`）；`revise` 失败路径顺手把计划置成终态。两条做了反向对照 | `3754993` `6802c09` |
| W7 假设装载丢三列 | 补 `Hypothesis::restore`，`status`/`updated_at`/`version` 真正读回；**PostgreSQL 往返测试** + 反向对照 | `e70a6f3` |
| turn_gate 资产路径 | 补 `canonicalize` + `starts_with(root)` 包含校验（对齐兄弟实现）；反向对照复现 | `4c6a935` |
| W8 Executive 目标 | Global 投影不再清掉别人作用域（与文档承诺相反）；补跨作用域上限 `MAX_GOALS_PER_CONTROLLER`；反向对照复现 | `d7419a8` |
| 世界模型四处查漏 | 不确定项补上限（唯一无界 mutator）；因果快照带上上下文里的每个参与者（此前只取第一个）；删掉永假的时间校验并补 `validate_at`；删掉 `Cancelled → Active` 不可达分支。反向对照复现因果快照丢人 | `0120eae` |
| 私聊无关系信号 | 见第十节：私聊接进相处证据通道 | `f88702a` |

### 第三轮（2026-09-15 下午第二批）

| 条目 | 改动 | Commit |
| --- | --- | --- |
| F10 Core 回合的语义理解 | 见 10.3：`bridge::resolve_and_submit_inner` 上补跑一次理解（`visible_reply_allowed` 保证恰好一次），开关 `[understanding] core_owned_enabled` 默认 true | `ea6bbfd` |
| L10 时间线视图没有翻页 | 补上同一个分页器；头部那行写清"本页 N 条"并给出全量总数 | `4670a5b` |
| L12 标注「下一条」按编号跳 | 改成按队列顺序（后端按 `(tier, Reverse(richness), index)` 排的），队尾回队首 | `4670a5b` |

### 仍未修（需要单独判断，不在这轮授权范围内自行拍板）

**这一节已经清空**：上面三条分别在 `ea6bbfd`（F10）、`b901270`/`99dbbc3`（W5 及
同族的上限语义）、以及第二轮那批（W7 `e70a6f3`、W8 `d7419a8`、W9 `0c33e67`、
turn_gate `4c6a935`）里修掉了；5.3 的 LOW 也逐条过了一遍（能修的都修了，剩下的
是"当前没有生产调用点"的 latent 项与文档性的措辞问题）。

剩下唯一需要你判断的是**成本口径**：`[understanding] core_owned_enabled` 默认开着，
意味着每条 Core 可见消息多一次有界分类调用；不想要就写 false（代价是那些回合不再
更新情绪/兴趣/关系等级）。

---

## 十、第二轮（2026-09-15 下午）：Core 接管之后，私聊这条路断在哪儿

第一轮把好感/信任接进了证据通道，但接着问"证据从哪来"时发现了一个更靠上游的断点。

### 10.1 发现：语义理解层只在 Host 那条路上跑

`understand()`（会话理解：mood / gratitude / interests / 画像学习）全仓只有五个调用
点：`model/private.rs:705`、`model/group.rs:991`、`proactive_chat`、`mood_system` 两处。
前两个都在 **Host 专属**的处理器里——`private_message_event_after_ingress` 与
`group_message_event_after_ingress` 只有 `owner == Host` 时才会被调用
（`lib.rs:807` / `lib.rs:724`）。

而普通私聊文本与"指向她"的群消息都是 **Core 接管**的
（`bridge::handles_private` / `supports_group`，cutover 默认开）。结论：

| 消息类型 | 归谁 | 语义理解 | 相处证据（关系/情绪输入） |
| --- | --- | --- | --- |
| 私聊普通文本 | Core | ❌ 不跑 | 修复前 ❌ 完全没有 |
| 群聊 · 指向她 | Core | ❌ 不跑 | ✅ 群聊入站闭包上有（我第一轮修的那条） |
| 群聊 · 未点名被抽样 | Host | ✅ 跑 | ✅ |

也就是说：**私聊这条路对情绪、兴趣、性格、关系等级、好感全都没有输入**——而私聊是
信号最强的 1:1 通道。README:484 承诺的"私聊还会持续更新用户的兴趣、性格、关系等级
和情绪历史"，自 Core 接管起就不再成立。

这也解释了另一件事：`[[INTERACTION_CUES]]` 回复 sidecar 那条通道（文档记为线上 24
小时 0 次）并不是"模型不配合"——Core 的提示词明确禁止输出协议标记，并有一条测试
钉着（`core_model.rs:8795`）。它是一条没有生产者的通道。

### 10.2 已修：私聊接进相处证据通道（`f88702a`）

复用群里那套已经上线的判据，不新造机制：同一个 prompt、同一个模型、同一个折算刻度
（`relation_evidence_nudge`）、同一个后台不阻塞形态、同一个开关
（`[silence] relation_evidence_model_enabled`，默认 true）。差别只有两点：1:1 天然
"指向她"，所以不判定向；不写群级气氛。

挂在 `lib.rs` 的私聊入站闭包上，与群聊那条同一个位置、同一条理由：与这一轮归 Host
还是归 Core 无关。**成本**：每条私聊消息多一次有界分类调用（输出 ≤160 token、
输入截断 400 字），与群里那条同量级；关掉开关即回到原状。

### 10.3 已修（`ea6bbfd`）：Core 回合也跑一次完整语义理解

按下面第 2 种选择实现：挂在 `bridge::resolve_and_submit_inner` 上——它是**所有**
交给 Core 的消息的唯一汇聚点（入站闭包只覆盖"新消息"，接续合批与私聊窗口排空同样
走这里）。判据是 `visible_reply_allowed`，也就是"这一条归 Core 的可见回合"：观察型
消息（未点名的群聊背景流量）由 Host 处理，那两条处理器自己会跑一次理解，不排掉它们
`interaction_count` 就会涨两次。

开关 `[understanding] core_owned_enabled`（默认 true；本轮之前的行为等于 false），
后台任务 + 15 秒超时，结果只喂情绪/关系/画像，**不参与**这一轮回不回、回什么。

一个已知的口径重叠：私聊里"谢谢你"这类消息会同时被相处证据判据（warm）和理解层
（gratitude）看到，两条都会给好感加一点。按当前锚点，理解层那次是 0.029、相处证据
那次是 0.002（约 7%），可以忽略；敌意只有相处证据那一路能判（理解层刻意不把情绪
当成"冲着她"的证据）。

### 10.4 原始结论（保留）

情绪、兴趣、性格、关系等级的更新仍然只在 Host 路上发生。要让它们在 Core 回合也成立，
只有一条路：**为 Core 接管的回合也跑一次语义理解**，代价是每条消息多一次分类调用
（输入上限 6000 字，输出上限 420 token）。

三种选择：

1. **不修**：接受"Core 回合不更新画像与情绪"，但把 README 与相关注释改成事实，
   免得下一个人再按文档去查为什么兴趣一直是空的。
2. **全开**（推荐）：Core 回合也跑一次 `understand` + `project_interaction_cues` +
   `learn_user_profile_from_message`，加一个开关（默认开）与一行成本说明。这是把
   README 承诺的行为接回来。
3. **只补画像**：不跑完整理解，只把 `relationship_level`/`interests` 的更新接回
   Core 路——但那条路径本身就是理解层的产物，等于半个方案。

我倾向 **2**，但它每次都要花钱且影响画像涨速，属于"成本与产品取舍"，没有替你拍板。

## 十一、第三轮修复台账（2026-09-15 晚，无人值守批次）

本批把"能客观判定对错"的问题全部修完，并把两条此前没审完的线（存储/mind、模型生成层）
补完。所有条目都跑了测试，关键改动做了**阴性对照**（把修复还原后测试必须失败）。

### 11.1 已修（14 个 commit）

| 提交 | 修了什么 | 怎么验证的 |
| --- | --- | --- |
| `0e48bee` | 后台标签表 / 配置文件名与备份数缓存永不失效；进页面、保存、按磁盘重载时置脏；选中文件消失时退回主文件 | node 抽出 `fetchTagList` 与文件列表守卫，4 项检查；改回旧写法后"置脏应重拉"失败 |
| `bf0da37` | 人物页 `共 N 人` 却只画 60 张卡片且无翻页；翻页条参数化，人物页与记录页页码各自独立 | node 抽出 `renderPager`，5 项检查（区间、禁用态、越界钳制、空结果、窗口口径） |
| `82dc9ca` | 放开 `allow_non_loopback` 后页面仍写"只监听回环地址"；按实际绑定渲染两种文案 | 新增 `index_states_the_actual_bind_scope`，两种绑定各取一次响应体 |
| `72b1f64` | 「全部展开/折叠」字面与动作相反；「再加载 N 条」写的是窗口总量而非本次追加量、到顶后按钮点了没反应；删死常量 `MASK` | node 抽出 `syncExpandButton`、`setOpen` 改动路径与追加量算式，4 项检查 |
| `1a3e607` | 图谱节点用裸 id 当身份，跨类型（v2 与旧表共享 UUID、档案 id 就是 QQ 号）撞车导致连线丢失、度数挂错人 | 新增 `link_endpoints_use_namespaced_keys`，改回裸 id 后立刻失败；node 侧 5 项折叠检查 |
| `319a769` | 对已了结的问题/议程重复提案会让**整批反思**以 VersionConflict 回滚；无变化的提案改为跳过 | 两个新用例覆盖"首次了结 → 带当前版本重放被跳过 → 带旧版本仍冲突" |
| `e091a70` | ① Host 工具结果未中和 `[[REPLY_ACTION]]` 等协议标记，被篡改的网页可让她对明确提问静默（Core 链有中和、Host 链漏了）；② `compression_cutoff` 用 `len-2` 夹住保留条数，`[系统, 本轮来信]` 会把本轮来信整条压掉，请求里一个 user 轮次都不剩 | 两条新测试；分别还原后对应测试失败 |
| `da90d68` | 折队只限条数不限字节，刷屏把单条 turn 撑到几十万字并写进记忆 | 新增 `folded_text_never_exceeds_one_inbound_message`（含连续折叠 50 次不增长） |
| `53dac39` | 账本存储建表漏了 13 个兄弟 store 都有的建议锁；短 id 前缀用 `LIKE` 当通配符（`_`/`%` 会命中一堆行并被报成"没找到"） | 两个单元测试 + 真库测试 `gag_prefix_lookup_uses_a_uuid_range_and_rejects_wildcards` |
| `f381485` | 语义召回无 LIMIT、无超时，一次可把整表向量（默认上限 5 万条 ≈ 100MB）拉进进程并独占共享连接池 | 补召回索引 + 2 秒预算 + 候选窗口；`memory::` 53 passed，真库 3 passed |
| `e65c7e4` | 表情包建表迁移无锁，`DROP CONSTRAINT`+`ADD CONSTRAINT` 并发时会失败并 `panic!` 掉整个插件 | 真库测试连跑两次迁移断言复合主键；另用 `BEGIN … ROLLBACK` 复刻旧表验证迁移语句与 `IF EXISTS` |
| `4b290fa` | 单轮工具调用无上限：一次响应里的上百个 `group.message.send` 会全部真的执行 | 抽出 `refuse_tool_call` 并加 5 项断言 |
| `cc1e129` | 群成员列表读不到时按 `Ok(lookup_failed)` 上报，宿主层记成"工具成功" | `model::tool_access` 20 passed |
| `b896f69` `31f39fe` | 上面两条引入的 clippy 告警（`items_after_test_module`、`manual_clamp`） | `clippy -D warnings` 干净 |

回归：`cargo fmt --check` 干净；`cargo clippy --workspace --all-targets --all-features -- -D warnings` 干净；
`cargo test --workspace` 1128 + 350 + 10 + 13 全过（58 ignored）；带 `DATABASE_URL` 跑 ignored
真库测试 57 passed（唯一失败是 `redis_store` 那条需要 `REDIS_URL`，本机没有 Redis）。

### 11.2 已核实为**误报**、不必改的审计结论

两条审计线共报 30 条，其中这几条经逐行复核不成立（记下来免得下次再查一遍）：

- "Postgres 在空查询下 `relevant()` 恒返回空、反思永远看不见已有看法" —— 不成立。
  `search_enabled` 为 false 时 SQL 里的 `NOT $4::BOOLEAN` 直接放行，空查询走的是
  "按 score/recency 排序"那条路；内存在线实现的 `query.trim().is_empty()` 分支是同一语义。
- "`BeliefStore::relevant` 传 `now: None`，退休信念仍进 planner 快照" —— 不成立，
  `mind_store.rs` 那一处传的是 `Some(now)`。
- "`InterestOperation::Decay` 在生产是空操作（elapsed 恒为 0）" —— 不成立。
  `decay(effective_at)` 用的是**记录自己的** `updated_at` 算 elapsed，于是
  `elapsed = max(now - updated_at, 0)`，正是想要的语义。
- "`relation_store::set` 绑定了 SQL 从不引用的 `$3`/`$4`" —— 不成立，那两个占位符
  在 `VALUES ($1,$2,$3,$4,$5,$9)` 里给新行用（首次插入时没有相处证据，取快照值是对的）。

### 11.3 待你拍板（都涉及语义取舍，不擅自改）

1. **`MessageReceivedEvent.stop_requested` 恒为 false**（ingress 四处硬编码）。字段与
   下游分支都在，但没有生产者：接上等于新增"用户说停就停下这一轮"的能力，删掉则是删字段与
   一整串死分支。二选一。
2. **TurnGate 用户文本可注入字段标记**（`\u{0001}`–`\u{0004}`）。需要选：标记改成不可预测的
   nonce / 渲染前转义用户文本 / 保持现状但写进文档。注意 `tools/turngate/features.py` 要同步。
3. **`AffectStore::set` 是无版本判据的整行覆盖**（与 2026-09-14 关系侧那次事故同型，关系侧
   已改成"只写自己两维 + delta 通道"）。要么加 `RETURNING` + 乐观判据，要么同样拆 delta 通道。
4. **`save_world` 是 7 表全表重写、与擦除路径不互斥**（作者自己的测试名就写着"erasure must
   resync the runtime"）。要么给快照写单独的建议锁并与擦除互斥，要么降级成按 id upsert + 墓碑清理。
5. **决策记录的 `ON CONFLICT (event_id) DO NOTHING` 返回值被丢弃**，重放保护形同虚设。
   要定"同 event_id 不同内容"算幂等还是冲突。
6. **后台接口把原始 DB/IO 错误文本与绝对路径回给前端**。单管理员 + 默认回环，是"便于运维"
   还是"收窄成一句通用错误 + 服务端日志"，取决于你的偏好。
7. **`Unknown` 出站记录 1 小时内不可淘汰**，攒满 16 格后该会话整批回复被拒（网络抖动 16 次
   即可让一个群最长哑 1 小时）。淘汰策略要定：容量优先（按 `terminal_at` 淘汰旧的）还是
   保留优先（把幂等记录搬到独立有界 LRU）。
8. **群聊说话人标记可被换行伪造**、且**折队会把别人的话挂到最新发言人名下**。两者都要动
   提示词格式（正文净化规则 + 片段级发言人），改完会改变模型看到的历史呈现方式。
9. **Executive 两处**：合并已有冲突时会重设 `conflict_scopes[id]`，让按作用域擦除漏掉它；
   `ExpectationStatus::Violated|Cancelled` 落进空分支被删、`Expectation::violate()` 无调用者。
   需要先定"预期被违反"在产品上意味着什么。
10. **后台 `/api/memory/stats` 把 DB 错误静默降级成 0**、**提醒发送失败对用户完全静默**、
    **agent_run 通知闸门瞬时故障被当成终态失败** —— 都是"失败该不该让用户/运维看见"的口径问题。

### 11.4 拍板之后的执行结果（2026-09-15 深夜，第二批）

§11.3 那 10 项你选了"低风险六项 + 提醒失败发创建者 + agent_run 闸门退避重试"。执行结果：

| 提交 | 修了什么 | 怎么验证的 |
| --- | --- | --- |
| `f16a911` | 合并已有冲突时归属被最后触碰的调用方改写，按作用域擦除会漏掉它 → 首个写者赢 | 新增 `coalesced_conflicts_keep_their_original_erasure_scope`；改回 `insert` 立刻失败 |
| `005cf86` | 决策记录 `ON CONFLICT (event_id) DO NOTHING` 的返回值被丢弃，重放保护形同虚设 → 同内容幂等成功、改了内容才冲突 | 真库测试：写两次只留一行、换结论必须冲突；丢掉返回值后立刻失败 |
| `77b2156` | 后台统计三处把 DB 错误吞成 0/"这一类消失" → 能算的照常算，算不出来的回 `null` + `partial_errors` + 服务端日志 | node 抽出 `statNumber`/`renderStatCards`：`null`→「—」、`0`→`0`、降级提示带具体项 |
| `267f060` | `save_world` 与建表迁移共用一把锁（每次落盘都挡迁移），且与擦除不互斥 → 新增快照专用锁，两侧共用 | 真库测试：占住建表锁照常落盘、占住快照锁必须等待；改回 `schema::lock` 立刻失败 |
| `624b8c3` | `Unknown` 出站与 Prepared/Committed 一样不可淘汰，16 格占满后整批回复被拒（最长哑 1 小时）→ 过了碰撞窗口即可淘汰，最旧优先 | 新增 `unknown_outgoing_records_must_not_wedge_the_reply_queue`；改回"一律不可淘汰"立刻失败 |
| `22ac38c` | `ApiError::internal` 把绝对路径与 SQL 报错回显给浏览器 → 只回"内部错误（编号 N）"，完整原因进日志；另留 `unavailable`/`internal_explained` 两个刻意保留原文的构造器 | 两个用例：回显不含绝对路径且带编号、两个保留原文的构造器语义不变 |
| `c9c978b` | 一次性提醒发送失败后用户完全静默 → 三个终态失败点都发说明，Message 类发**创建者私聊** | `every_terminal_failure_gets_a_user_notice` 覆盖两类文案 |
| `49fc7bf` | agent_run 通知闸门瞬时故障被当成终态失败（`recover_stale_claims` 也不再碰它）→ 放回 active、清租约、30 秒后重试 | 真库测试断言状态/通知状态/租约/失败计数；改回 `failed` 立刻失败 |

另外把前端检查固化成了 `tools/admin-ui-checks.mjs`（`25294e8`）：无依赖，7 项检查，
覆盖图谱节点身份、翻页条、两处列表缓存失效、按钮字面、统计降级显示——这些是 Rust
测试碰不到、又曾经真的错过的前端逻辑。写法上踩过一个坑：`check()` 最初没 await，
异步断言永远绿；已修正并用阴性对照确认会失败。

**§11.3 里两项结论有修正，不是"待做"**：

- **TurnGate 字段标记不需要改**。标记是**拼在 n-gram 前面再哈希**（`turn_gate.rs:279`
  `gram.push_str(field_marker(marker))`），Python 侧同构（`features.py:149-166`），样本按
  JSON 字段存、没有任何地方拿标记做分隔符解析。用户正文里塞 `\u{0001}` 只能得到"本字段
  多了一个控制字符"的 gram，造不出别的字段的特征。残留的只有可读性问题：导出/标注样本里
  会出现原始控制字符，值得在**导出边界**转义。
- **`stop_requested` 建议删而不是接**。真正处理"停"的是 Host 语义层的 `wants_stop` →
  `interrupt::cancel`；`wants_stop` 产生于 Host 语义层、**晚于**事件提交给 Core，接线等于
  每条消息先跑一次分类调用。Core 侧那 6 处判断是死分支。真实缺口是"Core 接管的回合不尊重
  别说了"，那应当作为独立特性走事件/取消信号，而不是复活这个永远为 false 的布尔。

**仍在队列（本轮未选，均需先定口径或属于独立特性）**：`stop_requested` 死代码清理、
TurnGate 导出转义、`AffectStore::set` 的乐观判据、`ExpectationStatus::Violated|Cancelled`
的可观测化、群聊说话人标记伪造与折队归属（提示词格式变更，改完要看效果），以及审计报告里
其余低优先项（私聊看门狗等待上限、precommit 30 秒租约的取消语义、孤儿 `Prepared` 的租约、
嵌入维度校验、后台列表窗口与 `total_is_window` 口径、标注下载绕过 `source_key` 剥离等）。

### 11.5 队列第三批（2026-09-16 凌晨，"继续吧"之后）

在 §11.4 的剩余队列里挑"对错客观、不需要新口径"的做，共 12 个 commit：

| 提交 | 修了什么 | 怎么验证的 |
| --- | --- | --- |
| `7022e3d` | `AffectStore::set` 返回**库里的值**（`RETURNING`），不再把入参快照回给 Core | 真库往返测试；乐观判据**没做**，理由写在代码里（单写者，改签名属跨 crate 接口变更） |
| `68f1951` `ecf220e` | 预期落空/取消（`Violated`/`Cancelled`）不再落进空分支：四个终态全部上报，配额当场释放 | 新增用例：标成 Cancelled 后必须出现在 `cancelled` 里、不算 satisfied、配额已释放；改回空分支立刻失败 |
| `871fa21` | 私聊看门狗不再等未解决的 admission（与群聊 `WindowDrainWait::Never` 对齐），不会再卡 180 秒并拖掉整轮扫描 | 新增用例：`Never` 下必须立刻返回；去掉该分支测试会挂住超时 |
| `e28b545` | 长来信不再让 Mind 快照整体失效：query 按**字符 + 字节**双上限截断（中文一字三字节，只按字符截仍过不了校验） | 新增用例：3072 字来信仍能构造请求且 query 是原文前缀；改回只截字符立刻失败 |
| `f992539` | `WorldModelSnapshot::is_empty` 计入 `causal`（只有因果关系的状态曾被判成"没东西可存"） | 新增用例：空白为空、加一条因果关系后非空；去掉该判断立刻失败 |
| `c92c539` | 标注下载不再把删除屏障字段 `source_key` 交给浏览器（批次文件每行都有，换个 `?name=` 就能拿到） | 新增用例：剥离后不含该字段、内容与其它字段逐字保留、非法行报错；去掉剥离立刻失败 |
| `daf9a47` | 后台记忆列表：**不过滤**时被窗口截断的总数也标注为窗口口径（以前只有过滤时标注，页码条会承诺翻不到的页） | 新增用例覆盖过滤/截断/装得下/正好装满四种情形 |
| `0828301` | 账本写入改成单事务 + 作用域建议锁：并发 `#记下` 不再突破每个作用域条数上限 | 真库并发用例（上限压到 1，`tokio::join!` 两条并发写入，断言只剩一条），连跑 5 次稳定 |
| `2b9b84a` | 账本命令出错时给回执，不再 `.ok()?` 静默不回（命令认得、只是没做成） | 新增用例断言回执含"没处理成功"与下一步 |
| `1592a19` | `stop_requested` 补现状说明（见下） | 纯文档，`cargo build -p yunxi-core` |

**一处重要纠正**：§11.4 里我写的"`stop_requested` 建议删而不是接"**是错的**——当时只看了
`grep` 的前半截。它的消费方在 Core 里是完整的：`AttentionSystem` 把它当作必须处理的理由
（`AttentionReason::StopRequested`）、`planner` 据此调低舒适度目标与好感增量、
`mind/decision` 用它挡掉自主发言、`model/intrinsic` 用它取消兜底回复。缺的只是生产者
（宿主入口四处硬编码 `false`，而语义层的 `wants_stop` 晚于事件提交给 Core）。所以字段
**不能删**，已把现状、消费方清单与"为什么不能靠这个布尔接线"写进字段文档。真正的"停"
目前走 `wants_stop` → `interrupt::cancel`，只覆盖宿主自己那轮；让 Core 回合也停下需要
一条后续信号，属独立特性。

**验证总量**：`cargo fmt --check` 干净；`cargo clippy --workspace --all-targets
--all-features -- -D warnings` 干净；`cargo test --workspace` = model 1142 + core 354 +
CLI 10 + acceptance 13，全过（63 ignored）；带 `DATABASE_URL` 的 ignored 真库测试
**62 passed / 1 failed**（`redis_store` 需要 `REDIS_URL`，本机无 Redis）；
`node tools/admin-ui-checks.mjs` 7 项全过。

**仍在队列**：嵌入维度/数值校验、precommit 30 秒租约的取消语义、孤儿 `Prepared` 的租约、
Executive 快照 upsert 的 `>` 与 `DO UPDATE` 判据、`put_record_tx` 的误导性 VersionConflict、
Mind cleanup 让退休信念"以新 UUID 复活"、`InnerAgenda::prune_to_limits` 终态项绕过上限、
`InMemoryMindStore::apply` 的硬编码校验配置、台账里更早的私聊/群聊提示词格式项。

### 11.6 队列第四批（2026-09-16 凌晨，"接做吧"之后）

又是挑"对错客观、无需新口径"的做，本批 9 个 commit（另有 1 个 clippy/属性修复）：

| 提交 | 修了什么 | 怎么验证的 |
| --- | --- | --- |
| `fcd3b2b` | 按 id 更新不存在的记录报 `NotFound`，不再伪装成 `VersionConflict{actual:0}`（上层会误以为版本过期去重试） | 真库用例：把夹具推到第 2 版再更新一条从未写入的记录；改回旧回落立刻失败 |
| `7053493` | Executive 快照两处 upsert 加上 `WHERE`：同版本不同内容报冲突，不再静默覆盖对方的 plan/expectation 投影 | 真库用例：同版本同内容幂等、同版本不同内容冲突且库里仍是先写者那份；去掉 `WHERE` 立刻失败 |
| `c13e42b` | 内存/Postgres 两份硬编码的"存储层校验界"收敛成一个公开来源，并钉住"不得小于策略默认值" | 新增用例逐字段断言；把界改小立刻失败 |
| `41567e1` | 嵌入向量非法元素不再被 `unwrap_or(0.0)` 静默补 0（字符串/null/NaN/空数组一律报错）；维度不一致的存量向量按时间节流告警，而不是"永远排不上" | 新增用例覆盖各类非法输入；`memory::` 54 passed |
| `7f8c578` | 世界模型模块文档改成事实（反序列化**不**校验边界） | 纯文档 + `cargo build` |
| `0072bb7` | 孤儿 `Prepared` 出站记录 5 分钟后降级为 `Unknown`：不再永久占容量、卡后续 prepare、让整份 `ReplyState` 不回收 | 新增用例（新准备的不动、过期的降级）；删掉降级分支立刻失败 |
| `3a9e37e` | 预提交校验超时取消可见回复时打印 WARN（以前与"被新消息顶掉"完全同形、无日志） | `model::interrupt` 37 passed（行为未变，只加日志分支） |
| `81c273b` | 群聊正文里伪造的"下一条消息"说话人标记被中和，不能再冒充别人说话（同时写进长期记忆的那份也一起处理） | 新增用例覆盖两种标记形态、正常正文不受影响；去掉中和立刻失败 |
| `432c426` | 修上一条把 `#[test]` 属性弄丢、导致原有用例**静默停跑**的问题（`clippy -D warnings` 的 dead-code 报出来才发现） | `cargo test -- --list` 确认两条都在；真库 4 passed |

**一条核实为误报**：`InnerAgenda::prune_to_limits` 让终态项绕过 per-scope 上限，但紧随其后的
`items.truncate(max_total)` 仍然对**全表**生效，所以 `validate` 的 `items.len() > max_total`
不可能被触发——审计里"终态项攒多了会让 upsert 恒失败"的说法不成立。终态项只是不吃
per-scope 配额（它们的生命周期由 `decay` 里 7 天的保留窗口管）。

**仍未做（需先定口径或属独立特性）**：

1. **折队归属**：折进来的旧发言仍挂在最新发言人名下（`PendingTurn` 只有一个 `sender`）。
   要做对得把 `PendingTurn.message` 拆成片段列表并贯通 6 处提示词拼装 + 记忆写入，属于
   提示词格式变更，建议单独一轮做完后人工看一次效果（上一轮已先给折队加了字符预算）。
2. **预提交租约到期该"取消"还是"续租"**：现在是 fail-closed 取消（进程死在校验中间也能
   自愈），代价是"活着但很慢"的回复被丢；续租则会让死进程的 precommit 永久卡住该会话。
   本轮只补了告警，取舍留给你。
3. **Mind cleanup 物理淘汰退休信念**：按 `updated_at` 每 scope 只留 256 条并物理删除，
   被删命题再次出现会以新 UUID 重新插入，而 `open_question.related_beliefs` 里仍指着旧 id。
   这属于"软引用还是引用完整性"的设计取舍。
4. 审计里其余低优先项：后台记忆接口每类两次串行查询共用 5 条连接池、租约用应用时钟写却用
   数据库时钟判（reminders/agent_runs）、`waiting_room` 作用域表 append-only 线性扫描、
   `group.rs` 的 1024 上限被粘性 `conversation.is_active()` 兜住、`private.rs` 控制命令回执
   在 admission 过期时静默不发送、`conversation.rs` 那条"只在时钟偏移下可达"的分支
   （本轮想改但没能在代码里定位到具体那一处，未动）。

**验证总量（本批结束时）**：`cargo fmt --check` 干净；`clippy --workspace --all-targets
--all-features -- -D warnings` 干净；`cargo test --workspace` = model 1147 + core 355 +
CLI 10 + acceptance 13 全过（65 ignored）；带 `DATABASE_URL` 的 ignored 真库测试
**64 passed / 1 failed**（`redis_store` 需要 `REDIS_URL`）；`node tools/admin-ui-checks.mjs`
7 项全过。

### 11.7 折队归属重构（2026-09-16，你确认"提示词格式变更没问题、允许特大改动"之后）

`011c7f3`：折队（队列满时把最旧的 turn 折进当前 turn）以前只把正文拼成一段字符串，
**发言人信息直接丢掉**——A、B 说的话在模型眼里成了 C 说的，而且这份错误归属会随记忆
写回长期保存（`add_conversation` 用的正是同一份拼接字符串）。

**这次动的东西**（4 个文件、+329/−84）：

| 位置 | 变化 |
| --- | --- |
| `PendingTurn` | 新增 `folded: Vec<FoldedFragment>`（`{ sender, message }`，FIFO、最老的在前）；折队时把旧 turn 的 `folded` 与"它自己"依次搬进片段列表，**不再拼接正文**；当前这条始终单独放在 `message` |
| `attributed_transcript` | 渲染收敛到一个入口：每段各带自己的说话人标记；用户正文里伪造的"下一条消息"标记在**这一个地方**中和（先中和正文、后拼宿主标记，所以宿主标记不会被自己破坏） |
| `enforce_fold_budget` | 超预算时**先整段丢最老的发言**（保住归属，而不是把文本揉成一团再截），丢了几段在正文开头写明；仍超才截当前这条的尾部 |
| 私聊那半 | `private_user_message` 增加 `先前消息` 数组（私聊 1:1，折进来的每段都是同一个人说的，不需要重复发言人）。**必须一起改**：私聊与群聊共用 `enqueue`，不改的话折进来的私聊消息会被静默丢掉 |
| 参数贯通 | `process_group_reply*` / `private_chat*` 各多一个 `folded`；群聊三个直接回合传 `&[]`、排空那处传 `&pending.folded`；私聊两处同理 |

**对外行为变化**（按规则重点标出）：群聊历史与长期记忆里，被折进来的旧发言现在**各占
一行、各带自己的说话人标记**，顺序为 FIFO；超预算时丢的是最老的整段而不是最新内容的
开头。私聊载荷多一个可选的 `先前消息` 字段（只在真的折过队时出现）。除此之外没有改动：
命令解析、附件、消息 id、understanding、`reply_expected` 仍按最新那条走。

**验证**：折队测试改成断言"片段各自带对的发言人、渲染出的两段各带各的标记"；新增
`transcript_keeps_host_markers_but_breaks_forged_ones`、`folded_private_messages_stay_in_
the_payload_and_json_escapes_newlines`；预算测试改成打 `enforce_fold_budget` +
`attributed_transcript`（连续折 50 次不超预算、留下的片段仍各自带标记）。阴性对照：把片段
里的 `sender` 置空后归属断言立刻失败。`cargo test -p model --lib` 1151 passed，零告警。

**顺带记两条观察**：① 私聊那条链路用的是 JSON 载荷（`json!` 会转义换行与控制字符），
结构上就伪造不出"另一条消息"，这也是它一直没这个问题的原因；② 群聊若要更彻底，可以把
行导向的文本换成同样的结构化载荷——但群聊历史同时进滚动摘要与记忆，格式迁移面较大，
当前"标记 + 中性化 + 片段归属"已经够用，暂不做。

**剩余（仍未做）**：预提交租约"取消 vs 续租"的取舍、Mind cleanup 物理淘汰退休信念导致的
悬空引用，以及审计里那批低优先项（见 §11.6 末尾）。

### 11.8 预提交续租 + Mind 清理引用完整性（2026-09-16）

**`b41984c` 预提交租约可续租**：租约到期会把那条 `Prepared` 改成 `Cancelled`（刻意的
fail-closed：校验没跑完不能发，进程死了也要能自愈）。代价是"活着只是慢"的校验会让
`commit` 拿到 `Stale`，**整条已经渲染好的回复被丢弃且不重试**——`delivery.rs` 里为此把
语音合成等慢步骤特意排到 `begin_outgoing_commit` 之前并写明原因，说明这个坑踩过，只是
当时用"调整顺序"绕开了，租约内剩下的 await 仍暴露在同一风险里。

新增 `PreparedOutgoingCommit::renew()`（只续自己那条，token 不匹配返回 false，绝不碰
别人的状态），并在三条发送路径的每个可能变慢的 await **之前**续租（多气泡回复、单条提示、
Core 投递的引用映射/路由/授权）。语义从"整段共用一个 30 秒"变成"每一步各自受 30 秒约束"。

**`ae3c568` Mind 清理的引用完整性**：`cleanup` 原先对所有 mind 记录一视同仁地按
`updated_at` 每 scope 留 256 条并物理删除，而信念的"退休"不是删行（`expires_at` 写
`valid_until`、`status` 仍是 'active'），于是退休信念会跟活着的知识抢配额；被挤掉后
那条命题的 `(scope_key, dedupe_key)` 唯一索引随行消失，同一命题再次出现会以**新 UUID**
复活，而 `open_question.related_beliefs` 还指着旧 id。三处改动：退休信念按
`RETIRED_BELIEF_RETENTION_DAYS`（30 天）单独过期；只有活着的信念吃 scope 上限；删完把
`related_beliefs` 里指向已不存在信念的条目摘掉（按存在性过滤，历史悬空引用一并自愈）。
顺带把 `orphaned_agenda` 挪到所有上限清理之后——它要找的正是本轮刚被清掉的那些目标。

验证：`precommit_lease_can_be_renewed_by_its_owner_only`（压缩租约后续租成功、睡过原截止
点仍有效、已过期的续不回来、状态清掉后返回 false）；
`postgres_cleanup_keeps_retired_beliefs_briefly_and_prunes_dangling_refs`（刚退休的留着、
退休 40 天的删掉、悬空引用被摘而有效引用保留），两个阴性对照分别跑过并如期失败。
全量回归：model 1153 + core 355 + CLI 10 + acceptance 13 全过，真库 ignored 65 passed
（唯一失败仍是需要 `REDIS_URL` 的 `redis_store`）。

### 11.9 已修：回复动作协议换成受约束通道（规则 7）

**当时为什么没动**：按第 7 条"不要让模型手写结构化文本"，`[[REPLY_ACTION]]{...}[[/REPLY_ACTION]]`
属于应当淘汰的那一类——让模型手写 JSON、再用容错解析器去猜（`complete_truncated_json_object`
就是在替它擦屁股）。但它横跨两条模型链路（Host 的 `model/reply.rs` 常驻协议 + 解析器；Core 的
自造包装、禁词分支与一大批测试；生成侧还是纯补全），半途改完比不改更糟，所以那一轮只写了方案。

**这次落地**（`f7099a3` 迁移 + `e26c02a` 顺带修 + `3ed59ed` 补漏 + `fea30c0` schema 守卫）：

Host（`plugins/model/src/model/`）

- `reply.rs`：新增 `reply_action_tool_spec`，字段契约随工具 description 下发（第 6 条），常驻的
  30 余行 `<回复协议>` 整段删除；语音/表情包字段只在下游真的可用时才进 schema。
  `ReplyActionCall::from_tool_arguments` 只校验 schema 管不了的两件事（越界值、字段名漂移），
  类型不对**整条动作作废**——畸形参数绝不能让 `silent` 生效。
  `parse_reply_output(content, call)` 只认工具参数提交的动作；旧解析器、`MAX_REPLY_PROTOCOL_CHARS`
  与 `complete_truncated_json_object` 在回复路径上的使用全部删除。正文里复述的旧标记改为整段
  截掉（`scrub_reply_protocol_markers`），保住"标记永远不发出去"这条安全属性，且不再被解释成动作。
- `memory_query.rs`：`reply_action` 与注册表工具并列下发，且是**终止轮**——提交动作即下结论；
  与其它工具同轮提交则动作作废、工具照常执行并回灌结果，下一轮要求重新单独提交（不静默丢调用）。
  返回值由 `BotMemory` 换成 `ReplyTurn`（正文 + 动作），宿主自有静默改为结构化
  `ReplyTurn::silent()`，`SILENT_REPLY_OUTPUT` 文本常量删除。工具注册表不可用时不再连带砍掉
  `reply_action`——它不走注册表。
- `utils.rs`：空回复修复真的把 `reply_action` 挂上（提示词点了名就必须给，否则又是"提示词点名了
  工具、她手里却没有"）；语气上下文按各条路原口径保留（新增 `NativeToolStyle`：非工具轮带
  `generate_reply_guidance`，与迁移前逐字一致）。
- `llm_mock.rs`：替身支持返回"正文 + 原生工具调用"，补两条端到端接缝用例——这一轮确实挂上了
  工具、工具调用确实变成动作、正文里的标记确实不再变成动作。

Core（`yunxi/core_model.rs`）

- 删除 `[[REPLY_ACTION]]` 兼容信封的读取器（`intrinsic_reply_payload` /
  `parse_intrinsic_reply_messages` / `safe_single_structured_reply_message`）与只服务它的
  测试用序列化/裁剪器（共 217 行）。intrinsic 提示词早已只要求一条自然消息，多气泡由 Core 逐条
  生成后自己排序，信封没有任何生产者。
- `sanitize_intrinsic_output` / `sanitize_autonomous_intrinsic_output` 只接受自然语言。
- `CORE_REPLY_REPAIR_PROMPT` 去掉 REPLY_ACTION 这个已死词（写进提示词就是教它写）。
- **泄漏守卫一条没删**：`intrinsic_output_is_unsafe`、修复通道校验器、
  `plain_reply_contains_transport_protocol`、诊断字段（`reply_action=` 改名
  `stale_reply_marker=`）——旧标记出现即判不可用，不会发给用户。这是"拒绝"不是"兜底"。

**验证**：`cargo fmt --check` 干净；`cargo clippy --workspace --all-targets --locked -- -D warnings`
干净；`cargo test --workspace --all-targets --locked` = model **1152** + core **355** + CLI **10** +
acceptance **13** 全过（model 侧删掉 3 个只测旧信封构建器的用例，新增 2 条端到端接缝用例、
1 条"动作必须单独调用"的分流用例与 1 条"截断参数被猜补后必须作废"的跨模块用例）。
接缝用例覆盖了"工具真的下发 / 工具调用真的变成动作 / 正文标记真的不再是动作"，把这次改动最容易
无声失效的那一段（跨模型调用的接缝）钉住了。

**自查补掉的一个口子**（`3ed59ed`）：流式累积那边 `finalize_native_tool_calls` 对**所有**工具
都开着 `complete_truncated_json_object` 的截断补全（registry 类工具沿用的既有行为），于是被切断的
动作参数会被"补"成一个完整对象照收——`{"disposition":"silent"` 补成 `{"disposition":"silent"}`
就是一次凭空出现的静默。现在回复动作这一侧按 `raw_arguments` 复核：不是 provider 原样给出的完整
JSON 对象就整条作废（registry 工具的行为不动），并有一条跨模块用例把"SSE 里吐到一半 → 累积器确实
补全了 → 校验器必须否决"这条链路钉住。

**没有密钥就静态核对的那部分**：真机探不了（见下），但"发出去的工具声明形状对不对"是可查的——
`reply_action` 与 `tool_access.rs` 的 `definition_spec`、与线上已在用的注册表 schema 是同一套形状
（`type: object` + `properties` + `additionalProperties: false`），只是全部字段可选、故不写
`required`；用例（`fea30c0`）守住"每个字段都必须有显式 `type` 与非空 `description`"。

**真机验证（`6081bbb`）**：文档原方案最后一步要求"跑一次真实的私聊/群聊端到端确认"。
QQ 那一层需要先部署（不可逆红线，未动），但整条链路上唯一无法用替身回答的问题——**配置里的那个
主模型拿到这份工具声明后到底会不会调它**——已经用真实端点测掉了：一条默认 ignored 的真机用例
（跑的是仓库自己的请求组装，不是手搓请求）实测 `deepseek-v4-flash`：

- 明确要求 @ 当前发言人 → `tool_calls=["reply_action"]`、`finish_reason="tool_calls"`，参数通过
  宿主校验（两次实跑分别填了 `at_current_sender`+`quote_message_id` 与候选里的 `at_user_ref`，
  两种都是合法表达）。
- 普通闲聊 → **不调工具**且仍留下可见正文。反向这条同样关键：工具每轮都在手里，若她见谁都调，
  每条回复都会多背一轮工具往返。

**遗留与代价（如实记下）**：

1. **QQ 层端到端仍未做**：需要在运行中的机器人上部署新代码（部署须由你拍板）。自动化与真机探针
   都过了，但"QQ 里真的发出这条 @ / 这次撤回 / 这张表情"这一步没有实测记录。
2. `server.wire_api = "responses"` 的部署**拿不到原生工具**（`params_model_with_native_tools_mode`
   在非 `chat_completions` 时直接返回模型错误）。这是既有约束——Core 的工具轮、sticker-only 轮
   在 responses 下本来就走不通——但这次把普通的结构化回复回合也纳入了同一约束：那些回合会以模型
   错误结束并触发"回复链路异常"报警，不再有文本协议兜底。已按决定把示范配置与 README 一并改成
   `chat_completions`（`e649ca1`，连 URL 一起改——`endpoint()` 只按 wire_api 补后缀）；要不要
   给 responses 补原生工具支持（请求体 tools/tool_choice + 工具结果走 function_call_output +
   工具循环历史换方言）**另立一项**，本次只消除模板陷阱。
3. 顺带修（`e26c02a`）：`CORE_REPLY_REPAIR_PROMPT` 与 `core_tool_follow_up_instruction` 两处生产
   提示词还在教模型手写 `[[TOOL_CALL]]` 标记，而代码那条通道不带工具、校验器又一律拒收标记，
   照做必败；已改成 function-calling 口径。
4. 顺带修（`6229ea2`）：对话摘要器的请求末尾被附了一份"本轮回复要求：先直接回应用户……"，
   与它自己的"只输出摘要，不要回答对话"矛盾（§11.10 第 1 条）。改走不带回复引导的路径，并删掉
   随之无用的 `interruptible_model_call` 与 `ModelGateway::complete_without_tools`。

### 11.10 审到的两处（已定：均按建议处理）

迁移过程中顺手审到两处，都**不在** §11.9 的范围里，改动会影响别的子系统的实际提示词，所以只记不改：

1. **对话摘要器被要求"直接回应用户"。** `utils.rs` 的 `summarize_conversation` 走的是
   `memory_query::interruptible_model_call`（`ModelPromptMode::LegacyReplyGuidance`），那条路会在请求
   末尾附一份 `generate_reply_guidance`——"本轮回复要求：先直接回应用户当前真正想问或表达的内容……情绪=X，
   强度=Y/10"。而摘要器自己的 system 提示词写的是"你是聊天记录压缩器……只输出摘要，不要回答对话"。
   两条指令互相矛盾，且没有任何测试覆盖这条请求的组装。看起来是"通用模型调用"与"回复专用调用"没分开
   的历史遗留。**已按建议修掉**（`6229ea2`）：改走 `interruptible_model_call_without_reply_guidance`
   （`ModelPromptMode::None`，不附加任何引导），与它原本 `progress = None` 的调用方式一致。
2. **`ModelGateway::complete_without_tools` 已无调用者**（本身带 `#[allow(dead_code)]`，本次迁移之前就
   是死代码）。与第 1 条相关，**已一并处理**（`6229ea2`）：删掉该网关方法、`interruptible_model_call`
   与 memory_query 那份 `ModelPromptMode::LegacyReplyGuidance` 分支。
   **更正后已一并删除**（`967006b`）：这里原先写的是"`params_model` → `params_model_with_token_limit`
   → `..._and_progress` → `..._for_reply` 这条链迁移前就没人调"——**那句是错的**。死的只有根上的
   `params_model`；`params_model_with_token_limit` 还有一个活用户：**表情含义候选整理器**
   （`sticker_memory.rs`）。它和摘要是同一个坑的第二个实例（严格 JSON 抽取任务被附
   "本轮回复要求：先直接回应用户……"，而失败是静默的——解析不出 JSON 就 `Ok(None)`），已改走
   `params_model_without_reply_guidance`（`3415f95`，并补了一条抓请求体的守卫用例）。用户搬走之后
   整条链才真的没有调用者，随后连同 utils 那份 `ModelPromptMode::LegacyReplyGuidance` 一起删掉
   （`967006b`）。教训记在这里：核"死代码"要核到**链条的每一个出口**，不能只看根上那个函数。

### 11.11 发布记录（2026-09-15 18:27，本次迁移上线）

**推送**：`986df30..e18983d`（16 个提交）推到 `origin/main`。

**部署**：线上 `5ee24f9` → `e18983df`。注意这次一次性上线的是 **150 个提交**（线上落后
`origin/main` 134 个，积压了 Mind 候选、折队/队列、表情包相册、画像情绪、提示词人格等好几个
会话的改动），不只是本次回复协议迁移——发布前已把范围写给用户确认。

- 演练：`scripts/deploy-local.sh --dry-run --require-clean` → 交叉编译 61 秒、包 14.1 MiB、
  sha256 `8f356d81…`，未上传未切换。
- 产物自检（照本仓库惯例按字节查，14 项全过）：新协议在（`reply_action`、工具说明、各字段说明、
  "拒绝按猜测补全"、"reply_action 必须单独调用"），旧协议的**生产者文本**已不在
  （旧常驻协议头、`SILENT_REPLY_OUTPUT` 那个静默常量、旧语音字段文案、旧空回复修复提示词、
  两处旧 `[[TOOL_CALL]]` 指令）。裸标记 `[[REPLY_ACTION]]` 仍在二进制里——那是泄漏守卫
  （`plain_reply_contains_transport_protocol`）刻意保留的，不是残留解析器。
- 真部署：`--no-build`（复用刚自检过的那份二进制），上传 8 秒、总耗时 18 秒；
  服务端原子切换 + readiness 通过，服务 active，启动以来 WARN/ERROR = 0，QQ 已连上。
- 发布后按台账要求推了 `[prompt]`：线上 override 里原本没有该段（用的是主配置那份旧的两段全文，
  新代码会把 `persona` 拼在前面 → 人格会在每轮出现两遍）。`scripts/apply-persona-config.sh --apply`
  写入成功（`ok=True reloaded=True`，备份 `bot.conf.override.toml.bak.1789468071214`）。

**发布后验收**（三条都在服务器上跑真实生产模型，只有第三条需要先修）：

| 验收 | 结果 | 备注 |
| --- | --- | --- |
| `verify-mind-observation.sh`（窗口自 18:27） | **PASS** | 观测丢失率 0.0%（基线 46%）、超时 0、事务告警 0、WARN/ERROR 0；**但窗口里只有 1 个 Mind 事件**，样本太小，过一阵值得重跑 |
| `verify-mind-candidates.sh` | **FAIL** | 6 个样本里只有 1 个带**能过解析器**的候选块；失败项是"候选块必须是唯一一段、且在最前面（解析器要求 `starts_with`）"——即她把块写在正文后面，解析器直接丢掉。**不是本次迁移引入的**（属另一条通道的提示词/解析器口径），未改 |
| `verify-sticker-album.sh` | **PASS**（先修脚本，`9ef5a1a`） | 条件四 3/3：宿主链路**通过 `reply_action` 的 `sticker` 字段**提交 `芸汐的照片`，即本次迁移后的行为；Core 链路条件一~三 3/3；对照组 3/3 仍复现旧否认 |

**这次发布暴露的两个环境事实**（都不是本次改动引入的，记下来免得下次重新查）：

1. `deploy-local.sh` 的本地通道**只带二进制**，配置继承自上一版 release。提示里列出了新版本模板
   有、而继承配置缺的键（`addressed_reply_rate_limit`、`api_key`、`continuation_*`、`annotation_dir`…），
   这些会退回默认值；要按模板重新生成配置得走 GitHub Actions 手动 dispatch。
2. 服务器上 `models/yunxi-turngate/manifest.toml` 不存在，启动日志出现
   `[TURNGATE] bundle 不可用 (回退 lexical+MiniMind)` —— turn gate 走的是回退路径（可用但降级）。

**回滚**：上一版 release 仍在服务器上，把 `current` 指回去再重启即可——
`ln -sfn /home/ubuntu/kovi-bot/releases/5ee24f9bf45762f6dabde56b92d522e86db3e35a /home/ubuntu/kovi-bot/current`
`&& sudo systemctl restart kovi-bot.service`（发布脚本在 readiness 不通过时也会自动做这一步）。

### 11.12 发布后发现并修复：群里的"撤回"没有执行者（`f8e0db0` + `7f0328f`）

**现场（2026-09-15 18:32，群 641996763）**：用户 @ 她"撤回你刚刚发的消息"，她答"我这边没有
撤回消息的权限，得你自己在群里长按那条消息操作一下"。日志里那一轮**从头到尾只有 Core**：
`purpose=core_reply`、`Yunxi Core Strong result … tool=false`、`Core reply repair`，没有任何 Host
链路行，也没有工具下发。

**根因（两条）**：

1. **归属**：群里被 @ 的消息经 `bridge.rs::classify_group` 判成 `GroupCoreHandling::Decide` →
   归 Core；只有 `#` 开头的控制面命令、群被禁言、以及"已有回复在进行中"才留给 Host。
2. **能力落位**：撤回只在 Host 链路（`reply_action` 的 `recall_message_ids`），Core 没有这只手。
   模型手里没有工具，就自己编了一句"没有权限"。

**不是这次部署引入的**（三条证据）：`classify_group` 那段路由在 `5ee24f9..HEAD` 里零改动；
`supports_group` 的 `YUNXI_CORE_GROUP_CUTOVER` 默认 true 在 `5ee24f9` 就是同一行；服务器 systemd
只设了 `YUNXI_CORE_PRIVATE_CUTOVER=1`，群聊那个开关走默认（开）。也就是说，自 v3（`26fd802`，9-02）
Core 接管群聊起，群里自然语言的"撤回"就没有执行者了。

**修法（按"副作用归注册表、回复形态归 hint"的边界，而不是给 Core 复制一套 reply_action）**：

- `f8e0db0`：新增两个**宿主侧**注册表工具——`message.recall_candidates`（只读，列她自己最近
  可撤回的消息，清单不进提示词，与 `sticker.list` 同范式）与 `message.recall`（写，id 只能来自
  候选）。**`crates/yunxi-core` 一行没改**：Core 用它已有的 `CognitiveIntent::UseTool` 就能调，
  因为注册表工具的执行能力是通用的 `ActionCapability::UseTool`；约 110 秒窗口、`delete_msg`、
  候选白名单全留在宿主。校验与执行**复用** `recent_bot_messages` + `recall_bot_messages`，与宿主
  那条撤回是同一份实现。
- `7f0328f`：光有工具不够——两条链路都只在工具轮下发工具，而判据 `likely_requires_tool_protocol`
  里没有"撤回"。改为复用既有的 `reply_action_tool_requested`（含"命令 vs 讨论"的排除），
  动作类请求从此进工具轮。顺带补上一个既有口径漏洞：动作字段说明里点名了
  `group_members_search`，而只挂 `reply_action` 的那一轮她根本没有这个工具。

**验证**：4 条单元用例（候选只列她自己的；编造 id / 空数组 / 超上限一律拒；候选里的 id 能过校验；
只读与定时任务的暴露口径；动作请求进工具轮而讨论不进）+ **真机探针**：明确要求撤回时，
配置里的 `deepseek-v4-flash` 真的调了 `message_recall_candidates`。`fmt`/`clippy` 干净，
workspace 测试全过（model 1157）。

**已上线**（`45df76a2`）：推送 `42a5679..45df76a` → 演练（交叉编译 66 秒、包 14.2 MiB、
sha256 `a1b5562b…`）→ 产物自检 8 项通过 → 真部署（`--no-build`，复用自检过的那份二进制）。
发布后核对：服务器上的二进制里确实有这两个工具（`grep -F` 逐条查过）、服务 active、
重启 0 次、WARN/ERROR 0、工具注册表已就绪。

**收敛完成**（`c4a148d`）：`reply_action.recall_message_ids` 已退役——schema、字段白名单、解析、
计划侧执行、`ReplyExecution` 的两个撤回字段、动作候选上下文里那段撤回候选清单全部删掉，
撤回只留 `message.recall` 一条路。旧字段现在是**报错**而不是静默忽略（有专门用例）。两处刻意的
行为变化已写在该提交里：① "只撤回不说话"不再静默收场（走既有空回复修复，她会补一句）；② 撤回通知
改由工具登记、宿主回合收尾写进会话历史（工具里直接写会与宿主整轮持有的历史锁自锁）。

**收敛已上线**（`f1555ba7`）：演练（交叉编译 65 秒、包 14.2 MiB、sha256 `22c520c6…`）→
产物自检 8 项通过（含"退役字段与退役的候选上下文**不在**二进制里"这类反向判据）→ 真部署
（`--no-build`）。发布后在服务器上逐条复核：两个工具在、`reply_action` 那句指向在、旧字段说明与
旧候选上下文已不在；服务 active、重启 0 次、WARN/ERROR 0、QQ 已连上。

**第一轮实测失败 → 又修了两处**（`989d10e`）。用户群里实测（19:00、19:15 两轮）她仍然回
"我这边没法撤回消息"。看日志分成两个各自独立的原因：

1. **接续回合被工具闸门排除**：19:15 那条**没带 @**，按 `continuation_to_agent`（她当前对话焦点
   里的人接着说）处理——bridge 侧的 `priority`/`requested_message_count` 都把接续算作"对她说的"，
   唯独 Core 的 `allow_tool_call` 漏了它，于是她照常回复但手里没有任何工具。
2. **"先查候选、再按 id 撤"在 Core 侧结构上走不通**：19:00 那几条是点名+引用的（工具已下发、
   `tool_calls=1`），但 Core 的**工具结果跟进轮强制只读收窄**（`register_core_tool_intents`），
   第一轮查完候选，第二轮就没有写工具了。Host 侧只有 web/news/weather/mcp 才算外部内容，
   所以这个坑只在 Core 侧——**这就是为什么"清单按需索取"这个范式不能照搬到写动作上**。

修法：判据抽成 `message_turn_allows_tool_call` 并补上接续；`message.recall` 增加一轮可完成的
`target:"last"`（撤最近那条），`message_ids` 只留给要挑几条的场合，两者都不给则明确报错。
真机探针改断言后重跑：模型第一轮就调 `message_recall {"target":"last"}`。

**第二轮修复已上线**（`47660c89`）：推 `dd0c934..47660c8` → 演练（交叉编译 64 秒、包 14.2 MiB、
sha256 `6821b6de…`）→ 产物自检 6 项（含 `target=last` 新写法在、旧报错文案已退役）→ 真部署
（`--no-build`）。发布后复核：服务器二进制里新 schema 在、旧文案不在；服务 active、重启 0 次、
WARN/ERROR 0、QQ 已连上。

**遗留**：QQ 层实测由人来做（两种形态都值得试：带 @ 的，以及紧接着她说的话、不 @ 的接续形态
——后者正是 19:15 那次失败的形态）。自动化只能验到"工具存在、真机模型会在一轮里用对形态"这一层。

### 11.13 优化：工具跟进轮不再一律只读（`b56ba74`）

用户看完 11.12 的结论后说"优化一下这个，这样的话很多事情干不了呀"——指的是上面那条：
"先查候选、再按 id 撤"在 Core 侧结构上走不通。这不只是撤回一件事：**任何"先查再写"的多步
任务在 Core 那条路上都做不了**（查完提醒列表再取消、查完记忆再写……）。根因是判据太粗：
跟进轮被硬收窄成"只读"，而"只读"里混着两件完全不同的事——查资料，和给她自己加一条提醒。

**改法：把"读 / 写"两档换成按副作用波及到谁的三档。**

| 档位 | 含义 | 典型工具 |
|---|---|---|
| `WriteScope::ReadOnly` | 不产生副作用 | 时间、算式、网页/检索、记忆检索、提醒列表、群成员查询、授权群清单、表情标签、命令清单、运行状态、健康检查、撤回候选 |
| `WriteScope::UserScoped` | 只影响本人、可逆、不对外可见 | `reminder.create` / `reminder.cancel`、`memory.remember`、`message.recall` |
| `WriteScope::Outbound` | 以她的身份对外发言，或改动群级/全局共享状态 | 群发/私聊发送、暂停/恢复群、取消群任务、启动/取消持续任务、教表情含义 |

`ToolAllowance::{ReadOnly, UserScoped, Full}` 决定"这一次下发/执行最多到哪一档"：

- 只读轮（QQ 通话试跑、自检）→ `ReadOnly`；
- 工具结果跟进而结果里可能夹别人写的字 → `UserScoped`（对外发言仍然挡住）；
- 其余首轮 → `Full`。

**跟进轮放到哪一档，只看上一个工具的结果可不可信**（`ToolRegistry::follow_up_allowance`）：
宿主自己算出来或自己配置的数据（时间、算式、表情标签、命令清单、系统/健康状态、授权群清单）
之后继续全量——这正是 `group.message.targets` → `group.message.send` 那条两步流程，它本来就
靠宿主自己的数据；可能夹外人文字的结果（网页/检索/天气/记忆/群成员昵称/私聊联系人/撤回候选/
群问题状态），以及**任何失败**（失败详情常夹远端原文），一律收窄到 `UserScoped`。注册表拿不到
时也按 `UserScoped` 兜底，宁可这一轮做不了写。

**两条纪律写进代码里：**

1. `write_scope` 故意**不留 `_` 兜底**：新增内置工具时编译器会在这里拦住，逼着当场判一遍
   波及范围。留兜底看着省事，但兜到哪一档都是错的——兜到只读，一个没人审过的写工具就会在工具
   结果之后被放出去；兜到对外写，一个纯查询工具会在试跑里莫名其妙地消失。（旧代码是
   `matches!` + 默认当副作用：安全，但是静默的，新工具永远不会被"提醒"到。）
2. 顺手审掉了一处保守默认的误伤：`group.members.search` 是**纯查询**，原先没进只读白名单，
   收窄档里连挂都挂不出去——它不是"写"，只是当年没人替它背书。

**执行边界与清单收窄现在用同一个判据**（`ToolRegistry::available_for_allowance`）：清单收窄
只是"不告诉她有这些工具"，delivery 在动手前按此刻的事实再算一次，幻觉或被注入出来的工具名
依然执行不了；顺带也把注册之后才变化的状态（群被暂停、主管理员身份被撤、素材库没了）重算了一遍。

**对外可观察的行为变化：**

- Core 的跟进轮：提醒、个人记忆、撤回她自己刚发的消息放开了（原先一律挡）；对外发言与群状态
  改动仍然挡住。
- delivery 的效果边界现在对**所有**档位都重算一次可用性（原先只在只读收窄时算），因此注册之后
  才变化的状态会在执行前被拦掉；这条严格来说是**收紧**，不是放松。
- 拒绝原因从 `tool_follow_up_requires_read_only_tool` 改名为
  `tool_allowance_rejected_at_effect_boundary`（旧名字已经说不清是什么原因了）。
- Host 那条 ReAct 循环**怎么判**不变（仍按外部工具名 `is_external_tool_name` 收窄），
  但收窄到哪一档变了：吃到外部内容之后从"只能只读"变成"只读 + 只影响本人的写"。
  换句话说，Host 侧这次是**放松**了提醒/记忆/撤回这三种写，对外发言依旧挡住；
  两边（判据来源不同、放行范围相同）现在落在同一个档位定义上。

**踩到的坑（顺手修）**：重构中把 `core_tool_allowance` 的文档块插到了
`likely_requires_controlled_tool` 的 doc comment 中间，rustdoc 会把连着的一串 `///` 都算给
下面那个 item——文档注释紧邻语法，插队就是改文档。已挪回去。

**验证**：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets` 干净；
model 1161（新增 `follow_up_allowance_follows_the_preceding_results_trustworthiness`、
把跟进档位用例拆成"算档位"与"登记原样传递"两条）、yunxi-core 355、yunxi-cli 10、
acceptance 13 全绿。没跑真机（这条改的是档位判据，探针测不到；真机验证仍然只能靠 QQ 层实测）。
