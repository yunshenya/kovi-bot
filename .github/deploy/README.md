# Production deployment bootstrap

发布工作流通过 GitHub Secret 中的部署密码登录服务器，也不会动态安装 systemd 服务。以下
操作只需由服务器管理员在控制台执行一次；之后 GitHub 只负责上传和切换发布版本。

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

`ubuntu` 账号只能无密码执行 `systemctl restart kovi-bot.service`。工作流不修改 systemd、
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

## 4. 配置 GitHub Actions Secrets 和 Variables

建议仍创建名为 `production` 的 Environment，限制只能从 `main` 发布，并启用 required
reviewers。截图中的敏感配置放在仓库级 Actions Secrets：

- `DEPLOY_HOST`、`DEPLOY_PASSWORD`。
- `NAPCAT_ACCESS_TOKEN`、`OPENAI_API_KEY`、`BOT_API_TOKEN`。`NAPCAT_ACCESS_TOKEN` 只要求
  非空，并且必须与 NapCat 配置完全一致。
- `POSTGRES_PASSWORD`：工作流会生成本机 PostgreSQL 的连接串。
- `KOVI_MAIN_ADMIN`：机器人所有者 QQ 号。
- `MODEL_API_URL`、`MODEL_NAME`、`MODEL_SUPPORTS_VISION`、`MODEL_WIRE_API`。
- `MODEL_ACTOR_AUTHORIZATION`、`VISION_API_TOKEN`。
- `VISION_ACTOR_AUTHORIZATION`、`VISION_API_URL`、`VISION_MODEL_NAME`、
  `VISION_REQUIRES_AUTH`、`VISION_WIRE_API`。

以下非敏感配置仍放在 `production` Environment Variables：

- `DEPLOY_USER`：默认 `ubuntu`，当前工作流要求填写 `ubuntu`。
- `DEPLOY_SSH_PORT`：默认 `22`。
- `REMOTE_APP_DIR`：默认 `/home/ubuntu/kovi-bot`，当前工作流要求使用此路径。
- `KOVI_ALLOWED_FRIENDS`：可选，逗号分隔的好友 QQ 号；所有者会自动加入。
- `KOVI_ALLOWED_GROUPS`：可选，逗号分隔的群号；留空即不处理群消息。
- `MODEL_API_KEY_ENV`：`OPENAI_API_KEY`（默认）或 `BOT_API_TOKEN`。
- `MODEL_REQUIRES_AUTH`：默认 `false`。
- `DATABASE_URL` 如果不想使用 `POSTGRES_PASSWORD` 自动生成的默认连接串。
- `BRAVE_SEARCH_API_KEY`、`REDIS_URL`：按需配置。

## 5. 发布与回滚

PR 和 `main` 推送先运行 `CI`。只有仓库自身 `main` 分支的 push 通过全部检查后才会触发
生产发布；也可在受保护 Environment 下手动运行。

每次发布都会创建 `releases/<commit-sha>`，再原子切换 `current` 软链接。进程完成数据库
初始化和事件注册后会将当前 SHA 写入 readiness 文件；工作流只有同时看到 systemd active
和匹配的 SHA 才判定成功。失败时会把二进制、配置和环境变量整体切回上一版。上传临时包
总会清理，成功后最多保留最近五个 release（当前版与回滚目标不会被误删）。
