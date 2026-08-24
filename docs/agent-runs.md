# 持续 Agent Run

Agent Run 用于需要跨多次唤醒继续推进的目标，不依赖当前模型调用或聊天窗口继续存活。当前第一个
执行器是 `url_watch`，后续内置 Action 或 MCP 可以复用同一生命周期。

## 使用方式

首版只允许主管理员在私聊中创建、查看和取消：

- “每隔 30 秒请求 `https://example.com/health`，直到正文包含 ready 后告诉我”
- “每分钟检查这个接口，HTTP 状态变成 200 就通知我，最多盯两小时”
- “轮询这个 JSON，等 `/deploy/state` 等于 `finished` 后告诉我”
- “刚才的接口监测到哪了？”
- “把 Run 12 停掉”

创建工具是 `agent.run.create`，状态和取消工具分别是 `agent.run.status` 与
`agent.run.cancel`。第一次检查立即执行。用户没有指定资源边界时使用 `[agent_runs]` 默认截止时间
和默认最大执行次数，但数据库中的每个 Run 始终同时具有这两个上限。

## 状态机

`active -> running -> active` 表示一次未命中的安全检查；命中或达到终止条件后进入
`notifying`，通知成功后成为 `completed`、`expired` 或 `failed`。用户可以取消 `active` 或
`running`，但不能撤销已经进入 `notifying` 的不可逆发送。

每次状态转换写入 `kovi_bot_agent_run_events`，每次能力调用写入
`kovi_bot_agent_run_actions`。能力名使用开放字符串，例如 `http.get` 和
`private.message.send`；动作同时标记 `read_only` 或 `irreversible`。worker 使用 PostgreSQL
租约和 `FOR UPDATE SKIP LOCKED` 领取任务，多实例不会正常并发执行同一个 tick。

最终通知采用 at-most-once 闸门：网络发送前先把 Run 和 Action 提交为 `sending`。如果发送超时、
进程退出或数据库回写失败，恢复逻辑会标记结果不确定，并且不自动重放。这样可能少发一次，但不会因为
无法判断外部副作用是否已经发生而重复私聊。

## 调度与延迟

同一进程内创建、取消和重排会通过 Tokio `Notify` 立即唤醒调度器。空闲时调度器查询最近的
`next_wake_at` 并睡到该时间；`recovery_scan_secs` 只是多实例写入和崩溃租约恢复的最长兜底窗口，
不是高频数据库轮询间隔。

聊天入站消息仍由消息事件直接触发，不受 URL 检查间隔影响。外部接口轮询最短默认限制为 5 秒，原因是
它消耗第三方网络和数据库资源；把人类反应时间直接当作所有外部能力的轮询周期会造成无意义负载。

## 网络边界

`url_watch` 只执行 GET，并复用网页工具的 URL 校验：

- 只允许 HTTP/HTTPS 标准端口，禁止 URL 凭据。
- 拒绝本机、私网、保留地址和内部域名。
- DNS 解析后固定公网地址，关闭自动重定向，降低 DNS rebinding 和跳转绕过风险。
- 每次请求受总超时和响应字节数限制。
- 文本与 JSON 条件只在 2xx 响应上匹配；要等待 3xx、4xx 或 5xx 必须使用 `status_equals`。

支持的条件为 `text_contains`、`text_not_contains`、`text_equals`、`status_equals` 和
`json_pointer_equals`。JSON 条件执行结构化解析和精确值比较，不执行响应中的代码或指令。

## 扩展边界

新增执行能力时，应先注册 capability、参数 schema、权限和 effect class，再由具体 Run kind 编排，
不能让保存的自然语言直接绕过工具校验。MCP 写操作必须单独授权，并根据外部系统幂等能力决定是否允许
恢复重试。

长期聊天行为（例如“在这个群只回复你真正想回复的消息”）属于持久策略。它应由每条入站消息事件触发
决策，而不是创建一个永久循环 Run。策略可以调用相同的 Action 目录，但需要独立的作用域、优先级、
撤销和审计模型。
