# 芸汐的本机嵌入服务

给记忆检索的**语义那一路**提供向量。只监听回环地址，只被同机的 kovi-bot 调用。

## 为什么是独立进程

和 `yunxi-speech` 同一个理由，而且是那个项目已经验证过的：用**预编译 wheel**
（onnxruntime / tokenizers），不把 ONNX Runtime 链接进机器人二进制，机器人的 CI
构建保持原样；模型放磁盘不放二进制。

## 协议

```
GET  /healthz   -> {"ok":true,"ready":true,"model":"bge-small-zh-v1.5","dim":512}
POST /v1/embed  <- {"texts":["...","..."],"query":false}
                -> {"vectors":[[...]],"dim":512,"model":"..."}
```

- `query=true` 时服务端会加 bge 的**查询侧指令前缀**（`为这个句子生成表示以用于检索相关文章：`）。
  bge 是非对称检索模型，查询与文档的编码方式不同，搞反了明显掉点——所以前缀在服务里
  定死，调用方不必知道这件事。
- 输出已做 L2 归一化，于是余弦相似度退化成点积。
- 一次最多 64 段；单段截断到 1024 字符（bge 窗口是 512 token）。

## 部署

模型文件从 hf-mirror 下到本机再上传（**不要**在服务器上直接下：实测那条链路
只有 ~3KB/s，95MB 要下九个小时）。本机下载后 `scp` 过去，服务器上是 2.4MB/s。

```bash
# 本机
mkdir -p /tmp/bge-small-zh/onnx && cd /tmp/bge-small-zh
for f in config.json tokenizer.json tokenizer_config.json special_tokens_map.json; do
  curl -sL -o "$f" "https://hf-mirror.com/Xenova/bge-small-zh-v1.5/resolve/main/$f"
done
curl -sL -o onnx/model_quantized.onnx \
  "https://hf-mirror.com/Xenova/bge-small-zh-v1.5/resolve/main/onnx/model_quantized.onnx"

# 上传
ssh <host> 'mkdir -p ~/yunxi-embed/models'
scp -r service.py requirements.txt <host>:~/yunxi-embed/
scp -r /tmp/bge-small-zh/* <host>:~/yunxi-embed/models/

# 服务器
cd ~/yunxi-embed && python3 -m venv .venv && .venv/bin/pip install -r requirements.txt
# 然后装 systemd 单元（见 deploy/）
```
