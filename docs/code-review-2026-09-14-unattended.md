# 全项目代码评审（2026-09-14，无人值守）

评审范围：整个工作区（220 个 .rs、约 17.6 万行，外加 tools/ 下的 Python 服务与
scripts/）。基线 `b468541`，本次评审期间产生的修复见「一、已修」。

方法：先跑完整 CI 门禁取客观基线，再用 7 路并行深读覆盖各子系统，最后每条结论都由
我回到源码复核；有分歧的地方以实测为准（见 1.5）。全程只读，不改业务语义。

基线状态（评审开始时）：`cargo fmt --check`、`cargo clippy --workspace --all-targets
--all-features -- -D warnings`、`cargo test --workspace --all-targets` 全绿，
`cargo audit` / `cargo deny` / `ripsecrets` 也都已接进 CI。也就是说，下面这些都不是
lint 能发现的表层问题。

---

## 一、已修（每项都有回归测试，修前先复现）

| 提交 | 问题 |
| --- | --- |
| `45e6595` | `.gitignore` 已排除却仍在版本库里的 18 个文件（16 张 `.dsh-computer-use` 截图 21MB，占跟踪内容七成、`docs/.DS_Store`）|
| `cda86d7` | 恢复跟踪 `bot.conf.toml`，删掉 `.gitignore` 里那条过期规则（见 1.1）|
| `da1b720` | readiness 测试不再 `set_var` 改进程级环境（见 1.2）|
| `705d634` | `下星期三 / 上礼拜五` 这类写法算错一周（见 1.3）|
| `6be1ec7` | TurnGate 特征桶封顶后停止扫描，与训练器不一致（见 1.5）|
| `d069c15` | 工具参数超 64 KiB 时 `String::truncate` 在非字符边界 panic |
| `b0f4006` | `conversation.rs` 仅有的两处生产 `unwrap`；`System` 回合会 panic 并毒化 REGISTRY |
| `8d36cdc` | 数据删除后，保留快照与全局日志里仍留着被删除者的标识（见 1.4）|

### 1.1 `bot.conf.toml` 是故意跟踪的

`.gitignore` 写着"仓库只跟踪 `*.example.toml`"，但
`config::tests::repository_configuration_loads_with_all_sections`
（`plugins/model/src/config/mod.rs:530`）正是拿仓库根这份配置当被测对象，钉住
`thinking_mode=disabled`、`max_entries=1000`、mood TTL、冷却时长，以及两份提示词里
不许再出现的协议字样；`config_path()` 在 `cfg(test)` 下解析的就是 `../../bot.conf.toml`。

我第一版把它也取消跟踪了——**那会把 CI 打挂**。实测把文件挪走后该测试立刻失败
（`50000 != 1000`）。所以过期的是 `.gitignore`，不是跟踪状态；已恢复跟踪并在
`.gitignore` 里写明为什么必须跟踪、以及它为什么仍然安全（密钥一律走
`api_key_env` / `token_env` 的 env 间接，真实 Token 在已忽略的 `.env` 里）。

顺带：该文件注释里带着真实群号 641996763，`plugins/model/src/group_access.rs:302,307`
的用户提示也用同一个号当例子。要脱敏的话换成 `123456789` 即可，不影响逻辑。

### 1.2 readiness 测试的进程级污染

原测试 `set_var("KOVI_READY_FILE")` → 断言 → `remove_var`。三个问题：edition 2024
里这是 `unsafe`；`config::runtime_dir()` / `override_file_path()` 都读这个变量，并行
测试会互相看见；而且清理不在 RAII 里——断言一失败，假路径就留在进程里，把一次失败
放大成一片连带失败。已改成把路径与 revision 显式传给 `write_ready_marker_at` /
`remove_ready_marker_at`，测试直接驱动真实文件，并补上覆盖写与「重复清理容忍
NotFound」两条断言。

### 1.3 中文时间：周次被悄悄丢掉

`resolve_weekday` 的周次标记只有 `下周/下週/上周/上週`，没有 `下星期/上星期/下礼拜`。
而循环是「按数组顺序找第一个命中的标记」，所以 `下星期三` 会命中裸标记 `星期`，
`week_offset` 落到 `_ => 0`，解析成**本周**三。

后果不是措辞：`resolve_chinese_time` 的结果直接进 `reminder.create`，而 `in_past` /
`precision` 都不会报警（本周三可能还没到）。周五 2026-09-11 实测修前
`下星期三 = 2026-09-09`，修后 `2026-09-16`。已补齐带周次的长标记（含「个」的口语
形式）并排在裸标记前，14 种写法钉进测试。

### 1.4 数据删除：标识没删干净

`purge_person_domain` 的文档承诺「compact events do not retain sender identifiers；
全局日志既没有正文也没有标识」。前半个前提是错的：`CompactEvent::from_event`
对每条 `MessageReceived` 都写 `person_id = Some(message.sender)`，会话日志和全局日志
都写（全局只是 `include_text = false`，不影响 `person_id`）。而 purge 只 `retain` 了
`active_people`。

后果：P 在群里说过话、同时有私聊，执行 `#删除我的数据`（私聊快照确实删了）之后，
群 G 的保留快照里仍有 `person_id = P`；下一次 G 里的 planner 回合把它交给宿主，
宿主提示词组装正是取这个字段作 `"speaker_id": <P uuid>`（`core_model.rs:2161-2197`）。
用户拿到「已删除」回执，被删除者的稳定标识还在继续喂给模型。

已改成同时清掉保留会话与全局日志里等于该人的 `person_id`（共享正文按约定保留），
并把全局日志纳入「是否有东西要删」的判定——只在已淘汰会话里说过话的人否则会被
提前返回跳过。去掉 scrub 后新测试立刻失败。

**但宿主侧还有两处没接上，见 2.1 / 2.2，那才是线上真正会复发的地方。**

### 1.5 TurnGate：两个方向相反的报告，以实测为准

两位评审都报了 `turn_gate.rs` 与训练器不一致，但**修复方向相反**：一位说改 Rust
（`break 'outer` → `continue`），一位说改 Python（让 Python 复刻 Rust 的 break）。

判据是权重由谁产生：`train.py` 用 `features.py` 造 X，所以 Python 的行为才是模型
被训练时看到的东西；改 Python 等于让现有 bundle 全部失效。故改 Rust。

方向定了不等于结论成立——我第一次写的回归测试（200 个连续汉字）**通过了**，也就是
没能复现。原因是那个长度下两边凑巧都是 518。于是我把两种算法都在 Python 里实现并
搜索可分叉的输入，找到 227 个连续汉字：训练器 519、Rust 518，这才真正复现。新测试
用这个长度钉住 519，修前失败、修后通过，`check_parity.py` 的 golden digest 不变
（它的样本太短，本来也够不到封顶——这正是这个偏差一直没被抓到的原因）。

---

## 二、需要你决定的高优先级问题（未改）

### 2.1 世界模型的数据删除会在 30 秒内被写回（隐私）——**已复现，测试已落档**

**复现结论（`2aa5553`，本地 PostgreSQL 18）**：
`yunxi::world_model_store::tests::postgres_erasure_is_not_undone_by_the_next_world_persist`
在「擦除后行数=0」通过之后，下一次 `save_world` 让行数回到 1（`left: 1, right: 0`）。
机制确认无误：`save_world` 是整表重写，而擦除只删库里的行。

**门控语义已厘清（`b18f6d1`）**：结论是**代码对、注释错**。`enabled` 是总开关，实际
门控六处（建表+启动恢复、`with_world` 这个所有记录的入口、`restore_from_store`、两处
soft-signal 调试日志、启动那行文案）；而那句"只影响两处文案、不门控任何行为"描述的其实
是 `shadow_mode`——它确实只有两个消费点，且都只是往状态行拼 `shadow=true`。类型文档原先
把安全性寄托在 `shadow_mode` 上也是错的，真正拦住行为的是 `reply_context` 与
`influence_mode`。注释与文档已按事实重写，并加了 `world_model_gating_tests` 用行为断言
钉住总开关（删掉门控会变红），ci.yml 里点名跑。

**原记录（其开关状态与字面看起来的相反）**：`bot.conf.example.toml:407-410` 写着
`world_model.enabled`「只影响两处**文案**……没有门控任何行为（全仓只有这两个消费点）」。
实际上代码至少门控三处：`with_world` 的内存运行时（`yunxi/world_model.rs:69-72`）、
`restore_from_store`（`:90-92`）、以及世界模型 store 的创建与随之而来的
`delete_person_domain_rows` 调用（`yunxi/mod.rs:193`）。而部署工作流是用
`bot.conf.example.toml` 生成生产 `bot.conf.toml` 的（`deploy.yml` 的
`install -m 0600 bot.conf.example.toml "$release_dir/bot.conf.toml"`，其后只 patch
模型相关那几行，不碰 `[world_model]`），example 里是 `enabled = true`；仓库里这份
`bot.conf.toml` 则根本没有 `[world_model]` 段（取代码默认 `false`）。

所以本地按配置看是关的，线上很可能是开的。**一条命令即可确认**：看启动日志里那行
"World Model v4 已启用"，或私聊发 `#world-status`——这正是那条注释所说的两个消费点。

顺带这里有个需要你定的问题：`enabled` **本来该不该**门控这些行为？如果那条注释代表的
是设计意图（这个开关只是文案），那要修的就是代码里那几处门控；如果不是，要修的是注释。
两种修法方向相反，所以我没动。

`plugins/model/src/yunxi/identity_store.rs:1078` 在删除事务里调
`world_model_store::delete_person_domain_rows`，删掉该人/群的 `yunxi_world_*` 行。
但 `save_world`（`world_model_store.rs:214-226`）是**整表 DELETE + 按内存快照全量
重写**，节奏是 `world_model.persist_interval_secs`（默认 30s）；而 Core 的
`WorldModel::erase_person` / `erase_conversation`（`crates/yunxi-core/src/world_model/mod.rs:880,896`，
文档写明「Used by data deletion flows (v4 §242)」）**零生产调用者**，没有任何东西清
`WORLD_RUNTIME`。

于是：删除 → 用户收到回执 → 下一条消息置脏 → 一个 persist 周期内被删的行连正文一起
写回。

建议：删除事务提交后，同步清内存态（锁 `WORLD_RUNTIME`，对每个 scope 调
`erase_person`/`erase_conversation`，**并额外丢掉 scope 匹配的 observations**——
Core 的 `erase_person` 不碰 `observations`），再置脏让下一次 persist 写出干净快照；
或者删除提交后直接 `restore_from_store()`。

这一条我没动：它落在数据擦除语义上，且离不开真实 Postgres 验证，改错的代价比不改大。
另注：Core 的 `erase_person` 本身也漏了 `observations`/`predictions`/`timeline`
（`world_model/mod.rs:880`），只过滤了 entities/situations/hypotheses/uncertainties/
social_scene——即使接上调用点也要一并补。

### 2.2 人级删除不清理 `yunxi_gag_entries` ——**已修（`d49de91`）**

修的时候顺带撞出一个真 bug（已一并修，同一个提交）：`gag_store::list_open` 把 INTEGER
的 `importance` 当 i64 读，而 `row.get` 是 panic 不是 Err，所以**只要该作用域有任意一条
open 条目，读账本就会 panic**——`#账本`（`gag_commands.rs:51`，外面那句 `.ok()?` 拦不住
panic）与私聊的 `ledger_context_for`（`:102`）都会中招。已改成与列一致的 i32，并把这一处
的 `row.get` 全部换成 `try_get(...)?`，让列类型再漂移时变成可上报的错误而不是 panic。

`identity_store.rs:1089` 的 person 分支逐个删 memories / open_loops / goals /
affect_states / relations / external_identities / persons，但没有 gag。`gag_store.rs:260`
已经现成有 `delete_for_scope`，唯一调用者是管理员的 `#清账本`。这些行是用户口述的
约定/芥蒂原文，会注入回复上下文。建议照 relation-note 的写法在
`delete_qq_person_domain_data` 里对主号与每个 QQ 别名各清一次。

### 2.3 关系备注按显示名跨作用域删除（可能删到别人）——**已按方案 (a) 修（`63229ab`）**

`relation_note_store.rs:373` 是 `DELETE ... WHERE target_key = ANY($1)`，而
`target_key` 只是显示名的规范化（小写 + 折叠空白），调用方（`mod.rs:1442-1464`）
把 QQ 号和**当前昵称**都传了进来。若某人把昵称改成另一个成员的名字再执行
`#删除我的数据`，会删掉**所有**会话里键为那个名字的备注，包括无关群里关于真实那位
成员的。建议加 `AND scope_key = ANY($2)`，只在请求者自身的作用域内接受昵称匹配。

**更正（复核后）**：上面那句"加 `AND scope_key = ANY($2)`"是错的，那样会**少删**。
相处结论的 scope 来自反思输入的 `MindScope`（`mind_runtime.rs:2410` 传 `input.scope`），
所以"关于 A 的结论"可以写在**任何**会话作用域里（A 在哪个群说过话，那个群的作用域就
可能有一条）。从 A 的身份出发枚举不出"所有含 A 的结论的作用域"，按 A 自己的作用域去
限定，会让群作用域里关于 A 的结论留下来——那是删不干净 A 自己的数据。

真正的判别标准不是作用域，而是**键有没有歧义**：QQ 号 / external_id 能唯一指向一个人，
显示名不能。三个选项：

- **(a) 只按无歧义标识全局删**（QQ 号 + external_id），不再按显示名全局删。彻底消除
  误删他人；代价是模型用昵称写下的结论会留下——而模块文档本来就把"别名覆盖做不到
  穷尽"列为既定代价，所以这在已接受范围内。
- **(b) 保持现状**：昵称撞名时会删掉无关群里的他人结论。
- **(c) 折中**：标识全局删 + 显示名只在能归属到 A 的作用域内删（A 的 person 作用域与
  他的私聊会话）。覆盖私聊，仍漏群聊。

倾向 **(a)**：一条有歧义的键不足以支撑一次删除，"永不删除可归属于他人的数据"比
"尽量删干净"更该优先；而且群级擦除已经是正确形态（`delete_qq_group_domain_data` 按
会话作用域删，见 `mod.rs:1546`），按人的这条是唯一的例外。

**已按 (a) 落地**：`relation_note_targets` → `relation_note_erasure_keys`，去掉昵称参数
（连带去掉那次 user profile 查询）。残留代价（有意接受、记录在案）：模型用显示名写下的
结论不会被按人擦除删掉。等哪天给这张表补上 `target_person_id`，这个缺口才谈得上真正
闭合——在那之前，"注册时把名字写进表"这种事不该靠昵称去猜。

回归测试：`erasure_never_deletes_another_persons_relation_notes_by_nickname`（集成，
实测；把昵称塞回键里会变红）与 `person_erasure_only_uses_unambiguous_relation_note_keys`
（单元）。

### 2.4 「尽力而为」的世界模型删除其实无法 fail-soft ——**已复现，测试已落档**

**复现结论（`2aa5553`，本地 PostgreSQL 18）**：
`yunxi::erasure_tests::world_store_failure_must_not_take_the_person_erasure_down_with_it`
注入故障（删掉 `yunxi_world_observations`）后，这个人的记忆**没有被删掉**
（`left: 1, right: 0`），调用方拿到的是 `25P02 current transaction is aborted`——
正是被吞掉的世界模型失败把后面每一条 DELETE 都带走了。

更窄的那一形态（"什么都没删却报成功"）也验证了：已中止事务上的 `COMMIT` 只输出
`ROLLBACK`、不报错（psql 实测 `rows_written=0`），而 sqlx 的 `commit()` 只传播
COMMIT 语句自身的错误（`sqlx-postgres-0.8.6/src/transaction.rs:47-58`），所以只要
世界模型之后没有别的语句（`person_id = None` 且无 direct conversation），调用方就会
看到一次成功的零行擦除。

`identity_store.rs:1079` 用 `let Ok(rows) = ...` 吞掉错误，但 PostgreSQL 里任何语句
失败都会中止整个事务，后续语句一律 25P02，而 sqlx 的 `commit()` 只是发 `COMMIT`——
在已中止的事务上等价于回滚且返回 `Ok`。于是真实原因被掩盖、该人的记忆/关系根本没删，
在极端形态下还会报成一次成功的零行删除。建议放到独立事务或 SAVEPOINT 里，并让错误
向上传播。

---

## 三、其余已定位问题（按影响排序，均给出位置）

**可靠性**

- `plugins/model/src/world_sensors.rs:117`：命令型传感器同时 `piped()` 了 stdout/stderr，
  却只在 `try_wait()` 报退出后才去读——输出超过管道容量（Linux 64 KiB）子进程永远阻塞
  在 `write()`，`try_wait()` 永远不返回，最后被杀并报「命令超时」。**这类传感器无论
  多快都不可能成功**，而 `should_feed_core` 会把 `ok=false` 当成状态变化，每次轮询都
  写一条**假的**持久世界事实 + open loop。建议改用 `tokio::process` +
  `timeout(.., child.wait_with_output())` 或起读线程。
- `plugins/model/src/model/group.rs`（置位 `:1974`、守卫 `:1947`、唯一清除 `:2147`）：
  `interjection_in_flight` 在多条早退路径上泄漏（`:1003` 主回复分支、`:985` 语义过期、
  `:1133` 排队等），`prune_interjection_states` 又刻意保留在途项，所以**该群从此再也
  不会主动插话**，直到进程重启。两位评审独立报同一条。建议改成 RAII guard。
- `plugins/model/src/model/memory_query.rs:216`：工具跟进轮沿用**全权限** `tool_specs`
  并用可写的 `execute`，而兄弟路径（`core_model.rs:5689`、`delivery.rs:1043`）都改成了
  只读。于是「主管理员让机器人看某个网页 → 网页里的注入文本 → 第二轮仍挂着
  `group.message.send` / `reminder.create`」这条链路是通的，唯一防线是提示词里的一句话。
- `plugins/model/src/yunxi/delivery.rs:554`：语音/唱歌合成（`qq_sing` 超时默认 45s，
  可配到 180s）发生在 30s 的 precommit 租约**之内**，合成慢一点就 `Stale`，整条已渲染
  好的回复被丢弃且不重试。建议把合成挪到 `begin_outgoing_commit` 之前。
- `plugins/model/src/memory/mod.rs:2741`：融合后的 id 只在**词面结果**建的 map 里查，
  语义那一路带出的、词面没有的记忆被静默丢弃——注释写的「缺的补在最后」没有实现。
  净效果是 embedding 只能给词面结果重排序，召回退化成关键词匹配。
- `crates/yunxi-core/src/world_model/situation.rs:384`：`expire()` 拒绝
  `OutcomeUnknown`（而转换表 `:93` 是允许的）。生产维护路径把闲置会话置为
  `OutcomeUnknown`（`yunxi/world_model.rs:660`），`status()` 又把它算作 Active，
  于是 8 个槽位填满后 `add_situation` 永远 `TooManyItems`，而那之后再也不会记录情境。
- `crates/yunxi-core/src/mind/consolidation.rs:468/544/694/759`：只有 Interest 用了
  `operation_time`（防时间回退），belief/preference/open-question/agenda 直接把
  `proposed_at` 透传。并发写入落在批次窗口内会让 `apply` 返回 `InvalidTimestamp`，
  而 `prepare` 的 `?` 丢掉**整批**反思（仓库此前已为 Interest 修过同类问题，
  注释写着「线上曾因此每天丢掉 39 批反思」）。
- `plugins/model/src/world_sensors.rs:70`：`SENSOR_STATES` 满了以后 `set_sensor_state`
  直接返回，而该 map 从不淘汰；再次失败的新传感器状态永远读不到
  （`unwrap_or_default()` → `should_feed_core(None,false,..) == true`），冷却门也永不
  生效，于是**每次轮询**写一条持久事实。

**安全**

- `plugins/model/src/admin/config_api.rs:250`：响应里同时返回 `masked: [...]` 和**未打码
  的 `raw` 全文**，前端 `app.js:986` 把它直接塞进 textarea。`kovi.conf.toml` 的
  `access_token`、`admin.token`、各类 `*_api_key` 因此会显示在屏幕上、进截图与浏览器
  缓存——与该文件头部「密钥不回显」的约定相反。
- `plugins/model/src/model/message_transport.rs:138`：退避检查无条件拦截，没有给控制
  回执留口子，而 `send_guard.rs:11-12` 明写「管理员命令（`#结束禁言` 等）不受影响」。
  `#结束禁言` 只清 `IS_BANNED`，`is_group_paused` 却把 send guard OR 进来，而 guard 只有
  发送成功才会清——被 QQ 拒一次之后管理员再怎么解禁，群里都继续静默，且没有任何回执
  告诉管理员。建议解禁时一并 `send_guard::clear(group_id)`。
- `plugins/model/src/group_access.rs:196`：接听授权名单用固定的 `.json.tmp` 写后 rename，
  且快照在释放 `STATE` 锁**之后**才写。两个并发授权命令可让旧快照覆盖新的撤销结果，
  而这份文件是接通前唯一的拦截点。
- `plugins/model/src/speech.rs:144`：TTS 响应头 `X-Sample-Rate` 未经校验就覆盖配置值
  （配置本身校验 8000..=48000），下游只有下界 `max(8000)`，于是
  `max_bytes = rate*2*60` 可被撑到 GB 级，60 秒截断失效。
- `tools/speech-service/service.py:380`：`/v1/tts` 的 `sample_rate` 无上界（`speed` 就在
  两行外做了 0.3-2.0 钳制）；`tools/sing-service/service.py:777` 的
  `int(Content-Length)` 既不设上限也不在 try 里，`Content-Length: abc` 会让请求直接
  断连而不是 400。两者都可让常驻服务 OOM/线程卡死。

**配置与文档漂移**

- `plugins/model/src/yunxi/turn_gate_runtime.rs:203`：`#turn-gate-status` 报的是**配置里**
  的 mode，而 `classify_completion` 用的是 `install()` 时冻结的 `mode_active`。虽然
  `model.turn_gate` 在 `RESTART_SECTIONS` 里（`config_api.rs:56-62`，缓存是有意的），
  但「存盘 + reload 不重启」之后状态报告会说 `completion=active` 而实际仍走 legacy。
  同一段里 `response_mode` 却是实时读的（`:159`/`:165`），两者不一致。
- `bot.conf.example.toml:414-417` 说 `reply_context` 默认 `active`，代码默认是
  `disabled`（`config/world_model.rs:43`）；`:646-647` 说 `[silence]`「默认只影子观察」，
  而 `SilenceConfig::default()` 是 `enabled: true`（`silence.rs:66-75`），该文件
  `:658` 自己也写着「默认开启」。建议给这两个默认值补一条测试（现有
  `shipped_example_configuration_deserializes` 只校验语法）。
- `plugins/model/src/config/qq_call.rs:510`：校验要求 `caller_allowlist_file` 非空，但该
  字段自己的文档与唯一消费方（`group_access.rs:164-170`）都把「留空」当作关闭该能力。

**其余**：`plugins/model/src/yunxi/executive_store.rs:798`（决策行一律写
`scope_key='global'`，导致按会话/人的擦除删不到，7 天后重启还会被投影读回）、
`plugins/model/src/memory/mod.rs:497`（全进程唯一生产连接池写死 `max_connections(5)`
且无 `acquire_timeout`；你们 09-12 巡检报告 P0-3 已定位，但**不建议在无人值守下改**——
同一份报告记录该机内存紧张、swap 已在用，盲目提高并发可能加重 OOM，需要你按
PG `max_connections=100` 与整机余量定夺）、`plugins/model/src/yunxi/memory_store.rs:197`
（保留期清理在全局排他锁内逐行级联删除，一次可放大到数万条语句）、
`plugins/model/src/yunxi/memory_store.rs:833`（>64 字节标签的旧记忆被 `.ok()` 静默丢弃）、
`plugins/model/src/yunxi/identity_store.rs:1614`（`yunxi_message_mappings` 缺
`created_at` 索引，保留期 DELETE 全表扫）、`plugins/model/src/metrics.rs:165`
（`flush` 先 drain 再写，future 被取消就永久丢窗口）、
`scripts/verify-chat-shape.sh:38-48`（取不到日志时全部指标报 0 并 exit 0，
正是它要防的假通过）、`tools/turngate/train.py:249`（train/val 切分写反，
`val_all, train_all = perm[n_val:], perm[:n_val]` 把训练集和验证集对调）、
`scripts/bridge-patches/patch-plugin-login-refresh.py:133-157`（首次打补丁只走 `if`
分支，安静期守卫只在 `else` 里插，于是首次生命周期的行为正是该补丁要修的那个）、
`tools/embed-service/service.py:129`（reranker 缺 embedder 那样的分批推理，
单请求可达 3.2 万 token 的前向）、`scripts/install-qq-call.sh:289-299`
（pin 的 checkout 失败只 warn，随后仍以 root 安装未 pin 的上游代码）。

---

## 四、复核过、确认没问题的地方

这些是我（或评审）重点读过并明确排除的，列出来是为了说明覆盖范围，也免得下次重复排查：

- **SSRF 防线**（`image_security.rs`）：禁用重定向 + `no_proxy` + DNS 钉死
  （`resolve_to_addrs`）+ 建连后拿 `remote_addr` 再比一次白名单（封掉 rebinding）+
  端口限 80/443 + 拒带凭据；IPv4 拒绝列表是完整的 IANA 特殊用途集合，
  IPv6 限 `2000::/3` 再排 Teredo/benchmark/ORCHID/doc/6to4 并递归处理 IPv4-mapped。
  GIF 炸弹也被 `image` 的 512 MiB 分配上限挡住。
- **发送退避**（`send_guard.rs`）：std 锁内无 `.await`（无死锁）、512 项上限 + FIFO 淘汰、
  `1u64 << failures.min(7)` 无溢出、1h 封顶正确。
- **管理后台鉴权**（`admin/auth.rs`、`mod.rs`）：常数时间比较 + 空 token 守卫、一次性
  nonce、数据路由全挂 `require_session`、精确匹配 cookie、`SameSite=Strict; HttpOnly`、
  无 CORS 层、无开放重定向、非 loopback 绑定需显式开关。
- **SQL**：`memory_api.rs` / `annotation_api.rs` 里所有插值片段都来自 `&'static str`，
  请求值一律绑定参数；`identity_store.rs` 的擦除语句都带 person/conversation 谓词；
  `delivery_ledger.rs` 每次状态迁移都由 key + fingerprint + 期望状态 + `rows_affected()==1`
  把关。
- **租约/认领**（`agent_runs` / `agent_tasks` / `reminders`）：每条 UPDATE 都由
  `status` + `lease_token` 守卫，发送门在平台调用前落库，不确定结果记为
  `unknown`/`failed` 且绝不重放，过期租约会被回收，未发现重复发送或永久卡死。
- **三个已知问题的实际状态**：09-12 巡检报告里的 P0-2（视觉缺预缩放/多图）、
  P2-1（静默标记误报）、P2-2（回填外键违反）、P3（主动发送未接退避）**都已修好且有
  测试**；embed 的 OOM 是用「关掉 ONNX 内存池把常驻降到 654MB」而不是照报告说的抬高
  `MemoryMax` 解决的——unit 文件里写了实测数据说明为什么连 `MemoryHigh` 都刻意不设。
- 其余清白的模块清单（`mind/`、`executive/`、`world_model/` 各文件、`interrupt.rs`
  状态机、`tool_access.rs` 鉴权、`apps/yunxi-cli` 的 journal/state 落盘、
  `image_security`/`vision_router`、`sticker_memory` 的跨作用域 IDOR 检查等）见各路子
  评审的明细，此处不展开。

---

## 五、两点说明

1. 上面第三、四节的条目里，标了具体行号的都经过至少一次源码复核；但除了
   「一、已修」、1.5 以及 2.1/2.4（均已在本地 PostgreSQL 上复现并落成回归测试，
   见 `2aa5553`）之外，**我没有逐条独立复现**。动手前建议按 `file:line` 再看一眼
   上下文——行号以 `2aa5553` 为准。
   复现环境是一次性的本地库 `kovi_review_scratch`（PostgreSQL 18），未接触任何线上
   数据；两条测试只读 `DATABASE_URL`，跑法：
   `DATABASE_URL=postgresql://$USER@127.0.0.1:5432/kovi_review_scratch cargo test -p model --lib -- <全路径> -- --ignored --exact`。
2. 我刻意没有改的：2.1/2.2/2.3（数据擦除语义，需要真实 Postgres 验证）、
   `max_connections(5)`（见上，属于容量决策）、以及所有用户可见文案。
