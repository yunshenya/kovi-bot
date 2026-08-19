# kovi-bot

一个使用 Rust 和 Kovi 编写的 QQ 聊天机器人。它支持群聊/私聊、兼容 OpenAI Chat
Completions 的模型服务、长期记忆、情绪与用户档案，以及可配置的随机主动消息推送。

## 运行

要求：Rust stable、可用的 OneBot 11 服务，以及一个兼容 OpenAI Chat Completions 的模型 API。

```bash
export BOT_API_TOKEN="你的模型 API Token"
cargo run
```

首次运行会创建 `bot.conf.toml`。Kovi/OneBot 连接配置位于 `kovi.conf.toml`，插件启用和群组、好友白名单位于 `kovi.plugin.toml`。

## 连接 NapCat

`6099` 是 NapCat WebUI 管理端口，不是机器人连接端口。本项目通过 NapCat 的 OneBot 11 WebSocket 服务端连接。当前 `kovi.conf.toml` 已配置为：

```toml
[server]
host = "139.155.156.152"
port = 3001
access_token = ""
secure = false
```

在 NapCat WebUI 的“网络配置”中新增或启用“WebSocket 服务端”，使用以下参数：

- Host：`0.0.0.0`
- Port：`3001`
- 消息格式：`array`
- 上报自身消息：关闭
- 强制推送事件：开启
- 心跳间隔：`30000` 毫秒
- Token：建议设置一个强随机值，并将相同值填入 `kovi.conf.toml` 的 `access_token`

保存并重启 NapCat 后再运行 `cargo run`。如果 NapCat 与机器人部署在同一台服务器，应将 NapCat Host 和 Kovi Host 都改为 `127.0.0.1`，并通过防火墙禁止公网访问 `3001`；只有跨服务器部署才需要监听公网网卡。

## GitHub Actions 部署

`.github/workflows/deplay.yml` 会在 `main` 分支推送或手动触发时运行测试、构建 Linux release、上传到 `/home/ubuntu/kovi-bot`，并创建或重启 `kovi.service`。部署前需要配置以下 GitHub Actions Secrets：

- `DEPLOY_PASSWORD`：Ubuntu 用户的 SSH 和 sudo 密码
- `BOT_API_TOKEN`：模型服务 Token
- `NAPCAT_ACCESS_TOKEN`：NapCat WebSocket 服务端 Token

部署生成的 `.env` 和 `kovi.conf.toml` 权限受限，Token 不会写入仓库。服务器与 NapCat 位于同一台机器，因此部署配置使用 `127.0.0.1:3001`。

模型与随机推送的配置示例：

```toml
[server_config]
url = "https://api.siliconflow.cn/v1/chat/completions"
model_name = "Qwen/QwQ-32B"

[proactive]
enabled = true
check_interval_secs = 300
inactivity_threshold_secs = 7200
cooldown_secs = 7200
push_probability_percent = 35
```

机器人会从最近活跃的群组/用户中随机选择接收方，再结合情绪、能量、群组话题和用户兴趣随机选择内容。冷却时间、空闲阈值和发送概率共同避免刷屏。长期记忆写入项目运行目录下的 `bot_memory.json`，最多保留 1000 条，并清理 30 天前的低重要性记录。

群聊中，普通消息只用于学习群组活跃度和话题；机器人仅在被 `@` 或消息以“芸汐/云汐”开头时调用模型回复。私聊消息会直接回复。

可用群聊命令：

- `#系统信息`
- `#健康检查`
- `#禁言` / `#结束禁言`
- `#重载配置文件` / `#重载全部配置`
- `#启用自动重载` / `#禁用自动重载`
- `#检查配置变化` / `#自动重载状态`

## 测试

```bash
cargo test --workspace --all-targets
```

## 交叉编译

仓库的 `.cargo/config.toml` 已配置以下 linker。先安装对应 Rust target 和系统交叉编译器。

Windows GNU：

```bash
rustup target add x86_64-pc-windows-gnu
# macOS: brew install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
```

Linux musl：

```bash
rustup target add x86_64-unknown-linux-musl
# macOS 可安装 musl-cross；Linux 可安装 musl-tools
cargo build --release --target x86_64-unknown-linux-musl
```

交叉编译器的可执行文件名必须分别为 `x86_64-w64-mingw32-gcc` 和 `x86_64-linux-musl-gcc`；如果本机名称不同，请调整 `.cargo/config.toml`。
