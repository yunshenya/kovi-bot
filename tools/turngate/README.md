# TurnGate 工具

TurnGate 特征抽取的 Python 参考实现（对齐
`crates/yunxi-core/src/model/turn_gate.rs`，设计文档
`docs/yunxi-turngate-design.md` v0.2）。供离线训练器直接 import，保证与
生产 Rust runtime 的特征完全一致。

## 文件

- `features.py`：特征参考实现（FNV-1a + 字符 2..=5-gram + 固定位置结构化
  特征），内置文档 §7.1 风格样例。
- `check_parity.py`：对内置样例计算特征向量，并与锁定摘要比对；CI 可调
  用（Rust 侧 golden 测试是权威校验）。

## 用法

```bash
python3 tools/turngate/features.py            # 打印内置样例特征向量
python3 tools/turngate/check_parity.py        # 校验与锁定摘要一致
```

## 修改约定

任一特征协议改动（归一化、n-gram、边界标记、桶数、结构化位置）必须：

1. 同步修改 Rust 端 `crates/yunxi-core/src/model/turn_gate.rs`，并提升
   `TURN_GATE_FEATURE_VERSION`（manifest 校验会拒绝旧 bundle）；
2. `python3 tools/turngate/features.py` 输出新向量 → 更新
   `check_parity.py` 中的 `GOLDEN_DIGEST`；
3. 把新向量写回 Rust 侧 golden 测试；
4. `cargo test -p yunxi-core --lib golden_vector` + `check_parity.py` 双绿。

训练数据格式与标注定义见设计文档 §7（schema_version 2、脱敏 JSONL、
label_provenance 人工复核）。
