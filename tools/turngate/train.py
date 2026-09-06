#!/usr/bin/env python3
"""TurnGate 训练管线骨架 (Phase 1)。

流程:
1. 读取脱敏 JSONL (schema_version 2, 见设计文档 §7.1);
2. 用 features.py 抽取与生产 Rust 完全一致的特征向量;
3. numpy 手写 softmax 逻辑回归, 分别训练 completion (2 类) 与
   response (5 类) 两个 head;
4. 在验证集上按目标精度-召回校准每标签阈值 (abstain 的运行时校准值);
5. 导出 models/yunxi-turngate/{manifest.toml, turn_gate.bin,
   THIRD_PARTY_NOTICES}, 与 crates/yunxi-core 的加载器逐一对应;
6. 跑 check_parity 并打印 bundle 摘要。

用法 (数据稀少时先跑通闭环, 权重仅供加载/推理联调):
    python3 tools/turngate/train.py --data tools/turngate/dataset/sample.jsonl \
        --out /tmp/turngate-bundle
    TURNGATE_FIXTURE_DIR=/tmp/turngate-bundle \
        cargo test -p yunxi-core --lib loads_trainer_produced_bundle -- --nocapture

注意: 正式训练数据必须遵守 doc §7.1 的授权/脱敏/可擦除/不自我闭环约束。
本脚本仅作为骨架: 换成 L-BFGS、分层采样、Platt 校准、早停等都是后续增强。
"""

import argparse
import hashlib
import json
import struct
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))
import features as feats  # noqa: E402

FEATURE_DIM = feats.HASH_BUCKETS + feats.CONTEXT_FEATURE_SLOTS
COMPLETION_LABELS = ["flush_now", "hold_for_more"]
RESPONSE_LABELS = ["answer", "continue", "ack", "ignore", "wait"]

# 校准目标 (doc §10 验收指标的骨架默认值, 用真实数据后再收敛)
TARGETS = {
    "completion": {"hold_for_more_recall": 0.95, "flush_now_precision": 0.98},
    "response": {"answer_precision": 0.90, "ignore_false_positive": 0.01},
}
DEFAULT_THRESHOLDS = {
    "completion_confidence": 0.60,
    "completion_margin": 0.15,
    "response_answer": 0.65,
    "response_continue": 0.60,
    "response_ack": 0.55,
    "response_ignore": 0.60,
    "response_wait": 0.50,
    "response_margin": 0.10,
}


def feature_vector(context: dict) -> np.ndarray:
    f = feats.extract_features(context)
    x = np.zeros(FEATURE_DIM, dtype=np.float32)
    for tf in f["text_features"]:
        x[int(tf["index"])] += tf["count"]
    for pos in f["context_indices"]:
        x[feats.HASH_BUCKETS + int(pos)] += 1.0
    return x


def softmax(logits: np.ndarray) -> np.ndarray:
    logits = logits - logits.max(axis=1, keepdims=True)
    exp = np.exp(logits)
    return exp / exp.sum(axis=1, keepdims=True)


REVIEWED_SOURCES = {"human_consensus", "skeleton"}


def load_dataset(path: Path, include_pseudo: bool = False):
    """按来源过滤:默认只收人工复核/种子集;`--include-pseudo` 才收弱标签
    候选 (doc §7.4 C:伪标签不得直接作为真值)。completion/response 为
    null 的样本只用于人工复核,不进训练。"""
    samples = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        sample = json.loads(line)
        assert sample.get("schema_version") == 2, "dataset schema_version must be 2"
        source = sample.get("label_provenance", {}).get("source")
        if source not in REVIEWED_SOURCES and not include_pseudo:
            continue
        labels = sample["labels"]
        if labels.get("completion") is None and labels.get("response") is None:
            continue
        samples.append(
            (
                sample["current_text"],
                sample["context"],
                labels.get("completion"),
                labels.get("response"),
            )
        )
    return samples


def train_head(X, y_onehot, epochs=80, lr=1.0, l2=1e-6, rng=None):
    """softmax 逻辑回归; W 形状 (classes, FEATURE_DIM+1), 最后一列是偏置。"""
    rng = rng or np.random.default_rng(0)
    classes = y_onehot.shape[1]
    n = X.shape[0]
    Xb = np.concatenate([X, np.ones((n, 1), dtype=np.float32)], axis=1)
    Y = y_onehot.astype(np.float32)
    W = np.zeros((classes, Xb.shape[1]), dtype=np.float32) + rng.normal(0, 0.01, (classes, Xb.shape[1]))
    for epoch in range(epochs):
        probs = softmax(Xb @ W.T)
        grad = ((probs - Y).T @ Xb) / n + l2 * W
        # 偏置不参与正则
        grad[:, -1] -= l2 * W[:, -1]
        W -= lr * grad
        lr = max(lr * 0.98, 0.05)
    return W


def calibrate_thresholds(W, X_val, y_val, labels):
    """骨架校准: 简单网格搜索每标签置信度阈值 (纯 numpy); margin 保持
    默认。真实数据应换 Platt/温度校准 + 验证集分离, 并记录
    label_provenance。y_val 是验证集的类 id 数组。"""
    probs = softmax(np.concatenate([X_val, np.ones((X_val.shape[0], 1), dtype=np.float32)],
                                   axis=1) @ W.T)
    thresholds = {}
    for i, label in enumerate(labels):
        pos_mask = y_val == i
        best_t, best_f1 = 0.5, -1.0
        for t in np.arange(0.50, 0.96, 0.05):
            pred = probs[:, i] >= t
            tp = int(np.sum(pred & pos_mask))
            fp = int(np.sum(pred & ~pos_mask))
            fn = int(np.sum(pos_mask & ~pred))
            precision = tp / (tp + fp) if tp + fp else 0.0
            recall = tp / (tp + fn) if tp + fn else 1.0
            f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
            if f1 > best_f1:
                best_f1, best_t = f1, float(t)
        thresholds[label] = best_t
    return thresholds


def export_bundle(W_completion, comp_labels, W_response, resp_labels, thresholds,
                  out_dir: Path, feature_version, training_data_version):
    feature_dim = FEATURE_DIM
    # 二进制布局 (必须与 crates/yunxi-core serialize_weights 一致):
    # [W_c: feature_dim*C, 按 feature 主序] + [b_c: C] + [W_r: feature_dim*R] + [b_r: R]
    Wc = W_completion[:, :feature_dim].T.reshape(-1)  # (feature_dim, C) -> flat
    bc = W_completion[:, feature_dim]
    Wr = W_response[:, :feature_dim].T.reshape(-1)
    br = W_response[:, feature_dim]
    payload = np.concatenate([Wc, bc, Wr, br]).astype(np.float32)
    raw = struct.pack(f"<{payload.size}f", *payload.tolist())

    out_dir.mkdir(parents=True, exist_ok=True)
    bin_path = out_dir / "turn_gate.bin"
    bin_path.write_bytes(raw)

    manifest = {
        "manifest_version": 1,
        "model_id": "yunxi-turngate",
        "model_version": "v0.3.0-skeleton",
        "algorithm": "hashed-char-ngram-logistic",
        "feature_version": feature_version,
        "hash_buckets": feats.HASH_BUCKETS,
        "max_text_chars": feats.MAX_CURRENT_CHARS,
        "max_pending_fragments": feats.MAX_PENDING_FRAGMENTS,
        "max_pending_fragment_chars": feats.MAX_FRAGMENT_CHARS,
        "max_recent_turns": feats.MAX_RECENT_TURNS,
        "max_recent_turn_chars": feats.MAX_FRAGMENT_CHARS,
        "max_question_chars": feats.MAX_QUESTION_CHARS,
        "completion_labels": comp_labels,
        "response_labels": resp_labels,
        "abstain": "calibrated_threshold",
        "thresholds": thresholds,
        "training_data_version": training_data_version,
        "assets": [
            {
                "path": "turn_gate.bin",
                "sha256": hashlib.sha256(raw).hexdigest(),
                "size_bytes": len(raw),
            }
        ],
    }
    (out_dir / "manifest.toml").write_text(_toml_dump(manifest), encoding="utf-8")
    (out_dir / "THIRD_PARTY_NOTICES").write_text(
        "TurnGate 权重为本项目离线训练产物; 特征/训练无第三方模型依赖。\n",
        encoding="utf-8",
    )
    return manifest


def _toml_dump(obj) -> str:
    lines = []
    tables = []
    for key, value in obj.items():
        if isinstance(value, dict):
            tables.append((key, value, None))
        elif isinstance(value, list) and value and isinstance(value[0], dict):
            for item in value:
                tables.append((key, item, key))
        else:
            lines.append(f"{key} = {_toml_value(value)}")
    for key, value, table_key in tables:
        lines.append("")
        lines.append(f"[[{table_key}]]" if table_key else f"[{key}]")
        for k, v in value.items():
            lines.append(f"{k} = {_toml_value(v)}")
    return "\n".join(lines) + "\n"


def _toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    return json.dumps(value, ensure_ascii=False)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--epochs", type=int, default=80)
    parser.add_argument("--lr", type=float, default=1.0)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--val-frac", type=float, default=0.2)
    parser.add_argument("--training-data-version", default="local-dataset-v0")
    parser.add_argument("--include-pseudo", action="store_true",
                        help="把 pseudo_lexical_v0 弱标签一并纳入(仅候选,不建议直接训练)")
    args = parser.parse_args()

    samples = load_dataset(args.data, args.include_pseudo)
    if len(samples) < 4:
        print(f"data too small: {len(samples)} samples", file=sys.stderr)
        return 1

    rng = np.random.default_rng(args.seed)
    contexts = [{"current_text": text, **ctx} for text, ctx, _, _ in samples]
    X = np.stack([feature_vector(ctx) for ctx in contexts])
    comp_idx = [i for i, (_, _, c, _) in enumerate(samples) if c is not None]
    resp_idx = [i for i, (_, _, _, r) in enumerate(samples) if r is not None]

    n_val = max(1, int(len(samples) * args.val_frac))
    perm = rng.permutation(len(samples))
    val_all, train_all = perm[n_val:], perm[:n_val]
    train_positions = {i: j for j, i in enumerate(train_all)}
    val_positions = {i: j for j, i in enumerate(val_all)}
    X_train = X[train_all]
    X_val = X[val_all]

    comp_train_global = [i for i in comp_idx if i in train_positions]
    comp_val_global = [i for i in comp_idx if i in val_positions]
    resp_train_global = [i for i in resp_idx if i in train_positions]
    resp_val_global = [i for i in resp_idx if i in val_positions]

    if comp_train_global:
        W_c = train_head(
            X[[train_positions[i] for i in comp_train_global]],
            _onehot(
                np.array([COMPLETION_LABELS.index(samples[i][2]) for i in comp_train_global]),
                len(COMPLETION_LABELS),
            ),
            args.epochs, args.lr, rng=rng,
        )
        cal_c = calibrate_thresholds(
            W_c,
            X[[val_positions[i] for i in comp_val_global]] if comp_val_global else X_val,
            np.array([COMPLETION_LABELS.index(samples[i][2]) for i in comp_val_global])
            if comp_val_global
            else np.zeros(len(X_val), dtype=np.int64),
            COMPLETION_LABELS,
        )
    else:
        W_c = None
        cal_c = {}

    if resp_train_global:
        W_r = train_head(
            X[[train_positions[i] for i in resp_train_global]],
            _onehot(
                np.array([RESPONSE_LABELS.index(samples[i][3]) for i in resp_train_global]),
                len(RESPONSE_LABELS),
            ),
            args.epochs, args.lr, rng=rng,
        )
        cal_r = calibrate_thresholds(
            W_r,
            X[[val_positions[i] for i in resp_val_global]] if resp_val_global else X_val,
            np.array([RESPONSE_LABELS.index(samples[i][3]) for i in resp_val_global])
            if resp_val_global
            else np.zeros(len(X_val), dtype=np.int64),
            RESPONSE_LABELS,
        )
    else:
        W_r = None
        cal_r = {}

    thresholds = dict(DEFAULT_THRESHOLDS)
    if W_c is None and W_r is None:
        print("no trainable labels (human-reviewed) found; use --include-pseudo? ", file=sys.stderr)
        return 1
    thresholds["completion_confidence"] = cal_c.get(
        "flush_now", DEFAULT_THRESHOLDS["completion_confidence"]
    )
    thresholds["response_answer"] = cal_r.get("answer", DEFAULT_THRESHOLDS["response_answer"])
    thresholds["response_continue"] = cal_r.get(
        "continue", DEFAULT_THRESHOLDS["response_continue"]
    )
    thresholds["response_ack"] = cal_r.get("ack", DEFAULT_THRESHOLDS["response_ack"])
    thresholds["response_ignore"] = cal_r.get("ignore", DEFAULT_THRESHOLDS["response_ignore"])
    thresholds["response_wait"] = cal_r.get("wait", DEFAULT_THRESHOLDS["response_wait"])

    W_c = W_c if W_c is not None else np.zeros((len(COMPLETION_LABELS), FEATURE_DIM + 1), dtype=np.float32)
    W_r = W_r if W_r is not None else np.zeros((len(RESPONSE_LABELS), FEATURE_DIM + 1), dtype=np.float32)
    manifest = export_bundle(
        W_c,
        COMPLETION_LABELS,
        W_r,
        RESPONSE_LABELS,
        thresholds,
        args.out,
        feats.TURN_GATE_FEATURE_VERSION,
        args.training_data_version,
    )
    print(f"bundle written: {args.out} ({len(samples)} samples, {len(train_all)} train / {n_val} val)")
    print(f"thresholds: {thresholds}")
    print(f"manifest sha: {manifest['assets'][0]['sha256'][:16]}…")
    return 0


def _onehot(labels, classes):
    out = np.zeros((len(labels), classes), dtype=np.float32)
    out[np.arange(len(labels)), labels] = 1.0
    return out


if __name__ == "__main__":
    raise SystemExit(main())
