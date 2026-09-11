# Production deployment bootstrap

生产发布有两条通道，服务端的解包、原子切换、readiness 校验和失败回滚是同一套逻辑：

- **本地快速发布（日常）**：[`scripts/deploy-local.sh`](../../scripts/deploy-local.sh)，在开发机
  上交叉编译后直连服务器上传。跨境的 GitHub Runner 上传太慢，因此这是默认通道。
- **GitHub Actions（兜底）**：`Deploy production` 只支持手动 dispatch，用于本机不可用或需要
  按仓库 Secrets 重新生成生产配置时。

工作流通过 GitHub Secret 中的部署密码登录服务器，并会在发布前同步 systemd 服务和
sudo 规则。服务器只需准备好 `ubuntu` 账号的 sudo 权限，之后发布通道负责构建、上传、切换
版本和重启服务。

## 1. 创建专用应用账号

以下示例与仓库中的 systemd 单元均使用现有的 `ubuntu` 账号和 `/home/ubuntu/kovi-bot`。
部署工作流、systemd 单元和 sudo 规则必须保持这两个值一致。

```bash
sudo install -d -o ubuntu -g ubuntu -m 0750 /home/ubuntu/kovi-bot
sudo install -d -o ubuntu -g ubuntu -m 0700 \
  /home/ubuntu/kovi-bot/incoming /home/ubuntu/kovi-bot/releases /home/ubuntu/kovi-bot/runtime
```

使用 `ubuntu` 账号的 SSH 密码登录服务器，并将密码保存为仓库级 Actions Secret
`DEPLOY_PASSWORD`。由于这是服务器登录密码，建议为自动部署单独创建低权限账号；如果继续
使用 `ubuntu`，至少应限制 SSH 密码登录来源并启用防火墙。

## 2. 准备服务权限

```bash
sudo -v
```

首次发布时工作流会安装并校验 `.github/deploy/kovi-bot.service` 和
`.github/deploy/kovi-bot.sudoers`，随后 `ubuntu` 可以无密码执行
`systemctl restart kovi-bot.service`。工作流不会安装系统软件包。

## 3. PostgreSQL 连接配置

当前服务器使用 PostgreSQL 默认的 `postgres` 用户和 `postgres` 数据库；`public` 是该数据库的
默认 schema。工作流在没有配置 `DATABASE_URL` 时会用 `POSTGRES_PASSWORD` 生成连接串。
`POSTGRES_PASSWORD` 必须是服务器上 `postgres` 用户的实际密码，密码中的特殊字符应在 URL
中进行百分号编码。

如果服务器确实使用了其他数据库名，请配置完整的 `DATABASE_URL`，不要把 schema 名当作数据库名。

对应的 Secret 为：

```text
DATABASE_URL=postgresql://postgres:encoded-password@127.0.0.1:5432/postgres
```

PostgreSQL、Redis 和 NapCat 应仅监听回环地址或受控私网。生产环境还应使用防火墙限制
OneBot、数据库和 Redis 端口。

## 4. 配置 GitHub Actions Secrets 和 Variables

建议仍创建名为 `production` 的 Environment，限制只能从 `main` 发布，并启用 required
reviewers。截图中的敏感配置放在仓库级 Actions Secrets：

- `DEPLOY_HOST`、`DEPLOY_PASSWORD`。
- `DEPLOY_USER`、`DEPLOY_SSH_PORT`、`REMOTE_APP_DIR`：虽然不是敏感信息，也可以按你当前
  的配置方式放在仓库 Secrets；默认分别是 `ubuntu`、`22`、`/home/ubuntu/kovi-bot`。
- `NAPCAT_ACCESS_TOKEN`、`OPENAI_API_KEY`、`BOT_API_TOKEN`。`NAPCAT_ACCESS_TOKEN` 只要求
  非空，并且必须与 NapCat 配置完全一致。
- `POSTGRES_PASSWORD`：服务器 `postgres` 用户密码；工作流会据此生成本机 PostgreSQL 的连接串。
- `KOVI_MAIN_ADMIN`：机器人所有者 QQ 号。
- `MODEL_API_URL`、`MODEL_NAME`、`MODEL_SUPPORTS_VISION`、`MODEL_WIRE_API`、`MODEL_THINKING_MODE`。
  `MODEL_NAME` 必须是
  服务端实际提供的模型 ID（例如 `gpt-5.5`），不能填 `-` 这类占位值。
- `MODEL_ACTOR_AUTHORIZATION`、`VISION_API_TOKEN`。
- `VISION_ACTOR_AUTHORIZATION`、`VISION_API_URL`、`VISION_MODEL_NAME`、
  `VISION_REQUIRES_AUTH`、`VISION_WIRE_API`。

以下配置也可以放在 `production` Environment Variables；如果同时存在，仓库 Secrets 优先：

- `KOVI_ALLOWED_FRIENDS`：可选，逗号分隔的好友 QQ 号；所有者会自动加入。
- `KOVI_ALLOWED_GROUPS`：可选，逗号分隔的群号；生产工作流默认不配置任何群。
  需要让群开始接收消息时，请在机器人私聊中使用 `#授权群 群号`，或在
  `production` Environment Variables 中显式设置初始群列表。
- 运行中的授权群名单由管理员命令维护，保存在 PostgreSQL 的
  `kovi_bot_authorized_groups` 表；首次初始化会从静态群列表迁移，之后以数据库内容为准。
- 运行中的动态副管理员由主管理员命令维护，保存在 PostgreSQL 的
  `kovi_bot_authorized_admins` 表；配置文件中的 `admins` 会在首次初始化时迁移。
- `MODEL_API_KEY_ENV`：`OPENAI_API_KEY`（默认）或 `BOT_API_TOKEN`。
- `MODEL_REQUIRES_AUTH`：默认 `true`；外部 HTTPS 模型服务应保持启用，以发送 `OPENAI_API_KEY` 的 Bearer Token。
- `MODEL_THINKING_MODE`：`disabled`（生产默认，适用于 DeepSeek v4）或 `auto`（不发送供应商特定的推理开关）。
- `DATABASE_URL` 如果不想使用 `POSTGRES_PASSWORD` 自动生成的默认连接串。
- `BRAVE_SEARCH_API_KEY`、`REDIS_URL`：按需配置。

## Intrinsic 模型资产

生产发布包只包含二进制、配置和环境变量，不包含约 400 MB 的 Intrinsic 权重。当前工作流
不会自动下载模型；如果服务器没有预装完整 bundle，`model.intrinsic.asset_dir` 找不到
`manifest.toml` 时会按设计退回 deterministic fallback。

如果生产需要本地 Intrinsic 推理，应从固定 revision 的模型发行地址下载 text 或 full bundle，
校验其中 manifest 的文件大小和 SHA-256，再安装到稳定目录
`/home/ubuntu/kovi-bot/models/yunxi-intrinsic/minimind-3o`。不要把权重放进
`current` 或某个具体的 `releases/<sha>`，也不要直接从可变的 `main`、`latest` 或未经校验的
URL 复制。发布工作流会在每个 release 下创建 `models` 到稳定目录的软链接，因此原子切换、
回滚和旧 release 清理都不会影响已安装的 bundle；升级模型后应重新校验 manifest 并重启服务。
只使用远程 OpenAI-compatible 模型时则无需下载本地 bundle。

本地手动安装可以在源码目录执行：

```bash
./scripts/download-model.sh --variant text
# 或：./scripts/download-model.sh --variant full
```

## 5. 本地快速发布（日常通道）

GitHub Runner 在境外，把 12~15 MB 的 release 包 scp 到国内服务器是跨境传输，慢且不稳定。
日常发布改走 [`scripts/deploy-local.sh`](../../scripts/deploy-local.sh)：在开发机上交叉编译
Linux 二进制，直连服务器上传，服务端的切换与回滚逻辑和 Actions 完全一致。

首次准备（各一次）：

```bash
ssh-copy-id -p 22 ubuntu@<DEPLOY_HOST>      # 安装开发机公钥，之后免密
cat > server-login <<'EOF'                  # 仓库根目录，已被 .gitignore 忽略
DEPLOY_HOST=ubuntu@<DEPLOY_HOST>
DEPLOY_PORT=22
EOF
```

工具链需要 `cargo-zigbuild`、`zig` 与 `x86_64-unknown-linux-gnu` 目标（`rustup target add
x86_64-unknown-linux-gnu`）。注意本地交叉编译产出的二进制要满足服务端的架构断言
（`ELF 64-bit LSB pie executable, x86-64`），脚本会在上传前先校验。

```bash
./scripts/deploy-local.sh                     # 编译 → 打包 → 上传 → 切换 → 等 readiness
./scripts/deploy-local.sh --dry-run           # 只编译打包，不上传
./scripts/deploy-local.sh --no-build          # 复用已有产物，最快
./scripts/deploy-local.sh --password-auth     # 不装公钥，改为输一次服务器密码（连接复用）
./scripts/deploy-local.sh --require-clean     # 工作区有未提交改动就拒绝发布
./scripts/deploy-local.sh --install-service   # 单元/sudo 规则漂移时同步（要交互输入 sudo 密码）
```

约定与边界：

- 发布包只有 `kovi-bot` 与 `REVISION`；`.env`、`bot.conf.toml`、`kovi.conf.toml`、
  `kovi.plugin.toml` 由服务端从上一版 release 继承，并就地改写 `KOVI_DEPLOY_REVISION`，
  所以开发机不需要保存任何生产密钥。
- 因此本地发布**不会**应用 GitHub Secrets 的变化。改模型地址、白名单、Token 等配置时，
  仍要用 Actions 手动发布一次，或直接改服务器上 `current` 里的配置。
- 工作区有未提交改动时 revision 记为 `<sha>-dirty.<时间戳>`，这些改动会进二进制；要严格
  对齐提交请先 commit 或加 `--require-clean`。
- 继承来的配置缺少模板新增键时只打印提示，不会自动补，也不会覆盖服务器上已有的值。
- systemd 单元或 sudo 规则与仓库不一致时只提示；加 `--install-service` 才会同步。

## 6. 发布与回滚（GitHub Actions 兜底）

PR 和 `main` 推送先运行 `CI`。`Deploy production` 不再随 CI 自动触发，只能在受保护
Environment 下手动 dispatch，用于本机不可用时兜底，或需要按仓库 Secrets 重新生成生产配置。

每次发布都会创建 `releases/<commit-sha>`，再原子切换 `current` 软链接。进程完成数据库
初始化和事件注册后会将当前 SHA 写入 readiness 文件；发布通道只有同时看到 systemd active
和匹配的 SHA 才判定成功。失败时会把二进制、配置和环境变量整体切回上一版。上传临时包
总会清理，成功后最多保留最近五个 release（当前版与回滚目标不会被误删）。
