# kovi-bot

一个使用 Rust 和 Kovi 编写的 QQ 聊天机器人。它支持群聊/私聊、兼容 OpenAI
Responses 或 Chat Completions 的模型服务、长期记忆、情绪与用户档案，以及可配置的随机主动消息推送。

## 配置与运行

要求：`rust-toolchain.toml` 指定的 Rust、OneBot 11 服务、PostgreSQL，以及兼容 OpenAI
Responses 或 Chat Completions 的模型 API。Redis 可选。

仓库只跟踪不含真实账号、主机和 Token 的模板。首次运行前复制模板，并替换所有占位值：

```bash
cp bot.conf.example.toml bot.conf.toml
cp kovi.conf.example.toml kovi.conf.toml
cp kovi.plugin.example.toml kovi.plugin.toml
cp .env.example .env
```

- 在 `kovi.conf.toml` 填写所有者 QQ 号和强随机 OneBot Token。
- 在 `kovi.plugin.toml` 明确填写允许的好友和群；默认空白名单会拒绝所有消息。
- 在 `bot.conf.toml` 填写 HTTPS 模型地址和模型名。

运行时文件已加入 `.gitignore`，不要强制提交它们。使用专用的非超级用户数据库账号：

```bash
# 编辑 .env 后显式加载；程序不会自行解析 dotenv 文件。
set -a
. ./.env
set +a
cargo run --locked
```

### 可选的本地 Intrinsic 模型

Intrinsic 模型不是编译进可执行文件的；运行时会从
`model.intrinsic.asset_dir` 读取外部 bundle。权重不放进 Git，首次需要本地推理时再下载：

```bash
# 只下载文字模型（约 216 MiB）
./scripts/download-model.sh --variant text

# 需要本地截图/图片推理时，下载包含视觉编码器的完整 bundle（约 416 MB）
./scripts/download-model.sh --variant full
```

脚本默认从固定 revision 的 Hugging Face 地址下载，并在安装前按 bundle manifest 校验每个
文件的大小和 SHA-256；可通过 `--language-base-url`、`--vision-base-url` 或对应环境变量
切换到镜像。模型包的 manifest、来源和第三方许可说明保存在 `model-assets/`，实际运行目录
`models/yunxi-intrinsic/` 被 `.gitignore` 忽略。

没有下载模型时，Core 不会编造固定的可见保底文案；需要本地生成时会进入无模型静默路径。
要启用完整本地模型，必须把对应变体下载到默认目录，并从项目根目录启动程序。文字包的
manifest 明确关闭视觉能力，不能只下载视觉权重后单独使用。

不依赖 Kovi、OneBot 或 QQ 的 Core 验收宿主：

```bash
cargo run -p yunxi-cli
```

CLI 默认使用进程内 Core stores；设置 `YUNXI_CLI_STATE` 后，会用单个有界 JSON snapshot
持久化稳定的 Core 身份以及 Memory、Affect、Relation、OpenLoop。`YUNXI_CLI_JOURNAL`
可另外开启 turn 审计日志：

```bash
YUNXI_CLI_STATE=./yunxi-cli-state.json \
YUNXI_CLI_JOURNAL=./yunxi-cli-turns.jsonl \
cargo run -p yunxi-cli
```

私聊安全纯文本/普通图片和群聊中的普通文本/普通图片默认由 Yunxi Core 接管；命令、提醒、Agent Run、音视频/文件、
表情协议和其他专用消息继续由 Kovi Host/Adapter 处理。Core 会观察所有普通群聊消息，但只有经过本地注意力限流的未点名
消息才进入模型决策；明确 `@` 芸汐、文本中叫“芸汐”或私聊消息会立即进入决策。Core 是支持事件的唯一可见回复 owner，Core
入队失败时不会回退到 Host，避免同一消息产生两条回复。

建议在 `[identity]` 配置 canonical owner 的 Core `PersonId`。该 Person 必须在 PostgreSQL
中恰好有一个 QQ ExternalIdentity；缺失或歧义时权限、主动投递和故障通知会 fail-closed，
不会猜测旧账号。未配置时才兼容 `[proactive].main_admin`。

### Yunxi Mind v2

Mind v2 在 PostgreSQL 中维护有界、版本化的 SelfModel、Belief、Preference、Interest、
OpenQuestion、Agenda 和 Episode。默认启用持久状态、低频确定性 Reflection 和 `active` 影响模式，
因此 Mind 会参与 Core 回复上下文、群聊候选接话和主动聊天候选。`shadow` 仍可作为诊断模式显式配置，
只记录“如果启用会怎样”的结构化指标；`disabled` 会关闭新增与影响，但数据删除仍会覆盖已经持久化的 Mind 数据。

```toml
[mind]
enabled = true
belief_enabled = true
preference_enabled = true
interest_enabled = true
curiosity_enabled = true
agenda_enabled = true
reflection_enabled = true
mind_planner_enabled = true
influence_mode = "active" # disabled / shadow / active
snapshot_timeout_ms = 75
event_update_timeout_ms = 40
reflection_min_interval_minutes = 60
reflection_max_events = 32
question_cooldown_minutes = 120
```

Mind 候选复用正常回复已有的 bounded structured sidecar，不会为每条消息增加第二次模型
调用；Reflection 第一版也完全是确定性处理。快照读取和入站状态更新都有独立超时，数据库
不可用或超时时会 fail-soft 到空快照，不阻断直接回复。候选只有在对应 outgoing 赢得现有
提交竞争后才会写入，发送被 supersede/cancel、repair、fallback、stop 或无效工具输出时均会
丢弃。管理员只能在私聊中使用 `#mind-status` 查看版本、计数、Reflection 与更新指标；该命令
不显示私聊内容、隐藏推理或 chain-of-thought。

## 连接 NapCat

`6099` 是 NapCat WebUI 管理端口，不是机器人连接端口。本项目连接 NapCat 的 OneBot 11
WebSocket 服务端。NapCat 和机器人同机时，两端都应只监听 `127.0.0.1:3001`，并配置同一个
至少 24 个随机安全字符的 Token；仓库模板故意使用无法工作的占位 Token。

跨主机部署时优先使用受控私网、VPN 或 WSS，并通过防火墙限制来源。不要把无 Token 的
OneBot 端口暴露到公网。

## GitHub Actions

[`CI`](.github/workflows/ci.yml) 会在 PR 与 `main` 推送时执行格式、Clippy、PostgreSQL
集成测试、release 构建、RustSec 审计、许可证/来源策略和密钥扫描。只有仓库自身 `main`
分支的 push 通过 CI 后，受保护的 [`Deploy production`](.github/workflows/deploy.yml) 才会
使用 SSH Key 发布；PR 代码不能读取生产 Secrets。

生产发布使用专用应用账号、最小权限数据库角色、固定 SSH 主机公钥、版本化 release 和
readiness 文件。二进制、配置与环境变量会作为一个整体原子切换，失败时整体回滚。服务器
初始化及 GitHub Environment 变量/Secrets 清单见
[部署手册](.github/deploy/README.md)。

模型与随机推送的配置示例：

```toml
[identity]
# canonical Yunxi PersonId；配置后优先使用该 Person 的唯一 QQ 路由
# owner_person_id = "00000000-0000-0000-0000-000000000000"

[server_config]
url = "https://api.example.com/v1/responses"
model_name = "your-model-name"
wire_api = "responses"
supports_vision = true
api_key_env = "OPENAI_API_KEY"
requires_auth = true
actor_authorization = ""
thinking_mode = "auto" # auto 或 disabled；DeepSeek v4 建议 disabled
max_output_tokens = 1200
request_timeout_secs = 60
max_retries = 2

[proactive]
enabled = true
check_interval_secs = 300
inactivity_threshold_secs = 7200
cooldown_secs = 7200
push_probability_percent = 35
# main_admin 可选，只应写在未跟踪的运行时配置中
main_admin_decision_interval_secs = 10800
# Neuro-sama 风格自主会话（宿主状态机结合语境和已发送结果决定是否继续，模型只生成候选正文）
autonomous_conversation_enabled = true
autonomous_conversation_check_interval_secs = 3
autonomous_conversation_idle_secs = 5
autonomous_conversation_cooldown_secs = 3
autonomous_conversation_group_idle_secs = 45
autonomous_conversation_group_cooldown_secs = 15
# Legacy compatibility field; retained for config compatibility, not read by the loop.
autonomous_conversation_group_max_turns = 1
# 单条入站消息之后最多连续多少次自主续聊（新入站归零计数，私聊与群聊通用）。
# 这是有界"想接话"的硬上限，防止把一次聊天刷成无上限的高潮并烧掉大量 token。
autonomous_conversation_max_turns = 6
# 私聊连续回合由宿主根据对话状态处理；模型只生成当前回合的自然正文。

[traffic]
enabled = true
window_secs = 60
per_user_limit = 20
global_limit = 300
cooldown_secs = 120
max_pending_turns = 16
max_input_chars = 6000
max_model_response_bytes = 2097152
max_model_queue = 64
model_queue_timeout_secs = 15

[group_interjection]
enabled = true
min_eligible_messages = 8
cooldown_secs = 180
response_probability_percent = 35
min_message_chars = 5
direct_spam_cooldown_secs = 600
direct_rate_window_secs = 60
direct_rate_limit = 4

[memory]
max_entries = 1000
retention_days = 30
episode_retention_days = 365
episode_max_per_scope = 128
episode_protected_salience = 0.7
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

[tools]
enabled = true
max_rounds = 2
timeout_secs = 15
max_result_chars = 12000
web_search_enabled = true
web_fetch_enabled = true
web_search_max_results = 5
web_fetch_max_chars = 12000

[agent_runs]
enabled = true
recovery_scan_secs = 30       # 跨进程恢复兜底；进程内创建会立即唤醒
lease_secs = 60
request_timeout_secs = 15
min_interval_secs = 5
max_interval_secs = 86400
default_interval_secs = 30
default_stop_after_minutes = 1440
max_stop_after_minutes = 10080
default_max_executions = 20000
max_executions_per_run = 100000
max_active_per_user = 10
max_active_total = 100
max_consecutive_failures = 5
max_response_bytes = 524288
max_body_preview_chars = 2000
max_notification_chars = 500
claim_batch_size = 16

[agent_tasks]
enabled = true
poll_interval_secs = 5
max_collect_minutes = 120
default_collect_minutes = 10
max_active_per_actor = 20
max_active_total = 200
max_events_per_task = 200
min_valid_replies = 2
quiet_period_secs = 45
max_event_chars = 500
max_report_chars = 3000
lease_secs = 180

[vision]
provider = "auto"              # auto、builtin 或 mcp
mcp_server = ""                # 对应 tools.mcp_servers.name
mcp_tool = "analyze_image"     # MCP 工具名
timeout_secs = 60

# MCP 服务默认不配置。只有明确列入 allowed_tools 的工具才会暴露给芸汐。
# [[tools.mcp_servers]]
# name = "notes"
# command = "/usr/local/bin/your-read-only-mcp-server"
# args = ["--stdio"]
# cwd = "/home/ubuntu/kovi-bot"
# inherit_env = ["NOTES_API_TOKEN"]
# allowed_tools = ["search_notes", "read_note", "analyze_image"]
# read_only = true

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

机器人会从最近活跃的群组和真正私聊过的用户中随机选择接收方，再结合情绪、能量、时间、群组话题和用户兴趣选择内容。冷却时间、空闲阈值、发送概率、目标冷却和每日上限共同避免刷屏。主动消息的决策时间、最后发送时间和每日计数单独写入 PostgreSQL 的 `kovi_bot_proactive_state`，不受普通记忆容量清理影响；服务重启后也不会重新触发一轮主动消息。长期记忆、用户档案、群组档案、滚动摘要和人格分别写入 PostgreSQL 分表，不再为每次变化重写整份 JSONB；默认最多保留 1000 条长期记忆明细，后台任务会定期去重并清理 30 天前的低重要性记录。Mind Episode 情节记忆使用独立策略：默认保留 365 天、每个作用域最多 128 条已知状态记录，保护项按优先级保留；当已知状态仍超过上限时，会从价值最低的记录开始淘汰（必要时也包括保护项）。未知状态不参与淘汰并始终保留。超过保留期未活跃的用户/群档案及其摘要也会清理；高重要性记忆不按年龄清理，但仍受各自容量策略约束，人格和表情标签则需显式删除。完整的数据范围、外部传输和删除边界见[数据与隐私说明](docs/privacy.md)。首次升级时会自动从旧 `kovi_bot_memory` JSONB 快照（或运行目录的 `bot_memory.json`）迁移，原数据保留作为兼容备份。

配置 canonical owner 后，该用户的关系等级会自动保持为最高。她会使用独立的主动私聊策略：模型最多每隔 `main_admin_decision_interval_secs`（默认 3 小时）评估一次，但实际发送还必须满足 `main_admin_cooldown_secs`（默认 6 小时）、`main_admin_daily_limit`（默认每天 2 条）和全局 `daily_limit`（默认每天 4 条）；同一目标的主动消息默认至少间隔 `target_cooldown_secs`（默认 6 小时）。用户或群组刚刚主动互动后，`recent_interaction_cooldown_secs`（默认 2 小时）内不会追加主动开场。上述状态独立持久化，服务重启和普通记忆清理都不会绕过限频。未配置 canonical owner 时，这段兼容策略才使用 `main_admin` QQ 号。

每段群聊和私聊还会维护一份可持久化的滚动摘要。短期记录超过 `max_conversation_messages`（默认 25 条）或估算超过 `max_conversation_tokens`（默认 6000 token）时，模型会将较早消息连同旧摘要压缩为不超过 `summary_max_chars`（默认 1500 字）的新摘要，并尽量保留最近 `summary_keep_recent_messages`（默认 15 条）原文继续聊天。模型暂时不可用时，会使用截断后的本地片段作为降级摘要，避免直接遗失上下文。

`traffic` 在模型调用和图片下载前实施硬边界：默认每用户每分钟 20 个、全局每分钟 300 个合格事件，超限冷却 120 秒；并发模型请求之外最多排队 64 个，单个请求最多等待 15 秒。输入、模型响应字节数和每会话待处理 turn 也分别有上限。管理员仍受全局资源上限约束，防止错误循环拖垮进程。

应用运行时不再使用 SQLite，`sqlx` 只启用 PostgreSQL；长期记忆、用户档案、群组档案、滚动摘要、人格和表情包记忆全部由 PostgreSQL 持久化。Redis 只保存可重建的短期运行态：芸汐最近 110 秒内可主动撤回的消息候选、等待图片的临时标记，以及直接点名限流计数。Redis 不可用时这些功能会回退到进程内状态，不会阻断启动，也不会把群名片和 QQ 昵称拼接写入记忆档案。

当自动附带的上下文仍不足时，芸汐可以自主提出一次受限的长期记忆查询，选择关键词、回看天数、记忆类型、最低重要性和结果数量；默认单次回复最多查询 2 轮、每轮返回 8 条。程序会把查询强制限定在当前私聊对象或当前群，并使用参数化 SQL 查询 PostgreSQL。模型不能指定用户号、群号、表名或 SQL，也不能借此写入、修改或删除数据；查询设有 2 秒超时、字段长度和返回数量限制。普通寒暄和已有足够上下文的对话不会额外查询模型。

聊天语义统一经过 `MessageUnderstanding` 层：一次受限的模型理解会同时产出情绪、停止/静默意图、图片意图、连续对话相关性、未点名接话价值、兴趣、话题和群聊氛围。情绪和这些自然语言信号不再由固定词表或字符串 `if/else` 推断；模型输出异常时回退到中性状态。权限、命令、消息段、撤回候选、限流和资源上限仍由程序硬边界控制。

群聊中，普通消息用于学习群组活跃度、成员、话题、氛围和长期上下文；被 `@`、回复她，或在正文中直接叫“芸汐”“云汐”时会调用模型回复。未点名消息不使用固定词表，也不会逐条调用模型：本地节流器只按消息长度、计数和冷却做低成本抽样，默认每累计 4 条按 60% 概率抽样一次，命中后由一次语义理解调用决定是否值得接话，再生成回复。每群两次未点名模型判断至少间隔 60 秒，10 分钟最多判断 3 次，单次最多生成 240 token；只有真正发出插话后才进入同群 3 分钟发言冷却。

她成功回复后会为当时的聊天成员开启 3 分钟滚动窗口，不再依赖接续词表：窗口参与者发出的消息由语义理解判断是否自然接着当前话题，符合时才把窗口截止时间向后延长 3 分钟；她刚发言后的 45 秒内，其他成员也可以直接接上并加入窗口。其他人的群内闲聊仍走低频抽样，不会让整个群在窗口期内每句话请求模型。

群聊和私聊都会在本地合并同一发送者连续发出的消息气泡，合并阶段只做本地判断；批次完成后才进行一次语义理解。完整句默认等待约 0.9 秒，普通句约 1.6 秒，短残句或未说完内容约 2.3 秒；新气泡会续期，但从第一条起最多等待 5 秒。达到 6 个气泡或约 500 字时立即提交。第一条中的 `@` 状态会保留，引用消息、已学习表情和后续文字也能一起交给模型；不同用户之间不会合并。超过 `traffic.max_input_chars` 的输入会被限制，每个会话最多排队 `traffic.max_pending_turns` 个完整 turn。

回复生成和连续气泡发送支持有条件打断。私聊中的普通新消息会作为完整 turn 排队，不会仅因为到达较晚就丢弃正在生成的回复；明确停止请求或新的识图命令会中断当前轮。群聊中的再次 `@`/回复、明确停止请求或识图命令可以中断当前轮，其他成员的无关消息不会影响她。ticket 在撤回、发送和状态更新等副作用前会重新校验；已经成功发出的气泡不会自动撤回，历史只记录实际发送的部分。

直接 `@`、回复或文字点名还带有按“群 + 成员”隔离的防刷限制：不比较消息文本，只按 60 秒内的触发次数计数，超过 4 次进入 10 分钟冷却。管理员不受此限制。普通模型只生成可见自然正文；是否发送、气泡数量、顺序和连续回合都由宿主状态机编排。每个气泡都应承载新的、无法自然合并的信息，详细解释、步骤、复杂分析或不能过度缩短的安慰仍可按内容需要完整回答，不会被固定字数截断。

普通聊天是否发送由入站语义、权限和宿主状态机决定；模型正文为空或包含内部标记时会被拒绝并进入有界修复，不会把模型自行构造的 silent/JSON 当作控制信号。引用、`@`、撤回等显式消息动作仍走独立的受控动作分支，不会混入普通正文。

连续气泡由宿主按内容逐条调用纯文本生成并按序发送；普通回复不会要求模型构造 `messages` 数组或其他协议。显式动作分支仍可使用独立的受控动作格式；旧版 `[[NEXT_MESSAGE]]` 仅在本地兼容解析器中保留，角色配置和运行提示都不会再要求模型生成它。

群聊与私聊都会展开被引用消息的文字和已学习表情含义交给模型。回复前模型会判断是否值得自然延续；每条之间的停顿会随当前情绪、能量、社交信心和随机浮动变化。私聊还会持续更新用户的兴趣、性格、关系等级和情绪历史。

## 表情包记忆库

表情包的人工标签保存在 PostgreSQL 的 `kovi_bot_sticker_memory` 表中，未学习但实际参与过回复的表情会记录在 `kovi_bot_sticker_usage` 表中，最近的少量使用上下文保存在 `kovi_bot_sticker_observations` 表中，模型整理出的待确认建议保存在 `kovi_bot_sticker_candidates` 表中。所有表都只保存 OneBot 图片/表情的稳定标识和有界文字资料，不下载或保存图片文件；使用记录还会保存作用域、使用次数和最近使用时间。标签、使用记录和候选按群聊或私聊用户隔离，某个群里的教学不会覆盖其他群的理解；旧版全局标签仍可作为只读兜底。Kovi 管理员引用或附带表情后直接描述含义，模型会调用受权限保护的 `sticker_memory.teach` 内置工具写入正式记忆；普通聊天、提问和猜测不会触发教学。原有命令仍然兼容：

```text
#教芸汐 这个表情是无语又想笑
```

她会确认已记住。之后相同表情会把“无语又想笑”作为上下文交给聊天模型，私聊中的纯表情也能直接结合这个含义自然接话。尚未学习但已经参与过互动的表情，下次出现时会明确告诉模型“以前用过但还没有明确含义”，仍然禁止模型擅自猜测；真正进入回复流程的群聊和私聊表情都会更新使用次数，普通群聊里被静默的纯图片不会写入使用记录。Core 接管的私聊普通图片会进入视觉输入，带有明确 `@` 或文字点名的群聊普通图片也会进入 Core 视觉输入；两者都会优先结合聊天语境自然回应，而不是默认写成逐项识图报告。未点名群图片仍默认静默，但芸汐刚发言后 90 秒内收到的表情会作为情绪回应候选，结合上一条消息自然接话。同一成员表情回应至少间隔 30 秒，同一群 5 分钟最多回应 3 次。

同一个未学习表情累计进入回复流程至少 3 次、且有多个上下文样本后，模型会整理一个待确认建议，但不会自动写入正式含义。Kovi 管理员可以在群里查看当前群候选，或在管理员私聊中查看全部候选：

```text
#待确认表情
#确认表情 104 无语又想笑
#驳回表情 104
#忽略表情 104 30
```

确认候选会将管理员填写的含义写入正式表情记忆；也可以直接在原聊天中引用表情发送 `#教芸汐 这个表情是……`，现有教学流程会自动完成候选确认。驳回或忽略只会暂时抑制重复建议，不会把模型猜测当成事实。

## 截图识别

她支持把截图作为当前对话的视觉输入，但一轮请求只使用一个主聊天模型；`#看截图`、`#看图`、`#识图` 这些兼容命令仍由 Host 视觉流程处理，普通自然语言图片消息由 Core 直接处理：

- 主模型支持图片时，图片和文字一起直接发送给主模型，不调用另一个聊天模型。
- 主模型不支持图片时，图片才会先交给独立视觉模型，视觉模型只返回图片文字分析；最终回复仍由主模型独立生成。

因此，`supports_vision = true` 时不会再调用独立视觉聊天模型；设为 `false` 时，只有图片输入才会额外调用视觉模型。图片文件本身不会写入长期记忆，但当前图片会发送给配置的模型服务，OneBot 图片标识还可能进入短期索引。

使用不支持视觉的主模型时，可按服务商文档填写 Chat Completions 配置：

```toml
[server_config]
url = "https://api.example.com/v1/chat/completions"
model_name = "your-text-model"
wire_api = "chat_completions"
supports_vision = false
api_key_env = "BOT_API_TOKEN"
requires_auth = true
actor_authorization = ""
```

仓库模板故意使用不可用的示例接口，部署前必须替换：

```toml
[server_config]
url = "https://api.example.com/v1/responses"
model_name = "your-model-name"
wire_api = "responses"
supports_vision = true
api_key_env = "OPENAI_API_KEY"
requires_auth = true
actor_authorization = ""
max_output_tokens = 1200
request_timeout_secs = 60
max_retries = 2
```

非回环模型地址必须使用 HTTPS。需要 Bearer Token 的服务应保持 `requires_auth = true`；接口路径不标准时，将 `url` 写成服务商给出的完整 HTTPS 地址。

只有主模型的 `supports_vision = false` 时，才需要配置独立视觉接口：

GitHub Actions 只会在配置了 `VISION_API_URL` Environment Variable 时写入独立视觉配置：

```env
VISION_API_URL=https://vision.example.com/v1/responses
VISION_WIRE_API=responses
VISION_MODEL_NAME=your-vision-model
VISION_REQUIRES_AUTH=true
```

如果视觉接口需要 Bearer Token，在 GitHub `production` Environment Secrets 中增加 `VISION_API_TOKEN`，并保持 `VISION_REQUIRES_AUTH=true`。

```bash
export VISION_API_URL="https://vision.example.com/v1/responses"
export VISION_WIRE_API="responses"
export VISION_MODEL_NAME="your-vision-model"
export VISION_API_TOKEN="你的视觉模型 Token"
export VISION_REQUIRES_AUTH="true"
```

`VISION_WIRE_API=responses` 时，`VISION_API_URL` 可以填写服务根地址，程序会补上 `/responses`；如果服务实际要求 `/v1/responses`，直接填写完整地址即可。若使用旧式 Chat Completions 接口，将其设为 `chat_completions`，程序会发送 `messages[].content` 中的 `image_url` 图片输入。

主模型的 `thinking_mode` 只控制上游推理开关，不是回复协议。`disabled` 会在 Chat Completions 请求中发送
DeepSeek 的 `thinking.type=disabled`，在 Responses 请求中发送 `reasoning.effort=none`；`auto` 不添加该字段，适合不支持这些 DeepSeek 扩展的兼容服务。生产默认的 DeepSeek v4 使用 `disabled`，避免隐藏推理耗尽可见输出预算。

`provider = "auto"` 时，视觉主模型优先直接接收图片；只有主模型的 `supports_vision = false` 时才启用独立视觉接口。若将 Provider 明确设为 `builtin` 或 `mcp`，则会强制先做独立图片分析，再把文字结果交给主模型。

也可以把视觉识别切换成 MCP Provider。将 `[vision]` 改为 `provider = "mcp"`，填写 `mcp_server` 和 `mcp_tool`，并在对应 MCP 服务的 `allowed_tools` 中加入这个工具。MCP 工具接收的参数固定为：

```json
{
  "question": "用户关于图片的问题",
  "images": [
    {
      "path": "/tmp/kovi-bot-vision-.../image-0.png",
      "mime_type": "image/png",
      "name": "image-0.png"
    }
  ]
}
```

工具需要在调用期间读取这些临时文件，并返回文字分析结果；调用结束后文件会自动删除。`provider = "auto"` 会优先使用现有 `VISION_*` 内置 Provider，失败后再尝试已配置的 MCP Provider。

## 工具与 MCP

芸汐可以在确实需要时自主调用受限工具：

- `time.now`：获取当前时间，支持 `Asia/Shanghai`、`Asia/Tokyo`、`UTC` 等 IANA 时区。
- `memory.search`：查询当前私聊对象或当前群的长期记忆，范围由程序强制决定。
- `web.search`：搜索公开网页；配置 `BRAVE_SEARCH_API_KEY` 时优先使用 Brave Search，失败后依次使用 Bing、DuckDuckGo HTML 兜底。
- `web.fetch`：读取公开网页正文，只允许 HTTP/HTTPS，拒绝本机、内网 IP、内网 DNS 解析和自动重定向。
- `news.search`：按主题和最近天数搜索新闻，可限制来源域名；定时新闻任务优先使用它。
- `weather.current` / `weather.forecast`：通过公开天气服务查询地点的当前天气和未来预报。
- `calculator`：在本地执行受限数学表达式，不执行命令、代码或文件操作。
- `help.commands`：管理员明确询问可用指令时，返回当前帮助内容。
- `system.info`：管理员查询运行时间、适配器、数据库、Redis、模型和配置状态。
- `group.pause` / `group.resume`：管理员在群聊中让芸汐暂停或恢复当前群的自动回复。
- `group.message.targets` / `group.message.send`：主管理员私聊专用，解析已授权群并执行持久化、可审计的跨群发言；在“去群里问一下，等回复后告诉我”这类请求中，`group.message.send` 还会创建持久化收集任务。
- `group.question.status` / `group.question.cancel`：主管理员私聊专用，查询跨群问答的收集进度或在私聊汇报开始前取消任务。
- `agent.run.create` / `agent.run.status` / `agent.run.cancel`：主管理员私聊专用，创建、查看和取消可跨重启持续运行的受限任务；当前首个执行器是 URL 条件监测。
- `health.check`：管理员专用，检查模型鉴权、数据库、Redis、工具注册表和 readiness 状态。
- `mcp.<服务名>.<工具名>`：来自配置白名单的 MCP 工具。

模型最多连续调用有限轮次，工具参数、超时、结果长度和工具名称都由程序校验。工具返回内容会被标记为资料，不会被当成新的系统指令。MCP 目前使用 stdio 子进程传输；服务必须在 `tools.mcp_servers` 中配置，工具必须列入 `allowed_tools`。`read_only = true` 时会拒绝 MCP 明确标记为破坏性或名称带常见写操作动词的工具。MCP 子进程只继承 `PATH` 和 `inherit_env` 明确列出的变量，不会拿到主进程的整套密钥。修改 `bot.conf.toml` 后需要重启机器人。

持久化定时任务支持固定消息和通用 `task` 动作。`task` 会在到期时重新执行保存的自然语言指令，新闻摘要只是其中一个普通例子；默认可使用时间、网页和当前会话记忆工具。若要让定时任务调用 MCP，必须在对应服务上额外设置 `allow_scheduled = true`，只授权可信且必要的工具。定时任务不能创建、查看或取消其他定时任务，也不是任意代码执行器，完整说明见 [`docs/reminders.md`](docs/reminders.md)。

### 持续 Agent Run

主管理员可以在私聊中说“每隔 30 秒请求 `https://example.com/health`，直到 JSON `/status` 等于 `ready` 后告诉我”。芸汐会创建持久化 Run，立即执行第一次检查，未命中时按间隔继续，命中、到期、达到最大次数或连续失败时私聊通知。支持 `text_contains`、`text_not_contains`、`text_equals`、`status_equals` 和 `json_pointer_equals`；URL 只允许公网标准端口 HTTP/HTTPS GET，并沿用网页工具的 DNS 固定、内网拦截、禁重定向、超时和响应大小限制。

调度器不是固定高频扫描：当前进程内创建和重排通过事件立即唤醒，数据库只按最近 `next_wake_at` 自适应等待，并以 `recovery_scan_secs` 做跨实例与崩溃恢复兜底。每次 `http.get` 都写入动作日志，状态转换写入事件日志；最终 QQ 通知属于不可逆动作，发送前先落库为 `sending`，超时或进程中断后只标记投递结果不确定，不会自动重发。完整状态机、限制和扩展边界见 [`docs/agent-runs.md`](docs/agent-runs.md)。

“在群里只回复你喜欢回复的”属于长期行为策略，不是不断执行的定时 Run。策略层应在每条入站群消息事件上做偏好和权限决策；Run Runtime 则负责有开始、资源预算、截止条件和取消入口的持续工作。两者后续可以共享 Action 能力目录，但不会混成同一种任务。

### 跨会话角色动作

主管理员可以在私聊中自然地让芸汐去已授权群发一条纯文本消息，例如“去 123456 群说今晚八点开会”。目标使用群名或简称时，她会先读取机器人当前仍然加入的授权群；只有唯一匹配才会继续，歧义时会询问。普通管理员、群聊上下文和定时任务均不能获得跨群发送工具。

每条来源私聊消息最多绑定一个跨群动作，主管理员每分钟最多发起 5 次。动作会先写入 PostgreSQL 的 `kovi_bot_agent_goals`，发送前再次检查会话 ticket 和群白名单，成功后保存 OneBot 消息 ID 并写入目标群上下文。模型只有收到真实的 `completed` 结果后才能确认已发送；进程中断的未知动作不会自动重放，避免重复发言。

### 跨群问答闭环

主管理员可以直接说“去开发群问一下今晚谁有空，等十分钟后把结果告诉我”。芸汐会先在唯一匹配的授权群里发出问题，随后在限定时间内收集成员文字回复；达到 `min_valid_replies` 条有效回复并安静 `quiet_period_secs` 秒后会提前汇总，否则到最长等待时间再汇总，结果发回主管理员私聊。引用群问题、@芸汐或与问题有明确文字关联的消息会优先收集，普通命令、纯闲聊和无关消息会被过滤。默认每个目标群同时只允许一个收集任务；等待分钟数、回复数量、单条回复长度和汇报长度都受 `[agent_tasks]` 上限约束。任务创建后可发送 `#群问答` 查看最近任务、`#群问答状态 任务编号` 查看详情、`#取消群问答 任务编号` 取消任务；群问题或私聊汇报正在发送的短暂阶段不可取消。群问题已经发出但后续状态不确定时不会自动重发，私聊汇报开始发送后也不会自动重复发送。

部署时可在 GitHub Actions Secrets 中增加 `BRAVE_SEARCH_API_KEY`。不配置也能搜索，但公共搜索服务可能有频率限制。

群聊中可以使用以下方式触发：

- `@芸汐` 后附截图，并提出问题
- 附图后直接问“芸汐你怎么看”“这个怎么样”或“评价一下”
- 回复她或其他人的截图，再发送“帮我看看这里”
- 将 `#看截图` 与截图放在同一条消息中
- 回复截图后发送 `#看截图`

私聊中，可解析的普通图片会自动进入 Core 视觉流程，以自然聊天为目标理解整体情绪、动作和重点；已学习表情也会携带人工标签参与回复。群聊中，明确 `@` 或文字点名芸汐的普通图片会进入 Core 视觉流程；普通未点名纯图片仍默认视为分享状态，不会因为处于群聊窗口期就自动回复。如果图片附带普通聊天文字，Core 会结合文字和图片内容自然回应；明确提到“看看图片/截图/报错/文字”等识图意图时，才会转为更仔细的内容提取。使用 `#看截图`、`#看图` 或 `#识图` 时，如果当前消息没有图片，私聊会先尝试读取最近图片，仍找不到才提示引用或补发。若她上一条消息明确让对方发图，随后收到的纯图片也会在短时间内自动进入识图流程。先发图片、再在消息合并窗口内补充问题，同样会合并为一次识图请求。

私聊还维护最近 1 小时、最多 8 张图片的本地短期索引，只保存 OneBot 图片引用、消息 ID 和当时附带的少量文字，不保存图片文件，也不写入 PostgreSQL 长期记忆。用户说“刚才那张图”“上面那张截图”时会取最近一条图片消息；说“有猫的那张”“带红色按钮的截图”时，会把最近最多 4 张候选按新到旧重新交给视觉模型结合描述辨认。候选不唯一时会自然确认，不会假装确定；原消息撤回后，对应图片索引也会删除。

群聊和私聊回复时，模型会看到最近消息的消息 ID、发送者和内容，并自行判断是否需要引用某条消息或 @ 某位参与者。不需要时不会强制引用；需要时只在第一条回复气泡中添加引用和 @，后续气泡保持普通消息格式。

单次最多处理 4 张图片，每张限制 10 MB，支持 PNG、JPEG 和 WebP。图片地址会先由机器人下载并转换成本地请求内容，因此视觉模型不需要直接访问 QQ 或 NapCat 的内网地址。

本地修改 `bot.conf.toml` 后需要重启。生产配置由模板和 GitHub `production` Environment
生成；修改模板或 Environment 设置后重新发布，运行中的机器人不提供配置热重载。

可用命令：

- Kovi 管理员：`#帮助`，查看常用指令和权限说明；非管理员发送会保持静默
- 私聊用户：`#删除我的数据`，再次发送 `#删除我的数据 确认` 后删除可直接归属到自己的数据
- Kovi 管理员（群聊）：`#系统信息`、`#健康检查`、`#禁言` / `#结束禁言`
- Kovi 管理员（私聊）：`#mind-status`，查看 Yunxi Mind 版本、状态计数和运行指标
- Kovi 管理员也可以在对应会话中直接说“查看系统信息”“检查健康状态”“暂停本群回复”或“恢复本群回复”；原指令仍然兼容
- Kovi 管理员（群聊）：`#删除本群数据`，再次发送 `#删除本群数据 确认` 后删除本群数据
- Kovi 管理员：`#授权群 群号`、`#取消授权群 群号`、`#授权群列表`；可在私聊中管理群聊白名单，授权后立即生效
- 主管理员：`#授权管理员 QQ号`、`#取消授权管理员 QQ号`、`#授权管理员列表`；新增管理员会写入 PostgreSQL，重启后保留
- 主管理员（私聊）：直接说“去某个已授权群发……”；群名不唯一或正文不明确时会先询问
- Kovi 管理员：引用或附带表情后直接描述含义即可教学，也可使用 `#教芸汐 <表情含义>`
- Kovi 管理员：`#待确认表情`、`#确认表情 编号 含义`、`#驳回表情 编号`、`#忽略表情 编号 [天数]`
- Kovi 管理员：`#看截图` / `#看图` / `#识图`（与截图同发，或回复截图后发送）

除私聊用户删除本人数据外，非管理员发送上述受限命令时保持静默。

新群尚未进入 Kovi 白名单时不会收到群消息，因此请先在机器人私聊中发送
`#授权群 群号`。授权名单保存在 PostgreSQL，重启和后续发布会保留；
`#授权群` 和 `#取消授权群` 在已授权群中也可直接操作当前群。

管理员授权命令只能由主管理员执行；配置文件中的 `admins` 副管理员和主管理员不能通过命令移除。
新增的副管理员会同时加入私聊白名单，授权后即可使用受限管理命令。

## 测试

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo audit --deny warnings
cargo deny --all-features check
ripsecrets --strict-ignore .

DATABASE_URL="postgresql://…" cargo test -p model --lib \
  memory::tests::normalized_postgres_storage_round_trips -- --ignored --exact
DATABASE_URL="postgresql://…" cargo test -p model --lib \
  agent_tasks::tests::postgres_task_reservation_is_atomic_and_event_recording_is_idempotent -- --ignored --exact
DATABASE_URL="postgresql://…" cargo test -p model --lib \
  yunxi::mind_store::tests::postgres_mind_store_contracts_are_durable_bounded_and_atomic -- --ignored --exact
REDIS_URL="redis://127.0.0.1:6379/15" cargo test -p model --lib \
  redis_store::tests::redis_runtime_store_round_trips -- --ignored --exact
```

后三项工具版本固定在 CI 工作流中；本地可使用同版本的 `cargo install --locked --version …`
安装。PostgreSQL 和 Redis 集成测试标记为 ignored；CI 使用独立服务与上述精确测试名
单独执行，本地运行时也必须显式使用 `--ignored --exact` 并提供测试连接地址。

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
