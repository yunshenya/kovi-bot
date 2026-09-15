/* 芸汐管理后台 —— 无构建前端
 *
 * 结构：登录 → 概览 / 配置 / 记忆 / 标注 / 系统。
 * 约定：
 *   - 一律用 DOM API 构造节点（记忆正文来自模型输出，绝不能当 HTML 插入）；
 *   - 所有写操作都先经服务端校验，失败时展示服务端返回的原因；
 *   - 配置页只提交"改动过的字段"，注释与未改部分由服务端无损保留。
 */
(() => {
  'use strict';

  // ───────────────────────────── 小工具 ─────────────────────────────

  const $ = (selector) => document.querySelector(selector);

  /** 建节点：h('div', {class: 'x'}, '文本', h('b', {}, '粗')) */
  function h(tag, attrs, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs || {})) {
      if (value === null || value === undefined || value === false) continue;
      if (key === 'class') node.className = value;
      else if (key === 'text') node.textContent = value;
      else if (key === 'html') node.innerHTML = value;
      else if (key.startsWith('on') && typeof value === 'function') {
        node.addEventListener(key.slice(2), value);
      } else if (key === 'value') node.value = value;
      else if (value === true) node.setAttribute(key, '');
      else node.setAttribute(key, value);
    }
    for (const child of children.flat()) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

  /** 图标节点：`<svg class="icon"><use href="#i-name"></use></svg>`。
   *  路径数据只在 index.html 的 sprite 里写一份，这里只引名字。 */
  function icon(name, className) {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('class', className ? `icon ${className}` : 'icon');
    svg.setAttribute('aria-hidden', 'true');
    const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
    use.setAttribute('href', `#i-${name}`);
    svg.append(use);
    return svg;
  }

  /** 当前是不是窄屏（手机）。与 app.css 里 760px 那一档断点保持同一个数——
   *  有些取舍（标注页把统计收起来、队列放到详情下面）靠 CSS 表达不了。 */
  const NARROW_QUERY = '(max-width: 760px)';
  const narrowLayout = () => window.matchMedia(NARROW_QUERY).matches;

  const TOAST_ICONS = { ok: 'check', warn: 'alert', bad: 'close' };

  function toast(message, kind = 'ok', ms = 4200) {
    const node = h('div', { class: `toast ${kind}`, title: '点击关闭' },
      h('span', { class: 'toast-icon' }, icon(TOAST_ICONS[kind] || 'info')),
      h('span', { class: 'toast-text', text: message }));
    node.addEventListener('click', () => node.remove());
    $('#toast-stack').append(node);
    setTimeout(() => node.remove(), ms);
  }

  /** 状态胶囊的文案要写进 .pill-text，不能整个 pill.textContent——
   *  那会把里面那个呼吸的小圆点一起冲掉。 */
  function setPill(node, kind, text) {
    if (!node) return;
    node.className = `pill ${kind}`;
    const label = node.querySelector('.pill-text');
    if (label) label.textContent = text;
    else node.textContent = text;
  }

  async function api(path, options = {}) {
    // `raw` 走原始请求体：表情包上传就是图片字节，不能 JSON 化。其余仍按 JSON 提交。
    const { body, raw, headers: extraHeaders, ...rest } = options;
    const headers = { ...(extraHeaders || {}) };
    let payload;
    if (raw !== undefined) payload = raw;
    else if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      payload = JSON.stringify(body);
    }
    const response = await fetch(path, {
      credentials: 'same-origin',
      headers,
      ...rest,
      body: payload,
    });
    if (response.status === 401) {
      showLogin();
      throw new Error('登录状态已失效，请重新登录');
    }
    let data = null;
    try { data = await response.json(); } catch (_) { /* 有些响应没有正文 */ }
    if (!response.ok) {
      throw new Error((data && data.error) || `请求失败（HTTP ${response.status}）`);
    }
    return data;
  }

  function fmtTime(value) {
    if (!value) return '—';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return String(value);
    const pad = (n) => String(n).padStart(2, '0');
    return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} `
      + `${pad(date.getHours())}:${pad(date.getMinutes())}`;
  }

  function fmtBytes(bytes) {
    if (bytes === null || bytes === undefined) return '—';
    const units = ['B', 'KB', 'MB', 'GB'];
    let value = Number(bytes);
    let unit = 0;
    while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
    return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
  }

  function relative(iso) {
    if (!iso) return '';
    const then = new Date(iso).getTime();
    if (Number.isNaN(then)) return '';
    const seconds = Math.floor((Date.now() - then) / 1000);
    if (seconds < 60) return '刚刚';
    if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
    if (seconds < 86400 * 30) return `${Math.floor(seconds / 86400)} 天前`;
    return fmtTime(iso);
  }

  const displayValue = (value) => {
    if (value === null || value === undefined) return '—';
    if (typeof value === 'object') return JSON.stringify(value);
    if (typeof value === 'string') return value;
    return String(value);
  };

  const MASK = '********';

  // ───────────────────────────── 登录态 ─────────────────────────────

  function showLogin() {
    $('#app-view').hidden = true;
    $('#login-view').hidden = false;
    $('#login-token').focus();
  }

  function showApp() {
    $('#login-view').hidden = true;
    $('#app-view').hidden = false;
  }

  async function sha256Hex(text) {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
    return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join('');
  }

  /** 优先用一次性挑战做应答，让原始 Token 不过网；没有 WebCrypto 时退回明文。 */
  async function submitToken(token) {
    if (window.crypto && window.crypto.subtle) {
      const challenge = await api('/api/login/challenge', { method: 'POST' });
      const proof = await sha256Hex(`${challenge.nonce}:${token}`);
      return api('/api/login', { method: 'POST', body: { nonce: challenge.nonce, proof } });
    }
    return api('/api/login', { method: 'POST', body: { token } });
  }

  /** 按时间来一句问候——门口站的是她本人，不是一块表单。 */
  function greetByHour() {
    const hour = new Date().getHours();
    if (hour < 5) return '这么晚还没睡呀';
    if (hour < 11) return '早上好';
    if (hour < 14) return '中午好';
    if (hour < 18) return '下午好';
    if (hour < 23) return '晚上好';
    return '夜深了';
  }

  function showLoginError(message) {
    const card = $('#login-form');
    const error = $('#login-error');
    error.textContent = message;
    error.hidden = false;
    // 重放抖动动画：先摘掉类、强制一次重排再挂回去。
    card.classList.remove('shake');
    void card.offsetWidth;
    card.classList.add('shake');
  }

  (() => {
    const greeting = $('#login-greeting');
    if (greeting) greeting.textContent = greetByHour();

    const reveal = $('#login-reveal');
    const tokenInput = $('#login-token');
    if (reveal && tokenInput) {
      reveal.addEventListener('click', () => {
        const shown = tokenInput.type === 'text';
        tokenInput.type = shown ? 'password' : 'text';
        reveal.textContent = shown ? '👁' : '🙈';
        tokenInput.focus();
      });
    }

    const themeButton = $('#login-theme');
    if (themeButton) {
      themeButton.addEventListener('click', () => {
        applyTheme(document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark');
      });
    }
  })();

  $('#login-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const error = $('#login-error');
    error.hidden = true;
    const button = $('#login-submit');
    const spinner = button ? button.querySelector('.login-spinner') : null;
    button.disabled = true;
    if (spinner) spinner.hidden = false;
    try {
      await submitToken($('#login-token').value);
      $('#login-token').value = '';
      showApp();
      await refreshAll();
    } catch (problem) {
      showLoginError(problem.message);
    } finally {
      button.disabled = false;
      if (spinner) spinner.hidden = true;
    }
  });

  $('#logout').addEventListener('click', async () => {
    try { await api('/api/logout', { method: 'POST' }); } catch (_) { /* 忽略 */ }
    config.dirty.clear();
    showLogin();
  });

  // ───────────────────────────── 主题 ─────────────────────────────

  function applyTheme(theme) {
    document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem('yunxi-admin-theme', theme);
    } catch (_) {
      /* 隐私模式下写不了，忽略 */
    }
    const label = $('#theme-label');
    if (label) label.textContent = theme === 'dark' ? '浅色' : '深色';
  }

  $('#theme-toggle').addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    applyTheme(next);
    // 有几处颜色是**渲染时按当前主题算出来**的（记忆页的实体 chip、模型页档案卡的首字），
    // 换主题不重画的话它们会停在上一个主题的那一版。
    if (currentPage === 'memory' || currentPage === 'model') goto(currentPage);
  });

  // ───────────────────────────── 页面切换 ─────────────────────────────

  const PAGES = {
    overview: { title: '概览', subtitle: '进程、存储、模型与调度器的现状', render: renderOverview },
    config: { title: '配置', subtitle: '全部参数；保存前会校验，保存时保留注释', render: renderConfigPage },
    memory: { title: '记忆', subtitle: '长期记忆、情节、人物与未完结线索', render: renderMemoryPage },
    model: { title: '模型', subtitle: '换服务商 / 换模型 / 填密钥 / 测连通性', render: renderModelPage },
    stickers: { title: '表情包', subtitle: '她能发出去的素材：上传、查看、删除', render: renderStickerPage },
    annotation: { title: '标注', subtitle: 'TurnGate 待复核样本：标完直接导出训练集', render: renderAnnotationPage },
    system: { title: '系统', subtitle: '主机、进程、模型与 OneBot 服务端', render: renderSystemPage },
  };

  let currentPage = 'overview';
  // 首次进入用 replaceState（不污染历史），之后的切换用 pushState（返回键可用）。
  let hashInitialized = false;

  function pageFromHash() {
    const [name, sub] = (location.hash || '').replace(/^#\/?/, '').split('/');
    if (PAGES[name] && sub) applyMemorySubPage(sub);
    return PAGES[name] ? name : 'overview';
  }

  /** `#/memory/table`、`#/memory/timeline`、`#/memory/people` 可以直接打开。 */
  function applyMemorySubPage(sub) {
    if (sub === 'people') {
      memory.tab = 'people';
      return;
    }
    if (['constellation', 'table', 'timeline'].includes(sub)) {
      memory.tab = 'records';
      memory.view = sub;
    }
  }

  // 渲染排成一条链，且只画最新那一页。
  //
  // 页面的渲染是"先清空、再 await 取数、再 append"。两次渲染重叠时（双击侧栏、
  // 点统计卡上的链接、刷新按钮点两下），后一次的清空会赶在前一次的 append 之前，
  // 页面上就出现两套卡片、两张表叠在一起。这里做两件事：渲染串行（不会重叠），
  // 以及在轮到自己时检查序号（被更新的一次导航顶掉的渲染直接跳过，不白画一遍）。
  let renderEpoch = 0;
  let renderChain = Promise.resolve();

  async function goto(page) {
    const epoch = ++renderEpoch;
    if (page !== 'system') stopSystemRefresh();
    // 概览页的自动刷新同理：离开就停，不在看不见的页面上白刷 /api/status。
    if (page !== 'overview') stopOverviewRefresh();
    // 离开配置页就把滚动监听的引用放掉，别让它抱着已经摘下来的按钮。
    if (page !== 'config' && config.spyCleanup) {
      config.spyCleanup();
      config.spyCleanup = null;
    }
    currentPage = page;
    if (pageFromHash() !== page) {
      const url = `#/${page}`;
      if (hashInitialized) history.pushState(null, '', url);
      else history.replaceState(null, '', url);
    }
    for (const button of document.querySelectorAll('.nav-item[data-page]')) {
      button.classList.toggle('active', button.dataset.page === page);
    }
    for (const key of Object.keys(PAGES)) {
      $(`#page-${key}`).hidden = key !== page;
    }
    $('#page-title').textContent = PAGES[page].title;
    $('#page-subtitle').textContent = PAGES[page].subtitle;
    document.title = `${PAGES[page].title} · 芸汐 管理后台`;
    const task = () => (epoch === renderEpoch ? PAGES[page].render() : undefined);
    const next = renderChain.then(task, task);
    // 链条本身不能因为某一次渲染失败而断掉，失败照常向上抛给调用方。
    renderChain = next.then(() => {}, () => {});
    await next;
  }

  for (const button of document.querySelectorAll('.nav-item[data-page]')) {
    button.addEventListener('click', () => goto(button.dataset.page));
  }

  // 深链接：`#/config`、`#/memory` 可以直接打开，浏览器前进/后退也能用。
  const followHash = () => {
    const page = pageFromHash();
    if (page !== currentPage) goto(page);
  };
  window.addEventListener('hashchange', followHash);
  window.addEventListener('popstate', followHash);

  $('#refresh').addEventListener('click', async (event) => {
    const glyph = event.currentTarget.querySelector('.icon');
    if (glyph) glyph.classList.add('spin');
    try {
      await refreshAll();
    } finally {
      if (glyph) glyph.classList.remove('spin');
    }
  });

  async function refreshAll() {
    await refreshHealth();
    await goto(currentPage);
  }

  /** 首次进入：先按地址栏里的 hash 决定落在哪一页，再拉数据。 */
  async function boot() {
    await refreshHealth();
    await goto(pageFromHash());
    hashInitialized = true;
  }

  // 跨越窄屏断点（转屏、拖窗口）时重画当前页：标注页的工具栏在窄屏下是折叠的、
  // 队列也换了顺序，这些取舍在渲染时就定死了，光靠 CSS 变不回来。
  window.matchMedia(NARROW_QUERY).addEventListener('change', () => {
    if (currentPage === 'annotation') renderAnnotationPage();
    if (currentPage === 'config') syncToolbarHeight();
  });

  // 配置页按 `/` 直接跳进搜索框——215 个字段里找一项时，这是最短的一条路。
  document.addEventListener('keydown', (event) => {
    if (currentPage !== 'config') return;
    if (event.key !== '/' || event.metaKey || event.ctrlKey || event.altKey) return;
    const tag = event.target && event.target.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
    const box = $('#config-search');
    if (!box) return;
    event.preventDefault();
    box.focus();
    box.select();
  });

  /** 顶栏状态胶囊和概览页要的是同一份 /api/status：短时间内的第二次直接复用，
   *  否则点一下"刷新"会对着同一个接口打两遍。传 0 可以强制拿新的。 */
  let statusCache = { at: 0, value: null };

  async function fetchStatus(maxAgeMs = 1500) {
    const now = Date.now();
    if (statusCache.value && now - statusCache.at < maxAgeMs) return statusCache.value;
    const value = await api('/api/status');
    statusCache = { at: Date.now(), value };
    return value;
  }

  async function refreshHealth() {
    const pill = $('#health-pill');
    try {
      // 顶栏永远拿新的，顺便把缓存刷新掉，紧接着的概览页就能直接复用。
      const status = await fetchStatus(0);
      const database = status.database || {};
      const redis = status.redis || {};
      const ok = database.ok && redis.ok;
      // 这里要用**进程**运行时长（`process_uptime`），不是主机开机时长：
      // 部署后第一眼就是看这个数字有没有归零。
      setPill(pill, ok ? 'ok' : 'bad', ok
        ? `运行中 · ${status.process_uptime || status.uptime || '—'}`
        : (database.ok ? 'Redis 不可用' : '数据库不可用'));
      $('#brand-revision').textContent = `${status.version} · ${String(status.revision).slice(0, 8)}`;
      overviewCache = status;
    } catch (problem) {
      setPill(pill, 'bad', problem.message);
      pill.title = problem.message;
    }
  }

  // ───────────────────────────── 概览 ─────────────────────────────
  //
  // 三块，回答三个问题：她还在正常跑吗（运行状态）→ 她记得多少东西（记忆规模）
  // → 她现在能用哪几种方式说话（模型与能力）。
  //
  // 排版上有一条硬规矩：**能一眼比较的数才配当大数字卡**。状态（"正常"）、路径
  // （配置文件在哪）、来源说明这些当大数字只会占地方还读不出来，一律走行式排版；
  // 记忆那几张卡的角标也换成一句人话（"共 4196 条：记忆 639 · 旧版 2901…"），
  // 塞在卡片小字里折成三行没人看。

  let overviewCache = null;

  function stat(label, value, extra, small) {
    return h('div', { class: 'stat' },
      h('div', { class: 'label', text: label }),
      h('div', { class: `value${small ? ' small' : ''}`, text: value }),
      extra ? h('div', { class: 'extra', text: extra }) : null);
  }

  /** 可点进某一页的 stat：运维看到数字后的下一个动作几乎总是"去看细节"。 */
  function statLink(label, value, extra, page) {
    const node = stat(label, value, extra);
    const open = () => goto(page);
    node.classList.add('clickable');
    node.title = `打开${PAGES[page] ? PAGES[page].title : page}页`;
    // 一个可点的 div 对键盘和读屏都是隐形的：补上角色、焦点与 Enter/Space。
    node.setAttribute('role', 'button');
    node.setAttribute('tabindex', '0');
    node.addEventListener('click', open);
    node.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        open();
      }
    });
    return node;
  }

  /** `/api/status` 的 process 是一句给人看的话（"芸汐进程内存: 728 MB"）：剥掉标签。 */
  function processMemoryText(text) {
    const raw = String(text || '').trim();
    if (!raw) return '—';
    return raw.replace(/^芸汐进程内存[:：]\s*/, '') || '—';
  }

  /** 服务那一行：状态点 + 结论 + 细节。比一张写着"正常"的大数字卡好读。
   *
   *  `tone` 只在出错时有意义，而且必须由调用方指定：PostgreSQL 挂了是"坏了"
   *  （记忆读写全废，红），Redis 挂了只是"退化了"（回退进程内状态，琥珀）——
   *  两个都用红会让人以为整机出事。 */
  function serviceLine(ok, statusText, detail, tone = 'danger') {
    const level = ok ? 'ok' : tone;
    return h('span', { class: 'service-line' },
      h('span', { class: `dot ${level === 'ok' ? 'ok' : (level === 'warn' ? 'warn' : 'off')}` }),
      h('strong', { class: `${level}-text`, text: statusText }),
      detail ? h('span', { class: 'muted', text: detail }) : null);
  }

  /** 一个能力开关。开着是绿的、关掉的压暗，一眼扫出她现在能用哪几种方式说话。 */
  function capChip(label, on, detail) {
    return h('span', { class: `cap-chip ${on ? 'on' : 'off'}`, title: detail ? `${label}：${detail}` : label },
      h('span', { class: 'cap-dot' }),
      h('span', { class: 'cap-label', text: label }),
      detail ? h('span', { class: 'cap-detail', text: detail }) : null);
  }

  function capabilityChips(scheduler) {
    const vision = String(scheduler.vision_provider || '');
    const stickerCount = Number(scheduler.sticker_files) || 0;
    const stickerOn = Boolean(scheduler.sticker_enabled);
    // 表情包是唯一一个"开关开着但还缺素材"的能力：库里是空的时候给个直接的入口。
    const sticker = [capChip('表情包', stickerOn, stickerOn ? (stickerCount ? `${stickerCount} 张` : '素材库为空') : '')];
    if (stickerOn && !stickerCount) {
      sticker.push(h('button', {
        class: 'link-toggle', type: 'button', text: '去上传', onclick: () => goto('stickers'),
      }));
    }
    return [
      capChip('主动消息', Boolean(scheduler.proactive_enabled)),
      capChip('群聊接话', Boolean(scheduler.group_interjection_enabled)),
      capChip('工具', Boolean(scheduler.tools_enabled)),
      capChip('实时通话', Boolean(scheduler.qq_call_enabled)),
      capChip('语音消息', Boolean(scheduler.voice_enabled)),
      capChip('唱歌', Boolean(scheduler.sing_enabled)),
      capChip('图片理解', Boolean(vision) && vision !== 'disabled', vision === 'disabled' ? '' : vision),
      ...sticker,
    ];
  }

  // ───────────────────────────── 等待房间 ─────────────────────────────
  //
  // 概览页的「等待房间」：她有没有卡住，一眼就能看出来。
  //
  // 为什么单独做这一块：2026-09-15 那次群 641996763 静了十个多小时，journal 里只有
  // "排队 N 次、排空 0 次"，没有任何一行能指出卡在哪一步——最后是重启才恢复的。
  // 后端为此记了每个会话的队列深度、排空活性与"当前回合走到哪一步"，这里把它摊开。
  //
  // 两条硬约定：
  //   1. **空闲时就一行字**（"空闲：没有排队，也没有在途回合"）。卡片长期占一大块
  //      版面却什么都不说，会让人开始无视它，等真出事时也不看。
  //   2. 消息正文只显示后端截好的 48 字摘要，且跟着"谁在等"一起展示——后台本来
  //      就能读全部记忆与配置，这里不搬运全文。

  /** 等待时长：秒级给秒，超过一分钟给"分+秒"，再长给"时+分"。戳在标题里给精确秒数。 */
  function formatWait(seconds) {
    if (seconds === null || seconds === undefined) return '—';
    const total = Math.max(0, Math.round(Number(seconds) || 0));
    if (total < 60) return `${total} 秒`;
    if (total < 3600) {
      // 整分钟不补"0 秒"：阈值默认就是 180/600 秒这种整数，写成"10 分 0 秒"太啰嗦。
      const seconds = total % 60;
      return seconds === 0 ? `${total / 60} 分` : `${Math.floor(total / 60)} 分 ${seconds} 秒`;
    }
    const minutes = Math.floor((total % 3600) / 60);
    return minutes === 0
      ? `${Math.floor(total / 3600)} 时`
      : `${Math.floor(total / 3600)} 时 ${minutes} 分`;
  }

  /** "排队 3 · 最老 12 秒"这一行，也用于顶部提醒。 */
  function waitingRoomSummary(info) {
    const stuck = Number(info.stuck || 0);
    const scopes = info.scopes || [];
    const queued = scopes.reduce((sum, item) => sum + Number(item.queued || 0), 0);
    if (stuck > 0) {
      const worst = scopes.find((item) => item.stuck) || {};
      return `${stuck} 个会话疑似卡住${worst.subject_id ? `（${worst.kind === 'group' ? '群' : ''}${worst.subject_id}）` : ''}`;
    }
    if (!scopes.length) return '空闲：没有排队，也没有在途回合';
    if (queued > 0) return `${scopes.length} 个会话有在途活动 · 排队共 ${queued} 条`;
    return `${scopes.length} 个会话正在生成回复`;
  }

  function waitingRoomCard(info) {
    const scopes = info.scopes || [];
    const stuck = Number(info.stuck || 0);
    const idle = scopes.length === 0;
    const auto = Boolean(info.auto_reclaim_enabled);
    // 单轮硬上限（0 = 关闭）。它和"自动回收"是两件事：那个管"多久没动静算死"，
    // 这个管"一轮最多活多久"——每一步都还在慢慢动的长回合只有它能兜住。
    const deadline = Number(info.turn_deadline_secs || 0);
    return h('div', { class: `card wait-card${stuck > 0 ? ' stalled' : ''}` },
      h('div', { class: 'card-head' },
        h('h3', { text: '等待房间' }),
        // 一键切换"影子档 ↔ 自动回收"。放在这里而不是只留在「配置」页：看到
        // "疑似卡住"的地方就是这张卡片，切档也该在这里，不该让人去 249 个字段里找。
        h('button', {
          class: `btn ghost small${auto ? ' wait-toggle-on' : ''}`,
          text: auto ? '关闭自动回收' : '开启自动回收',
          title: auto
            ? '关掉之后只记账、只打 [STALL] 日志，卡住了需要人来点「回收这一轮」'
            : '开启后：静默超过阈值且没有推进的回合会被自动判死（队列里的原文会补答）。'
              + '先确认 [STALL] 日志里 would_reclaim=true 没有误报再开。',
          onclick: (event) => toggleAutoReclaim(!auto, event.currentTarget),
        }),
        h('span', {
          class: `pill ${stuck > 0 ? 'bad' : (idle ? '' : 'ok')}`,
          title: `静默超过 ${formatWait(info.stalled_after_secs)} 判为疑似卡住`,
        }, h('span', { class: 'pill-text', text: stuck > 0 ? `${stuck} 个疑似卡住` : (idle ? '空闲' : '在跑') }))),
      h('div', { class: 'wait-body' },
        idle
          ? h('div', { class: 'empty', text: waitingRoomSummary(info) })
          : h('div', { class: 'wait-list' }, scopes.map(waitingRoomRow))),
      h('div', { class: 'hint wait-foot' },
        `静默超过 ${formatWait(info.stalled_after_secs)} 判为疑似卡住；空闲会话不列出。`
        + (deadline > 0
          ? `单轮上限 ${formatWait(deadline)}：活过这个时长的回合会被直接判死。`
          : '单轮上限未开启（traffic.turn_deadline_secs = 0）。')
        + (auto
          ? `自动回收已开启（超过 ${formatWait(info.reclaim_after_secs)} 的卡死回合会被自动判死）。`
          : `自动回收未开启：目前只记账、只打 [STALL] 日志，卡住了需要人来点「回收这一轮」。`)));
  }

  /** 影子档 ↔ 自动回收：改一个配置键，立刻生效。
   *
   *  走的是「配置」页同一个接口（`/api/config/patch`），只是把入口挪到看得到症状的
   *  地方；`traffic` 不在 `restart_sections` 里，看门狗每一轮都用 `config::get()`
   *  现取，所以**不用重启进程**，下一个扫描周期就按新档位跑。
   *
   *  开启要二次确认：它是唯一会主动丢掉"已生成未发出"回复的开关。  */
  async function toggleAutoReclaim(enabled, button) {
    if (enabled) {
      const ok = window.confirm(
        '开启自动回收？\n\n'
        + '· 静默超过阈值、确认没有推进的回合会被自动判死\n'
        + '· 那一轮已生成但没发出去的回复会被丢弃（不补发）\n'
        + '· 队列里排着的消息不受影响，会按顺序补答\n'
        + '· 立刻生效，不需要重启进程\n\n'
        + '建议先确认 [STALL] 日志里 would_reclaim=true 没有误报。');
      if (!ok) return;
    }
    button.disabled = true;
    try {
      // 首选运行时覆盖配置（生产上它落在可写目录，且发布新版本不会把它冲掉）；
      // 开发机上那个文件可能还不存在，写不进去就退回主配置——两种部署都能切成。
      const changes = { 'traffic.turn_reclaim_enabled': enabled };
      try {
        await api('/api/config/patch', {
          method: 'POST',
          body: { file: 'bot.conf.override.toml', changes },
        });
      } catch (overrideProblem) {
        try {
          await api('/api/config/patch', {
            method: 'POST',
            body: { file: 'bot.conf.toml', changes },
          });
        } catch (mainProblem) {
          throw new Error(`覆盖配置与主配置都写不进去（${overrideProblem.message} / ${mainProblem.message}）`);
        }
      }
      toast(enabled
        ? '自动回收已开启：卡死的回合会被自动判死，队列里的消息会补答'
        : '自动回收已关闭：只记账、只打 [STALL] 日志', enabled ? 'warn' : 'ok', 8000);
      await fetchStatus(0);
      updateWaitingRoomCard((statusCache.value && statusCache.value.waiting_room) || {});
    } catch (problem) {
      toast(`切换失败：${problem.message}`, 'bad', 9000);
    } finally {
      button.disabled = false;
    }
  }

  /** 回收一个卡死的回合：人判断、机器执行。
   *
   *  为什么要有按钮：等待房间能看出卡住、日志能证明卡在哪一步，但真出事时最终
   *  往往是重启进程——那会清掉**所有**会话的排队。这个按钮只动一个会话，而且只动
   *  "回合"不动"消息"（队列里的原文会按顺序补答）。
   *
   *  先 `confirm` 说清代价再发请求：它确实会丢掉那一轮已经生成的回复，这个决定
   *  不该由一次误点做出。  */
  async function reclaimTurn(item, button) {
    const label = item.kind === 'group' ? '群' : '会话';
    const ok = window.confirm(
      `回收 ${label} ${item.subject_id} 卡住的回合？\n\n`
      + `· 这一轮已经生成但没发出去的回复会被丢弃（不会补发）\n`
      + `· 队列里排着的消息保持不动，会按顺序补答\n`
      + `· 只影响这一个会话，其他会话不受影响`);
    if (!ok) return;
    button.disabled = true;
    try {
      const result = await api('/api/waiting-room/reclaim', {
        method: 'POST',
        body: { kind: item.kind, subject_id: item.subject_id, generation: item.reply && item.reply.generation },
      });
      toast(result.detail || '已回收', 'ok', 8000);
      // 立刻重拉：这个会话应当马上从"疑似卡住"变成在跑或空闲。
      await fetchStatus(0);
      updateWaitingRoomCard((statusCache.value && statusCache.value.waiting_room) || {});
    } catch (problem) {
      toast(`回收失败：${problem.message}`, 'bad', 9000);
    } finally {
      button.disabled = false;
    }
  }

  /** 一行一个会话。字段与 `/api/status` 的 waiting_room 一一对应，不在这里二次推断。 */
  function waitingRoomRow(item) {
    const replied = item.reply || {};
    const ticket = item.ticket || {};
    const label = item.kind === 'group' ? '群' : (item.kind === 'private' ? '私聊' : (item.kind === 'call' ? '通话' : '任务'));
    const stepText = item.turn_step ? `停在 ${item.turn_step}` : '';
    // 详情放在 title 里：卡片要能一眼扫过，但出事时又必须查得到数字。
    const detail = [
      `静默 ${item.drain_last_progress_secs ?? '—'} 秒`,
      `回合已 ${item.turn_waiting_secs ?? '—'} 秒`,
      `当前步骤 ${stepText || '—'}`,
      ticket.generation !== undefined ? `票据代数 ${ticket.generation}` : null,
      `已生成未发出的回复 ${replied.prepared_outgoing ?? 0} 条`,
      `待定入站 ${replied.pending_incoming ?? 0} · 活跃预留 ${replied.active_incoming ?? 0}`,
    ].filter(Boolean).join(' · ');

    return h('div', { class: `wait-row${item.stuck ? ' bad' : ''}`, title: detail },
      h('div', { class: 'wait-who' },
        h('span', { class: 'wait-kind', text: label }),
        h('span', { class: 'wait-id mono', text: String(item.subject_id) }),
        item.stuck ? null : h('span', { class: 'wait-state', text: item.drain_active ? '在跑' : '排队' })),
      h('div', { class: 'wait-metrics' },
        h('span', { class: 'wait-metric' },
          h('span', { class: 'wait-metric-label', text: '排队' }),
          h('span', { class: 'wait-metric-value mono', text: String(item.queued ?? 0) })),
        h('span', { class: 'wait-metric' },
          h('span', { class: 'wait-metric-label', text: '最老' }),
          h('span', { class: 'wait-metric-value mono', text: formatWait(item.oldest_queued_secs) })),
        h('span', { class: 'wait-metric' },
          h('span', { class: 'wait-metric-label', text: '静默' }),
          h('span', { class: 'wait-metric-value mono', text: formatWait(item.drain_last_progress_secs) })),
        stepText ? h('span', { class: 'wait-step mono', text: stepText, title: `这一步已停 ${item.turn_step_secs ?? '—'} 秒` }) : null),
      // 卡住的行才给按钮：正常在跑的会话不该出现"强行取消"的入口。
      item.stuck
        ? h('div', { class: 'wait-actions' },
            h('button', {
              class: 'btn ghost small',
              text: '回收这一轮',
              title: '判死卡住的回合，让这个会话立刻能重新接话；队列里的消息保持不动',
              onclick: (event) => reclaimTurn(item, event.currentTarget),
            }))
        : null,
      h('div', { class: 'wait-why' },
        h('span', { text: item.stuck_reason || item.summary || '' }),
        !item.stuck && item.oldest_sender
          ? h('span', { class: 'wait-waiting', text: ` · ${item.oldest_sender}：${item.oldest_preview || ''}` })
          : null));
  }

  async function renderOverview() {
    const page = $('#page-overview');
    clear(page);
    page.append(h('div', { class: 'loading', text: '读取状态…' }));

    // 记忆条目与状态并行取：这两件事互不依赖，串起来只是让首屏白等第二次往返。
    // 失败不抛给外层——下面显示一行"暂时读不到"就够了，不该把整页带走。
    const recentPromise = api('/api/memory/records?limit=10').catch(() => ({ items: [] }));

    let status;
    try {
      status = await fetchStatus();
      overviewCache = status;
    } catch (problem) {
      clear(page);
      page.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    clear(page);

    // 要人动手的事排在最前面：库连不上、模型没密钥、有改动等重启——这些都不该
    // 埋在卡片里让人自己找。外面套一层，自动刷新时只动这一层。
    const notices = h('div', { class: 'overview-notices' });
    for (const note of overviewNotices(status)) notices.append(note);
    if (notices.childElementCount) page.append(notices);

    page.append(runtimeCard(status));
    page.append(waitingRoomCard(status.waiting_room || {}));
    page.append(memoryCard(status.counts || {}));

    const recent = await recentPromise;
    const list = h('div', { class: 'record-list' });
    if (!(recent.items || []).length) {
      list.append(h('div', { class: 'empty', text: '暂时读不到记忆记录' }));
    }
    for (const item of recent.items || []) {
      list.append(recordNode(item, () => { goto('memory').then(() => openRecord(item)); }, false));
    }

    page.append(h('div', { class: 'overview-split' },
      modelCard(status.model || {}, status.scheduler || {}),
      h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: '最近的记忆变化' }),
          h('button', { class: 'btn ghost small', text: '去记忆页', onclick: () => goto('memory') })),
        list)));

    startOverviewRefresh();
  }

  /** 概览页的自动刷新：等待房间是"正在发生的事"，不刷就等于摆设。
   *
   *  为什么是 15 秒：后端每 30 秒扫一次等待房间（`traffic.window_drain_sweep_secs`
   *  默认 30），比它更快只会白拉；而"卡住"的判定阈值是分钟级，晚十几秒发现不影响
   *  处置。系统页每 5 秒是整块重绘（会打断阅读），这里**只换卡片**，别的卡片不动。
   *
   *  两个必须处理的边角：
   *  1. 离开概览页时若还有在途请求，必须丢弃它的结果——否则它会把新鲜的
   *     `overviewCache` 覆盖成旧的（"切走再切回来，数据反而变旧"）。
   *  2. 同一时刻只允许一个刷新在途，慢请求不会叠成一串。 */
  let overviewTimer = null;
  let overviewInFlight = null;

  function stopOverviewRefresh() {
    if (overviewTimer !== null) {
      clearInterval(overviewTimer);
      overviewTimer = null;
    }
    // 让在途请求的返回值作废：它回来时会发现自己的票已经不是当前那张。
    if (overviewInFlight !== null) overviewInFlight = null;
  }

  function startOverviewRefresh() {
    stopOverviewRefresh();
    overviewTimer = setInterval(async () => {
      if (currentPage !== 'overview') {
        stopOverviewRefresh();
        return;
      }
      // 标签页在后台时不刷：没人看，白占 CPU 与网络。
      if (document.hidden || overviewInFlight !== null) return;
      const ticket = Date.now();
      overviewInFlight = ticket;
      try {
        // 走 fetchStatus：它顺带刷新缓存，用户手点"刷新"或切走再切回时可以直接复用。
        const fresh = await fetchStatus(0);
        if (overviewInFlight !== ticket) return;
        overviewInFlight = null;
        updateWaitingRoomCard(fresh.waiting_room || {});
      } catch (_) {
        /* 自动刷新失败不打扰用户，下一轮再试 */
        if (overviewInFlight === ticket) overviewInFlight = null;
      }
    }, 15000);
  }

  /** 原地替换那两张与等待房间有关的卡片。
   *
   *  找不到位置（用户正在别的页、页面还没画完）就什么都不做——**绝不整页重绘**：
   *  那会把人正在看的记录列表与滚动位置一起冲掉。 */
  function updateWaitingRoomCard(info) {
    if (currentPage !== 'overview') return;
    const page = $('#page-overview');
    const live = page.querySelector('.wait-card');
    if (live) live.replaceWith(waitingRoomCard(info));
    const notices = page.querySelector('.overview-notices');
    if (!notices) return;
    for (const node of notices.querySelectorAll('[data-stall-notice]')) node.remove();
    const stallNotice = overviewNotices({ waiting_room: info })
      .find((node) => node.dataset.stallNotice !== undefined);
    if (stallNotice) notices.prepend(stallNotice);
  }

  /** 需要动手的几件事，按"严重到不严重"排。没有就什么都不返回。 */
  function overviewNotices(status) {
    const database = status.database || {};
    const redis = status.redis || {};
    const model = status.model || {};
    const pending = (status.admin && status.admin.pending_restart) || [];
    const notes = [];

    if (!database.ok) {
      notes.push(h('div', { class: 'restart-note bad' },
        icon('alert'),
        h('strong', { text: 'PostgreSQL 不可用：' }),
        database.detail || '记忆的读写会全部失败，先确认数据库与 DATABASE_URL。'));
    }
    if (!redis.ok) {
      // Redis 掉了不影响她说话，只影响可重建的运行态，所以是提醒不是告警。
      notes.push(h('div', { class: 'restart-note' },
        icon('info'),
        h('strong', { text: 'Redis 不可用：' }),
        redis.detail || '撤回候选、等待图片与点名限流会退回进程内状态，重启后丢失。'));
    }
    if (model.enabled !== false && model.has_key === false) {
      notes.push(h('div', { class: 'restart-note' },
        icon('info'),
        h('strong', { text: '模型没有可用密钥：' }),
        '外部模型调不通，她只能退回本地能力；去「模型」页填一把，或检查环境变量 ',
        h('code', { text: model.api_key_env || '（未配置 api_key_env）' }), '。'));
    }
    if (pending.length) {
      notes.push(h('div', { class: 'restart-note' },
        icon('info'),
        `有 ${pending.length} 个分区的改动已保存，但要重启进程才生效：`,
        h('strong', { text: pending.join('、') }),
        '（按你的部署方式重启服务，例如 systemctl restart kovi-bot）。'));
    }
    // 卡住的会话排在最前：它意味着"群里有人说话、她一个字都没回"，是这几条里
    // 唯一正在伤害用户的那条。判定口径与阈值由后端统一给，前端不自己算。
    const room = status.waiting_room || {};
    const stuckScopes = (room.scopes || []).filter((scope) => scope.stuck);
    if (stuckScopes.length) {
      const worst = stuckScopes[0];
      const where = `${worst.kind === 'group' ? '群' : '会话'} ${worst.subject_id}`;
      const note = h('div', { class: 'restart-note bad' },
        icon('alert'),
        h('strong', { text: `疑似卡住 ${stuckScopes.length} 个会话：` }),
        `${where} 已静默 ${formatWait(worst.drain_last_progress_secs)}`,
        worst.turn_step ? `，停在 ${worst.turn_step}` : '',
        '——等待房间那一栏有完整数字；急着重启进程即可清空排队（',
        h('code', { text: 'systemctl restart kovi-bot' }),
        '）。');
      // 打上标记：自动刷新时按它替换，不会误删"模型没密钥"那几条。
      note.dataset.stallNotice = '1';
      notes.unshift(note);
    }
    return notes;
  }

  /** 运行状态：三张数字卡（进程 / 内存 / 主机）+ 四行事实（服务、配置、后台）。 */
  function runtimeCard(status) {
    const database = status.database || {};
    const redis = status.redis || {};
    const admin = status.admin || {};
    const configInfo = status.config || {};
    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', { text: '运行状态' }),
        h('span', { class: 'hint mono', text: `v${status.version || '—'} · ${String(status.revision || '未知').slice(0, 8)}` })),
      h('div', { class: 'stat-grid cols-3' },
        // 进程时长与主机开机时长必须分开写：线上这台机器已经开了 6 天，而芸汐可能
        // 5 分钟前刚发完版。混在一起显示会让人以为新版本没生效。
        stat('芸汐进程', status.process_uptime || '—', `pid ${status.pid ?? '—'}`),
        stat('进程内存', processMemoryText(status.process), '常驻内存'),
        stat('主机运行', status.host_uptime || status.uptime || '—', '机器开机时长')),
      h('dl', { class: 'kv overview-facts' },
        h('dt', { text: 'PostgreSQL' }),
        h('dd', {}, serviceLine(database.ok, database.ok ? '正常' : '异常',
          database.ok ? fmtBytes(database.size_bytes) : (database.detail || '—'), 'danger')),
        h('dt', { text: 'Redis' }),
        h('dd', {}, serviceLine(redis.ok, redis.ok ? '正常' : '异常', redis.detail || '—', 'warn')),
        h('dt', { text: '配置文件' }),
        h('dd', {}, h('span', {
          class: 'mono ellipsis',
          title: configInfo.path || '',
          text: `${configInfo.path || '—'} · ${fmtBytes(configInfo.bytes)} · 修改于 ${configInfo.modified || '—'}`,
        })),
        h('dt', { text: '管理后台' }),
        h('dd', { text: `${admin.sessions ?? 0} 个会话 · 已运行 ${formatDuration(admin.uptime_secs)}` })));
  }

  /** 记忆规模：五张能互相比较的数字卡，底下补一行"总数由哪几档构成"。 */
  function memoryCard(counts) {
    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', { text: '记忆规模' }),
        h('span', { class: 'hint', text: '全部由 PostgreSQL 持久化' })),
      h('div', { class: 'stat-grid cols-5' },
        statLink('长期记忆', String(counts.memories ?? '—'), 'Memory v2 长期条目', 'memory'),
        statLink('情节', String(counts.episodes ?? '—'), 'Mind Episode', 'memory'),
        statLink('人物', String(counts.people ?? '—'), 'canonical Person', 'memory'),
        statLink('目标', String(counts.goals ?? '—'), 'Mind Agenda', 'memory'),
        statLink('未完结线索', String(counts.open_loops ?? '—'), 'Open Loop', 'memory')),
      h('div', { class: 'card-foot' },
        h('span', { class: 'hint', text: memoryBreakdown(counts) })));
  }

  /** 模型与能力：四行现状 + 一排能力开关。 */
  function modelCard(model, scheduler) {
    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', {}, '模型与能力',
          model.enabled === false ? h('span', { class: 'pill', text: '外部模型已关闭' }) : null),
        h('button', { class: 'btn ghost small', text: '去模型页', onclick: () => goto('model') })),
      h('dl', { class: 'kv overview-facts flush' },
        h('dt', { text: '当前模型' }), h('dd', {}, h('strong', { text: model.model_name || '—' })),
        h('dt', { text: '接口地址' }),
        h('dd', {}, h('span', { class: 'mono ellipsis', title: model.endpoint || '', text: model.endpoint || '—' })),
        h('dt', { text: '模型密钥' }),
        h('dd', { class: model.has_key ? 'ok-text' : 'warn-text', text: model.key_source_text || '未知' }),
        h('dt', { text: '本地能力' }),
        h('dd', {
          text: `Intrinsic ${model.intrinsic_enabled ? '启用' : '关闭'} · TurnGate ${model.turn_gate_mode || '—'}`,
        })),
      h('div', { class: 'cap-block' },
        h('div', { class: 'cap-title', text: '表达能力' }),
        h('div', { class: 'cap-grid' }, ...capabilityChips(scheduler))));
  }

  /** 记忆总量的构成：`total` 里含旧版记忆，只写"639 / 共 4196"容易被读成矛盾。 */
  function memoryBreakdown(counts) {
    const byKind = counts.by_kind || {};
    // `memory` 必须排第一：它就是那张卡片的数字本身，落到"其它"里会让人找不到。
    const names = [
      ['memory', '记忆'], ['legacy_memory', '旧版'], ['episode', '情节'],
      ['user_profile', '用户档案'], ['agenda', '议程'], ['group_profile', '群档案'],
      ['summary', '摘要'],
    ];
    const shown = names
      .filter(([key]) => (byKind[key] || 0) > 0)
      .slice(0, 4);
    const parts = shown.map(([key, label]) => `${label} ${byKind[key]}`);
    // 只排除**真正列出来**的那几档：把"有名字但没列出来"的也算进"其它"，
    // 各档加起来才等于 total（否则总和对不上，一眼就看出这行在糊弄人）。
    const rest = Object.entries(byKind)
      .filter(([key, value]) => value > 0 && !shown.some(([known]) => known === key))
      .reduce((sum, [, value]) => sum + value, 0);
    if (rest > 0) parts.push(`其它 ${rest}`);
    const total = counts.total ?? '—';
    return parts.length ? `共 ${total} 条：${parts.join(' · ')}` : `共 ${total} 条记录`;
  }

  // ───────────────────────────── 配置 ─────────────────────────────

  const config = {
    files: [],
    main: 'bot.conf.toml',
    name: null,
    data: null,
    dirty: new Map(),
    filter: '',
    // 默认全部折叠：215 个字段铺开是一整屏都放不下的长页，
    // 先让人看见"有哪些分区"，要点开哪块再点哪块。
    expanded: new Set(),
    sectionPaths: [],
    /** 左栏分区导航的滚动监听清理函数（切页/重画时先放掉）。 */
    spyCleanup: null,
    /** 左栏当前高亮的分区按钮（只在变化时滚动它）。 */
    spyActive: null,
    /** 工具行里显示"命中多少分区 / 多少参数"的那个节点。 */
    matchNode: null,
    fieldShown: 0,
  };

  async function renderConfigPage() {
    const page = $('#page-config');
    if (!config.files.length) {
      const payload = await api('/api/config/files');
      config.files = payload.files;
      config.main = payload.main;
      config.restartSections = payload.restart_sections || [];
      config.name = config.name || config.main;
    }
    if (!config.data || config.data.name !== config.name) {
      try {
        await loadConfigFile(config.name);
      } catch (problem) {
        clear(page);
        page.append(h('div', { class: 'empty', text: `读取配置失败：${problem.message}` }));
        return;
      }
    }
    clear(page);
    const layout = h('div', { class: 'config-layout' });

    // 左栏：文件 + 分区导航
    const files = h('div', { class: 'file-list' });
    for (const file of config.files) {
      files.append(h('button', {
        class: `file-card${file.name === config.name ? ' active' : ''}`,
        onclick: async () => {
          config.dirty.clear();
          config.name = file.name;
          try {
            await renderConfigPage();
          } catch (problem) {
            toast(`切换到 ${file.name} 失败：${problem.message}`, 'bad');
          }
        },
      },
        h('strong', { text: file.title }),
        h('div', { class: 'file-meta', text: `${file.name} · ${file.exists ? fmtBytes(file.bytes) : '尚未创建'}` }),
        h('div', { class: 'file-meta', text: file.restart_required ? '改完需重启' : '保存后热加载' })));
    }
    const sidebar = h('div', { class: 'card config-side' }, files);
    layout.append(sidebar);

    const main = h('div', { class: 'config-main' });
    main.append(renderConfigToolbar());
    if (config.dirty.size) main.append(renderSaveBar());
    if (!config.data.writable) {
      main.append(h('div', { class: 'restart-note' },
        `这个文件在当前部署里是只读的（${config.data.path}）。生产环境把二进制与主配置放在只读的发布目录里，`,
        '网页上的改动请写进「运行时覆盖配置」——它落在可写的运行时目录，发布新版本不会把它冲掉。'));
    }
    if (config.data.restart_required) {
      main.append(h('div', { class: 'restart-note', text: '这个文件在启动时读取一次：保存会落盘，但要重启进程才会生效。' }));
    }
    const pending = (overviewCache && overviewCache.admin && overviewCache.admin.pending_restart) || [];
    if (pending.length) {
      main.append(h('div', { class: 'restart-note' },
        `待重启生效的分区：${pending.join('、')}`));
    }
    if (config.data.view === 'sparse' && config.data.typed) {
      main.append(h('div', { class: 'field-hint', style: 'padding: 0 4px', text: '这里只列出本文件真正写了的字段；同名字段会覆盖主配置，未列出的沿用主配置。' }));
    }
    const body = h('div', { class: 'config-sections' });

    main.append(body);
    layout.append(main);
    page.append(layout);

    config.sectionsEl = body;
    config.sidebarEl = sidebar;
    renderConfigSections(body, sidebar);
    syncToolbarHeight();
  }

  /** 工具行的高度会随搜索行换行变化（窄屏能折成两行），而吸顶的保存栏、
   *  分区跳转的滚动留白都按它定位。量一次写进 CSS 变量，免得写死像素。 */
  function syncToolbarHeight() {
    const toolbar = document.querySelector('#page-config .config-toolbar');
    if (!toolbar) return;
    const height = Math.round(toolbar.getBoundingClientRect().height);
    document.documentElement.style.setProperty('--config-toolbar-h', `${height}px`);
  }

  /** 只重画「分区列表 + 左栏导航」。
   *
   * 搜索框的 oninput 以前直接调 `renderConfigPage()`，整页重建会把输入框本身
   * 也换掉——焦点随之丢失，等于每敲一个字都要重新点一下搜索框。 */
  function rerenderConfigBody() {
    if (!config.sectionsEl || !config.sectionsEl.isConnected) {
      renderConfigPage();
      return;
    }
    clear(config.sectionsEl);
    renderConfigSections(config.sectionsEl, config.sidebarEl);
    syncExpandButton();
  }

  /** 「全部展开 / 全部折叠」按钮的字面要跟着状态走。 */
  function syncExpandButton() {
    const button = config.expandButton;
    if (!button) return;
    button.textContent = config.expanded.size ? '全部折叠' : '全部展开';
  }

  function renderConfigToolbar() {
    const data = config.data;
    const search = h('input', {
      class: 'input search', id: 'config-search',
      placeholder: '搜索参数名或说明…（按 / 聚焦）',
      value: config.filter,
      oninput: (event) => {
        config.filter = event.target.value.trim().toLowerCase();
        rerenderConfigBody();
      },
      onkeydown: (event) => {
        if (event.key === 'Escape' && config.filter) {
          event.preventDefault();
          search.value = '';
          config.filter = '';
          rerenderConfigBody();
        }
      },
    });
    const expandAll = (open) => {
      config.expanded = open ? new Set(config.sectionPaths) : new Set();
      // 按钮自己的字面也要跟着变，所以先存下引用。
      config.expandButton = button;
      rerenderConfigBody();
    };
    const button = h('button', {
      class: 'btn ghost small',
      text: config.expanded.size ? '全部折叠' : '全部展开',
      title: '分区默认是折起来的：这里可以一次全开或全收',
      onclick: () => expandAll(config.expanded.size === 0),
    });
    config.expandButton = button;
    // 命中数写在说明行里：搜索框一变，这里立刻跟着变，不必去数扇区。
    const match = h('span', { class: 'config-match' });
    config.matchNode = match;
    return h('div', { class: 'card tight config-toolbar' },
      h('div', { class: 'search-row' }, search,
        button,
        h('button', { class: 'btn ghost', text: '原始 TOML', onclick: openRawEditor }),
        h('button', { class: 'btn ghost', text: `备份 (${backupCount()})`, onclick: openBackups }),
        config.data.writable ? null : h('span', { class: 'badge secret', text: '只读' }),
        h('button', {
          class: 'btn ghost', text: '重新加载',
          title: '丢弃未保存的改动，按磁盘上的内容重读',
          onclick: async () => {
            try {
              await api('/api/config/reload', { method: 'POST' });
              config.dirty.clear();
              await loadConfigFile(config.name);
              toast('已按磁盘内容重新加载配置');
              await renderConfigPage();
            } catch (problem) { toast(problem.message, 'bad'); }
          },
        })),
      h('div', { class: 'config-meta' },
        h('span', { class: 'field-hint', text: data.description || '' }),
        match));
  }

  function backupCount() {
    return (config.files.find((file) => file.name === config.name) || {}).backups || 0;
  }

  async function loadConfigFile(name) {
    const payload = await api(`/api/config/file/${encodeURIComponent(name)}`);
    config.data = payload;
    config.name = name;
    config.dirty.clear();
  }

  /** 分区与字段渲染。filter 命中时只保留匹配的字段。 */
  function renderConfigSections(container, sidebar) {
    const data = config.data;
    const values = data.values || {};
    const nav = h('div', { class: 'section-nav' });
    const navButtons = new Map();
    const isTyped = data.typed;
    let shown = 0;
    // 顶层分区是稳定的，但重画时会收集一遍；这里只在第一次算。
    config.sectionPaths = [];
    config.fieldShown = 0;

    const sections = Object.keys(values);

    for (const key of sections) {
      const value = values[key];
      const sectionNode = renderSection([key], key, value, isTyped);
      if (!sectionNode) continue;
      shown += 1;
      container.append(sectionNode);
      const button = h('button', {
        type: 'button',
        title: `跳到「${sectionTitle(key)}」`,
        onclick: () => {
          sectionNode.openSection?.();
          sectionNode.scrollIntoView({ behavior: 'smooth', block: 'start' });
        },
      }, h('span', { text: sectionTitle(key) }),
        h('span', { class: 'count', text: countFields(value) }));
      navButtons.set(key, button);
      nav.append(button);
    }

    if (!shown) {
      container.append(h('div', { class: 'empty', text: '没有匹配的参数' }));
    }
    if (config.matchNode) {
      config.matchNode.textContent = config.filter
        ? `匹配 ${shown} 个分区 · ${config.fieldShown} 个参数`
        : `${shown} 个分区 · ${config.fieldShown} 个参数`;
    }

    // 导航区整块替换：只删 nav 会把标题留下，重画几次就叠出好几行"分区"。
    // 用 details 而不是死板的标题：窄屏上 26 个分区会把搜索框顶到屏幕外，
    // 折起来当索引用；宽屏默认展开，跟以前一样。
    let block = sidebar.querySelector('.section-nav-block');
    if (!block) {
      block = h('details', { class: 'section-nav-block', open: !narrowLayout() });
      sidebar.append(block);
    }
    clear(block);
    block.append(
      h('summary', { class: 'section-nav-head' },
        h('span', { text: isTyped ? '分区' : '顶层键' }),
        h('span', { class: 'count', text: `${shown} 个` })),
      nav);
    setupSectionSpy(navButtons);
  }

  /** 左栏分区导航跟随正文高亮：滚到哪一块，哪一行就是选中态。
   *
   *  用滚动位置算而不是 IntersectionObserver：吸顶的工具栏遮住了正文顶部，
   *  观察器的"可见"判定会把刚滚过去的分区也算成可见。 */
  function setupSectionSpy(buttons) {
    if (config.spyCleanup) { config.spyCleanup(); config.spyCleanup = null; }
    const scroller = document.querySelector('.page-scroll');
    const entries = [...buttons.entries()];
    if (!scroller || !entries.length) return;
    let frame = 0;
    const update = () => {
      frame = 0;
      const first = document.getElementById(`section-${entries[0][0]}`);
      // 已经离开配置页（整页被重建过）：别再动那些已经摘下来的按钮。
      if (!first || !first.isConnected) return;
      // 判定线放在吸顶工具栏下沿再往下一点，避免"刚露头"就被算作当前分区。
      const line = scroller.getBoundingClientRect().top + 86;
      let active = entries[0][1];
      for (const [path, button] of entries) {
        const card = document.getElementById(`section-${path}`);
        if (!card) continue;
        if (card.getBoundingClientRect().top <= line) active = button;
        else break;
      }
      for (const button of buttons.values()) button.classList.toggle('active', button === active);
      // 选中项可能在左栏的滚动区之外（分区多、栏位矮）：把它带进视野，
      // 否则高亮跟没高亮一样。block:'nearest' 只在看不见时才动，不会带着页面跳。
      if (active !== config.spyActive) {
        config.spyActive = active;
        active.scrollIntoView({ block: 'nearest' });
      }
    };
    const onScroll = () => { if (!frame) frame = requestAnimationFrame(update); };
    scroller.addEventListener('scroll', onScroll, { passive: true });
    config.spyCleanup = () => scroller.removeEventListener('scroll', onScroll);
    update();
  }

  function sectionTitle(key) {
    const names = {
      identity: '身份', prompt: '人设提示词', server_config: '模型服务', proactive: '主动消息',
      group_interjection: '群聊接话', memory: '记忆', mind: 'Mind v2', message_batch: '消息合并',
      mood: '情绪', topic: '话题', traffic: '流量与资源上限', tools: '工具', reminders: '提醒',
      agent_tasks: '跨群问答任务', agent_runs: 'Agent Run', world_sensors: '世界传感器',
      world_model: 'World Model', gag_ledger: '梗账本', vision: '图片理解', qq_call: '语音通话',
      qq_voice: '语音消息', executive: 'Executive', model: '模型路由', admin: '管理后台',
      config: '框架', server: 'OneBot 连接', access_list: '访问白名单', plugin: '插件',
    };
    return names[key] || key;
  }

  function countFields(value) {
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      return String(Object.keys(value).length);
    }
    return '';
  }

  /** 一个分区卡片；返回 null 表示被搜索条件过滤掉了。 */
  function renderSection(path, key, value, isTyped) {
    const isObject = value && typeof value === 'object' && !Array.isArray(value);
    const doc = isTyped ? docsFor(path.join('.')) : null;
    const body = h('div', { class: 'section-body' });
    let visible = 0;

    if (isObject) {
      for (const [childKey, childValue] of Object.entries(value)) {
        const childPath = [...path, childKey];
        const childIsObject = childValue && typeof childValue === 'object' && !Array.isArray(childValue);
        if (childIsObject && !Array.isArray(childValue)) {
          const nested = renderSection(childPath, childKey, childValue, isTyped);
          if (nested) { body.append(nested); visible += 1; }
          continue;
        }
        const fieldNode = renderField(childPath, childValue, isTyped);
        if (fieldNode) { body.append(fieldNode); visible += 1; }
      }
    } else {
      const fieldNode = renderField(path, value, isTyped);
      if (fieldNode) { body.append(fieldNode); visible += 1; }
    }

    if (!visible) return null;
    const sectionPath = path.join('.');
    config.sectionPaths.push(sectionPath);
    // 搜索时一律展开（否则搜到的东西藏在折叠里，等于没搜）；
    // 平时的展开状态由 config.expanded 记着，切文件、改字段都不会丢。
    const searching = Boolean(config.filter);
    const isOpen = searching || config.expanded.has(sectionPath);
    body.hidden = !isOpen;

    const chevron = h('span', { class: `section-chevron${isOpen ? ' open' : ''}` }, icon('chevron'));
    // 说明文字只在展开时露出：折起来时它就是白白占一行高度。
    const docNode = doc && doc.section ? h('p', { class: 'section-doc', text: doc.section, hidden: !isOpen }) : null;
    const setOpen = (open, remember = true) => {
      body.hidden = !open;
      chevron.classList.toggle('open', open);
      if (docNode) docNode.hidden = !open;
      header.setAttribute('aria-expanded', open ? 'true' : 'false');
      if (!remember) return;
      if (open) config.expanded.add(sectionPath);
      else config.expanded.delete(sectionPath);
    };
    const header = h('header', {
      class: searching ? 'locked' : '',
      role: 'button',
      tabindex: searching ? null : '0',
      'aria-expanded': isOpen ? 'true' : 'false',
      title: searching ? '搜索中：结果已自动展开' : '点击折叠 / 展开',
      onclick: () => { if (!searching) setOpen(body.hidden); },
      onkeydown: (event) => {
        if (searching) return;
        if (event.key !== 'Enter' && event.key !== ' ') return;
        event.preventDefault();
        setOpen(body.hidden);
      },
    },
      chevron,
      h('div', { class: 'section-head-main' },
        h('h3', { text: sectionTitle(key) }),
        h('span', { class: 'path', text: sectionPath })),
      h('span', { class: 'section-count', text: `${visible} 个参数` }));

    const card = h('section', { class: 'card section-card', id: `section-${sectionPath}` });
    // 左栏导航要用它：先展开再滚过去，否则滚到一个折着的标题上什么也看不见。
    card.openSection = () => setOpen(true);
    card.append(header);
    if (docNode) card.append(docNode);
    card.append(body);
    return card;
  }

  function docsFor(path) {
    const docs = (config.data && config.data.docs) || {};
    const fields = docs.fields || {};
    const parts = path.split('.');
    const entry = fields[path] || null;
    const sectionDoc = (docs.sections || {})[parts[0]] || null;
    return { entry, section: sectionDoc };
  }

  /** 单个字段：标签 + 说明 + 按类型渲染的控件。null 表示被过滤掉。 */
  function renderField(path, value, isTyped, options) {
    const overrides = options || {};
    const dotted = path.join('.');
    const numericSegments = path.filter((segment) => /^\d+$/.test(segment));
    const docPath = dotted.replace(/\.\d+(?=\.|$)/g, '[]');
    const meta = isTyped ? ((config.data.docs || {}).fields || {})[docPath] || null : null;
    const doc = meta && meta.doc;
    const hint = meta && meta.hint;
    const secret = Boolean(meta && meta.secret);
    const present = !isTyped || !config.data.file_paths || config.data.file_paths.includes(filePrefix(docPath));

    if (config.filter) {
      const haystack = `${dotted} ${doc || ''} ${hint || ''}`.toLowerCase();
      if (!haystack.includes(config.filter)) return null;
    }

    const dirtyKey = overrides.dirtyKey || valueKey(path);
    const current = overrides.current !== undefined
      ? overrides.current
      : (config.dirty.has(dirtyKey) ? config.dirty.get(dirtyKey) : value);
    const changed = config.dirty.has(dirtyKey);

    const control = buildControl(path, value, current, {
      secret,
      options: meta && meta.options,
      commit: overrides.commit,
    });

    const restart = isTyped && restartRequiredFor(docPath);
    config.fieldShown = (config.fieldShown || 0) + 1;

    // 字段名与状态在左栏，控件与说明在右栏：说明跟着控件走才能用满整行宽度，
    // 挤在窄窄的标签栏里会折成好几行，把每一行都撑得老高。
    return h('div', { class: `field${changed ? ' changed' : ''}`, 'data-field-path': dotted },
      h('div', { class: 'field-label' },
        h('div', { class: 'field-head' },
          h('span', { class: 'field-name', text: path.slice(-1)[0] }),
          secret ? h('span', { class: 'badge secret', text: '密钥' }) : null,
          restart ? h('span', { class: 'badge restart', text: '需重启' }) : null,
          !present ? h('span', { class: 'badge default', text: '本文件未写' }) : null,
          numericSegments.length ? h('span', { class: 'badge', text: `第 ${Number(numericSegments[0]) + 1} 项` }) : null)),
      h('div', { class: 'field-control' }, control,
        doc ? h('div', { class: 'field-doc', text: doc }) : null,
        hint ? h('div', { class: 'field-hint', text: hint }) : null));
  }

  /** 数组元素的路径形如 `tools.mcp_servers[].name`，是否"写进文件"取决于数组本身。 */
  function filePrefix(docPath) {
    const index = docPath.indexOf('[]');
    return index === -1 ? docPath : docPath.slice(0, index).replace(/\.$/, '');
  }

  function restartRequiredFor(docPath) {
    const sections = (config.data && config.data.restart_sections) || [];
    return sections.some((section) => docPath === section || docPath.startsWith(`${section}.`));
  }

  function valueKey(path) { return path.join('.'); }

  function markDirty(path, value, original) {
    const key = valueKey(path);
    if (JSON.stringify(value) === JSON.stringify(original)) config.dirty.delete(key);
    else config.dirty.set(key, value);
    // 局部更新高亮与保存栏，避免整页重绘打断输入。
    // 高亮的宿主是带 `data-field-path` 的那一行（CSS 里只有 `.field.changed`）；
    // 以前查的是 `[data-field=…]`，页面上没有这个属性，于是这段一直是空操作。
    const field = document.querySelector(`[data-field-path="${CSS.escape(key)}"]`);
    if (field) field.classList.toggle('changed', config.dirty.has(key));
    refreshSaveBar();
  }

  function refreshSaveBar() {
    const main = document.querySelector('.config-main');
    if (!main) return;
    const existing = main.querySelector('.save-bar');
    if (existing) existing.remove();
    const toolbar = main.querySelector('.card.tight');
    if (config.dirty.size && toolbar) toolbar.after(renderSaveBar());
  }

  function renderSaveBar() {
    const keys = [...config.dirty.keys()];
    const bar = h('div', { class: 'save-bar' },
      h('div', { class: 'changed-list' },
        h('strong', { text: `${keys.length} 项改动` }),
        h('span', { text: `：${keys.slice(0, 4).join('、')}${keys.length > 4 ? ` 等 ${keys.length} 项` : ''}` })),
      h('div', { class: 'actions' },
        h('button', { class: 'btn ghost', text: '查看差异', onclick: () => openDiff(keys) }),
        h('button', {
          class: 'btn ghost', text: '放弃',
          onclick: async () => { config.dirty.clear(); await renderConfigPage(); },
        }),
        h('button', { class: 'btn primary', text: '保存并应用', onclick: saveConfig })));
    return bar;
  }

  async function saveConfig() {
    const changes = {};
    for (const [key, value] of config.dirty) changes[key.replace(/\.\d+(?=\.|$)/g, '[]')] = value;
    try {
      const result = await api('/api/config/patch', {
        method: 'POST',
        body: { file: config.name, changes },
      });
      config.dirty.clear();
      await loadConfigFile(config.name);
      await renderConfigPage();
      const restart = (result.changed || []).some((key) => restartRequiredFor(key));
      toast(
        `已保存 ${(result.changed || []).length} 项${result.skipped.length ? `，跳过 ${result.skipped.length} 项密钥` : ''}`
        + (restart ? '；其中部分分区需要重启才生效' : '，已热加载'),
        restart ? 'warn' : 'ok');
      await refreshHealth();
    } catch (problem) {
      toast(`保存失败：${problem.message}`, 'bad', 9000);
    }
  }

  function openDiff(keys) {
    const rows = [h('tr', {}, h('th', { text: '参数' }), h('th', { text: '当前' }), h('th', { text: '将改为' }))];
    for (const key of keys) {
      const path = key.split('.');
      const before = valueAtPath(config.data.values, path);
      const after = config.dirty.get(key);
      rows.push(h('tr', {},
        h('td', { class: 'path', text: key }),
        h('td', { class: 'old', text: displayValue(before) }),
        h('td', { class: 'new', text: displayValue(after) })));
    }
    openModal('本次改动', h('table', { class: 'diff-table' }, ...rows));
  }

  function valueAtPath(root, path) {
    let current = root;
    for (const segment of path) {
      if (current === null || current === undefined) return undefined;
      current = current[Array.isArray(current) ? Number(segment) : segment];
    }
    return current;
  }

  /** 按 JSON 类型选择控件；返回一个 DOM 节点。 */
  function buildControl(path, original, value, options) {
    const key = valueKey(path);
    const commit = options.commit || ((next) => markDirty(path, next, original));
    let control;

    if (typeof value === 'boolean') {
      const input = h('input', { type: 'checkbox', checked: value });
      input.addEventListener('change', () => commit(input.checked));
      control = h('label', { class: 'switch' }, input,
        h('span', { class: 'track' }), h('span', { class: 'switch-text', text: value ? '开' : '关' }));
      input.addEventListener('change', () => { control.querySelector('.switch-text').textContent = input.checked ? '开' : '关'; });
    } else if (typeof value === 'number') {
      const isFloat = !Number.isInteger(value);
      const input = h('input', {
        class: 'input', type: 'number', value: String(value),
        step: isFloat ? 'any' : '1',
      });
      input.addEventListener('input', () => {
        const parsed = isFloat ? Number.parseFloat(input.value) : Number.parseInt(input.value, 10);
        if (!Number.isNaN(parsed)) commit(parsed);
      });
      control = input;
    } else if (value === null) {
      const input = h('input', { class: 'input mono', placeholder: '（未设置，留空保持）', value: '' });
      input.addEventListener('input', () => commit(input.value === '' ? null : input.value));
      control = input;
    } else if (Array.isArray(value)) {
      control = buildArrayControl(path, original, value, commit);
    } else if (typeof value === 'object') {
      control = h('div', { class: 'field-hint', text: '嵌套结构请在原始 TOML 里编辑' });
    } else {
      const text = String(value);
      if (options && options.length) {
        const select = h('select', { class: 'select' });
        const known = options.includes(text);
        if (!known) select.append(h('option', { value: text, text: `${text}（当前）` }));
        for (const option of options) {
          select.append(h('option', { value: option, text: option, selected: option === text }));
        }
        select.addEventListener('change', () => commit(select.value));
        control = select;
      } else if (text.length > 90 || text.includes('\n')) {
        const area = h('textarea', { class: 'textarea', rows: text.length > 400 ? 12 : 5, value: text });
        area.addEventListener('input', () => commit(area.value));
        control = area;
      } else {
        const input = h('input', { class: 'input', value: text });
        input.addEventListener('input', () => commit(input.value));
        control = input;
      }
    }

    const wrapper = h('div', { 'data-field': key }, control);
    return wrapper;
  }

  /** 数组：标量数组用逗号分隔；对象数组渲染成可增删的子表单。 */
  function buildArrayControl(path, original, value, commit) {
    const container = h('div');

    if (value.every((item) => typeof item !== 'object' || item === null)) {
      const isNumeric = value.every((item) => typeof item === 'number');
      const input = h('input', {
        class: 'input mono',
        value: value.map((item) => (typeof item === 'string' ? item : String(item))).join(', '),
        placeholder: '逗号分隔；留空表示空数组',
      });
      input.addEventListener('input', () => {
        const parts = input.value.split(',').map((part) => part.trim()).filter((part) => part !== '');
        commit(isNumeric ? parts.map(Number).filter((n) => !Number.isNaN(n)) : parts);
      });
      container.append(input, h('div', { class: 'field-hint', text: isNumeric ? '数字数组，逗号分隔' : '字符串数组，逗号分隔' }));
      return container;
    }

    // 服务端的 patch 以"整个数组"为单位，所以数组内的每次改动都要回写整份数组；
    // working 是当前编辑中的真值，避免连续改两个字段时后一次覆盖前一次。
    let working = [];
    const render = (items) => {
      working = structuredClone(items);
      clear(container);
      working.forEach((item, index) => {
        const itemPath = [...path, String(index)];
        const fields = h('div');
        for (const [childKey, childValue] of Object.entries(item)) {
          fields.append(renderField([...itemPath, childKey], childValue, false, {
            current: childValue,
            dirtyKey: valueKey(path),
            commit: (nextValue) => {
              working[index][childKey] = nextValue;
              commit(structuredClone(working));
            },
          }));
        }
        container.append(h('div', { class: 'array-item' },
          h('div', { class: 'array-head' },
            h('span', { class: 'array-title', text: `#${index + 1}` }),
            h('button', {
              class: 'btn ghost small', text: '删除',
              onclick: () => {
                const next = working.filter((_, position) => position !== index);
                commit(structuredClone(next));
                render(next);
              },
            })),
          fields));
      });
      container.append(h('button', {
        class: 'btn ghost small', text: '+ 添加一项',
        onclick: () => {
          const template = working.length ? structuredClone(working[working.length - 1]) : {};
          for (const childKey of Object.keys(template)) template[childKey] = defaultValueFor(template[childKey]);
          const next = [...working, template];
          commit(structuredClone(next));
          render(next);
        },
      }));
    };

    render(value);
    return container;
  }

  function defaultValueFor(value) {
    if (typeof value === 'string') return '';
    if (typeof value === 'number') return 0;
    if (typeof value === 'boolean') return false;
    if (Array.isArray(value)) return [];
    if (value && typeof value === 'object') return {};
    return null;
  }

  // 原始 TOML 编辑器

  function openRawEditor() {
    const area = h('textarea', { class: 'textarea', spellcheck: 'false', value: config.data.raw });
    openModal(`原始 TOML · ${config.name}`, area,
      [h('button', {
        class: 'btn primary', text: '校验并保存',
        onclick: async (event) => {
          event.target.disabled = true;
          try {
            const result = await api(`/api/config/file/${encodeURIComponent(config.name)}`, {
              method: 'PUT', body: { raw: area.value },
            });
            closeModal();
            await loadConfigFile(config.name);
            await renderConfigPage();
            toast(`已保存${result.reloaded ? '并热加载' : ''}${result.backup ? `，备份 ${result.backup}` : ''}`, 'ok');
          } catch (problem) {
            toast(`保存失败：${problem.message}`, 'bad', 10000);
          } finally {
            event.target.disabled = false;
          }
        },
      })],
      '留空表示不修改密钥字段；服务端会先校验再落盘');
  }

  async function openBackups() {
    let payload;
    try { payload = await api('/api/config/backups'); } catch (problem) { toast(problem.message, 'bad'); return; }
    const rows = [h('tr', {}, h('th', { text: '文件' }), h('th', { text: '备份' }), h('th', { text: '大小' }), h('th', { text: '时间' }), h('th', {}))];
    for (const backup of payload.backups.filter((item) => item.file === config.name)) {
      rows.push(h('tr', {},
        h('td', { text: backup.file }),
        h('td', { class: 'path', text: backup.name }),
        h('td', { text: fmtBytes(backup.bytes) }),
        h('td', { text: backup.modified || '—' }),
        h('td', {}, h('button', {
          class: 'btn ghost small', text: '回滚',
          onclick: async () => {
            if (!confirm(`确定回滚到 ${backup.name}？当前内容会先备份。`)) return;
            try {
              await api('/api/config/restore', { method: 'POST', body: { name: backup.name } });
              closeModal();
              await loadConfigFile(config.name);
              await renderConfigPage();
              toast('已回滚到所选备份');
            } catch (problem) { toast(problem.message, 'bad'); }
          },
        }))));
    }
    openModal('配置备份', h('table', { class: 'diff-table' }, ...rows), null, `每个文件保留最近 ${payload.keep} 份`);
  }

  // ───────────────────────────── 记忆 ─────────────────────────────
  //
  // 对照 Hindsight 的「记忆」页：顶部统计卡 → 常驻筛选行 → 三个视图
  // （星座图 / 表格 / 时间线）→ 详情。每条记录都带标签与实体，
  // 实体 chip 的取色算法跟直播版一致（31 进制哈希取模五色调色板）。

  /** 星座图当前绘制的热度范围（drawConstellation 回填，图例用它）。 */
  let heatRange = null;

  const memory = {
    tab: 'records',
    view: 'constellation',
    kinds: [],
    query: '',
    tags: [],
    scope: '',
    limit: 100,
    page: 1,
    data: null,
    stats: null,
    graph: null,
    tagList: null,
    selected: null,
    granularity: 'month',
    groupIndex: 0,
    colorBy: 'mentioned_at',
    linkTypes: new Set(['semantic', 'temporal', 'entity', 'causal']),
    // 记忆里常有大量同内容的记录（例如同一个传感器每次重启写一条），
    // 直接画就是一团毛线球。默认按标题折叠。
    collapseDuplicates: true,
  };

  // 数据库里存的是枚举标识，界面上给人话名字。
  const TAG_LABELS = {
    fact: '事实', event: '事件', preference: '偏好', emotion: '情绪',
    profile: '档案', conversation: '对话', memory: '记忆',
    personal: '个人', follow_up: '跟进', project: '项目', system: '系统',
    promise: '许诺', awaiting_outcome: '等结果', future_event: '将来', pending_question: '待回答',
    open: '进行中', active: '进行中', resolved: '已了结', expired: '已过期',
    global: '全局', person: '个人', agenda: '议程',
  };
  const tagLabel = (tag) => TAG_LABELS[tag] || tag;

  // 与 Hindsight 直播版相同的五色调色板（浅色 / 深色两套）。
  const ENTITY_PALETTE = [
    ['#00BC7D', '#00D492'],
    ['#2B7FFF', '#7DB6FF'],
    ['#8E51FF', '#B695FF'],
    ['#F59E0B', '#FBB040'],
    ['#EC4899', '#F472B6'],
  ];

  /** 直播版的取色算法：hash = 31*hash + charCode，再对调色板长度取模。 */
  function entityColor(name) {
    let hash = 0;
    for (let index = 0; index < name.length; index += 1) {
      hash = (31 * hash + name.charCodeAt(index)) | 0;
    }
    const pair = ENTITY_PALETTE[Math.abs(hash) % ENTITY_PALETTE.length];
    return document.documentElement.dataset.theme === 'dark' ? pair[1] : pair[0];
  }

  function entityChip(name) {
    const color = entityColor(name);
    const dark = document.documentElement.dataset.theme === 'dark';
    const alpha = dark ? { bg: '26', border: '4d' } : { bg: '1a', border: '33' };
    return h('span', {
      class: 'facet-chip',
      title: name,
      style: `background:${color}${alpha.bg};color:${color};border-color:${color}${alpha.border}`,
    }, name);
  }

  function tagChip(tag, onClick, active) {
    return h('span', {
      class: `facet-chip tag${active ? ' active' : ''}`,
      onclick: onClick || null,
      style: onClick ? 'cursor:pointer' : null,
    }, `#${tagLabel(tag)}`);
  }

  /** 把命中搜索词的片段包进 <mark>；用 DOM 节点拼，不拼 HTML 字符串。 */
  function highlighted(text, needle) {
    const source = String(text ?? '');
    const term = (needle || '').trim();
    if (!term) return [source];
    const lowerSource = source.toLowerCase();
    const lowerTerm = term.toLowerCase();
    const parts = [];
    let cursor = 0;
    for (;;) {
      const hit = lowerSource.indexOf(lowerTerm, cursor);
      if (hit === -1) break;
      if (hit > cursor) parts.push(source.slice(cursor, hit));
      parts.push(h('mark', { text: source.slice(hit, hit + term.length) }));
      cursor = hit + term.length;
    }
    if (!parts.length) return [source];
    if (cursor < source.length) parts.push(source.slice(cursor));
    return parts;
  }

  const LINK_TYPES = [
    ['semantic', '语义', '#0074d9'],
    ['temporal', '时序', '#009296'],
    ['entity', '实体', '#f59e0b'],
    ['causal', '因果', '#8b5cf6'],
  ];

  /** 记录进入记忆的时间 / 事情发生的时间，与 Hindsight 的两列一一对应。 */
  /** 大数字缩写：2600 → 2.6K，108100 → 108.1K，跟 Hindsight 的写法一致。 */
  function compactNumber(value) {
    const number = Number(value) || 0;
    if (Math.abs(number) < 1000) return String(number);
    if (Math.abs(number) < 1_000_000) {
      const scaled = number / 1000;
      return `${scaled >= 100 ? Math.round(scaled) : scaled.toFixed(1)}K`;
    }
    const scaled = number / 1_000_000;
    return `${scaled >= 100 ? Math.round(scaled) : scaled.toFixed(1)}M`;
  }

  /** 用量卡的环比行；上周为 0 时不说百分比（除零没有意义）。 */
  function usageDelta(card) {
    const current = Number(card.current) || 0;
    const previous = Number(card.previous) || 0;
    if (!previous && !current) return h('div', { class: 'delta flat' }, '与上周持平');
    if (!previous) return h('div', { class: 'delta up' }, `本周新记 ${compactNumber(current)}`);
    const change = Number(card.delta_percent) || 0;
    const arrow = change > 0 ? '↗' : change < 0 ? '↘' : '→';
    return h('div', { class: `delta ${change > 0 ? 'up' : change < 0 ? 'down' : 'flat'}` },
      `${arrow} ${change > 0 ? '+' : ''}${change}% 对比上周`);
  }

  /** 顶部的用量卡：保存 / 召回 / 反思 / 心智模型 / 模型调用（本周）。 */
  function renderUsageCards(usage) {
    const wrap = h('div', { class: 'usage-block' });
    const cards = h('div', { class: 'usage-grid six' });
    for (const card of usage.cards || []) {
      cards.append(h('div', { class: 'stat usage-card', title: card.hint || '' },
        h('div', { class: 'label', text: card.label }),
        h('div', { class: 'value' },
          compactNumber(card.current),
          h('span', { class: 'unit', text: card.unit })),
        usageDelta(card)));
    }
    wrap.append(h('div', { class: 'card-head' },
      h('h3', { text: '本周' }),
      h('span', { class: 'hint', text: `${usage.period_start} → ${usage.period_end} · 单位 tokens/calls 为估算` })),
      cards);
    return wrap;
  }

  const occurredOf = (row) => row.occurred_at || row.mentioned_at;
  const mentionedOf = (row) => row.mentioned_at || row.occurred_at;

  function shortDate(value) {
    if (!value) return '—';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return '—';
    return date.toLocaleDateString('zh-CN', { year: 'numeric', month: 'short', day: 'numeric' });
  }

  /** 时间线左侧那一列只放"几月几日"，年份交给分组标题。 */
  function monthDay(value) {
    if (!value) return '—';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return '—';
    return date.toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' });
  }

  function shortTime(value) {
    if (!value) return '';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return '';
    return `${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')} `
      + `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
  }

  async function renderMemoryPage() {
    const page = $('#page-memory');
    clear(page);

    page.append(h('div', { class: 'tabs' },
      tabButton('records', '记忆'), tabButton('people', '人物')));

    if (memory.tab === 'records') await renderRecordsTab(page);
    else await renderPeopleTab(page);
  }

  function tabButton(key, label) {
    return h('button', {
      class: memory.tab === key ? 'active' : '',
      text: label,
      onclick: () => {
        memory.tab = key;
        syncMemoryHash();
        renderMemoryPage();
      },
    });
  }

  /** 把当前 tab/视图写回地址栏，刷新与分享都能回到同一屏。 */
  function syncMemoryHash() {
    if (currentPage !== 'memory') return;
    const sub = memory.tab === 'people' ? 'people' : memory.view;
    history.replaceState(null, '', `#/memory/${sub}`);
  }

  // ── 记录页：统计 + 筛选 + 三视图

  async function renderRecordsTab(page) {
    const [stats, tagList] = await Promise.all([
      api('/api/memory/stats'),
      memory.tagList ? Promise.resolve(memory.tagList) : api('/api/memory/tags'),
    ]);
    memory.stats = stats;
    memory.tagList = tagList;

    if (stats.usage) page.append(renderUsageCards(stats.usage));
    page.append(renderStatCards(stats));
    page.append(renderFilterRow(tagList.tags || []));
    page.append(renderViewBar());
    if (memory.view === 'constellation') await renderConstellation(page);
    else if (memory.view === 'table') await renderTable(page);
    else await renderTimeline(page);
  }

  function deltaLine(current, previous) {
    if (!previous && !current) return h('div', { class: 'delta flat' }, '与上周持平');
    if (!previous) return h('div', { class: 'delta up' }, `本周 +${current}`);
    const change = Math.round(((current - previous) / previous) * 100);
    const arrow = change > 0 ? '↗' : change < 0 ? '↘' : '→';
    return h('div', { class: `delta ${change > 0 ? 'up' : change < 0 ? 'down' : 'flat'}` },
      `${arrow} ${change > 0 ? '+' : ''}${change}% 对比上周`);
  }

  /** 第二排：语料规模，正好 6 张（存储大小在概览页，这里不重复占位）。 */
  function renderStatCards(stats) {
    const byKind = stats.by_kind || {};
    const week = stats.week || {};
    const cards = [
      ['记忆总数', String(stats.total ?? 0), deltaLine(week.new || 0, week.previous || 0)],
      ['本周新增', String(week.new ?? 0), h('div', { class: 'delta flat' }, `涉及 ${(week.by_kind || []).length} 类记录`)],
      ['人物', String(stats.people ?? 0), h('div', { class: 'delta flat' }, `${stats.conversations ?? 0} 个会话`)],
      ['情节', String(byKind.episode ?? 0), h('div', { class: 'delta flat' }, '第一人称经历')],
      ['目标', String(byKind.goal ?? 0), h('div', { class: 'delta flat' }, '长期目标')],
      ['未完结线索', String(byKind.open_loop ?? 0), h('div', { class: 'delta flat' }, '等着被接上')],
    ];
    const grid = h('div', { class: 'stat-grid six' });
    for (const [label, value, extra] of cards) {
      grid.append(h('div', { class: 'stat' },
        h('div', { class: 'label', text: label }),
        h('div', { class: 'value', text: value }),
        extra));
    }
    return grid;
  }

  function renderFilterRow(tags) {
    const search = h('input', {
      class: 'input search',
      placeholder: '按文本或上下文筛选（按 Enter 确认）…',
      value: memory.query,
      onkeydown: (event) => {
        if (event.key === 'Enter') {
          memory.query = search.value.trim();
          memory.page = 1;
          renderMemoryPage();
        }
      },
      oninput: () => {
        // 清空输入立即恢复完整列表：否则退格会停在一个零结果页上。
        if (search.value === '' && memory.query !== '') {
          memory.query = '';
          memory.page = 1;
          renderMemoryPage();
        }
      },
    });

    const picker = h('select', { class: 'select' },
      h('option', { value: '', text: '按标签筛选…' }),
      ...tags.map((entry) => h('option', {
        value: entry.tag,
        text: `${tagLabel(entry.tag)}（${entry.count}）`,
        selected: memory.tags.includes(entry.tag),
      })));
    picker.addEventListener('change', () => {
      if (!picker.value) return;
      if (!memory.tags.includes(picker.value)) memory.tags.push(picker.value);
      memory.page = 1;
      renderMemoryPage();
    });

    const active = h('div', { class: 'filter-chips' });
    for (const tag of memory.tags) {
      active.append(tagChip(tag, () => {
        memory.tags = memory.tags.filter((item) => item !== tag);
        memory.page = 1;
        renderMemoryPage();
      }, true));
    }
    if (memory.scope) {
      active.append(h('span', {
        class: 'facet-chip tag active',
        style: 'cursor:pointer',
        onclick: () => { memory.scope = ''; memory.page = 1; renderMemoryPage(); },
      }, `作用域：${memory.scope} ✕`));
    }

    const row = h('div', { class: 'search-row' }, search, picker,
      h('button', {
        class: 'btn ghost',
        title: '刷新记忆',
        text: '刷新',
        onclick: () => renderMemoryPage(),
      }),
      (memory.query || memory.tags.length || memory.scope || memory.kinds.length)
        ? h('button', {
          class: 'btn ghost',
          text: '清除筛选',
          onclick: () => {
            memory.query = '';
            memory.tags = [];
            memory.scope = '';
            memory.kinds = [];
            memory.page = 1;
            renderMemoryPage();
          },
        })
        : null);

    const box = h('div', { class: 'card tight' }, row);
    if (active.childNodes.length) box.append(active);
    return box;
  }

  function renderViewBar() {
    const views = [['constellation', '星座图'], ['table', '表格'], ['timeline', '时间线']];
    return h('div', { class: 'view-bar' },
      h('span', { class: 'view-count' }, memoryViewSummary()),
      h('div', { class: 'segmented' },
        ...views.map(([key, label]) => h('button', {
          class: memory.view === key ? 'active' : '',
          onclick: () => {
            memory.view = key;
            memory.tab = 'records';
            syncMemoryHash();
            renderMemoryPage();
          },
        }, label))));
  }

  function memoryViewSummary() {
    if (memory.view === 'constellation') {
      const total = memory.graph ? memory.graph.total : null;
      return total === null ? '正在加载…' : `星座图：${total} 条记忆`;
    }
    if (!memory.data) return '正在加载…';
    const filtered = memory.query || memory.tags.length || memory.scope;
    return filtered ? `${memory.data.total} 条匹配记忆` : `共 ${memory.data.total} 条记忆`;
  }

  /** 视图栏左边那句统计要等数据回来才准：建栏时数据还没到，取到之后回填一次。 */
  function syncViewCount() {
    const label = document.querySelector('#page-memory .view-count');
    if (label) label.textContent = memoryViewSummary();
  }

  // ── 表格视图

  async function renderTable(page) {
    const params = new URLSearchParams({ limit: String(memory.limit) });
    params.set('offset', String((memory.page - 1) * memory.limit));
    if (memory.query) params.set('q', memory.query);
    if (memory.kinds.length) params.set('kinds', memory.kinds.join(','));
    if (memory.tags.length) params.set('tags', memory.tags.join(','));
    if (memory.scope) params.set('scope', memory.scope);

    const data = await api(`/api/memory/records?${params}`);
    memory.data = data;
    syncViewCount();

    const card = h('div', { class: 'card' });
    if (!data.items.length) {
      card.append(h('div', { class: 'empty' },
        memory.query || memory.tags.length ? '没有记忆匹配你的筛选条件' : '还没有记忆'));
      page.append(card);
      return;
    }

    const table = h('table', { class: 'memory-table' },
      h('thead', {}, h('tr', {},
        h('th', { class: 'w-memory', text: '记忆' }),
        h('th', { class: 'w-entities', text: '实体' }),
        h('th', { class: 'w-tags', text: '标签' }),
        h('th', { class: 'w-time', text: '发生时间' }),
        h('th', { class: 'w-time', text: '提及时间' }))));

    const body = h('tbody');
    for (const row of data.items) {
      body.append(h('tr', {
        onclick: () => openRecord(row),
      },
        h('td', {},
          h('div', { class: 'cell-title' }, ...highlighted(row.title, memory.query)),
          h('div', { class: 'cell-context' },
            `${row.label} · ${row.scope_label || '全局'}${row.status ? ` · ${row.status}` : ''}`)),
        // data-label 只给"离开表头就看不懂"的列：窄屏下表格会摊成卡片，
        // 表头没了，这几格得自己报出列名（记忆标题那格自明，不挂）。
        h('td', { 'data-label': '实体' }, chipList(row.entities, entityChip)),
        h('td', { 'data-label': '标签' }, chipList(row.tags, (tag) => tagChip(tag, null, false))),
        h('td', { class: 'cell-time', 'data-label': '发生时间', text: shortDate(occurredOf(row)) }),
        h('td', { class: 'cell-time', 'data-label': '提及时间', text: shortDate(mentionedOf(row)) })));
    }
    table.append(body);
    // 包一层可滚动容器，表头才能 sticky 住（表格自己滚动，页码留在下面）。
    card.append(h('div', { class: 'table-scroll' }, table));
    card.append(renderPager(data));
    page.append(card);
  }

  /** 一行里最多放两个 chip，其余折成 +N（跟直播版一致）。 */
  function chipList(values, render) {
    const items = values || [];
    if (!items.length) return h('span', { class: 'cell-empty', text: '-' });
    const box = h('div', { class: 'chip-cell' });
    for (const item of items.slice(0, 2)) box.append(render(item));
    if (items.length > 2) {
      box.append(h('span', { class: 'chip-more', text: `+${items.length - 2}` }));
    }
    return box;
  }

  function renderPager(data) {
    const pages = Math.max(1, Math.ceil(data.total / memory.limit));
    const jump = (target) => {
      memory.page = Math.min(Math.max(1, target), pages);
      renderMemoryPage();
    };
    const start = data.total === 0 ? 0 : (memory.page - 1) * memory.limit + 1;
    const end = Math.min(memory.page * memory.limit, data.total);
    // 过滤与分页都只在后端抓到的那个窗口里做（每类最多 200 条）。真实总数远超
    // 窗口时如实说明：页码条只承诺它翻得到的页，不再出现"共 5000 条、翻到第 5 页
    // 全是空的"。
    const windowNote = data.total_is_window
      ? `（在最近 ${data.window} 条里统计；库里共 ${data.db_total} 条）`
      : '';
    return h('div', { class: 'table-foot' },
      h('span', { class: 'muted', text: `${start}-${end} / 共 ${data.total} 条${windowNote}` }),
      h('div', { class: 'pager-btns' },
        h('button', { class: 'btn ghost small', text: '«', disabled: memory.page === 1, onclick: () => jump(1) }),
        h('button', { class: 'btn ghost small', text: '‹', disabled: memory.page === 1, onclick: () => jump(memory.page - 1) }),
        h('span', { class: 'muted', text: `${memory.page} / ${pages}` }),
        h('button', { class: 'btn ghost small', text: '›', disabled: memory.page >= pages, onclick: () => jump(memory.page + 1) }),
        h('button', { class: 'btn ghost small', text: '»', disabled: memory.page >= pages, onclick: () => jump(pages) })));
  }

  // ── 时间线视图

  const GRANULARITY = [
    ['year', '年'],
    ['month', '月'],
    ['week', '周'],
    ['day', '日'],
  ];

  function groupKey(date, granularity) {
    const year = date.getFullYear();
    const month = String(date.getMonth() + 1).padStart(2, '0');
    const day = String(date.getDate()).padStart(2, '0');
    if (granularity === 'year') return `${year}`;
    if (granularity === 'month') return `${year}-${month}`;
    if (granularity === 'day') return `${year}-${month}-${day}`;
    const start = new Date(date);
    start.setDate(date.getDate() - date.getDay());
    return `${start.getFullYear()}-${String(start.getMonth() + 1).padStart(2, '0')}-${String(start.getDate()).padStart(2, '0')}`;
  }

  function groupLabel(date, granularity) {
    if (granularity === 'year') return `${date.getFullYear()} 年`;
    if (granularity === 'month') return date.toLocaleDateString('zh-CN', { year: 'numeric', month: 'long' });
    if (granularity === 'day') return date.toLocaleDateString('zh-CN', { month: 'long', day: 'numeric', weekday: 'short' });
    const end = new Date(date);
    end.setDate(date.getDate() + 6);
    return `${date.getMonth() + 1}/${date.getDate()} – ${end.getMonth() + 1}/${end.getDate()}`;
  }

  function timelineGroups(rows, granularity) {
    const groups = new Map();
    for (const row of rows) {
      const value = occurredOf(row);
      if (!value) continue;
      const date = new Date(value);
      if (Number.isNaN(date.getTime())) continue;
      const key = groupKey(date, granularity);
      if (!groups.has(key)) groups.set(key, { key, date, items: [] });
      groups.get(key).items.push(row);
    }
    return [...groups.values()].sort((left, right) => left.date - right.date);
  }

  async function renderTimeline(page) {
    const params = new URLSearchParams({ limit: String(memory.limit) });
    params.set('offset', String((memory.page - 1) * memory.limit));
    if (memory.query) params.set('q', memory.query);
    if (memory.kinds.length) params.set('kinds', memory.kinds.join(','));
    if (memory.tags.length) params.set('tags', memory.tags.join(','));
    if (memory.scope) params.set('scope', memory.scope);

    const data = await api(`/api/memory/records?${params}`);
    memory.data = data;
    syncViewCount();
    const groups = timelineGroups(data.items, memory.granularity);
    const dated = groups.reduce((total, group) => total + group.items.length, 0);

    const card = h('div', { class: 'card' });
    if (!groups.length) {
      card.append(h('div', { class: 'empty' }, '没有时间线数据'));
      page.append(card);
      return;
    }

    const first = groups[0].date;
    const last = groups[groups.length - 1].date;
    const zoomIndex = GRANULARITY.findIndex(([key]) => key === memory.granularity);

    card.append(h('div', { class: 'timeline-head' },
      h('span', { class: 'muted' },
        // 这一行说的是**本页**这几组覆盖的区间与条数，不是全部——以前它读起来像
        // 全局统计（旁边的视图条又写着"共 N 条"），两处数字对不上就会让人以为
        // 数据丢了。写清"本页"并给出全量总数。
        `本页 ${dated} 条 · ${first.toLocaleDateString('zh-CN', { month: 'short', year: 'numeric' })} → ${last.toLocaleDateString('zh-CN', { month: 'short', year: 'numeric' })}`
        + `（共 ${data.total} 条${data.total_is_window ? `，在最近 ${data.window} 条里统计` : ''}）`),
      h('div', { class: 'pager-btns' },
        h('div', { class: 'segmented small' },
          h('button', {
            text: '－', title: '缩小（年）',
            disabled: zoomIndex === 0,
            onclick: () => { memory.granularity = GRANULARITY[zoomIndex - 1][0]; renderMemoryPage(); },
          }),
          h('span', { class: 'segmented-label', text: GRANULARITY[zoomIndex][1] }),
          h('button', {
            text: '＋', title: '放大（日）',
            disabled: zoomIndex === GRANULARITY.length - 1,
            onclick: () => { memory.granularity = GRANULARITY[zoomIndex + 1][0]; renderMemoryPage(); },
          })),
        h('div', { class: 'segmented small' },
          ...['«', '‹', '›', '»'].map((symbol, index) => h('button', {
            text: symbol,
            onclick: () => {
              const target = [0, memory.groupIndex - 1, memory.groupIndex + 1, groups.length - 1][index];
              memory.groupIndex = Math.min(Math.max(0, target), groups.length - 1);
              const node = document.getElementById(`timeline-group-${memory.groupIndex}`);
              if (node) node.scrollIntoView({ behavior: 'smooth', block: 'start' });
            },
          })),
          h('span', { class: 'segmented-label', text: `${Math.min(memory.groupIndex + 1, groups.length)} / ${groups.length}` })))));

    const rail = h('div', { class: 'timeline-body' });
    groups.forEach((group, index) => {
      const block = h('div', { class: 'timeline-group', id: `timeline-group-${index}` },
        h('div', { class: 'timeline-group-head' },
          h('span', { class: 'timeline-group-label', text: groupLabel(group.date, memory.granularity) }),
          h('span', { class: 'timeline-dot' }),
          h('span', { class: 'muted small', text: `${group.items.length} 条目` })));
      for (const item of group.items) {
        block.append(h('div', { class: 'timeline-item', onclick: () => openRecord(item) },
          h('div', { class: 'timeline-when' },
            h('div', { text: monthDay(occurredOf(item)) }),
            h('div', { class: 'timeline-clock', text: fmtTime(occurredOf(item)).slice(-5) })),
          h('span', { class: 'timeline-node' }),
          h('div', { class: 'timeline-card' },
            h('p', { class: 'timeline-text' }, ...highlighted(item.title, memory.query)),
            item.body && item.body !== item.title
              ? h('p', { class: 'timeline-context' }, item.body.slice(0, 120))
              : null,
            chipList(item.entities, entityChip))));
      }
      rail.append(block);
    });
    card.append(rail);
    // 时间线此前只画了当前这一页，却没有任何翻页入口（表格视图有 `renderPager`），
    // 于是第 100 条之后的数据在这个视图里根本到不了。用同一个分页器。
    card.append(renderPager(data));
    page.append(card);
  }

  // ── 星座图视图

  // 冷 → 暖的感知渐变：越新越暖（与 Hindsight 的 heatColor 同一套端点）。
  function heatColor(t) {
    const value = Math.max(0, Math.min(1, t));
    const stops = [[56, 130, 220], [170, 130, 200], [240, 100, 60]];
    const segment = value * (stops.length - 1);
    const index = Math.min(Math.floor(segment), stops.length - 2);
    const fraction = segment - index;
    const from = stops[index];
    const to = stops[index + 1];
    const mix = (a, b) => Math.round(a + (b - a) * fraction);
    return `rgb(${mix(from[0], to[0])},${mix(from[1], to[1])},${mix(from[2], to[2])})`;
  }

  /** 把标题相同的记录折成一个节点，并重映射连线。 */
  function collapseDuplicateNodes(graph) {
    const groups = new Map();
    for (const node of graph.nodes || []) {
      const key = (node.title || '').trim() || node.id;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(node);
    }
    const nodes = [];
    const idMap = new Map();
    for (const [key, members] of groups) {
      const representative = { ...members[0] };
      if (members.length > 1) {
        representative.count = members.length;
        representative.title = `${key} ×${members.length}`;
        // 代表节点用最近一次提及的时间与最高权重，免得折叠后把新记录画成旧的。
        const times = members
          .map((member) => new Date(mentionedOf(member)).getTime())
          .filter((time) => !Number.isNaN(time));
        if (times.length) representative.mentioned_at = new Date(Math.max(...times)).toISOString();
        representative.weight = Math.max(...members.map((member) => Number(member.weight) || 0));
      }
      nodes.push(representative);
      for (const member of members) idMap.set(member.id, representative.id);
    }

    const seen = new Set();
    const links = [];
    for (const link of graph.links || []) {
      const source = idMap.get(link.source);
      const target = idMap.get(link.target);
      if (!source || !target || source === target) continue;
      const key = `${[source, target].sort().join('|')}|${link.type}`;
      if (seen.has(key)) continue;
      seen.add(key);
      links.push({ ...link, source, target });
    }

    const linkCounts = {};
    for (const [key] of LINK_TYPES) linkCounts[key] = 0;
    for (const link of links) linkCounts[link.type] = (linkCounts[link.type] || 0) + 1;

    return { nodes, links, linkCounts, hidden: (graph.nodes || []).length - nodes.length };
  }

  async function renderConstellation(page) {
    const params = new URLSearchParams({ limit: '200' });
    if (memory.query) params.set('q', memory.query);
    if (memory.kinds.length) params.set('kinds', memory.kinds.join(','));
    if (memory.tags.length) params.set('tags', memory.tags.join(','));
    const graph = await api(`/api/memory/graph?${params}`);
    memory.graph = graph;

    // 画布、节点坐标、缩放跨重绘保留：切换一个开关不该重建整页（滚动位置、
    // 筛选状态都会丢），也不该让图重新布局跳一下。
    const view = memory.graphView || (memory.graphView = {
      positions: new Map(),
      zoom: 0.9,
      panX: 0,
      panY: 0,
      hovered: null,
      dragging: false,
      moved: false,
    });

    const heatLabel = h('span', { class: 'graph-heat-label' });
    const heatMin = h('span');
    const heatMax = h('span');
    const canvas = h('canvas', { id: 'memory-graph' });
    const tooltip = h('div', { class: 'graph-tooltip', id: 'graph-tooltip', hidden: true });
    const canvasBox = h('div', { class: 'graph-canvas-box' },
      canvas,
      // 提示两种设备都照顾到：手机上是双指缩放、点节点看详情，没有悬停。
      h('div', { class: 'graph-hint', text: '滚轮/双指缩放 · 拖动平移 · 点节点看详情' }),
      h('div', { class: 'graph-heat' },
        heatLabel,
        h('div', { class: 'graph-heat-bar' }),
        h('div', { class: 'graph-heat-range' }, heatMin, heatMax)),
      h('button', { class: 'btn ghost small graph-full', text: '全屏', onclick: () => toggleGraphFullscreen(canvasBox) }),
      tooltip);

    const sideBody = h('div', { class: 'graph-side-body' });

    /** 只重画图：开关类控件都走它，不重建整页。 */
    const refresh = () => {
      heatLabel.textContent = memory.colorBy === 'weight'
        ? '重要度'
        : memory.colorBy === 'mentioned_at'
          ? '近期度 · 提及时间'
          : '近期度 · 发生时间';
      // 用**正在画的那条序列**的范围，而不是服务端固定给的 occurred_at 那一份。
      const drawn = memory.colorBy === 'mentioned_at'
        ? (graph.ranges && graph.ranges.mentioned) || graph.range
        : graph.range;
      const span = heatRange
        ? { min: new Date(heatRange.min).toISOString(), max: new Date(heatRange.max).toISOString() }
        : drawn;
      heatMin.textContent = memory.colorBy === 'weight' ? '低' : shortDate(span && span.min);
      heatMax.textContent = memory.colorBy === 'weight' ? '高' : shortDate(span && span.max);

      const collapsed = memory.collapseDuplicates ? collapseDuplicateNodes(graph) : null;
      const shown = collapsed || {
        nodes: graph.nodes || [],
        links: graph.links || [],
        linkCounts: graph.link_counts || {},
        hidden: 0,
      };
      const visibleLinks = (shown.links || []).filter((link) => memory.linkTypes.has(link.type));

      // `replaceChildren` 会把 null 变成 "null" 文本节点（只有 h() 会跳过空子
      // 节点），所以先过滤。以前这里直接把三元表达式的结果传进去，折叠重复为 0
      // 时——也就是平时——侧栏里就挂着一个孤零零的 "null"。
      sideBody.replaceChildren(...[
        h('div', { class: 'graph-stat' },
          h('div', {}, h('div', { class: 'label', text: '节点' }),
            h('div', { class: 'value', text: String(shown.nodes.length) })),
          h('div', {}, h('div', { class: 'label', text: '链接' }),
            h('div', { class: 'value', text: String(visibleLinks.length) }))),
        shown.hidden > 0
          ? h('p', { class: 'muted small', text: `已把 ${shown.hidden} 条内容重复的记录折进节点` })
          : null,
        h('div', { class: 'graph-legend' },
          ...LINK_TYPES.map(([key, label, color]) => h('div', { class: 'graph-legend-row' },
            h('i', { style: `background:${color}` }),
            h('span', { class: memory.linkTypes.has(key) ? '' : 'muted', text: label }),
            h('span', { class: 'muted', text: String(shown.linkCounts[key] || 0) })))),
      ].filter(Boolean));

      // 视图栏那行也得跟着更新——整页重绘被去掉之后，它不会自己刷新了。
      const countLabel = document.querySelector('#page-memory .view-count');
      if (countLabel) {
        countLabel.textContent = shown.hidden > 0
          ? `星座图：${graph.total} 条记忆（折叠为 ${shown.nodes.length} 个节点）`
          : `星座图：${graph.total} 条记忆`;
      }

      drawConstellation(canvas, tooltip, { nodes: shown.nodes, links: visibleLinks }, view);
    };

    const controls = h('div', { class: 'graph-controls' },
      h('label', { class: 'graph-control' },
        h('span', { text: '着色依据' }),
        (() => {
          const select = h('select', { class: 'select small' },
            h('option', { value: 'mentioned_at', text: '提及时间', selected: memory.colorBy === 'mentioned_at' }),
            h('option', { value: 'occurred_at', text: '发生时间', selected: memory.colorBy === 'occurred_at' }),
            h('option', { value: 'weight', text: '重要度', selected: memory.colorBy === 'weight' }));
          select.addEventListener('change', () => {
            memory.colorBy = select.value;
            refresh();
          });
          return select;
        })()),
      h('label', { class: 'graph-control', title: '把内容完全相同的记录折成一个节点并标出条数' },
        h('span', { text: '折叠重复' }),
        (() => {
          const input = h('input', { type: 'checkbox', checked: memory.collapseDuplicates });
          input.addEventListener('change', () => {
            memory.collapseDuplicates = input.checked;
            refresh();
          });
          return h('span', { class: 'switch small' }, input, h('span', { class: 'track' }));
        })()),
      h('div', { class: 'graph-control' },
        h('span', { text: '链接类型' }),
        ...LINK_TYPES.map(([key, label, color]) => {
          const button = h('button', {
            class: `link-toggle${memory.linkTypes.has(key) ? '' : ' off'}`,
          }, h('i', { style: `background:${color}` }), label);
          button.addEventListener('click', () => {
            if (memory.linkTypes.has(key)) memory.linkTypes.delete(key);
            else memory.linkTypes.add(key);
            button.classList.toggle('off', !memory.linkTypes.has(key));
            refresh();
          });
          return button;
        })));

    const layout = h('div', { class: 'graph-layout' }, canvasBox,
      h('aside', { class: 'graph-side' },
        h('h4', { text: '星座视图' }),
        h('p', { class: 'muted small' },
          '画布渲染的记忆地图：节点是记忆，连线是它们之间真实存在的关系。滚动缩放，拖动平移，悬停探索，点击查看详情。'),
        sideBody));

    page.append(h('div', { class: 'card' }, controls, layout));
    refresh();
  }

  function toggleGraphFullscreen(box) {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else box.requestFullscreen?.().catch(() => toast('浏览器拒绝了全屏请求', 'bad'));
  }

  /** 力导向布局 + canvas 绘制。
   *
   * 画布与事件监听只挂一次（存在 `canvas.__graphRuntime` 上）：切换开关时复用
   * 同一块画布、同一份节点坐标，图不会跳，也不会因为反复挂监听越跑越慢。 */
  function drawConstellation(canvas, tip, graph, view) {
    const context = canvas.getContext('2d');
    // 图例要写"这张图正在用的那条序列"的范围，由绘制方回填（见 heat()）。
    heatRange = null;
    const width = () => canvas.clientWidth || 640;
    const height = () => canvas.clientHeight || 520;

    // 节点对象每次都是新的，悬停状态不能跨重绘沿用。
    view.hovered = null;

    const nodes = graph.nodes || [];
    const byId = new Map(nodes.map((node) => [node.id, node]));
    const edges = (graph.links || [])
      .map((link) => ({ ...link, a: byId.get(link.source), b: byId.get(link.target) }))
      .filter((edge) => edge.a && edge.b);

    // 时间/权重 → 0..1 的热度。**按当前绘制的那条序列算范围**：配色可以选"提及
    // 时间"或"发生时间"，而 mentioned_at 总是不早于 occurred_at——拿 occurred_at
    // 的范围去归一化 mentioned_at，最新提及的那批节点会全部顶到最暖色，图例写的
    // 还是另一条序列。
    const timeOf = (node) =>
      new Date(memory.colorBy === 'mentioned_at' ? mentionedOf(node) : occurredOf(node)).getTime();
    const times = nodes.map(timeOf).filter((t) => !Number.isNaN(t));
    const minTime = times.length ? Math.min(...times) : 0;
    const maxTime = times.length ? Math.max(...times) : 1;
    heatRange = { min: minTime, max: maxTime };
    const maxWeight = Math.max(1, ...nodes.map((node) => Number(node.weight) || 0));
    const heat = (node) => {
      if (memory.colorBy === 'weight') return (Number(node.weight) || 0) / maxWeight;
      const value = timeOf(node);
      if (Number.isNaN(value) || maxTime === minTime) return 0.5;
      return (value - minTime) / (maxTime - minTime);
    };

    const degree = new Map();
    for (const edge of edges) {
      degree.set(edge.a.id, (degree.get(edge.a.id) || 0) + 1);
      degree.set(edge.b.id, (degree.get(edge.b.id) || 0) + 1);
    }

    // 坐标优先复用上一次的：只有新出现的节点才需要重新摆放。
    if (view.positions.size > 4000) view.positions.clear();
    let needsLayout = false;
    nodes.forEach((node, index) => {
      const cached = view.positions.get(node.id);
      if (cached) {
        node.wx = cached.wx;
        node.wy = cached.wy;
      } else {
        const angle = index * 2.399963;
        const radius = Math.sqrt(index + 1) * 26;
        node.wx = Math.cos(angle) * radius;
        node.wy = Math.sin(angle) * radius;
        needsLayout = true;
      }
      node.vx = 0;
      node.vy = 0;
    });

    const relax = (iterations) => {
      for (let step = 0; step < iterations; step += 1) {
        const alpha = 0.35 * (1 - step / iterations) + 0.02;
        for (let i = 0; i < nodes.length; i += 1) {
          const a = nodes[i];
          for (let j = i + 1; j < nodes.length; j += 1) {
            const b = nodes[j];
            let dx = a.wx - b.wx;
            let dy = a.wy - b.wy;
            let distance = Math.hypot(dx, dy) || 0.01;
            if (distance > 420) continue;
            const push = (2600 / (distance * distance)) * alpha;
            dx /= distance;
            dy /= distance;
            a.vx += dx * push;
            a.vy += dy * push;
            b.vx -= dx * push;
            b.vy -= dy * push;
          }
        }
        for (const edge of edges) {
          const dx = edge.b.wx - edge.a.wx;
          const dy = edge.b.wy - edge.a.wy;
          const distance = Math.hypot(dx, dy) || 0.01;
          const pull = (distance - 70) * 0.006 * alpha;
          const fx = (dx / distance) * pull;
          const fy = (dy / distance) * pull;
          edge.a.vx += fx;
          edge.a.vy += fy;
          edge.b.vx -= fx;
          edge.b.vy -= fy;
        }
        for (const node of nodes) {
          node.wx += node.vx = node.vx * 0.6;
          node.wy += node.vy = node.vy * 0.6;
        }
      }
    };

    const toScreen = (node) => ({
      x: width() / 2 + (node.wx + view.panX) * view.zoom,
      y: height() / 2 + (node.wy + view.panY) * view.zoom,
    });

    const paint = () => {
      const ratio = window.devicePixelRatio || 1;
      canvas.width = width() * ratio;
      canvas.height = height() * ratio;
      context.setTransform(ratio, 0, 0, ratio, 0, 0);
      context.clearRect(0, 0, width(), height());

      const dim = view.hovered ? new Set([view.hovered.id]) : null;
      if (view.hovered) {
        for (const edge of edges) {
          if (edge.a.id === view.hovered.id) dim.add(edge.b.id);
          if (edge.b.id === view.hovered.id) dim.add(edge.a.id);
        }
      }

      for (const edge of edges) {
        const active = !view.hovered || edge.a.id === view.hovered.id || edge.b.id === view.hovered.id;
        const a = toScreen(edge.a);
        const b = toScreen(edge.b);
        context.strokeStyle = LINK_TYPES.find(([key]) => key === edge.type)[2];
        context.globalAlpha = active ? 0.5 : 0.08;
        context.lineWidth = active ? 1.2 : 0.8;
        context.beginPath();
        context.moveTo(a.x, a.y);
        const midX = (a.x + b.x) / 2 + (b.y - a.y) * 0.08;
        const midY = (a.y + b.y) / 2 - (b.x - a.x) * 0.08;
        context.quadraticCurveTo(midX, midY, b.x, b.y);
        context.stroke();
      }
      context.globalAlpha = 1;

      for (const node of nodes) {
        const point = toScreen(node);
        const radius = 3.2 + Math.sqrt(degree.get(node.id) || 0) * 1.7;
        const faded = dim && !dim.has(node.id);
        context.beginPath();
        context.fillStyle = heatColor(heat(node));
        context.globalAlpha = faded ? 0.25 : 0.95;
        context.arc(point.x, point.y, radius * (view.hovered === node ? 1.5 : 1), 0, Math.PI * 2);
        context.fill();
        context.globalAlpha = faded ? 0.2 : 0.85;
        context.strokeStyle = 'rgba(255,255,255,0.55)';
        context.lineWidth = 0.8;
        context.stroke();
      }
      context.globalAlpha = 1;

      // 标签：悬停优先，其次在高缩放级别显示度数高的节点。
      context.font = '11px system-ui, sans-serif';
      context.fillStyle = getComputedStyle(document.body).color;
      for (const node of nodes) {
        const show = view.hovered === node || (view.zoom > 0.55 && (degree.get(node.id) || 0) >= 3);
        if (!show) continue;
        const point = toScreen(node);
        context.globalAlpha = view.hovered === node ? 1 : 0.7;
        context.fillText(String(node.title || '').slice(0, 18), point.x + 7, point.y + 3);
      }
      context.globalAlpha = 1;
    };

    const nodeAt = (event, radius = 16) => {
      const rect = canvas.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      let best = null;
      let bestDistance = radius;
      for (const node of nodes) {
        const point = toScreen(node);
        const distance = Math.hypot(point.x - x, point.y - y);
        if (distance < bestDistance) {
          bestDistance = distance;
          best = node;
        }
      }
      return best;
    };

    // 运行时快照：监听器只挂一次，因此它们必须每次从画布上取最新的这份。
    canvas.__graphRuntime = { nodes, edges, degree, heat, paint, nodeAt, toScreen, view };

    if (!canvas.__graphListenersAttached) {
      canvas.__graphListenersAttached = true;
      const runtime = () => canvas.__graphRuntime;

      // 全部走指针事件（鼠标 / 触屏 / 触控笔一套代码）。
      //
      // 为什么不能在触屏上靠 click：手指按在画布上时，浏览器默认把这次触摸当成
      // "滚动页面"，一旦判定成滚动就不再生成本次点击的 click——所以点节点没反应。
      // 现在点击判定自己算（pointerup 且几乎没位移），并且 CSS 上给画布
      // `touch-action: pan-y`：竖划仍然滚页面（手机上最常用的手势），横划与双指
      // 交给画布，浏览器也不会自己缩放页面。
      //
      // 另外 movementX/movementY 在触屏指针上是 0，位移必须自己用 clientX/Y 算。
      const pointers = new Map(); // pointerId -> 最新位置
      const TAP_SLOP = 6; // 位移超过它就算拖动，不再当点击
      let panStart = null; // 单指/鼠标的按下点
      let pinchStart = null; // 双指缩放起点 { distance, zoom }

      const endGesture = () => {
        panStart = null;
        pinchStart = null;
        pointers.clear();
      };

      canvas.addEventListener('pointerdown', (event) => {
        const current = runtime();
        if (!current) return;
        if (event.pointerType === 'mouse' && event.button !== 0) return;
        if (canvas.setPointerCapture) {
          // 指针捕获：拖到画布外面也能继续跟手（否则会停在原地，还得等下一次
          // 按下才恢复）。拿不到就跳过——某些浏览器在指针已失效时会抛
          // NotFoundError，而那不影响拖动与点击本身。
          try {
            canvas.setPointerCapture(event.pointerId);
          } catch (_) {
            // 忽略：捕获只是体验优化，不是这次交互的前提。
          }
        }
        pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });

        if (pointers.size === 2) {
          const [a, b] = [...pointers.values()];
          pinchStart = {
            distance: Math.hypot(a.x - b.x, a.y - b.y),
            zoom: current.view.zoom,
          };
          panStart = null;
          current.view.moved = true; // 双指手势不可能是"点一下"
        } else if (pointers.size === 1) {
          panStart = { x: event.clientX, y: event.clientY };
          current.view.dragging = true;
          current.view.moved = false;
          canvas.style.cursor = 'grabbing';
        }
      });

      canvas.addEventListener('pointermove', (event) => {
        const current = runtime();
        if (!current) return;
        const previous = pointers.get(event.pointerId);
        if (previous) pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });

        if (pinchStart && pointers.size === 2) {
          const [a, b] = [...pointers.values()];
          const distance = Math.hypot(a.x - b.x, a.y - b.y);
          if (pinchStart.distance > 0) {
            current.view.zoom = Math.min(
              4,
              Math.max(0.15, pinchStart.zoom * (distance / pinchStart.distance)),
            );
            current.paint();
          }
          return;
        }

        if (current.view.dragging && panStart && previous) {
          if (
            Math.abs(event.clientX - panStart.x) > TAP_SLOP
            || Math.abs(event.clientY - panStart.y) > TAP_SLOP
          ) {
            current.view.moved = true;
          }
          // 屏幕位移换算回图坐标：屏幕 = 中心 + (世界 + pan) * zoom。
          current.view.panX += (event.clientX - previous.x) / current.view.zoom;
          current.view.panY += (event.clientY - previous.y) / current.view.zoom;
          current.paint();
          return;
        }

        // 悬停只有鼠标有：触屏上让 tooltip 跟着手指闪没有意义，点下去就是详情。
        if (event.pointerType !== 'mouse') return;
        const { nodeAt: pick } = current;
        const node = pick(event);
        if (node !== current.view.hovered) {
          current.view.hovered = node;
          canvas.style.cursor = node ? 'pointer' : 'grab';
          current.paint();
        }
        if (node && tip) {
          const rect = canvas.getBoundingClientRect();
          tip.hidden = false;
          clear(tip);
          tip.append(
            h('div', { class: 'tip-kind', text: node.label || node.kind }),
            h('div', { class: 'tip-title', text: node.title || '' }),
            h('div', { class: 'tip-meta', text: `${node.scope_label || '全局'} · ${shortDate(mentionedOf(node))}` }));
          // 画布是 `overflow: hidden` 的：贴着右/下边缘的节点会把 tooltip 顶出去
          // 裁掉一截。先摆出来量一下实际尺寸，再收进画布内。
          const left = Math.min(event.clientX - rect.left + 14, rect.width - tip.offsetWidth - 8);
          const top = Math.min(event.clientY - rect.top + 10, rect.height - tip.offsetHeight - 8);
          tip.style.left = `${Math.max(8, left)}px`;
          tip.style.top = `${Math.max(8, top)}px`;
        } else if (tip) {
          tip.hidden = true;
        }
      });

      const finishPointer = (event) => {
        const current = runtime();
        const wasTap = Boolean(
          current && pointers.has(event.pointerId) && !current.view.moved && pointers.size === 1,
        );
        pointers.delete(event.pointerId);
        if (canvas.hasPointerCapture && canvas.hasPointerCapture(event.pointerId)) {
          canvas.releasePointerCapture(event.pointerId);
        }
        if (pointers.size < 2) pinchStart = null;
        if (pointers.size === 0 && current) {
          panStart = null;
          current.view.dragging = false;
          canvas.style.cursor = current.view.hovered ? 'pointer' : 'grab';
        }
        if (!wasTap) return;
        // 触屏上手指按不准：命中半径给大一点，否则要点好几次才中。
        const node = current.nodeAt(event, event.pointerType === 'mouse' ? 16 : 26);
        if (node) openRecord(node);
      };
      canvas.addEventListener('pointerup', finishPointer);
      canvas.addEventListener('pointercancel', (event) => {
        endGesture();
        const current = runtime();
        if (current) {
          current.view.dragging = false;
          current.paint();
        }
        if (canvas.hasPointerCapture && canvas.hasPointerCapture(event.pointerId)) {
          canvas.releasePointerCapture(event.pointerId);
        }
      });

      canvas.addEventListener('pointerleave', () => {
        const current = runtime();
        if (!current) return;
        current.view.hovered = null;
        if (tip) tip.hidden = true;
        current.paint();
      });

      canvas.addEventListener('wheel', (event) => {
        const current = runtime();
        if (!current) return;
        event.preventDefault();
        const factor = event.deltaY < 0 ? 1.12 : 0.89;
        current.view.zoom = Math.min(4, Math.max(0.15, current.view.zoom * factor));
        current.paint();
      }, { passive: false });
    }

    const rememberPositions = () => {
      for (const node of nodes) view.positions.set(node.id, { wx: node.wx, wy: node.wy });
    };

    if (!needsLayout) {
      // 只是开关变了：坐标还在，直接重画，图不会跳。
      rememberPositions();
      paint();
      return;
    }

    // 有新节点才跑布局，分帧执行避免卡住首屏。
    let remaining = 140;
    const step = () => {
      if (!canvas.isConnected) return;
      relax(6);
      remaining -= 6;
      paint();
      if (remaining > 0) requestAnimationFrame(step);
      else rememberPositions();
    };
    step();
  }

  // ── 详情

  async function openRecord(item) {
    memory.selected = item;
    openModal(item.title || '记忆详情', h('div', { class: 'loading', text: '读取详情…' }));
    let payload;
    try {
      payload = await api(`/api/memory/record/${encodeURIComponent(item.kind)}/${encodeURIComponent(item.id)}`);
    } catch (problem) {
      $('#modal-body').replaceChildren(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    const record = payload.record || {};
    const body = h('div', { class: 'record-detail' });
    body.append(h('dl', { class: 'kv' },
      h('dt', { text: '类型' }), h('dd', {}, h('span', { class: 'badge kind', text: payload.label })),
      h('dt', { text: '作用域' }), h('dd', { text: payload.scope_label || '全局' }),
      h('dt', { text: '发生时间' }), h('dd', { text: fmtTime(occurredOf(item)) }),
      h('dt', { text: '提及时间' }), h('dd', { text: fmtTime(mentionedOf(item)) }),
      item.status ? h('dt', { text: '状态' }) : null,
      item.status ? h('dd', { text: item.status }) : null,
      // 有的表（用户档案、群档案、会话摘要）没有 `id` 列，接口原样返回行，
      // 这里直接拼接会印出 "kovi_bot_user_profiles · undefined"。
      h('dt', { text: '标识' }), h('dd', {
        class: 'mono',
        text: record.id ? `${payload.table} · ${record.id}` : payload.table,
      })));

    if ((item.entities || []).length) {
      body.append(h('div', { class: 'field-label', style: 'margin-top:14px', text: '实体' }),
        h('div', { class: 'chip-cell' }, ...item.entities.map(entityChip)));
    }
    if ((item.tags || []).length) {
      body.append(h('div', { class: 'field-label', style: 'margin-top:12px', text: '标签' }),
        h('div', { class: 'chip-cell' }, ...item.tags.map((tag) => tagChip(tag, null, false))));
    }

    const bodyFields = ['content', 'text', 'summary', 'proposition', 'question', 'details'];
    for (const field of bodyFields) {
      if (typeof record[field] === 'string' && record[field].trim()) {
        body.append(h('div', { class: 'body-text', style: 'margin-top:14px' },
          ...highlighted(record[field], memory.query)));
        break;
      }
    }

    const nested = Object.entries(record).filter(([, value]) => value !== null && typeof value === 'object');
    if (nested.length) {
      const details = h('details', { style: 'margin-top:16px' },
        h('summary', { style: 'cursor:pointer;color:var(--text-dim);font-size:12.5px', text: '原始字段' }));
      for (const [key, value] of nested) {
        details.append(
          h('div', { class: 'field-name', style: 'margin-top:12px', text: key }),
          h('pre', { class: 'json', text: JSON.stringify(value, null, 2) }));
      }
      body.append(details);
    }
    $('#modal-body').replaceChildren(body);
  }

  // ── 人物

  async function renderPeopleTab(page) {
    const search = h('input', { class: 'input search', placeholder: '按 QQ 号或身份搜索…', value: memory.query });
    page.append(h('div', { class: 'card tight' },
      h('div', { class: 'search-row' }, search,
        h('button', {
          class: 'btn', text: '搜索',
          onclick: () => { memory.query = search.value; renderMemoryPage(); },
        }))));

    const data = await api(`/api/memory/people?limit=60${memory.query ? `&q=${encodeURIComponent(memory.query)}` : ''}`);
    page.append(h('div', { class: 'card-head' },
      h('h3', { text: `人物（${data.total}）` }),
      h('span', { class: 'hint', text: 'canonical Person 及其 QQ 身份' })));

    const grid = h('div', { class: 'person-list' });
    for (const person of data.items) {
      grid.append(h('div', { class: 'person-card', onclick: () => openPerson(person) },
        h('div', { class: 'name', text: person.display }),
        h('div', { class: 'ids', text: person.identities || person.id }),
        bars(person.relation),
        h('div', { class: 'ids', style: 'margin-top: 8px', text: `记忆 ${person.memory_count} 条` })));
    }
    if (!data.items.length) grid.append(h('div', { class: 'empty', text: '还没有人物记录' }));
    page.append(grid);
  }

  // 关系五维。三个刻意的选择：
  //
  // 1. **张力也要画**。它是唯一直接改变行为的维度（群聊静默门控的判据），此前
  //    恰好是唯一没画的那一维——页面上写着"关系"，却少了那一根会让她不回话的线。
  // 2. **以 0 为中心**。五个维度都是 -1..1 的有符号量，0 是中性。原来把
  //    (v+1)/2 画成 0..100% 的条，0 看起来就是"50%"：一个中性的人被画成"好感
  //    一半"，而真正该一眼看出的"负"（不喜欢、紧张）在视觉上完全消失。
  // 3. **给数字**。没有数字时相差 0.05 的两个人长得一模一样，也没法核对后台
  //    与库里是否一致。
  const RELATION_DIMENSIONS = [
    ['affinity', '好感'],
    ['trust', '信任'],
    ['familiarity', '熟悉'],
    ['comfort', '自在'],
    ['tension', '张力'],
  ];

  function bars(relation) {
    if (!relation) return null;
    const node = h('div', { class: 'bars' });
    for (const [key, label] of RELATION_DIMENSIONS) {
      const value = relation[key];
      if (value === null || value === undefined) continue;
      const clamped = Math.max(-1, Math.min(1, Number(value)));
      // 从中点向两侧长：宽度是"离中性有多远"，方向由侧别与颜色表达。
      const half = Math.round(Math.abs(clamped) * 50);
      node.append(h('div', { class: 'bar' },
        h('span', { text: label }),
        h('div', { class: 'track' },
          h('div', { class: 'center' }),
          h('div', {
            class: `fill ${clamped < 0 ? 'negative' : 'positive'}`,
            style: `width: ${half}%; ${clamped < 0 ? 'right: 50%;' : 'left: 50%;'}`,
          })),
        h('span', { class: 'value', text: clamped.toFixed(2) })));
    }
    return node;
  }

  async function openPerson(person) {
    openModal(person.display, h('div', { class: 'loading', text: '读取人物详情…' }));
    let payload;
    try {
      payload = await api(`/api/memory/person/${encodeURIComponent(person.id)}`);
    } catch (problem) {
      $('#modal-body').replaceChildren(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    const body = h('div');
    body.append(h('dl', { class: 'kv' },
      h('dt', { text: 'PersonId' }), h('dd', { class: 'mono', text: payload.id }),
      h('dt', { text: '外部身份' }), h('dd', {
        text: (payload.identities || []).map((identity) => `${identity.platform}:${identity.external_id}`).join('、') || '—',
      }),
      h('dt', { text: '会话' }), h('dd', {
        text: (payload.conversations || []).map((conversation) => `${conversation.kind} ${conversation.external_id || conversation.id.slice(0, 8)}`).join('、') || '—',
      })));

    if (payload.relation) {
      body.append(h('h4', { text: '关系' }),
        h('div', { class: 'hint', text: '中点是中性（0）；这里是按流逝时间衰减之后的值，也就是她此刻真正在用的那份。张力 ≥ 0.35 会进语气提示，≥ 0.6 会让群聊里的点名回合被静默。' }),
        bars(payload.relation));
    }
    if (payload.relation_notes && payload.relation_notes.length) {
      // 她在独处反思时写下的相处结论。只读展示：它不参与"回不回"的判定，
      // 那个由关系张力（上面的"关系"）决定。
      body.append(h('h4', { text: `相处结论（${payload.relation_notes.length}）` }),
        h('div', { class: 'hint', text: '模型在反思时记下的观察，仅供参考；门控用的是关系张力。' }));
      const notes = h('div', { class: 'record-list' });
      for (const note of payload.relation_notes) {
        notes.append(h('div', { class: 'record' },
          h('div', { class: 'record-title', text: note.target || '（未具名）' }),
          h('div', { class: 'record-body', text: note.note }),
          // `confidence_milli` 是**千分比**（写入侧收口在 0..=200，另一个消费方
          // 也按 /1000 读）。这里此前按 /100 渲染，0.13 会显示成「置信 1.30」。
          h('div', { class: 'ids', text: `${fmtTime(note.observed_at)}　置信 ${(Number(note.confidence_milli || 0) / 1000).toFixed(2)}` })));
      }
      body.append(notes);
    }
    if (payload.affect) {
      const affect = payload.affect;
      body.append(h('h4', { text: '情绪' }),
        h('dl', { class: 'kv' },
          h('dt', { text: '愉悦度' }), h('dd', { text: Number(affect.valence ?? 0).toFixed(2) }),
          h('dt', { text: '唤醒度' }), h('dd', { text: Number(affect.arousal ?? 0).toFixed(2) }),
          h('dt', { text: '社交能量' }), h('dd', { text: Number(affect.social_energy ?? 0).toFixed(2) }),
          h('dt', { text: '好奇' }), h('dd', { text: Number(affect.curiosity ?? 0).toFixed(2) })));
    }
    if (payload.records && payload.records.length) {
      body.append(h('h4', { text: `相关记录（${payload.records.length}）` }));
      const list = h('div', { class: 'record-list' });
      for (const item of payload.records) {
        list.append(recordNode(item, () => openRecord(item), false));
      }
      body.append(list);
    }
    $('#modal-body').replaceChildren(body);
  }

  /**
   * 一条记忆记录（概览的"最近变化"与人物弹窗共用）。
   *
   * 行内的排版是按"扫"来定的，不是按"读"：
   *   - 第一行只放**分类 + 作用域 + 时间**，时间靠右对齐——一列下来时间能连成一条
   *     竖线，比每行都从左往右找要快；
   *   - 标题最多两行（`clamp-2`）：群聊记忆的标题可以很长，不夹住的话一条就占掉
   *     半屏，十条"最近变化"要滚三屏才看得完；
   *   - 正文只在**它比标题多说了一点**的时候才出现。旧版记忆的 body 和 title 本来
   *     就是同一段文字，Mind 的 body 往往以 title 开头，照抄一遍纯粹是浪费行高。
   */
  function recordNode(item, onClick, active) {
    const needle = memory.query;
    const body = recordBodyPreview(item);
    return h('div', {
      class: `record${active ? ' active' : ''}`,
      onclick: onClick,
    },
      h('div', { class: 'record-meta' },
        h('span', { class: 'badge kind', text: item.label || item.kind }),
        statusBadge(item.status),
        recordScope(item.scope_label),
        h('span', { class: 'record-when', text: relative(item.occurred_at) || fmtTime(item.occurred_at) })),
      h('div', { class: 'record-title clamp-2' }, ...highlighted(item.title, needle)),
      body ? h('div', { class: 'record-body clamp-2' }, ...highlighted(body, needle)) : null);
  }

  /**
   * 作用域那一段。
   *
   * 服务端把"没有作用域"也写成了字面量 `全局`（memory_api 的 scope_label_fallback），
   * 所以不能只判空——线上实测每行都印一个"全局"，和状态徽标一样是纯噪声。
   */
  function recordScope(label) {
    const text = String(label || '').trim();
    if (!text || text === '全局' || text.toLowerCase() === 'global') return null;
    return h('span', { class: 'record-scope', text });
  }

  /** 这些状态是"一切正常"的默认值：每行挂一个只是噪声，真正有信息量的才给 badge。 */
  const QUIET_STATUSES = new Set(['', 'active', 'open', 'conversation', 'pending']);

  /** 有信息量的状态翻成中文再显示——界面上不该出现 `resolved` 这种库里的枚举。 */
  function statusBadge(status) {
    const raw = String(status || '').trim();
    if (QUIET_STATUSES.has(raw.toLowerCase())) return null;
    return h('span', { class: 'badge', text: tagLabel(raw.toLowerCase()) });
  }

  /** 正文预览：与标题重复（或只是标题的续写）就返回空串，不再占一行。 */
  function recordBodyPreview(item) {
    const body = String(item.body || '').trim();
    const title = String(item.title || '').trim();
    if (!body || body === title) return '';
    if (title && body.startsWith(title)) return '';
    const flat = body.split(/\s+/).filter(Boolean).join(' ');
    return flat.length > 160 ? `${flat.slice(0, 160)}…` : flat;
  }

  // ───────────────────────────── 模型 ─────────────────────────────
  //
  // 换模型这件事的专用页：选服务商预设 → 填密钥 → 测一下 → 应用。写进去的还是
  // `[server_config]`（落在运行时覆盖配置，0600），所以配置页、概览页与
  // `#系统信息` 看到的是同一份真相；这里只是把"换一个模型"从"改五个字段、再去
  // 服务器环境里塞密钥"变成一次下拉加一次点击。
  //
  // 密钥只进不出：接口从不回显它，本页也从不预填。

  const MODEL_PROVIDERS = [
    {
      id: 'deepseek', label: 'DeepSeek（内置默认）',
      url: 'https://api.deepseek.com', wire_api: 'chat_completions',
      thinking_mode: 'disabled', supports_vision: false,
      models: ['deepseek-v4-flash', 'deepseek-chat', 'deepseek-reasoner'],
    },
    {
      id: 'openai', label: 'OpenAI',
      url: 'https://api.openai.com/v1', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: true,
      models: ['gpt-4o-mini', 'gpt-4o', 'gpt-4.1', 'o4-mini'],
    },
    {
      id: 'zhipu', label: '智谱 GLM',
      url: 'https://open.bigmodel.cn/api/paas/v4', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: true,
      models: ['glm-4-plus', 'glm-4-air', 'glm-4v-plus'],
    },
    {
      id: 'moonshot', label: '月之暗面 Kimi',
      url: 'https://api.moonshot.cn/v1', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: false,
      models: ['kimi-k2-0905-preview', 'moonshot-v1-32k'],
    },
    {
      id: 'dashscope', label: '阿里通义千问',
      url: 'https://dashscope.aliyuncs.com/compatible-mode/v1', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: true,
      models: ['qwen-plus', 'qwen-max', 'qwen-vl-max'],
    },
    {
      id: 'siliconflow', label: '硅基流动',
      url: 'https://api.siliconflow.cn/v1', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: false,
      models: ['deepseek-ai/DeepSeek-V3', 'Qwen/Qwen2.5-72B-Instruct'],
    },
    {
      id: 'openrouter', label: 'OpenRouter',
      url: 'https://openrouter.ai/api/v1', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: true,
      models: ['deepseek/deepseek-chat', 'openai/gpt-4o-mini', 'anthropic/claude-3.5-sonnet'],
    },
    {
      id: 'ollama', label: '本地 Ollama / 兼容端点（免密钥）',
      url: 'http://127.0.0.1:11434/v1', wire_api: 'chat_completions',
      thinking_mode: 'auto', supports_vision: false, requires_auth: false,
      models: ['qwen2.5:7b', 'llama3.1:8b'],
    },
    {
      id: 'custom', label: '自定义（OpenAI 兼容 / Responses）',
      url: '', wire_api: 'chat_completions', thinking_mode: 'auto', supports_vision: false,
      models: [],
    },
  ];

  /** 模型页状态。切页会整页重建，靠它挂住预设选择、测试结果与结果节点。 */
  /** 模型页状态：`data` 是最近一次 /api/model，`provider` 记住服务商下拉的选择。 */
  const model = { data: null, provider: 'deepseek', test: null, testNode: null };

  async function renderModelPage() {
    const page = $('#page-model');
    clear(page);
    page.append(h('div', { class: 'loading', text: '读取模型配置…' }));
    let data;
    try {
      data = await api('/api/model');
    } catch (problem) {
      clear(page);
      page.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    if (currentPage !== 'model') return;
    model.data = data;
    clear(page);
    page.append(renderModelStatus(data));
    // 只有两块：上面是"她现在跑的是什么"，下面是可切换的档案卡。
    // 新增/编辑走弹窗（openModelForm），不再在页面里占一张卡。
    page.append(renderModelProfiles(data));
  }

  function renderModelStatus(data) {
    const current = data.current || {};
    const profilesPath = (data.profiles && data.profiles.path) || '';
    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', {},
          '当前生效 ',
          h('span', {
            class: current.enabled ? 'pill ok' : 'pill bad',
            text: current.enabled ? '外部模型已启用' : '外部模型已禁用',
          })),
        h('div', { class: 'hint', text: current.enabled
          ? '她在用这一套回话；想换就点下面「模型档案」里的卡片'
          : '关掉时她只用本地能力（Core / Intrinsic），配置照旧保留' })),
      h('div', { class: 'model-facts' },
        modelFact('模型', current.model_name || '—'),
        modelFact('地址', current.endpoint || '—'),
        modelFact('协议', current.wire_api === 'responses' ? 'Responses' : 'Chat Completions'),
        modelFact('思考', current.thinking_mode === 'disabled' ? '关闭（省输出预算）' : '交给服务商默认'),
        modelFact('视觉', current.supports_vision ? '主模型直接读图' : '交给独立视觉模型'),
        h('div', { class: 'model-fact' },
          h('span', { class: 'model-fact-label', text: 'API Key' }),
          h('span', {
            class: `pill ${current.has_key ? 'ok' : 'bad'}`,
            text: current.key_source_text || '未知',
          }))),
      h('div', { class: 'hint', text: `配置改动写在 ${data.config_file || '—'}（权限 0600）；档案写在 ${profilesPath}。` }));
  }

  function modelFact(label, value) {
    // 端点地址常常比这一格宽：截断显示，但鼠标停上去要看得到全文。
    return h('div', { class: 'model-fact', title: `${label}：${value}` },
      h('span', { class: 'model-fact-label', text: label }),
      h('span', { class: 'model-fact-value mono', text: value }));
  }

  /**
   * 一套档案 = 一张可点的卡片。
   *
   * 这一页最常做的事是"换一套模型"，所以卡片本身就是主控件：点一下切过去，
   * 不为这件事再要求人去右边找一个"应用"小按钮。`切换 / 编辑 / 删除` 三个按钮
   * 是给键盘与不习惯点卡片的人留的（卡片本身没有 role/tabindex，嵌套可交互元素
   * 在 ARIA 里是无效结构，所以键盘入口就靠这三个真按钮）。
   *
   * 「正在用」由后端给（`profile.active`）：页面不再自己重算"地址 + 模型名"那个
   * 口径——那份判定同时管着删除闸门，两处各写一份迟早会不一致。正在用的那张不给
   * 删：删除按钮就地灰掉并说明原因（后端对直接打接口的请求同样返回 409）。
   */
  function profileCard(profile) {
    const isActive = Boolean(profile.active);
    const switchTo = () => switchModelProfile(profile);
    const color = entityColor(profile.label);
    return h('div', {
      class: `provider-card${isActive ? ' active' : ''}`,
      title: isActive ? '正在使用这一套' : `点一下切到「${profile.label}」`,
      onclick: () => { if (!isActive) switchTo(); },
    },
      h('span', {
        class: 'provider-avatar',
        style: `color:${color};border-color:${color}55;background:${color}1a`,
      }, avatarLetter(profile.label)),
      h('div', { class: 'provider-main' },
        h('div', { class: 'provider-head' },
          h('span', { class: 'provider-label', text: profile.label }),
          ...profileBadges(profile, isActive)),
        h('div', { class: 'provider-meta', text: profileMeta(profile) }),
        h('div', {
          class: 'provider-url mono',
          title: profile.url || '',
          text: profile.url || '（没有填地址）',
        })),
      h('div', { class: 'provider-actions' },
        isActive ? null : h('button', {
          class: 'btn small', text: '切换',
          title: '立刻切到这一套（写进运行时覆盖配置，不需要重启）',
          onclick: (event) => { event.stopPropagation(); switchTo(); },
        }),
        h('button', {
          class: 'btn ghost small', text: '编辑',
          title: '改这套档案（改完就地更新，不用另存一套）',
          onclick: (event) => { event.stopPropagation(); openModelForm(profile); },
        }),
        h('button', {
          class: 'btn danger small', text: '删除',
          disabled: isActive,
          title: isActive
            ? '正在用的这一套不能删：先切到另一套档案，或在配置页关掉外部模型'
            : `删掉「${profile.label}」这套档案（不影响当前生效的配置）`,
          onclick: (event) => { event.stopPropagation(); removeModelProfile(profile); },
        })));
  }

  /** 首字：中文取第一个字，英文取首字母（大写）。 */
  function avatarLetter(label) {
    const text = String(label || '').trim();
    return text ? text.slice(0, 1).toUpperCase() : '?';
  }

  /** 「带密钥 / 无密钥」「能读图」「免鉴权」：一眼决定切到哪一套。 */
  function profileBadges(profile, isActive) {
    const badges = [];
    if (isActive) badges.push(h('span', { class: 'badge kind', text: '正在用' }));
    badges.push(h('span', {
      class: `badge ${profile.has_key ? 'ok' : 'warn'}`,
      text: profile.has_key ? '带密钥' : '无密钥',
    }));
    if (profile.supports_vision) badges.push(h('span', { class: 'badge', text: '能读图' }));
    if (profile.requires_auth === false) badges.push(h('span', { class: 'badge', text: '免鉴权' }));
    return badges;
  }

  /** 卡片副标题：协议 · 模型名 · 输出上限。 */
  function profileMeta(profile) {
    const parts = [
      profile.wire_api === 'responses' ? 'Responses' : 'Chat Completions',
      profile.model_name || '（没有填模型名）',
    ];
    if (profile.max_output_tokens) parts.push(`最多 ${profile.max_output_tokens} token`);
    return parts.join(' · ');
  }

  /** 地址反查服务商预设：编辑一套已有档案时，下拉要能选中它当年是从哪家建的。 */
  function guessProvider(url) {
    const text = String(url || '').trim();
    if (!text) return '';
    const hit = MODEL_PROVIDERS.find((item) => item.url && text.startsWith(item.url));
    return hit ? hit.id : 'custom';
  }

  async function switchModelProfile(profile) {
    try {
      await api('/api/model/apply', { method: 'POST', body: { profile_id: profile.id } });
      toast(`已切到「${profile.label}」`, 'ok');
      await renderModelPage();
      // 概览页/顶栏看的是同一份 /api/status，顺手让它别再拿缓存里的旧模型名。
      await refreshHealth();
    } catch (problem) {
      toast(problem.message, 'bad', 7000);
    }
  }

  /**
   * 删除一套档案。正在用的那张的按钮是灰的，正常点不到；这里仍按后端的话如实转达
   * ——列表是上一次拉取的快照，从渲染到点下去之间完全可能刚好被切过去（那时接口
   * 返回 409）。
   */
  async function removeModelProfile(profile) {
    if (!window.confirm(`删除档案「${profile.label}」？（不影响当前生效的配置）`)) return;
    try {
      await api(`/api/model/profiles/${encodeURIComponent(profile.id)}`, { method: 'DELETE' });
      toast('已删除', 'ok');
    } catch (problem) {
      toast(problem.message, 'bad', 7000);
    }
    await renderModelPage();
  }

  /** 当前生效的那套还没存成档案时，列表里得有个东西——空列表看不出她现在跑的是什么。 */
  function currentEndpointCard(current) {
    const color = entityColor('当前配置');
    return h('div', { class: 'provider-card active' },
      h('span', {
        class: 'provider-avatar',
        style: `color:${color};border-color:${color}55;background:${color}1a`,
      }, '当'),
      h('div', { class: 'provider-main' },
        h('div', { class: 'provider-head' },
          h('span', { class: 'provider-label', text: '当前生效的配置' }),
          // 关掉外部模型时她只用本地能力，这张卡就不是"正在用"了：
          // 标成「正在用」会和上面那行"外部模型已禁用"自相矛盾。
          current.enabled
            ? h('span', { class: 'badge kind', text: '正在用' })
            : h('span', { class: 'badge warn', text: '未启用' }),
          h('span', {
            class: `badge ${current.has_key ? 'ok' : 'warn'}`,
            text: current.has_key ? '带密钥' : '无密钥',
          }),
          current.supports_vision ? h('span', { class: 'badge', text: '能读图' }) : null,
          current.requires_auth === false ? h('span', { class: 'badge', text: '免鉴权' }) : null),
        h('div', { class: 'provider-meta', text: profileMeta(current) }),
        h('div', {
          class: 'provider-url mono',
          title: current.url || '',
          text: current.url || '（没有填地址）',
        })),
      h('div', { class: 'provider-actions' },
        h('button', {
          class: 'btn small', text: '存成档案',
          title: '把当前这套填进表单，起个名字存下来，以后一键切回',
          onclick: () => openModelForm(null),
        })));
  }

  function renderModelProfiles(data) {
    const profiles = (data.profiles && data.profiles.items) || [];
    const current = data.current || {};
    // 已经有一套档案正好是当前生效的，就不用再补一张"当前配置"（会重复）。
    const hasActive = profiles.some((profile) => Boolean(profile.active));
    const cards = [
      ...(hasActive ? [] : [currentEndpointCard(current)]),
      ...profiles.map((profile) => profileCard(profile)),
    ];

    const addButton = h('button', {
      class: 'btn primary small', text: '+ 新增档案',
      title: '选服务商、填密钥，存成一套新的端点',
      onclick: () => openModelForm(null),
    });

    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', { text: '模型档案' }),
        addButton),
      h('div', { class: 'hint', text: profiles.length
        ? '点一张卡片就切过去，立刻生效、不需要重启；正在用的那张高亮并标着「正在用」，删不掉（先切走再删）。'
        : '还没有存过档案：下面这张就是她现在跑的配置，点「存成档案」把它存下来，以后就能一键切换。' }),
      h('div', { class: 'provider-list' }, ...cards));
  }

  /**
   * 「新增 / 编辑端点」弹窗。
   *
   * 走弹窗而不是页面里的一张常驻表单：这一页的常驻内容只有"她现在跑的是什么"和
   * "存过哪几套"，改字段是低频动作，不该一直占着一屏。
   *
   * `profile` 为空 = 新增：初值取**当前生效的配置**（所以打开就能看到现在这套长
   * 什么样，改两个字段就能存成新档案）；给了 profile = 编辑那一套，保存时带
   * `profile_id` 就地更新。
   */
  function openModelForm(profile) {
    const current = (model.data && model.data.current) || {};
    const source = profile || current;
    const presetId = guessProvider(source.url) || model.provider;
    const preset = MODEL_PROVIDERS.find((item) => item.id === presetId) || MODEL_PROVIDERS[0];

    const providerSelect = h('select', { class: 'select', id: 'model-provider' },
      ...MODEL_PROVIDERS.map((item) => h('option', {
        value: item.id, selected: item.id === presetId, text: item.label,
      })));
    const urlInput = h('input', {
      class: 'input mono', id: 'model-url', value: source.url || '',
      placeholder: 'https://api.example.com/v1',
    });
    const modelInput = h('input', {
      class: 'input mono', id: 'model-name', value: source.model_name || '',
      placeholder: '模型名，例如 deepseek-v4-flash', list: 'model-name-options',
    });
    const modelOptions = h('datalist', { id: 'model-name-options' },
      ...preset.models.map((name) => h('option', { value: name })));
    const keyInput = h('input', {
      class: 'input mono', id: 'model-key', type: 'password', autocomplete: 'new-password',
      // 密钥只进不出：编辑既有档案时留空就是"还用这把"，不是"清空"。
      placeholder: profile
        ? (profile.has_key ? '留空 = 沿用这套档案里的密钥' : '这套档案没有密钥；要用就粘一把进来')
        : (current.has_key ? '留空 = 沿用现在这把' : '粘贴 API Key（只写不读，后台不回显）'),
    });
    const labelInput = h('input', {
      class: 'input', id: 'model-save-as', value: profile ? profile.label : '',
      placeholder: '例如：DeepSeek 主力、中转站、本机 Ollama',
    });
    const visionToggle = h('input', { type: 'checkbox', checked: Boolean(source.supports_vision) });
    const authToggle = h('input', { type: 'checkbox', checked: source.requires_auth !== false });
    const thinkingSelect = h('select', { class: 'select' },
      ...[['disabled', '关闭（推荐：省下的预算留给正文）'], ['auto', '交给服务商默认']]
        .map(([value, text]) => h('option', { value, selected: source.thinking_mode === value, text })));
    const wireSelect = h('select', { class: 'select' },
      ...[['chat_completions', 'Chat Completions（绝大多数）'], ['responses', 'Responses（OpenAI 新协议）']]
        .map(([value, text]) => h('option', { value, selected: source.wire_api === value, text })));
    const tokensInput = h('input', {
      class: 'input mono', type: 'number', min: '128',
      // 档案里 0 表示"沿用当前配置"，别在编辑时把它变成具体的 1200：
      // 那样一存就把"不表态"改成了"就按 1200 来"。
      value: source.max_output_tokens ? String(source.max_output_tokens) : (profile ? '' : '1200'),
      placeholder: profile ? '留空 = 沿用当前配置' : '',
    });
    const result = h('div', { class: 'model-test', id: 'model-test' });
    model.testNode = result;

    // 选预设只填"这一家长什么样"，不碰密钥；模型名给候选，也可以手填。
    providerSelect.addEventListener('change', () => {
      model.provider = providerSelect.value;
      const picked = MODEL_PROVIDERS.find((item) => item.id === model.provider);
      if (!picked) return;
      if (picked.url) urlInput.value = picked.url;
      wireSelect.value = picked.wire_api;
      thinkingSelect.value = picked.thinking_mode;
      visionToggle.checked = Boolean(picked.supports_vision);
      authToggle.checked = picked.requires_auth !== false;
      clear(modelOptions);
      for (const name of picked.models) modelOptions.append(h('option', { value: name }));
      if (picked.models.length && !picked.models.includes(modelInput.value)) {
        modelInput.value = picked.models[0];
      }
    });

    const formBody = () => ({
      url: urlInput.value.trim(),
      model_name: modelInput.value.trim(),
      wire_api: wireSelect.value,
      thinking_mode: thinkingSelect.value,
      supports_vision: visionToggle.checked,
      requires_auth: authToggle.checked,
      max_output_tokens: Number(tokensInput.value) || 0,
      api_key: keyInput.value,
    });

    const testButton = h('button', {
      class: 'btn', text: '测试连接',
      title: '用表单里的值真发一次最小请求（不会保存）',
      onclick: async (event) => {
        const button = event.target;
        button.disabled = true;
        button.textContent = '测试中…';
        try {
          model.test = await api('/api/model/test', { method: 'POST', body: formBody() });
        } catch (problem) {
          model.test = { ok: false, detail: problem.message };
        }
        button.disabled = false;
        button.textContent = '测试连接';
        renderModelTestResult();
      },
    });

    const applyButton = h('button', {
      class: 'btn primary', text: profile ? '保存并应用' : '应用',
      title: '写入运行时覆盖配置并立刻生效（不需要重启）',
      onclick: async () => {
        const payload = formBody();
        const label = labelInput.value.trim();
        if (!payload.model_name) { toast('先填模型名', 'bad'); modelInput.focus(); return; }
        if (!label) { toast('给这套档案起个名字——列表里显示的就是它', 'bad'); labelInput.focus(); return; }
        // 编辑态必须带上 profile_id：它决定"就地更新"而不是"再存一套同名的新档案"。
        if (profile) payload.profile_id = profile.id;
        payload.save_as = label;
        try {
          const outcome = await api('/api/model/apply', { method: 'POST', body: payload });
          toast(profile ? `已更新「${label}」并生效`
            : (outcome.profile_id ? `已应用，并存成档案「${label}」` : '已应用，立刻生效'), 'ok');
          closeModal();
          await renderModelPage();
          await refreshHealth();
        } catch (problem) {
          toast(problem.message, 'bad', 7000);
        }
      },
    });

    const fields = h('div', { class: 'model-form' },
      h('label', { class: 'model-field wide' }, h('span', { text: '服务商' }), providerSelect),
      h('label', { class: 'model-field wide' },
        h('span', { text: '地址（base_url 或完整地址）' }), urlInput),
      h('label', { class: 'model-field wide' },
        h('span', { text: '模型名' }), modelInput, modelOptions),
      h('label', { class: 'model-field wide' }, h('span', { text: 'API Key' }), keyInput),
      h('label', { class: 'model-field wide' },
        h('span', { text: '档案名（列表里显示的名字）' }), labelInput),
      h('label', { class: 'model-field' }, h('span', { text: '单次最大输出 token' }), tokensInput),
      h('label', { class: 'model-check' }, visionToggle, ' 主模型能直接看图'),
      h('label', { class: 'model-check' }, authToggle, ' 需要 Bearer 鉴权'),
      h('details', { class: 'model-advanced' },
        h('summary', { text: '高级（协议 / 思考模式）' }),
        h('div', { class: 'model-form' },
          h('label', { class: 'model-field' }, h('span', { text: '协议' }), wireSelect),
          h('label', { class: 'model-field' }, h('span', { text: '思考模式' }), thinkingSelect))));

    const body = h('div', { class: 'model-form-body' }, fields, result);
    openModal(
      profile ? `编辑档案：${profile.label}` : '新增一套端点',
      body,
      [testButton, applyButton],
      profile ? '改完点「保存并应用」就地更新这一套；密钥留空表示不动'
        : '初值就是她现在跑的那套配置；填好名字，存下来以后就能一键切回');
    const first = $('#modal-body #model-url');
    if (first) first.focus({ preventScroll: true });
  }

  function renderModelTestResult() {
    const node = model.testNode;
    if (!node) return;
    clear(node);
    const outcome = model.test;
    if (!outcome) return;
    node.className = `model-test ${outcome.ok ? 'ok' : 'bad'}`;
    node.append(h('span', { class: 'model-test-badge', text: outcome.ok ? '通了' : '没通' }));
    node.append(h('span', {
      text: `${outcome.detail || ''}${outcome.latency_ms ? `（${outcome.latency_ms} ms）` : ''}`,
    }));
  }

  // ───────────────────────────── 表情包 ─────────────────────────────
  //
  // 她"发得出去"的那些图。素材只有一个来源：`qq_sticker.dir` 那个目录——这里
  // 上传/删除的就是她发送时读的文件，所以列表里看到的必然就是能发的，不存在
  // "后台里有、磁盘上没有"。文件名（去掉扩展名）就是标签；同名不覆盖，服务端
  // 自动加编号（`开心.png` 已存在就写 `开心-2.png`，仍然归到「开心」下）。

  async function renderStickerPage() {
    const page = $('#page-stickers');
    clear(page);
    page.append(h('div', { class: 'loading', text: '读取表情包素材库…' }));
    let data;
    try {
      data = await api('/api/stickers');
    } catch (problem) {
      clear(page);
      page.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    if (currentPage !== 'stickers') return;

    clear(page);
    page.append(renderStickerStatus(data));
    page.append(renderStickerUpload(data));
    page.append(renderStickerGrid(data));
  }

  function renderStickerStatus(data) {
    const pill = h('span', {
      class: `pill ${data.enabled ? 'ok' : 'bad'}`,
      text: data.enabled ? '已启用' : '未启用',
    });
    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', {}, '素材库 ', pill),
        h('div', { class: 'hint', text: `${data.file_count} 张 · ${data.label_count} 个标签 / 上限 ${data.max_files} 张` })),
      h('div', { class: 'sticker-facts' },
        stickerFact('目录', data.dir),
        stickerFact('可写', data.writable ? '是' : '否（后台传不上去，检查权限或只读挂载）'),
        stickerFact('单张上限', `${data.max_file_kb} KB`),
        stickerFact('重扫间隔', `${data.rescan_secs} 秒`)),
      !data.enabled
        ? h('div', { class: 'hint' }, '配置里 qq_sticker.enabled = false：素材可以先传好，但她现在不会发。改配置页的 [qq_sticker] 打开即可。')
        : null,
      data.enabled && data.file_count === 0
        ? h('div', { class: 'hint' }, '还没有素材——传一张上去，然后可以在 QQ 里发 #表情列表 / #发表情 标签 直接验收。')
        : null);
  }

  function stickerFact(label, value) {
    // 目录路径同理：截断显示，悬停补全。
    return h('div', { class: 'sticker-fact', title: `${label}：${value}` },
      h('span', { class: 'sticker-fact-label', text: label }),
      h('span', { class: 'sticker-fact-value mono', text: value }));
  }

  function renderStickerUpload(data) {
    const labelInput = h('input', {
      class: 'input',
      id: 'sticker-label',
      placeholder: '标签，例如：无语又想笑（文件名就是它）',
      maxlength: '64',
    });
    const fileInput = h('input', {
      class: 'input',
      id: 'sticker-files',
      type: 'file',
      accept: 'image/png,image/jpeg,image/gif,image/webp,image/bmp',
      multiple: true,
    });
    const submit = h('button', {
      class: 'btn primary',
      text: '上传',
      onclick: async () => {
        const label = labelInput.value.trim();
        const files = Array.from(fileInput.files || []);
        if (!label) { toast('先填一个标签', 'bad'); labelInput.focus(); return; }
        if (!files.length) { toast('先选至少一张图片', 'bad'); return; }
        submit.disabled = true;
        const original = submit.textContent;
        let done = 0;
        for (const file of files) {
          submit.textContent = `上传中 ${done + 1}/${files.length}…`;
          try {
            await api(`/api/stickers?label=${encodeURIComponent(label)}`, {
              method: 'POST',
              raw: file,
              headers: { 'Content-Type': file.type || 'application/octet-stream' },
            });
            done += 1;
          } catch (problem) {
            toast(`${file.name}：${problem.message}`, 'bad', 7000);
          }
        }
        submit.disabled = false;
        submit.textContent = original;
        if (done) toast(`已上传 ${done} 张`, 'ok');
        fileInput.value = '';
        await renderStickerPage();
      },
    });

    // 拖进来就传：上传是这一页最高频的动作，不值当先点开文件选择器。
    const drop = h('div', {
      class: 'sticker-drop',
      ondragover: (event) => { event.preventDefault(); drop.classList.add('over'); },
      ondragleave: () => drop.classList.remove('over'),
      ondrop: (event) => {
        event.preventDefault();
        drop.classList.remove('over');
        if (!event.dataTransfer || !event.dataTransfer.files.length) return;
        fileInput.files = event.dataTransfer.files;
        toast(`已选中 ${event.dataTransfer.files.length} 个文件，填好标签后点上传`, 'ok');
      },
    }, '把图片拖到这里，或在下面选择文件');

    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', { text: '上传素材' }),
        h('div', { class: 'hint', text: '同名不覆盖：会自动加编号，并归到同一个标签下' })),
      h('div', { class: 'sticker-form' },
        labelInput,
        fileInput,
        submit),
      drop);
  }

  function renderStickerGrid(data) {
    const files = data.files || [];
    if (!files.length) {
      return h('div', { class: 'card tight' },
        h('div', { class: 'hint', text: '素材库现在是空的。' }));
    }
    return h('div', { class: 'card' },
      h('div', { class: 'card-head' },
        h('h3', { text: '现有素材' }),
        h('div', { class: 'hint', text: '点缩略图看原图；删除会立刻生效（她下一轮就发不出这张）' })),
      h('div', { class: 'sticker-grid' },
        ...files.map((file) => renderStickerTile(file))));
  }

  function renderStickerTile(file) {
    const url = `/api/stickers/file/${encodeURIComponent(file.name)}`;
    return h('div', { class: 'sticker-tile' },
      h('a', { class: 'sticker-thumb', href: url, target: '_blank', rel: 'noopener', title: '在新标签打开原图' },
        h('img', { src: url, alt: file.label, loading: 'lazy' })),
      h('div', { class: 'sticker-meta' },
        h('div', { class: 'sticker-label', text: file.label, title: file.name }),
        h('div', { class: 'sticker-sub', text: `${file.name} · ${fmtBytes(file.bytes)}` })),
      h('button', {
        class: 'btn danger small',
        text: '删除',
        onclick: async () => {
          if (!window.confirm(`删除「${file.label}」（${file.name}）？`)) return;
          try {
            await api(`/api/stickers/file/${encodeURIComponent(file.name)}`, { method: 'DELETE' });
            toast('已删除', 'ok');
          } catch (problem) {
            toast(problem.message, 'bad');
          }
          await renderStickerPage();
        },
      }));
  }

  // ───────────────────────────── 标注 ─────────────────────────────
  //
  // TurnGate 复核闭环的网页端（doc §7.4 B）：左边是"标注价值"队列，右边是单条
  // 详情与标签按钮，标完直接导出训练集。与终端里的 tools/turngate/review.py
  // 读写同一份 JSONL、同一套语义（tier / 排序理由 / human_consensus 标注块 /
  // 导出剥掉 source_key），两个入口可以换着用——但同一时刻只用一个：文件被别处
  // 改动时服务端返回 409，页面会要求刷新后重来，而不是把对方的改动盖掉。

  /** 标注页状态。goto() 每次进页都整页重建，靠它挂住批次与当前样本。 */
  const annotation = {
    dir: '',
    exists: true,
    writable: true,
    batch: '',
    revision: '',
    summary: null,
    coverage: null,
    /** 队列的 tier 分布（下标= tier，含每档含义），由接口下发。 */
    tiers: [],
    items: [],
    matched: 0,
    limit: 40,
    tab: 'pending',
    skipFlagged: true,
    current: null,
    draft: { completion: '', response: '' },
    exports: [],
  };

  const COMPLETION_CHOICES = [['flush_now', '说完了'], ['hold_for_more', '还没说完']];
  const RESPONSE_CHOICES = [
    ['answer', '回答'], ['continue', '接续'], ['ack', '应一声'],
    ['ignore', '不发言'], ['wait', '再等等'],
  ];
  /** 键盘：左手选标签（'null' = 判不了），右手回车保存、S 跳过。 */
  const ANNOTATION_KEYS = {
    1: ['completion', 'flush_now'],
    2: ['completion', 'hold_for_more'],
    3: ['completion', 'null'],
    a: ['response', 'answer'],
    c: ['response', 'continue'],
    k: ['response', 'ack'],
    i: ['response', 'ignore'],
    w: ['response', 'wait'],
    0: ['response', 'null'],
  };
  /** 群友编号用的字母表（下标 = collector 落的编号：0=a，1=b…）。 */
  const SPEAKER_LETTERS = 'ABCDEFGH';
  /** 群友编号的显示名前缀。字段名仍叫 speaker（数据契约），界面上一律叫"群友"——
   *  这些回合本来就是同一个群里的人，叫"说话人"既绕口又不像人话。 */
  const SPEAKER_LABEL = '群友';
  /** 其他群友的配色档数，与 CSS 的 --speaker-0..3 一一对应（多了就轮转）。 */
  const SPEAKER_TONES = 4;
  /**
   * 老批次没有群友编号时的退路：只能按"是不是正文那个发言者"分两拨。
   *
   * 这里刻意不写"同一人"：正文框也曾不带标签，于是"同一人"没有前指——标注的人
   * 会问"跟谁同一人？"。改成"发言者"之后，正文框、前一句、他自己更早的回合用的
   * 是同一个词，同一个人只有一个名字；别人一律"他人"。
   */
  const ROLE_LABELS = { assistant: '芸汐', user: '发言者', other_member: '他人' };
  const SNIPPET_CHARS = 56;

  /**
   * 上下文里一个回合的显示名与配色。
   *
   * collector 从 2026-09-13 起给 recent_turns 落样本内匿名的 `speaker`
   * （a = 当前发言者，b/c/… 按首次出现顺序给其他群友）。没有这个字段时，几个
   * 其他人都显示成"他人"——标注时看不出是"一个人连说三条"还是"三个人在互相
   * 接话"，而这正是判断 completion/response 的关键线索。
   *
   * 返回 `{ label, tone }`，`tone` 直接当 `annotate-role` 的类后缀用。
   */
  function annotationSpeaker(turn) {
    if (turn && turn.role === 'assistant') {
      return { label: ROLE_LABELS.assistant, tone: 'assistant' };
    }
    const id = turn && typeof turn.speaker === 'string' ? turn.speaker.toLowerCase() : '';
    const index = id.length === 1 ? id.charCodeAt(0) - 'a'.charCodeAt(0) : -1;
    if (index >= 0 && index < SPEAKER_LETTERS.length) {
      return {
        label: `${SPEAKER_LABEL}${SPEAKER_LETTERS[index]}`,
        // 当前发言者（A）沿用原来的绿色，其他人按 --speaker-0..3 轮转。
        tone: index === 0 ? 'user' : `speaker-${(index - 1) % SPEAKER_TONES}`,
      };
    }
    return { label: ROLE_LABELS[turn && turn.role] || (turn && turn.role) || '?', tone: 'other' };
  }

  /** 这条样本有没有群友编号——没有就只能是"他人"，得让标注的人知道。 */
  function annotationHasSpeakers(sample) {
    return ((sample && sample.context && sample.context.recent_turns) || [])
      .some((turn) => turn && turn.speaker);
  }

  /**
   * 正文框（以及同属一次发言的"前一句"）该用哪个名字。
   *
   * 新批次有群友编号，这里就是"群友A"（正文与他自己更早的回合共用一个字母）。
   * 老批次编不出号，退回"发言者"——必须和上下文里 `user` 回合用的词一模一样，
   * 否则同一个人会有两个名字（正文框一个、他自己那条上下文另一个），看着像两个人。
   * 压根没有上下文时照写"群友A"：没有东西跟它打架。
   */
  function annotationCurrentLabel(sample) {
    const turns = (sample && sample.context && sample.context.recent_turns) || [];
    if (annotationHasSpeakers(sample)) return `${SPEAKER_LABEL}A`;
    return turns.length ? ROLE_LABELS.user : `${SPEAKER_LABEL}A`;
  }

  /** 老批次里"他人"的悬停说明：不是没显示，是当时根本没记。 */
  const SPEAKER_UNRECORDED = '这批采于群友编号之前：只分得出"发言者"和"其他人"，'
    + '看不出几条"他人"是不是同一个人';

  function annotationSnippet(text) {
    const flat = String(text == null ? '' : text).split(/\s+/).filter(Boolean).join(' ');
    return flat.length > SNIPPET_CHARS ? `${flat.slice(0, SNIPPET_CHARS)}…` : flat;
  }

  function annotationQuery(extra) {
    // 「已标注」页签要看的是**只看已标**：`reviewed_only`。只发 include_reviewed
    // 是不够的——那个参数的意思是"连已标的一起列"，队列会变成整批样本（实测
    // 4302 条里混着 4289 条待标的，页签名字完全对不上）。
    const reviewedTab = annotation.tab === 'reviewed';
    const params = new URLSearchParams({
      batch: annotation.batch,
      limit: String(annotation.limit),
      include_reviewed: String(reviewedTab),
      reviewed_only: String(reviewedTab),
      skip_flagged: String(annotation.skipFlagged),
      ...extra,
    });
    return `/api/annotation/queue?${params.toString()}`;
  }

  function currentAnnotationIndex() {
    return annotation.current ? annotation.current.index : -1;
  }

  async function renderAnnotationPage() {
    const page = $('#page-annotation');
    clear(page);
    page.append(h('div', { class: 'loading', text: '读取标注目录…' }));
    let data;
    try {
      data = await api('/api/annotation/batches');
    } catch (problem) {
      clear(page);
      page.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    if (currentPage !== 'annotation') return;

    annotation.dir = data.dir || '';
    annotation.exists = Boolean(data.exists);
    annotation.writable = Boolean(data.writable);
    annotation.exports = data.exports || [];
    const batches = data.batches || [];
    if (!batches.some((batch) => batch.name === annotation.batch)) {
      // 批次没了或被换掉：当前样本与草稿都失效，避免把标签写到别的批次的同一序号上。
      annotation.batch = batches.length ? batches[0].name : '';
      annotation.current = null;
      annotation.draft = { completion: '', response: '' };
    }

    clear(page);
    page.append(renderAnnotationToolbar(batches));
    if (!annotation.exists) {
      // 后台启动时会自动建这个目录；走到这里说明建不出来（只读路径、权限）。
      page.append(h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: '标注目录建不出来' })),
        h('div', { class: 'hint', text: data.error
          ? `后台会按 admin.annotation_dir 自动创建它，这次失败了：${data.error}`
          : '后台会按 admin.annotation_dir 自动创建它，但它现在不在。' }),
        h('div', { class: 'hint', text: '批次由 collector.py 从 journalctl 导出（doc §7.4 B），要落在机器人所在机器上：' }),
        h('pre', { class: 'annotate-cmd', text: `mkdir -p ${annotation.dir}\nscp review-batch-*.jsonl <服务器>:${annotation.dir}/` })));
      renderAnnotationSummary();
      return;
    }
    if (!batches.length) {
      page.append(h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: '这个目录里还没有批次' })),
        h('div', { class: 'hint', text: '把 collector.py 采出来的批次放进来（页面只读这一层目录）：' }),
        h('pre', { class: 'annotate-cmd', text: `scp review-batch-*.jsonl <服务器>:${annotation.dir}/` })));
      renderAnnotationSummary();
      return;
    }
    // 队列外面套一张卡：宽屏下它的高度要跟右边那条样本齐平，没有可见的容器
    // 就"对齐"不出任何东西（两张透明列表各撑各的）。
    page.append(h('div', { class: 'annotate-grid' },
      h('div', { class: 'card annotate-queue' },
        h('div', { class: 'record-list', id: 'annotate-list' })),
      h('div', { class: 'detail', id: 'annotate-detail' })));
    await loadAnnotationQueue();
  }

  function renderAnnotationToolbar(batches) {
    const select = h('select', { class: 'select', id: 'annotate-batch' },
      ...batches.map((batch) => h('option', {
        value: batch.name,
        selected: batch.name === annotation.batch,
        text: `${batch.name}（${batch.summary && !batch.summary.error ? `${batch.summary.pending} 待标` : '读不动'}）`,
      })));
    select.addEventListener('change', async () => {
      annotation.batch = select.value;
      annotation.current = null;
      annotation.draft = { completion: '', response: '' };
      await loadAnnotationQueue();
    });

    const toggle = h('input', {
      type: 'checkbox',
      id: 'annotate-skip-flagged',
      checked: annotation.skipFlagged,
    });
    toggle.addEventListener('change', async () => {
      annotation.skipFlagged = toggle.checked;
      await loadAnnotationQueue();
    });

    const exportButton = h('button', {
      class: 'btn primary',
      text: '导出训练集',
      title: '只收人工复核过、agreement 达标的样本，并剥掉 review_status 与 source_key',
      onclick: (event) => exportAnnotation(event.target),
    });
    const refreshButton = h('button', {
      class: 'btn ghost',
      text: '刷新',
      onclick: () => renderAnnotationPage(),
    });

    // 统计、覆盖率、tier 分布、过往导出：都是"想不起来才看一眼"的东西，
    // 但加起来有一屏高。窄屏上它们会把真正要标的样本顶到首屏之外，所以先收进
    // 一个默认折叠的 details；桌面宽度照旧摊开。
    const secondary = [
      h('div', { class: 'stat-grid annotate-stats', id: 'annotate-stats' }),
      h('div', { class: 'annotate-hint', id: 'annotate-hint' }),
      h('div', { class: 'annotate-hint', id: 'annotate-tiers' }),
      h('div', { class: 'annotate-exports', id: 'annotate-exports' }),
    ];

    return h('div', { class: 'card annotate-toolbar' },
      h('div', { class: 'annotate-bar' },
        h('span', { class: 'annotate-bar-label', text: '批次' }),
        select,
        h('div', { class: 'tabs annotate-tabs' },
          annotationTabButton('pending', '待标注'),
          annotationTabButton('reviewed', '已标注')),
        h('label', {
          class: 'annotate-toggle',
          title: '这批采于"@ 判定"修复之前：@ 别人也曾被记成在叫她，目标其实分不出来',
        }, toggle, '跳过目标可疑的样本'),
        h('span', { class: 'annotate-spacer' }),
        exportButton,
        refreshButton),
      narrowLayout()
        ? h('details', { class: 'annotate-folds' },
          h('summary', {},
            h('span', { text: '本批统计与队列分布' }),
            // 折起来也留一句"还剩多少要标"：这是标的时候唯一需要随时看到的数。
            h('span', { class: 'muted small', id: 'annotate-fold-note' })),
          ...secondary)
        : secondary);
  }

  function annotationTabButton(key, label) {
    return h('button', {
      class: annotation.tab === key ? 'active' : '',
      text: label,
      onclick: async () => {
        if (annotation.tab === key) return;
        annotation.tab = key;
        annotation.current = null;
        annotation.draft = { completion: '', response: '' };
        await renderAnnotationPage();
      },
    });
  }

  /** 进度与队列覆盖率（口径与 review.py --queue 的表头一致）。 */
  function renderAnnotationSummary() {
    const summary = annotation.summary || {};
    const stats = $('#annotate-stats');
    if (stats) {
      clear(stats);
      stats.append(
        stat('这批样本', compactNumber(summary.total || 0), null, true),
        stat('已标注', compactNumber(summary.reviewed || 0), null, true),
        stat('待标注', compactNumber(summary.pending || 0), null, true),
        stat('目标判定可疑', compactNumber(summary.flagged || 0), '默认不进队列', true));
    }
    const hint = $('#annotate-hint');
    if (hint) {
      const coverage = annotation.coverage || {};
      clear(hint);
      // `append` 会把 null 变成 "null" 文本节点（h() 才会跳过空子节点），所以先过滤。
      hint.append(...[
        h('span', { class: 'mono', text: annotation.dir }),
        h('span', { text: `队列 ${annotation.matched} 条` }),
        h('span', { text: `灰区 ${coverage.gray_zone || 0}` }),
        h('span', { text: `有上下文 ${coverage.with_recent_turns || 0}` }),
        h('span', { text: `有芸汐发言 ${coverage.with_bot_turn || 0}` }),
        annotation.writable ? null : h('span', { class: 'badge restart', text: '目录不可写' }),
      ].filter(Boolean));
    }
    renderAnnotationTiers();
    renderAnnotationExports();
    const foldNote = $('#annotate-fold-note');
    if (foldNote) {
      // 窄屏上这块是折起来的：把"还剩多少要标"留在标题行，标的时候不用展开。
      foldNote.textContent = `待标 ${compactNumber(summary.pending || 0)} / 共 ${compactNumber(summary.total || 0)}`;
    }
  }

  /** 队列的 tier 分布 + 每档含义。
   *
   * 队列按标注价值升序排，所以首屏必然全是 tier 0——不写清楚含义，这个 badge
   * 看起来就像"全是低等级"。含义文案由接口下发（annotation_api.rs 的
   * TIER_LABELS），这里只负责渲染，免得两边口径漂移。 */
  function renderAnnotationTiers() {
    const host = $('#annotate-tiers');
    if (!host) return;
    clear(host);
    const tiers = annotation.tiers || [];
    if (!tiers.length) return;
    const spans = tiers.map((row) => h('span', {
      class: row.tier === 0 ? 'facet-chip tag' : 'facet-chip',
      title: `tier ${row.tier}：${row.label}（越小越先标）`,
      // 精确条数，不套 compactNumber：这一行是"还剩多少要标"，得能跟上面的
      // 覆盖率数字（灰区 1223）对得上，1.0K 这种约数在这里只会碍事。
      text: `${row.tier} ${row.label} ${row.count}`,
    }));
    host.append(
      h('span', { text: '按标注价值排队，tier 越小越先标：' }),
      ...spans);
  }

  function renderAnnotationExports() {
    const host = $('#annotate-exports');
    if (!host) return;
    clear(host);
    if (!annotation.exports.length) {
      host.append(h('span', { class: 'muted small', text: '还没有导出过训练集。' }));
      return;
    }
    host.append(h('span', { class: 'muted small', text: '已导出：' }));
    for (const item of annotation.exports.slice(0, 5)) {
      host.append(h('a', {
        class: 'facet-chip tag',
        href: `/api/annotation/download?name=${encodeURIComponent(item.name)}`,
        text: item.name,
        title: `${fmtBytes(item.bytes)} · ${fmtTime(item.modified)}`,
      }));
    }
  }

  async function loadAnnotationQueue() {
    const list = $('#annotate-list');
    if (!list) return;
    clear(list);
    list.append(h('div', { class: 'loading', text: '读队列…' }));
    let data;
    try {
      data = await api(annotationQuery());
    } catch (problem) {
      clear(list);
      list.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    if (currentPage !== 'annotation') return;

    annotation.revision = data.revision;
    annotation.summary = data.summary;
    annotation.coverage = data.coverage;
    annotation.tiers = data.tiers || [];
    annotation.items = data.items || [];
    annotation.matched = data.matched || 0;
    renderAnnotationSummary();

    const selected = currentAnnotationIndex();
    if (!annotation.items.some((item) => item.index === selected)) {
      // 队列换了（切批次/切页签/刚标完），当前样本已不在列表里就重新挑第一条。
      annotation.current = null;
      annotation.draft = { completion: '', response: '' };
      const first = annotation.items[0];
      if (first) {
        await openAnnotationSample(first.index);
        return;
      }
    }
    renderAnnotationList();
    renderAnnotationDetail();
  }

  function renderAnnotationList() {
    const list = $('#annotate-list');
    if (!list) return;
    clear(list);
    if (!annotation.items.length) {
      list.append(h('div', {
        class: 'empty',
        text: annotation.tab === 'reviewed' ? '这一批还没有已标注的样本' : '这个队列空了，切到「已标注」看看，或换一批',
      }));
      return;
    }
    const selected = currentAnnotationIndex();
    for (const item of annotation.items) {
      list.append(h('div', {
        class: `record${item.index === selected ? ' active' : ''}`,
        onclick: () => openAnnotationSample(item.index, true),
      },
        h('div', { class: 'record-head' },
          h('span', { class: 'record-title', text: `#${item.index}` }),
          h('span', { class: 'badge kind', text: `tier ${item.tier}` }),
          item.review_status === 'reviewed' ? h('span', { class: 'badge', text: '已标' }) : null,
          item.flagged ? h('span', { class: 'badge secret', text: '目标可疑' }) : null),
        h('div', { class: 'record-body', text: annotationSnippet(item.current_text) }),
        h('div', { class: 'record-foot' },
          h('span', { text: item.reason }),
          h('span', { text: `上下文 ${item.richness}` }),
          h('span', { text: item.scope === 'group' ? '群聊' : '私聊' }))));
    }
    // 跳过不会把样本移出队列，所以标完之前队列长度不变——得能给"再看下一屏"。
    const remaining = annotation.matched - annotation.items.length;
    if (remaining > 0) {
      list.append(h('button', {
        class: 'btn ghost small annotate-more',
        text: `再加载 ${annotation.limit} 条（还有 ${remaining} 条）`,
        onclick: async () => {
          annotation.limit = Math.min(annotation.limit + 40, 500);
          await loadAnnotationQueue();
        },
      }));
    }
  }

  /**
   * 打开一条样本。
   *
   * `scrollToDetail` 只在"人主动点了队列里的某条"时为真：窄屏上队列排在详情
   * 下面，点完得把详情滚回视野里，否则像是没反应；而首屏自动打开第一条时不能
   * 滚——那会一进来就跳过整块工具栏。
   */
  async function openAnnotationSample(index, scrollToDetail = false) {
    const host = $('#annotate-detail');
    if (!host) return;
    annotation.draft = { completion: '', response: '' };
    clear(host);
    host.append(h('div', { class: 'loading', text: `打开 #${index}…` }));
    let data;
    try {
      data = await api(`/api/annotation/sample?batch=${encodeURIComponent(annotation.batch)}&index=${index}`);
    } catch (problem) {
      clear(host);
      host.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    if (currentPage !== 'annotation') return;
    annotation.current = data;
    annotation.revision = data.revision;
    annotation.draft = annotationDraftFor(data);
    renderAnnotationList();
    renderAnnotationDetail();
    if (scrollToDetail && narrowLayout()) {
      // 内容区自己滚动（.page-scroll），不是文档滚动，所以只能滚那个容器；
      // 偏移量按"容器顶到详情卡顶"算，工具栏高度随标题换行变化也不用写死像素。
      const scroller = host.closest('.page-scroll');
      if (scroller) {
        const top = host.getBoundingClientRect().top
          - scroller.getBoundingClientRect().top + scroller.scrollTop;
        scroller.scrollTo({ top: Math.max(0, top - 10), behavior: 'smooth' });
      }
    }
  }

  /**
   * 打开一条样本时，标签按钮该预选什么。
   *
   * **已标注的回显人工值**：打开一条标过的样本却四个按钮全空，既看不出自己标过
   * 什么，也没法在原值上改。已标注样本的 `labels` 里存的就是人工标签（标记时
   * 覆盖了采集器的弱标签），直接拿来预选。
   *
   * **未标注的不预选**：那里 `labels` 是弱标签（机器猜的），先替人选上就是把猜测
   * 当成人的判断——而"保存"会把它当人工标签写下去，复核就白做了。弱标签仍然以
   * 文字显示在按钮上方，供人参考而不是替人决定。
   *
   * 注意一个数据模型上的老问题：只标了一个 head 的样本，另一个 head 里留着的还是
   * 采集器的弱标签，而 `label_provenance` 整条已变成 human_consensus——从数据上
   * 分不出哪一侧是人选的，所以那一侧也会显示成"已选中"。要区分得改数据模型，
   * 不在这一层解决。
   */
  function annotationDraftFor(current) {
    const empty = { completion: '', response: '' };
    if (!current || !current.reviewed) return empty;
    const labels = (current.sample && current.sample.labels) || {};
    // 存进去的 JSON `null` 表示"判不了"，界面上对应按钮的值是字符串 'null'。
    const pick = (value) => {
      if (value === null) return 'null';
      return typeof value === 'string' ? value : '';
    };
    return { completion: pick(labels.completion), response: pick(labels.response) };
  }

  function annotationChoice(head, value, label) {
    const active = annotation.draft[head] === value;
    const key = Object.keys(ANNOTATION_KEYS).find(
      (candidate) => ANNOTATION_KEYS[candidate][0] === head && ANNOTATION_KEYS[candidate][1] === value);
    return h('button', {
      class: `label-choice${active ? ' active' : ''}`,
      title: `${head} = ${value}`,
      onclick: () => {
        annotation.draft[head] = active ? '' : value;
        renderAnnotationDetail();
      },
    }, label, key ? h('span', { class: 'annotate-key', text: key }) : null);
  }

  function renderAnnotationDetail() {
    const host = $('#annotate-detail');
    if (!host) return;
    clear(host);
    const current = annotation.current;
    if (!current) {
      host.append(h('div', { class: 'empty', text: '从左边挑一条开始标。' }));
      return;
    }
    const sample = current.sample || {};
    const context = sample.context || {};
    const labels = sample.labels || {};
    const provenance = sample.label_provenance || {};

    // 当前发言者的标签：老批次上下文编不出号时整条样本都不写成员名，
    // 免得同一个人出现"群友A"和"同一人"两个名字（见 annotationCurrentLabel）。
    const currentLabel = annotationCurrentLabel(sample);
    const body = h('div', { class: 'annotate-body' });
    for (const fragment of context.pending_user_fragments || []) {
      // 片段与正文同属一次发言（collector 按"同群同发送者间隔 ≤3s"切分），
      // 所以它们必然是当前发言者——与正文框共用一个标签，不必各自带字段。
      body.append(h('div', { class: 'annotate-turn' },
        h('span', { class: 'annotate-role user', text: currentLabel || '前一句' }),
        h('span', { class: 'annotate-text' },
          currentLabel ? h('span', { class: 'annotate-tag', text: '前一句' }) : null,
          String(fragment))));
    }
    for (const turn of context.recent_turns || []) {
      const speaker = annotationSpeaker(turn);
      body.append(h('div', { class: 'annotate-turn' },
        h('span', {
          class: `annotate-role ${speaker.tone}`,
          text: speaker.label,
          title: turn.speaker
            ? '样本内的匿名编号：同一个人每次都是同一个字母'
            : SPEAKER_UNRECORDED,
        }),
        h('span', { class: 'annotate-text', text: String(turn.text == null ? '' : turn.text) })));
    }
    if (context.bot_last_asked_question) {
      body.append(h('div', { class: 'annotate-turn' },
        h('span', { class: 'annotate-role assistant', text: '她在等回答' }),
        h('span', { class: 'annotate-text', text: String(context.bot_last_asked_question) })));
    }

    host.append(h('div', { class: 'card annotate-card' },
      h('div', { class: 'card-head' },
        h('h3', { text: `#${current.index}` }),
        h('span', { class: 'badge kind', text: `tier ${current.tier}` }),
        h('span', { class: 'badge', text: current.reason }),
        h('span', { class: 'badge', text: context.scope === 'group' ? '群聊' : '私聊' }),
        current.reviewed ? h('span', { class: 'badge default', text: '已标注' }) : null),
      current.flagged
        ? h('div', { class: 'annotate-warn' },
          '这批采于"@ 判定"修复之前：@ 别人也曾被记成在叫她。这条默认不进队列，标它等于把旧判定确认一遍。')
        : null,
      h('div', { class: 'annotate-current' },
        currentLabel ? h('span', { class: 'annotate-role user', text: currentLabel }) : null,
        h('span', {
          class: 'annotate-current-text',
          text: String(sample.current_text == null ? '' : sample.current_text),
        })),
      body.childNodes.length ? h('div', { class: 'annotate-context' }, body) : null,
      h('div', { class: 'annotate-meta' },
        // 已标注样本里 `labels` 存的是**人工**标签（弱标签在标记时被覆盖了），
        // 再叫"弱标签"会让人以为那是机器的猜测。
        h('span', { text: `${current.reviewed ? '已标注' : '弱标签'}：completion=${labels.completion == null ? '未给' : labels.completion}` }),
        h('span', { text: `response=${labels.response == null ? '未给' : labels.response}` }),
        h('span', { text: `来源 ${provenance.source || '未知'}` }),
        // 老批次的"他人"分不出是几个人：明说，免得以为界面漏了编号。
        annotationHasSpeakers(sample) || !(context.recent_turns || []).length
          ? null
          : h('span', { class: 'annotate-note', text: '群友未记录（这批采于编号之前）' })),
      h('div', { class: 'annotate-labels' },
        h('span', { class: 'annotate-label-head', text: 'completion' }),
        ...COMPLETION_CHOICES.map(([value, label]) => annotationChoice('completion', value, label)),
        annotationChoice('completion', 'null', '判不了')),
      h('div', { class: 'annotate-labels' },
        h('span', { class: 'annotate-label-head', text: 'response' }),
        ...RESPONSE_CHOICES.map(([value, label]) => annotationChoice('response', value, label)),
        annotationChoice('response', 'null', '判不了')),
      h('div', { class: 'annotate-actions' },
        h('button', { class: 'btn primary', text: '保存并下一条（Enter）', onclick: () => saveAnnotation() }),
        h('button', { class: 'btn ghost', text: '跳过（S）', onclick: () => skipAnnotation() }),
        h('span', { class: 'muted small', text: '标不了就跳过——硬标的噪声会进权重。' }))));
  }

  /** 保存当前草稿。只提交点过的 head，与 review.py --mark 的语义一致。 */
  async function saveAnnotation() {
    const current = annotation.current;
    if (!current) return;
    const draft = annotation.draft;
    if (!draft.completion && !draft.response) {
      toast('先选 completion 或 response；两个都判不了就点「判不了」', 'warn');
      return;
    }
    const body = { batch: annotation.batch, index: current.index, revision: annotation.revision };
    if (draft.completion === 'null') body.clear_completion = true;
    else if (draft.completion) body.completion = draft.completion;
    if (draft.response === 'null') body.clear_response = true;
    else if (draft.response) body.response = draft.response;

    let result;
    try {
      result = await api('/api/annotation/mark', { method: 'POST', body });
    } catch (problem) {
      toast(problem.message, 'bad', 9000);
      return;
    }
    annotation.revision = result.revision;
    annotation.summary = result.summary;
    const chosen = [draft.completion, draft.response].filter(Boolean).length;
    toast(`#${current.index} 已标注${chosen === 1 ? '（另一个 head 保留原值）' : ''}`, 'ok');
    await advanceAnnotation();
  }

  /** 队列里的下一条：**按队列顺序**，不是按样本编号。
   *
   * 队列由后端按 `(tier, Reverse(richness), index)` 排好（标注 API 的
   * `load_queue`），也就是说"下一条"应当是列表里的下一个，而不是编号更大的那个。
   * 以前用 `find(item => item.index > marked)`，遇到编号更大的样本排在前面时会
   * **往回跳**——标完 #37 跳到 #12，而列表里 #37 的下面明明是 #5。
   */
  function nextAnnotationSample(markedIndex) {
    const items = annotation.items;
    const position = items.findIndex((item) => item.index === markedIndex);
    if (position < 0) return items[0];
    return items[position + 1] || items[0];
  }

  /** 标完一条之后：本地摘掉它并打开下一条，队列空了就重新拉一页。 */
  async function advanceAnnotation() {
    renderAnnotationSummary();
    const marked = currentAnnotationIndex();
    // 先算出下一条再摘掉当前这条：`nextAnnotationSample` 依赖列表顺序，而摘除
    // 之后位置会整体前移。
    const next = nextAnnotationSample(marked);
    annotation.items = annotation.items.filter((item) => item.index !== marked);
    if (annotation.tab === 'pending') annotation.matched = Math.max(0, annotation.matched - 1);
    annotation.current = null;
    if (next && next.index !== marked) {
      await openAnnotationSample(next.index);
      return;
    }
    renderAnnotationList();
    renderAnnotationDetail();
    await loadAnnotationQueue();
  }

  async function skipAnnotation() {
    const current = currentAnnotationIndex();
    if (current < 0) return;
    const next = nextAnnotationSample(current);
    if (!next || next.index === current) {
      toast('队列里没有别的样本了', 'warn');
      return;
    }
    await openAnnotationSample(next.index);
  }

  async function exportAnnotation(button) {
    if (button) button.disabled = true;
    try {
      const result = await api('/api/annotation/export', {
        method: 'POST',
        body: { batch: annotation.batch },
      });
      toast(`导出 ${result.exported} 条（人工复核 ${result.human_reviewed} 条）→ ${result.file}`, 'ok', 7000);
      await refreshAnnotationExports();
    } catch (problem) {
      toast(problem.message, 'bad', 9000);
    } finally {
      if (button) button.disabled = false;
    }
  }

  async function refreshAnnotationExports() {
    let data;
    try {
      data = await api('/api/annotation/batches');
    } catch (problem) {
      toast(problem.message, 'bad');
      return;
    }
    annotation.exports = data.exports || [];
    renderAnnotationExports();
  }

  // 键盘只在标注页生效，且不抢输入框（批次下拉、导出链接都还要用键盘操作）。
  document.addEventListener('keydown', (event) => {
    if (currentPage !== 'annotation') return;
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const tag = event.target && event.target.tagName;
    // BUTTON 也要排除：焦点在「跳过」「导出」「刷新」上按回车，用户想要的是那个
    // 按钮，不是"保存并下一条"。以前这里不排按钮又无条件 preventDefault，
    // 于是回车被劫持成保存（而且保存是重入的，连按两下会一个成功一个 409）。
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'BUTTON') return;
    if (event.key === 'Enter') {
      event.preventDefault();
      saveAnnotation();
      return;
    }
    if (event.key === 's' || event.key === 'S') {
      event.preventDefault();
      skipAnnotation();
      return;
    }
    const choice = ANNOTATION_KEYS[event.key.toLowerCase()];
    if (!choice || !annotation.current) return;
    event.preventDefault();
    const [head, value] = choice;
    annotation.draft[head] = annotation.draft[head] === value ? '' : value;
    renderAnnotationDetail();
  });

  // ───────────────────────────── 系统 ─────────────────────────────
  //
  // 主机与进程的实时面板：身份、运行环境、CPU / 内存（环形仪表盘）、磁盘、
  // 网络，以及从 OneBot 服务端问来的 NapCat 版本与登录号。页面停留时每 5 秒
  // 自动刷新一次，离开就停——采样器是复用的，只有真在看才值得刷。

  let systemTimer = null;

  function stopSystemRefresh() {
    if (systemTimer !== null) {
      clearInterval(systemTimer);
      systemTimer = null;
    }
  }

  async function renderSystemPage() {
    const page = $('#page-system');
    clear(page);
    page.append(h('div', { class: 'loading', text: '采集系统信息…' }));
    let data;
    try {
      data = await api('/api/system');
    } catch (problem) {
      clear(page);
      page.append(h('div', { class: 'empty', text: problem.textContent || problem.message }));
      return;
    }
    if (currentPage !== 'system') return;
    clear(page);

    const identity = data.identity || {};
    const host = data.host || {};
    const cpu = data.cpu || {};
    const memory = data.memory || {};
    const onebot = data.onebot || {};

    const left = h('div', { class: 'sys-column' },
      h('div', { class: 'card identity-card' },
        h('div', { class: 'identity-mark' }, '汐'),
        h('div', { class: 'identity-body' },
          h('div', { class: 'identity-name' },
            identity.name || '芸汐',
            h('span', {
              class: `dot ${onebot.available ? 'ok' : 'off'}`,
              title: onebot.available ? 'OneBot 已连接' : 'OneBot 未连接',
            })),
          h('div', { class: 'identity-sub' },
            onebot.login && onebot.login.user_id
              ? `QQ ${onebot.login.user_id}${onebot.login.nickname ? ` · ${onebot.login.nickname}` : ''}`
              : '未取到登录号'))),

      h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: '运行环境' })),
        h('dl', { class: 'kv sys-kv' },
          h('dt', { text: '机器人版本' }), h('dd', { text: identity.version || '—' }),
          h('dt', { text: '部署版本' }), h('dd', { class: 'mono', text: String(identity.revision || '未知').slice(0, 12) }),
          h('dt', { text: '操作系统' }), h('dd', { text: host.os || '—' }),
          h('dt', { text: '内核' }), h('dd', { text: host.kernel || '—' }),
          h('dt', { text: '架构' }), h('dd', { text: host.arch || '—' }),
          h('dt', { text: '主机名' }), h('dd', { text: host.hostname || '—' }),
          h('dt', { text: '系统运行' }), h('dd', { text: formatDuration(host.uptime_secs) }),
          h('dt', { text: '进程运行' }), h('dd', { text: `${formatDuration(data.process && data.process.uptime_secs)} · pid ${(data.process && data.process.pid) || '—'}` }),
          h('dt', { text: '工作目录' }), h('dd', { class: 'mono', text: identity.runtime_dir || '—' }))),

      h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: 'OneBot 服务端' })),
        h('dl', { class: 'kv sys-kv' },
          h('dt', { text: '实现' }), h('dd', {
            text: onebot.available
              ? [onebot.version && onebot.version.app_name, onebot.version && onebot.version.app_version].filter(Boolean).join(' ') || '已连接'
              : (onebot.detail || '未连接'),
          }),
          h('dt', { text: '协议' }), h('dd', { text: (onebot.version && onebot.version.protocol_version) || '—' }),
          h('dt', { text: '在线' }), h('dd', {
            text: onebot.status && onebot.status.online !== undefined
              ? (onebot.status.online ? '在线' : '离线')
              : '—',
          }),
          h('dt', { text: '连接状态' }), h('dd', {
            text: onebot.status && onebot.status.good !== undefined
              ? (onebot.status.good ? '正常' : '异常')
              : '—',
          }))));

    const right = h('div', { class: 'sys-column wide' },
      h('div', { class: 'sys-meters' },
        meterCard('CPU', cpu.usage_percent || 0, loadPercentTone(cpu.usage_percent || 0, 85, 95), [
          ['型号', cpu.brand || '—'],
          ['内核数', cpu.physical_cores ? `${cpu.physical_cores} 物理 / ${cpu.cores} 逻辑` : String(cpu.cores || '—')],
          ['主频', cpu.frequency_mhz ? `${cpu.frequency_mhz} MHz` : '—'],
          ['本进程', cpu.process_percent === null || cpu.process_percent === undefined
            ? '—' : `${Number(cpu.process_percent).toFixed(1)}%`],
        ]),
        meterCard('内存', memory.percent || 0, loadPercentTone(memory.percent || 0), [
          ['总量', fmtBytes(memory.total)],
          ['已用', `${fmtBytes(memory.used)}（可用 ${fmtBytes(memory.available)}）`],
          ['本进程', memory.process_rss === null || memory.process_rss === undefined
            ? '—' : `${fmtBytes(memory.process_rss)} 常驻`],
          ['交换区', memory.swap_total ? `${fmtBytes(memory.swap_used)} / ${fmtBytes(memory.swap_total)}` : '未启用'],
        ])),

      h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: '磁盘' }),
          h('span', { class: 'hint', text: (data.disks || []).length ? '' : '没有可读的挂载点' })),
        ...(data.disks || []).map((disk) => h('div', { class: 'disk-row' },
          h('div', { class: 'disk-head' },
            h('span', { class: 'mono', text: disk.mount }),
            h('span', { class: 'muted small', text: `${fmtBytes(disk.used)} / ${fmtBytes(disk.total)}` })),
          h('div', { class: 'meter-bar' },
            h('div', {
              class: `meter-fill ${loadPercentTone(disk.percent)}`,
              style: `width: ${Math.min(100, disk.percent)}%`,
            }))))),

      h('div', { class: 'card' },
        h('div', { class: 'card-head' }, h('h3', { text: '网络与模型' })),
        h('dl', { class: 'kv sys-kv' },
          h('dt', { text: '累计接收' }), h('dd', { text: fmtBytes(data.network && data.network.received) }),
          h('dt', { text: '累计发送' }), h('dd', { text: fmtBytes(data.network && data.network.transmitted) }),
          h('dt', { text: '外部模型' }), h('dd', { text: `${(data.model && data.model.model_name) || '—'}（${(data.model && data.model.wire_api) || '—'}）` }),
          h('dt', { text: '接口地址' }), h('dd', { class: 'mono', text: (data.model && data.model.endpoint) || '—' }))));

    page.append(h('div', { class: 'sys-layout' }, left, right));

    stopSystemRefresh();
    systemTimer = setInterval(async () => {
      if (currentPage !== 'system') {
        stopSystemRefresh();
        return;
      }
      try {
        const fresh = await api('/api/system');
        if (currentPage !== 'system') return;
        const meters = $('#page-system .sys-meters');
        if (!meters) return;
        // 只更新仪表盘与磁盘数字，避免整页重绘打断阅读。
        const values = meters.querySelectorAll('.gauge-value');
        if (values[0]) values[0].textContent = `${Number(fresh.cpu.usage_percent || 0).toFixed(0)}%`;
        setGauge(meters.querySelectorAll('.gauge')[0], fresh.cpu.usage_percent || 0,
          loadPercentTone(fresh.cpu.usage_percent || 0, 85, 95));
        if (values[1]) values[1].textContent = `${Number(fresh.memory.percent || 0).toFixed(0)}%`;
        setGauge(meters.querySelectorAll('.gauge')[1], fresh.memory.percent || 0,
          loadPercentTone(fresh.memory.percent || 0));
      } catch (_) {
        /* 自动刷新失败不打扰用户，下一次再试 */
      }
    }, 5000);
  }

  /** 按占用率挑颜色档；CPU 是突发型的，所以阈值另给。 */
  function loadPercentTone(percent, warnAt = 75, dangerAt = 90) {
    if (percent >= dangerAt) return 'danger';
    if (percent >= warnAt) return 'warn';
    return 'ok';
  }

  function formatDuration(seconds) {
    if (seconds === null || seconds === undefined) return '—';
    const total = Math.max(0, Math.floor(Number(seconds)));
    const days = Math.floor(total / 86400);
    const hours = Math.floor((total % 86400) / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    if (days > 0) return `${days} 天 ${hours} 小时`;
    if (hours > 0) return `${hours} 小时 ${minutes} 分`;
    return `${minutes} 分 ${total % 60} 秒`;
  }

  const GAUGE_RADIUS = 52;
  const GAUGE_CIRCUMFERENCE = 2 * Math.PI * GAUGE_RADIUS;

  /** 环形仪表盘：CPU / 内存各一个，跟参考面板一样一眼看出水位。 */
  function gauge(percent, tone) {
    const value = Math.max(0, Math.min(100, Number(percent) || 0));
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 120 120');
    svg.setAttribute('class', `gauge ${tone}`);
    const track = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
    track.setAttribute('class', 'gauge-track');
    track.setAttribute('cx', '60');
    track.setAttribute('cy', '60');
    track.setAttribute('r', String(GAUGE_RADIUS));
    const fill = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
    fill.setAttribute('class', 'gauge-fill');
    fill.setAttribute('cx', '60');
    fill.setAttribute('cy', '60');
    fill.setAttribute('r', String(GAUGE_RADIUS));
    fill.setAttribute('stroke-dasharray', String(GAUGE_CIRCUMFERENCE));
    fill.setAttribute('stroke-dashoffset', String(GAUGE_CIRCUMFERENCE * (1 - value / 100)));
    const label = document.createElementNS('http://www.w3.org/2000/svg', 'text');
    label.setAttribute('class', 'gauge-value');
    label.setAttribute('x', '60');
    label.setAttribute('y', '66');
    label.textContent = `${value.toFixed(0)}%`;
    svg.append(track, fill, label);
    return svg;
  }

  function setGauge(svg, percent, tone) {
    if (!svg) return;
    const value = Math.max(0, Math.min(100, Number(percent) || 0));
    const fill = svg.querySelector('.gauge-fill');
    if (fill) fill.setAttribute('stroke-dashoffset', String(GAUGE_CIRCUMFERENCE * (1 - value / 100)));
    // 颜色也要跟着值走，不然刷新后环的长度变了、颜色还停在首屏那一档。
    if (tone) svg.setAttribute('class', `gauge ${tone}`);
  }

  function meterCard(title, percent, tone, rows) {
    return h('div', { class: 'card meter-card' },
      h('div', { class: 'meter-main' },
        h('h3', { text: title }),
        h('dl', { class: 'kv sys-kv' },
          ...rows.flatMap(([label, value]) => [h('dt', { text: label }), h('dd', { text: value })]))),
      gauge(percent, tone));
  }

  // ───────────────────────────── 弹窗 ─────────────────────────────

  let modalFooter = null;
  /** 关掉弹窗后把焦点还回去：键盘用户不该被丢在 body 上。 */
  let modalReturnFocus = null;

  function openModal(title, bodyNode, buttons, note) {
    if ($('#modal').hidden) modalReturnFocus = document.activeElement;
    $('#modal-title').textContent = title;
    $('#modal-note').textContent = note || '';
    $('#modal-body').replaceChildren(bodyNode);
    const footer = $('#modal-foot');
    clear(footer);
    if (buttons && buttons.length) {
      footer.hidden = false;
      for (const button of buttons) footer.append(button);
    } else {
      footer.hidden = true;
    }
    $('#modal').hidden = false;
    const card = $('#modal .modal-card');
    // 要在大段文本里编辑（原始 TOML）时把弹窗放宽，少折几行。
    if (card) {
      card.classList.toggle('wide', Boolean(bodyNode && bodyNode.querySelector?.('.textarea')));
      // 焦点进弹窗：Escape 关得掉，Tab 也不会跑到背后的页面上。
      card.focus({ preventScroll: true });
    }
  }

  function closeModal() {
    const modal = $('#modal');
    if (modal.hidden) return;
    modal.hidden = true;
    if (modalReturnFocus && modalReturnFocus.isConnected) {
      modalReturnFocus.focus({ preventScroll: true });
    }
    modalReturnFocus = null;
  }

  $('#modal-close').addEventListener('click', closeModal);
  $('#modal').addEventListener('click', (event) => {
    if (event.target.id === 'modal' || event.target.classList.contains('modal-backdrop')) closeModal();
  });
  document.addEventListener('keydown', (event) => { if (event.key === 'Escape') closeModal(); });

  // ───────────────────────────── 启动 ─────────────────────────────

  // 主题由 index.html 的头部脚本先定好（避免闪白），这里只同步标签，不重新决定。
  applyTheme(document.documentElement.dataset.theme || 'dark');

  (async () => {
    // `?token=` 入口失败后会带着 error 回到这里；地址栏当场清干净。
    const params = new URLSearchParams(location.search);
    if (params.get('error') === 'token') {
      history.replaceState(null, '', location.pathname);
      showLoginError('链接里的 Token 不正确或已失效，请重新输入。');
    }
    // **"没登录"与"登录了但页面没渲染出来"必须分开。** 以前整段共用一个
    // try/catch：任何一次首屏渲染失败（例如数据库暂时连不上，而 `/api/memory/stats`
    // 是允许 503 的）都会被当成"会话失效"，于是把人踢回登录页——重新登录还是同一个
    // 结果，控制台恰好在出事的时候进不去。现在只把真正的会话检查失败当作未登录。
    let authenticated = false;
    try {
      await api('/api/session');
      authenticated = true;
    } catch (_) {
      authenticated = false;
    }
    if (!authenticated) {
      showLogin();
      return;
    }
    showApp();
    try {
      await boot();
    } catch (problem) {
      // 会话是好的，是页面本身没起来：留在控制台里，把原因写在当前页上。
      // （`goto` 会先设好 `currentPage` 再渲染，所以这里拿到的是出错的那一页。）
      const reason = problem && problem.message ? problem.message : String(problem);
      const page = document.getElementById(`page-${currentPage || 'overview'}`)
        || document.getElementById('page-overview');
      if (page) {
        page.replaceChildren(h('div', { class: 'card' },
          h('h3', { text: '这一页没能加载' }),
          h('div', { class: 'hint', text: '会话有效，是页面要的数据没取到。修好后点侧栏或刷新即可。' }),
          h('pre', { class: 'json', text: reason })));
      }
    }
  })();
})();
