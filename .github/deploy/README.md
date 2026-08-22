# Production deployment bootstrap

发布工作流不会持有服务器登录密码，也不会动态安装 systemd 服务。以下操作只需由服务器
管理员在控制台执行一次；之后 GitHub 只使用专用 SSH Key 和一条受限 sudo 规则发布。

## 1. 创建专用应用账号

以下示例与仓库中的 systemd 单元均使用 `/opt/kovi-bot` 和 `kovi-bot`。如果修改路径或
账号，需同步修改 systemd 单元与 GitHub Environment 变量。

```bash
sudo useradd --system --create-home --home-dir /opt/kovi-bot --shell /bin/bash kovi-bot
sudo passwd --lock kovi-bot
sudo -u kovi-bot install -d -m 700 /opt/kovi-bot/.ssh
sudo -u kovi-bot install -d -m 700 \
  /opt/kovi-bot/incoming /opt/kovi-bot/releases /opt/kovi-bot/runtime
```

在可信机器生成独立部署密钥，将私钥保存为 GitHub `production` Environment Secret
`DEPLOY_SSH_PRIVATE_KEY`。公钥以 `restrict` 选项写入：

```text
restrict ssh-ed25519 AAAA... github-kovi-deploy
```

将该行保存到 `/opt/kovi-bot/.ssh/authorized_keys`，所有者设为 `kovi-bot:kovi-bot`，
权限设为 `0600`。`restrict` 会关闭端口、Agent、X11 和 PTY 转发；不要复用个人 SSH Key。

从服务器控制台核对 SSH 主机公钥指纹，再把对应的完整 `known_hosts` 行保存为 Environment
Secret `DEPLOY_KNOWN_HOSTS`。不要在 CI 内无条件信任 `ssh-keyscan` 的结果。

## 2. 安装受限服务权限

```bash
sudo install -o root -g root -m 0644 \
  .github/deploy/kovi-bot.service /etc/systemd/system/kovi-bot.service
sudo install -o root -g root -m 0440 \
  .github/deploy/kovi-bot.sudoers /etc/sudoers.d/kovi-bot-deploy
sudo visudo -cf /etc/sudoers.d/kovi-bot-deploy
sudo systemctl daemon-reload
sudo systemctl enable kovi-bot.service
```

部署账号只能无密码执行 `systemctl restart kovi-bot.service`。工作流不修改 systemd、
sudoers 或系统软件包。

## 3. 创建最小权限数据库

不要让机器人使用 PostgreSQL 超级用户。以下示例创建独立数据库及其所有者；密码应使用
强随机值，并在 URL 中进行百分号编码。

```sql
CREATE ROLE kovi_bot LOGIN PASSWORD 'replace-with-a-strong-password'
  NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;
CREATE DATABASE kovi_bot OWNER kovi_bot;
```

对应的 Secret 为：

```text
DATABASE_URL=postgresql://kovi_bot:encoded-password@127.0.0.1:5432/kovi_bot
```

PostgreSQL、Redis 和 NapCat 应仅监听回环地址或受控私网。生产环境还应使用防火墙限制
OneBot、数据库和 Redis 端口。

## 4. 配置 GitHub Environment

创建名为 `production` 的 Environment，限制只能从 `main` 发布，并建议启用 required
reviewers。配置以下 Variables：

- `DEPLOY_HOST`：服务器 DNS 名称或 IPv4 地址。
- `DEPLOY_USER`：默认 `kovi-bot`。
- `DEPLOY_SSH_PORT`：默认 `22`。
- `REMOTE_APP_DIR`：默认 `/opt/kovi-bot`，必须是 `/opt` 或 `/srv` 下的具体目录。
- `KOVI_MAIN_ADMIN`：机器人所有者 QQ 号。
- `KOVI_ALLOWED_FRIENDS`：可选，逗号分隔的好友 QQ 号；所有者会自动加入。
- `KOVI_ALLOWED_GROUPS`：可选，逗号分隔的群号；留空即不处理群消息。
- `MODEL_API_URL`：HTTPS 模型接口完整地址。
- `MODEL_NAME`：模型名称。
- `MODEL_WIRE_API`：`responses`（默认）或 `chat_completions`。
- `MODEL_SUPPORTS_VISION`：默认 `true`。
- `MODEL_API_KEY_ENV`：`OPENAI_API_KEY`（默认）或 `BOT_API_TOKEN`。
- `MODEL_REQUIRES_AUTH`：默认 `true`。
- `VISION_API_URL`、`VISION_WIRE_API`、`VISION_MODEL_NAME`、
  `VISION_REQUIRES_AUTH`：仅独立视觉接口需要。

配置以下 Secrets：

- `DEPLOY_SSH_PRIVATE_KEY`、`DEPLOY_KNOWN_HOSTS`。
- `NAPCAT_ACCESS_TOKEN`：至少 24 个随机安全字符，并与 NapCat 完全一致。
- `DATABASE_URL`、`OPENAI_API_KEY`。
- `BOT_API_TOKEN`、`MODEL_ACTOR_AUTHORIZATION`、`VISION_API_TOKEN`、
  `VISION_ACTOR_AUTHORIZATION`、`BRAVE_SEARCH_API_KEY`、`REDIS_URL`：按需配置。

旧的 `DEPLOY_PASSWORD` 和 `POSTGRES_PASSWORD` 不再使用。迁移完成并验证新流程后，应从
GitHub 删除这两个 Secret，并轮换曾用于自动发布的服务器密码。

## 5. 发布与回滚

PR 和 `main` 推送先运行 `CI`。只有仓库自身 `main` 分支的 push 通过全部检查后才会触发
生产发布；也可在受保护 Environment 下手动运行。

每次发布都会创建 `releases/<commit-sha>`，再原子切换 `current` 软链接。进程完成数据库
初始化和事件注册后会将当前 SHA 写入 readiness 文件；工作流只有同时看到 systemd active
和匹配的 SHA 才判定成功。失败时会把二进制、配置和环境变量整体切回上一版。上传临时包
总会清理，成功后最多保留最近五个 release（当前版与回滚目标不会被误删）。
