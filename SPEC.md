# 跨 Harness 桌宠（litepet）实现规格书

> 版本：v0.1 草案 ｜ 目标平台：macOS（arm64）优先 ｜ 交接文档：执行 agent 从本文档零上下文开工
>
> 本文档分「✅ 已核实事实」与「⚠️ 待执行时核实」两级信息，见 §9。禁止把待核实项当成结论直接实现。

## 0. 背景与目标

为两个开源编程工具各做一个**共享的桌面宠物**：

- **pi**：TUI coding agent（本机已装，文档见 `~/tools/pi-web/node_modules/@earendil-works/pi-coding-agent/docs/`）
- **dsh**：DeepSeek Harness（`github.com/deepseek-ai/deepseek-harness`，MIT，"Everything is a Plugin"，底层 Cordis 框架，文档 https://deepseek-harness.github.io/deepseek-harness/reference/ ）

产品形态：屏幕角落一只**白鼬**，透明置顶小窗播放 **animated WebP** 动画（素材格式决策见 §3.5）。agent 开始干活 → 切「打字」动画；任务完成 → 举杠铃庆祝；工具调用 → 头顶气泡显示工具名。pi 和 dsh 可同时接入，宠物按仲裁规则显示。

参考实现：Open Vetta 仓库（本机 `/Users/eee/tools/open-vetta`）的桌宠。核心算法直接搬运，具体清单见 §6；**素材需转码**（VP9 webm → animated WebP，见 §3.5、§7）。

宠物形象**不写死**：动作词表、素材布局、行为规则全部由宠物包 manifest 声明，契约见 `docs/PET-PACK.md`（§3.8 为要点）。

**硬性设计约束**（不可妥协）：

1. 协议中立：桌宠进程不 import 任何宿主（pi/dsh）的代码，只认一份 JSON-RPC 2.0 协议
2. daemon 单例：多个宿主打同一个 daemon，而不是各开一只宠物
3. 宿主退出即注销：宿主发 `host/bye` 立即注销；进程崩溃则靠心跳超时（60s）回收，不许留僵尸状态
4. 桌宠进程不是沙箱：与 Open Vetta 桌宠同等信任级（本机自用工具，不做安全隔离）

## 1. 架构总览

```text
┌─ pi TUI ───────────────┐                          ┌─ litepet（Tauri App）─────────────┐
│ ~/.pi/agent/extensions/ │  HTTP + JSON-RPC 2.0     │ Rust 侧：                            │
│ pet.ts                  │ ─────┐                   │  - HTTP server（回环 + Bearer 鉴权） │
│  pi.on(...) 事件适配     │      │    协议 v1         │  - 宿主注册表 + 心跳 + 仲裁          │
└─────────────────────────┘      ├─────────────────► │  - 光标轮询 + 点击穿透（照搬 Vetta） │
┌─ dsh ───────────────────┐      │                   │ 前端(WebView)：                       │
│ out-of-tree plugin      │ ─────┘                   │  - animated WebP 帧动画 + 动作状态机  │
│ bundle（Cordis 事件订阅）│                          │  - 气泡（TTL/dedupe/宿主徽章）        │
└─────────────────────────┘                          │  - 拖拽/滚轮缩放                     │
                                                     └──────────────────────────────────────┘
```

三个组件**独立版本化、独立发布**：

| 组件 | 产物 | 归属 |
|---|---|---|
| litepet | Tauri App（.app/.dmg） | **本仓库**（服务端 + 协议契约） |
| pi 适配器 | 单文件 `pet.ts` 放 `~/.pi/agent/extensions/` | **独立定义、独立发版，不在本仓库** |
| dsh 适配器 | dsh plugin bundle | **独立定义、独立发版，不在本仓库** |

**本仓库只交付 daemon 与协议定义**：`docs/PROTOCOL.md` 是唯一契约。宿主侧适配器由宿主方各自实现，本仓库不含参考实现。协议中立（§0 约束 1）既是架构约束，也是交付边界。

> 仓库内唯一像「发送端」的东西是 `scripts/host-sim.mjs`：它是**协议压测/回归工具**，不是发给任何宿主用的适配器，也不属于适配器范敵。

## 2. 协议规范 v1（先定稿再写码）

**传输**：HTTP/1.1 over **回环地址**，报文为 **JSON-RPC 2.0**。`POST http://127.0.0.1:<port>/rpc`，`Authorization: Bearer <token>`。只绑回环，绝不出现在局域网上。

**端点发现**：daemon 绑定成功后写出 `~/.litepet/daemon.json`（权限 `0600`，与 `config.json`／`pets/` 同处家目录，受 `LITEPET_HOME` 控制），内容是 `{"protocolVersion": 1, "port": <端口>, "token": "<32 位十六进制>"}`，退出时删除。宿主每次启动重新读它，不缓存。

**单例锁**：端口被占用即视为已有实例在跑，新进程直接退出。bind 是原子的，比旧的 socket 文件探测更可靠。

> **为什么不用 Unix socket**（M0–M1 曾用 socket + JSONL）：宿主侧零依赖（`fetch` 即可），没有「连接」这层状态——不存在半开连接与 EOF/ECONNRESET 区分，宿主崩溃天然无副作用，也不需要指数退避重连；并且可以直接用 `curl` 手工验证。代价是每个事件一次回环往返（几十微秒），且 daemon 不能主动推消息（只能应答）。

> **协议性质**：这不是纯 RPC，而是「单向事件流 + 少量查询」。只有 `host/hello`、`daemon/ping`、`daemon/info` 是请求-应答；**其余全部是通知（不带 `id`），daemon 只回 HTTP 204，宿主不得等待响应**。宿主死活检测靠心跳超时与显式 `host/bye`，而非连接断开。实现时严禁把 `agent/start`/`tool/start` 等做成同步等待回包的调用。

**消息信封**：标准 JSON-RPC 2.0——`{ "jsonrpc": "2.0", "method": "<名字>", "params": {...}, "id": <有则是请求> }`。未知 `method` 出现在通知里必须静默忽略（前向兼容），出现在请求里回 `-32601`。自定义错误码：`-32001` 宿主未注册、`-32002` 协议版本过高。

### 2.1 宿主 → daemon

| method | 参数 | 语义 |
|---|---|---|
| `host/hello` | `protocolVersion`, `pid?`, `agentVersion?`, `clientVersion?` | 注册宿主（**请求**）；daemon 应答 `{daemonVersion, petId, protocolVersion}` |
| `host/bye` | `reason?` | 主动注销，立即生效不等超时 |
| `agent/start` | `sessionId?`, `summary?` | 该宿主的 agent 开始干活 |
| `agent/end` | `sessionId?`, `success: bool` | 该宿主的 agent 结束 |
| `tool/start` | `toolName`, `bubble?` | 工具开始；`bubble` 可选文本（建议 ≤48 字符，超长由 daemon 截断） |
| `tool/end` | `toolName`, `isError?` | 工具结束 |
| `pet/bubble` | `kind: "info"\|"status"\|"tool"\|"success"\|"warning"\|"error"`, `text`, `ttlMs?` | 直接发一条气泡 |
| `daemon/ping` | `ts` | 心跳（**请求**），应答原样回显 `ts` |
| `daemon/info` | — | 查 daemon 现状（**请求**，调试与适配器自检用） |

### 2.2 daemon → 宿主

只应答请求，**没有主动推送**（`host.welcome`／`pong`／`host.evicted` 三种旧推送报文已废弃）。

| method | `result` 字段 | 语义 |
|---|---|---|
| `host/hello` | `daemonVersion`, `petId`, `protocolVersion` | 注册确认 |
| `daemon/ping` | `host`, `protocolVersion`, `ts` | 心跳回应 |
| `daemon/info` | `daemonVersion`, `protocolVersion`, `petId`, `resident`, `hostCount`, `pingIntervalMs`, `hostTimeoutMs` | daemon 现状 |

### 2.3 仲裁规则（M1 实现最小版，M3 打磨）

- 维护**每宿主状态机**：`idle → working → (agent.end) → celebrating(≤8s) → idle`
- 显示优先级：最近一次事件的宿主优先（last-event-wins）
- 聚合降级：若两个宿主都 working，宠物保持「打字」动画，气泡轮换显示最近工具调用，气泡文本带宿主徽章前缀（`[pi]` / `[dsh]`）
- 气泡规则（照搬 Vetta）：`ttlMs` 默认 4s；`dedupeKey`（tool.start 气泡用 toolName 做去重键，避免连发）；正文截断 48 字符、详情截断 120 字符
- 心跳：宿主每 20s 一 ping，60s（3 次）未收到判死并注销；收到 `host/bye` 立即注销

### 2.4 生命周期

- daemon 启动即绑回环端口；端口被占用直接退出（单例）；绑定成功后写 `daemon.json`，退出时删除
- 所有宿主注销 → linger 30 秒（可配 `--resident` 常驻）→ 退出
- 宿主侧原则：进程启动时读 `daemon.json` + `host/hello`；退出时尽量发一条 `host/bye`，不发也只是等 60s 心跳超时

## 3. 组件 A：litepet（Tauri）

### 3.1 环境准备（⚠️ 本机未装 Rust，需先装）

```bash
# 本机环境：macOS arm64，Node 24（nvm），包管理 pnpm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install tauri-cli --version "^2"
pnpm create tauri-app   # 选 Vanilla TS 或 React（建议 React，Vetta 前端就是 React 可参考）
```

⚠️ Tauri 2.x 具体窗口/事件 API 以官方文档为准（https://tauri.app/），下述映射按 Tauri 2 语义描述，写码时逐一核对。

### 3.2 窗口形态（对应 Vetta `apps/desktop/src/main/pet-window.ts:499`）

| Vetta (Electron) | Tauri 等价物 |
|---|---|
| `frame: false` | `decorations: false` |
| `transparent: true` | `transparent: true`（macOS 需 `macOSPrivateApi: true`） |
| `skipTaskbar: true` | `skipTaskbar: true` |
| `hasShadow: false` | `shadow: false` |
| `alwaysOnTop: true, level "screen-saver"` | `alwaysOnTop: true`（macOS level 用 `setAlwaysOnTop` 对应 NSFloatingWindowLevel，⚠️ 核对 tauri 的 window level API） |
| `resizable: false` | `resizable: false` |

### 3.3 点击穿透（核心算法，照搬 Vetta）

**关键教训（Vetta `pet-window.ts:761` 原注释）**：窗口内透明像素区域**不要**使用「鼠标事件转发」类方案（Electron 的 `{forward:true}`）——转发会把 mousemove 送进宠物页面，CSS cursor 会在透明空隙上抢占下层应用的光标。**正确做法：窗口层做鼠标穿透，由 daemon 侧轮询全局光标位置 + 宠物 hitbox 矩形做命中检测，命中时临时关闭穿透。**

搬运清单：

- `apps/desktop/src/main/pet/pet-mouse-poll.ts`：轮询节奏常量与判定函数
  - 近距离/拖拽中：`PET_MOUSE_POLL_NEAR_MS = 50` ms
  - 远距离：`PET_MOUSE_POLL_FAR_MS = 100` ms
  - 「近」= 光标落在窗口外扩 `PET_MOUSE_POLL_PROXIMITY_PX = 240` px 的矩形内（`isPointNearRect`）
  - `nextPetMousePollMs({dragging, cursor, windowBounds})` 决定下轮间隔
- 命中判定：光标 ∈ 宠物 hitbox 矩形（前端把宠物图片元素在屏幕坐标系的位置上报给 Rust 侧）→ 穿透关；否则穿透开
- **已知局限（不是 bug，是继承来的设计）**：命中检测是**包围盒**而非 alpha 轮廓——点到宠物旁边的透明空白角落也算命中。Vetta（`petVideoHitbox`）和 PiDeck（`isPointOverElement(videoRef.current, …)`）都是这么做的，**所以从视频改到图片不会让点击精度变差**；要改成像素级得额外做 alpha 采样，列为 M5 可选打磨项
- macOS 光标位置获取：⚠️ 核对 Rust crate（`enigo` 的 mouse location 或 `core-graphics` CGEventSource）；Electron 用 `screen.getCursorScreenPoint()`，无权限要求，预期 Rust 同源 API 也无障碍，实测为准
- 穿透开关：Tauri `set_ignore_cursor_events(true/false)`（⚠️ 核对 API 名）

### 3.4 动作状态机（照搬 Vetta 数据模型）

动作定义模型（源：`apps/desktop/src/shared/pet-actions.ts`）：

```ts
interface PetAction {
  id: string;                    // 由宠物包 manifest 声明（§3.8 / `docs/PET-PACK.md`）；内置包恰好与文件名一致：`${id}.webp`
  groupId: "idle" | "working" | "resting" | "feedback";
  label: string;                 // 中文
  videoBaseSize: number;         // 基准显示尺寸 px
  autoDuration: { minMs: number; maxMs: number };  // 自动模式持续时间范围
}
```

动作清单与默认映射（源：`pet-actions.ts` + `session-event-action-policy.ts` 的 `DEFAULT_ACTION_BY_GROUP`）：

> ⚠️ **本节表格已降级为「内置包示例」。** 现采用 **Codex 包格式**（`docs/PET-PACK.md` §3）：动画名由 `pet.json` 的 `animations` 声明、**与文件名解耦**，素材是**单张图集**而非逐动作文件。下表内置包恰好满足「动画名 = 素材文件名」，属巧合而非契约。

| 组 | 动画名 | 默认 | autoDuration |
|---|---|---|---|
| idle | `stoat_spin_color_hula_hoop` | ✅ | 60–120s |
| working | `stoat_work_laptop_typing_desk_cushion` | ✅ | 180–300s |
| resting | `stoat_sit_cushion_drink_tea_slow` | ✅ | 120–240s |
| resting | `stoat_sleep_lie_on_cushion` | | 180–300s |
| resting | `stoat_listen_music_headphones_nod` | | 60–120s |
| resting | `stoat_skip_rope_jump` | | 30–60s |
| feedback | `stoat_stand_lift_barbell_one_hand_fast` | ✅ | 8–12s |
| feedback | `stoat_wave_backflip_smoke_fade_exit` | | 3–5s |

> 注 1：上表是**内置包**的动作清单，作为契约的示例数据，**不是不可扩展的固定词表**——外部包可声明任意动作 id（§3.8）。
> 注 2：上表 autoDuration 数值为设计建议值，Vetta 源码里只对部分动作定义了精确范围；以搬运源码时的实际值为准。

状态机规则（源：`session-event-action-policy.ts` + `PetApp.tsx`）：

- 协议事件 → 动作组：`agent.start`→working；`agent.end`(success)→feedback(举杠铃)+success 气泡；`agent.end`(fail)→feedback(挥手退场)+info 气泡；长时间无事件→resting 轮换
- `tool.start` → 保持当前动作 + 工具气泡（dedupe/截断规则见 §2.3）
- 用户手动切动作（右键菜单）→ 保持 10 秒后交还自动模式（`USER_ACTION_HOLD_MS = 10_000`）
- 前端展示节流：动作切换最小保持间隔（Vetta `usePetPresentationThrottle`，`PET_APP_PRESENTATION_MIN_HOLD_MS`），防止事件风暴导致动画狂跳

### 3.5 素材格式与播放（✅ 本机实测，非查文献）

> **本节实测结论仍有效，但产物形态已变。** 本节证明的是「WebP 编解码路线可行、webm 不可行」；在新格式下最终产物是**单张图集** `spritesheet.webp`（打包见 `docs/PET-PACK.md` §5），播放方式从 `<img src=...>` 改为**按 `frames` 索引切图**。`<video>` 依然不能用。

**结论：用 animated WebP，不用 webm。**

Vetta 原素材是 8 个 VP9 + alpha 的 webm（302 帧 / 10.066s / 30fps / 576×576）。VP9 要带透明，做法是在一个 webm 里塞**两路流**（彩色 + alpha 灰度），文件头标 `ALPHA_MODE=1`，播放器必须同时解两路再逐像素合成。**Chromium 做了，WebKit 没做**——只解彩色那路，alpha 整条丢弃，透明区变成不透明。

**实测方式**：Swift + WKWebView 测试台，红底页面里三个素材并排，`takeSnapshot` 后逐像素统计。

| 素材 | 透出红底比例 | 左上角采样 RGBA | 结论 |
|---|---|---|---|
| static PNG (RGBA) — **对照组** | 78% | `(255,0,0,255)` | ✅ 测试台有效 |
| **animated WebP (alpha)** | **76%** | `(255,0,0,255)` | ✅ 透明正常 |
| **VP9 webm `<video>`** | **0%** | **`(0,0,0,255)` 纯黑** | ❌ 确认不透明 |

对照组的意义：PNG 要是也不透，说明错的是测试台而不是 VP9；PNG 透了 → 那 0% + 纯黑是真结论。
同类 bug 的修复至今未合：`WebKit/WebKit#64837 "VP9 with transparency plays back incorrectly"`（+608/-17，动 39 个文件）。

- **播放方式**：**按 `frames` 索引切图集**（`background-position` 或 canvas，运行时模型见 `docs/PET-PACK.md` §3.5）。**不用 `<video>`**——WebP 不是视频容器。这一条直接消掉了原计划里的 codec 探测。（原方案「`<img>` 直接播 animated WebP」因改用图集而作废）
- **转码工具链**：本机 `ffmpeg 8.1.1` **没有 webp 编码器**（`-encoders | grep webp` 为空），但 brew 装了完整 libwebp：`cwebp` / `img2webp` / `webpmux` / `dwebp` 在 `/opt/homebrew/bin`。用 `img2webp` 转。⚠️ 脚本尚未入库（见 §7 的缺口说明）
- **省电**：锁屏/休眠暂停渲染（Vetta `pet-idle-guard.ts` 用 Electron powerMonitor；Tauri 侧 ⚠️ 核对：监听系统睡眠/唤醒事件的插件，或用 `tauri-plugin-window-state` 类生态，实在没有可先跳过并标 TODO）
- **素材加载失败降级**：隐藏宠物面，仅保留气泡（Vetta `failedVideoSrc` 逻辑）

**体积实测（8 个动作合计）**：

| 方案 | 体积 | 备注 |
|---|---|---|
| 原 webm (VP9+alpha) | 4.98 MB | 最小，但 WKWebView 播不了 |
| animated WebP — 200px/10fps/q80 | 3.81 MB | 比原视频还小 |
| animated WebP — 288px/15fps/q80 | 9.35 MB | |
| **animated WebP — 440px/12fps/q80** | **~12 MB** | 匹配 Retina 2x 的 220px 显示；曾是仓库采用的形态（`assets/pet-440/`，**现已移出仓库**） |
| PNG 精灵图 288px/15fps | ~26 MB | |
| APNG 288px/15fps | ~51 MB | 逐帧全量 |
| 逐帧 PNG 序列 288px | ~107 MB | 排除 |

> ⚠️ **素材缺口（M1 阻塞）**：`assets/` 已从工作区移除（8 个 webm 在 git 中标记为删除，WebP 产物与转码脚本移至 `/tmp/removed-vetta-assets/`）。**内置包目前无素材可打图集。** 需先定素材来源（重新引入这批素材并打图集，或换 CC0 素材），见 `docs/PET-PACK.md` §5 / §8。

关于 animated WebP 的硬验证（供后续怀疑时复现）：动画确实在跑（0.5/1.5/2.5/3.5s 四点截图，相邻帧 34%–38% 像素在变）；结构正确（`webpinfo` 显示 `Alpha: 1, Animation: 1`，每帧带独立 `Chunk ALPH`）；alpha 是真数据（`webpmux -get frame 40` → `dwebp` 解出 206x225 with alpha）。
⚠️ 坑：`dwebp` 不支持 animated webp（官方提示 `Animated WebP files are not supported`），取帧要用 `webpmux -get frame`。

### 3.6 交互（M3）

- 拖拽移动、四角 resize、滚轮缩放（Vetta：`pet-widget-bounds.ts`、`resizePetVideoByWheel`）
- 右键菜单：动作切换 / 置顶开关 / 退出
- 位置与大小持久化：daemon 侧 JSON 配置文件（**`~/.litepet/config.json`**，家目录约定见 `docs/PET-PACK.md` §2）

### 3.7 气泡（M5 打磨项）

- 数据驱动换肤：Vetta 的气泡样式是 JSON（`apps/desktop/src/shared/pet-bubble-styles/*.json`，`surface` 用 Tailwind 类名 + `decor.corners` 四角 PNG）。MVP 先做 plain 样式，换肤 JSON 格式照搬，节日皮肤后续加
- 气泡位置：默认在宠物上方；宠物贴屏幕顶边时翻到下方（Vetta `bubblePlacement` 逻辑）

### 3.8 宠物包与可扩展性

动作词表、素材布局、行为规则全部由**宠物包 manifest** 声明，不在代码里写死。完整契约见 **`docs/PET-PACK.md`**（v0.2，采用 **Codex 包格式**；§2 目录约定、§3 规格已定稿，§5 图集单格尺寸、§4.4 降级映射待定）。本节只列要点。

三层模型：

| 层 | 含义 | v1 |
|---|---|---|
| L1 换美术 | 换掉整套素材，动作集合不变 | ✅ |
| L2 加动作 | manifest 声明任意动作 id，不限词表 | ✅ |
| L3 改行为 | 声明「什么事件 → 播什么」，Rust 侧只做通用解释器 | ✅ |

L3 是唯一能把 §3.4 「策略逻辑要手写 Rust」这条成本压下去的办法：把 Vetta `session-event-action-policy.ts`（315 行）那类逻辑变成 manifest 里的规则表。

**硬性要求：加载期校验，声明了但不可达的动作 id 必须拒绝加载。**

依据：PetPal Desktop 的格式允许任意动作名，但运行时只播硬编码子集——演示包声明了 `sit: [9,12]`、目录里有 `actions/look_right/`，而在 `src/main.js` 里按名统计 `sit`、`look` 各为 **0 次命中**，永远不会被触发；`anchor`、`sounds`、`portrait` 同样 0 次命中，`personality.catchphrases` 只写不读。**死 schema 比直接不支持更糟**，因为用户会以为能用。详见 `docs/PET-PACK.md` §0.1。

## 4. 宿主适配器的职责边界

适配器**独立定义、独立发版，不在本仓库**。但职责边界必须写死，否则「统一」会退化成「每个宿主各写一套语义」。

**适配器负责**（宿主私有知识，daemon 不该知道）：

1. **事件订阅**——把宿主 API（如 `pi.on(...)`）接到协议方法上
2. **载荷归一化**——宿主私有枚举/结构 → 协议中立的值。例：pi 的 `stopReason: "error"|"aborted"` → `success: false`；pi 的 `bash` 工具参数 `{command}` → `bubble` 短文本
3. 心跳、宿主标识、`host/hello` / `host/bye` 生命周期
4. 容错：超时、吞异常、daemon 不在时静默降级（**桌宠的问题不能变成宿主的问题**）

**适配器不负责**（daemon 独占）：

1. 播什么动画、播多久、动画优先级
2. 气泡着色、优先级、截断到显示宽度
3. 多宿主仲裁与徽章

**边界判据**（三条都过才算划对）：

| 变更 | 只应影响 |
|---|---|
| 宿主改了事件 payload 结构 | 适配器 |
| 重新设计动画集 | daemon |
| dsh 的工具参数 schema 与 pi 不同 | 适配器 |

> **反面案例**：若适配器把整个工具参数对象原样转发、由 daemon 去认 `bash.command`，那 daemon 就被迫理解 pi 的工具 schema。
> 这不是「中立」，而是把耦合翻了个方向。所以「从参数里提取气泡文本」看似是展示逻辑，实际必须留在适配器里。

**协议为独立适配器提供的保证**（daemon 侧已实现并有测试）：

- 未知 `method`：通知静默忽略；请求回 `-32601`
- 未知 `params` 字段：忽略，不报错（无 `deny_unknown_fields`）
- 未知 `bubble.kind`：**降级显示**而不拒绝（`BubbleKind::Unknown`，优先级最低）
  注：同样的未知 `kind` 写在**包配置** `litepet.behavior` 里则**加载期硬拒**——线格式是别人的新版本，包配置是自己的声明，拼错不能静默
- 参数真缺必需字段/类型错：回 `-32602`；若为通知则只记 daemon 日志

## 5. dsh 适配器（同样独立定义）

**✅ 已核实**：dsh = deepseek-ai/deepseek-harness，MIT，Cordis 框架「一切皆插件」（类型化事件贡献到共享上下文），out-of-tree bundle 是官方认可的扩展形态（先例：dsh-tui、DSH-Code、codsh 均为社区 bundle）。

**⚠️ 待执行时核实（写适配器前必做）**：

1. agent loop 插件对外发布的**具体事件名与 payload**——读 https://deepseek-harness.github.io/deepseek-harness/reference/ 与仓库源码，把「agent 开始/结束、工具开始/结束」映射到协议 v1，找不到完全对应的事件就映射最接近的并在适配器 README 说明差异
2. out-of-tree bundle 的**标准目录结构与打包方式**——参考 dsh-tui 的仓库结构
3. 插件生命周期是否跟随会话（若是，心跳/注销策略与 pi 适配器同形：会话起连、会话断开）

约束：同一份协议（`docs/PROTOCOL.md`）、同一套 §4 职责边界，仅事件订阅 API 不同。

## 6. Open Vetta 搬运清单（精确路径）

| 源文件（`/Users/eee/tools/open-vetta/` 下） | 搬什么 |
|---|---|
| `apps/desktop/src/main/pet/pet-mouse-poll.ts` | 轮询常量、`isPointNearRect`、`nextPetMousePollMs`（可直译成 Rust） |
| `apps/desktop/src/main/pet-window.ts:761-767` | 穿透方案与教训注释（禁止 forward 转发） |
| `apps/desktop/src/main/pet/session-event-action-policy.ts` | 事件→动作映射、气泡 TTL/dedupe/截断常量 |
| `apps/desktop/src/main/pet/pet-idle-guard.ts` | 锁屏/休眠暂停恢复播放的思路 |
| `apps/desktop/src/main/pet/pet-playback-policy.ts` | 播放意图合并策略 |
| `apps/desktop/src/shared/pet-actions.ts` | 动作数据模型与清单 |
| `apps/desktop/src/shared/pet-bubble-styles/*.json` | 气泡换肤 JSON 格式（M5） |
| `apps/desktop/src/renderer/domains/pet/` | React 前端参考：`PetApp.tsx`（状态编排）、`PetVideoSurface.tsx`、`PetSpeechBubble.tsx`、`usePetBubble.ts`、`usePetPresentationThrottle.ts` |
| `apps/desktop/build/pet/*.webm` | 8 个白鼬透明视频素材（见 §7 许可）；**需转码为 animated WebP 才能用**（§3.5） |

## 7. 素材与许可

**两套素材并存，用途不同**（均在本仓库）：

| 目录 | 内容 | 体积 | 用途 |
|---|---|---|---|
| `assets/pet/` | 8 个 `.webm`（VP9+alpha，302 帧 / 10.066s / 30fps / 576×576） | 5.0 MB | **仅作再生成源**，不参与运行、不打包 |
| `assets/pet-440/` | 8 个 animated `.webp`（440px/12fps/q80） | 12 MB | **运行用**，`<img>` 直接播放 |

已逐项验过 `assets/pet-440/*.webp` 均为 `Alpha: 1, Animation: 1`。

- 素材来源：Open Vetta 仓库（Apache-2.0），原始 webm 位于 `apps/desktop/build/pet/`
- **必须**：在 litepet 项目中保留 Apache-2.0 要求的版权与许可声明；动手前核对 `/Users/eee/tools/open-vetta/NOTICE` 中是否有针对这批素材的额外归属说明（若 NOTICE 把素材列为第三方，则按 NOTICE 归属，不可直接搬）
- **动作 id 由宠物包 manifest 声明，不得从文件名推断**（§3.8 / `docs/PET-PACK.md` §4）。内置包恰好满足「文件名 = 动作 id」，但外部包允许文件名任意，否则用户无法用自己命名的素材
- ⚠️ **转码脚本缺失，素材当前不可复现**：`assets/pet-440/` 是早期在 `/tmp` 里一次性用 `img2webp` 生成的，`scripts/` 目录**为空**，仓库内无任何转码代码。M1 动手前必须补 `scripts/transcode-pet-assets.sh`（工具链见 §3.5）

## 8. 里程碑与验收

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| M0 | 仓库初始化 + 本文档入库 + 协议 v1 定稿 | `docs/PROTOCOL.md` 与本文 §2 一致；CI（fmt/clippy/build）跑通 |
| M1 | Tauri daemon：透明窗 + animated WebP 播放 + HTTP server + 状态机最小版 | `node scripts/host-sim.mjs` 演完整会话：宠物切打字动画 → 出气泡 → `agent/end` 后举杠铃 → 道别后 linger 30s 退出并删掉 `daemon.json`；二次启动检测单例 |
| M2 | 协议层可被真实宿主驱动（本仓库只做到这一步） | `node scripts/host-sim.mjs` 演完整会话全部通过；“宿主侧适配器”已移出本仓库，另在独立仓库验收（pi：`~/tools/pi-pet-adapter`） |
| M3 | 点击穿透 + 拖拽/缩放/右键菜单 + 位置持久化 | 宠物不挡下层点击；点中宠物可拖可缩；重启后位置保留 |
| M4 | dsh 适配器（独立仓库） | 与 M2 同标准在 dsh 上验收；pi+dsh 同时跑时仲裁与徽章正确 |
| M5 | 打磨：省电、气泡换肤 JSON、`--resident`、登录自启、dmg 打包 | 手工清单逐项过 |

每完成一个里程碑：git commit（中文 commit message）+ 在本文档勾选状态。

## 9. 事实分级（执行 agent 必读）

**✅ 已核实（可直接依赖）**：

1. pi 扩展机制、事件名、扩展位置、完整系统权限（pi 官方文档 extensions.md）
2. Vetta 桌宠全部实现细节与文件路径（§3、§6 所列，均出自源码勘察）
3. dsh 的定位、Cordis 架构、out-of-tree bundle 生态（官方仓库与文档）
4. Open Vetta 仓库 Apache-2.0
5. **animated WebP 在 WKWebView 下 alpha 正常**，VP9+alpha 的 webm 在 WKWebView 下完全不透明（§3.5，本机 Swift + WKWebView 像素级实测，含 PNG 对照组）
6. 三家同类实现的扩展性边界（codex-pet-desktop / PiDeck / PetPal Desktop 的扩展能力与死 schema 清单，`docs/PET-PACK.md` §0.1）

**⚠️ 待执行时核实（先查证再写码，查不到就按保守方案并标注）**：

1. Tauri 2.x 各 API 确切签名（窗口透明需要 `macOSPrivateApi`、always-on-top level、`set_ignore_cursor_events`、光标位置获取 crate）
2. Rust 侧获取全局光标位置在 macOS 上是否需要辅助功能权限（预期不需要，实测为准）
3. dsh 事件名清单与 bundle 结构（§5）
4. Tauri 生态的系统睡眠/唤醒监听方案
5. `apps/desktop/build/pet/*.webm` 素材在 NOTICE 中有无特殊归属（§7）
6. Vetta 各动作的精确 autoDuration（§3.4 表以源码为准）
7. **目标机型上的 animated WebP 透明需复测**：本机 macOS 上验证通过，但 `PaperMC/fill-ui#14` 有 2025-11-14 在 macOS Safari 26.2 上的最小复现结论为「Safari macOS does not support webp transparency」，与本机实测**相反**。可能这一年修了，也可能编码参数不同（本机用 `img2webp -lossy -q 80`，每帧带独立 ALPH chunk）。以实测为准，但发布前按目标机型重测

## 10. 环境备忘

- macOS (darwin/arm64)，zsh；Node 24（nvm），包管理 pnpm；**Rust 未装**（M0 第一步装）
- pi 文档：`~/tools/pi-web/node_modules/@earendil-works/pi-coding-agent/docs/`（重点 extensions.md、tui.md）
- Open Vetta 源码：`/Users/eee/tools/open-vetta`（只读参考，**不得修改该仓库**）
- dsh 文档：https://deepseek-harness.github.io/deepseek-harness/reference/ ；仓库 https://github.com/deepseek-ai/deepseek-harness
- 项目语言：TS（前端）+ Rust（daemon）；注释、文档、commit message 用中文
