# TurnGate 工具

TurnGate 特征抽取的 Python 参考实现与离线训练管线（对齐
`crates/yunxi-core/src/model/turn_gate.rs`，设计文档
`docs/yunxi-turngate-design.md` v0.2）。供离线训练器直接 import，保证与
生产 Rust runtime 的特征完全一致。

## 文件

- `features.py`：特征参考实现（FNV-1a + 字符 2..=5-gram + 固定位置结构化
  特征，含 `CONTEXT_FEATURE_SLOTS=24` 与协议版本常量）。
- `check_parity.py`：对内置样例计算特征向量并与锁定摘要比对；CI 可调用
  （Rust 侧 golden 测试是权威校验）。
- `train.py`：训练管线骨架（Phase 1）。
- `dataset/sample.jsonl`：31 条脱敏样例（schema_version 2，含半句/完整/
  群聊逗玩/接续/任务等待等形态），用于闭环联调，**不是**真实训练数据。

## Phase 0/1 用法

```bash
# 特征与 golden 向量
python3 tools/turngate/features.py
python3 tools/turngate/check_parity.py

# 训练并导出 bundle（骨架：numpy softmax 逻辑回归 + 网格阈值校准）
python3 tools/turngate/train.py \
    --data tools/turngate/dataset/sample.jsonl \
    --out /tmp/turngate-bundle

# 端到端：Rust 加载器读训练器产出的 bundle
TURNGATE_FIXTURE_DIR=/tmp/turngate-bundle \
    cargo test -p yunxi-core --lib loads_trainer_produced_bundle -- --nocapture

# Rust 侧全量
cargo test -p yunxi-core --lib turn_gate
```

Bundle 布局（`models/yunxi-turngate/`，生产由部署方放到稳定目录）：

- `manifest.toml`：协议版本/特征版本/桶数/标签集/abstain 策略/校准阈值/
  assets（`turn_gate.bin` 的 size + sha256）。
- `turn_gate.bin`：LE f32，`[W_c (feature_dim×C)][b_c (C)][W_r (feature_dim×R)][b_r (R)]`，
  feature_dim = 65536 + 24；维度与 manifest 相互校验，加载失败 fail-soft。
- `THIRD_PARTY_NOTICES`：仅本地训练产物，无第三方模型依赖。

## 线上采集与人工复核闭环 (doc §7.4)

```bash
# 1) 导出日志并采集候选(在服务器上运行,数据不出机器)
#    必须带 syslog 时间戳：`[send]` 行本身没有时间戳，用 `-o cat` 导出会让所有
#    机器人发言被记成"现在"，采出的批次 assistant turns 与 conversation_active
#    全为 0（collector 现在会直接报错拦下这种批次）。
#    也不要只 grep `[group`/`[send]`：目标判定还要读运行时打印的
#    「群聊消息指向其他成员，仅观察不回复」标记行。
journalctl -u kovi-bot.service -o short-iso --since "2026-09-07 00:00:00" \
    > /tmp/tg-journal.txt
python3 tools/turngate/collector.py --journal /tmp/tg-journal.txt \
    --out datasets/review-batch-$(date +%Y%m%d).jsonl
# 输出: pending 候选(schema v2), 弱标签 source=pseudo_lexical_v0,
#       脱敏(URL/长数字), 不透明 source_key(删除屏障用)

# 2) 人工复核(≥500 条/周 达成后产出第一版 v0.1 训练集)
#    先排"标注价值"的队：只用不依赖策略的客观信号（弱标签有没有判断、上下文
#    够不够、标签稀不稀有）。**不要**拿"线上实际回了没有"来排序或当标签——那等于
#    把现有的概率/冷却/时间窗策略抄进权重（doc §7.4 E）。
python3 tools/turngate/review.py --batch review-batch-*.jsonl --queue
python3 tools/turngate/review.py --batch review-batch-*.jsonl --queue --slice 300:40  # 第 301-340 条
python3 tools/turngate/review.py --batch review-batch-*.jsonl --show 1221
python3 tools/turngate/review.py --batch review-batch-*.jsonl --status
python3 tools/turngate/review.py --batch review-batch-*.jsonl --mark 1221 completion=flush_now response=answer
python3 tools/turngate/review.py --batch review-batch-*.jsonl --export train_turngate-v0.1.jsonl --min-agreement 0.9
# 队列 tier：0/1 灰区（lexical 给不出判断，信息量最大）、2 无上下文、
# 3 无机器人发言、4 其余；`--queue` 头部会打印上下文覆盖率，覆盖率低说明
# 导出格式或时间窗有问题，先回去查第 1 步。
# tier 是"标注价值"，**越小越先标**，不是等级或质量：队列按它升序排，所以
# `--queue` 与网页端的首屏必然全是 tier 0。网页端会把整批的 tier 分布和每档
# 含义显示在队列上方（一眼能看出还剩多少灰区）；它一次最多列 500 条待标样本，
# 但**已标注的样本会离开队列**，所以标完前面的，后面的 tier 会自己浮上来，
# 不存在"翻不到"的档。

#    也可以走网页端：管理后台的「标注」页用同一套队列与标注语义，鼠标点或
#    快捷键打标，标完直接下载导出的训练集（导出的字段与 --export 逐条一致）。
#    批次目录是 admin.annotation_dir（默认运行时目录下的 turngate/，生产上
#    就是 /home/ubuntu/kovi-bot/runtime/turngate/——current/ 只读，写不进去），
#    后台启动时会自动建好，直接把 review-batch-*.jsonl 放进去即可。两个入口
#    读写**同一份文件**，所以同一时刻只用一个：文件被别处改动时网页返回 409，
#    刷新后重来。
#    采于「@ 判定」修复前的批次里 addressed_to_agent 分不出"@ 她"还是"@ 别人"，
#    网页默认把这类样本排除在队列外（开关可放回来），等重采后再标它们。

# 3) 训练(默认仅人工复核/种子集; --include-pseudo 只做候选对比)
python3 tools/turngate/train.py --data train_turngate-v0.1.jsonl \
    --out models/yunxi-turngate --training-data-version local-dataset-v2
# → 校验: TURNGATE_FIXTURE_DIR=... cargo test -p yunxi-core --lib loads_trainer_produced_bundle

# 4) 部署 bundle(稳定目录,校验 manifest)+ shadow 观察 Phase 3 指标
#    (私有 #turn-gate-status 查看 FP/FN),再切 completion active。
```

采集纪律 (doc §7.4 D): 默认不采集;原文只存待标注区;训练集不含
QQ 号/昵称/URL;删除请求按 source_key 从未训练样本移除;模型更新=新版本+
重新评估。

### 目标判定（`context.targeting`）

日志里的 `[at]`/`[reply]` 只说明"这条消息有指向"，**不说明指向谁**：kovi 的
`Message::to_human_string` 对任何人的 @ 都渲染成 `[at]`，函数注释还明确写着
"不要靠此函数做判断"。采集器最初直接把 `[at]` 当成"在叫她"，于是"@了别人"的
消息被标成 `addressed_to_agent=true` / `must_reply`——拿它训练等于教 TurnGate
"别人被 @ 时该回她"，正好把线上"过度接话"的毛病固化进模型。

现在：

- 运行时判定"指的是别人"时会打印
  `[INFO] 群聊消息指向其他成员，仅观察不回复 (群组: N, 用户: M)`（Host 与
  Core 两条链路同一文案）；采集器据此记 `targeting = "other_member"`，
  `addressed_to_agent` 保持 false；
- 这条标记是**只在"不是叫她"时才出现的否定信号**：两处判定都带
  `!addressed_to_bot` / `!addressed_to_agent`（`group.rs` / `bridge.rs`）。
  所以带 at/reply 段、却没命中标记、且**该群在整份日志里出现过标记**的消息，
  反推为 `targeting = "her"`（在叫她 / 回她），`addressed_to_agent` /
  `replies_to_agent` / `must_reply` 照实为真，摘要里打印
  `inferred N samples as addressed to her`；
- 一次标记都没出现过的群仍然**丢弃并计数**（`dropped N ... unresolved`）：那种
  日志可能来自旧版本，或者这条消息根本没走到判定点（群未授权、"等她发图"这类
  早退分支），没有证据就不猜。
- 因此 `addressed_to_agent` / `replies_to_agent` 不再会从"文本里有 `[at]`"
  直接推断为真，但也不会因为"目标分不出来"而把真正的点名样本一起扔掉——
  早先那版"一律丢弃"会让整批样本里 `addressed_to_agent=true` 的数量变成 0，
  而 response head 恰恰需要"被点名该回"的正样本（实测 3 天日志：263 条被
  误丢，占候选的 ~12%）。

`targeting` 只是复核用元数据，**不进特征向量**：特征协议是锁定的（见下方"修改
约定"），要把它变成特征必须先走 Rust 与 Python 双侧的版本升级流程。
注意 `addressed_to_agent` / `replies_to_agent` / `policy_override` **是**结构化
特征（`features.py`），所以上面这条反推的准确度直接进权重——它靠的是"标记只否定
不肯定"这个代码事实，改动任一侧的标记判定都要同步回来看这里。

### 正文续行（导出格式的坑）

`[group...]` 消息行是"当前句"的唯一来源，采集器还会把**不带 syslog 前缀**的
后续行当成同一条消息的续行（journald 里带内嵌换行的正文，journalctl 不打前缀）。
带前缀的行是独立日志记录，绝不能粘进正文——早先少了这条判断，`-o short-iso`
整份导出时每一条 `INFO`/`Yunxi Mind`/`YUNXI_WORLD` 行都会被当成续行粘上去，
实测 2000 条样本里 94% 的正文被污染，弱标签可用率也从 775 掉到 517。

## 修改约定

任一特征协议改动（归一化、n-gram、边界标记、桶数、结构化槽位）必须：

1. 同步修改 Rust 端 `crates/yunxi-core/src/model/turn_gate.rs`，并提升
   `TURN_GATE_FEATURE_VERSION`（manifest 校验会拒绝旧 bundle）；
2. `python3 tools/turngate/features.py` 输出新向量 → 更新
   `check_parity.py` 中的 `GOLDEN_DIGEST`；
3. 把新向量写回 Rust 侧 golden 测试；
4. `cargo test -p yunxi-core --lib golden_vector` + `check_parity.py` 双绿。

训练数据格式与标注定义见设计文档 §7（schema_version 2、脱敏 JSONL、
label_provenance 人工复核、授权/可擦除/不自我闭环）。

## 训练管线现状（骨架）

- 逻辑：softmax 逻辑回归（numpy，纯手写梯度），completion/response 两
  head 独立训练；验证集按 F1 网格搜索每标签置信度阈值；导出
  manifest+bin+通知文件。
- 已达成：sh 端到端（train.py → bundle → Rust 加载 → 推理）、特征与
  Rust 逐项一致、大小/SHA/有限值校验、失败不阻断。
- 待数据增强：真实标注数据（详见 doc §7.4 采集闭环 + 人工复核）、
  分层采样、Platt/温度校准、提前停止、伪标签仅作待复核来源。
- Phase 2（已完成接线）：`[model.turn_gate]` active/shadow/disabled；
  group/private 的 coalesce 入口走 `push_with_turn_gate`——TurnGate
  高置信度决策时直接决定 flush/hold（MiniMind 不再被调用），abstain 或
  无 bundle 时惰性回退现有 lexical + MiniMind 路径（**生产零影响直到
  放入第一个 bundle**），shadow 模式记录与现有路径的分歧。
- Phase 3（已完成接线）：response head 进入 shadow——批次成型时跑
  response head 并配对真实走向（OutcomeGuard），只记账
  （FP 误接/ FN 漏接/一致），`[model.turn_gate].response_mode`
  默认 shadow，**不改变任何路由**；bundle 缺失时整个影子零运行。
- Phase 4（已接线、默认休眠）：`response_mode="active"` 且 bundle 就绪
  时,response head 的 Ignore/Wait 在群(未点名)与私聊入口抑制可见回复
  (Abstain 不写入字段走原管线;被点名/视觉/教学/命令不受影响,
  continue 仍走 ConversationState)。生产默认 shadow,未放置 bundle 时
  门控零运行。
- Phase 5(待办):真实数据校准 → 先私聊灰度 active,再群聊未点名
  answer 高精度阈值 + 预算;新模型版本绑定 manifest 与评估。
