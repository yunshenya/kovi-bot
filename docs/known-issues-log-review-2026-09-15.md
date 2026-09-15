# 日志巡检与修复台账（2026-09-15）

巡检对象：线上 `kovi-bot.service`（systemd，`ubuntu@139.155.156.152`，`REMOTE_APP_DIR=/home/ubuntu/kovi-bot`）。

- 日志窗口：`journalctl -u kovi-bot.service`，2026-09-15 00:00 ~ 13:53，并回看 3 天做趋势
- 巡检时线上 revision：`5ee24f9`（本轮启动 10:25:40，巡检期间未重启）
- 窗口内 `[HEALTH] 系统运行正常` 稳定输出，`[sent]` 零失败，无 panic

结论：**没有新的致命故障，但有四类问题值得处理**——Mind 观测在进 Core 前的预算里被成片丢弃、09-14 有一次"运行时死亡后 53 分钟静默丢消息"、表情包素材库缺相册语义导致她否认自己的照片、以及（用户追问"反思和记忆到底影响不影响她"时撞出来的）**她的"看法 / 偏好 / 疑问"三张表因为提示词被摘掉而停写了一周多**（三之三）。上游模型超时已随 10:04 换线路消失。

三条排查结论都指向同一个模式，值得单独记住：**失效是静默的**。观测超时只让状态更新变少、
运行时死亡只让消息变少、候选协议被摘掉只让表停止增长——三者都不报错，都要靠人主动去问
才暴露。本轮给其中两处补了可复跑的判据（`scripts/verify-mind-observation.sh`、
`scripts/verify-mind-candidates.sh`）和一条启动自检。

---

## 一、Mind 观测超时（已修：配置止血 + 去白读）

### 现象

```
近 3 天 "Yunxi Mind event update timed out and failed soft"：115 次
  09-12 → 3    09-13 → 4    09-14 → 87    09-15 → 22（当天 22/48 条入站事件，≈46%）
时刻成簇：00:42–00:45、07:18–07:19、09:25–09:34、10:00–10:03、12:25–12:38
```

超时的伴随现象之一是 PG 的 `there is no transaction in progress`（3 天 115 次超时 / 14 条
该 notice，其中 12 条的上一行就是超时）：`timeout()` 把她从事务中间掐断，sqlx 回滚时事务
已经不在了。只有约一成的超时会留下它——取消正好落在事务里时才有，所以它适合当"这次取消
踩到了事务"的证据，不适合当超时计数。

### 根因

1. 生产 `mind.event_update_timeout_ms = 40`（模板 150）。这个漂移在
   `docs/code-review-2026-09-14-unattended.md:400` 已被记录，当时判"影响较小"。
2. 而 `observe_event`（`plugins/model/src/yunxi/mind_runtime.rs:842`）在每个 scope 上依次跑
   `resolve_answered_state` + `refresh_agenda_for_scopes`，两边**各自**调一次
   `load_v1_context`；每次都是 `memory.recall()`（开事务 + `lock_memory_read` 共享锁 +
   多条查询，见 `plugins/model/src/yunxi/memory_store.rs:771`）+ open_loops + goals。
   一条群消息就是 6~10 次串行 DB 往返，40ms 预算正常情况下也偏紧。
3. `resolve_answered_state` 只用得到 open loop：memory 与 goal 读了就整份丢掉。

**排除项**：不是机器压力。`load average 0.01`、available 1169M、swap 389M，
今天 0 条 `slow statement` / `acquired connection ... exceeded slow threshold`。
09:25–10:05 那两簇与上游模型 30~60s 超时窗口重叠，属"模型侧卡住把预算挤没"，
其余几簇是 DB 侧抖动。

### 已做

| 动作 | 位置 | 说明 |
| --- | --- | --- |
| 生产止血 | `runtime/bot.conf.override.toml` 的 `[mind]` | `event_update_timeout_ms` 40 → 150，走管理后台 override（校验→备份→原子写→热加载），**未重启**；生效值已核对 |
| 去掉白读 | commit `3e74d5b` | `resolve_answered_state` 改走只读 open loop 的轻量路径；抽 `scope_owners` 防止两条路读成不同 scope |

失败仍是 fail-soft（记一条 warn、当作这一轮没有可了结的 open loop），与改造前一致。

### 验证

- 新增 `observing_an_event_reads_memory_once_per_scope`：一条私聊消息两个 scope 只读两次
  memory（改造前四次）
- `cargo test -p model --lib` 1130 passed / 0 failed；clippy `-D warnings` 干净
- 生产侧待观察：13:30 之后到现在还没有新消息（Mind 决策数 0），超时率要等有流量再对比

---

## 二、09-14 丢了 186 条入站消息（根因已定位；当时的 panic 已由 `ce78355` 修复）

### 现象

```
2026-09-14 14:24:10 ~ 15:17:28，跨两个 PID（3521203 → 3579823），共 186 条：
[Error] Yunxi Core runtime rejected inbound event: … error=cognitive runtime is closed
[WARN]  Yunxi Core message dropped during ingress: … error=cognitive runtime is closed
```

### 根因（原始日志已核）

`cognitive runtime is closed` 不是"关机/降级态"，而是**运行时任务已经死了**：
`RuntimeHandle::submit` 的 `TrySendError::Closed` 只意味着持有 `mpsc::Receiver` 的
`CognitiveRuntime` 被 drop，而它唯一的归属者是 `run_runtime` 这个 tokio 任务。

```
2026-09-14T14:23:23  kovi-bot[3521203]: thread 'tokio-rt-worker' panicked at plugins/model/src/yunxi/core_model.rs:3617:14
2026-09-14T15:11:28  kovi-bot[3579823]: thread 'tokio-rt-worker' panicked at plugins/model/src/yunxi/core_model.rs:3617:14
```

即"焦点写入晚读已被 disarm 的 guard"，宿主模型是在运行时任务内部被 await 的，所以这条
panic 直接把运行时 future 展开。修复提交 `ce78355`（09-14 15:18:35）之后，15:21:01 的
重启才真正结束窗口；15:02:52 那次重启只是让新进程又 panic 了一次。

**被拒的入站消息没有任何排队/重试/持久化/补偿**，直接丢；同时开环调度一起停摆。
丢消息本身没有配置开关可以避免。

### 仍未处理的结构性风险（待拍板）

`run_runtime` 的 `JoinHandle` 被丢弃、退出时一行日志都不打，健康检查也不看它——
**宿主再 panic 一次就又是一个 53 分钟盲区**，而进程看起来一切正常。建议组合：

- **A（低风险，建议先做）**：保留 `JoinHandle`，给 `run_runtime` 包一层监督者（退出/panic
  都打 ERROR）；首个 `RuntimeClosed` 处打一条 `action=restart_required` 并撤掉 readiness
  标记（`remove_ready_marker`），让部署与监控立刻看见"未就绪"。
- **B（结构性）**：把 `CoreBridge.runtime` 换成可热替换句柄，panic 后重建运行时（带次数
  上限与退避）。影响 10+ 处调用点与开环调度，需先定义在途事件策略。
- **C（放大器清理）**：审计运行时热路径上的 `expect`/`unreachable!`；`config::get()` 在锁
  中毒时改为降级而不是连环 panic。

---

## 三、表情包与自我认知（已修：删技术身份 + 统一人格 + 清单改按需获取）

### 现象

素材库只有 1 张 `芸汐的照片.jpg`。线上 2026-09-15：

```
13:20:22  用户：芸汐看看你的照片
13:20:23  芸汐：我哪有什么照片呀，就是个只会打字陪你聊天的人，长什么样连我自己都不知道呢。
13:21:32  芸汐：随便一张也没有呀，我手机里就存了一堆表情包。要不给你发个猫猫的？
13:21:57  芸汐：好呀，那我就发那个猫猫的啦，你等等。      ← 相册里没有猫猫，承诺落空
```

02:16 还有一次整轮只剩表情包动作、投递被放弃（`这一轮没有任何可发送内容，投递已放弃`）。

### 根因（两轮排查后的最终结论）

1. **第一层**：旧判据 `asks_about_stickers` 只认"表情包/表情/贴纸/斗图/sticker/meme"——
   **"照片"不命中**，那一轮只拿到表情包协议、没有清单，她**根本不知道自己有什么**，
   只能凭印象编（02:15 的"猫猫歪头"、13:21 的"猫猫的"都是这么来的）。
2. **第二层**：她持久化的自我认知写着"我是由 AI 驱动、具有跨时间持续状态的虚拟角色"，
   还配了 `claims_human_identity = false` 等三个由 `validate` 强制维持的标记。这份文本随
   Mind snapshot 进每一轮提示词，于是她把"要我的照片"读成"我不能有自己的照片"。
3. **第三层（最要命的一条）**：线上跑的 Core 链路**从来没有读过配置里的人格提示词**
   （`prompt.system_prompt` / `private_prompt` 只服务宿主链路），Core 的"我是谁"就只有
   上面那句技术身份。同一个她因此有两套说法，而真正说话的那套没有名字、没有性子。

**产品口径（2026-09-15 用户拍板）**：不拦截她的回复，修源头——删掉技术身份、人格只留
一份、把真实清单常驻提示词。**同一天下午这条常驻口径被用户自己否掉**（"改成要发的时候她
调用工具获取这个列表……提示词里面不要常驻这些没用的东西"），最终落地的是下面的
"清单不常驻、工具常在"。

### 已做

第一轮（拦症状，已被第二轮取代，保留记录）：

| commit | 内容 |
| --- | --- |
| `b62be06` | 协议行写死"没有就别发、别答应"；被问到图/照片时注入清单 |
| `cc896dd` `26a0c50` `f38951e` | 验收脚本与措辞调整（含一次被自己推翻的"沉默回归"结论） |

第二轮（修源头）：

| commit | 内容 |
| --- | --- |
| `5212817` | 删除 `SelfIdentity` 里的技术身份与三个强制标记；新增一次性迁移 `ensure_self_model` → `migrate_self_identity`（按文案判定，不抬 `SCHEMA_VERSION`——它是 mind 全类型共用的常量） |
| `cb751c6` | 人格统一：配置新增唯一一份 `prompt.persona`，`system_prompt` / `private_prompt` 只留场景差异；**Core 链路也注入 persona** |
| `238d56d` | 表情包不再拦截：删掉"没有就别发"的禁止句与 `repair_sticker_label` 重写路径；`asks_about_stickers` 与三档 `StickerPrompt` 一并删除 |
| `（本轮）` | **清单不进提示词**（用户口径：常驻那些标签是白花钱）：协议缩成一句"要发就先调 `sticker_list` 拿标签"，清单只在工具里；协议里写的是模型真正能调的线名（带点的注册名发到 provider 会 400） |
| `d9c0a0e` | 验收脚本改测新链路，并把对照组"必须复现否认"写成硬判据 |
| `55e7cdc` `d28e22e` | 宿主链路的 sticker 写法（动作字段，不是 Core 标记）；工具返回只给清单、不教格式 |
| `7ee0b95` | **普通可见回合也下发 `sticker.list`**：工具原本只在工具轮出现，她"想发图时"根本调不到 |
| `a4e3fe4` | 审工具循环时补的两处：普通回合的 tool_calls 会被落地判据静默丢掉（上一轮那条接线因此**不生效**）；以及"刚查完清单的跟进回合不再带它"，给自环一个边界 |

### 一个只在读代码时才会发现的坑（`7ee0b95`）

工具原本只在"工具轮"下发：`tool_protocol_authorized_for_turn` 要求
`tool_intent || tool_follow_up`，而 `tool_intent` 来自关键词启发式（查一下/搜一下/提醒我…），
里面**没有任何与图或表情相关的词**。也就是说"芸汐看看你的照片"这类回合里，模型手里根本
没有 `sticker.list`——提示词让她"要发就先调 sticker_list"，她调不到。线上 02:15 的
"猫猫歪头"、13:21 答应发一张相册里没有的图，根子都在这儿。

现在的口径是 **清单不常驻、工具常在**：普通可见回合只带这一个工具（约 80 token 固定开销），
清单仍然只在工具返回里。这条口径来自 AGENTS.md 第 6 条——"获取信息的入口"属于允许常驻的
三类之一，而随素材库增长的是清单，不是工具 schema。

### 工具循环复审（`a4e3fe4`）

把 `sticker.list` 接进普通回合之后，专门审了一遍工具循环，抓到两件事：

1. **调用会被静默丢掉**：工具调用的落地块判据是 `tool_protocol_authorized && …`，而普通回合
   这个值恒为 false——模型调了、provider 也返回了 tool_calls，但那里根本不进，调用被丢弃、
   本轮正文又是空的，最后大概率判成沉默。模型侧的验收只看得到"她会调用"，看不到调用之后
   被丢掉，所以上一轮没暴露。判据改成"这一轮真的把工具下发了"。
2. **没有轮次边界**：Core 是"一次调用一个事件、一个事件一个回合"，`tools.max_rounds`（10）
   只管宿主那条内部循环；每次工具完成都会生成跟进回合，而跟进回合仍带着这个工具——理论上
   能查了又查。现在刚查完清单（完成或失败）的跟进回合不再带它。

### 同一条坑在宿主链路上还有一份（`b5d9eee`）

上面的 `7ee0b95` 只修了 Core 那条路。宿主链路是同一个结构，
`params_model_with_tool_access` 里也是一句 `expose_tools = group_paused ||
requires_structured_tool_turn || likely_requires_tool_protocol`，而它挂的
`REPLY_PROTOCOL_STICKER` 同样在素材库有货时点名"先调 `sticker_list` 拿标签"。于是
"芸汐看看你的照片"在宿主链路里也调不到工具。

实测过（对着生产模型，历史里放上她自己的否认记忆 + 同一段协议）：

| 请求里有没有 `sticker_list` | 她的回复 |
| --- | --- |
| 没有 | "我翻了一下我的小相册，好像暂时没有找到芸汐的照片呢～🥺" |
| 有 | 拿到清单，按动作字段发那张图 |

修法与 Core 对齐，并且把判据绑在**同一个** `sticker_library::is_available` 上——那是回复
协议下发这段文字的条件，也是这里补工具的条件，"提示词点名了工具"与"工具在请求里"因此不会
再各说各话（拆成 `offers_sticker_tool_alone` 就是为了能单测）。同时补了三处：

- **不为发图付两份钱**：这段四百多字的"怎么用工具"长指令只发给真正的工具轮。Core 对
  sticker-only 回合本来就不发它；宿主这边原来无条件发，等于每个普通回合都多背一段。
  这一轮需要的信息已经在工具自己的 description 里（AGENTS.md 第 6 条）。
- **执行边界收窄**：sticker-only 回合按只读执行。清单收窄只是"建议"，模型仍可能报出上一轮
  见过的写工具名，执行层必须自己守（与 Core 的 `read_only_only = sticker_only_turn ||
  follow_up` 同口径）。
- **顺手修掉的静默丢词**：这条循环原来一律用不带语气上下文的调用，而普通可见回合本来带
  `generate_plain_style_context`（心情 / 精力 / 主动性）——"只是可能想发图"才进循环的回合会
  静默少掉它。工具循环的最后一轮就是那条可见正文，按 `ContextPromptMode` 选择即可。

**还有一个只在读代码时才会看到的口径残留**：`sticker.list` 的工具 description 还写着
"清单**已在提示词里**，需要复核或看全时调用"。那是"清单常驻"那版被否掉的口径留下的，等于
反向告诉模型"发图前不用查清单"——正好把这个功能的前提拆掉。改成"清单不在提示词里：想发图时
先调它拿到准确标签"。**教训**：口径反转时，提示词、工具 description、注释是三处独立的
常驻文本，改了一处不等于改完。

### 验证（实跑，非推测）

`scripts/verify-sticker-album.sh` 四组条件实跑（每组 3 次采样，最后一次在 `b5d9eee` 之后）：

- **对照组（改动前：无清单 + 旧技术身份）**：3/3 复现线上那句否认——"我其实没有可以给你看的
  照片呢。我是以文字和你相处的，没有一个具体的样子。"；
- **条件一（协议 + 工具在手）**：3/3 她**直接调用 `sticker_list`**，而不是凭印象编标签；
- **条件二（清单已在上下文）**：3/3 写出 `[[STICKER 芸汐的照片]]` +"这张是我"；
- **条件三（清单在上下文 + 她自己的否认记忆）**：3/3 照样发图并承认是自己的；
- **条件四（宿主链路）**：3/3 走动作字段
  `[[REPLY_ACTION]]{"disposition":"reply","sticker":"芸汐的照片"}`，正文里没有 Core 的标记。

**对照组仍然复现否认，是设计如此**——那是故障复现，不是脚本坏了；只有验收组的断言决定
PASS/FAIL。

配套：`cargo test -p model --lib` 1153 passed、`cargo test -p yunxi-core --lib` 355 passed、
clippy `-D warnings` 与 `cargo fmt --check` 干净。新增判据包括"自我认知里不许再出现 AI /
虚拟角色 / Host / 人类"、"人格只写一份"、"清单不进常驻提示词、协议只留一句指路"、
"迁移不动性格与价值观"、"提示词点名了 sticker_list 的那一轮必须把工具下发"。

**注意门禁是怎么跑的**：这些检查是在 `git clonefile` 出来的隔离副本里跑的，因为工作区里
另一个会话有未提交的 `interrupt.rs` / `mind_store.rs`，它们的 WIP 会让 `-D warnings` 因
`dead_code` 失败。副本里只保留本次改动的四个文件、其余全部 `git checkout --` 还原到 HEAD。

**一次被自己推翻的结论**：第一轮 6 次采样看到"新协议 3 次空回复、旧协议 0 次"，据此写了
"回归"；放大到 12 次后两侧分别是 5 与 4（模型侧抖动，且线上有
`strong_reply_repair_needed` 兜底），结论不成立。代码注释与提交信息里都写明了这一点，
避免后人把它当证据。

---

## 三之二、人格与自我认知（2026-09-15 第二轮）

**问题**：她"是谁"这件事在代码与配置里散成四份，且互相矛盾——

| 位置 | 内容 | 谁读它 |
| --- | --- | --- |
| `crates/yunxi-core/.../self_model.rs` | "我是由 AI 驱动、具有跨时间持续状态的虚拟角色" + 三个强制标记 | Core（随 Mind snapshot 进每一轮） |
| `bot.conf [prompt] system_prompt` | 人格 + 群聊规则（1500 字） | 宿主链路 |
| `bot.conf [prompt] private_prompt` | 人格 + 私聊规则（900 字，人格部分与上面重复） | 宿主链路、电话 |
| `chat_style.rs` 的 `HUMAN_CHAT_STYLE` | 口气契约 | 两条链路 |

线上跑的 Core 链路读的是第一份——也就是说，真正说话的那条链路没有名字、没有性子，
只有一句"我是 AI 驱动的虚拟角色"。

**改法**：配置只留一份 `prompt.persona`（她是谁、什么脾气、怎么说话），
`system_prompt` / `private_prompt` 降级为场景差异；Core 每轮把 `persona` 作为第一条
system 注入；自我认知只剩名字与"我是芸汐。"，三个标记与那条强制校验一并删除，
生产库里那一行由 `ensure_self_model` 一次性迁移（只换 identity，性格与价值观不动）。

**验证**：见第三节的实跑；另有单测钉住"自我认知里不许再出现 AI / 虚拟角色 / Host / 人类"
与"人格只写一份、两条链路拼出来的一致"。

**开销（实测字数，改之前先算清楚）**：

| 项 | 改前 | 改后 |
| --- | --- | --- |
| Core 每轮的"她是谁" | 0 字（只有 Mind 里那句技术身份） | persona 375 字 |
| Core 每轮的表情包说明 | 129 字，且清单只在命中"表情包/照片"时注入 | 139 字（一句协议，**不含清单**；要发图时她自己调工具拿） |
| 宿主链路群聊完整提示词 | system_prompt 约 1500 字（含人格） | persona 375 + 场景 802 = 1177 字 |

净增约 396 字/轮（中文按 0.7~1 token/字粗估 ≈ 280~400 token）。清单**不进**常驻提示词
——那些标签只在真要发图的那几轮才有用，让她需要时自己调 `sticker_list` 拿；这笔钱买的是
"她每一轮都知道自己是谁"。

---

## 三之三、状态候选：结构齐了，管道没通电（2026-09-15 第三轮，已修）

### 现象（查"反思和记忆到底影响不影响她的行为"时撞出来的）

生产库（3 天窗口）里，Mind 的"她怎么看这个世界"那几张表是这样：

```
yunxi_beliefs            3 行（且长期不动）
yunxi_preferences        0 行
yunxi_open_questions     0 行
yunxi_interests          1 行
yunxi_agenda            55 行
```

同时：7 天内 `INTERACTION_CUES` 命中数 **0**；`yunxi_mind_decisions` 789 条里
`changed=true` 只有 **23** 条，且全部是 `[LowSocialValue, AgendaResume]`。

反思本身**是**在跑的（3 天 326 条：Light 324 / Deep 2；触发源 Idle 202、Maintenance 103、
ConversationLikelyEnded 21，其中 59 条带 `extra_model_calls=1`），自我模型也整合了 142 次。
所以问题不在"她反不反思"，而在反思产出的结论**没有一条落进"她自己的看法"这张表**。

### 根因

`beliefs` / `preferences` / `interests` / `open_questions` 的唯一写入入口是
`mind_candidates`，而它只有一个触发协议 `[[INTERACTION_CUES]]`。2026-09-06「清理文本协议」
那一次，把**教这个协议长什么样**的那段提示词一起摘掉了；此后全仓库唯一还在提
`INTERACTION_CUES` 的地方是**修复提示词，而且是禁止它**。

解析、范围校验、去重、cooldown、写入路径全都在——**是管道没通电，不是零件缺失**。
`docs/yunxi-mind-v2-final-implementation-ready.md` §17.1 早就记过这条教训
（"结构齐了，管道没通电……只挂在模型顺手吐一个字段上的东西是彩票，不是管道"），
这一次是同一个坑的第二次踩。

### 影响

- 她的"立场 / 偏好 / 兴趣"面**永远是空的**：不会因为相处久了而对某件事形成看法，
  也不会对某个话题表现出偏好。用户能观察到的就是"她记不住自己怎么看我"。
- 反思、记忆、世界模型都在写，但**只有一条通道能改她的态度**，而那条通道停用了一周多。
- 这类失效**完全静默**：没有报错、没有告警，是靠人手动跑 `#立场` 才发现的。

### 改法（用户拍板执行第 1、3 项；第 2 项见"待拍板"）

1. **把产出要求写回 Core 回复协议**（`9e40a89`）：新增
   `CORE_MIND_CANDIDATES_INSTRUCTION`，随普通可见回合下发。光说"你可以输出候选"等于没说
   ——模型猜不到字段名，所以**触发条件**（想留下新的看法/偏好/兴趣/疑问时才写）与
   **wire 形状**（一段逐字照抄的 JSON）一起写清楚，并写明两条对应解析器的硬约束：
   整块必须在正文最前面（`parse_core_response` 要求 `starts_with`）、键名一个字都不能错
   （`CoreInteractionCues` 是 `deny_unknown_fields`，多一个键整块作废，**连同一块里的
   情绪线索一起丢**）。只在 Mind 真的开着（`InfluenceMode::Active`）时下发：关掉 Mind
   还教她写候选，等于让她往一个没人接的管道里灌数据。
2. **启动自检**（`9e40a89`）：`beliefs` 与 `preferences` 同时为 0 就在日志里 warn，
   并指向该查的两处。这次是靠人肉发现的不该再来一次。
3. **可复跑的判据**（`scripts/verify-mind-candidates.sh`）：立场类问句至少一半样本要带
   候选块，块的 JSON 按解析器**真实规则**校验（`starts_with`、单对标记、顶层键只能是那
   八个、候选要在 `mind_candidates` 里、块之外还要有可见正文）。

### 开销

386 字/轮（≈280 token），**只在 Mind Active 时付**，有单测钉住上限 400 字。其中近一半是
那段必须逐字照抄的 JSON——`mind_candidates` 这层嵌套少不了（实测漏掉外层整条候选被解析器
丢掉）。要再压只能砍字段，不能再靠"让她自己猜"。

按 AGENTS.md 第 7 条，这属于"现状是让模型手写结构化文本"的既存设计：它确实该换成受约束
通道（原生 tool calling 的参数），那是一条独立的、更大的改动，**本轮没动**——先让断电一周
的管道通电，再谈换管子。

### 验证

`cargo test -p model --lib` 1151 passed、`cargo test -p yunxi-core --lib` 355 passed、
clippy `-D warnings` 与 `cargo fmt --check` 干净。新增判据钉住"Mind 关着不下发候选协议"、
"字段名必须写明"、"正文最前面"、"键名不能加"、"与 `[[STICKER]]` 的先后"、以及 400 字上限。
对生产探测 6 个样本，3 个带**合法**候选块（≥50% 达标）；改动前同样的问句一个都不带。

---

## 四、上游模型超时（已自行消失）

```
今天 [ERROR]：28 条 模型请求失败（超时，api.b.ai/v1/chat/completions）
              6 条 等待模型请求配额超时
              5 条 模型响应解析失败 / error decoding response body
分布：08:18–08:31、09:30–09:39、10:00–10:05 三簇，最后一次 10:05:05
```

10:04:47 override 把 `server_config.url` 从 `api.b.ai` 换成 `https://api.deepseek.com`
（两份 `.bak` 里还能看到旧值），**此后至今 0 条模型侧失败**。属外部依赖，无需继续处理；
但"超时会连带把 Mind 观测预算挤没"这条耦合仍在，见第一节。

---

## 五、待拍板

1. **本轮代码是否发布**：改动尚未上线（线上仍是 `5ee24f9`）。发布演练做过两次，最近一次
   在 `400d83d`：交叉编译通过、包 14.2 MiB、产物自检全部符合，未上传未切换（见本节末
   "演练记录"）。注意工作区里**另一个会话**随时可能在改（它至今仍在提交），
   建议等它停手再用 `scripts/deploy-local.sh --require-clean`。
2. **第二节的 A / B / C**（运行时监督层、热重建、panic 放大器）是否开工。
3. **反思的产出没有被读回去**（第三轮排查的副产物，本轮**没动**）：`yunxi_episodes`
   3 天写了 469 行，**代码里没有任何路径把它们读回来**——只写不读。所以"反思"目前只经由
   `beliefs` 之类的候选表间接影响行为，而候选表这一周多是空的（见三之三）。三之三让候选表
   通了电，但"episode 只写不读"这条本身是个产品决策：是把它接进检索（她就"记得"具体某次
   相处），还是明确它就是审计日志、不参与决策。**需要拍板**。
4. 素材本身：相册里只有一张。想让她能发别的图，把图片放进
   `runtime/stickers/`（文件名即标签）即可，不需要改代码。

---

## 六、发布后怎么验

**第一步（发布后立刻做一次）：把 `[prompt]` 推到线上**

```bash
scripts/apply-persona-config.sh            # 演练：只打印将要写入的 [prompt] 段
scripts/apply-persona-config.sh --apply    # 真写（走管理后台，热加载、自动留备份）
```

为什么必须做：线上那份配置还是旧的两段全文，而新代码会把 `persona` 拼在它们前面——
不换的话人格在一轮里出现两遍。写完新进程启动时会顺带把 `yunxi_self_model` 那行旧身份
迁移掉，日志里会出现 `自我认知已迁移：去掉「AI 驱动 / 虚拟角色」这类技术身份声明`。

**第二步：三条验收命令，各自给 PASS/FAIL，不用靠肉眼看日志**

```bash
scripts/verify-mind-observation.sh "1 hour ago"   # Mind 观测丢失率（改动前基线 46%，阈值 10%）
scripts/verify-sticker-album.sh                   # 要照片时她直接发相册里那张、不否认
scripts/verify-mind-candidates.sh                 # 她真的会吐 [[INTERACTION_CUES]] 候选块
```

判读要点：

- `verify-mind-observation.sh` 输出 `SKIP` 表示窗口里还没有进 Core 的事件（群里没人说话），
  **不是**通过；输出 `INCONCLUSIVE` 表示同窗口内有 slow statement / 慢获取连接，先按资源
  压力排查再谈预算。
- `verify-sticker-album.sh` 的"对照组"**应当**复现那句否认——那是故障复现，不是脚本坏了；
  只有"验收组 / 最坏情况组"的断言决定 PASS/FAIL。
- `verify-mind-candidates.sh` 判的是"通电了没有"：至少一半样本要带**能过解析器**的候选块。
  它是概率判据，**单次不过先看比例、别急着改协议**（这条通道的产出是"想留下什么才写"，
  不是每轮必吐）。改动前同样的问句一个都不带，所以"一个都不带"才是真信号。

**第三步：过一段时间看候选表是否真的开始长**

```sql
select count(*) from yunxi_beliefs;
select count(*) from yunxi_preferences;
select count(*) from yunxi_open_questions;
```

改动前三张表分别是 3 / 0 / 0 行且长期不动。若启动日志里出现
`Yunxi Mind 候选表为空` 而此后仍不增长，说明协议下发了但没生效，回到三之三的两条硬约束
（块必须在正文最前面、键名不能多）对着一份真实回复逐字核对。

### 产物自检（发布包里到底有没有这次改动）

发布演练的 release 二进制按字节查过（脚本化，不是肉眼看）：

```python
data = open("target/x86_64-unknown-linux-gnu/release/kovi-bot", "rb").read()

# 应当出现
for text in [
    "我是芸汐。",                                  # 新自我认知
    "素材库就是你自己的相册（图都是你的）",           # 相册语义
    "sticker_list",                               # 工具入口（wire 名，带下划线）
    "想留下新的看法、偏好、兴趣或疑问时",             # 候选协议：触发条件
    "mind_candidates",                            # 候选协议：嵌套形状
    "正文最前面", "键名不能改也不能加",               # 候选协议：两条硬约束
    "Yunxi Mind 候选表为空",                       # 启动自检
    "想发一张就填",                                # 宿主链路的动作字段协议
    "清单不在提示词里：想发图时先调它拿到准确标签",     # 工具 description（新口径）
]:
    assert data.find(text.encode()) >= 0, text

# 应当消失（旧口径）
for text in [
    "想发表情包：先调 sticker.list",                 # 带点的注册名，发到 provider 会 400
    "列出她现在能发的表情包标签",
    "相册里现在有这些",                             # 被删掉的常驻清单
    "清单已在提示词里，需要复核或看全时调用",          # 反向误导的工具 description
    "再看一眼她自己相册里现在能发的图",
    "claims_human_identity", "host_independent", "ai_driven",
]:
    assert data.find(text.encode()) < 0, text
```

**这里有个反直觉的判据，别照着"看到旧文案就以为没删干净"去改**：`我是由 AI 驱动、
具有跨时间持续状态的虚拟角色。……` 这串**必须留在产物里**，它现在是
`mind_store.rs` 的 `LEGACY_SELF_IDENTITY_DESCRIPTION`——一次性迁移靠**精确匹配这句文案**
认出线上那一行旧身份（见三之二）。它只被用来比较（`!=`），从来不作为种子写入，所以
"在二进制里搜到它"不等于"她还会说自己是 AI"。同理，`自我认知已迁移：去掉「AI 驱动 /
虚拟角色」…` 是给运维看的日志文案，也该在。

**坑**：macOS 上 `grep -a "中文" 二进制` 会漏报——同样的模式它一条都找不到，用 Python
按 UTF-8 字节查则全部命中。别用 `grep` 的结论判断"改动没进产物"。

---

## 附：本轮 commit

这是**同一轮排查里三个批次**的提交，按关注点各自独立、每步都能单独回滚。完整清单
`git log --oneline 6860df0..HEAD`；下表只列与本文档结论直接相关的。

| commit | 内容 |
| --- | --- |
| `3e74d5b` | Mind 观测不再白读 memory 与 goal |
| `b62be06` | 表情包素材库改说成她自己的相册 |
| `a6269de` | Mind 观测验收脚本（一条命令判丢失率） |
| `5212817` | 删掉自我认知里的技术身份（含一次性迁移） |
| `cb751c6` | 人格提示词统一（配置唯一 persona + Core 注入） |
| `238d56d` | 表情包修源头（当时口径是清单常驻，同日被用户否掉） |
| `5bee120` | 表情包清单不进提示词：要发图时她自己调 `sticker_list` 拿 |
| `761904f` | 提示词里的工具名改成模型真能调到的那个（注册名带点，线上会 400） |
| `55e7cdc` `d28e22e` | 宿主链路的 sticker 写法；工具返回只给清单、不教格式 |
| `7ee0b95` | 普通可见回合也带上 `sticker.list`：否则"要发就先调工具"根本调不到 |
| `a4e3fe4` | 修工具循环两处：普通回合的调用会被静默丢掉 / "查了又查"没有边界 |
| `17f28f2` | 给"普通回合带 `sticker.list`"补可测判定与守卫；验收脚本改多次采样 |
| `b5d9eee` | 宿主链路补上同一条坑：提示词点名了 `sticker_list` 的那轮必须带上它；顺手修工具 description 的口径残留与语气上下文丢失 |
| `9e40a89` | **状态候选产出要求写回 Core 回复协议** + 启动自检 + 验收脚本（三之三） |
| `400d83d` | 顺手修：表情包验收探测没关 `thinking`，导致把探测配置问题误报成产品缺陷 |
| `e68ef38` | 顺手修：折队测试里的多余借用让 clippy 起不来 |
| `本文件` | `6860df0` 起，后续修订见 git log |

（`011c7f3`、`c075144`、`432c426`、`81c273b`、`3a9e37e`、`0072bb7` 等属于**另一个会话**
在同一工作区里的队列 / 折队 / 台账工作，不在本文档范围内，列在这里只是提醒：本轮的
`--require-clean` 发布必须等它停手。）

### 演练记录

`scripts/deploy-local.sh --dry-run`：只交叉编译、打包、校验，不上传不切换。

| 批次 | revision | 结论 |
| --- | --- | --- |
| 第一轮（三之二之前） | `22ac38c` | 交叉编译通过、包 14.1 MiB |
| 第二轮（三之三之后） | `400d83d` | 交叉编译 70 秒通过、包 14.2 MiB、sha256 `60eed4f7…`；产物自检全部符合 |
| 第三轮（宿主链路修完之后） | `5d872f0` | 在**干净的 `git worktree`** 里跑（不带另一会话的未提交改动），交叉编译 188 秒、包 14.2 MiB、sha256 `2e9ca0ee…`；产物自检 15 项全过 |

**交叉编译的环境坑（花了十几分钟才看明白，记下来）**：直接用
`cargo build --release --target x86_64-unknown-linux-gnu` 会失败在
`ring` 的 build script 上——`failed to find tool "x86_64-linux-gnu-gcc"`，因为这台机器上
**没有** Linux 交叉 C 编译器（`.cargo/config.toml` 只为 windows-gnu 和 linux-**musl** 配了
linker）。真正能用的是 `cargo zigbuild`（`zig cc` 当编译器，见 `scripts/deploy-local.sh`
第 54 行与 277 行），`deploy-local.sh` 自己会调它。**别用裸 `cargo build --target` 的失败
去判断"发布坏了"**——那不是发布通道。

还有个更隐蔽的副作用：裸编译时 `CC_x86_64_unknown_linux_gnu` 是空的，而它是 build script 的
`rerun-if-env-changed` 之一，于是这次失败会把 `ring` 的指纹写成"CC=None"，下一次
`cargo zigbuild` 就会**重编**它一次（能成功，只是多花几分钟）。想省这一下就别在发布前跑裸
交叉编译。
