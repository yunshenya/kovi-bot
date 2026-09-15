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

**先说范围**：这五个是 Core 的**接口面**（进什么、出什么、改什么、以及不做什么），
**不是"她这个人"的完整模型**。情绪、关系、记忆、时间感、习惯、人格都在别的子系统里
（Mind / affect / relation / temporal …）。把它们当成完整的人模型会误导后续设计——例如
试图把情绪塞进"表达"。**改的是接口的词表，不是她的全部。**

把五个平铺成"五个动作变体"就是把 `StartCall` 的错误放大五倍。它们的类别不同：

| 抽象 | 是什么类 | 方向 | 谁的载体 |
|---|---|---|---|
| **知觉** | 输入 | 进 | `WorldEvent` + 注意力（`AttentionSystem`） |
| **表达** | **对外动作** | 出 | 目标（本会话 / 某人 / 某处）× 媒介（文字 / 语音 / 图片 / 电话）× 内容 |
| **作为** | **对外动作** | 出 | 宿主声明的能力名（今天的 `UseTool`，已经是泛化的模板） |
| **承诺** | **Core 自己的状态** | 内 | 未结之事 / 目标 / 提醒（走状态更新通道）。**注意**：情绪、关系、话题是**状态**不是承诺——见下方注记 |
| **沉默** | 输出为零 | — | 不是动作，是"没有表达、没有作为" |

所以 `ActionCapability` 的终态是 **`Speak` + `Act` 两个**；承诺走状态通道（不是动作）；
知觉与沉默本来就不在动作枚举里。

**但"承诺"这个标签目前偏宽，是误名**（2026-09-16 写台账时发现，需要在下一轮定）：

```rust
pub enum StateUpdateProposal {          // 今天承诺类的唯一通道
    Affect(AffectState),                // 情绪  ← 是"状态"，不是承诺
    Relation(RelationState),            // 关系  ← 是"状态"，不是承诺
    SetTopic { .. },                    // 当前话题 ← 是"状态"，不是承诺
    ConversationDirective { .. },       // 她下次何时再说话 ← 更像"未来行为的指令"
    ResolveOpenLoop { .. },             // 未结之事 ← 这个才是承诺
    DeferOpenLoop { .. },               // 把未结之事推后 ← 也是承诺
}
```

按语义细分，这里至少有三类：**状态**（情绪/关系/话题）、**承诺**（未结之事；宿主侧的提醒与
Agent Run 也是承诺，只是走声明）、**未来行为指令**（`ConversationDirective`）。
所以**"五个"这个数字本身有两条路，必须选一条**：

- **路 A**：把"承诺"放宽成"改她自己"（自省面），六个变体都归它。名字松，但数字保持五个。
- **路 B**：承认"状态"与"承诺"是两个类别 → **就是六个**：知觉、表达、作为、状态、承诺、沉默。

我倾向 **路 B**（"她现在的状态"和"她答应过的未来"是两种东西，混在一起会让"承诺"这个
概念没法用来推理——而 `ProactiveMotive::FollowUp`、未结之事、提醒恰恰都需要它准确）。

### 1.1 任何"新东西"先过这四个问题

这是本台账的可复用判据，也是 `StartCall` 那次错误的补救：

1. **它是朝外的吗？** 是 → 它是 `Speak` 或 `Act` 的**数据**，不是新变体。
2. **它改的是 Core 自己的状态吗？** 是 → 走承诺（状态更新）通道。
3. **它有独有语义吗**（档位豁免、专属协议门、专属授权位）？有 → 那是**属性**，不是**种类**。
4. **它只是新媒介或新落点吗？** 是 → 只加数据 + 一条声明。

四条都过不了，才考虑动 Core 的类型。

---

### 1.2 判据：**载荷语义归谁** → 决定 Core 的介入程度

"表达 vs 作为"不是"说什么 vs 做什么"，而是**谁拥有载荷的语义**。类型里就写着：

```rust
// 表达：Core 认识形状
SendMessageAction { conversation_id: ConversationId, content: MessageContent, reply_to: Option<MessageId> }
// 作为：Core 只认识一个名字 + 一条声明
ToolAction        { tool_name: String, input: String, scope: ActionScope }   // input 是不透明 String
```

**这条判据决定 Core 能介入多少**——也就是整个抽象值不值得的关键：

| | Core 能管什么 | 靠什么管 | 管不到什么 |
|---|---|---|---|
| **表达** | 目标、内容、是否本轮产出、记账、档位豁免 | Core **自己的类型** | ——（判断力**完整**） |
| **作为** | 授权、声明效果（档位）、收到结果 | 宿主的**声明** | 目标与语义 → **必须在效果边界由宿主复核**（`revalidate_tool_effect` 就是这件事） |

推论，都要当规则用：

- **规则 A**：Core 必须对"目标或内容"有判断力的 → 必须是**表达**；只能授权和路由的 → **作为**，
  并接受"细节委托给宿主复核"。
- **规则 B**：**表达侧的安全性自足**（内容由 Core 自己 typed，不依赖宿主）；**作为侧的安全性
  等于声明的诚实度**（宿主把 `Outbound` 谎报成 `ReadOnly`，档位防线就没了）。
  两侧这个不对称是本质的，不是缺陷——但必须写下来，否则"新东西默认归哪"会变成看方便。
- **规则 C**（防垃圾桶）：声明很便宜，于是所有难的都容易被塞进"作为"，Core 的 typed 表面
  随之萎缩、她的判断力退化成"转发给宿主"。**Core 必须对目标或内容有判断的，不许图省事
  走作为。**

### 1.3 知觉侧是**同一个问题**，而且已经落盘

`EventType`（`crates/yunxi-core/src/event.rs:977`）是个封闭枚举，**没有任何宿主扩展点**：

```rust
pub enum EventType { MessageReceived, ..., CallEnded, IdleTick, MaintenanceTick, HostStarted, HostStopping }
```

不用推演——**2026-09-16 加 `CallEnded` 时亲手走过同一串流水线**（commit `4ffa137`）：
`WorldEventKind` 变体 + `EventType` 变体 + `event_type()` 映射 + 注意力档 +
`baseline_disposition` + 路由解析 + 提示词分支 + 落盘取值。**与 `StartCall` 同构的 6–8 处。**

所以：**`StartCall` 不是偶发失误，是"封闭枚举"这个设计在输入与输出两侧各犯了一次。**
只改输出侧等于只做了一半。对称的形态：

| | 种类（**宿主声明**） | 类别/属性（**Core 拥有**） | Core 的介入 |
|---|---|---|---|
| **知觉** | 宿主声明的事件种类（"电话结束了""构建失败""传感器触发"） | Core 拥有：注意力档（忽略/观察/关注/必办） | 按**属性**决定要不要花一轮，不认识每种 kind |
| **表达** | 宿主声明的通道与媒介 | Core 拥有：目标、内容、是否本轮产出 | 完整理解 + 记账 |
| **作为** | 宿主声明的能力 | Core 拥有：声明效果、授权 | 授权 + 档位；细节宿主复核 |
| **承诺** | 宿主声明的提醒/存储 | Core 拥有：目标、未结之事的语义 | Core 自己的状态 |
| **沉默** | —— | Core | 无动作 |

第一行是重点：今天"这个事件值不值得花一轮"是 Core **认识每一种 kind** 才算出来的
（`AttentionSystem::evaluate` 的 match）。对称之后应该由**声明的属性**决定，就像输出侧
"这个工具能走多远"由声明的 `effect` 决定。那样加一个新事件种类就是加一条声明。

**额外约束**：`EventType` 的取值**已经在落盘数据里**（`yunxi_expectations.expected_event`，
且 `expires_at` 可为 NULL）。知觉侧一旦改成宿主声明，必须回答"历史取值怎么读"（见 §5.2）。

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

### 2.2 "表达"今天被拆成五套机制

| 机制 | 归属 | 档位 | 落点 |
|---|---|---|---|
| `SendMessage`（回复） | **能力** | **豁免** | 本会话 |
| `ReachOut` | **能力** | **不受约束**（见 §6.1） | 某人（`ReachOutMedium::Call` 是电话） |
| `group.message.send` | 工具 | 受约束 | 别的群 |
| `private.message.send` | 工具 | 受约束 | 别人 |
| `call.start` | 工具 | 受约束 | 本会话那个人（电话） |

五套机制里做的事是同一件：**把某个东西说出来，给某个目标，用某种媒介**。它们之所以
分裂，是因为枚举按"落点"开变体。**同一个"打电话"在两条路上档位待遇不同**
（`reach_out` 不受约束、`call.start` 受约束，见 §6.1）——这就是分裂的代价。

（`message.recall` 不在上表：它是"表达的逆"，按 §1.2 的判据归**作为**——Core 只需授权
与路由，不需要理解内容语义。）

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
| 知觉 | `Perception`（宿主声明种类+属性，Core 拥有注意力档） | **不取代任何东西——今天它和输出侧是同一种病**，见 §1.3 / 第六步 |
| 表达 | `Speak` / `SpeakAction` | `SendMessage` + `ReachOut` |
| 作为 | `Act` / `ActAction` | `UseTool` |
| 承诺 | `Commitment` + 只留 `StateUpdateProposal` | 四个动作变体 + 两套通道 |
| **状态**（**待定**，见 §10 第 12 条） | 若走"路 B"则单列：`State` | 今天混在 `StateUpdateProposal` 里 |
| 沉默 | `Silence` | `Noop` + `DecisionDisposition::Silent` |

`Speak` 的载荷建议：`target`（本会话 / 某人 / 某处）× `medium`（`Text` / `Voice` / `Image` /
`Call`）× `content` × **`is_turn_output: bool`**（见 §6.2）。

**这张表的行数取决于 §10 第 12 条**：选"路 A"（承诺泛指"改她自己"）就是五行；
选"路 B"（状态与承诺分开）就是六行。**先定那个，再定名。**

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
"档位只检查 `UseTool`"实现（`runtime.rs:1822` 那段注释写明了意图：
*"may still read and may still change its own person's state, but must not speak in her name"*）。

重构后这条必须有**名字**，不能继续靠"恰好只检查了某一种动作"。同时要修一处现存不一致：
**`ReachOut` 走的是 `ProposedAction::ReachOut`，因此完全不受档位约束**，而同样"打电话"
走 `call.start` 时就受。今天不可达（`reach_out` 只有宿主 `proactive_chat` 会构造，模型
不能提出 `ReachOut`；那条路是定时器 + 记忆生成话题，不读外人文字），但形状上不一致。

### 6.2 Core 必须知道"这一轮她说了什么"

`TurnReport.delivered_replies`、`directive=Wait/Continue`、turn shape 分析都依赖它。
**这就是我不建议把回复做成普通注册表工具的理由**（详见 §9）。回复是 `Speak` 的一个
实例（目标＝本会话），"是本轮产出"是它的**属性**。

### 6.3 表达的目标集**若封闭，病就搬到目标层**

桌面通知 / TTS / 屏幕输出**没有 `ConversationId`**。两条路，必须选一条并写下来：

1. 目标集是 Core 知道的**有限枚举**（`Conversation | Person | Channel`）——表达仍然是 typed 的，
   但每出现一种新通道就要动 Core：**这是 `StartCall` 的错误在目标层的复刻**；
2. 或者这类输出只能走"作为"——**那么"回复"在某类宿主上就变回工具，"本轮说了什么"的记账
   分裂成两套**（正是 §9 反对"回复即工具"的理由在新宿主上重演）。

倾向：目标集**限定在 Core 真正会推理的东西**（本轮所属的会话、以及人），其余一律走"作为"；
将来要扩，也必须是一次**有理由的 Core 改动**，而不是默认行为。

### 6.4 两套契约 = 两套失败语义

表达失败 = **她什么都没说**（对外表现是沉默）；作为失败 = **她报告一次失败的动作**。
两者不能混。而今天 `ActionPortOutcome` 把两者塞在一个枚举里，载荷形状都不同：

```rust
Delivered { external_reference, message_id, conversation_id }   // 表达形状
DeliveryIndeterminate { reason, conversation_id }                // 表达形状
ToolCompleted { operation, output }                             // 作为形状
ToolFailed { operation, error_category, detail }                // 作为形状
Deferred { reason }                                             // 两者共用
```

这不是必须改的 bug，但它是一个**症状**：两个契约今天共用一个出口类型。收口时要明确
"哪种动作产出哪种结果"，别让"沉默"和"报错"在某个分支上等价。

### 6.5 "作为"不能变成垃圾桶

见 §1.2 规则 C。这条**不能靠类型防**——只能靠判据写下来 + 评审时问一句。
它是这次重构最大的风险，而且是文化性的，不是技术性的。

### 6.6 别动的那些

- 宿主侧的回复协议：气泡切分、`[[VOICE]]` / `[[SING]]` / `[[STICKER]]` 标记、
  `reply_action` 的 @ / 引用 / 条数契约、沉默与空回复修复、语义解析与围栏。
- 工具侧：未声明工具 fail-closed；档位收窄；`may_carry_foreign_text`。
- 电话侧：只拨名单内的人、如实回报 AVSDK 回执（不假装成功）、三道闸门默认放开但生效、
  开场白与通话事件（`CallEnded`）。
- 投递侧：幂等键语义、投递账本的状态机、`prepared_outgoing` 竞争处理与路由复核。

---

## 7. 迁移计划（六步：①–④ 必做，⑤ 可选，⑥ 独立）

**接手须知**：①–⑤ 是**输出侧**，⑥ 是**知觉侧**（独立决定）。每步一个语义完整的提交、
单独可回滚；不要把它们合成一次大改。**第一次提交应该是第一步。**

### 第一步：属性显式化（无对外行为变化）

把"波及范围"和"是不是本轮产出"变成显式属性，而不是靠"是不是 `UseTool`"隐式表达。

今天的 `EffectScope { ReadOnly, UserScoped, Outbound }` **把两个轴揉在一起**——它自己的文档
注释写着"deliberately about *reach*"，可 `ReadOnly` 根本不是"波及范围"，是"有没有副作用"：

- **有没有副作用**：`ReadOnly` vs 有副作用
- **波及多远**：`UserScoped`（只她自己）vs `Outbound`（对外）

拆成两个轴（建议 `Access { Read, Write }` × `Reach { Self, ThisConversation, Elsewhere }`），
再加"是不是本轮产出"（`is_turn_output: bool`）。

**两个实现上的坎，必须先知道：**

1. **它今天是靠全序工作的。** `EffectScope` derive 了 `PartialOrd, Ord`，档位检查写的是
   `effect > ceiling`（`arbiter.rs:1034`、`runtime.rs:1822`）。拆成两个轴之后这就不是
   全序而是**偏序**，`>` 不再成立，两处检查都要改成**逐轴比较**。
2. **必须保住"只读工具永不被档位拒"**：今天 `ReadOnly < UserScoped ≤ ceiling` 恒真，
   所以查询类工具从来不会因为档位消失。拆轴时这条要显式测。

**顺带修掉 §6.1 那处 `ReachOut` 绕过档位。**

`Reversible` / `Visible` 这类属性**这一步不需要**——只有真出现依赖它的判据时再加，
别在第一步就把属性集铺开。

- 验证：现有测试全绿 + 新增"回复在读过外人文字后仍可发""只读工具永不被档位拒"
  "ReachOut 受档位约束"三条。
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

### 第四步：收口能力枚举（**保留线上取值，零数据库迁移**）

`ActionCapability` → `{ Speak, Act }`；`permits` → `allow_speak` / `allow_act`。
**类型名换新、线上升级与落盘取值全部保持旧名**（`#[serde(rename = "send_message")]` 等，
先例见下方"为什么第四步与第五步要分开"）。这一步因此**不碰 wire、不碰数据、不碰 schema**。

### 第五步（可选，**红线②，单独审批**）：连线上取值也改名

只有想把 `send_message` / `reach_out` 这些**落盘取值**也换掉时才需要：按 §5 处理
`CHECK` 约束、四处 JSONB、以及 16 个文档章节。**价值是词汇统一，代价是数据库迁移**，
所以它是独立决定，不是收口的必要部分。

### 第六步（**与输出侧并行，但是独立决定**）：知觉侧同样收口

§1.3 已经说明知觉侧是同一个问题。它**不在第一到第五步里**，因为：

- 它的接口不同（宿主声明"种类 + 属性"，Core 拥有"注意力档"），
- 它**已经落盘**（`EventType` 在 `yunxi_expectations` 里，且 `expires_at` 可为 NULL），
- 它需要先定"事件属性都有哪些"（注意力档是最少的一个；可能还需要"是否要求她回应"）。

要做的话建议**先只加声明通道、不动现有 `EventType` 取值**（与第四步同样的手法：
类型侧可扩展、线上取值不变），这样它和输出侧的收口可以互相独立地回滚。
**如果这一轮不做，台账要在 §12 里写明"知觉侧尚未动"，否则下一个人会以为已经做完了。**

---


**顺序建议**：第一到第四步必做（完全不碰数据），第五步单独一批、可无限期推迟；
`CHECK` 约束的过渡策略见 §10。

**为什么第四步与第五步要分开**：**风险全部来自"改名"**（数据库 CHECK 约束、落盘 JSONB、
16 个文档章节），而**价值全部来自"结构收口"**。两者可以解耦——第四步用
`#[serde(rename = "send_message")]` / `rename = "reach_out"`（以及日志里的显示名）
**保留线上取值不变**，于是**零数据库迁移、零落盘兼容问题**。

**这不是新发明，仓库里已有先例**（`planner.rs:1017`）：

```rust
impl DecisionDisposition {
    /// Compatibility spelling for callers that use "respond" in their
    /// product language while the wire representation remains `reply`.
    pub const Respond: Self = Self::Reply;
    pub const Ignore: Self = Self::Silent;
}
```

"产品语言里叫新名字，线上取值保持旧名"已经是这里的做法。沿用它，第四步就不必是红线。

**如果新对话只做了改名而没做判据与结构收口，结果会比今天更差**（多了一层间接性，
封闭枚举的行为一模一样，只是换了个名字）。

## 8. "泛化成功"的度量

两个方向都要量，缺一个就是只做了一半（§1.3）：

- **加一个新的对外能力**（"视频通话""发朋友圈""发语音条"）：**Core 改动处数 = 0**，
  只写一条声明 + 数据。今天基线 **8–10 处**。
- **加一个新的知觉种类**（"构建失败""传感器触发"）：同样 **0 处**，只写一条声明
  （种类 + 属性）。今天基线 **6–8 处**（`CallEnded` 那次就是）。

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

8. **知觉侧要不要一起做**：如果要，是"宿主声明事件种类 + 声明属性（注意力档）"，
   还是先只把输出侧做完？只做输出侧等于只做了一半（§1.3）。
9. **表达的目标集封闭还是有限开放**（§6.3）：这是"病会不会搬到目标层"的分水岭。
10. **两套失败语义要不要拆开出口类型**（§6.4）：`ActionPortOutcome` 今天混着两种形状。
11. **历史落盘取值怎么办**（§5.2 + §1.3）：`EventType` 与 `DecisionActionKind` 都在 JSONB 里，
    是 serde alias 读兼容，还是保留线上取值只改类型名（见 §7"为什么第四步与第五步要分开"）。
12. **五个还是六个**（§1 那条说明）：把"承诺"放宽成"改她自己"（路 A，五个），
    还是承认"状态"与"承诺"是两类（路 B，六个）？我倾向路 B，理由是"她现在的状态"与
    "她答应过的未来"混在一起会让"承诺"没法用来推理，而未结之事/提醒/主动跟进恰恰需要它准确。

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
| §24 WorldEvent / §25 EventScope / §26 WorldEventKind | **知觉侧**：种类与声明属性（若做第六步） |
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
