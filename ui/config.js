// LitePet 设置页。
//
// 所有数据都经 Tauri 命令 `local_call` 走 daemon 出口（见 src/main.rs），
// 不直接 fetch 那个 HTTP 端口——那样得把 token 交给 webview，而 token 是给
// **外部进程**用的凭证。
//
// 本页刻意不缓存表单状态：每次保存都用 `config/set` 归一化后的返回值回填，
// 因为「daemon 实际接受了什么」才算数（尺寸会被夹进 64..512，音量会被夹进 0..1）。

const { invoke } = window.__TAURI__.core;
const { convertFileSrc } = window.__TAURI__.core;

/** 元素捷径。 */
const el = (id) => document.getElementById(id);

/** 发一次 daemon 级 RPC。 */
function rpc(method, params) {
  return invoke('local_call', { method, params: params ?? null });
}

/** 上一次从 daemon 读到的配置；推送的 provider 要从这里带回去，免得被覆盖掉。 */
let loaded = null;

/** 当前选中的宠物包 id。 */
let currentPet = null;

/** 状态条：显示一条提示，几秒后自动收起。 */
let statusTimer = 0;
function toast(message, isError = false) {
  const node = el('status');
  node.textContent = message;
  node.dataset.error = String(isError);
  node.hidden = false;
  clearTimeout(statusTimer);
  statusTimer = setTimeout(() => {
    node.hidden = true;
  }, isError ? 6000 : 2500);
}

/** 把一份配置填进表单。 */
function fill(config) {
  const notify = config.notify;
  el('size').value = String(config.size);
  el('size-out').textContent = `${config.size}px`;
  el('always-on-top').checked = config.alwaysOnTop;

  el('notify-enabled').checked = notify.enabled;
  el('sound-enabled').checked = notify.sound.enabled;
  el('sound-volume').value = String(notify.sound.volume);
  el('volume-out').textContent = `${Math.round(notify.sound.volume * 100)}%`;
  el('desktop-enabled').checked = notify.desktop.enabled;
  el('push-enabled').checked = notify.push.enabled;
  // 密钥用密码框之外的普通输入框：它多数时候是个本地服务地址，
  // 输错了要能看见，而不是一串圆点。
  el('push-key').value = notify.push.deviceKey;
  el('push-endpoint').value = notify.push.endpoint ?? '';

  const auth = config.auth;
  el('auth-token').value = auth.token;
  el('auth-out').textContent = authStateHint(auth);
}

/**
 * 鉴权状态的说明：**只在出问题时给话**。
 *
 * 只有 token 一个字段，所以只有两种情形需要说话：填了但用不了、填好了。
 * 没填（= 不鉴权）返回空串——“留空就不鉴权”这句由输入框的 placeholder 表达，
 * 状态位再写一遍是多余的。
 * 规则是 `src/config.rs` 里 `Config::auth_gate` 的镜像——那边才是权威，
 * 这里再算一遍是因为「宿主一定连不上」必须在**改完当场**就能看见，
 * 而不是等重启之后去翻日志。改规则时两处一起改。
 */
function authStateHint(auth) {
  if (auth.token === '') {
    return '';
  }
  // 可见 ASCII 且不含空白，与 Rust 侧的 `is_ascii_graphic` 同义。
  if (!/^[\x21-\x7e]+$/.test(auth.token)) {
    return '这个 token 用不了（不能有空格或非 ASCII 字符）：保存并重启后所有 HTTP 请求都会被拒绝，宿主连不上。';
  }
  return '已启用：请求必须带 Authorization: Bearer <token>。改动需重启 litepet 后生效。';
}

/**
 * 收集要写回的配置。
 *
 * **只发这一页管得到的顶层键**：`config/set` 是按补丁合并的，
 * 没提到的 `x`/`y`/`pet`/`port` 会原样保留。要是整份发回去，
 * 窗口位置和端口就会被这里的默认值冲掉。
 */
function collect() {
  return {
    size: Number(el('size').value),
    alwaysOnTop: el('always-on-top').checked,
    notify: {
      enabled: el('notify-enabled').checked,
      sound: {
        enabled: el('sound-enabled').checked,
        volume: Number(el('sound-volume').value),
      },
      desktop: { enabled: el('desktop-enabled').checked },
      push: {
        enabled: el('push-enabled').checked,
        // provider 目前只有 bark，但它是配置里的字段，别在保存时弄丢。
        provider: loaded?.notify?.push?.provider ?? 'bark',
        deviceKey: el('push-key').value.trim(),
        endpoint: el('push-endpoint').value.trim() || null,
      },
    },
    // token 去掉两头空白：带上空白会让它配不上任何请求（Rust 侧把这种值算不可用）。
    // 清空它就是「不鉴权」——不需要另外的开关。
    auth: {
      token: el('auth-token').value.trim(),
    },
  };
}

/** 保存：发给 daemon，并用返回值回填。 */
async function save() {
  try {
    const { config, restartRequired } = await rpc('config/set', collect());
    loaded = config;
    fill(config);
    applyEnabled();
    if (restartRequired.length) {
      toast(`已保存；${restartRequired.join('、')} 需要重启 litepet 后才生效`);
    } else {
      toast('已保存');
    }
  } catch (err) {
    toast(String(err), true);
  }
}

/**
 * 按总开关把从属项灰掉——不然会让人以为它们还在起作用。
 */
function applyEnabled() {
  const notifyOn = el('notify-enabled').checked;
  for (const id of [
    'sound-enabled',
    'sound-volume',
    'desktop-enabled',
    'push-enabled',
    'push-key',
    'push-endpoint',
    'test',
  ]) {
    el(id).disabled = !notifyOn;
  }
}

/** 撑开中间空白的弹性占位。 */
function spacer() {
  const node = document.createElement('span');
  node.className = 'spacer';
  return node;
}

/** 抠出图集的第一格做预览。 */
function makePreview(summary) {
  if (!summary.spritesheetPath || !summary.frame) return null;
  const frame = summary.frame;
  const node = document.createElement('span');
  node.className = 'preview';
  // 整张图集摊开看不出宠物长什么样，所以只取第一格（idle 的首帧）：
  // 背景按「列数 / 行数」等比放大，再对齐到左上角就等于裁出第一格。
  const height = Math.round((40 * frame.height) / frame.width);
  node.style.height = `${height}px`;
  node.style.backgroundImage = `url("${convertFileSrc(summary.spritesheetPath)}")`;
  node.style.backgroundSize = `${frame.columns * 100}% ${frame.rows * 100}%`;
  node.style.backgroundPosition = '0 0';
  return node;
}

/** 渲染宠物列表。 */
function renderPets(pets) {
  const list = el('pets');
  list.replaceChildren();
  if (!pets.length) {
    const empty = document.createElement('li');
    empty.className = 'broken';
    empty.textContent = '一个包都没有。放一个含 pet.json 的目录进宠物目录即可。';
    list.append(empty);
    return;
  }
  for (const summary of pets) {
    const item = document.createElement('li');
    item.setAttribute('aria-current', String(summary.id === currentPet));
    const preview = makePreview(summary);
    if (preview) item.append(preview);

    const text = document.createElement('span');
    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = summary.displayName;
    const id = document.createElement('span');
    id.className = 'id';
    id.textContent = ` ${summary.id}`;
    text.append(name, id);
    item.append(text);

    if (summary.problem) {
      // 坏包也列出来，并写明坏在哪：凭空消失只会让人怀疑自己装错了地方。
      item.classList.add('broken');
      const problem = document.createElement('span');
      problem.className = 'problem';
      problem.textContent = summary.problem;
      item.append(spacer(), problem);
    } else {
      item.append(spacer());
      if (summary.id === currentPet) {
        const badge = document.createElement('span');
        badge.className = 'id';
        badge.textContent = '当前';
        item.append(badge);
      }
      item.addEventListener('click', () => selectPet(summary.id));
    }
    list.append(item);
  }
}

/** 切换宠物包：当场生效（daemon 会换规则表并让渲染层重载图集）。 */
async function selectPet(id) {
  try {
    await rpc('pet/select', { id });
    await refreshPets();
    toast(`已切换到 ${id}`);
  } catch (err) {
    toast(String(err), true);
  }
}

/** 重新拉一次宠物列表。 */
async function refreshPets() {
  const { pets, current } = await rpc('pet/list');
  currentPet = current;
  renderPets(pets);
}

/** 测试提醒：走真实提醒那条路，返回哪些通道真的发出去了。 */
async function testAlert() {
  const out = el('test-out');
  out.textContent = '发送中…';
  try {
    const actions = await rpc('notify/test');
    const channels = [];
    if (actions.sound) channels.push(`音效 ${actions.sound}`);
    if (actions.desktop) channels.push('系统通知');
    if (actions.push) channels.push('手机推送');
    out.textContent = channels.length
      ? `已发出：${channels.join('、')}`
      : '一条都没发出去：总开关、通道开关或规则表把这条拦下了';
  } catch (err) {
    out.textContent = String(err);
  }
}

/** 渲染路径信息。 */
function renderPaths(info) {
  const rows = [
    ['家目录', info.home],
    ['宠物目录', info.petsRoot],
    ['当前包', currentPet],
    ['日志', info.log],
    ['版本', info.version],
  ];
  const list = el('paths');
  list.replaceChildren();
  for (const [key, value] of rows) {
    if (!value) continue;
    const dt = document.createElement('dt');
    dt.textContent = key;
    const dd = document.createElement('dd');
    dd.textContent = value;
    list.append(dt, dd);
  }
}

/** 启动：拉配置与宠物列表，挂事件。 */
async function boot() {
  const info = await rpc('config/get');
  loaded = info.config;
  fill(info.config);
  applyEnabled();
  currentPet = null;
  await refreshPets();
  renderPaths(info);
  el('meta').textContent = `v${info.version} · 端口与鉴权改动需重启，其余当场生效`;

  // 滑块拖动时先更新数字，松手（`change`）才写盘，免得拖动过程中刷出一串写入。
  el('size').addEventListener('input', () => {
    el('size-out').textContent = `${el('size').value}px`;
  });
  el('sound-volume').addEventListener('input', () => {
    el('volume-out').textContent = `${Math.round(Number(el('sound-volume').value) * 100)}%`;
  });
  for (const id of ['size', 'sound-volume']) {
    el(id).addEventListener('change', save);
  }
  for (const id of [
    'always-on-top',
    'notify-enabled',
    'sound-enabled',
    'desktop-enabled',
    'push-enabled',
  ]) {
    el(id).addEventListener('change', () => {
      applyEnabled();
      save();
    });
  }
  // 文本框在失焦或回车时才写盘。
  for (const id of ['push-key', 'push-endpoint', 'auth-token']) {
    el(id).addEventListener('change', save);
  }
  el('test').addEventListener('click', testAlert);
}

boot().catch((err) => toast(`设置页加载失败：${err}`, true));
