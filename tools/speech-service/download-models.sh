#!/usr/bin/env bash
#
# 下载本机语音服务需要的中文模型。
#
# 默认装两组纯 CPU 可跑的模型：
#   识别：SenseVoice int8（中英日韩粤，非自回归，CPU 上很快）
#   合成：sherpa-onnx-vits-zh-ll（中文 VITS，5 个音色，自带分词词典与文本正则）
#
# --with-matcha 会额外下一组更快的合成模型（matcha-icefall-zh-baker +
# vocos 声码器），用 --tts-kind matcha 切换。
#
set -euo pipefail

DEST_DIR="${SPEECH_MODELS_DIR:-$HOME/.local/share/yunxi-speech/models}"
BASE_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download"

ASR_NAME="sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17"
TTS_NAME="sherpa-onnx-vits-zh-ll"
MATCHA_NAME="matcha-icefall-zh-baker"
VOCODER_NAME="vocos-22khz-univ.onnx"

WITH_MATCHA=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --with-matcha) WITH_MATCHA=1; shift ;;
        --dest) DEST_DIR=${2:?missing value for --dest}; shift 2 ;;
        -h|--help)
            printf 'usage: %s [--with-matcha] [--dest DIR]\n' "$0"
            exit 0
            ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

info() { printf '[info] %s\n' "$*"; }
die() {
    printf '[fail] %s\n' "$*" >&2
    exit 1
}

for command in curl tar; do
    command -v "$command" >/dev/null 2>&1 || die "缺少必需命令: $command"
done

install -d -m 0755 "$DEST_DIR"

fetch_archive() {
    local url=$1 name=$2
    if [[ -d "$DEST_DIR/$name" ]]; then
        info "$name 已存在，跳过"
        return 0
    fi
    info "下载 $name"
    curl -fL --retry 3 --retry-delay 2 -o "$DEST_DIR/$name.tar.bz2" "$url" \
        || die "下载失败: $url"
    info "解压 $name"
    tar -xjf "$DEST_DIR/$name.tar.bz2" -C "$DEST_DIR"
    rm -f -- "$DEST_DIR/$name.tar.bz2"
}

fetch_file() {
    local url=$1 name=$2
    if [[ -f "$DEST_DIR/$name" ]]; then
        info "$name 已存在，跳过"
        return 0
    fi
    info "下载 $name"
    curl -fL --retry 3 --retry-delay 2 -o "$DEST_DIR/$name" "$url" || die "下载失败: $url"
}

fetch_archive "$BASE_URL/asr-models/${ASR_NAME}.tar.bz2" "$ASR_NAME"
fetch_archive "$BASE_URL/tts-models/${TTS_NAME}.tar.bz2" "$TTS_NAME"

asr_dir="$DEST_DIR/$ASR_NAME"
tts_dir="$DEST_DIR/$TTS_NAME"
[[ -d $asr_dir ]] || die "识别模型目录缺失: $asr_dir"
[[ -d $tts_dir ]] || die "合成模型目录缺失: $tts_dir"
[[ -f "$tts_dir/model.onnx" ]] || die "合成模型缺少 model.onnx"
[[ -f "$tts_dir/lexicon.txt" ]] || die "合成模型缺少 lexicon.txt"

if [[ $WITH_MATCHA -eq 1 ]]; then
    fetch_archive "$BASE_URL/tts-models/${MATCHA_NAME}.tar.bz2" "$MATCHA_NAME"
    matcha_dir="$DEST_DIR/$MATCHA_NAME"
    fetch_file "$BASE_URL/vocoder-models/${VOCODER_NAME}" "${MATCHA_NAME}/${VOCODER_NAME}"
fi

cat <<EOF

模型就绪。

识别模型目录: $asr_dir
合成模型目录: $tts_dir
EOF

if [[ $WITH_MATCHA -eq 1 ]]; then
    cat <<EOF
低延迟合成目录: $DEST_DIR/$MATCHA_NAME
  （该目录使用 Baker 数据集，上游标注为仅限非商业用途；切换前请自行确认）
EOF
fi

cat <<EOF

先在前台跑一次，确认能起来：

  python3 tools/speech-service/service.py \\
    --asr-dir "$asr_dir" \\
    --tts-dir "$tts_dir" \\
    --port 6120

自检：

  curl -s http://127.0.0.1:6120/healthz
  curl -s -X POST http://127.0.0.1:6120/v1/tts \\
    -H 'Content-Type: application/json' \\
    -d '{"text":"你好呀，我在的。"}' --output /tmp/tts.pcm
  ffplay -f s16le -ar 16000 -ac 1 /tmp/tts.pcm

音色：vits-zh-ll 有 0-4 共 5 个音色，用 --tts-speaker 选，先各听一遍再定。

切到低延迟合成（可选）：

  python3 tools/speech-service/service.py --asr-dir "$asr_dir" \\
    --tts-dir "$DEST_DIR/$MATCHA_NAME" --tts-kind matcha --port 6120
EOF
