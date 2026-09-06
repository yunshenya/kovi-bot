#!/usr/bin/env python3
"""TurnGate 特征一致性校验。

计算内置样例的特征向量并与其规范摘要比对。该摘要与 Rust 侧 golden 测试
(crates/yunxi-core/src/model/turn_gate.rs::golden_vector_matches_python_reference_extractor)
锁定的是同一组特征值;训练器在改动特征抽取后,必须同时更新本文件中的
GOLDEN_DIGEST 与 Rust 侧的 golden 测试(先由本脚本输出新摘要,再把新向量
写回 Rust 测试,最后跑 `cargo test -p yunxi-core --lib golden_vector`)。

Phase 0 验收:Rust 与离线训练器对相同样本得到完全一致的非零特征索引。
"""

import hashlib
import json
import sys

import features

GOLDEN_DIGEST = "85903f0c2884ab3b411a99de96aacda540993b1cca2d4296c2905ca83d1eabd9"


def canonical_digest(sample: dict) -> str:
    extracted = features.extract_features(sample)
    canonical = json.dumps(extracted, ensure_ascii=False, sort_keys=True)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def main() -> int:
    actual = canonical_digest(features.SAMPLE)
    if actual != GOLDEN_DIGEST:
        print(f"PARITY FAIL: digest {actual} != {GOLDEN_DIGEST}", file=sys.stderr)
        print("特征抽取已漂移:先更新 tools/turngate/features.py 与 Rust 端后再重新锁定", file=sys.stderr)
        return 1
    print("PARITY OK: feature extraction matches the locked golden digest")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
