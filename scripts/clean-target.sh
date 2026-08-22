#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

cargo clean
printf '%s\n' "target/ 已清理。后续构建不会启用增量编译。"
