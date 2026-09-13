# 记忆改变行为：相处信号 → 群聊静默门控（2026-09-14）

让长期记忆与关系状态**真的改变她的行为**，而不只是"她知道"。触发背景：2026-09-13
群 `641996763`，白浅骂了一句，宿主按点名直答照常回了她，还把"这句话我就不接了"
写进了正文——她对"这个人一直这样对我"没有任何发言权。

## 结论先行：不用新造状态，要接的是两条断掉的线

动手前查了三件事，改变了整个设计：

1. **core 已经有关系模型**：`RelationState { familiarity, affinity, trust, comfort,
   tension }`（`crates/yunxi-core/src/planner.rs`），落库在 `yunxi_relations`，
   `PostgresRelationStore` 带 `drift_relation_state` 漂移——**tension 的半衰期是
   3 天**。也就是说"冷却"这件事早就有了。
2. **但负向情绪进不去关系**：`apply_interaction_cues` 只把 `sentiment_valence`
   喂给 `affect.valence`（mood，小时级衰减），`relation.tension` 只有"道谢"和
   `message.stop_requested` 能碰。**骂她一晚上，关系纹丝不动。**
3. **张力也已经有了消费点**：`core_model.rs` 里 `relation.tension >= 0.35` 会让
   语气变成"和对方还有点生分、需要分寸"。缺的不是表达能力，是**闸**。

所以本次没有新建"静默表"，而是把两条断线接上：

```
指向她的字面敌意（确定性）┐
                          ├─→ relation.tension ─→ 漂移(3天半衰期) ─→ 门控判据
模型判定的敌意（语义）    ┘        ↑                                      ↓
                              善意主动降温                        被 @ / 被引用也不回
```

## 三层改动

| 层 | 落点 | 做什么 |
| --- | --- | --- |
| 证据（字面） | `plugins/model/src/silence_signal.rs` | 只吃**指向她**的消息，判不友好/友好/中性。一条最多一分；熟人玩笑里的"笨蛋"故意不收 |
| 证据（语义）→ 关系 | `crates/yunxi-core/src/planner.rs` | 负向 valence 抬高 `tension`（0.2 混合率），正向与道谢降温；两条证据通道共用 `adjust_relation_tension` 一套刻度 |
| 闸 | `plugins/model/src/yunxi/core_model.rs` 的 `silence_gate_plan` | 与 `pre_model_plan` 同层的**模型调用前**否决点：群聊 + 张力 ≥ 0.6 + 管理员除外 → 静默 |

字面证据的写入在 Host 群聊入口（`model/group.rs` 的 `record_target_experience`），
经 `nudge_tension` 落到 `yunxi_relations`。字面命中只给 0.08（模型明确判定最高
0.2），因为它更容易误判（玩笑互怼、转述别人的话）。

## 三条硬约束（为什么可以安全上线）

1. **默认开启（2026-09-14 起）**。`[silence] enabled = true` 是默认值：相处经验
   改变行为就是这个功能的用途，装上但不开会让它在需要时恰好没作用。写成 `false`
   即回到"只看不动"——只打 `[SILENCE] shadow=true person=… tension=… threshold=…
   reason=…`，判定照跑、可见回复一条不少。
   ~~开启前的存量核对：线上 `yunxi_relations` 里活跃张力最高只有 0.085，那 40 条
   `tension = 0.8` 是 8 月底的老行，经 `updated_at` 漂移后实际不足 0.1——不存在
   "一打开就有人被静默"。~~
   **这条结论是错的，上线 3 分钟就出事故，见下一节。**它只核对了**已存在**的行，
   没核对**还会不断造出新行**的那条写入路径（`relation_store::seed_if_absent`）。
2. **管理员永远放行**。Core 的事件里只有平台无关的 `PersonId`、判不了管理员
   （`is_bot_admin` 要 QQ 号），所以结论由 Host 经 `HostMessageContext.sender_is_admin`
   带进来——唯一能解除紧张的人不能被自己触发的静默挡住。
3. **不是封禁**。张力按 3 天半衰期自然消退，善意（道谢或明确友好）按 0.12 的
   混合率主动降温；没有需要人工解封的状态。私聊一律不拦。

## 事故与修正：投影出来的张力把新人挡在门外（2026-09-14 02:09）

**现象**。新群 `687898502` 刚授权（02:09:00），02:09:39 成员 @ 她，Mind 判的是
`baseline=Reply projected=Reply`，紧接着 `[SILENCE] shadow=false person=…
tension=0.800 threshold=0.60 reason=relation_tension` 把它否决——那个群里唯一得到
回复的人是 `main_admin`（管理员放行）。

**根因**。`plugins/model/src/yunxi/mod.rs` 的 legacy 投影把 `relationship_level` 反推
成了张力：`affinity = (level - 5) / 5`、`tension = -affinity`。而 `level = 1` 是
**新用户默认值**（线上 130 条档案里 80 条），语义是"礼貌、稍微正式"（`utils.rs` 的
1..=4 档），不是"有仇"。于是等级 1 → 张力 0.8 ≥ 0.6；等级 2 → 0.6，正好压线。
`seed_if_absent` 又是在**每个新认识的人**第一次出现时跑
（`project_legacy_user_state` ← `model/utils.rs`），所以这不是存量问题，而是一条持续
出产"越线新人"的流水线：白鸽的 person 记录 02:09:05 建档，relation 同时被种上 0.8，
34 秒后门控读到的就是 0.800（间距小于 `MINIMUM_DRIFT_ELAPSED`，漂移恰好为 0）。

这个投影是 8 月的老代码，一直无害——在门控之前 `tension` 只喂一句语气提示
（≥0.35 → "和对方还有点生分"）。**是新的消费方让一个被误译的老字段有了否决权。**

**修正**。

1. 代码：投影不再写张力（恒为 0），并把它抽成纯函数 `legacy_relation_projection`
   补上不变量测试——"投影不许凭空造出张力"从此可测。张力只能由相处证据累积。
2. 数据：`scripts/backfill-relation-tension-seed.sql` 清掉库里由投影产生的张力
   （80 行），修正前值留在台账表 `yunxi_relation_tension_seed_backfill`。判据是
   `tension <= -affinity`（证据只加不减，而 tension 的漂移比 affinity 快得多，
   所以"从未有过证据"必然满足该式），而不是拿 legacy 的**当前** level 反推——
   level 会随互动上升，线上就有一条建档于等级 1、现在显示等级 2 的行会被漏掉。

**代价与残余风险**。清零按"整行张力都在该初值以内"判定，因此**已经衰减回初值以内的
真实证据会被一起清掉**（那些值都低于门控阈值 0.6，影响只到语气提示那一档）；此后新
的证据照常累积。另外：数据修正立即生效，**代码修正要下一次发布才生效**——在那之前
常驻的旧进程仍会给每个新建档的人种上 0.8。

## 已知边界与代价

1. **`[silence] negative_threshold` 是预留旋钮**：门控用的是张力阈值（代码常量
   `SILENCE_TENSION_THRESHOLD = 0.6`），不是负面消息条数。留着是为了让"几次算
   持续"将来可配，但在改为计数判据之前它不产生行为——已在配置注释里写明。
   `decay_days` / `warm_recovery_count` 同理，是那条通道的目标值说明。
2. **阈值与强度的标定（实测，不是拍脑袋）**：张力每次按 `(1 - tension)` 的
   `0.2 × 强度` 往 1.0 拉，越靠近上限越慢。到达 0.6 所需的条数：

   | 证据 | 强度 | 到 0.6 需要 |
   | --- | --- | --- |
   | 模型判定强敌意（valence -0.9 / 置信 0.9） | 0.81 | 6 条 |
   | 模型判定中等抱怨（-0.5 / 0.7） | — | **永不累积**（被准入门槛挡下） |
   | 字面辱骂/驱赶（`silence_signal`） | 0.15 | 22 条 |
   | 轻微吐槽（-0.2 / 0.5） | — | **永不累积** |

   第一次实现时语义通道没有准入门槛，量出来"中等抱怨连续 13 条即可静默"——
   等于一个心情差的群友吐槽十几句就能让她闭嘴。因此加了双重门槛
   （valence ≤ -0.5 且置信 ≥ 0.8），并把字面强度从 0.08 提到 0.15（0.08 要
   57 条，等于字面通道形同虚设）。校准过程钉在
   `ordinary_grumbling_never_reaches_the_relationship` 与
   `sustained_hostility_accumulates_and_a_single_message_barely_moves_tension`
   两条测试里。
3. **字面判据的误判类别**：玩笑式互怼、转述别人的话（"他刚才让我闭嘴"）、
   引用她的话来反驳。靠"单次只给 0.08 + 阈值 + 衰减 + 管理员放行"兜住，
   不会变成永久后果。`target_experience` 的文档注释里列了这三类。
4. **`nudge_tension` 的读改写不是原子的**：并发下可能丢一次加法。刻意不上事务——
   证据是连续事件流，丢一次只让张力升得慢一点，门控有阈值与半衰期兜着。
5. **私聊没接**：私聊的关系语气已经由人物档案的关系等级管着（`utils.rs` 里
   8-10 亲密 / 5-7 友好保持距离 / 1-4 礼貌正式），本次没有改动它。

## 验收方式

```bash
# 影子阶段：判定有没有跑、会不会误伤（线上 sudo journalctl -u kovi-bot）
sudo journalctl -u kovi-bot --since "-1 day" | grep -E "\[SILENCE\]|\[RELATION\]"
# 打开开关后：同一人再 @ 她，应当没有 [send]
# 关系张力现状（PG）
#   SELECT person_id, tension, updated_at FROM yunxi_relations ORDER BY tension DESC LIMIT 10;
```

## 阶段三（同批次完成）

### 群级降温：整个群把她当外人

个人级看的是"这个人"，群级看的是"这个群"。判据是**群压力**的累积与衰减
（`plugins/model/src/group_cooling.rs` + `yunxi_group_cooling`），压力来自
"这个群里针对她的驱赶"与"她插话后持续无人应答"。命中时**只是放弃这一次未点名
抽样机会**（`interjection_sampling_vetoed`），被 @ / 被引用**永远不受影响**。
开关是 `[silence] group_cooling_enabled`，**2026-09-14 起默认 true**（写 false 即
回到只打 `[GROUP_COOLING] shadow=true …` 的观察态）。开启前核对过线上压力表：
`yunxi_group_cooling` 是空的，不存在"一开就命中"。删除本群数据时压力一起清掉。

### 模型抽相处结论：可读的那一半

数值（张力）能决定行为但读不出"为什么"。结论由**既有**的
`purpose=stance_formation` 调用顺带产出（新增 `{"kind":"relation",…}` 候选，
两类候选**分开配额**，`STANCE_MAX_TOKENS` 320→480 以免截断整个数组），落库到
`yunxi_relation_notes`：`(scope_key, target_key)` 为主键 ⇒ 同一对象覆盖不追加，
每作用域裁到 32 条，正文 ≤200 字、对象 ≤80 字。

**它不参与门控**——这条边界在 store 的读接口注释、mind 的写入注释与本节各写了
一次，原因是它太容易被误用：让模型决定"封谁"是这套机制唯一不能碰的红线。
原提示词里"不写关于具体人的判断——那是记忆，不是看法"被改成"**看法里**不写……
看清这个人怎么对你，就用 relation 记一条"，立场仍然拒绝写人。

### 数据擦除（子代理标出的缺口，已补）

相处结论按显示名/QQ 的**文本**存，不在 Core 的身份外键级联里，因此
`#删除我的数据` 原本删不掉它们。现在：

- 按人擦除：传 QQ 号 + 外部身份 + 当前昵称三种标签（模型可能用任意一种写下结论），
  按归一化键删除（`relation_note_store::delete_targets`），标签构造有单测；
- 按群擦除：解析出群会话 id 后删该作用域全部结论
  （`delete_conversations`）；
- 两者失败都只记日志、不阻断擦除主流程。

**残余风险**：模型写出我们没见过的称呼（例如自己起的外号）时，按人擦除匹配不到；
同一人换名会产生两行、不同人同名会互相覆盖——这是"不对齐身份"的既定代价。

## 后续（仍未做）

- 静默期间的行为分级：现在只有"回/不回"，没有"回得更短更淡"这一档。
- 群级信号的恢复路径偏慢（靠压力衰减），可以考虑"她插话后有人接"加速回温。
- 后台展示相处结论（读接口 `notes_for_scope` 已就位，暂无调用点）。
