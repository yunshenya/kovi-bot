# 本机歌声合成服务（"人力VOCALOID"）

芸汐唱歌不需要新的歌声模型：**逐字调用现有的朗读 TTS，再用 Praat 的 PSOLA 一次把
每个字的基频改成音符频率、时长拉到音符时值**。共振峰保持不变，所以唱出来还是她自己的
音色；CPU 开销是几十毫秒一个字（实测 3.2 秒渲染一首 34 音的歌，其中九成时间是 TTS）。

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
- 响应头：`X-Template`、`X-Syllables`（**实际唱出的字数**，不含换气段）。
- 也可以直接给谱子（Phase 1 用）：`{"notes": [[1,1],[1,1],[5,1]], "lyrics": "…", "octave": 5, "tempo": 100}`。
- 音节数与音符数不一致时：多出的歌词丢掉，歌词不够则把剩下的时值并进最后一个字——
  乐句始终落在终止音上，不会半句断掉。模板的 `breath_after` 会跟着旋律一起重复。
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

## 调优要点

按"改一处、量一处"的顺序读；每一条都是踩过的坑，括号里是当时的实测值。

- **音区别超过她说话的音高太多**。模板的 `octave` 以 4 为基准（C4–A4）。第一版用八度 5，
  线上直接被评为"克式恐怖"：基频中位 696Hz vs 她说话的 248Hz。
- **旋律中心要落在她说话的音高上**：整体移调（`transpose`，默认 -5 半音）与 EQ 是
  "像不像她"的关键——250Hz 低架 +4dB 补胸腔、3kHz 峰 −6dB 去硬度、7kHz 以上 −6dB。
- **音高轮廓的均值必须恰好等于音符频率**。旧版拿 `np.cumsum(rng.normal(...))` 当 jitter，
  那是布朗游走，会被 ±3 的截断钉在偏离 0 的位置整段不动——每个音级固定跑调（deg2 低
  18 音分、deg1/5/7 高 18 音分，相邻音程被压扁最多 36 音分）。真人的 jitter 是零均值的
  快抖；现在用平滑过的零均值噪声，最后按均值归一化。
- **音高与时值一次 PSOLA 做完**（Manipulation 同时挂 PitchTier + DurationTier）。旧链路
  是"PSOLA 改音高 → 循环元音核 → 再用 Lengthen (PSOLA) 补零头"，一个音过三遍重合成，
  接缝与颗粒感都从这儿来。
- **时长靠"合成时放慢"，不是事后硬拉**：一个字只有 0.1~0.2 秒，一个音符常常 0.55~1.1 秒。
  实测 VITS 到 speed≈0.4 还能再慢一倍多；再慢个别字会塌成近静音。慢速那一档撑不住时
  才退回自然语速。
- **单个汉字的孤立合成有三种塌法**，判据必须是"有没有一段够长的有声段"，不能只看峰值：
  近乎无声（"安""星"，峰值 0.004）、有峰值但没有元音（"月""慢"，整段一个周期性帧都没有）。
  两种都撑不住长音，依次用 `字。`（句末读法，无多余音节）和 `字，啊`（最稳，按能量低谷
  切出第一个字）带出来。
- **拼接一律走交叠**：字内"清辅音 → 变调元音"3 毫秒、音与音之间 15 毫秒（连音）。
- **逐音响度对齐要 ±12dB**：逐字合成出来的响度差本来就到 20dB 以上（实测"慢"的字均 RMS
  只有"晶"的十分之一），±2.5dB 的夹子等于没夹。对齐取的是**有声段** RMS，不是整段——
  整段会被又短又响的辅音带偏。
- **换气点跟着旋律一起铺开**：模板只写自己那一段的句尾（`breath_after: [4]`），歌词更长、
  旋律重复时换气也要重复，否则 20 秒的歌只在第 4 个字之后喘一口气。
- **混响默认关闭**（试听后定稿：干声更贴她的音色）；要开用 `--reverb`，注意混响冲激必须先低通。
- 长音要加 3–6Hz/3–6% 的幅度微颤：纯循环是"逐样本重复"，耳朵一听就是机器。

改完音高轮廓 / 时长 / 响度逻辑后，用 `verify_song.py` 逐音验收（音准 ±25 音分、
无声音符、响度极差三个判据），别只靠耳朵。

## 已知限制

- **多音字与轻声**：逐字合成拿不到上下文，`了`、`的`、`不`、`行` 这类字可能被念成
  另一个读音。模板歌词尽量避开；后续可以加一张逐字读音修正表。
- **个别字模型本身就发不好**：如"安""满"孤立合成只有正常字 1/3 的响度、基频很弱，
  拉平之后仍然偏轻偏薄。这是 `vits-zh-ll` 自身的短板，不是拼接链路引入的（实测这些字
  1.2–3kHz 占比 35~41%，我们链路反而把它压到 29~32%）。
- **电子感**：这是"人力VOCALOID"路线，不是真正的歌声合成；要更自然的音质得换
  DiffSinger（见 `docs/qq-singing.md`）。
- **旋律有限**：还没做"模型自己写简谱"，目前只从 `templates.json` 里选。
- 只收公有领域或自创旋律：模板里不要放还在版权期内的流行曲。
