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

  /** 当前是不是窄屏（手机）。与 app.css 里 760px 那一档断点保持同一个数——
   *  有些取舍（标注页把统计收起来、队列放到详情下面）靠 CSS 表达不了。 */
  const NARROW_QUERY = '(max-width: 760px)';
  const narrowLayout = () => window.matchMedia(NARROW_QUERY).matches;

  function toast(message, kind = 'ok', ms = 4200) {
    const node = h('div', { class: `toast ${kind}`, text: message });
    $('#toast-stack').append(node);
    setTimeout(() => node.remove(), ms);
  }

  async function api(path, options = {}) {
    const response = await fetch(path, {
      credentials: 'same-origin',
      headers: options.body ? { 'Content-Type': 'application/json' } : {},
      ...options,
      body: options.body ? JSON.stringify(options.body) : undefined,
    });
    if (response.status === 401) {
      showLogin();
      throw new Error('登录状态已失效，请重新登录');
    }
    let payload = null;
    try { payload = await response.json(); } catch (_) { /* 有些响应没有正文 */ }
    if (!response.ok) {
      throw new Error((payload && payload.error) || `请求失败（HTTP ${response.status}）`);
    }
    return payload;
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
  });

  // ───────────────────────────── 页面切换 ─────────────────────────────

  const PAGES = {
    overview: { title: '概览', subtitle: '进程、存储、模型与调度器的现状', render: renderOverview },
    config: { title: '配置', subtitle: '全部参数；保存前会校验，保存时保留注释', render: renderConfigPage },
    memory: { title: '记忆', subtitle: '长期记忆、情节、人物与未完结线索', render: renderMemoryPage },
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

  async function goto(page) {
    if (page !== 'system') stopSystemRefresh();
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
    await PAGES[page].render();
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

  $('#refresh').addEventListener('click', () => refreshAll());

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
  });

  async function refreshHealth() {
    const pill = $('#health-pill');
    try {
      const status = await api('/api/status');
      const ok = status.database.ok && status.redis.ok;
      pill.className = `pill ${ok ? 'ok' : 'bad'}`;
      pill.textContent = ok
        ? `运行中 · ${status.uptime}`
        : (status.database.ok ? 'Redis 不可用' : '数据库不可用');
      $('#brand-revision').textContent = `${status.version} · ${String(status.revision).slice(0, 8)}`;
      overviewCache = status;
    } catch (problem) {
      pill.className = 'pill bad';
      pill.textContent = problem.message;
    }
  }

  // ───────────────────────────── 概览 ─────────────────────────────

  let overviewCache = null;

  function stat(label, value, extra, small) {
    return h('div', { class: 'stat' },
      h('div', { class: 'label', text: label }),
      h('div', { class: `value${small ? ' small' : ''}`, text: value }),
      extra ? h('div', { class: 'extra', text: extra }) : null);
  }

  async function renderOverview() {
    const page = $('#page-overview');
    clear(page);
    page.append(h('div', { class: 'loading', text: '读取状态…' }));
    let status;
    try {
      status = await api('/api/status');
      overviewCache = status;
    } catch (problem) {
      clear(page);
      page.append(h('div', { class: 'empty', text: problem.message }));
      return;
    }
    clear(page);

    const counts = status.counts || {};
    page.append(h('div', { class: 'section-title' }, h('span', { text: '运行状态' })));
    page.append(h('div', { class: 'stat-grid' },
      stat('系统运行', status.uptime, `芸汐进程 pid ${status.pid}`),
      stat('进程内存', (status.process || '').replace('芸汐进程内存: ', '') || '—'),
      stat('PostgreSQL', status.database.ok ? '正常' : '异常',
        status.database.ok ? fmtBytes(status.database.size_bytes) : status.database.detail, true),
      stat('Redis', status.redis.ok ? '正常' : '异常', status.redis.detail, true),
      stat('长期记忆', String(counts.memories ?? '—'), `共 ${counts.total ?? '—'} 条记录`),
      stat('情节', String(counts.episodes ?? '—'), 'Mind Episode'),
      stat('人物', String(counts.people ?? '—'), 'canonical Person'),
      stat('目标 / 线索', `${counts.goals ?? '—'} / ${counts.open_loops ?? '—'}`),
    ));

    const pendingRestart = (status.admin && status.admin.pending_restart) || [];
    if (pendingRestart.length) {
      page.append(h('div', { class: 'restart-note' },
        `有 ${pendingRestart.length} 个分区的改动已保存，但要重启进程才生效：`,
        h('strong', { text: pendingRestart.join('、') }),
        '（按你的部署方式重启服务，例如 systemctl restart kovi-bot）。'));
    }

    const model = status.model || {};
    const modelCard = h('div', { class: 'card' },
      h('div', { class: 'card-head' }, h('h3', { text: '模型与调度' }),
        h('span', { class: 'hint', text: `配置 ${status.config.path}` })),
      h('dl', { class: 'kv schedule-kv' },
        h('dt', { text: '外部模型' }), h('dd', { text: model.enabled ? `${model.model_name}（${model.endpoint}）` : '已关闭' }),
        h('dt', { text: 'Token 环境变量' }), h('dd', { class: 'mono', text: model.api_key_env || '—' }),
        h('dt', { text: '本地 Intrinsic' }), h('dd', { text: model.intrinsic_enabled ? '启用' : '关闭' }),
        h('dt', { text: 'TurnGate' }), h('dd', { text: model.turn_gate_mode || '—' }),
        h('dt', { text: '主动消息' }), h('dd', { text: status.scheduler.proactive_enabled ? '启用' : '关闭' }),
        h('dt', { text: '群聊接话' }), h('dd', { text: status.scheduler.group_interjection_enabled ? '启用' : '关闭' }),
        h('dt', { text: '工具' }), h('dd', { text: status.scheduler.tools_enabled ? '启用' : '关闭' }),
        h('dt', { text: '视觉 Provider' }), h('dd', { text: status.scheduler.vision_provider || '—' }),
        h('dt', { text: '实时通话' }), h('dd', { text: status.scheduler.qq_call_enabled ? '启用' : '关闭' }),
        h('dt', { text: '配置文件' }), h('dd', { text: `${status.config.modified || '—'} · ${fmtBytes(status.config.bytes)}` }),
        h('dt', { text: '管理会话' }), h('dd', { text: `${status.admin.sessions} 个 · 后台已运行 ${Math.floor(status.admin.uptime_secs / 60)} 分钟` }),
      ));

    // 先取数据再拼版：两栏要一起进场，避免右栏比左栏晚一拍。
    const recent = await api('/api/memory/records?limit=12');
    const list = h('div', { class: 'record-list' });
    if (!recent.items.length) {
      list.append(h('div', { class: 'empty', text: '还没有记忆记录' }));
    }
    for (const item of recent.items) {
      list.append(recordNode(item, () => { goto('memory').then(() => openRecord(item)); }, false));
    }
    const recentCard = h('div', { class: 'card' },
      h('div', { class: 'card-head' }, h('h3', { text: '最近的记忆变化' }),
        h('button', { class: 'btn ghost small', text: '去记忆页', onclick: () => goto('memory') })),
      list);

    page.append(h('div', { class: 'overview-split' }, modelCard, recentCard));
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
    const sidebar = h('div', { class: 'card' }, files);
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
      class: 'input', placeholder: '搜索参数名或说明…', value: config.filter,
      oninput: (event) => {
        config.filter = event.target.value.trim().toLowerCase();
        rerenderConfigBody();
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
    return h('div', { class: 'card tight config-toolbar' },
      h('div', { class: 'search-row' }, search,
        button,
        h('button', { class: 'btn ghost', text: '原始 TOML', onclick: openRawEditor }),
        h('button', { class: 'btn ghost', text: `备份 (${backupCount()})`, onclick: openBackups }),
        config.data.writable ? null : h('span', { class: 'badge secret', text: '只读' }),
        h('button', {
          class: 'btn ghost', text: '重新加载',
          onclick: async () => {
            try {
              await api('/api/config/reload', { method: 'POST' });
              config.dirty.clear();
              await loadConfigFile(config.name);
              toast('已按磁盘内容重新加载配置');
            } catch (problem) { toast(problem.message, 'bad'); }
          },
        })),
      h('div', { class: 'field-hint', text: data.description || '' }));
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
    const isTyped = data.typed;
    let shown = 0;
    // 顶层分区是稳定的，但重画时会收集一遍；这里只在第一次算。
    config.sectionPaths = [];

    const sections = Object.keys(values);

    for (const key of sections) {
      const value = values[key];
      const sectionNode = renderSection([key], key, value, isTyped);
      if (!sectionNode) continue;
      shown += 1;
      container.append(sectionNode);
      nav.append(h('button', {
        onclick: () => {
          sectionNode.openSection?.();
          sectionNode.scrollIntoView({ behavior: 'smooth', block: 'start' });
        },
      }, h('span', { text: sectionTitle(key, isTyped) }),
        h('span', { class: 'count', text: countFields(value) })));
    }

    if (!shown) {
      container.append(h('div', { class: 'empty', text: '没有匹配的参数' }));
    }

    // 导航区整块替换：只删 nav 会把标题留下，重画几次就叠出好几行"分区"。
    let block = sidebar.querySelector('.section-nav-block');
    if (!block) {
      block = h('div', { class: 'section-nav-block' });
      sidebar.append(block);
    }
    clear(block);
    block.append(
      h('div', { class: 'card-head', style: 'padding: 12px 2px 0' },
        h('h3', { text: isTyped ? '分区' : '顶层键' })),
      nav);
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

    const chevron = h('span', { class: 'section-chevron', text: isOpen ? '▾' : '▸' });
    // 说明文字只在展开时露出：折起来时它就是白白占一行高度。
    const docNode = doc && doc.section ? h('p', { class: 'section-doc', text: doc.section, hidden: !isOpen }) : null;
    const header = h('header', {
      class: searching ? 'locked' : '',
      title: searching ? '搜索中：结果已自动展开' : '点击折叠 / 展开',
      onclick: () => {
        if (searching) return;
        const open = body.hidden;
        body.hidden = !open;
        chevron.textContent = open ? '▾' : '▸';
        if (open) config.expanded.add(sectionPath);
        else config.expanded.delete(sectionPath);
        if (docNode) docNode.hidden = !open;
      },
    },
      chevron,
      h('div', { class: 'section-head-main' },
        h('h3', { text: sectionTitle(key) }),
        h('span', { class: 'path', text: sectionPath })),
      h('span', { class: 'section-count', text: `${visible} 个参数` }));

    const card = h('section', { class: 'card section-card', id: `section-${sectionPath}` });
    // 左栏导航要用它：先展开再滚过去，否则滚到一个折着的标题上什么也看不见。
    card.openSection = () => {
      body.hidden = false;
      chevron.textContent = '▾';
      if (docNode) docNode.hidden = false;
      config.expanded.add(sectionPath);
    };
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

    return h('div', { class: `field${changed ? ' changed' : ''}` },
      h('div', { class: 'field-label' },
        h('div', { class: 'field-head' },
          h('span', { class: 'field-name', text: path.slice(-1)[0] }),
          secret ? h('span', { class: 'badge secret', text: '密钥' }) : null,
          restart ? h('span', { class: 'badge restart', text: '需重启' }) : null,
          !present ? h('span', { class: 'badge default', text: '本文件未写' }) : null,
          numericSegments.length ? h('span', { class: 'badge', text: `第 ${Number(numericSegments[0]) + 1} 项` }) : null),
        doc ? h('div', { class: 'field-doc', text: doc }) : null,
        hint ? h('div', { class: 'field-hint', text: hint }) : null),
      h('div', { class: 'field-control' }, control));
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
    const field = document.querySelector(`[data-field="${CSS.escape(key)}"]`);
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
      class: 'input',
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
    return h('div', { class: 'table-foot' },
      h('span', { class: 'muted', text: `${start}-${end} / 共 ${data.total} 条` }),
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
        `${dated} 条记忆（${first.toLocaleDateString('zh-CN', { month: 'short', year: 'numeric' })} → ${last.toLocaleDateString('zh-CN', { month: 'short', year: 'numeric' })}）`),
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
      h('div', { class: 'graph-hint', text: '滚动缩放 · 拖动平移 · 悬停探索 · 点击查看详情' }),
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
      heatMin.textContent = memory.colorBy === 'weight' ? '低' : shortDate(graph.range && graph.range.min);
      heatMax.textContent = memory.colorBy === 'weight' ? '高' : shortDate(graph.range && graph.range.max);

      const collapsed = memory.collapseDuplicates ? collapseDuplicateNodes(graph) : null;
      const shown = collapsed || {
        nodes: graph.nodes || [],
        links: graph.links || [],
        linkCounts: graph.link_counts || {},
        hidden: 0,
      };
      const visibleLinks = (shown.links || []).filter((link) => memory.linkTypes.has(link.type));

      sideBody.replaceChildren(
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
            h('span', { class: 'muted', text: String(shown.linkCounts[key] || 0) })))));

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
    const width = () => canvas.clientWidth || 640;
    const height = () => canvas.clientHeight || 520;

    // 节点对象每次都是新的，悬停状态不能跨重绘沿用。
    view.hovered = null;

    const nodes = graph.nodes || [];
    const byId = new Map(nodes.map((node) => [node.id, node]));
    const edges = (graph.links || [])
      .map((link) => ({ ...link, a: byId.get(link.source), b: byId.get(link.target) }))
      .filter((edge) => edge.a && edge.b);

    // 时间/权重 → 0..1 的热度
    const times = nodes.map((node) => new Date(occurredOf(node)).getTime()).filter((t) => !Number.isNaN(t));
    const minTime = times.length ? Math.min(...times) : 0;
    const maxTime = times.length ? Math.max(...times) : 1;
    const maxWeight = Math.max(1, ...nodes.map((node) => Number(node.weight) || 0));
    const heat = (node) => {
      if (memory.colorBy === 'weight') return (Number(node.weight) || 0) / maxWeight;
      const value = new Date(memory.colorBy === 'mentioned_at' ? mentionedOf(node) : occurredOf(node)).getTime();
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

    const nodeAt = (event) => {
      const rect = canvas.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      let best = null;
      let bestDistance = 16;
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

      canvas.addEventListener('mousemove', (event) => {
        const current = runtime();
        if (!current) return;
        const { view: currentView, nodeAt: pick, paint: repaint } = current;
        if (currentView.dragging) {
          currentView.panX += event.movementX / currentView.zoom;
          currentView.panY += event.movementY / currentView.zoom;
          currentView.moved = true;
          repaint();
          return;
        }
        const node = pick(event);
        if (node !== currentView.hovered) {
          currentView.hovered = node;
          canvas.style.cursor = node ? 'pointer' : 'grab';
          repaint();
        }
        if (node && tip) {
          const rect = canvas.getBoundingClientRect();
          tip.hidden = false;
          tip.style.left = `${event.clientX - rect.left + 14}px`;
          tip.style.top = `${event.clientY - rect.top + 10}px`;
          clear(tip);
          tip.append(
            h('div', { class: 'tip-kind', text: node.label || node.kind }),
            h('div', { class: 'tip-title', text: node.title || '' }),
            h('div', { class: 'tip-meta', text: `${node.scope_label || '全局'} · ${shortDate(mentionedOf(node))}` }));
        } else if (tip) {
          tip.hidden = true;
        }
      });
      canvas.addEventListener('mouseleave', () => {
        const current = runtime();
        if (!current) return;
        current.view.hovered = null;
        if (tip) tip.hidden = true;
        current.paint();
      });
      canvas.addEventListener('mousedown', () => {
        const current = runtime();
        if (!current) return;
        current.view.dragging = true;
        current.view.moved = false;
        canvas.style.cursor = 'grabbing';
      });
      window.addEventListener('mouseup', () => {
        const current = runtime();
        if (!current) return;
        current.view.dragging = false;
        canvas.style.cursor = current.view.hovered ? 'pointer' : 'grab';
      });
      canvas.addEventListener('click', (event) => {
        const current = runtime();
        if (!current || current.view.moved) return;
        const node = current.nodeAt(event);
        if (node) openRecord(node);
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
      h('dt', { text: '标识' }), h('dd', { class: 'mono', text: `${payload.table} · ${record.id}` })));

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
    const search = h('input', { class: 'input', placeholder: '按 QQ 号或身份搜索…', value: memory.query });
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

  function bars(relation) {
    if (!relation) return null;
    const node = h('div', { class: 'bars' });
    for (const [key, label] of [['familiarity', '熟悉'], ['affinity', '好感'], ['trust', '信任'], ['comfort', '自在']]) {
      const value = relation[key];
      if (value === null || value === undefined) continue;
      node.append(h('div', { class: 'bar' },
        h('span', { text: label }),
        h('div', { class: 'track' },
          h('div', { class: 'fill', style: `width: ${Math.round(((Number(value) + 1) / 2) * 100)}%` }))));
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
      body.append(h('h4', { text: '关系' }), bars(payload.relation));
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

  /** 人物弹窗里用的紧凑记录行。 */
  function recordNode(item, onClick, active) {
    const needle = memory.query;
    return h('div', {
      class: `record${active ? ' active' : ''}`,
      onclick: onClick,
    },
      h('div', { class: 'record-head' },
        h('span', { class: 'badge kind', text: item.label || item.kind }),
        item.status ? h('span', { class: 'badge', text: item.status }) : null),
      h('div', { class: 'record-title' }, ...highlighted(item.title, needle)),
      item.body && item.body !== item.title
        ? h('div', { class: 'record-body' }, ...highlighted(item.body.slice(0, 160), needle))
        : null,
      h('div', { class: 'record-foot' },
        h('span', { text: item.scope_label || '全局' }),
        h('span', { text: relative(item.occurred_at) || fmtTime(item.occurred_at) })));
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
  /** 说话人编号用的字母表（下标 = collector 落的编号：0=a，1=b…）。 */
  const SPEAKER_LETTERS = 'ABCDEFGH';
  /** 其他说话人的配色档数，与 CSS 的 --speaker-0..3 一一对应（多了就轮转）。 */
  const SPEAKER_TONES = 4;
  /** 老批次没有说话人编号时的退路：只能按角色显示，看不出是几个其他人。 */
  const ROLE_LABELS = { assistant: '芸汐', user: '同一人', other_member: '他人' };
  const SNIPPET_CHARS = 56;

  /**
   * 上下文里一个回合的显示名与配色。
   *
   * collector 从 2026-09-13 起给 recent_turns 落样本内匿名的 `speaker`
   * （a = 当前发言者，b/c/… 按首次出现顺序给其他成员）。没有这个字段时，几个
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
        label: `说话人${SPEAKER_LETTERS[index]}`,
        // 当前发言者（A）沿用原来的绿色，其他人按 --speaker-0..3 轮转。
        tone: index === 0 ? 'user' : `speaker-${(index - 1) % SPEAKER_TONES}`,
      };
    }
    return { label: ROLE_LABELS[turn && turn.role] || (turn && turn.role) || '?', tone: 'other' };
  }

  /** 这条样本有没有说话人编号——没有就只能是"他人"，得让标注的人知道。 */
  function annotationHasSpeakers(sample) {
    return ((sample && sample.context && sample.context.recent_turns) || [])
      .some((turn) => turn && turn.speaker);
  }

  /** 老批次里"他人"的悬停说明：不是没显示，是当时根本没记。 */
  const SPEAKER_UNRECORDED = '这批采于说话人编号之前，只能标出"其他人"，看不出是几个';

  function annotationSnippet(text) {
    const flat = String(text == null ? '' : text).split(/\s+/).filter(Boolean).join(' ');
    return flat.length > SNIPPET_CHARS ? `${flat.slice(0, SNIPPET_CHARS)}…` : flat;
  }

  function annotationQuery(extra) {
    const params = new URLSearchParams({
      batch: annotation.batch,
      limit: String(annotation.limit),
      include_reviewed: String(annotation.tab === 'reviewed'),
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
    page.append(h('div', { class: 'annotate-grid' },
      h('div', { class: 'record-list', id: 'annotate-list' }),
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
    renderAnnotationList();
    renderAnnotationDetail();
    if (scrollToDetail && narrowLayout()) {
      // 让开吸顶的工具栏再滚：直接 scrollIntoView 会把卡片标题压在工具栏下面。
      // 工具栏高度随标题换行变化，所以量一次，不用写死像素。
      const topbar = document.querySelector('.topbar');
      const offset = (topbar ? topbar.getBoundingClientRect().height : 0) + 10;
      window.scrollTo({
        top: host.getBoundingClientRect().top + window.scrollY - offset,
        behavior: 'smooth',
      });
    }
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

    const body = h('div', { class: 'annotate-body' });
    for (const fragment of context.pending_user_fragments || []) {
      // 片段与正文同属一次发言（collector 按"同群同发送者间隔 ≤3s"切分），
      // 所以它们的说话人必然是当前发言者——编号固定是 A，不必各带一个字段。
      body.append(h('div', { class: 'annotate-turn' },
        h('span', { class: 'annotate-role user', text: '说话人A' }),
        h('span', { class: 'annotate-text' },
          h('span', { class: 'annotate-tag', text: '前一句' }),
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
        h('span', { class: 'annotate-role user', text: '说话人A' }),
        h('span', {
          class: 'annotate-current-text',
          text: String(sample.current_text == null ? '' : sample.current_text),
        })),
      body.childNodes.length ? h('div', { class: 'annotate-context' }, body) : null,
      h('div', { class: 'annotate-meta' },
        h('span', { text: `弱标签：completion=${labels.completion == null ? '未给' : labels.completion}` }),
        h('span', { text: `response=${labels.response == null ? '未给' : labels.response}` }),
        h('span', { text: `来源 ${provenance.source || '未知'}` }),
        // 老批次的"他人"分不出是几个人：明说，免得以为界面漏了编号。
        annotationHasSpeakers(sample) || !(context.recent_turns || []).length
          ? null
          : h('span', { class: 'annotate-note', text: '说话人未记录（这批采于编号之前）' })),
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

  /** 标完一条之后：本地摘掉它并打开下一条，队列空了就重新拉一页。 */
  async function advanceAnnotation() {
    renderAnnotationSummary();
    const marked = currentAnnotationIndex();
    annotation.items = annotation.items.filter((item) => item.index !== marked);
    if (annotation.tab === 'pending') annotation.matched = Math.max(0, annotation.matched - 1);
    annotation.current = null;
    const next = annotation.items.find((item) => item.index > marked) || annotation.items[0];
    if (next) {
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
    const next = annotation.items.find((item) => item.index > current)
      || annotation.items.find((item) => item.index !== current);
    if (!next) {
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
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
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

  function openModal(title, bodyNode, buttons, note) {
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
  }

  function closeModal() { $('#modal').hidden = true; }

  $('#modal-close').addEventListener('click', closeModal);
  $('#modal').addEventListener('click', (event) => { if (event.target.id === 'modal') closeModal(); });
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
    try {
      await api('/api/session');
      showApp();
      await boot();
    } catch (_) {
      showLogin();
    }
  })();
})();
