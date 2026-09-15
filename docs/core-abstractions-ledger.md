# Core 抽象收口台账：知觉 / 表达 / 作为 / 承诺 / 沉默

**这份台账是给"下一个对话"用的执行计划，不是设计散文。** 它记录 2026-09-16 我们关于
`ActionCapability` 的讨论结论、已核实的现状与硬约束，以及分步迁移方案。执行时请逐步
核对"现状"一节的行号——代码会漂移，台账里的事实要重新验一遍再动手。

## 0. 起点：一句话

> Core 目前对外的抽象收成五个：**知觉、表达、作为、承诺、沉默**，命名也改成这几个。

发起时的原始问题：*"如果之后有一个类似 send message 这样的工具，是不是又要重新写一个
分支了，而不是泛化？"* —— 是的，会。这份台账就是要把那个轴换掉。

---

## 1. 五个抽象的**类别**（先看这张表，它防的是重复犯同一个错）

把五个平铺成"五个动作变体"就是把 `StartCall` 的错误放大五倍。它们的类别不同：

| 抽象 | 是什么类 | 方向 | 谁的载体 |
|---|---|---|---|
| **知觉** | 输入 | 进 | `WorldEvent` + 注意力（`AttentionSystem`） |
| **表达** | **对外动作** | 出 | 目标（本会话 / 某人 / 某处）× 媒介（文字 / 语音 / 图片 / 电话）× 内容 |
| **作为** | **对外动作** | 出 | 宿主声明的能力名（今天的 `UseTool`，已经是泛化的模板） |
| **承诺** | **Core 自己的状态** | 内 | 状态更新（目标 / 未结之事 / 提醒 / 记忆 / 情绪 / 关系） |
| **沉默** | 输出为零 | — | 不是动作，是"没有表达、没有作为" |

所以 `ActionCapability` 的终态是 **`Speak` + `Act` 两个**；承诺走状态通道（不是动作）；
知觉与沉默本来就不在动作枚举里。

### 1.1 任何"新东西"先过这四个问题

这是本台账的可复用判据，也是 `StartCall` 那次错误的补救：

1. **它是朝外的吗？** 是 → 它是 `Speak` 或 `Act` 的**数据**，不是新变体。
2. **它改的是 Core 自己的状态吗？** 是 → 走承诺（状态更新）通道。
3. **它有独有语义吗**（档位豁免、专属协议门、专属授权位）？有 → 那是**属性**，不是**种类**。
4. **它只是新媒介或新落点吗？** 是 → 只加数据 + 一条声明。

四条都过不了，才考虑动 Core 的类型。

---

## 2. 为什么改：三条已核实的证据

### 2.1 `StartCall` 是这条流水线的证明

2026-09-16 我给"她能打电话"加了 `ActionCapability::StartCall`，然后**全部拆掉**
（commit `f2b3e13`）。加的时候改的是：

```
arbiter.rs  枚举变体 + as_str + capability_for + AuthorizationPolicy 字段
            + allow_* + allow_all/deny_all + permits 分支 + EnvironmentCapabilities::all()
宿主        capabilities()
```

而同样的功能用**工具**实现只需要在宿主写一个 `ToolDefinition` + 几个 `scope`/`schedule`
分支——Core 那侧的 `declared_effects` 是通用的。**这就是"轴的选错了"的实证**：能力枚举
按"她做的每一件事"开变体，于是每加一件对外的事都要动 Core。

并且它**零独有语义**：核过之后它唯一的活消费者是一个可以改问"工具声明了吗"的闸门。

### 2.2 "表达"今天被拆成六套机制

| 机制 | 归属 | 档位 | 落点 |
|---|---|---|---|
| `SendMessage`（回复） | **能力** | **豁免** | 本会话 |
| `ReachOut` | **能力** | 受约束 | 某人（`ReachOutMedium::Call` 是电话） |
| `group.message.send` | 工具 | 受约束 | 别的群 |
| `private.message.send` | 工具 | 受约束 | 别人 |
| `call.start` | 工具 | 受约束 | 本会话那个人（电话） |
| `message.recall` | 工具 | 受约束 | 撤销她发过的（"表达"的逆） |

六套机制里做的事是同一件：**把某个东西说出来，给某个目标，用某种媒介**。它们之所以
分裂，是因为枚举按"落点"开变体。注意 `reach_out` 与 `call.start` 的档位待遇不同
（见 §6.1）——这就是分裂的代价。

### 2.3 "承诺"有两套通道

```
StateUpdateProposal:  Affect / Relation / SetTopic / ConversationDirective
                    / ResolveOpenLoop / DeferOpenLoop     ← 已有一套
ProposedAction:       CreateOpenLoop / ResolveOpenLoop
                    / StartGoal / CancelGoal              ← 又来一套
```

核实过（2026-09-16，可复现）：

```bash
for v in send_message reach_out use_tool create_open_loop resolve_open_loop start_goal cancel_goal; do
  printf '%s: ' "$v"
  grep -rc "CognitiveIntent::$v\|ProposedAction::$v\|::$v(" \
    plugins/model/src/yunxi/core_model.rs plugins/model/src/yunxi/delivery.rs \
    plugins/model/src/proactive_chat/mod.rs | paste -sd+ | bc
done
# send_message: 1   reach_out: 1   use_tool: 7
# create_open_loop: 0   resolve_open_loop: 0   start_goal: 0   cancel_goal: 0
```

宿主**一次都没有**构造过那四个承诺类动作。它们是 Core 内部记账，而同一批语义在
`StateUpdateProposal` 里已经有了。

---

## 3. 现状（2026-09-16 核实，动手前重验）

- `ActionCapability`（`crates/yunxi-core/src/arbiter.rs:39`）：`SendMessage` / `ReachOut` /
  `UseTool` / `CreateOpenLoop` / `ResolveOpenLoop` / `StartGoal` / `CancelGoal`。
- `ProposedAction`（`action.rs:782`）：上面七个各一个变体 + `Noop`。
- `CognitiveIntent`（`intent.rs:28`）：同样七个 + `Noop`，带手写 Wire 枚举（`deny_unknown_fields`）。
- `ActionDescriptor`（`arbiter.rs:73`）：`capability` / `allowed_scopes` / `tool` /
  `effect` / `may_carry_foreign_text`。**`ActionDescriptor::tool(...)` 里 `capability` 恒为
  `UseTool`**，工具名放在 `tool` 字段——工具那条路已经是泛化的。
- `EffectScope`：`ReadOnly` / `UserScoped` / `Outbound`。它把两个轴混在一起：
  "读还是写" 与 "波及多远"。
- 档位检查**只有两处**，且**两处都被 `if let ProposedAction::UseTool(_)` 守着**：
  `arbiter.rs:1034`、`runtime.rs:1822`。档位只卡工具。
- `AuthorizationPolicy`（`arbiter.rs:304`）：七个能力各一位 `allow_*`，
  外加 `allowed_actors` / `allowed_people` / `allowed_conversations` / `owner` / `admin_override`。
- `DecisionDisposition`（`planner.rs:993`）：`Reply` / `Silent` / `Defer` / `ReactOnly` /
  `AskQuestion` / `ChangeTopic` / `ResumeAgenda` / `SpecialAction`。
- `TurnReport.delivered_replies`（`driver.rs:65`）：Core 侧"这一轮她说了什么"的账。

---

## 4. 目标形态（建议的标识符，待定名）

| 抽象 | 建议标识符 | 取代 |
|---|---|---|
| 知觉 | `Perception`（事件侧沿用 `EventType` / `AttentionSystem`） | 无（本来就是对的） |
| 表达 | `Speak` / `SpeakAction` | `SendMessage` + `ReachOut` |
| 作为 | `Act` / `ActAction` | `UseTool` |
| 承诺 | `Commitment` + 只留 `StateUpdateProposal` | 四个动作变体 + 两套通道 |
| 沉默 | `Silence` | `Noop` + `DecisionDisposition::Silent` |

`Speak` 的载荷建议：`target`（本会话 / 某人 / 某处）× `medium`（`Text` / `Voice` / `Image` /
`Call`）× `content` × **`is_turn_output: bool`**（见 §6.2）。

---

## 5. 硬约束：**改名会撞上落盘数据**

这是这份台账里最要紧的一节。改枚举名不只是代码改动。

### 5.1 数据库 CHECK 约束里硬编码了能力名（红线②）

`plugins/model/src/yunxi/delivery_ledger.rs:241`（CHECK 在 249） 建的表：

```sql
action_kind      TEXT NOT NULL CHECK (action_kind IN ('send_message', 'reach_out')),
target_kind      TEXT NOT NULL CHECK (target_kind IN ('conversation', 'person')),
status           TEXT NOT NULL CHECK (status IN ('prepared','committed','sent','unknown','failed')),
```

表名 `yunxi_action_delivery_ledger`，**是活表、有数据**。`send_message` / `reach_out` 这两个
取值同时来自代码（`DeliveryActionKind`）。改名的代价是：
`ALTER TABLE ... DROP CONSTRAINT` + 回填 + `ADD CONSTRAINT`——属于五类红线里的
**数据库迁移**，必须单独审批、单独一步、有回滚脚本。

顺带注意：`target_kind IN ('conversation','person')` **已经就是"表达"的目标轴**——
数据模型比代码更早承认了这件事。

### 5.2 JSONB 里存了会被改名的枚举

| 表 / 列 | 存的内容 | 风险 |
|---|---|---|
| `yunxi_decision_records.record` (JSONB) | `DecisionRecord { disposition: DecisionDisposition, selected_action: Option<DecisionActionKind>, .. }` | 改名即老记录读不出。**有 `expires_at`**，可倚赖过期 |
| `yunxi_expectations.expected_event` / `.payload` (JSONB) | `PlanExpectation::ExpectEvent { event_type: EventType, .. }` | `EventType` 取值已落盘。**`expires_at` 可为 NULL**（可长期存活）→ 需要 serde alias 读兼容，不能只靠过期 |
| `yunxi_plans.payload` (JSONB) | 计划载荷 | 需查是否含动作种类 |
| `yunxi_executive_snapshots.snapshot` (JSONB) | 执行态快照 | 需查是否含期望/计划 |

### 5.3 已核实**不会**受影响

- `ActionCapability` / `ProposedAction` / `CognitiveIntent` / `DecisionPlan`：全仓库
  **没有任何 sqlx / redis 写入**它们（`grep -iE "sqlx|query!|redis|INSERT"` 为空）。
- **能力快照不落盘**：每轮现建（`bridge.rs` 里 `adapter.capabilities()` →
  `ActionArbiterConfig::with_capabilities(...)`），没有 store/save/load。
  所以**删**一个能力变体是安全的（`StartCall` 那次已按此核实）。

**结论**：删变体安全，**改名不安全**。所以"改名"必须是最后一步，且与数据库迁移同批审批。

---

## 6. 不能破的不变量

### 6.1 回复必须永远发得出去

"她读过外人文字之后，仍然必须能回答正在跟她说话的人"——今天这条**隐式**地靠
"档位只检查 `UseTool`"实现（`runtime.rs:~1820` 那段注释写明了意图：
*"may still read and may still change its own person's state, but must not speak in her name"*）。

重构后这条必须有**名字**，不能继续靠"恰好只检查了某一种动作"。同时要修一处现存不一致：
**`ReachOut` 走的是 `ProposedAction::ReachOut`，因此完全不受档位约束**，而同样"打电话"
走 `call.start` 时就受。今天不可达（`reach_out` 只有宿主 `proactive_chat` 会构造，模型
不能提出 `ReachOut`；那条路是定时器 + 记忆生成话题，不读外人文字），但形状上不一致。

### 6.2 Core 必须知道"这一轮她说了什么"

`TurnReport.delivered_replies`、`directive=Wait/Continue`、turn shape 分析都依赖它。
**这就是我不建议把回复做成普通注册表工具的理由**（详见 §9）。回复是 `Speak` 的一个
实例（目标＝本会话），"是本轮产出"是它的**属性**。

### 6.3 别动的那些

- 宿主侧的回复协议：气泡切分、`[[VOICE]]` / `[[SING]]` / `[[STICKER]]` 标记、
  `reply_action` 的 @ / 引用 / 条数契约、沉默与空回复修复、语义解析与围栏。
- 工具侧：未声明工具 fail-closed；档位收窄；`may_carry_foreign_text`。
- 电话侧：只拨名单内的人、如实回报 AVSDK 回执（不假装成功）、三道闸门默认放开但生效、
  开场白与通话事件（`CallEnded`）。
- 投递侧：幂等键语义、投递账本的状态机、`prepared_outgoing` 竞争处理与路由复核。

---

## 7. 迁移计划（四步，每步可单独回滚）

### 第一步：属性显式化（无对外行为变化）

把"波及范围"和"是不是本轮产出"变成显式属性，而不是靠"是不是 `UseTool`"隐式表达。
建议把 `EffectScope` 里混在一起的两个轴拆开：

- `Reach { SelfOnly, ThisConversation, Elsewhere }`（波及多远）
- `Reversible` / `Visible`（可逆、对外可见）
- `is_turn_output: bool`（是不是本轮的产出）

**顺带修掉 §6.1 那处 `ReachOut` 绕过档位。**

- 验证：现有测试全绿 + 新增"回复在读过外人文字后仍可发"与"ReachOut 受档位约束"两条。
- 回滚：这一步不动枚举与 wire 格式。

### 第二步：表达收口成 `Speak`

`SendMessage` + `ReachOut`（含 `ReachOutMedium::Call`）合成一个动作，目标与媒介为数据。
**对外行为与回复协议保持不变**；`group.message.send` / `private.message.send` / `call.start`
先不合并（它们是"作为"里的能力名，只是恰好也在表达——是否归并见 §10）。

- 验证：回复链路端到端不变（气泡数、@、引用、语音/唱歌/表情标记、`delivered_replies`）；
  主动接触的电话路径不变。
- 回滚：合成前的能力变体保留为别名一段时间。

### 第三步：承诺只留状态更新通道

删掉 `CreateOpenLoop` / `ResolveOpenLoop` / `StartGoal` / `CancelGoal` 四个动作变体、
`permits` 的四个分支、`AuthorizationPolicy` 的四位，以及 `DecisionActionKind` 里对应的取值
（**注意 §5.2 的落盘风险**）。

- 验证：目标/未结之事的创建、解析、延后全链路测试；`#立场` / 执行态自检不变。
- 回滚：这一步要迁数据，回滚脚本必须同批准备。

### 第四步：改名 + 收口能力枚举 + 数据库迁移（**红线，单独审批**）

`ActionCapability` → `{ Speak, Act }`；`permits` → `allow_speak` / `allow_act`；
`DecisionDisposition::Silent` → 沉默的新名；按 §5 处理三处落盘。

**顺序建议**：第一、二、三步先做（不碰 wire 与数据），第四步单独一批，
`CHECK` 约束的过渡策略见 §10。

---

## 8. "泛化成功"的度量

加一个新的对外能力（例如"视频通话""发朋友圈""发语音条"）时，**Core 的改动处数为 0**：
只写一条声明 + 数据。今天的基线是 8–10 处。

---

## 9. 我明确不建议做的事

**不要把"回复"变成 `time.now` 那样的普通注册表工具。** 理由不是"现状也能用"：

Core 必须知道**这一轮她说了什么**（§6.2）。如果回复变成普通工具，Core 就得从工具结果里
**反推**这件事，那比留一个概念更差。正确做法是：回复是 `Speak` 这个动作的一个实例
（目标＝本会话），"本轮产出"是它的属性。

这样两边都满足：新媒介/新落点零 Core 改动（泛化拿到了），而"本轮说了什么"仍然是 Core
的一等事实。

---

## 10. 需要新对话定的开放问题

1. **五个的最终标识符**：台账给的是建议（`Perception` / `Speak` / `Act` / `Commitment` /
   `Silence`）。定名后所有文档与代码一起改。
2. **`message.recall` 归哪**：它是"表达"的逆操作。倾向放在"作为"（她是用能力撤销，不是
   在说话），但语义上贴着表达。
3. **`group.message.send` / `private.message.send` 是否也并进 `Speak`**：并入后
   "表达"只剩一个入口，但这两者今天受档位约束、而回复豁免——并入意味着一部分 `Speak`
   受约束、一部分不受，属性要能表达这件事。
4. **`CHECK` 约束怎么过渡**：过渡期"同时接受新旧值"（宽松、可回滚），还是"回填 + 切换"
   （干净、但需要停写窗口）？
5. **`EventType` 改名怎么办**：用 serde alias 读兼容，还是只改文档、线上取值不动？
   后者会留下"名不副实现"，与本仓库其它文档惯例冲突。
6. **沉默的载体**：今天 `Noop` 与"没有意图"都存在。终态留一个还是两个？
7. **`ReachOut` 这个概念名是否保留**：它其实是"表达给某人"，并入 `Speak` 后名字就多余了；
   但它在事件、执行态、文档里到处都是。

---

## 11. 验证协议（沿用本仓库既有习惯）

- 每步：`cargo test --workspace`、`cargo clippy --workspace --all-targets`、`cargo fmt --all --check`。
- **关键判据必须做变异验证**：把实现改回旧行为，确认对应测试变红。没有变异验证的"新测试"
  不算证据。
- 断言要断言**行为**而不是**形状**：本仓库已经栽过两次——拿常量跟常量比、
  用单元测试里永远为 `None` 的全局注册表导致断言空转。
- **上线后必须用真实消息验证**：这是最高流量那条路（每一轮可见回复都走它），
  自动化只能验到"没坏"，验不到"她还是她"。参照 2026-09-15 那次：`5eeff5b` 上线后
  第一个真实回合就 400，而单测全绿。
- 数据库迁移那一步：先在生产快照上演练 `ALTER`，准备回滚脚本。

---

## 12. 文档改动清单

`docs/yunxi-core-architecture.md` 里有**专门章节规定现在这个 `ProposedAction`**，
所以这次不是"改代码"，是**改架构文档**。受影响的章节（编号按当前版本）：

| 章节 | 为什么受影响 |
|---|---|
| §28 Environment Capability | 能力/声明的形态 |
| §29 Action 也必须平台无关 | **就是现在这个枚举** |
| §30 SendMessage | 并入表达 |
| §31 ReachOut | 并入表达 |
| §32 DeliveryResolver | 投递按"目标 × 媒介"而不是按动作种类分发 |
| §41–43 Attention / AttentionDisposition / MustHandle | 知觉的命名与档位 |
| §47–52 OpenLoop 系列 | 承诺只留状态通道 |
| §67 Intent 与 Action 分开 | 意图枚举同步收口 |
| §69 Action Arbiter | `permits` 与授权位 |
| §71 Goal | 承诺 |
| §79–80 Tool / 环境专属 Tool | "作为"的模板 |
| §94 Phase 6：Intent / Action | 阶段计划要对齐 |
| §100 Phase 11：Goal Event Integration | 同上 |
| §104 App 所需未来 Action | 未来动作清单要按新轴重写 |
| §133 目标 Definition of Done | 验收项 |
| §137 非目标 / 当前阶段禁止事项 | 可能要新增"禁止为新动作开能力变体" |

另：`README.md` 的文档索引表加一行指向本台账。
