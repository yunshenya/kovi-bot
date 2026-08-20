# kovi-bot

一个使用 Rust 和 Kovi 编写的 QQ 聊天机器人。它支持群聊/私聊、兼容 OpenAI
Responses 或 Chat Completions 的模型服务、长期记忆、情绪与用户档案，以及可配置的随机主动消息推送。

## 运行

要求：Rust stable、可用的 OneBot 11 服务，以及一个兼容 OpenAI Responses 或 Chat Completions 的模型 API。

```bash
export OPENAI_API_KEY="你的 GPT API Token"
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

`.github/workflows/deplay.yml` 会在 `main` 分支推送或手动触发时检查格式、运行 Clippy、使用临时 PostgreSQL 做集成测试、构建 Linux release、上传到 `/home/ubuntu/kovi-bot`，并创建或重启 `kovi.service`。发布采用可回滚的二进制替换，服务启动失败时会自动恢复上一版。部署前需要配置以下 GitHub Actions Secrets：

- `DEPLOY_PASSWORD`：Ubuntu 用户的 SSH 和 sudo 密码
- `OPENAI_API_KEY`：GPT 主模型 Token
- `BOT_API_TOKEN`：切换到 DeepSeek 主模型时使用的 Token
- `VISION_API_TOKEN`：可选，切换到 DeepSeek 时使用的独立视觉模型 Token
- `NAPCAT_ACCESS_TOKEN`：NapCat WebSocket 服务端 Token
- `POSTGRES_PASSWORD`：服务器本机 `postgres` 用户的数据库密码

部署生成的 `.env` 和 `kovi.conf.toml` 权限受限，Token 和数据库密码不会写入仓库。服务器与 NapCat、PostgreSQL 位于同一台机器，因此部署配置分别使用 `127.0.0.1:3001` 和 `127.0.0.1:5432`。

模型与随机推送的配置示例：

```toml
[server_config]
url = "https://codex666ai.com"
model_name = "gpt-5.5"
wire_api = "responses"
supports_vision = true
api_key_env = "OPENAI_API_KEY"
requires_auth = true
actor_authorization = "local-image-extension"
max_output_tokens = 1200
request_timeout_secs = 60
max_retries = 2

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
direct_repeat_window_secs = 120
direct_spam_cooldown_secs = 600
direct_rate_window_secs = 60
direct_rate_limit = 4

[memory]
max_entries = 1000
retention_days = 30
max_conversation_messages = 25
max_conversation_tokens = 6000
contextual_memory_limit = 5
maintenance_interval_secs = 86400
summary_keep_recent_messages = 15
summary_max_chars = 1500
autonomous_query_enabled = true
autonomous_query_max_rounds = 2
autonomous_query_max_results = 8
autonomous_query_max_days = 3650

[message_batch]
enabled = true
complete_delay_ms = 900
normal_delay_ms = 1600
incomplete_delay_ms = 2300
max_wait_ms = 5000
max_parts = 6
max_chars = 500

[mood]
cache_ttl_secs = 300
cache_retention_secs = 3600
natural_drift_after_secs = 7200
natural_drift_check_secs = 1800

[topic]
recent_topic_cooldown_secs = 604800
```

机器人会从最近活跃的群组和真正私聊过的用户中随机选择接收方，再结合情绪、能量、时间、群组话题和用户兴趣选择内容。冷却时间、空闲阈值、发送概率和话题去重共同避免刷屏。长期记忆、用户档案、群组档案、滚动摘要和人格分别写入 PostgreSQL 分表，不再为每次变化重写整份 JSONB；默认最多保留 1000 条明细，后台任务会定期去重并清理 30 天前的低重要性记录。首次升级时会自动从旧 `kovi_bot_memory` JSONB 快照（或运行目录的 `bot_memory.json`）迁移，原数据保留作为兼容备份。

配置 `main_admin` 后，该用户的关系等级会自动保持为最高。她会使用独立的主动私聊策略：每隔 `main_admin_decision_interval_secs`（默认 3 小时）才让模型基于近期互动、对话摘要、当前情绪与时间，自主决定是否联系以及说什么；没有固定日上限或固定发送间隔。该间隔只限制决策请求频率，避免每轮检查都额外消耗 token；此策略不与普通群聊/私聊随机推送竞争。

每段群聊和私聊还会维护一份可持久化的滚动摘要。短期记录超过 `max_conversation_messages`（默认 25 条）或估算超过 `max_conversation_tokens`（默认 6000 token）时，模型会将较早消息连同旧摘要压缩为不超过 `summary_max_chars`（默认 1500 字）的新摘要，并尽量保留最近 `summary_keep_recent_messages`（默认 15 条）原文继续聊天。模型暂时不可用时，会使用截断后的本地片段作为降级摘要，避免直接遗失上下文。

当自动附带的上下文仍不足时，芸汐可以自主提出一次受限的长期记忆查询，选择关键词、回看天数、记忆类型、最低重要性和结果数量；默认单次回复最多查询 2 轮、每轮返回 8 条。程序会把查询强制限定在当前私聊对象或当前群，并使用参数化 SQL 查询 PostgreSQL。模型不能指定用户号、群号、表名或 SQL，也不能借此写入、修改或删除数据；查询设有 2 秒超时、字段长度和返回数量限制。普通寒暄和已有足够上下文的对话不会额外查询模型。

群聊中，普通消息用于学习群组活跃度、成员、话题、氛围和长期上下文；被 `@`、回复她，或消息以“芸汐/云汐”开头时会调用模型回复。未点名消息不匹配固定关键词，也不会逐条调用模型：本地节流器只排除命令、无有效文字和过短内容，默认每累计 4 条按 60% 概率抽样一次，命中后由一次模型调用直接决定回复或 `[sp]`，不会为“先判断、再生成”重复调用模型。每群两次未点名模型判断至少间隔 60 秒，10 分钟最多判断 3 次，单次最多生成 240 token；只有真正发出插话后才进入同群 3 分钟发言冷却。

她成功回复后会为当时的聊天成员开启 3 分钟滚动窗口，不再依赖“你、吗、嗯、然后”等接续关键词：窗口参与者发出的任何有效聊天内容都可自然续聊；她刚发言后的 45 秒内，其他成员也可以直接接上并加入窗口。其他人的群内闲聊仍走低频抽样，不会让整个群在窗口期内每句话都请求模型。

群聊和私聊都会在本地合并同一发送者连续发出的消息气泡，不额外消耗模型 token。完整句默认等待约 0.9 秒，普通句约 1.6 秒，短残句或以“因为、但是、然后”等结尾的未说完内容约 2.3 秒；新气泡会续期，但从第一条起最多等待 5 秒。达到 6 个气泡或约 500 字时立即提交。第一条中的 `@` 状态会保留，引用消息、已学习表情和后续文字也能一起交给模型；不同用户之间绝不会合并。若用户随后明确说“别回复”，尚未提交的气泡会被取消。

回复生成和连续气泡发送都支持打断。私聊收到任何新消息时，会废弃上一轮尚未返回的模型结果，并停止尚未发出的后续气泡；群聊只在再次 `@`/回复她、活跃窗口内自然接话，或收到“停、等等、算了、不用说了、别回复这条”等明确要求时打断，其他成员的无关消息不会影响她。已经发出的气泡不会撤回，短期上下文和 PostgreSQL 长期记忆也只记录实际发出的部分。

直接点名还带有按“群 + 成员”隔离的防刷限制：120 秒内重复同一句时，从第二次开始静默，连续第三次会让该成员进入 10 分钟冷却；即使不断换内容，60 秒超过 4 次直接触发也会进入冷却。管理员不受此限制。群聊回复默认采用一两条短气泡；遇到详细解释、步骤、复杂分析或不能过度缩短的安慰时，模型可以按内容需要完整回答，不会被固定字数截断。私聊长度不受影响。

群聊与私聊都会展开被引用消息的文字和已学习表情含义交给模型。回复前模型会判断是否值得自然延续；每条之间的停顿会随当前情绪、能量、社交信心和随机浮动变化。私聊还会持续更新用户的兴趣、性格、关系等级和情绪历史。

## 表情包记忆库

表情包标签直接保存在 PostgreSQL 的 `kovi_bot_sticker_memory` 表中，只保存 OneBot 图片/表情的唯一标识和人工标签，不下载或保存图片文件。标签按群聊或私聊用户隔离，某个群里的教学不会覆盖其他群的理解；旧版全局标签仍可作为只读兜底。回复（引用）那张表情包并发送命令即可教会她；把命令和表情放在同一条消息中也仍然兼容：

```text
#教芸汐 这个表情是无语又想笑
```

她会确认已记住。之后相同表情会把“无语又想笑”作为上下文交给聊天模型；纯图片且没有学习过时默认静默，不调用模型、不消耗 token。图片同时带有普通文字时，仍会正常理解和回复文字内容。

## 截图识别

她支持把截图作为当前对话的视觉输入，但一轮请求只使用一个主聊天模型：

- 主模型支持图片时，图片和文字一起直接发送给主模型，不调用另一个聊天模型。
- 主模型不支持图片时，图片才会先交给独立视觉模型，视觉模型只返回图片文字分析；最终回复仍由主模型独立生成。

因此，选择 GPT-5.5 作为主模型时不会再调用 DeepSeek；选择 DeepSeek 作为主模型时，只有图片输入才会额外调用视觉模型。图片本身不会写入长期记忆或日志。

切换到 DeepSeek 主模型时，将 `[server_config]` 改为：

```toml
[server_config]
url = "https://api.deepseek.com/chat/completions"
model_name = "deepseek-v4-flash"
wire_api = "chat_completions"
supports_vision = false
api_key_env = "BOT_API_TOKEN"
requires_auth = true
actor_authorization = ""
```

当前仓库默认的 GPT-5.5 主聊天配置为：

```toml
[server_config]
url = "https://codex666ai.com"
model_name = "gpt-5.5"
wire_api = "responses"
supports_vision = true
api_key_env = "OPENAI_API_KEY"
requires_auth = false
actor_authorization = "local-image-extension"
max_output_tokens = 1200
request_timeout_secs = 60
max_retries = 2
```

当前 `codex666ai.com` 接口要求通过 `OPENAI_API_KEY` 发送 Bearer Token，因此这里必须保持 `requires_auth = true`。如果服务实际要求 `/v1/responses`，将 `url` 直接写成完整接口地址。

只有主模型的 `supports_vision = false` 时，才需要配置独立视觉接口：

GitHub Actions 会在部署时自动写入以下视觉配置；当前 GPT-5.5 主模型会忽略它们，切换到 DeepSeek 后直接生效：

```env
VISION_API_URL=https://codex666ai.com
VISION_WIRE_API=responses
VISION_MODEL_NAME=gpt-5.5
VISION_ACTOR_AUTHORIZATION=local-image-extension
VISION_REQUIRES_AUTH=false
```

如果视觉接口需要 Bearer Token，在 GitHub Repository Secrets 中增加 `VISION_API_TOKEN`，并把 `VISION_REQUIRES_AUTH` 改为 `true`。

```bash
export VISION_API_URL="https://codex666ai.com"
export VISION_WIRE_API="responses"
export VISION_MODEL_NAME="gpt-5.5"
export VISION_API_TOKEN="你的视觉模型 Token"
export VISION_ACTOR_AUTHORIZATION="local-image-extension"
# 若视觉服务明确不要求 Bearer Token，改为 false；此时不会发送 VISION_API_TOKEN
export VISION_REQUIRES_AUTH="false"
```

`VISION_WIRE_API=responses` 时，`VISION_API_URL` 可以填写服务根地址，程序会补上 `/responses`；如果服务实际要求 `/v1/responses`，直接填写完整地址即可。若使用旧式 Chat Completions 接口，将其设为 `chat_completions`，程序会发送 `messages[].content` 中的 `image_url` 图片输入。

视觉接口只在非视觉主模型收到图片时启用；GPT-5.5 主模型会完全跳过这些 `VISION_*` 配置。

群聊中可以使用以下方式触发：

- `@芸汐` 后附截图，并提出问题
- 回复她或其他人的截图，再发送“帮我看看这里”
- 将 `#看截图` 与截图放在同一条消息中
- 回复截图后发送 `#看截图`

私聊中，发送截图并附文字即可；只发送截图时，她也会请求模型描述图片。没有截图时使用 `#看截图`，她会提示补发截图。群聊中没有被点名、没有回复机器人、也没有显式看图命令的图片不会触发视觉模型；但已经进入芸汐回复后的窗口期时，参与者发送的纯截图也会作为接话处理。

群聊和私聊回复时，模型会看到最近消息的消息 ID、发送者和内容，并自行判断是否需要引用某条消息或 @ 某位参与者。不需要时不会强制引用；需要时只在第一条回复气泡中添加引用和 @，后续气泡保持普通消息格式。

单次最多处理 4 张图片，每张限制 10 MB，支持 PNG、JPEG 和 WebP。图片地址会先由机器人下载并转换成本地请求内容，因此视觉模型不需要直接访问 QQ 或 NapCat 的内网地址。

可用群聊命令（除表情教学外，管理类命令仅 Kovi 管理员可执行）：

- `#系统信息`
- `#健康检查`
- `#禁言` / `#结束禁言`
- `#重载配置文件` / `#重载全部配置`
- `#启用自动重载` / `#禁用自动重载`
- `#检查配置变化` / `#自动重载状态`
- `#教芸汐 <表情含义>`（回复或引用要教学的表情包后发送）
- `#看截图`（与截图同发，或回复截图后发送）

## 测试

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
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
