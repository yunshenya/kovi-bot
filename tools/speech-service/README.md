# 本机语音服务（ASR + TTS）

芸汐打 QQ 语音电话时，需要把对方的声音转成文字、再把她的话合成成声音。
这两件事都由这个常驻在**部署服务器本机**的小服务完成，只监听 `127.0.0.1`。

## 为什么是一个独立进程

中文流式识别与合成的实际实现是 ONNX Runtime 加模型文件。把它们链接进机器人
二进制会让 CI 多出一套原生依赖和一个几十 MB 的二进制；做成独立服务则可以：

- 机器人二进制的构建流程完全不变（`cargo build --release --locked` 照旧）；
- 模型、音色、采样率想换就换，不用重新发布机器人；
- 语音引擎崩了只影响通话，不会把聊天一起带走；
- `/healthz` 能直接告诉运维"模型没加载起来"以及为什么。

代价是多一个进程（约 700–900 MB 常驻）和一次回环 HTTP 往返（每句约十几毫秒），
在通话场景里都可以接受。

## 协议

机器人只依赖下面三个端点。你自己实现一份同样的服务也可以替换掉它。

### `GET /healthz`

```json
{"ok": true, "asr": true, "tts": true, "tts_sample_rate": 16000}
```

模型没加载起来时返回 `503`，并在 `error` 字段里给出原因。

### `POST /v1/asr`

- 请求体：WAV（单声道、16 位 PCM）。采样率写在 WAV 头里。
- 响应：`{"text": "识别结果"}`

### `POST /v1/tts`

- 请求体：`{"text": "要合成的话", "sample_rate": 24000}`（`sample_rate` 可选）
- 响应体：单声道 S16LE 裸 PCM，边合成边下发
- 响应头 `X-Sample-Rate`：实际采样率。**机器人以这个值为准**，因为模型输出的
  采样率是模型属性（VITS 中文常见 16 kHz，Matcha 是 22.05 kHz，Kokoro 是 24 kHz），
  不能写死。

## 安装

```bash
# 1. 依赖（Ubuntu 24.04）
sudo apt-get install -y python3-venv

# 2. 放脚本
install -d -m 0755 ~/yunxi-speech
install -m 0755 tools/speech-service/service.py ~/yunxi-speech/service.py

# 3. 建虚拟环境并装依赖
python3 -m venv ~/yunxi-speech/.venv
~/yunxi-speech/.venv/bin/pip install -U pip
~/yunxi-speech/.venv/bin/pip install -r tools/speech-service/requirements.txt --index-url https://mirrors.aliyun.com/pypi/simple/

# 4. 下模型（约 1 GB，模型落到 ~/.local/share/yunxi-speech/models）
SPEECH_MODELS_DIR=~/.local/share/yunxi-speech/models \
  tools/speech-service/download-models.sh

# 5. 前台跑一次，确认能起来
~/yunxi-speech/.venv/bin/python ~/yunxi-speech/service.py \
  --asr-dir ~/.local/share/yunxi-speech/models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17 \
  --tts-dir ~/.local/share/yunxi-speech/models/sherpa-onnx-vits-zh-ll \
  --port 6120

# 6. 自检
curl -s http://127.0.0.1:6120/healthz
```

第 5 步能起来以后再装成 systemd 服务：

```bash
sudo install -m 0644 tools/speech-service/yunxi-speech.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now yunxi-speech.service
systemctl status yunxi-speech.service
journalctl -u yunxi-speech.service -f
```

`yunxi-speech.service` 里的路径按你的实际账号和目录改（默认写的是 `ubuntu` 与
`/home/ubuntu/yunxi-speech`）。

## 模型选型

| 用途 | 默认模型 | 大小 | 说明 |
|---|---|---|---|
| 识别 | `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17` | 163 MB | 非自回归，CPU 上很快；中英日韩粤 |
| 合成 | `sherpa-onnx-vits-zh-ll` | 119 MB | 中文 VITS，5 个音色，自带分词词典与文本正则 |
| 合成（低延迟备选） | `matcha-icefall-zh-baker` + `vocos-22khz-univ.onnx` | 126 MB | 更快、22.05 kHz，但训练数据标注为仅限非商业用途 |

`download-models.sh --with-matcha` 会额外下备选合成组，之后用
`--tts-dir .../matcha-icefall-zh-baker --tts-kind matcha` 切换。

### 关于识别模型的两点说明

- 仓库默认用**整句识别**（VAD 在机器人侧完成切段），不是流式识别。这样切出来的
  每句话都能拿到完整上下文，识别更准，代价是句尾要多等一次推理；在 4 核机器上
  一句 3 秒的话大约 0.2–0.5 秒。
- 如果想换成流式识别（`sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30`，133 MB），
  需要把机器人侧的 VAD 换成"边说边送"，并注意 sherpa-onnx 默认的句尾静音判定是
  **1.2 秒**，不改的话每句话都会多等 1.2 秒。

### 文本正则很重要

`vits-zh-ll` 自带 `phone.fst`、`date.fst`、`number.fst`、`new_heteronym.fst`。
服务会把模型目录下所有 `*.fst` 自动带上。缺了它们，"110""2024年""1234块"
会被逐字念出来——打电话时这类内容很常见，不要省。

## 性能与资源

4 核 / 4 GB 的机器上：

| 组件 | CPU | 内存 |
|---|---|---|
| 识别（int8，2 线程） | 通话时约 0.1 核 | 300–500 MB |
| 合成（2 线程） | 说话时约 0.3 核 | 300–400 MB |

真正的资源大户其实是第二个 QQ（AV Host，Electron）进程，约 400–800 MB。**建议
整机至少 4 GB，8 GB 更稳。** `--threads` 默认 2，就是为了给 QQ 和系统留余量；
把它调成 4 会在最需要延迟的时候制造争抢。

## 排查

```bash
# 服务起没起来、模型加载成没成功
curl -s http://127.0.0.1:6120/healthz | python3 -m json.tool

# 合成一句话，直接听
curl -s -X POST http://127.0.0.1:6120/v1/tts \
  -H 'Content-Type: application/json' \
  -d '{"text":"你好呀，我在的。"}' --output /tmp/tts.pcm
ffplay -f s16le -ar 16000 -ac 1 /tmp/tts.pcm

# 识别一句话（用机器人采集到的同一格式：16 kHz 单声道 S16LE 的 WAV）
curl -s -X POST http://127.0.0.1:6120/v1/asr \
  -H 'Content-Type: audio/wav' --data-binary @/tmp/utt.wav
```

常见问题：

- **`/healthz` 返回 503**：看 `error` 字段，通常是模型目录写错或下载不完整。
- **合成出来的声音变调、快放或慢放**：`X-Sample-Rate` 没有被使用。机器人侧会读
  这个响应头并按它启动 `pacat`，如果你自己换客户端，记得也读。
- **合成很慢**：`--threads` 太小、或者模型目录里混进了别的模型。先看
  `journalctl -u yunxi-speech` 里的加载日志确认实际用的是哪个模型。
- **数字被逐字念**：模型目录里缺 `number.fst` / `date.fst`。

## 合规提示

中文 TTS 的模型授权普遍比代码授权更严：

- `matcha-icefall-zh-baker` 使用的是 Baker 数据集，上游标注为**仅限非商业用途**。
- `sherpa-onnx-vits-zh-ll` 由社区贡献，训练数据来源在上游 README 里没有完整声明。

自己用、给朋友用一般没问题；如果要商用，请先确认模型与训练数据的授权。
