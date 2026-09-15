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
