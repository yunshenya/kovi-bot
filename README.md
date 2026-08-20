# kovi-bot

一个使用 Rust 和 Kovi 编写的 QQ 聊天机器人。它支持群聊/私聊、兼容 OpenAI Chat
Completions 的模型服务、长期记忆、情绪与用户档案，以及可配置的随机主动消息推送。

## 运行

要求：Rust stable、可用的 OneBot 11 服务，以及一个兼容 OpenAI Chat Completions 的模型 API。

```bash
export BOT_API_TOKEN="你的模型 API Token"
export DATABASE_URL="postgresql://postgres:数据库密码@127.0.0.1:5432/postgres"
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
- `POSTGRES_PASSWORD`：服务器本机 `postgres` 用户的数据库密码

部署生成的 `.env` 和 `kovi.conf.toml` 权限受限，Token 和数据库密码不会写入仓库。服务器与 NapCat、PostgreSQL 位于同一台机器，因此部署配置分别使用 `127.0.0.1:3001` 和 `127.0.0.1:5432`。

模型与随机推送的配置示例：

```toml
[server_config]
url = "https://api.deepseek.com/chat/completions"
model_name = "deepseek-v4-flash"

[proactive]
enabled = true
check_interval_secs = 300
inactivity_threshold_secs = 7200
cooldown_secs = 7200
push_probability_percent = 35
main_admin = 123456789 # 可选：最信任用户的 QQ 号
main_admin_decision_interval_secs = 10800

[group_interjection]
enabled = true
min_eligible_messages = 8
cooldown_secs = 180
response_probability_percent = 35
min_message_chars = 5
conversation_window_secs = 180

[memory]
max_entries = 1000
retention_days = 30
max_conversation_messages = 25
contextual_memory_limit = 5
maintenance_interval_secs = 86400
summary_keep_recent_messages = 15
summary_max_chars = 1500

[mood]
cache_ttl_secs = 300
cache_retention_secs = 3600
natural_drift_after_secs = 7200
natural_drift_check_secs = 1800

[topic]
recent_topic_cooldown_secs = 604800
```

机器人会从最近活跃的群组和真正私聊过的用户中随机选择接收方，再结合情绪、能量、时间、群组话题和用户兴趣选择内容。冷却时间、空闲阈值、发送概率和话题去重共同避免刷屏。长期记忆以 JSONB 快照写入 PostgreSQL 的 `kovi_bot_memory` 表，默认最多保留 1000 条；后台任务会定期去重并清理 30 天前的低重要性记录。首次连接且数据库表为空时，程序会自动导入运行目录中已有的 `bot_memory.json`，并保留原文件作为备份。

配置 `main_admin` 后，该用户的关系等级会自动保持为最高。她会使用独立的主动私聊策略：每隔 `main_admin_decision_interval_secs`（默认 3 小时）才让模型基于近期互动、对话摘要、当前情绪与时间，自主决定是否联系以及说什么；没有固定日上限或固定发送间隔。该间隔只限制决策请求频率，避免每轮检查都额外消耗 token；此策略不与普通群聊/私聊随机推送竞争。

每段群聊和私聊还会维护一份可持久化的滚动摘要。短期记录超过 `max_conversation_messages`（默认 25 条）时，模型才会将较早消息连同旧摘要压缩为不超过 `summary_max_chars`（默认 1500 字）的新摘要，并保留最近 `summary_keep_recent_messages`（默认 15 条）原文继续聊天。模型暂时不可用时，会使用截断后的本地片段作为降级摘要，避免直接遗失上下文。

群聊中，普通消息用于学习群组活跃度、成员、话题、氛围和长期上下文；被 `@` 或消息以“芸汐/云汐”开头时会调用模型回复。未点名消息不会逐条调用模型：本地节流器只对话题、提问或情绪表达计数，默认每累计 8 条候选消息才按 35% 概率抽样一次，且同一群至少间隔 3 分钟，命中后才会请求模型自然接话。她成功接话（或正常回复）后，会开启 3 分钟对话窗口；窗口内只对本地判断为追问、回答、感谢或继续聊天的消息调用模型，成功回复会续期，无关群消息仍只记忆。群聊与私聊回复前，模型都会判断是否值得自然延续：通常只发一条，想继续表达时可自行连续发送任意条。每条之间的停顿会随当前情绪、能量、社交信心和随机浮动变化。私聊还会持续更新用户的兴趣、性格、关系等级和情绪历史。

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
