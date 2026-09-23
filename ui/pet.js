// 桌面宠物渲染层：按播放计划切图集，并渲染 daemon 推来的显示指令。
//
// 数据来源：Rust 侧 `pack_info` 命令（见 src/pack.rs）与 Tauri 事件 `display`
// （载荷为 `protocol::DisplayDirective`）、`pet-changed`（载荷为 `pack::PetInfo`）。
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
/** 当前计算出的单格在屏幕上的渲染宽高（换包或 resize 时更新，避免每帧重复重排）。 */
let cellW = 0;
let cellH = 0;
/** 上一次生效的气泡文本与类型（用于 DOM 变更去重）。 */
let lastBubbleText = null;
let lastBubbleKind = null;
/** 上一次回报给 daemon 的动画与气泡（用于 IPC 去重）。 */
let lastAppliedAnimation = '';
let lastAppliedBubble = null;

/** 单格适配窗口的缩放比：取宽高两个方向中更受限的那个。 */
function fitScale(width, height) {
  return Math.min(window.innerWidth / width, window.innerHeight / height);
}

/** 更新图集与视口尺寸：仅在换包或窗口尺寸变化时调用，每帧绝不重复修改这些属性。 */
function updateViewport() {
  if (!pack?.frame) return;
  const grid = pack.frame;
  const el = document.getElementById('pet');
  const scale = fitScale(grid.width, grid.height);
  cellW = grid.width * scale;
  cellH = grid.height * scale;

  el.style.backgroundImage = `url("${sheetUrl}")`;
  el.style.backgroundSize = `${grid.columns * cellW}px ${grid.rows * cellH}px`;
}

/** 把某一帧画到 DOM 上：由 spriteIndex 反推行、列，仅修改 background-position。 */
function render(spriteIndex) {
  if (!pack?.frame) return;
  const grid = pack.frame;
  const el = document.getElementById('pet');
  const row = Math.floor(spriteIndex / grid.columns);
  const col = spriteIndex % grid.columns;

  el.style.backgroundPosition = `${-col * cellW}px ${-row * cellH}px`;
}

/** 重绘当前帧（窗口尺寸变化时用）。 */
function redraw() {
  updateViewport();
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
    if (!el.hidden) el.hidden = true;
    lastBubbleText = null;
    lastBubbleKind = null;
    return;
  }
  // 内容未变时跳过 DOM 属性修改，防止无谓的排版与合成开销
  if (lastBubbleText !== bubble.text || lastBubbleKind !== bubble.kind) {
    el.textContent = bubble.text;
    el.dataset.kind = bubble.kind;
    lastBubbleText = bubble.text;
    lastBubbleKind = bubble.kind;
  }
  if (el.hidden) el.hidden = false;
}

/** 应用一条 DisplayDirective：先切动画，再渲染气泡。 */
function apply(directive) {
  if (!directive) return;
  if (directive.animation) play(directive.animation);
  renderBubble(directive.bubble);

  // 仅在真实状态变更时回报给 daemon，避免重复触发 IPC 与文件同步日志
  const currentBubbleText = directive.bubble?.text ?? null;
  if (currentName !== lastAppliedAnimation || currentBubbleText !== lastAppliedBubble) {
    lastAppliedAnimation = currentName;
    lastAppliedBubble = currentBubbleText;
    invoke('renderer_applied', {
      animation: currentName,
      bubble: currentBubbleText,
    }).catch(() => {});
  }
}

/** 换用一份宠物包信息：更新图集地址与标题，并从 idle 重新开始。 */
function usePack(info) {
  pack = info;
  sheetUrl = convertFileSrc(info.spritesheetPath);
  document.title = info.displayName;
  updateViewport();
  // 必须清掉当前动画名：新包的动画名集合与旧包可能完全不同，
  // 留着旧名字会让 `play` 的同名直返判断把新包的第一帧吞掉。
  currentName = '';
  frameIndex = 0;
  clearTimeout(timer);
  const names = Object.keys(info.animations);
  play(names.includes('idle') ? 'idle' : names[0]);
}

/** 订阅 daemon 推来的事件：显示指令与换包。 */
async function watchEvents() {
  const { event } = window.__TAURI__;
  await event.listen('display', (e) => apply(e.payload));
  await event.listen('pet-changed', (e) => {
    usePack(e.payload);
    invoke('renderer_ready', {
      animations: Object.keys(pack.animations).length,
    }).catch(() => {});
  });
}

/** 启动：拉包信息、进入 idle、挂事件与 resize。 */
async function boot() {
  usePack(await invoke('pack_info'));
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
  await invoke('renderer_ready', {
    animations: Object.keys(pack.animations).length,
  });
  await watchEvents();
}

boot().catch((err) => console.error('litepet: 渲染层启动失败', err));
