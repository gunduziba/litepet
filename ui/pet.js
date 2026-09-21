// 桌面宠物渲染层：按播放计划切图集，并渲染 daemon 推来的显示指令。
//
// 数据来源：Rust 侧 `pack_info` 命令（见 src/pack.rs）与 Tauri 事件 `display`
// （载荷为 `protocol::DisplayDirective`）。
// 本层不做任何状态判断：动画名与气泡全由 daemon 决定（docs/PROTOCOL.md）。

const { invoke } = window.__TAURI__.core;
const { convertFileSrc } = window.__TAURI__.core;

/** 当前宠物包信息（含 frame 网格与 animations 播放计划）。 */
let pack = null;
/** 图集的 webview 可加载 URL。 */
let sheetUrl = '';
/** 当前动画名。 */
let currentName = '';
/** 当前帧在 frames 中的下标。 */
let frameIndex = 0;
/** 下一帧的定时器句柄。 */
let timer = 0;

/** 单格适配窗口的缩放比：取宽高两个方向中更受限的那个。 */
function fitScale(width, height) {
  return Math.min(window.innerWidth / width, window.innerHeight / height);
}

/** 把某一帧画到 DOM 上：由 spriteIndex 反推行、列，再移动 background-position。 */
function render(spriteIndex) {
  const grid = pack.frame;
  const el = document.getElementById('pet');
  const row = Math.floor(spriteIndex / grid.columns);
  const col = spriteIndex % grid.columns;
  const scale = fitScale(grid.width, grid.height);
  const cellW = grid.width * scale;
  const cellH = grid.height * scale;

  el.style.backgroundImage = `url("${sheetUrl}")`;
  el.style.backgroundSize = `${grid.columns * cellW}px ${grid.rows * cellH}px`;
  el.style.backgroundPosition = `${-col * cellW}px ${-row * cellH}px`;
}

/** 重绘当前帧（窗口尺寸变化时用）。 */
function redraw() {
  const plan = pack?.animations?.[currentName];
  if (plan) render(plan.frames[frameIndex].spriteIndex);
}

/** 排下一帧。 */
function schedule() {
  clearTimeout(timer);
  const plan = pack.animations[currentName];
  const frame = plan.frames[frameIndex];
  render(frame.spriteIndex);
  timer = setTimeout(() => advance(plan), frame.durationMs);
}

/** 推进一帧；序列走完时按 loopStart 循环，或交棒 fallback。 */
function advance(plan) {
  const next = frameIndex + 1;
  if (next < plan.frames.length) {
    frameIndex = next;
  } else if (plan.loopStart === null) {
    // 一次性动画：播完切走
    play(plan.fallback);
    return;
  } else {
    frameIndex = plan.loopStart;
  }
  schedule();
}

/** 播放指定动画；同名动画重复调用不重头开始，避免状态抖动打断动作。 */
function play(name) {
  const plan = pack?.animations?.[name];
  if (!plan) {
    console.warn(`litepet: 未知动画 ${name}，保持当前动画`);
    return;
  }
  if (currentName === name) return;
  currentName = name;
  frameIndex = 0;
  schedule();
}

/** 渲染一条气泡；`bubble` 为 `null` 时隐藏。 */
function renderBubble(bubble) {
  const el = document.getElementById('bubble');
  if (!bubble) {
    el.hidden = true;
    return;
  }
  el.textContent = bubble.text;
  el.dataset.kind = bubble.kind;
  el.hidden = false;
}

/** 应用一条 DisplayDirective：先切动画，再渲染气泡。 */
function apply(directive) {
  if (!directive) return;
  if (directive.animation) play(directive.animation);
  renderBubble(directive.bubble);
  // 回报给 daemon（仅用于日志验证：webview 的 console 在外面看不到）。
  invoke('renderer_applied', {
    animation: currentName,
    bubble: directive.bubble?.text ?? null,
  }).catch(() => {});
}

/** 订阅 daemon 推来的显示指令。 */
async function watchDisplay() {
  await window.__TAURI__.event.listen('display', (event) => apply(event.payload));
}

/** 启动：拉包信息、进入 idle、挂事件与 resize。 */
async function boot() {
  pack = await invoke('pack_info');
  sheetUrl = convertFileSrc(pack.spritesheetPath);
  document.title = pack.displayName;
  window.addEventListener('resize', redraw);
  // 调试入口：控制台可 __pet.play('waving') / __pet.apply({animation:'idle'})
  window.__pet = {
    play,
    redraw,
    apply,
    get pack() {
      return pack;
    },
  };
  play('idle');
  await invoke('renderer_ready', { animations: Object.keys(pack.animations).length });
  await watchDisplay();
}

boot().catch((err) => console.error('litepet: 渲染层启动失败', err));
