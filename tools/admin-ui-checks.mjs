// 后台前端的几处「文案/缓存/身份」不变式检查。
//
// 后台是没有构建链的原生 JS（`plugins/model/src/admin/assets/app.js`，浏览器里以
// IIFE 运行），拿不到 Rust 侧的单元测试。这里把几个曾经出过问题、又完全由前端决定
// 对错的地方钉住：图谱节点身份、翻页条、列表缓存失效、按钮字面、统计降级显示。
//
// 用法：node tools/admin-ui-checks.mjs（无依赖，退出码非 0 表示有检查失败）

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const source = readFileSync(
  join(root, 'plugins/model/src/admin/assets/app.js'),
  'utf8',
);

/** 取出一条以 `;` 结尾的语句（括号配平后截断）。 */
function statement(marker) {
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`找不到: ${marker}`);
  let depth = 0;
  let seen = false;
  for (let i = start; i < source.length; i += 1) {
    const ch = source[i];
    if (ch === '{' || ch === '(' || ch === '[') {
      depth += 1;
      seen = true;
    } else if (ch === '}' || ch === ')' || ch === ']') {
      depth -= 1;
    } else if (ch === ';' && seen && depth === 0) {
      return source.slice(start, i + 1);
    }
  }
  throw new Error(`语句没有结束: ${marker}`);
}

/** 取出一个以 `}` 结尾的函数声明（函数声明不带分号）。
 *
 *  注意参数表里的解构也算括号：要从参数表结束之后的那个 `{` 开始配对，否则
 *  `function f(a, { b })` 会被当成"函数体只有一行"。 */
function block(marker) {
  const start = source.indexOf(marker);
  if (start < 0) throw new Error(`找不到: ${marker}`);
  let index = source.indexOf('(', start);
  let parens = 0;
  for (; index < source.length; index += 1) {
    if (source[index] === '(') parens += 1;
    else if (source[index] === ')') {
      parens -= 1;
      if (parens === 0) break;
    }
  }
  const open = source.indexOf('{', index);
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    if (source[i] === '{') depth += 1;
    else if (source[i] === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(start, i + 1);
    }
  }
  throw new Error(`括号不配对: ${marker}`);
}

/** 极小的 h() 替身：保留标签、属性、文本与 append。 */
function makeH() {
  return (tag, attrs, ...children) => {
    const node = {
      tag,
      attrs: attrs || {},
      children: children
        .flat()
        .filter((child) => child !== null && child !== undefined),
      append(child) {
        node.children.push(child);
        return node;
      },
    };
    return node;
  };
}

const textOf = (node) =>
  typeof node === 'string'
    ? node
    : `${node.attrs.text || ''}${node.children.map(textOf).join('')}`;
const nodesOf = (node) =>
  typeof node === 'string'
    ? []
    : [node, ...node.children.flatMap((child) => nodesOf(child))];
const buttonsOf = (node) => nodesOf(node).filter((item) => item.tag === 'button');

let failures = 0;
/** 异步检查也要等：`check` 里跑的断言可能 await，漏等就等于没检查。 */
async function check(name, fn) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (error) {
    failures += 1;
    console.log(`FAIL ${name}: ${error.message}`);
  }
}
function assert(condition, message) {
  if (!condition) throw new Error(message);
}

// ── 1. 图谱节点身份：跨类型同 id 不能撞车 ─────────────────────────
{
  const built = new Function(
    'h',
    `${statement('const nodeKey =')}
     ${statement('const LINK_TYPES =')}
     ${statement('const mentionedOf =')}
     ${statement('function collapseDuplicateNodes(')}
     return { collapseDuplicateNodes, nodeKey };`,
  )(makeH());
  const { collapseDuplicateNodes, nodeKey } = built;

  await check('图谱：跨类型同 id 的记录各自成节点，连线端点用命名空间键', () => {
    const collapsed = collapseDuplicateNodes({
      nodes: [
        { key: 'memory:42', id: '42', kind: 'memory', title: '', mentioned_at: '2026-01-01T00:00:00Z' },
        { key: 'profile:42', id: '42', kind: 'profile', title: '', mentioned_at: '2026-02-01T00:00:00Z' },
      ],
      links: [{ source: 'memory:42', target: 'profile:42', type: 'entity' }],
    });
    assert(collapsed.nodes.length === 2, `两条不同记录不该折叠：${collapsed.nodes.length}`);
    assert(collapsed.links.length === 1, '连线应当保留');
    const keys = new Set(collapsed.nodes.map(nodeKey));
    for (const link of collapsed.links) {
      assert(keys.has(link.source) && keys.has(link.target), '端点必须是节点的 key');
    }
    assert(nodeKey({ id: '9', kind: 'memory' }) === 'memory:9', '缺 key 时按 kind:id 兜底');
  });
}

// ── 2. 翻页条 ────────────────────────────────────────────────────
{
  const renderPager = new Function(
    'h',
    `${block('function renderPager(')}\nreturn renderPager;`,
  )(makeH());

  await check('翻页条：区间、页码与边界钳制按传入的 page/limit 计算', () => {
    const seen = [];
    const node = renderPager(
      { total: 130 },
      { page: 2, limit: 60, onJump: (target) => seen.push(target) },
    );
    assert(textOf(node).includes('61-120 / 共 130 条'), `区间不对: ${textOf(node)}`);
    assert(textOf(node).includes('2 / 3'), `页码不对: ${textOf(node)}`);
    const buttons = buttonsOf(node);
    assert(buttons.length === 4, `应有 4 个翻页按钮，实际 ${buttons.length}`);
    buttons[0].attrs.onclick();
    buttons[3].attrs.onclick();
    assert(seen[0] === 1, `首页应钳到 1，实际 ${seen[0]}`);
    assert(seen[1] === 3, `末页应钳到 3，实际 ${seen[1]}`);
    const single = renderPager({ total: 0 }, { page: 1, limit: 60, onJump: () => {} });
    assert(textOf(single).includes('0-0 / 共 0 条'), '空结果不该出现 1-0 这种区间');
  });
}

// ── 3. 列表缓存失效 ─────────────────────────────────────────────
{
  const built = new Function(
    'api',
    `${statement('const memory = {')}
     ${statement('async function fetchTagList()')}
     return { memory, fetchTagList };`,
  );
  const guard = block('if (config.staleFiles || !config.files.length) {');

  await check('标签表：页内复用缓存，置脏后重拉', async () => {
    let calls = 0;
    const { memory, fetchTagList } = built(async () => {
      calls += 1;
      return { tags: [`t${calls}`] };
    });
    await fetchTagList();
    await fetchTagList();
    assert(calls === 1, `页内重画不该重复请求，实际 ${calls}`);
    memory.staleTags = true;
    const fresh = await fetchTagList();
    assert(calls === 2 && fresh.tags[0] === 't2', `置脏后应重拉，实际 ${calls}`);
  });

  await check('配置文件列表：置脏重拉，选中的文件消失时退回主文件', async () => {
    let calls = 0;
    const api = async () => {
      calls += 1;
      return {
        main: 'bot.conf.toml',
        files: [{ name: 'bot.conf.toml', backups: calls }],
        restart_sections: [],
      };
    };
    const runGuard = new Function('config', 'api', `return (async () => { ${guard} })();`);
    const config = { files: [], staleFiles: true, name: 'gone.conf.toml', data: {} };
    await runGuard(config, api);
    assert(calls === 1 && config.staleFiles === false, '置脏时应拉取并清标记');
    assert(config.name === 'bot.conf.toml', '选中的文件消失时应退回主文件');
    assert(config.data === null, '退回主文件时要丢掉旧内容缓存');
    await runGuard(config, api);
    assert(calls === 1, '未置脏时不该重复拉取');
  });
}

// ── 4. 按钮字面与追加量 ─────────────────────────────────────────
{
  const line = (marker) => {
    const start = source.indexOf(marker);
    if (start < 0) throw new Error(`找不到: ${marker}`);
    return source.slice(start, source.indexOf('\n', start));
  };
  const built = new Function(
    `${line('const ANNOTATION_PAGE_STEP =')}
     ${line('const ANNOTATION_LIMIT_MAX =')}
     ${statement('const annotation = {')}
     return { ANNOTATION_PAGE_STEP, ANNOTATION_LIMIT_MAX, annotation };`,
  )();

  await check('「再加载 N 条」的 N 是本次追加量，到顶后不再渲染', () => {
    const step = (limit) =>
      Math.min(built.ANNOTATION_PAGE_STEP, built.ANNOTATION_LIMIT_MAX - limit);
    assert(built.annotation.limit === built.ANNOTATION_PAGE_STEP, '初始窗口应等于一屏');
    assert(step(40) === 40 && step(80) === 40, '每次追加一屏');
    assert(step(480) === 20, `接近上限时只补差额，实际 ${step(480)}`);
    assert(step(500) === 0, '到上限后应给 0');
  });

  await check('单节折叠会同步「全部展开/折叠」按钮字面', () => {
    const setOpen = block('const setOpen = (open, remember = true) => {');
    assert(
      setOpen.indexOf('syncExpandButton()') > setOpen.indexOf('config.expanded.add'),
      'setOpen 改完展开集合后没有同步按钮字面',
    );
    const config = { expandButton: { textContent: '' }, expanded: new Set() };
    const sync = new Function(
      'config',
      `${block('function syncExpandButton()')}\nreturn syncExpandButton;`,
    )(config);
    sync();
    assert(config.expandButton.textContent === '全部展开', '空集合应写「全部展开」');
    config.expanded.add('mood');
    sync();
    assert(config.expandButton.textContent === '全部折叠', '有展开节应写「全部折叠」');
  });
}

// ── 5. 统计降级显示 ─────────────────────────────────────────────
{
  const built = new Function(
    'h',
    `${statement('const statNumber =')}
     ${block('function deltaLine(')}
     ${block('function renderStatCards(')}
     return { statNumber, renderStatCards };`,
  )(makeH());
  const { statNumber, renderStatCards } = built;

  await check('统计：查不出来画「—」并给出降级提示，0 仍然是 0', () => {
    assert(statNumber(null) === '—' && statNumber(undefined) === '—', 'null 应画成 —');
    assert(statNumber(0) === '0', '0 必须还是 0');
    const healthy = renderStatCards({ total: 3, people: 2, conversations: 1, by_kind: {}, week: {} });
    assert(!textOf(healthy).includes('部分统计不可用'), '完整统计不该出现降级提示');
    const degraded = renderStatCards({
      total: 3,
      people: null,
      conversations: null,
      by_kind: {},
      week: {},
      partial_errors: ['人物计数不可用'],
    });
    const rendered = textOf(degraded);
    assert(rendered.includes('—'), `查不出来的项应画成 —：${rendered}`);
    assert(
      rendered.includes('部分统计不可用') && rendered.includes('人物计数不可用'),
      `降级提示要带上具体项：${rendered}`,
    );
  });
}

console.log(failures === 0 ? '\n全部通过' : `\n${failures} 项失败`);
process.exit(failures === 0 ? 0 : 1);
