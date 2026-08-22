# Production deployment bootstrap

发布工作流通过 GitHub Secret 中的部署密码登录服务器，并会在发布前同步 systemd 服务和
sudo 规则。服务器只需准备好 `ubuntu` 账号的 sudo 权限，之后 GitHub 负责构建、上传、切换
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
- `MODEL_API_URL`、`MODEL_NAME`、`MODEL_SUPPORTS_VISION`、`MODEL_WIRE_API`。
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
- `MODEL_API_KEY_ENV`：`OPENAI_API_KEY`（默认）或 `BOT_API_TOKEN`。
- `MODEL_REQUIRES_AUTH`：默认 `true`；外部 HTTPS 模型服务应保持启用，以发送 `OPENAI_API_KEY` 的 Bearer Token。
- `DATABASE_URL` 如果不想使用 `POSTGRES_PASSWORD` 自动生成的默认连接串。
- `BRAVE_SEARCH_API_KEY`、`REDIS_URL`：按需配置。

## 5. 发布与回滚

PR 和 `main` 推送先运行 `CI`。只有仓库自身 `main` 分支的 push 通过全部检查后才会触发
生产发布；也可在受保护 Environment 下手动运行。

每次发布都会创建 `releases/<commit-sha>`，再原子切换 `current` 软链接。进程完成数据库
初始化和事件注册后会将当前 SHA 写入 readiness 文件；工作流只有同时看到 systemd active
和匹配的 SHA 才判定成功。失败时会把二进制、配置和环境变量整体切回上一版。上传临时包
总会清理，成功后最多保留最近五个 release（当前版与回滚目标不会被误删）。
