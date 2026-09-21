#!/usr/bin/env node
// 用真浏览器把设置页渲染出来：既截图，也断言样式真的生效了。
//
// 存在的理由：这个页面在 daemon 里是托盘菜单弹出的窗口，不看一眼只能靠读 CSS 猜。
// 已经因此吃过一次大亏——`config.css` 开头四行 `//` 注释不是合法 CSS，把整个
// `:root` 块吞掉了，于是所有 `var(--x)` 失效：背景变白、说明栏变透明。
// `check-ui.mjs` 只跑 JS，查不出这种事。
//
// 只截图不够：图得靠人看，而「背景是白的」这种问题人不在场就漏过去了。
// 所以让页面把关键元素的 `getComputedStyle` 汇报出来，由脚本断言。
//
// 用法：node scripts/preview-ui.mjs [输出 png]（默认 /tmp/litepet-ui.png）

import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { CONFIG_GET, PET_LIST, respond } from './ui-fixture.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const png = process.argv[2] ?? '/tmp/litepet-ui.png';
const html = readFileSync(join(root, 'ui', 'config.html'), 'utf8');
const css = readFileSync(join(root, 'ui', 'config.css'), 'utf8');
const js = readFileSync(join(root, 'ui', 'config.js'), 'utf8');

/** daemon 窗口有真的 `__TAURI__` 注入，静态页面得自己搭一个。 */
const stub = `
const CONFIG_GET = ${JSON.stringify(CONFIG_GET)};
const PET_LIST = ${JSON.stringify(PET_LIST)};
const respond = ${respond.toString()};
window.__TAURI__ = {
  core: {
    invoke: async (command, a) => respond(a.method, a.params),
    // 让缩略图真的加载出来：Chrome 用 file:// 打开本地图集。
    convertFileSrc: (path) => 'file://' + path,
  },
};
`;

/** 渲染完把计算样式塞进 DOM，好让 `--dump-dom` 带出来。 */
const probe = `
const styleOf = (selector) => {
  const el = document.querySelector(selector);
  if (!el) return null;
  const cs = getComputedStyle(el);
  return { bg: cs.backgroundColor, color: cs.color, border: cs.borderTopColor };
};
window.__probe = () => {
  const pre = document.createElement('pre');
  pre.id = 'probe';
  pre.textContent = JSON.stringify({
    bgVar: getComputedStyle(document.documentElement).getPropertyValue('--bg').trim(),
    body: styleOf('body'),
    header: styleOf('header'),
    section: styleOf('section'),
    hint: styleOf('.hint'),
    pill: styleOf('.pets li'),
    statusError: document.getElementById('status').dataset.error ?? null,
    petRows: document.querySelectorAll('#pets li').length,
    pathRows: document.querySelectorAll('#paths dt').length,
  });
  document.body.appendChild(pre);
};
setTimeout(() => window.__probe(), 800);
`;

// 去掉外链全部内联，页面就能直接丢给 Chrome。
const preview = html
  .replace(/<link rel="stylesheet"[^>]*>/, `<style>\n${css}\n</style>`)
  .replace(/<script src="config\.js"><\/script>/, `<script>\n${stub}\n${js}\n${probe}\n</script>`);

const previewPath = '/tmp/litepet-ui-preview.html';
writeFileSync(previewPath, preview);

const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const common = [
  '--headless=new',
  '--disable-gpu',
  '--hide-scrollbars',
  // 缩略图走 file://，不给这个权限会是空白，容易误判成「预览坏了」。
  '--allow-file-access-from-files',
  // 等异步的 boot() 跑完、缩略图解码完。
  '--virtual-time-budget=3000',
];
execFileSync(
  chrome,
  [...common, '--force-device-scale-factor=2', `--screenshot=${png}`, '--window-size=560,1080', `file://${previewPath}`],
  { stdio: ['ignore', 'ignore', 'ignore'] },
);
// 再跑一次取 DOM（`--dump-dom` 与 `--screenshot` 不能同时用）。
const dom = execFileSync(chrome, [...common, '--dump-dom', `file://${previewPath}`], {
  encoding: 'utf8',
  stdio: ['ignore', 'pipe', 'ignore'],
});
const match = dom.match(/<pre id="probe">([\s\S]*?)<\/pre>/);
if (!match) {
  console.error('渲染后没拿到样式探针——页面可能整个没跑起来。');
  process.exit(1);
}
const probeData = JSON.parse(match[1].replace(/&quot;/g, '"').replace(/&amp;/g, '&'));

const failures = [];
const expect = (actual, want, what) => {
  if (actual !== want) failures.push(`${what}：期望 ${want}，实际 ${actual}`);
};

// 这四条合起来就是「CSS 到底生效了没有」。`:root` 被吞掉时它们全是默认值。
expect(probeData.bgVar, '#1c1c1f', ':root 的 --bg 变量');
expect(probeData.body?.bg, 'rgb(28, 28, 31)', 'body 背景');
expect(probeData.header?.bg, 'rgb(28, 28, 31)', 'header 背景（不能是透明）');
expect(probeData.section?.bg, 'rgb(38, 38, 43)', '分区面板背景');
expect(probeData.hint?.color, 'rgb(154, 154, 166)', '说明文字颜色');
// 页面逻辑本身也不能塌。
expect(probeData.statusError, null, '状态条不该有错误');
expect(probeData.petRows, 2, '宠物条目数');
expect(probeData.pathRows, 5, '路径条目数');

console.log(`已截图：${png}`);
console.log(JSON.stringify(probeData, null, 2));
if (failures.length) {
  console.error(`样式检查未通过（${failures.length} 项）：`);
  for (const failure of failures) console.error(`  ✗ ${failure}`);
  process.exit(1);
}
console.log('样式检查通过：变量已定义，背景与说明栏颜色都落到了真值上。');
