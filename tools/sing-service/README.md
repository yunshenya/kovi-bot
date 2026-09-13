# 本机歌声合成服务（"人力VOCALOID"）

芸汐唱歌不需要新的歌声模型：**逐字调用现有的朗读 TTS，再用 Praat 的 PSOLA 把每个字
的基频改成音符频率、时长拉到音符时值**。共振峰保持不变，所以唱出来还是她自己的音色；
CPU 开销是几十毫秒一个字（实测 2.8 秒的《小星星》渲染 1.03 秒）。

为什么不用 DiffSinger 那类 SVS：部署机的内存只剩 ~1.1 GB（`kovi-bot` 724 MB +
`yunxi-speech` 556 MB + `yunxi-embed` 386 MB），塞不下第二个常驻模型。完整选型对比与
分期计划见 [`docs/qq-singing.md`](../../docs/qq-singing.md)。

## 协议

只监听回环（默认 `127.0.0.1:6121`），依赖 `yunxi-speech`（`127.0.0.1:6120`）。

### `GET /healthz`

```json
{"ok": true, "tts_ok": true, "templates": 5}
```

### `GET /v1/templates`

给机器人拼提示词用的模板清单：

```json
{"templates":[{"id":"xiaoxingxing","name":"小星星","mood":"童谣 / 轻快","syllables":14,"bpm":104}]}
```

### `POST /v1/sing`

```json
{"template": "xiaoxingxing", "lyrics": "一闪一闪亮晶晶满天都是小星星"}
```

- 响应体：单声道 16 位 PCM 的 WAV（采样率见 `X-Sample-Rate`，默认 16 kHz）。
- 响应头：`X-Template`、`X-Syllables`（实际唱出的字数）。
- 也可以直接给谱子（Phase 1 用）：`{"notes": [[1,1],[1,1],[5,1]], "lyrics": "…", "octave": 5, "tempo": 100}`。
- 音节数与音符数不一致时：多出的歌词丢掉，歌词不够则把剩下的时值并进最后一个字——
  乐句始终落在终止音上，不会半句断掉。
- 上限：单条 45 秒、120 个字；超了在句末截断。

简谱记法：`[音级, 拍数]`，音级 1–7 是 C 大调（`octave=5` 时 1 = C5 = 523.25 Hz），
8–14 是它的高八度；一拍 = `60/bpm` 秒。

## 安装

```bash
# 1. 依赖（Ubuntu 24.04；venv 是现成的 Python 3.12）
install -d -m 0755 ~/yunxi-sing
install -m 0755 tools/sing-service/service.py ~/yunxi-sing/service.py
install -m 0644 tools/sing-service/templates.json ~/yunxi-sing/templates.json
python3 -m venv ~/yunxi-sing/.venv
~/yunxi-sing/.venv/bin/pip install -r tools/sing-service/requirements.txt

# 2. 前台跑一次
~/yunxi-sing/.venv/bin/python ~/yunxi-sing/service.py --port 6121 --tts http://127.0.0.1:6120/v1/tts
curl -s http://127.0.0.1:6121/healthz

# 3. 听一段
curl -s -X POST http://127.0.0.1:6121/v1/sing -H 'Content-Type: application/json' \
  -d '{"template":"xiaoxingxing","lyrics":"一闪一闪亮晶晶满天都是小星星"}' \
  --output /tmp/sing.wav && afplay /tmp/sing.wav   # 服务器上用 ffplay/aplay

# 4. 装成 systemd 服务
sudo install -m 0644 tools/sing-service/yunxi-sing.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now yunxi-sing.service
```

## 已知限制

- **多音字与轻声**：逐字合成拿不到上下文，`了`、`的`、`不`、`行` 这类字可能被念成
  另一个读音。模板歌词尽量避开；后续可以加一张逐字读音修正表。
- **电子感**：这是"人力VOCALOID"路线，不是真正的歌声合成；要更自然的音质得换
  DiffSinger（见 `docs/qq-singing.md`）。
- **旋律有限**：还没做"模型自己写简谱"，目前只从 `templates.json` 里选。
- 只收公有领域或自创旋律：模板里不要放还在版权期内的流行曲。
