# 自托管聊天机器人 Web 管理后台 · UI/UX 设计参考报告

> 参考产品：**A) NapCat WebUI**（NapCatQQ 的 Web 管理面板，默认端口 6099）、**B) Hindsight**（Vectorize 的 Agent Memory 产品，记忆/知识页展示参考）
>
> 用途：为 kovi-bot 的「配置页 + 记忆页」自托管 Web 管理后台提供可直接照搬的设计依据。

---

## 0. 证据等级与方法说明（先读这一节）

本报告**不是**从博客/截图推测出来的。两个产品的**前端与后端源码都是开源的**，因此绝大部分结论来自一手源码。

| 等级 | 标记 | 含义 |
| --- | --- | --- |
| 一手源码 | `[源码]` | 直接读到的实现代码/配置/文案键值，最可靠 |
| 官方文档 | `[官方文档]` | 项目官方文档站 |
| 官方博客/截图 | `[官方截图]` | 官方博客正文与配图，我逐张读图核对 |
| 未能验证 | `[推测]` | 没有找到证据，标注为推测 |

**固定引用版本（所有 GitHub 链接都钉在这两个 commit 上，可复现）：**

- NapCatQQ：`109d0c1dff755875f3b79795e99cee6115289fbb`
  链接前缀 `NC = https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb`
- Hindsight：`bde55237f53bf55aacd048b01e29d7dc23b83a85`（`hindsight-control-plane` v0.9.2）
  链接前缀 `HS = https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85`

**关键澄清（会影响照搬决策）：**

1. NapCat WebUI 前端是 **React 19 + HeroUI + Tailwind**，**不是 Vue / Naive UI / Element Plus**。很多中文博客说它是 Vue 系，这是错的，源码可证。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/package.json)
2. Hindsight 的 `hindsight-control-plane` 是 **Next.js 16 + React 19 + Tailwind v4 + shadcn/ui + Radix**，且**其完整设计系统 token（含全部 hex 色值）写在 `globals.css` 里**，可直接抄色板。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css)
3. 官方文档关于「WebUI 默认弱口令 `napcat`」的说法**已经过时**：新版本源码里 `token === 'napcat'` 或为空时会**随机生成 8 位 token** 并写回配置。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/index.ts) + [官方文档](https://napneko.github.io/other/security)

---

## 1. NapCat WebUI 登录与鉴权（可照搬的做法 + 具体字段名/端点名）

### 1.1 配置文件 `webui.json`：字段全集

源码中的默认文件（注意比官方文档多出 3 个访问控制字段）：[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/webui.json)

```json
{
  "host": "0.0.0.0",
  "port": 6099,
  "prefix": "",
  "token": "random",
  "loginRate": 3,
  "accessControlMode": "none",
  "ipWhitelist": [],
  "ipBlacklist": [],
  "enableXForwardedFor": false
}
```

| 字段 | 语义 | 证据 |
| --- | --- | --- |
| `host` | 监听地址，默认 `0.0.0.0`；不可用时 WebUI 被禁用 | [官方文档](https://napneko.github.io/config/basic) |
| `port` | 默认 6099；设为 `0` 完全禁用 WebUI；被占用时自动 +1（最多 100 次） | [官方文档](https://napneko.github.io/config/basic) |
| `prefix` | 配置文件中仍存在，但 **v4.4 之后不再支持**（遗留字段） | [官方文档](https://napneko.github.io/config/basic) |
| `token` | 登录密钥，`"random"`/空/`"napcat"` 触发随机生成 | [源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/index.ts) |
| `loginRate` | **每分钟每 IP 登录次数上限**，默认 3 | [官方文档](https://napneko.github.io/config/basic) |
| `accessControlMode` / `ipWhitelist` / `ipBlacklist` / `enableXForwardedFor` | IP 访问控制 | [源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/webui.json) |
| `enable2FA` / `totpSecret` | 由启用 2FA 时写入 | [源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts) |

### 1.2 Token 生成与下发（可照搬的「首次上手」体验）

启动逻辑 `[源码]`（[index.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/index.ts)）：

1. 若存在环境变量 `NAPCAT_WEBUI_SECRET_KEY`，**强制覆盖**配置里的 token。
2. 否则若 `config.token === 'napcat'` 或为空 → `getRandomToken(8)` 生成随机 token，**写回 `webui.json`**，并把它挂到 `pendingTokenToSend`，等 QQ 登录成功后**私聊发给机器人自己**。
3. 把 token 缓存进模块级变量 `initialWebUiToken`（`setInitialWebUiToken`），**之后的鉴权一律用这个缓存值**，不重新读文件——注释明确说明是为了避免「运行时手改密码导致会话混乱」。
4. 启动日志打印：`[NapCat] [WebUi] WebUi Token: ${token}`。

官方文档对应的用户可见行为：启动日志里会出现
`[info] [NapCat] [WebUi] WebUi User Panel Url: http://127.0.0.1:6099/webui?token=xxxxx`，也可直接打开 `webui.json` 查 token。`[官方文档]`（[basic](https://napneko.github.io/config/basic)）

> **可照搬**：首次启动生成随机 token → 写回配置文件 → 同时打印到启动日志 → 并私聊推送一次。用户不需要「先有一个默认密码」这个不安全中间态。二维码登录后会刷新 token，前端会强制要求改密码，否则禁用大部分功能。`[官方文档]`（[basic](https://napneko.github.io/config/basic)）

### 1.3 登录请求：token 从不明文传输

前端 `[源码]`（[webui_manager.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/controllers/webui_manager.ts)）：

```ts
const sha256 = CryptoJS.SHA256(token + '.napcat').toString();
await serverRequest.post('/auth/login', { hash: sha256, totpCode });
```

后端 `[源码]`（[Auth.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts)）用同一算法校验：`sha256(password + '.napcat')`（见 [SignToken.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/helper/SignToken.ts) 的 `generatePasswordHash`）。

**端点表（全部 `[源码]` 自 [Auth.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts) 与 [webui_manager.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/controllers/webui_manager.ts)）：**

| 端点 | 方法 | 入参 | 出参 / 错误文案 |
| --- | --- | --- | --- |
| `/auth/login` | POST | `{ hash, totpCode? }` | `{ Credential }` 或 `{ require2FA: true, message }`；错误：`token is empty` / `login rate limit` / `token is invalid` / `Invalid or expired code` |
| `/auth/check` | POST | — | 校验 Authorization 头；错误：`Token has been revoked` / `Authorization Failed` |
| `/auth/logout` | POST | — | `Logged out successfully`（把凭证加入黑名单） |
| `/auth/update_token` | POST | `{ oldToken, newToken }` | 见 1.7 |
| `/auth/2fa/status` | GET | — | `{ enable2FA, hasSecret }` |
| `/auth/2fa/generate-secret` | POST | — | `{ secret, qrCodeUrl }`（issuer `NapCat WebUI` / 账号 `NapCat`） |
| `/auth/2fa/enable` | POST | `{ secret, totpCode }` | `2FA enabled` |
| `/auth/2fa/disable` | POST | `{ totpCode }` | 关闭也**必须**验一次 TOTP |
| `/auth/passkey/generate-authentication-options` | POST | — | WebAuthn 认证选项 |
| `/auth/passkey/verify-authentication` | POST | `{ response }` | `{ Credential }` |

### 1.4 凭证（Credential）结构与生命周期

`[源码]`（[SignToken.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/helper/SignToken.ts)）：

```ts
{ Data: { CreatedTime: <秒级时间戳>, HashEncoded: <sha256(token+'.napcat')> },
  Hmac: <HMAC-SHA256(secretKey, JSON.stringify(Data))> }
```

- 整个 JSON 再 **base64** 作为 `Credential` 返回。
- `secretKey = process.env['NAPCAT_WEBUI_JWT_SECRET_KEY'] || Math.random().toString(36).slice(2)` —— **进程级随机**，因此**重启后所有会话立即失效**。这正是官方文档所说的「基于时间的 HMAC 密钥动态生成机制」`[官方文档]`（[security](https://napneko.github.io/other/security)）。
- `MAX_CREDENTIAL_VALID_SECONDS = 3600` → 凭证**有效期 1 小时**，且校验 `timeDifference >= 0`（拒绝未来时间戳）。
- 登出：把该凭证的 HMAC 写入黑名单 `revoked:<hmac>`，TTL 3600 秒。

### 1.5 三处 token 校验的差异（重要，容易踩坑）

| 位置 | 读的是哪个 token | 证据 |
| --- | --- | --- |
| REST 中间件 | `getInitialWebUiToken()`（内存缓存） | [middleware/auth.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/middleware/auth.ts) |
| 登录接口 | `getInitialWebUiToken()` | [api/Auth.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts) |
| 终端 WebSocket | `config.token`（每次读配置） | [terminal_manager.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/terminal/terminal_manager.ts) |

### 1.6 鉴权如何携带：`Authorization` 头 + `webui_token` 查询参数

`[源码]`（[middleware/auth.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/middleware/auth.ts)）中间件按顺序取：

1. `req.headers.authorization` → 按空格切分取 `[1]`，即 **`Authorization: Bearer <base64凭证>`**；
2. 否则 `req.query['webui_token']` → 支持 `?webui_token=<base64凭证>`；
3. 两者都没有 → `Unauthorized`。

**白名单（免鉴权路径）**：`/auth/login`、`/auth/passkey/generate-authentication-options`、`/auth/passkey/verify-authentication`。

前端注入头的方式 `[源码]`（[utils/request.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/utils/request.ts)）：

```ts
config.headers['Authorization'] = `Bearer ${JSON.parse(token)}`;
```

`webui_token` 的实际用途可验证：插件图标 URL 会带上它（`url.searchParams.set('webui_token', token)`），因为 `<img>` 无法设置请求头。`[源码]`（[plugin_card.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/display_card/plugin_card.tsx)）

### 1.7 存储位置：**localStorage，不是 Cookie**

`[源码]`：

- Key 名就是字符串 `'token'`，见 [const/key.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/const/key.ts) 的 `enum key { token = 'token', theme = 'theme', ... }`。
- 写入用 `useLocalStorage<string>(key.token, '')`（`@uidotdev/usehooks`，**JSON 序列化**存储，所以读取时要 `JSON.parse`）。
- 登录态判定极简 `[源码]`（[hooks/auth.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/hooks/auth.ts)）：`isAuth: !!token` —— 只看本地有没有值，**不在路由守卫里同步请求后端**。
- **全局 401 处理** `[源码]`（[utils/request.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/utils/request.ts)）：响应 `code !== 0` 且 `message === 'Unauthorized'` → `localStorage.removeItem('token')` + `window.location.reload()`。统一响应信封是 `{ code, message, data }`，成功 `code === 0`。

### 1.8 限流实现（可直接照搬的粒度）

`[源码]`（[helper/Data.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/helper/Data.ts) `checkLoginRate`）：

```
key = `login_rate:${ip}`         // 按客户端 IP 计数
count === 0 → set(key, 1, 60s)   // 首次，60 秒窗口
count >= RateLimit → false        // 拒绝：'login rate limit'
否则 count+1
```

- IP 取自 `req.ip || req.socket.remoteAddress`。
- 普通登录和 Passkey 认证**都**走这个限流。
- 窗口是**每 60 秒**，不是滑动窗口。`[推测]`（60 秒固定 TTL 是我从 `store.set(key,1,60)` 读出的实现语义，源码未写「固定窗口」字样。）

### 1.9 改密码的校验规则（值得直接抄的强度策略）

`[源码]`（[Auth.ts `UpdateTokenHandler`](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts)）：

- `newToken` 不能为空；**必须提供 `oldToken`**；
- 新旧不能相同（`新密码不能与旧密码相同`）；
- 长度 ≥ 6（`新密码至少需要6个字符`）；
- 必须含字母（`/[a-zA-Z]/`）且含数字（`/[0-9]/`）；
- 成功后：① 注销当前凭证 → ② 更新配置文件 → ③ **同步刷新内存缓存**，使新密码立即生效。

### 1.10 登录页长什么样（逐项，来自源码）

`[源码]`（[web_login.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/web_login.tsx)）：

| 元素 | 实现 |
| --- | --- |
| 页面标题 | `<title>WebUI登录 - NapCat WebUI</title>` |
| 布局 | `PureLayout`：`h-screen` 垂直居中；右上角固定一个 GitHub 图标外链（Tooltip「查看WebUI源码」）[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/layouts/pure.tsx) |
| 卡片 | `HoverEffectCard`，宽度 `w-[608px] max-w-full`，带 **3D 倾斜跟随鼠标**（`maxXRotation={3} maxYRotation={3}`），入场 spring 动画（`opacity 0→1, y 20→0, scale 0.95→1`） |
| 主标题 | `Web` + 紫色变体 `Login`（样式基元 `title({ color: 'violet' })`），左侧 logo 图 |
| 主题开关 | 绝对定位 `absolute right-4 top-4`，日/月图标 |
| 唯一输入框 | **没有用户名**。`<Input type='password' label='Token' placeholder='请输入token'>`，前置钥匙图标 `IoKeyOutline`，`isClearable`、`size='lg'`、`radius='lg'`；`autoComplete='current-password'` |
| 浏览器密码管理器兼容 | 有一个 **隐藏的 `username` 输入**，值硬编码为 `napcat-webui`，`className='absolute -left-[9999px] opacity-0 pointer-events-none'`、`tabIndex={-1}`、`readOnly` |
| 提示文案 | `💡 提示：请从 NapCat 启动日志中查看登录密钥` |
| 主按钮 | `color='primary'` `radius='full'` `size='lg'` `variant='shadow'`，按钮内嵌 logo 图 + 文案「登录」 |
| 输入框视觉 | `shadow-xl` + `bg-default-100/70` + `backdrop-blur-xl` + `backdrop-saturate-200`，聚焦态 `group-data-[focus=true]:bg-default-100/50` |
| 全局回车提交 | `document.addEventListener('keydown')`，Enter 直接提交（Token 步骤或 2FA 步骤按状态分派） |
| Passkey 自动登录 | 进入页面若无 `?token=`，先显示「🔐 正在检查Passkey...」并自动 `navigator.credentials.get()`；失败则静默回落到 Token 输入 |
| URL token 自动登录 | `?token=xxx` 存在时**跳过 Passkey 检查**并立即提交（源码注释说明：避免登录失败后输入框被永久禁用） |
| 2FA 步骤 | 右上返回箭头（`IoArrowBack`）→ 文案「请输入Authenticator中的验证码」→ 6 位数字输入（`maxLength=6`、`inputMode='numeric'`、`pattern='[0-9]*'`、`\D` 过滤）→ 按钮「验证」 |
| 错误提示 | `react-hot-toast` 的 `toast.error(...)` |

### 1.11 终端 WebSocket 的鉴权（独立于 REST 中间件）

`[源码]`：

- 前端把当前页面的协议从 `http` 换成 `ws`，路径 `/api/ws/terminal`：
  `url.searchParams.set('token', token)`，然后 `new WebSocket(url)`（[terminal_manager.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/controllers/terminal_manager.ts)）
- 后端在 `ws` 服务器的 `verifyClient` 回调里取 `url.searchParams.get('token')`，base64 解码后 `AuthHelper.validateCredentialWithinOneHour(config.token, Credential)`，失败 `cb(false, 401, 'Unauthorized')`（[terminal_manager.ts](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/terminal/terminal_manager.ts)）

> **照搬要点**：WebSocket 无法自定义请求头，所以必须提供 `?token=` / `?webui_token=` 这条 query 通道；且**必须在握手阶段（verifyClient）拒绝**，不要等连接建立后再发一条错误消息。

### 1.12 其他安全设计（官方文档列举）

`[官方文档]`（[security](https://napneko.github.io/other/security)）：

- 鉴权过程全程 SHA256 加盐处理；
- 按来源 IP 限速防爆破；
- HMAC 密钥动态生成，重启即失效；
- 端口设为 0 可完全关闭面板，之后可安全删除 `./static/`；
- **删除 `./pty/` 目录即可完全禁用终端控制功能**；
- 支持 SSL：在 `./config/` 放 `cert.pem` 与 `key.pem` 并重启；
- v4.8.106 起取消固定默认密码。

---

## 2. NapCat WebUI 视觉与布局

### 2.1 技术栈（实测依赖）

`[源码]`（[package.json](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/package.json)）：

| 层 | 选型 |
| --- | --- |
| 框架 | **React 19** + `react-dom` 19（`App.tsx` / `main.tsx`） |
| 构建 | **Vite 6** + `@vitejs/plugin-react` + TypeScript 5.7 |
| 样式 | **Tailwind CSS 3.4** + `tailwind-variants` + `clsx` + `tailwind-merge` |
| 组件库 | **HeroUI**（`@heroui/*`，约 30 个包：button/card/input/select/switch/table/tabs/modal/navbar/pagination/slider/chip/kbd/snippet/skeleton/spinner/tooltip/dropdown/listbox/popover/form/checkbox/divider/image/link/avatar/breadcrumbs/accordion/code） |
| 路由 | `react-router-dom` v7（lazy + Suspense） |
| 状态 | `@reduxjs/toolkit` + `react-redux`；数据请求 `ahooks` 的 `useRequest` |
| 表单 | `react-hook-form` + Controller + `zod` + `@sinclair/typebox` |
| 动画 | `motion`（Framer Motion 新包名），入场/侧边栏 spring |
| 图标 | `react-icons`（导航用 `lu` = Lucide 系列）、`qface`（QQ 表情） |
| 终端 | `@xterm/xterm` + `addon-canvas` / `addon-fit` / `addon-web-links` |
| 编辑器 | CodeMirror（`@uiw/react-codemirror` + json/js/css 语言包 + `one-dark` 主题）与 Monaco loader（配置编辑/文件编辑） |
| 内容渲染 | `react-markdown` + `remark-gfm`、`quill`（富文本）、`qrcode.react`（登录二维码）、`react-photo-view`（图片查看）、`react-window`（长列表虚拟化） |
| 反馈 | `react-hot-toast`；`react-error-boundary` 做页面级兜底 |
| 实时 | `react-use-websocket`、`event-source-polyfill`（SSE） |
| 加密 | `crypto-js`（登录时算 SHA256） |

**产物视觉语言**：HeroUI 的「圆角胶囊 + 毛玻璃 + 柔和阴影」风格（radius 默认 0.75rem，见下），叠加自定义的樱花粉主色与可自定义背景图，整体偏「二次元萌系工具面板」，与 Naive UI / Element Plus 的「方正企业后台」观感明显不同。

### 2.2 配色（hex 全量，来自 Tailwind 主题配置）

`[源码]`（[tailwind.config.js](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/tailwind.config.js)）：

**浅色（light）**

| 语义 | DEFAULT | 50 | 100 | 200 | 300 | 400 | 500 | 600 | 700 | 800 | 900 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `primary`（注释：樱花粉） | `#FF7FAC` | `#FFF0F5` | `#FFE4E9` | `#FFCDD9` | `#FF9EB5` | `#FF7FAC` | `#F33B7C` | `#C92462` | `#991B4B` | `#691233` | `#380A1B` |
| `secondary`（注释：冰霜蓝） | `#88C0D0` | `#F0F9FC` | `#D7F0F8` | `#AEE1F2` | `#88C0D0` | `#5E9FBF` | `#4C8DAE` | `#3A708C` | `#2A546A` | `#1A3748` | `#0B1B26` |
| `danger` | `#DB3694` | `#FEEAF6` | `#FDD7DD` | `#FBAFC4` | `#F485AE` | `#E965A3` | `#DB3694` | `#BC278B` | `#9D1B7F` | `#7F1170` | `#690A66` |

**深色（dark）**：`primary` DEFAULT `#f31260`（50~900：`#310413` `#610726` `#920b3a` `#c20e4d` `#f31260` `#f54180` `#f871a0` `#faa0bf` `#fdd0df` `#fee7ef`）；`danger` DEFAULT `#DB3694`，且色阶**整体反向**（50=`#690A66` … 900=`#FEEAF6`）。

其他 token `[源码]`（[globals.css](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/styles/globals.css)）：

- `--heroui-radius: 0.75rem`
- `--heroui-primary: 217.2 91.2% 59.8%`（HSL，注释「自然的现代蓝」）
- `--text-primary: 222.2 47.4% 11.2%`（标题色）、`--text-secondary: 215.4 16.3% 46.9%`
- 深色标题色：`hsl(210 40% 98%)`
- `body { letter-spacing: 0.02em }`；`h1..h6 { letter-spacing: -0.02em }`
- `.shiny-text` 流光文字动画（5s linear infinite）

### 2.3 主题切换

- Tailwind `darkMode: 'class'`；开关是 HeroUI `useSwitch` 包装的日/月图标按钮，`aria-label` 为 `Switch to dark mode` / `Switch to light mode`。`[源码]`（[tailwind.config.js](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/tailwind.config.js)、[theme-switch.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/theme-switch.tsx)）
- 主题偏好持久化到 localStorage 的 `theme` 键；登录页也带这个开关（右上角）。
- 主题色本身可被用户在「主题配置」页自定义（存在 `pages/dashboard/config/theme.tsx`，含 `ColorPicker.tsx` 与 `danger` 色阶数组）。`[源码]`（[theme.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/theme.tsx)）
- 支持自定义背景图（localStorage `background-image`），卡片会自动切换成半透明毛玻璃版本。

### 2.4 字体

`[源码]`（[globals.css](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/styles/globals.css) + [tailwind.config.js](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/tailwind.config.js)）：

- 正文：`Quicksand, Nunito, Inter, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, ..., 'PingFang SC', 'Microsoft YaHei', sans-serif`，由 CSS 变量 `--font-family-base` 控制，**可被 JS 动态覆盖**（即用户可在主题页换字体）。
- 等宽：`ui-monospace, SFMono-Regular, SF Mono, Menlo, Consolas, Liberation Mono, **JetBrains Mono**, monospace`；且自带 `@font-face` 本地托管的 JetBrains Mono（含 Italic），路径 `/webui/fonts/...`。`[源码]`（[fonts.css](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/styles/fonts.css)）
- 构建脚本里有 `fontmin`（`node scripts/fontmin.cjs`），说明中文字体做了子集化裁剪。`[源码]`（[package.json](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/package.json)）

### 2.5 导航结构（12 项，含图标与路由）

`[源码]`（[config/site.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/config/site.tsx)）：

| # | 标签 | 路由 | 图标（react-icons/lu） |
| --- | --- | --- | --- |
| 1 | 基础信息 | `/` | `LuLayoutDashboard` |
| 2 | 网络配置 | `/network` | `LuSignal` |
| 3 | 猫猫日志 | `/logs` | `LuFileText` |
| 4 | 接口调试 | `/debug/http` | `LuActivity` |
| 5 | 实时调试 | `/debug/ws` | `LuZap` |
| 6 | 文件管理 | `/file_manager` | `LuFolderOpen` |
| 7 | 插件管理 | `/plugins` | `LuPackage` |
| 8 | 插件商店 | `/plugin_store` | `LuStore` |
| 9 | 扩展页面 | `/extension` | `LuPuzzle` |
| 10 | 系统终端 | `/terminal` | `LuTerminal` |
| 11 | 系统配置 | `/config` | `LuSettings` |
| 12 | 关于我们 | `/about` | `LuInfo` |

- 侧边栏为**单层扁平**，**没有二级折叠菜单**（`MenuItem` 类型支持 `items` 子项，但当前配置未使用）。`[源码]`（[site.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/config/site.tsx)）→ `[推测]`「设计上预留了嵌套能力但产品刻意保持扁平」是我的解读。
- 侧边栏宽度用 motion 动画在 `16rem` ↔ `0` 之间切换；移动端是带遮罩的抽屉（`fixed inset-y-0 left-64 right-0 bg-black/20 backdrop-blur-[1px] md:hidden`）；折叠状态存 localStorage `side-bar-open`。`[源码]`（[sidebar/index.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/sidebar/index.tsx)）
- 侧边栏底部固定两个按钮：用户信息区与**退出登录**（`bg-danger-50/50 ... text-danger-500`）。`[源码]`（同上）

### 2.6 路由表与页面骨架

`[源码]`（[App.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/App.tsx)）：

```
/                       → IndexPage(布局) → DashboardIndexPage（基础信息）
  network | config | logs | debug(/ws,/http) | file_manager | terminal
  plugins | plugin_store | extension | about
/qq_login               → QQLoginPage（独立，无侧边栏）
/web_login              → WebLoginPage（独立，PureLayout）
```

- 所有页面 `lazy()` + `Suspense`，切换时 `AnimatePresence mode='wait'` + `motion.div` 做 `opacity 0→1 / y 20→0` 过渡。`[源码]`（[pages/index.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/index.tsx)）
- 未登录时 `AuthChecker` 自动 `navigate('/web_login')`；**注意它是纯前端守卫**（`isAuth = !!localStorage.token`），不做同步后端校验。`[源码]`（[App.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/App.tsx)）

**主布局三段式** `[源码]`（[layouts/default.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/layouts/default.tsx)）：

1. `div.h-screen.relative.flex.items-stretch.overflow-hidden`，可挂背景图（`backgroundSize: cover`）；
2. 左侧 `<SideBar>`；
3. 右侧内容区 `flex-1 overflow-y-auto`，顶部有一个 **sticky 面包屑条**：`h-10 ... backdrop-blur-lg rounded-full ... sticky top-2 z-30 m-2 mb-0`，内含折叠按钮 + HeroUI `Breadcrumbs`，标题由 `findTitle()` 沿导航树按 pathname 推导（如「基础信息」/「接口调试 / 接口」）。切换路由时内容区自动 `scrollTo({top:0, behavior:'smooth'})`。
4. 内容外面包 `ErrorBoundary`（`react-error-boundary` + 自定义 `error_fallback`）。

### 2.7 卡片风格

`[源码]`（[display_card/container.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/display_card/container.tsx)、[config/index.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/index.tsx)）：

```
Card: backdrop-blur-sm border border-white/40 dark:border-white/10
      shadow-sm rounded-2xl overflow-hidden transition-all
      bg-white/60 dark:bg-black/40        （有背景图时：bg-white/20 dark:bg-black/10）
CardHeader: p-4 pb-2 flex items-center justify-between gap-3
  标题胶囊: inline-flex px-3 py-1 rounded-lg bg-default-100/50 dark:bg-white/10
            border border-transparent dark:border-white/5, font-bold text-sm, truncate select-text
  右侧: enableSwitch（启用开关）
CardBody: px-4 py-2 text-sm text-default-600
CardFooter: px-4 pb-4 pt-2（放编辑/删除/调试按钮）
```

- 配置页容器还有宽度档位：`size='sm'` → `max-w-xl`，`'md'` → `max-w-3xl`，`'lg'` → `max-w-6xl`；外层 section `max-w-[1200px] mx-auto py-4 md:py-8`。
- 可选 3D 悬浮倾斜卡片：`components/effect_card.tsx`（登录页用它，`maxXRotation/maxYRotation` 可配）。
- 另有 `hover_titled_card.tsx`、`switch_card.tsx` 等小型卡片变体。

### 2.8 配置页的标签页导航

`[源码]`（[config/index.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/index.tsx)）：

- 用 HeroUI `Tabs`，`selectedKey` **与 URL query 双向绑定**（`navigate('/config?tab=' + key)`），默认 `onebot` —— 即**配置分组是可分享的深链接**。
- 9 个标签：`OneBot配置` / `服务器配置` / `SSL配置` / `WebUI配置` / `登录配置` / `主题配置` / `备份与恢复` / `核心配置` / `反检测`。
- TabList 视觉：`bg-white/40 dark:bg-black/20 backdrop-blur-md rounded-2xl p-1.5 shadow-sm border border-white/20 dark:border-white/5`；滑动指示器 `cursor: bg-white/80 dark:bg-white/10 backdrop-blur-md shadow-sm rounded-xl`；选中文字 `group-data-[selected=true]:text-primary`。
- 讨论：原本独立的「修改密码」标签被**合并进 WebUI 标签**（源码中保留注释掉的旧 Tab）。这是一个可借鉴的「减少顶层分组」的演进动作。

### 2.9 布尔开关怎么渲染（`SwitchCard`，强烈建议照搬）

`[源码]`（[switch_card.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/switch_card.tsx)）：

**它不是一个裸开关，而是「整行可点的设置行」**：

```
<Switch classNames={{ base:
  'inline-flex flex-row-reverse w-full bg-default-100/50 dark:bg-white/5
   hover:bg-default-200/50 dark:hover:bg-white/10
   items-center justify-between cursor-pointer rounded-xl gap-2 p-4
   border border-transparent transition-all duration-200
   data-[selected=true]:border-primary/50 data-[selected=true]:bg-primary/5 backdrop-blur-md' }} >
  <div class='flex flex-col gap-1'>
    <p class='text-medium'>{label}</p>
    <p class='text-tiny text-default-400'>{description}</p>
  </div>
</Switch>
```

要点：
- `flex-row-reverse` 让**开关在右、文案在左**，整行 `w-full` 可点击；
- **开启状态会用主色描边 + 主色 5% 淡背景**提示（`data-[selected=true]:border-primary/50 bg-primary/5`），比只变色块更能说明「这一项当前是开的」；
- 每个开关都强制带一行 `description`（`text-tiny text-default-400`），把「这个开关会发生什么」写在控件旁边。

### 2.10 数字输入怎么渲染

`[源码]`（[config/onebot.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/onebot.tsx)、[config/server.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/server.tsx)）：

- 就是 HeroUI `<Input type='number'>`，没有自制步进器；
- **单位写进 label**：`基础超时时间(毫秒)`、`预估上传速度(KB/s)`、`预估下载速度(KB/s)`、`最大超时时间(毫秒)`；
- `placeholder` 给的是**真实默认值**（`10000` / `256` / `1000` / `1800000`）；
- 取值做 `parseInt(e.target.value) || 0`，且 `value={field.value?.toString() ?? ''}`（避免 React 数字/字符串告警）；
- 统一 classNames：`inputWrapper: 'bg-default-100/50 dark:bg-white/5 backdrop-blur-md border border-transparent hover:bg-default-200/50 dark:hover:bg-white/10 transition-all shadow-sm data-[hover=true]:border-default-300'`。
- WebUI 端口等字段用 `inputMode='numeric'` 调起数字键盘。`[源码]`（[config/webui.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/webui.tsx)）

### 2.11 表格风格

- **配置/网络/插件页一律用卡片网格，不用表格**；全站只有**文件管理**用真表格（`components/file_manage/file_table.tsx`）。`[源码]`（[file_table.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/file_manage/file_table.tsx)）
- 网络配置列表用 `NetworkDisplayCard`：把 `name / enable / debug` 之外的所有字段以 `label: value` 键值对渲染，URL/Token/AccessToken 三个字段做**整行宽**（`isFullWidthField`）。`[源码]`（[display_card/common_card.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/display_card/common_card.tsx)）
- 首页「基础信息」用 6 列计数器网格（`grid grid-cols-8 md:grid-cols-3 lg:grid-cols-6`）展示 网络配置总数 / HTTP服务器 / HTTP客户端 / WS服务器 / WS客户端 数量。`[源码]`（[dashboard/index.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/index.tsx)）

**可照搬**：服务器/机器人管理后台的「配置对象列表」用卡片比表格更合适——因为每条记录的字段数不一致（有的有 token、有的没有），表格会出现大量空格子；卡片能按 `colSpan` 自适应。

### 2.12 新增/编辑表单：Schema 驱动的通用表单

`[源码]`（[network_edit/generic_form.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/network_edit/generic_form.tsx)）：

- 字段用声明式数组描述：`{ name, label, type: 'input'|'select'|'switch', options?, placeholder?, isRequired?, isDisabled?, description?, colSpan?: 1|2 }`；
- 弹窗内布局：`grid grid-cols-2 gap-y-4 gap-x-2`，`colSpan` 控制整行；
- 校验只需 `isRequired` → 自动生成错误文案 `请填写${label}`，提交失败统一 `toast.error(errors[0].message)`；
- 底部按钮固定为「关闭」（light）+「保存」（primary，提交中 `isLoading`）；
- 同文件还导出一个 `random_token(length)` 工具，字符集 `A-Za-z0-9-_.~`。

### 2.13 危险操作如何处理

`[源码]`：

- 统一用自建 Dialog Provider 的命令式 API（`contexts/dialog`，`useDialog()`），调用形态是
  `dialog.confirm({ title, content, confirmText, cancelText, onConfirm, onCancel })`。示例：`components/guid_manager.tsx` 的重启确认
  → `title: '确认重启'`、`content: '确定要重启 NapCat 吗？这将导致当前连接断开。'`、`confirmText: '重启'`。
  [源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/guid_manager.tsx)
- 破坏性按钮一律 HeroUI `color='danger'`，并常配 `bg-danger-50/50 hover:bg-danger-100/80 text-danger-500` 的浅红底。`[源码]`（[ssl.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/ssl.tsx)、[webui.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/webui.tsx)）
- 「GUID / MAC」这类高风险改动会**先自动备份再执行**，并在界面写明「操作前会自动备份」。`[源码]`（[guid_manager.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/guid_manager.tsx)）
- Hindsight 的对照做法：整库批量删除要求**输入库名确认**——i18n 文案 `bank.typeToConfirm = 'Type {bankName} to confirm:'`。`[源码]`（[messages/en.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json)）

### 2.14 「需重启才生效」怎么做（这是 NapCat 最值得抄的一处）

NapCat 用了**三层递进**，从轻到重：

**第一层 · 字段旁的 description 前置声明**（保存前就告诉你要重启）
`[源码]`：

- `description='启用后将完全禁用WebUI服务，需要重启生效'`（[server.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/server.tsx)）
- 「控制 NapCat 框架底层的核心行为设定，修改后需重启生效。」（[core.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/core.tsx)）
- 「控制 Napi2Native 模块的各项反检测功能，修改后需重启生效。」（[bypass.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/bypass.tsx)）
- SSL 页更完整：「配置SSL证书后重启即可启用HTTPS。将证书(cert.pem)和私钥(key.pem)的内容粘贴到下方文本框中。」+ 加粗警示「**注意：**保存证书后需要重启服务才能生效。删除证书后同样需要重启才能切换回HTTP模式。」（[ssl.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/ssl.tsx)）

**第二层 · 保存后用 toast 明确「重启后生效」**（而不是含糊的「保存成功」）
`[源码]`：`toast.success('保存成功，重启后生效')`（core.tsx / bypass.tsx）；GUID 相关：`toast.success('GUID 已设置，重启后生效')`、`'已删除，重启后生效'`、`'MAC 已设置，重启后生效'`（[guid_manager.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/guid_manager.tsx)）

**第三层 · 持久化横幅 + 一键重启**
`[源码]`（[system_info.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/system_info.tsx)）：更新完成后渲染一个绿色成功块（圆形对勾 + `更新完成` + `请重启 NapCat 以应用新版本`），其内再嵌一条**琥珀色警示条**（三角形图标 + `重启 NapCat 生效`），底部一个占满宽度的主色按钮 `立即重启`（`bg-primary-500 hover:bg-primary-600 shadow-primary-500/20`）。另有一处提供 `稍后重启` + `立即重启` 双按钮。

**重启动作本身的交互保障**：

- 重启按钮文案会变：`isRestarting ? '正在重启进程...' : '重启进程'`；并写明代价「重启进程将关闭当前 Worker 进程，等待 3 秒后启动新进程」。`[源码]`（[config/login.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/login.tsx)）
- 重启期间全屏 `PageLoading`，然后**轮询探测后端是否恢复**，15 秒超时给出明确失败提示：「后端在 15 秒内未响应，请检查 NapCat 运行日志或手动重启。」`[源码]`（[layouts/default.tsx](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/layouts/default.tsx)）
- 掉线自动弹窗：每 5 秒轮询 QQ 在线状态，掉线弹 `账号已离线 / 您的 QQ 账号已掉线，是否重启进程以重新登录？`，`confirmText: '重启进程'`，`cancelText: '退出账户'`。`[源码]`（同上）

---

## 3. Hindsight 记忆展示（逐条列出可验证的事实与出处链接）

### 3.1 证据基础与可信度声明（重要）

Hindsight 的产品 UI（`ui.hindsight.vectorize.io` 的 Control Plane）**需要注册才能访问**，我没有账号，因此**没有直接登录截图**。但它的 **Control Plane 前端源码完全开源**，本节的绝大多数结论来自：
`hindsight-control-plane/src/**`（v0.9.2，commit `bde55237...`）。

同时我**逐张读取了官方博客的 4 张界面截图**（`[官方截图]`），用于交叉验证源码结论：
[官方博客（Knowledge Pages for coding agents）](https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents)

**明确声明不足的地方**（不要当成已验证）：
- 我没有实测其在线 SaaS 版本，**线上 UI 与 v0.9.2 源码可能存在差异**；`[推测]`
- 我**没有找到**任何官方文档对「记忆列表分页策略」的说明文字，分页结论全部来自源码实现；`[源码]`
- 关于**移动端/窄屏**的具体断点表现，源码里只有 Tailwind 响应式类，**没有官方截图佐证**；`[推测]`

### 3.2 整体信息架构

**左侧图标导航栏**（`components/sidebar.tsx`）`[源码]`（[sidebar.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/sidebar.tsx)）：

| 顺序 | 标签（i18n） | 英文落值 | 图标（lucide-react） |
| --- | --- | --- | --- |
| 1 | `home` | Home | `Home` |
| 2 | `memories` | Memories | `Database` |
| 3 | `knowledge` | Knowledge base | `Network` |
| 4 | `recall` | Recall | `Search` |
| 5 | `reflect` | Reflect | `Sparkles` |
| 6 | `documents` | Documents | `FileText` |
| 7 | `entities` | Entities | `Users` |
| 8 | `bankConfiguration` | Bank configuration | `Settings` |

- 导航项 href 形如 `bankRoute(currentBank, '?view=' + item.id)` —— **视图切换走 query 参数**，可深链接。
- 支持**折叠**（`isCollapsed`，`aria-label` 在 `expandSidebar` / `collapseSidebar` 间切换），折叠后靠 `title` 属性提供悬浮提示。
- 这与官方截图完全一致：左侧一条窄图标栏，第 3 个（图谱/网络图标）高亮表示当前在 Knowledge 视图。`[官方截图]`

**银行（Bank）选择器**：列表随滚动分页（源码注释 "the bank selector pages as it is scrolled"），行有交错入场动画 `animate-list-row-enter`（`translateY(-4px)` → 0，200ms，`animation-delay` 交错）。`[源码]`（[globals.css](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css)）

### 3.3 Home（记忆总览）——与截图逐项对应

`[源码]`（[home-view.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/home-view.tsx)）+ `[官方截图]`

布局：`grid grid-cols-1 lg:grid-cols-3 gap-4 lg:h-[520px]`

- **左 2/3**：`Memory constellation` 卡片（标题栏带 `Network` 图标 + 右侧 `View all →`），内容为 `<Constellation height={464}>`；卡片容器 `bg-card border border-border border-solid rounded-[16px] overflow-hidden flex flex-col lg:h-full`。
- **右 1/3**：竖向两个等分卡片
  - `Knowledge pages`：页树（复用 Pages 标签的 `TreeRow`，`readOnly`），默认**展开全部文件夹**；空态是 `Layers` 图标 + `No knowledge pages yet` + `Create your first page` 按钮；`View all →` 仅在 `pages.length > 0` 时出现。
  - `Recent documents`：列表项 = `FileText` 图标 + **等宽字体的文档 UUID**（`text-xs font-mono truncate`，`title` 属性给全文）+ 相对时间（`text-[11px]`，`title` 给绝对时间）；只取 `limit: 6` 条；空态 `No documents yet`。
- **下方**：`MemoryStoreCard`（统计）与 `MemoriesActivityChart`——注释写明是**刻意降级到折叠线以下**的（"Memory stats demoted below the constellation + knowledge/docs"）。
- 加载策略有明确取舍注释：轻面板（stats/tree/docs）用 `Promise.allSettled` 一起等，**图谱单独加载、永不阻塞首屏**，并因边数超线性增长（"≈1k nodes → ~67k edges, ~22MB"）在首页**硬性限制 200 个节点**（`GRAPH_NODE_CAP = 200`），完整图谱放在 Memories 视图。
- 标题区：`<h1 className="text-3xl font-bold mb-2">Home</h1>` + `<p className="text-muted-foreground mb-6">Overview of <span className="font-mono">{bankId}</span></p>`。

截图核对：H1 `Home`、副标题 `Overview of acme-api`、左卡片标题 `Memory constellation` 带工具栏提示 `Scroll to zoom · Drag to pan · Hover to explore · Click to select`、`LINKS` 渐变图例（few→many）、右上 `Share` / `Fullscreen` 按钮、底部状态栏 `30 memories · 30 visible · 2 labels · 604 links · zoom 0.53x` 与四色图例 `semantic / temporal / entity / causal`、右栏 `Knowledge pages` 与 `Recent documents` 卡片——**与源码结构逐项吻合**。`[官方截图]`

### 3.4 Memories 视图（记忆浏览）——最接近「记忆页」需求

`[源码]`（[data-view.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx)，1461 行）

**筛选区（"Always visible filters"）**，非紧凑模式常驻：

1. **文本搜索框** `max-w-xs`，左侧内嵌放大镜图标（加载时替换为 spinner），`placeholder = 'Filter by text or context (press Enter)...'`。
   - **回车才真正检索**；清空输入框时**立即**回到未筛选列表（源码注释解释这是唯一无歧义的编辑动作）。
2. **标签筛选** `TagFilterInput`（设标签会**清空**已选 scope，两者互斥）。
3. **观测范围筛选** `ObservationScopeFilter`（仅当 `factType === 'observation'` 且存在 scope 时出现；选它则清空标签筛选）。

**视图模式切换**（右侧分段控件，`bg-muted rounded-lg p-1`，选中项 `bg-background shadow-sm`）`[源码]`：

| 按钮 | 图标 | 值 |
| --- | --- | --- |
| `Constellation` | `ScatterChart` | 图谱 |
| `Table` | `List` | 表格 |
| `Timeline` | （时间轴） | 时间轴 |

**表格列**（i18n 键 → 英文落值）`[源码]`（[messages/en.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json)）：

`Memory`（观测模式下变 `Observation`，宽度 `w-[15%]` 之外的列如下）、`Entities`（15%）、`Tags`（15%）、`Sources`（10%）、`Occurred`、`Mentioned`。

**计数与分页文案** `[源码]`：

- 筛选生效时：`{count} matching memories`
- 未加载完：`Showing {shown} of {total} total memories` + **`Load more` 文字按钮**（点击 `fetchLimit += 1000`，上限为 `total_units`）
- 全部加载完：`{count} total memories`

> **注意**：这**不是**无限滚动，是**显式的 "Load more" 按钮 + "Showing X of Y"**。`[源码]`

**观测模式的整合状态徽标** `[源码]`：

- 无待整合：绿色 `bg-green-500/10 text-green-700 dark:text-green-400 border-green-500/20` + `CheckCircle` 图标 + `In Sync`；`title` 悬浮显示上次整合时间。
- 有待整合：琥珀色 `bg-amber-500/10 ...` + `Clock` 图标 + `{count} memories pending consolidation` + 一个内嵌的刷新按钮（加载中 `animate-spin`）。
- 这是「后台异步任务进度」在列表头部的极佳范例：**同一个位置、两种状态、图标 + 颜色 + 文案三重编码**。

**无效化（软删除）语义** `[源码]`（[messages/en.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json)）：

- 过滤器分 `Active` / `Invalidated` 两态；
- 提示原文：`Invalidated memories are archived — hidden from recall and reflect, but kept for audit.` —— **明确写出「隐藏但保留可审计」**，这是软删除最好的文案范式。

**其他控件**：`Group by scope`、`Color by`（`Mentioned` / `Occurred (start)` / `Occurred (end)`）、`Link types`、recency 基准下拉。

### 3.5 Knowledge base 视图（知识页）

`[源码]`（[knowledge-base-view.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx)，1021 行）+ `[官方截图]`

**整体是「Obsidian 式工作区」**——源码原话注释：`Obsidian-style workspace: one frame, a file-explorer sidebar + editor pane`。

```
<div className="flex items-stretch overflow-hidden h-[calc(100vh-13rem)] min-h-[520px]">
  <aside className="w-1/3 flex-shrink-0 bg-muted/30 border-r border-border">
    [搜索框]
    [结果列表 或 文件夹树]
  </aside>
  <main className="flex-1 min-w-0 overflow-y-auto bg-background">
    [编辑器标签条] + [页面正文]
  </main>
</div>
```

**① 左栏搜索框** `[源码]`

- 占位符 `Search pages…`（i18n `searchPlaceholder`），左侧放大镜，有内容时右侧出现 `×` 清除按钮（`aria-label = 'Clear search'`）。
- **混合检索（BM25 + 向量）**——源码注释：`Hybrid search (BM25 + vector). A non-empty query swaps the tree for ranked hits.`，且**有防抖**（debounced）。
- 后端端点：`GET /api/knowledge-base/search?bank_id=&q=&limit=`（默认 limit 10），Control Plane 再转发到 dataplane 的 `/knowledge-base/search`。`[源码]`（[route.ts](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/api/knowledge-base/search/route.ts)、[lib/api.ts](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/lib/api.ts)）

**② 搜索结果行** `[源码]`

```
<button className="w-full text-left px-3 py-2 border-l-2 transition-colors
   选中: 'bg-primary/10 border-primary'
   未选中: 'border-transparent hover:bg-muted'">
  <span className="flex items-center gap-1.5">
    <FileText className="w-3.5 h-3.5 text-muted-foreground" />
    <span className="text-sm truncate">{r.name}</span>
  </span>
  {r.snippet && (
    <span className="mt-0.5 block pl-5 text-xs text-muted-foreground/80 line-clamp-2">{r.snippet}</span>
  )}
</button>
```

> **重要且经验证的事实**：搜索结果是**有 snippet 的（`line-clamp-2` 截断，缩进 20px 与标题对齐）**，但**没有做关键词高亮**——我在 `knowledge-base-view.tsx` 与所有 `components/ui/*.tsx` 中搜索 `highlight` / `<mark` / `matchSnippet` 均**未命中**任何高亮实现。`[源码]`

**③ 文件夹树**：有内容为空时显示斜体灰字 `Create a folder or a page to get started.`；根文件夹默认展开；每个节点支持新增子项与删除。树的计数文案 `{folders} folders · {pages} pages`。

**④ 编辑器标签条**（多标签，可关闭）`[源码]`

```
sticky top-0 z-10 flex items-stretch border-b border-border bg-muted overflow-x-auto
每个 tab: group flex items-center gap-2 px-3 py-2 border-r border-border text-sm whitespace-nowrap
  激活: 'bg-background text-foreground'；未激活: 'text-muted-foreground hover:bg-background/50'
  标签名 truncate max-w-[160px]
  关闭按钮 ×: opacity-0 group-hover:opacity-100（悬浮才出现）
```

**⑤ 单页详情的信息层级（这是记忆详情页最该抄的部分）** `[源码]`

按从上到下顺序：

1. **标题行**：`<h1 className="text-2xl font-bold text-foreground">{name}</h1>` + 右侧 `Edit` 按钮（`variant="outline" size="sm"`，带 `Pencil` 图标）。
2. **溯源行**（`text-xs`，两种链接）：
   - `Backed by {count} memories` —— 主色可点，点开溯源弹窗（provenance modal）；
   - `Mental model options` —— 灰色 + `SlidersHorizontal` 图标，链接到拥有完整检索范围（tags / tags_match / tag_groups / fact_types）的编辑器。
   - 源码注释解释了这个设计的理由：**不在页面详情里复制一份残缺的表单**，而是链接到唯一权威编辑器。
3. **新鲜度行**：`<FreshnessLine>` 组件。若无时间戳则显示 `Generating…`。
4. **标签行**：每个 tag 渲染为
   `px-1.5 py-0.5 rounded bg-blue-500/10 text-blue-600 dark:text-blue-400`。
5. **「如何派生」折叠区**：原生 `<details>` + `<summary>`（`list-none`，隐藏 `::-webkit-details-marker`），摘要为 `Info` 图标 + `How this page is derived`；展开后以**斜体 + 左侧 2px 竖线**引用块显示 source query：
   `<p className="text-xs italic pl-3 border-l-2 border-border">"{description}"</p>`
   —— 源码注释：把「生成用的机器细节」收进折叠区，让页面**一打开就是知识本身**。
6. **正文**：`<div className="prose prose-sm dark:prose-invert max-w-none border-t border-border mt-5 pt-5">` 渲染 `<CompactMarkdown>`（`react-markdown` + `remark-gfm`）；正文为空时显示斜体 `This page has no content yet.`

**⑥ 未选中任何页的空态**：`FileText` 图标（`opacity-60`）+ `Select a page to read it.`；进入视图时**自动打开第一个页面**以免右侧空白（源码注释明确说明此意图）。

**⑦ 新建/编辑弹窗字段**（`[官方截图]` 与 `[源码]` 双重验证）

官方截图中「Edit page」弹窗的实际内容：

- 标题 `Edit page`，副标题 `Update the page's name, the question that rebuilds it, and its tags.`
- 字段 1：`Name`（单行输入，聚焦时主色描边）
- 字段 2：`Source query (the question that rebuilds this page)`（多行 textarea）
  - 帮助文案：`Changing this rebuilds the page's content from your memories.`
- 字段 3：`Tags`（占位符 `type:policy, revenue, ...`）
  - 帮助文案：`Comma-separated. A type:... tag sets the page's type.`
- 底部：`Cancel`（描边次要按钮）+ `Save changes`（主色按钮）

`[官方截图]`（[博客配图](https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents)）+ `[源码]`（[messages/en.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json) 的 `knowledgeBase.*` 键）

> 核心概念（官方博客原文）：**你不写正文**。页面由 `name` + `source query`（重建该页面的那个问题）定义，内容由记忆重新合成；「改问题 → 内容重新合成」。`[官方文档]`（[Knowledge Pages 概念文档](https://hindsight.vectorize.io/developer/knowledge-pages)、[博客](https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents)）

### 3.6 图谱可视化（Constellation）

`[源码]`（[constellation.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/constellation.tsx)，1720 行）+ `[官方截图]`

- **渲染技术：自研 HTML5 Canvas 2D 渲染器**（`canvasRef` + `ctx = canvas.getContext('2d')`），**不是** d3/sigma/cytoscape 之类的库；`dpr` 做高清适配。数据来自 dataplane 的 cytoscape 风格 `{ data: {...} }` 结构，被转成扁平的 `GraphData { nodes, links }`。
- **连线按类型着色** `[源码]`（`LINK_TYPE_COLORS`）：
  `semantic: #0074d9`、`temporal: #009296`、`entity: #f59e0b`、`causal: #8b5cf6`
  —— 与截图底部图例 `semantic / temporal / entity / causal` 四个色点**完全对应**。`[官方截图]`
- **节点按 fact type 聚类着色**（Home 视图）`[源码]`：
  `world: #8b5cf6`、`experience: #ec4899`、`observation: #6366f1`、`entity: #0ea5e9`（兜底色 `#0074d9`）。
- **节点半径与连线权重挂钩**：`4 + sqrt(nodeWeight / maxWeight) * 9`（`nodeWeight` = 该节点所有连线权重之和）。
- **平移/缩放**：`panX/panY/zoom` 状态，默认 `zoom: 0.5`；每帧用 `lerp(..., 0.12)` 平滑逼近目标值（惯性感）；悬浮节点时用 `driftX/driftY` 做轻微漂移。
- **标签防重叠 + 缩放阈值分级显示**：`zoom > 1.5` 时画卡片式标签（`cardW = min(200, 80 + zoom*25)`），`zoom > 0.5` 或悬浮邻居时画普通标签；`compactLabels` 选项用于实体名等短标签密集排布。
- **图例画在 canvas 内**（右下角，`legendX = W - 12`），含连线类型色点 + 热度渐变条（默认标题 `LINKS`，可配 `sizeLegendLabel` 做「小点→大点」尺寸图例）。
- **底部状态栏**：`memories · visible · labels · links · zoom` 计数与 `zoom.toFixed(2)`。
- 交互提示文案：`Scroll to zoom · Drag to pan · Hover to explore · Click to select`。`[官方截图]`
- 每个卡片右上角有 `Share` 与 `Fullscreen` 按钮。`[官方截图]`

**Entities 视图的关系图** `[源码]`（[entities-view.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/entities-view.tsx)）：

- 视图切换 `viewRelations`（关系图）/ `viewList`（列表），**懒加载**：只有切到 relations 且还没数据时才拉图（`if (viewMode === 'relations' && !graphData && !graphLoading) loadGraph()`）。
- 复用同一个 `Constellation` 组件，传 `heatLegendLabel`（按时间新旧的热度）与 `sizeLegendLabel`。
- 空态：`noCooccurrences` / `noCooccurrencesDescription`。
- 列表模式是标准表格：表头 `Name` / `Mentions` / `First Seen` / `Last Seen`，顶部有 `{count} entities`；空态 `noEntitiesFound` + 描述。
- 数据源：`GET /api/entities?bank_id=...` 与 `GET /api/entities/graph?bank_id=&limit=&min_count=`。`[源码]`（[lib/api.ts](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/lib/api.ts)）
- **单实体时间轴**：每个实体可展开「该实体关联的全部记忆」的时间轴视图（复用 `data-view` 的 `TimelineView`，走反向查询；注释说明派生的 observations 不参与实体链接）。`[源码]`（[entities-view.tsx](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/entities-view.tsx)）

### 3.7 空状态、分页、片段高亮（汇总，逐条给证据）

| 主题 | 结论 | 出处 |
| --- | --- | --- |
| 空记忆库 | `No memories yet` + `Add a document to start building this memory bank.` + 一个 `Add document` 按钮（触发页面上 `[data-add-document]` 按钮） | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx) |
| 筛选无结果 | `No memories match your filter` | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json) |
| 搜索无结果 | `No memories found` / Knowledge 页是 `No matching pages.` | 同上 |
| 时间轴无数据 | `No Timeline Data` + `No memories have occurred_at dates.` | 同上 |
| 知识页空 | `No folders or pages yet.` + `Create a folder or a page to get started.` | 同上 |
| 详情未选中 | `Select a page to read it.`（图标 + 灰字居中） | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx) |
| 正文为空 | `This page has no content yet.`（斜体） | 同上 |
| 记忆分页 | **"Load more" 按钮**，`fetchLimit += 1000`；文案 `Showing {shown} of {total} total memories` → 全量后 `{count} total memories` | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx) |
| 文档分页 | **页码式**：`Showing {from}-{to} of {total}` + `Previous` / `Next` | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json) |
| 知识页搜索 | 固定 `limit=10`，**无分页** | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/lib/api.ts) |
| Bank 选择器 | **无限滚动分页**（源码注释 + `list-row-enter` 交错动画） | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css) |
| 片段高亮 | **搜索结果有 snippet（两行截断），但不做 `<mark>` 关键词高亮**（全仓 grep 未命中高亮实现） | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx) |
| 时间显示 | 相对时间为主 + `title` 悬浮给绝对时间（`formatRelativeTime` / `formatAbsoluteDateTime`）；新鲜度行中「刷新时间用**年龄**（4 hours ago），水位线用**位置**（formatWatermark）」，避免两者折叠成同一字符串 | [源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/freshness-line.tsx) |

### 3.8 Hindsight 的配色设计系统（hex 全量）

`[源码]`（[globals.css](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css)）——**这是本报告中最可直接复用的资产。**

**浅色**

| Token | 值 | 备注 |
| --- | --- | --- |
| `--background` | `#F3F5F9` | 页面底 |
| `--foreground` | `#0D1117` | 正文 |
| `--card` / `--popover` | `#FFFFFF` | 卡片 |
| `--primary` | `#0074D9` | |
| `--primary-gradient` | `linear-gradient(135deg,#0074d9 0%,#009296 100%)` | 品牌渐变 |
| `--secondary` / `--muted` / `--accent` | `#EDF0F6` | |
| `--muted-foreground` | `#525866` | 源码注释：5.3:1 / 7.1:1，满足 WCAG AA |
| `--destructive` | `#D4183D` | |
| `--border` / `--input` | `rgba(0,0,0,0.08)` | |
| `--ring` | `#0074D9` | |
| `--chart-1..5` | `#2C82F5` `#10B981` `#8B5CF6` `#F59E0B` `#EC4899` | |
| `--chip-tag` / border | `#e7edf5` / `#d3dfec` | 标签 chip |
| `--chip-entity` / border | `#eaeaf7` / `#dcdcf1` | 实体 chip |
| `--chip-meta` / border | `#f4f6f8` / `#e7ebef` | 元数据 chip |
| `--chip-fg` / `--chip-accent` | `#55677d` / `#0e7f8c` | |
| `--sidebar` / `--sidebar-foreground` | `#FFFFFF` / `#6C7586` | |
| `--sidebar-accent` / `--sidebar-border` | `#EDF0F6` / `rgba(0,0,0,0.05)` | |
| `--radius` | `0.5rem` | |
| `--tracking-normal` | `0em` | 注释：0 追踪最适合 Inter 正文 |
| `--hs-success` / `--warning` / `--danger` / `--info` / `--neutral` | `#00BC7D` / `#F59E0B` / `#D4183D` / `#0074D9` / `#6C7586` | |
| `--hs-brand-blue` / `--hs-brand-teal` | `#0074D9` / `#009296` | |
| chart 语义色 | `world #2C82F5`、`experience #10B981`、`observations #8B5CF6`、`temporal #F59E0B`、`semantic #009296`、`entity #EC4899` | |

**深色**

| Token | 值 |
| --- | --- |
| `--background` | `#080C17` |
| `--foreground` | `#DCE8FF` |
| `--card` / `--popover` | `#0F1724` |
| `--primary` / `--ring` | `#5BB0FF`（前景 `#080C17`） |
| `--secondary` / `--muted` / `--accent` | `#172036` |
| `--muted-foreground` | `#94A8C9` |
| `--destructive` | `#C0183A` |
| `--border` / `--input` | `rgba(100,160,255,0.10)` |
| chip 组 | tag `#1c2836`/边框 `#2e4056`；entity `#272f45`/`#3b4463`；meta `#141b24`/`#232d3a`；fg `#94a8bc`；accent `#4dd8e6` |
| `--sidebar` / `--sidebar-foreground` | `#0A1020` / `#6E8AAF` |
| `--hs-success` / `--warning` | `#00D492` / `#FBB040` |

**组件级细节（同一文件）** `[源码]`：

- **开关**：选中时用品牌渐变 —— `[data-slot="hs-switch"][data-state="checked"] { background: var(--primary-gradient) !important; }`；未选中浅色 `#c3cad6`（注释：介于 slate-300/400），深色 `#475569`（slate-600）。
- **chip 设计原则**（注释原文要点）：所有 tag/entity/metadata 用**同一个安静的中性 chip**，**强调色只花在「正在参与筛选的那一个 chip」上**；三种 chip 是**同一色系的明度变体**而非三种颜色，且各自带边框（因为小尺寸下边框对感知色调的贡献不亚于填充），任意两者 ΔE 3.4–13。
- **主题实现**：CSS 变量 + `.dark` 类；并显式声明 `@custom-variant dark (&:where(.dark, .dark *))`，源码注释解释了原因：**Tailwind v4 默认把 `dark:` 编译成 `prefers-color-scheme` 媒体查询**，与「应用内切换」的 `.dark` 类不一致，会导致约 145 处 `dark:` 工具类静默失效。
- **字体**：Inter（`next/font/google` 注入 `--font-sans`）+ JetBrains Mono（`--font-mono`，用于 `code/pre` 与 UUID 展示）；并强制 `html, body, button, input, select, textarea` 都用 `--font-sans`（否则表单元素会回退到浏览器默认字体）。`h1..h6 { font-weight: 600 }`，注释：**600 是 UI 文本的上限**。
- **阴影**：`--shadow-sm: 0px 1px 3px 0px hsl(0 0% 0% / 0.17), 0px 1px 2px -1px ...` 等一套 8 档。
- **表格正文样式**：`.prose table` 有专门覆盖（`border: 2px solid rgba(0,0,0,0.2)`，表头下边框 3px，`tbody tr:hover` 背景 `rgba(0,0,0,0.04)`），深色用白色透明边框。
- **滚动条**：`scrollbar-width: thin` + `color-mix(in oklab, var(--muted-foreground) 30%, transparent)` 自定义 thumb。
- **动效可达性**：所有动画都写了 `@media (prefers-reduced-motion: reduce)` 降级（logo 翻滚 → 透明度脉冲）。

**技术栈** `[源码]`（[package.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/package.json)、[components.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/components.json)）：Next.js `^16.2.11`（App Router + Turbopack）、React `^19.2.0`、Tailwind CSS `^4.1.17`（CSS-first 配置）、shadcn/ui（`style: "default"`、`baseColor: "slate"`、`cssVariables: true`）、Radix UI 原语（alert-dialog / checkbox / dialog / dropdown-menu / label / popover / select / slot / switch / tabs / visually-hidden）、`lucide-react` 图标、`next-intl` 多语言（含 `zh-CN.json`！）、`next-themes`、`recharts`、`react-markdown` + `remark-gfm`、`sonner`（toast）、`cmdk`（命令面板）、`react18-json-view`（元数据 JSON 查看）、`cronstrue` / `cron-parser`。
所有 UI 文案都是 i18n 键，`messages/` 下有 10 种语言（含简体中文 `zh-CN.json`）。

---

## 4. 给「配置页 + 记忆页」的具体设计建议

> 本节是**设计建议**（我的判断），不是对参考产品的陈述。凡直接引用事实处均带链接；纯建议处不加链接。

### 4.1 整体信息架构

**建议直接采用 Hindsight 的左侧图标栏 + query 参数视图**：

```
[图标栏]  概览 · 配置 · 记忆 · 实体 · 日志/终端 · 关于
```

理由：NapCat 的 12 项扁平导航对本项目过重（其中「插件商店」「扩展页面」「反检测」等不适用）；Hindsight 的 8 项带图标栏更紧凑，且视图状态走 `?view=` 可深链接、可分享、刷新不丢。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/sidebar.tsx)

**融合两者各自的强项：**

| 场景 | 抄谁 | 具体做法 |
| --- | --- | --- |
| 配置页分组 | NapCat | 用**横向 Tabs + `?tab=` 深链接**，默认第一个分组；TabList 用 `rounded-2xl backdrop-blur` 胶囊条。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/index.tsx) |
| 记忆页分组 | Hindsight | `Pages / Mental Models`（或「知识页 / 原始记忆」）两个下划线 Tab，配一句副标题说明它们各是什么。[官方截图](https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents) |
| 配置对象列表 | NapCat | **卡片网格而非表格**（不同对象字段数不同，表格会大量留空）。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/display_card/common_card.tsx) |
| 记忆列表 | Hindsight | **Constellation / Table / Timeline 三视图分段控件** + 常驻筛选行。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx) |
| 记忆详情 | Hindsight | **双栏 Obsidian 式工作区**：左 1/3 搜索+树，右侧多标签正文。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx) |

### 4.2 配置页：字段如何分组

按**「变更半径 / 是否需重启」**分组，而不是按技术模块分组——这是 NapCat 分组方式暴露出的一个可改进点（它的「服务器配置」里混着端口、SSL、WebUI 开关，重启影响面差别很大）。

建议分组：

1. **基本信息**（机器人名、头像、Owner）—— 热更新
2. **连接与网络**（OneBot 上报地址、端口、token、心跳）—— 热更新
3. **模型 / 推理**（API key、模型名、温度、超时）—— 热更新
4. **记忆系统**（开关、嵌入模型、保留策略、整合周期）—— 部分需重启
5. **WebUI 与安全**（登录 token、2FA、Passkey、IP 白名单、SSL 证书）—— **多数需重启**
6. **高级 / 危险区**（重置数据库、导出导入、清空记忆）—— 独立最后一块

**字段渲染规范**（全部有 NapCat 源码依据）：

- 每个布尔项使用 `SwitchCard` 模式：**整行可点 + 开关在右 + 强制描述文案 + 开启时主色描边与 5% 底色**。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/switch_card.tsx)
- 数字输入：**单位写进 label**（`基础超时时间(毫秒)`），`placeholder` 放**真实默认值**，`parseInt(...) || 0` 兜底。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/onebot.tsx)
- 表单用**声明式字段数组**驱动（`{name,label,type,options,placeholder,description,isRequired,colSpan}`），新增字段零 UI 代码。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/network_edit/generic_form.tsx)
- 密钥类字段：等宽字体 + 默认打码 + 「显示/复制」按钮；参考 Hindsight 在 Recent documents 里用 `font-mono truncate` 展示 UUID 并配 `title` 悬浮全文。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/home-view.tsx)
- 帮助文案分三层用：**字段旁 description（常态）→ 折叠 `<details>` 放长解释（参考 "How this page is derived"）→ 文档外链**。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx)

### 4.3 危险操作如何处理

**分级策略（三档，各有出处）：**

| 档 | 触发条件 | 交互 | 参考 |
| --- | --- | --- | --- |
| 低 | 可逆、影响单条 | 不需要确认，操作后给 **Undo toast** | 通用 |
| 中 | 影响服务连续性（重启、断开连接） | `dialog.confirm({title, content, confirmText, cancelText})`，`content` **必须写明后果**（如「这将导致当前连接断开」），确认按钮用 `color='danger'` | [NapCat guid_manager](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/guid_manager.tsx) |
| 高 | 不可逆 / 影响全库 | **输入名称确认**（`Type {bankName} to confirm:`）+ 弹窗正文列出**将被删除的具体数量**（如「{count} memory units」），并写明 `This action cannot be undone.` | [Hindsight en.json](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json) |

**另外两条可直接抄的做法：**

- **软删除优先**：记忆/知识条目用 `Active / Invalidated` 双态过滤，提示文案照抄 Hindsight 的语义——`Invalidated memories are archived — hidden from recall and reflect, but kept for audit.` 即「隐藏但不物理删除，保留可审计」。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json)
- **破坏性配置自动备份**：NapCat 在改 GUID/MAC 前自动备份，并在界面写明「操作前会自动备份」。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/guid_manager.tsx)

### 4.4 如何提示「需重启」（建议三层，全部照搬 NapCat）

**L1 · 保存前，字段旁声明**（最重要——用户不该保存完才知道）
在需要重启的字段 `description` 里直接写：`修改后需重启生效`。NAP 的原文样例：`控制 NapCat 框架底层的核心行为设定，修改后需重启生效。`[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/core.tsx)
对整块都需重启的分组，在分组顶部加一条**琥珀色横幅**（图标 + 一句话），而不是逐个字段重复。

**L2 · 保存后，toast 说清代价**
不要只 `toast.success('保存成功')`，而要 `toast.success('保存成功，重启后生效')`。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/core.tsx)

**L3 · 持久化「待生效」状态 + 一键重启**
维护一个全局 `pendingRestart: Set<fieldPath>`，在**顶部 sticky 条或全局浮动条**渲染：
`N 项修改需重启才能生效` + `[立即重启]` `[稍后重启]`。
NapCat 的对应实现是绿色成功块 + 内嵌琥珀警示条 + 整宽主色 `立即重启` 按钮。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/components/system_info.tsx)

**重启动作本身必须做的三件事（NapCat 已全部实现，强烈照搬）：**

1. **按钮文案随状态变**：`立即重启` → `正在重启进程...`；旁边写明耗时预期「等待 3 秒后启动新进程」。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/dashboard/config/login.tsx)
2. **重启期间全屏 loading**，避免用户以为页面挂了。（NapCat 用 `PageLoading` 覆盖层。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/layouts/default.tsx)）
3. **轮询探测后端恢复，并设超时**：15 秒未响应则明确失败——`后端在 15 秒内未响应，请检查 NapCat 运行日志或手动重启。` 恢复后自动 `window.location.reload()`。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/layouts/default.tsx)

### 4.5 记忆页：信息层级（建议按此顺序堆放）

参考 Hindsight Knowledge 页详情的顺序，建议单条记忆/知识页从上到下为：

1. **标题**（`text-2xl font-bold`）+ 右侧 `编辑` 次要按钮
2. **来源/溯源行**（`text-xs`）：`由 N 条记忆支撑`（可点开溯源）· `检索范围设置`（链接到权威编辑器，**不在详情页复制一份残缺表单**）
3. **新鲜度行**：`更新于 {相对时间}` + 状态徽标（`已同步` / `待整合 N 条`），绝对时间放 `title`
4. **标签行**：中性 chip；**只有正在参与筛选的那个 chip 才用强调色**
5. **「如何派生」折叠区**（`<details>`）：默认收起，展开显示生成该内容的原始 query/提示词
6. **正文**：Markdown（`prose prose-sm dark:prose-invert`），顶部一条分隔线
7. **元数据面板**：建议做成**右侧可折叠抽屉或底部 `<details>`**，而不是常驻侧栏——原始 JSON 适合用 `react18-json-view` 之类折叠树展示

**依据**：[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx) + [官方截图](https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents)

### 4.6 记忆页：搜索与筛选

- **搜索框只在回车时检索**（避免每敲一个字符打一次后端），但**清空输入框时立即恢复完整列表**。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx)
- 检索用**混合检索（BM25 + 向量）+ 防抖**，结果固定 `limit=10`。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx)
- 结果行：`文件图标 + 标题(truncate) + snippet(line-clamp-2, 缩进对齐)`；选中态用 **左侧 2px 主色竖线 + `bg-primary/10`**，不用整行高亮。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/knowledge-base-view.tsx)
- **筛选器互斥要显式处理**：Hindsight 里「标签筛选」与「范围筛选」互相清空对方（源码注释：`so the two filters never fight over the same query`）。这是个很值得抄的细节——多筛选器共存时必须有明确的优先级规则。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx)
- **计数文案要区分三种状态**：`匹配 N 条` / `已显示 X / 共 Y 条` / `共 Y 条`。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json)
- **snippet 是否高亮**：Hindsight **没有做**关键词高亮。建议本项目**补上** `<mark>` 高亮（这是比参考产品更好的一处），但保持 snippet 两行截断与缩进对齐。

### 4.7 分页策略建议

| 数据量 | 建议方案 | 依据 |
| --- | --- | --- |
| 记忆列表（可上万条） | 显式 **"加载更多"** 按钮 + `已显示 X / 共 Y 条`（不要无限滚动：用户需要知道总量，且便于回到位置） | [Hindsight data-view](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/data-view.tsx) |
| 文档/日志表 | **页码式** + `Showing {from}-{to} of {total}` + Previous/Next | [Hindsight documentsView](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/messages/en.json) |
| 知识页搜索结果 | 固定上限，**不分页**（少而准优于多） | 同上 |
| 切换器/选择器里的长列表 | 无限滚动 + 行交错入场动画 | [Hindsight globals.css 注释](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css) |

### 4.8 图谱可视化建议

**如果要做出记忆关系图，Hindsight 的实现给了明确的工程取舍：**

- **Canvas 2D 自研渲染**，不用图谱库；支持 pan/zoom + `lerp` 惯性 + 悬浮漂移。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/constellation.tsx)
- **边数超线性增长是硬约束**（源码注释：「≈1k nodes → ~67k edges, ~22MB」），所以**概览页必须限节点数**（Hindsight 限 200），完整图谱放在专门视图、**独立加载、不阻塞首屏**。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/home-view.tsx)
- **按缩放级别分级显示标签**（`zoom > 1.5` 卡片式 / `> 0.5` 普通 / 否则只在悬浮时），并做标签防重叠。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/constellation.tsx)
- **节点大小映射连接权重**：`4 + sqrt(w/maxW) * 9` —— 用平方根压缩长尾。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/home-view.tsx)
- **图例画在画布内右下角**，含连线类型色点与热度渐变条；底部状态栏给 `节点数 · 可见数 · 边数 · zoom`。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/constellation.tsx)
- **连线配色沿用 Hindsight 语义**：`semantic #0074d9` / `temporal #009296` / `entity #f59e0b` / `causal #8b5cf6`。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/constellation.tsx)
- **一定要给空态**：主图谱空时显示 `No memories yet`；实体共现图空时是 `noCooccurrences` + 一句解释「为什么现在是空的」。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/components/entities-view.tsx)

**如果资源有限**：可以先只做「Table + Timeline」两个视图，图谱放到二期——Hindsight 的表格视图已经承载了全部筛选能力，图谱是增强而非必需。

### 4.9 配色与设计 token 建议

- **直接采用 Hindsight 的双层色板**（外层 shadcn 语义 token + 内层 `--hs-*` 品牌 token），深色底 `#080C17` / 卡片 `#0F1724` / 正文 `#DCE8FF` / 主色 `#5BB0FF`，浅色底 `#F3F5F9` / 卡片 `#FFFFFF` / 正文 `#0D1117` / 主色 `#0074D9`。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css)
- **如果要做「萌系」风格**（NapCat 路线）：主色 `#FF7FAC`、辅色 `#88C0D0`、危险色 `#DB3694`，圆角 `0.75rem`，正文 `Quicksand/Nunito/Inter`，等宽 `JetBrains Mono`。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/tailwind.config.js)
- **无论选哪种，都建议抄 Hindsight 的 chip 策略**：所有标签/实体/元数据用同一色系的中性 chip，**强调色只留给「正在筛选」的那一个**。这是让表格不显得花的关键。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css)
- **Tailwind v4 用户注意**：如果主题用 `.dark` 类切换，**必须**加 `@custom-variant dark (&:where(.dark, .dark *))`，否则所有 `dark:` 工具类会绑到系统偏好而非应用内开关（Hindsight 源码注释记录了这个坑，约 145 处受影响）。[源码](https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/hindsight-control-plane/src/app/globals.css)

### 4.10 登录/鉴权建议（照搬 NapCat 的具体清单）

1. 首次启动生成随机 token → 写回配置文件 → 打印启动日志 → 可选私聊推送。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/index.ts)
2. 登录只传 `sha256(token + 固定盐)`，不传明文；`.napcat` 这个盐可换成项目自己的常量。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/helper/SignToken.ts)
3. 会话用 **HMAC 签名 + 时间戳**的自包含凭证（无服务端 session 存储），**密钥进程级随机**→ 重启即全员登出；有效期 1 小时。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/helper/SignToken.ts)
4. 传输：`Authorization: Bearer <credential>`；**同时**支持 `?webui_token=` 供 `<img>`/WebSocket 使用。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/middleware/auth.ts)
5. WebSocket 在**握手阶段** `verifyClient` 校验 query token，失败返回 401。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/terminal/terminal_manager.ts)
6. 按 IP 限流：`login_rate:{ip}`，60 秒窗口，默认 3 次/分钟。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/helper/Data.ts)
7. 全局 401 拦截：清 token + 整页 reload（不要只弹一个 toast 然后把用户留在坏掉的界面上）。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/utils/request.ts)
8. 改密码：要求旧密码 + 长度 ≥ 6 + 含字母 + 含数字 + 新旧不同；成功后**立即刷新内存缓存并注销旧凭证**。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts)
9. 可选增强：（a）TOTP 2FA——登录先返回 `require2FA: true`，第二次带 `totpCode`；关闭 2FA 也要求验一次码。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts)（b）Passkey/WebAuthn 自动登录，单用户固定 `userId`。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-backend/src/api/Auth.ts)
10. 登录页细节：**不加用户名字段**，只留一个 Token 密码框；但**放一个隐藏的 username 输入**（值 `napcat-webui`）以便浏览器密码管理器正常保存。[源码](https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/packages/napcat-webui-frontend/src/pages/web_login.tsx)

---

## 5. 明确标注：未能验证的结论与证据缺口

以下内容**我没有找到可引用的一手证据**，请勿当作事实使用。

### 5.1 关于 NapCat WebUI

| # | 未验证项 | 我的标注与理由 |
| --- | --- | --- |
| 1 | **实际渲染出来的像素级视觉**（间距、圆角实际观感、hover 动效体感） | `[推测]`。我没有运行 NapCat WebUI 实例，也没有找到官方界面截图；所有视觉结论都来自 Tailwind class 字符串的**代码阅读**，实际渲染可能因 HeroUI 默认主题变量、浏览器差异而与描述有出入。 |
| 2 | **限流是「固定窗口」还是「滑动窗口」** | `[推测]`。源码是 `store.set(key, 1, 60)` 的 60 秒 TTL 计数，我据此判断为固定窗口，但源码没有文字说明。 |
| 3 | **`accessControlMode` / `ipWhitelist` / `ipBlacklist` / `enableXForwardedFor` 的具体行为** | 部分未验证。我确认这些字段存在于 `webui.json`，但**没有逐行阅读它们的判定实现**（`src/middleware/` 下只有 `auth.ts` 与 `cors.ts`，访问控制逻辑应在别处，我未定位）。 |
| 4 | **导航 `MenuItem` 的 `items` 嵌套是否在任何分支/版本中被使用过** | `[推测]`。类型定义支持，当前配置未使用；我推断为「预留能力」，但没有 git 历史证据。 |
| 5 | **官方文档「二维码登录后会刷新 WebUI Token」的准确触发时机** | `[官方文档]`（[basic](https://napneko.github.io/config/basic)）有此描述，源码中我看到了 `setPendingTokenToSend` 机制与相关注释，但**没有完整追完 QQ 登录成功后的回调链路**来确认「必定刷新」。 |
| 6 | **`prefix` 字段在 v4.4 之后是否真的完全无效** | `[官方文档]` 如此说明，配置文件里字段仍存在（值为 `""`）。我**没有在源码中定位 `prefix` 的消费点**。 |
| 7 | **NapCat WebUI 是否使用 SSR / 是否有独立的 WebSocket 会话管理** | `[推测]`。前端是纯 Vite SPA（`vite build` + `index.html`），我据此判断无 SSR；WS 会话管理我只读了终端这一条链路。 |

### 5.2 关于 Hindsight

| # | 未验证项 | 我的标注与理由 |
| --- | --- | --- |
| 1 | **线上 `ui.hindsight.vectorize.io` 的真实外观与操作手感** | `[推测]`。需要注册账号，我没有登录。**本文关于 Hindsight 的一切都基于开源 Control Plane 源码（v0.9.2）+ 官方博客的 4 张截图**。线上可能已迭代到更高版本（仓库另有 0.9.1 的更新日志）。 |
| 2 | **`memory-detail-modal.tsx`（单条记忆详情弹窗）的完整字段布局** | **未完成**。我确认该文件存在（912 行）且 `constellation` 支持「open memories in a dialog」的提交记录，但**没有逐行读完**其字段分组。本文 3.5 节的「单页详情层级」描述的是 **Knowledge Page 详情页**（我已完整读完），不是 memory modal。二者是不同界面，请勿混用。 |
| 3 | **记忆列表的时间轴（Timeline）视图的具体视觉** | 部分验证。我确认它存在、有缩放/首末页/前后页控件、粒度切换（年/月/周/日）、空态文案与「无日期项」的单独提示，但**没有读完其 400+ 行渲染代码**，因此无法描述其视觉细节。 |
| 4 | **图谱的布局算法** | `[推测]`。源码里节点有 `wx/wy`（world coords）且在渲染前已算好，`home-view.tsx` 的注释只说数据来自 "cytoscape-style" 结构。我**没有找到力导向布局的代码位置**，因此**不能断言**它用力导向、也不排除布局在后端完成。 |
| 5 | **搜索结果 snippet 的服务端生成方式**（是否做 query 词周围窗口截取） | 未验证。我只确认前端渲染 `r.snippet` 且不做高亮，**没有追踪 dataplane 侧 snippet 的构造逻辑**（不在本仓库范围内）。 |
| 6 | **移动端/窄屏表现** | `[推测]`。源码只有 Tailwind 响应式类，无官方移动端截图；`h-[calc(100vh-13rem)]` 这类硬编码高度在窄屏下的表现我无法确认。 |
| 7 | **多语言切换的实际 UI** | 部分验证。我确认存在 `language-switcher.tsx` 与 10 种语言包（含 `zh-CN`），但**没有读切换器的交互细节**。 |
| 8 | **官方博客的 benchmark 数字（如「降低 57% 修正」）** | 这是官方自述的营销数据，我**未做独立验证**，报告中仅作为「官方称」引用来源。[官方博客](https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents) |

### 5.3 通用缺口

- **两个产品我都没有实际部署运行**，没有做交互测试、可访问性测试或性能测量；所有结论均来自静态源码阅读与官方文档/截图。
- **未见任何官方设计规范文档**（design system doc / Figma）。Hindsight 的设计意图只散落在 `globals.css` 的注释里（这些注释质量很高，但仍是代码注释而非规范）。
- 截图分辨率有限，**未从中提取像素级尺寸/间距**；截图中的颜色我只做了目视核对，未做取色器采样，因此**配色以源码 hex 为准**，不以截图为准。

---

## 附：核心出处清单

**NapCat（commit `109d0c1dff755875f3b79795e99cee6115289fbb`）**

- 官方文档 · WebUI 配置：https://napneko.github.io/config/basic
- 官方文档 · 安全：https://napneko.github.io/other/security
- 后端鉴权接口：`packages/napcat-webui-backend/src/api/Auth.ts`
- 后端鉴权中间件：`packages/napcat-webui-backend/src/middleware/auth.ts`
- 凭证签名：`packages/napcat-webui-backend/src/helper/SignToken.ts`
- 启动与 token 生成：`packages/napcat-webui-backend/index.ts`
- 限流：`packages/napcat-webui-backend/src/helper/Data.ts`
- 终端 WS 鉴权：`packages/napcat-webui-backend/src/terminal/terminal_manager.ts`
- `webui.json` 默认值：`packages/napcat-webui-backend/webui.json`
- 登录页：`packages/napcat-webui-frontend/src/pages/web_login.tsx`
- 路由与布局：`packages/napcat-webui-frontend/src/App.tsx`、`src/layouts/default.tsx`、`src/layouts/pure.tsx`
- 导航配置：`packages/napcat-webui-frontend/src/config/site.tsx`
- 主题色板：`packages/napcat-webui-frontend/tailwind.config.js`
- 全局样式与字体：`src/styles/globals.css`、`src/styles/fonts.css`
- 开关组件：`src/components/switch_card.tsx`
- 卡片组件：`src/components/display_card/container.tsx`、`common_card.tsx`
- 通用表单：`src/components/network_edit/generic_form.tsx`
- 重启提示：`src/components/system_info.tsx`、`src/components/guid_manager.tsx`、`src/pages/dashboard/config/{core,ssl,server,bypass,login}.tsx`
- 请求拦截与 401：`src/utils/request.ts`

（以上路径均前缀 `https://github.com/NapNeko/NapCatQQ/blob/109d0c1dff755875f3b79795e99cee6115289fbb/`）

**Hindsight（commit `bde55237f53bf55aacd048b01e29d7dc23b83a85`）**

- 官方文档 · Knowledge Pages：https://hindsight.vectorize.io/developer/knowledge-pages
- 官方博客 · Knowledge Pages for coding agents（含 4 张界面截图）：https://hindsight.vectorize.io/blog/2026/08/13/knowledge-pages-coding-agents
- 设计系统与全部色值：`hindsight-control-plane/src/app/globals.css`
- 侧边栏：`src/components/sidebar.tsx`
- 首页：`src/components/home-view.tsx`
- 记忆浏览：`src/components/data-view.tsx`
- 知识库：`src/components/knowledge-base-view.tsx`
- 图谱渲染：`src/components/constellation.tsx`
- 实体与关系图：`src/components/entities-view.tsx`
- 新鲜度行：`src/components/freshness-line.tsx`
- 全部 UI 文案（i18n 落值）：`src/messages/en.json`
- API 客户端：`src/lib/api.ts`
- 技术栈：`hindsight-control-plane/package.json`、`components.json`

（以上路径均前缀 `https://github.com/vectorize-io/hindsight/blob/bde55237f53bf55aacd048b01e29d7dc23b83a85/`）
