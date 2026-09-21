#!/usr/bin/env node
// UI 检查：ui/ 下的脚本既没有构建步骤，也没有别的自动化入口。
//
// 三项检查：
// 1. snake_case 字段 lint——Rust 侧所有发给前端的结构体都是 `rename_all = "camelCase"`，
//    但 JS 里很容易顺手写成结构体的 Rust 字段名。这个错已经真发生过 5 次
//    （`display_name`/`spritesheet_path`/`always_on_top`/`device_key`）。
// 2. CSS 静态检查：双斜杠注释、用了未定义的 `var(--x)`。这两条都真发生过。
// 3. 把 ui/config.js 真的跑一遍。用一套最小 DOM 替身执行页面逻辑，喂进去的是从真 daemon
//    `config/get` / `pet/list` 抄回来的原样 JSON——字段名对不对由它说了算。
//    已漏过两次：`append()` 链式赋值崩溃、字段名写错。
//
// 它检查不了视觉与布局（那由 preview-ui.mjs 用真浏览器查），
// 只检查「会不会抛、字段名对不对、CSS 有没有写坏、补丁有没有越界」。
//
// 用法：node scripts/check-ui.mjs [被检查的 config.js 路径]

import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { CONFIG_GET, PET_LIST, respond } from './ui-fixture.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const scriptPath = process.argv[2] ?? join(root, 'ui', 'config.js');
const html = readFileSync(join(root, 'ui', 'config.html'), 'utf8');
const code = readFileSync(scriptPath, 'utf8');

/** 最小元素替身：只实现 config.js 用到的那部分 DOM。 */
class El {
  constructor(tag) {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.attrs = {};
    this.style = {};
    this.dataset = {};
    this.handlers = {};
    this._text = '';
    this._classes = new Set();
    this.value = '';
    this.checked = false;
    this.disabled = false;
    this.hidden = false;
  }

  get className() {
    return [...this._classes].join(' ');
  }

  set className(value) {
    this._classes = new Set(String(value).split(/\s+/).filter(Boolean));
  }

  get classList() {
    const set = this._classes;
    return {
      add: (...names) => names.forEach((name) => set.add(name)),
      remove: (...names) => names.forEach((name) => set.delete(name)),
      contains: (name) => set.has(name),
    };
  }

  get textContent() {
    return this._text + this.children.map((child) => child.textContent).join('');
  }

  set textContent(value) {
    this._text = String(value);
    this.children = [];
  }

  append(...nodes) {
    for (const node of nodes) {
      if (node !== null && node !== undefined) this.children.push(node);
    }
    // 真实 DOM 的 append 返回 undefined。这里刻意同样不返回节点：
    // 之前正是 `el.append(x).className = ...` 这种写法崩的，替身必须能复现它。
    return undefined;
  }

  replaceChildren(...nodes) {
    this._text = '';
    this.children = [];
    this.append(...nodes);
  }

  setAttribute(name, value) {
    this.attrs[name] = String(value);
  }

  getAttribute(name) {
    return this.attrs[name] ?? null;
  }

  addEventListener(type, handler) {
    (this.handlers[type] ??= []).push(handler);
  }

  /** 触发事件；返回所有处理器的返回值（异步的也一并收下）。 */
  fire(type) {
    return (this.handlers[type] ?? []).map((handler) => handler({ type }));
  }
}

/** 从 config.html 的 id 建出元素表，缺元素时能立刻暴露。 */
const registry = new Map();
// 连标签一起匹配：只按 id 建空元素的话，HTML 上写死的 `hidden` 等属性就丢了，
// 替身会与真浏览器不一致（曾因此误报“状态条没藏起来”）。
for (const match of html.matchAll(/<[^>]*\bid="([^"]+)"[^>]*>/g)) {
  const node = new El('div');
  const tag = match[0];
  if (/\shidden(?=[\s/>=])/.test(tag)) node.hidden = true;
  if (/\sdisabled(?=[\s/>=])/.test(tag)) node.disabled = true;
  if (/\schecked(?=[\s/>=])/.test(tag)) node.checked = true;
  registry.set(match[1], node);
}

const sent = [];
const sandbox = {
  window: {
    __TAURI__: {
      core: {
        invoke: async (command, args) => {
          sent.push({ command, method: args?.method, params: args?.params });
          return respond(args?.method, args?.params);
        },
        convertFileSrc: (path) => `asset://localhost/${encodeURIComponent(path)}`,
      },
    },
  },
  document: {
    getElementById: (id) => registry.get(id) ?? null,
    createElement: (tag) => new El(tag),
  },
  console,
  setTimeout,
  clearTimeout,
};
vm.createContext(sandbox);

/** 等页面把异步的 boot() 跑完。 */
async function settle() {
  for (let i = 0; i < 50; i += 1) await new Promise((resolve) => setImmediate(resolve));
}

const failures = [];
function check(ok, description) {
  if (!ok) failures.push(description);
}

// 第 1 项：snake_case 字段 lint。在所有 ui/*.js 上跑，不只 config.js。
// 只扫非注释行：注释里出现 Rust 字段名是正常的。
const SNAKE_FIELD = /\.([a-z][a-zA-Z0-9]*_[a-zA-Z0-9_]*)\b/g;
for (const name of readdirSync(join(root, 'ui')).filter((file) => file.endsWith('.js'))) {
  const lines = readFileSync(join(root, 'ui', name), 'utf8').split('\n');
  lines.forEach((line, index) => {
    if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
    for (const match of line.matchAll(SNAKE_FIELD)) {
      check(
        false,
        `ui/${name}:${index + 1} 访问了 snake_case 字段 .${match[1]}——Rust 侧 serde 是 camelCase`,
      );
    }
  });
}

// 第 2 项：CSS 静态检查。浏览器能容忍的错误写法没那么无害：
// `config.css` 曾以四行双斜杠开头，那不是 CSS 注释，解析器会把它当成一个选择器，
// 然后丢掉紧随其后的整个块——正好是 `:root`，于是全部 `var(--x)` 失效，
// 页面变成白底黑字。这两条不用开浏览器就能拦下。
const stripComments = (source) =>
  source.replace(/\/\*[\s\S]*?\*\//g, (block) => block.replace(/[^\n]/g, ' '));
for (const name of readdirSync(join(root, 'ui')).filter((file) => file.endsWith('.css'))) {
  const source = stripComments(readFileSync(join(root, 'ui', name), 'utf8'));
  source.split('\n').forEach((line, index) => {
    if (!/^\s*\/\//.test(line)) return;
    check(false, `ui/${name}:${index + 1} CSS 里没有双斜杠注释——会把后面的块整个丢掉`);
  });
  const defined = new Set([...source.matchAll(/(--[a-z0-9-]+)\s*:/g)].map((m) => m[1]));
  for (const [, variable, fallback] of source.matchAll(/var\(\s*(--[a-z0-9-]+)\s*(,)?/g)) {
    if (defined.has(variable) || fallback) continue;
    check(false, `ui/${name} 用了未定义且无兜底值的变量 ${variable}——整条声明会失效`);
  }
}

vm.runInContext(code, sandbox, { filename: scriptPath });
await settle();

// 1. 页面不能把错误吞进状态条——这正是「设置页加载失败：TypeError」的落点。
const status = registry.get('status');
check(
  status.dataset.error !== 'true' && status.hidden !== false,
  `状态条报了错（页面抛异常了）：${status.textContent}`,
);

// 2. 宠物列表要真的渲染出来（含坏包）。
const list = registry.get('pets');
check(list.children.length === 2, `宠物列表应有 2 项，实际 ${list.children.length} 项`);
const [good, broken] = list.children;
check(good?.classList.contains('broken') !== true, '正常包不该被标成坏包');
check(broken?.classList.contains('broken') === true, '坏包应被标成 broken');
check(good?.textContent.includes('巡检喵'), `正常包应显示 displayName，实际「${good?.textContent}」`);
check(
  broken?.textContent.includes('图集缺 spritesheet 字段'),
  `坏包应显示坏在哪，实际「${broken?.textContent}」`,
);

// 3. 预览要用 camelCase 的 spritesheetPath，并按列数把第一格抠出来。
const preview = good?.children.find((child) => child.classList.contains('preview'));
check(preview !== undefined, '正常包应有首帧预览');
check(
  preview?.style.backgroundImage?.includes('spritesheet.webp'),
  `预览没用上 spritesheetPath：${preview?.style.backgroundImage}`,
);
check(preview?.style.backgroundSize === '800% 1100%', `预览裁切尺寸不对：${preview?.style.backgroundSize}`);

// 4. 表单要按 camelCase 读配置（alwaysOnTop / deviceKey）。
check(registry.get('size').value === '220', `尺寸应回填 220，实际 ${registry.get('size').value}`);
check(registry.get('always-on-top').checked === true, 'alwaysOnTop 没读出来');
check(registry.get('notify-enabled').checked === true, 'notify.enabled 没读出来');
check(registry.get('push-key').value === '', `deviceKey 应回填空串，实际「${registry.get('push-key').value}」`);

// 5. 保存发出去的补丁不能带上这一页不管的顶层键（否则会冲掉窗口位置与端口）。
registry.get('push-endpoint').value = 'https://example.com';
await Promise.all(registry.get('push-endpoint').fire('change'));
await settle();
const patch = sent.findLast((call) => call.method === 'config/set')?.params;
check(patch !== undefined, '改文本框没触发 config/set');
check(patch?.notify?.push?.deviceKey === '', `补丁字段名应为 deviceKey，实际 ${JSON.stringify(patch?.notify?.push)}`);
check(patch?.notify?.push?.provider === 'bark', '补丁丢了 provider，保存会把 provider 打回默认值');
check(patch?.notify?.push?.endpoint === 'https://example.com', '端点没写进补丁');
check(patch?.size === 220, `补丁应带当前尺寸，实际 ${patch?.size}`);
for (const key of ['port', 'pet', 'x', 'y']) {
  check(!(key in (patch ?? {})), `补丁不该带 ${key}——会把它的值冲掉`);
}

// 6. 测试提醒按钮。
await Promise.all(registry.get('test').fire('click'));
await settle();
check(
  sent.some((call) => call.method === 'notify/test'),
  '「测试提醒」没发出 notify/test',
);
check(
  registry.get('test-out').textContent.includes('音效'),
  `测试结果没渲染：${registry.get('test-out').textContent}`,
);

if (failures.length) {
  console.error(`UI 检查未通过（${failures.length} 项）：`);
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  process.exit(1);
}
console.log('UI 检查通过：snake_case lint、CSS 注释与变量、设置页渲染/字段名/补丁范围/测试提醒均正常。');
