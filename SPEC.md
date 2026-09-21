# 跨 Harness 桌宠（pet-daemon）实现规格书

> 版本：v0.1 草案 ｜ 目标平台：macOS（arm64）优先 ｜ 交接文档：执行 agent 从本文档零上下文开工
>
> 本文档分「✅ 已核实事实」与「⚠️ 待执行时核实」两级信息，见 §9。禁止把待核实项当成结论直接实现。

## 0. 背景与目标

为两个开源编程工具各做一个**共享的桌面宠物**：

- **pi**：TUI coding agent（本机已装，文档见 `~/tools/pi-web/node_modules/@earendil-works/pi-coding-agent/docs/`）
- **dsh**：DeepSeek Harness（`github.com/deepseek-ai/deepseek-harness`，MIT，"Everything is a Plugin"，底层 Cordis 框架，文档 https://deepseek-harness.github.io/deepseek-harness/reference/ ）

产品形态：屏幕角落一只**白鼬**，透明置顶小窗播放 webm 动画。agent 开始干活 → 切「打字」动画；任务完成 → 举杠铃庆祝；工具调用 → 头顶气泡显示工具名。pi 和 dsh 可同时接入，宠物按仲裁规则显示。

参考实现：Open Vetta 仓库（本机 `/Users/eee/tools/open-vetta`）的桌宠。素材与核心算法直接搬运，具体清单见 §6。

**硬性设计约束**（不可妥协）：

1. 协议中立：桌宠进程不 import 任何宿主（pi/dsh）的代码，只认一份 JSONL 协议
2. daemon 单例：多个宿主连接同一个 daemon，而不是各开一只宠物
3. 宿主退出即注销：宿主进程死 → 连接断 → 宠物知道，不许留僵尸状态
4. 桌宠进程不是沙箱：与 Open Vetta 桌宠同等信任级（本机自用工具，不做安全隔离）

## 1. 架构总览

```text
┌─ pi TUI ───────────────┐                          ┌─ pet-daemon（Tauri App）─────────────┐
│ ~/.pi/agent/extensions/ │   Unix domain socket     │ Rust 侧：                            │
│ pet.ts                  │ ─────┐                   │  - JSONL socket server（多客户端）   │
│  pi.on(...) 事件适配     │      │    协议 v1         │  - 宿主注册表 + 心跳 + 仲裁          │
└─────────────────────────┘      ├─────────────────► │  - 光标轮询 + 点击穿透（照搬 Vetta） │
┌─ dsh ───────────────────┐      │                   │ 前端(WebView)：                       │
│ out-of-tree plugin      │ ─────┘                   │  - webm 透明视频播放 + 动作状态机     │
│ bundle（Cordis 事件订阅）│                          │  - 气泡（TTL/dedupe/宿主徽章）        │
└─────────────────────────┘                          │  - 拖拽/滚轮缩放                     │
                                                     └──────────────────────────────────────┘
```

三个组件**独立版本化、独立发布**：

| 组件 | 产物 | 归属 |
|---|---|---|
| pet-daemon | Tauri App（.app/.dmg） | 本仓库主体 |
| pi 适配器 | 单文件 `pet.ts` 放 `~/.pi/agent/extensions/` | 可选交付，也可只给文档 |
| dsh 适配器 | dsh plugin bundle | 可选交付 |

## 2. 协议规范 v1（先定稿再写码）

**传输**：Unix domain socket。路径规则：macOS 用 `~/Library/Application Support/pet-daemon/daemon.sock`，Linux 用 `$XDG_RUNTIME_DIR/pet-daemon.sock`（无 XDG_RUNTIME_DIR 则 `/tmp/pet-daemon-$UID.sock`）。Windows 走命名管道 `\\.\pipe\pet-daemon`（M5 再做）。

**帧**：JSONL，`\n` 分隔，UTF-8。socket 文件被占用即视为 daemon 已运行（天然单例锁）。

> **协议性质**：这不是纯 RPC。`host.hello`/`ping` 是请求-应答模式；**其余全部是单向事件通知，daemon 不回包，宿主不得阻塞等待响应**。宿主死活检测依赖连接断开与心跳超时，而非调用超时。实现时严禁把 `agent.start`/`tool.start` 等做成同步等待回包的 RPC 调用。

**消息通用信封**：`{ "v": 1, "type": "<类型>", "host": "<pi|dsh|其他>", ... }`

### 2.1 宿主 → daemon

| type | 字段 | 语义 |
|---|---|---|
| `host.hello` | `pid`, `agentVersion`, `clientVersion` | 注册宿主；daemon 回 `host.welcome` |
| `agent.start` | `sessionId?`, `summary?` | 该宿主的 agent 开始干活 |
| `agent.end` | `sessionId?`, `success: bool` | 该宿主的 agent 结束 |
| `tool.start` | `toolName`, `bubble?` | 工具开始；`bubble` 可选文本（建议 ≤48 字符，超长由 daemon 截断） |
| `tool.end` | `toolName` | 工具结束 |
| `bubble` | `kind: "status"\|"success"\|"info"`, `text`, `ttlMs?` | 直接发一条气泡 |
| `ping` | `ts` | 心跳 |

### 2.2 daemon → 宿主

| type | 字段 | 语义 |
|---|---|---|
| `host.welcome` | `daemonVersion`, `assignedHost` | 注册确认 |
| `pong` | `ts` | 心跳回应 |
| `host.evicted` | `reason` | 该宿主被强制注销（如协议版本不兼容） |

### 2.3 仲裁规则（M1 实现最小版，M3 打磨）

- 维护**每宿主状态机**：`idle → working → (agent.end) → celebrating(≤8s) → idle`
- 显示优先级：最近一次事件的宿主优先（last-event-wins）
- 聚合降级：若两个宿主都 working，宠物保持「打字」动画，气泡轮换显示最近工具调用，气泡文本带宿主徽章前缀（`[pi]` / `[dsh]`）
- 气泡规则（照搬 Vetta）：`ttlMs` 默认 4s；`dedupeKey`（tool.start 气泡用 toolName 做去重键，避免连发）；正文截断 48 字符、详情截断 120 字符
- 心跳：宿主每 5s 一 ping，3 次未收到判死并注销该宿主；连接断开立即注销

### 2.4 生命周期

- daemon 启动即创建 socket；若已存在且能 `ping` 通，直接退出（单例）
- 所有宿主连接断开 → linger 30 秒（可配 `--resident` 常驻）→ 退出
- 宿主侧原则：扩展加载时连接 + 注册，宿主退出时进程死、socket 自然断，无需显式清理

## 3. 组件 A：pet-daemon（Tauri）

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

**关键教训（Vetta `pet-window.ts:761` 原注释）**：窗口内透明像素区域**不要**使用「鼠标事件转发」类方案（Electron 的 `{forward:true}`）——转发会把 mousemove 送进宠物页面，CSS cursor 会在透明空隙上抢占下层应用的光标。**正确做法：窗口层做鼠标穿透，由 daemon 侧轮询全局光标位置 + 宠物视频 hitbox 矩形做命中检测，命中时临时关闭穿透。**

搬运清单：

- `apps/desktop/src/main/pet/pet-mouse-poll.ts`：轮询节奏常量与判定函数
  - 近距离/拖拽中：`PET_MOUSE_POLL_NEAR_MS = 50` ms
  - 远距离：`PET_MOUSE_POLL_FAR_MS = 100` ms
  - 「近」= 光标落在窗口外扩 `PET_MOUSE_POLL_PROXIMITY_PX = 240` px 的矩形内（`isPointNearRect`）
  - `nextPetMousePollMs({dragging, cursor, windowBounds})` 决定下轮间隔
- 命中判定：光标 ∈ 视频 hitbox 矩形（前端把视频元素在屏幕坐标系的位置上报给 Rust 侧）→ 穿透关；否则穿透开
- macOS 光标位置获取：⚠️ 核对 Rust crate（`enigo` 的 mouse location 或 `core-graphics` CGEventSource）；Electron 用 `screen.getCursorScreenPoint()`，无权限要求，预期 Rust 同源 API 也无障碍，实测为准
- 穿透开关：Tauri `set_ignore_cursor_events(true/false)`（⚠️ 核对 API 名）

### 3.4 动作状态机（照搬 Vetta 数据模型）

动作定义模型（源：`apps/desktop/src/shared/pet-actions.ts`）：

```ts
interface PetAction {
  id: string;                    // 与文件名一致：`${id}.webm`
  groupId: "idle" | "working" | "resting" | "feedback";
  label: string;                 // 中文
  videoBaseSize: number;         // 基准显示尺寸 px
  autoDuration: { minMs: number; maxMs: number };  // 自动模式持续时间范围
}
```

动作清单与默认映射（源：`pet-actions.ts` + `session-event-action-policy.ts` 的 `DEFAULT_ACTION_BY_GROUP`）：

| 组 | 动作 id（即文件名） | 默认 | autoDuration |
|---|---|---|---|
| idle | `stoat_spin_color_hula_hoop` | ✅ | 60–120s |
| working | `stoat_work_laptop_typing_desk_cushion` | ✅ | 180–300s |
| resting | `stoat_sit_cushion_drink_tea_slow` | ✅ | 120–240s |
| resting | `stoat_sleep_lie_on_cushion` | | 180–300s |
| resting | `stoat_listen_music_headphones_nod` | | 60–120s |
| resting | `stoat_skip_rope_jump` | | 30–60s |
| feedback | `stoat_stand_lift_barbell_one_hand_fast` | ✅ | 8–12s |
| feedback | `stoat_wave_backflip_smoke_fade_exit` | | 3–5s |

（注：上表 autoDuration 数值为设计建议值，Vetta 源码里只对部分动作定义了精确范围；以搬运源码时的实际值为准。）

状态机规则（源：`session-event-action-policy.ts` + `PetApp.tsx`）：

- 协议事件 → 动作组：`agent.start`→working；`agent.end`(success)→feedback(举杠铃)+success 气泡；`agent.end`(fail)→feedback(挥手退场)+info 气泡；长时间无事件→resting 轮换
- `tool.start` → 保持当前动作 + 工具气泡（dedupe/截断规则见 §2.3）
- 用户手动切动作（右键菜单）→ 保持 10 秒后交还自动模式（`USER_ACTION_HOLD_MS = 10_000`）
- 前端展示节流：动作切换最小保持间隔（Vetta `usePetPresentationThrottle`，`PET_APP_PRESENTATION_MIN_HOLD_MS`），防止事件风暴导致动画狂跳

### 3.5 视频与省电

- webm 透明视频直接用 HTML `<video>`（Vetta 的 `PetVideoSurface.tsx` 同方案，webm 带 alpha 通道）
- 锁屏/休眠暂停解码（Vetta `pet-idle-guard.ts` 用 Electron powerMonitor；Tauri 侧 ⚠️ 核对：监听系统睡眠/唤醒事件的插件或用 `tauri-plugin-window-state` 类生态，实在没有可先跳过，标注 TODO）
- 视频加载失败降级：隐藏视频面，仅保留气泡（Vetta `failedVideoSrc` 逻辑）

### 3.6 交互（M3）

- 拖拽移动、四角 resize、滚轮缩放（Vetta：`pet-widget-bounds.ts`、`resizePetVideoByWheel`）
- 右键菜单：动作切换 / 置顶开关 / 退出
- 位置与大小持久化：daemon 侧 JSON 配置文件（`~/Library/Application Support/pet-daemon/config.json`）

### 3.7 气泡（M5 打磨项）

- 数据驱动换肤：Vetta 的气泡样式是 JSON（`apps/desktop/src/shared/pet-bubble-styles/*.json`，`surface` 用 Tailwind 类名 + `decor.corners` 四角 PNG）。MVP 先做 plain 样式，换肤 JSON 格式照搬，节日皮肤后续加
- 气泡位置：默认在宠物上方；宠物贴屏幕顶边时翻到下方（Vetta `bubblePlacement` 逻辑）

## 4. 组件 B：pi 扩展适配器

**✅ 已核实**（源：pi 扩展文档 `extensions.md`）：

- 扩展位置：`~/.pi/agent/extensions/pet.ts`（全局自动发现，支持 `/reload` 热重载）
- 扩展是普通 TS 模块，拥有完整系统权限：可直接 `import { net } from "node:net"`、`node:child_process`
- 可用事件（本适配器需要的全部）：
  - `pi.on("session_start")` → 连接 daemon + `host.hello`
  - `pi.on("session_shutdown")` → 关闭连接
  - `pi.on("agent_start")` → `agent.start`
  - `pi.on("agent_end")` → `agent.end`
  - `pi.on("tool_execution_start")` → `tool.start`（事件里取工具名）
  - `pi.on("tool_execution_end")` → `tool.end`
  - 事件详细 payload 结构写码前读 `docs/extensions.md` 的 Events 章节核对
- `pi.registerCommand()` 注册 `/pet` 命令（开关/查看连接状态）
- 心跳：`setInterval` 5s 发 ping，连接断开时指数退避重连（1s 起，上限 30s）

骨架（可直接用）：

```ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createConnection } from "node:net";

export default function (pi: ExtensionAPI) {
  // 状态：连接句柄 + 重连定时器；实现 host.hello / 事件转发 / ping / 重连
  // 细节按本节上文规则补全
}
```

## 5. 组件 C：dsh 插件适配器

**✅ 已核实**：dsh = deepseek-ai/deepseek-harness，MIT，Cordis 框架「一切皆插件」（类型化事件贡献到共享上下文），out-of-tree bundle 是官方认可的扩展形态（先例：dsh-tui、DSH-Code、codsh 均为社区 bundle）。

**⚠️ 待执行时核实（写码前必做）**：

1. agent loop 插件对外发布的**具体事件名与 payload**——读 https://deepseek-harness.github.io/deepseek-harness/reference/ 与仓库源码，把「agent 开始/结束、工具开始/结束」四个事件映射到协议 v1，找不到完全对应的事件就映射最接近的并在 README 说明差异
2. out-of-tree bundle 的**标准目录结构与打包方式**——参考 dsh-tui 的仓库结构
3. dsh 侧配置文件/插件如何常驻后台连接（若插件生命周期跟随会话，则连接管理策略与 pi 适配器相同：会话起连、会话断开）

实现要求与 pi 适配器对齐：同一份协议、同一套心跳/重连/注销规则，仅事件订阅 API 不同。

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
| `apps/desktop/build/pet/*.webm` | 8 个白鼬透明视频素材（见 §7 许可） |

## 7. 素材与许可

- Open Vetta 仓库整体 **Apache-2.0**。白鼬 webm 位于 `apps/desktop/build/pet/`，属于仓库内容，按 Apache-2.0 可复用
- **必须**：在 pet-daemon 项目中保留 Apache-2.0 要求的版权与许可声明；动手前核对 `/Users/eee/tools/open-vetta/NOTICE` 中是否有针对这批素材的额外归属说明（若 NOTICE 把素材列为第三方，则按 NOTICE 归属，不可直接搬）
- 素材拷贝到 pet-daemon 的 `assets/pet/` 目录，动作文件名即动作 id（§3.4 表）

## 8. 里程碑与验收

| 里程碑 | 内容 | 验收标准 |
|---|---|---|
| M0 | 仓库初始化 + 本文档入库 + 协议 v1 定稿 | `docs/PROTOCOL.md` 与本文 §2 一致；CI（fmt/clippy/build）跑通 |
| M1 | Tauri daemon：透明窗 + webm 播放 + socket server + 状态机最小版 | `echo '{"v":1,"type":"agent.start","host":"test"}' \| socat - UNIX-CONNECT:<sock>` 后宠物切打字动画；`agent.end` 后举杠铃；socat 断开 linger 30s 后退出；二次启动检测单例 |
| M2 | pi 适配器 | pi 里跑一次真实任务：agent 起时打字、结束举杠铃、工具调用出气泡；pi 退出后 daemon 注销该宿主；`/pet` 命令可用 |
| M3 | 点击穿透 + 拖拽/缩放/右键菜单 + 位置持久化 | 宠物不挡下层点击；点中宠物可拖可缩；重启后位置保留 |
| M4 | dsh 适配器 | 与 M2 同标准在 dsh 上验收；pi+dsh 同时跑时仲裁与徽章正确 |
| M5 | 打磨：省电、气泡换肤 JSON、`--resident`、登录自启、dmg 打包 | 手工清单逐项过 |

每完成一个里程碑：git commit（中文 commit message）+ 在本文档勾选状态。

## 9. 事实分级（执行 agent 必读）

**✅ 已核实（可直接依赖）**：

1. pi 扩展机制、事件名、扩展位置、完整系统权限（pi 官方文档 extensions.md）
2. Vetta 桌宠全部实现细节与文件路径（§3、§6 所列，均出自源码勘察）
3. dsh 的定位、Cordis 架构、out-of-tree bundle 生态（官方仓库与文档）
4. Open Vetta 仓库 Apache-2.0

**⚠️ 待执行时核实（先查证再写码，查不到就按保守方案并标注）**：

1. Tauri 2.x 各 API 确切签名（窗口透明需要 `macOSPrivateApi`、always-on-top level、`set_ignore_cursor_events`、光标位置获取 crate）
2. Rust 侧获取全局光标位置在 macOS 上是否需要辅助功能权限（预期不需要，实测为准）
3. dsh 事件名清单与 bundle 结构（§5）
4. Tauri 生态的系统睡眠/唤醒监听方案
5. `apps/desktop/build/pet/*.webm` 素材在 NOTICE 中有无特殊归属（§7）
6. Vetta 各动作的精确 autoDuration（§3.4 表以源码为准）

## 10. 环境备忘

- macOS (darwin/arm64)，zsh；Node 24（nvm），包管理 pnpm；**Rust 未装**（M0 第一步装）
- pi 文档：`~/tools/pi-web/node_modules/@earendil-works/pi-coding-agent/docs/`（重点 extensions.md、tui.md）
- Open Vetta 源码：`/Users/eee/tools/open-vetta`（只读参考，**不得修改该仓库**）
- dsh 文档：https://deepseek-harness.github.io/deepseek-harness/reference/ ；仓库 https://github.com/deepseek-ai/deepseek-harness
- 项目语言：TS（前端）+ Rust（daemon）；注释、文档、commit message 用中文
