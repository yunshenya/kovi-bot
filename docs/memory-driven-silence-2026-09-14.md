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

1. **默认关闭**。`[silence] enabled = false` 时只打影子日志
   （`[SILENCE] shadow=true person=… tension=… threshold=… reason=…`），
   判定照跑、可见回复一条不少。上线顺序：先跑几天日志确认不误伤，再打开开关。
2. **管理员永远放行**。Core 的事件里只有平台无关的 `PersonId`、判不了管理员
   （`is_bot_admin` 要 QQ 号），所以结论由 Host 经 `HostMessageContext.sender_is_admin`
   带进来——唯一能解除紧张的人不能被自己触发的静默挡住。
3. **不是封禁**。张力按 3 天半衰期自然消退，善意（道谢或明确友好）按 0.12 的
   混合率主动降温；没有需要人工解封的状态。私聊一律不拦。

## 已知边界与代价

1. **`[silence] negative_threshold` 是预留旋钮**：门控用的是张力阈值（代码常量
   `SILENCE_TENSION_THRESHOLD = 0.6`），不是负面消息条数。留着是为了让"几次算
   持续"将来可配，但在改为计数判据之前它不产生行为——已在配置注释里写明。
   `decay_days` / `warm_recovery_count` 同理，是那条通道的目标值说明。
2. **"0.6 需要多久"**：单条敌对消息把张力往 1.0 拉 0.2 的比例，所以约 5 条强烈敌意
   可越过 0.6，之后 3 天衰减一半。阈值与速率都钉在测试里
   （`sustained_hostility_accumulates_and_a_single_message_barely_moves_tension`）。
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

## 后续（本次未做）

- 群级信号：整个群长期赶她 → 群级降温（动的是未点名回合的频率）。
- 模型抽取"与某人的相处结论"：挂进现成的 `purpose=stance_formation` 管线
  （已有 6 小时冷却、3 条上限、容量兜底），覆盖字面与一次性 sentiment 都看不见的
  长期模式。
- 静默期间的行为分级：现在只有"回/不回"，没有"回得更短更淡"这一档。
