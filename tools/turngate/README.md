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
- 待接线（Phase 3/4）：response head shadow → active、ConversationState
  联动、私聊/群聊统一走 response 决策。
