# 桌宠协议 v1（pet-daemon）

> 状态：**已定稿**（M0）｜ 实现互操作契约，任何适配器不得偏离本文
>
> 本文与 `SPEC.md` §2 一致。若两者冲突，以本文为准，并回改 SPEC。

## 0. 定位

`pet-daemon` 是一个**单例守护进程**，在屏幕角落显示一只白鼬。

- daemon **不认识**任何宿主（pi / dsh / 其他）的代码，只认本协议的一行一条 JSON
- 多个宿主**连接同一个 daemon**，而不是各开一只宠物
- 宿主进程退出 → 连接断开 → daemon 立即注销该宿主，不留僵尸状态

## 1. 传输

Unix domain socket，`SOCK_STREAM`，UTF-8，**JSONL 帧**（每帧一行，`\n` 结尾）。

| 平台 | socket 路径 |
|---|---|
| macOS | `~/Library/Application Support/pet-daemon/daemon.sock` |
| Linux | `$XDG_RUNTIME_DIR/pet-daemon.sock`，无 `XDG_RUNTIME_DIR` 时回退 `/tmp/pet-daemon-$UID.sock` |
| Windows（M5 后） | 命名管道 `\\.\pipe\pet-daemon` |

**单例锁**：socket 文件被占用（`bind` 报 `EADDRINUSE` 且能连通）即视为 daemon 已运行，新进程直接退出。

## 2. 协议性质（实现前必读）

**这不是纯 RPC。**

- `host.hello` / `ping` 是请求-应答
- **其余全部是单向事件通知：daemon 不回包，宿主不得阻塞等待响应**
- 宿主死活依赖**连接断开**与**心跳超时**判定，不依赖调用超时
- 严禁把 `agent.start` / `tool.start` 之类做成「发出去然后等 ack」的同步调用

## 3. 通用信封

每帧是一个 JSON 对象，至少含：

```json
{ "v": 1, "type": "<类型>", "host": "<pi|dsh|其他>" }
```

- `v`：协议版本，当前固定 `1`。daemon 收到 `v != 1` 的 `host.hello` 回 `host.evicted` 后断开
- `host`：宿主标识，宿主自行声明（推荐 `pi` / `dsh`）；同时接入多个同类型宿主时允许 `pi#2` 之类的后缀
- 未知 `type`：**必须静默忽略**（前向兼容），不得断连、不得报错退出

## 4. 宿主 → daemon

| type | 附加字段 | 语义 |
|---|---|---|
| `host.hello` | `pid: number`, `agentVersion?: string`, `clientVersion?: string` | 注册宿主；daemon 回 `host.welcome`。**连接后第一帧必须是它** |
| `agent.start` | `sessionId?: string`, `summary?: string` | 该宿主的 agent 开始干活 |
| `agent.end` | `sessionId?: string`, `success: boolean` | 该宿主的 agent 结束 |
| `tool.start` | `toolName: string`, `bubble?: string` | 工具开始；`bubble` 为可选展示文本，**建议 ≤ 48 字符**，超长由 daemon 截断 |
| `tool.end` | `toolName: string`, `isError?: boolean` | 工具结束 |
| `bubble` | `kind: "status"\|"success"\|"info"\|"warning"\|"error"`, `text: string`, `ttlMs?: number` | 直接发一条气泡 |
| `ping` | `ts: number`（epoch ms） | 心跳，建议 5s 一次 |

`sessionId` 仅用于日志与去重，daemon 不做多会话区分（每宿主一个状态机）。

## 5. daemon → 宿主

| type | 附加字段 | 语义 |
|---|---|---|
| `host.welcome` | `daemonVersion: string`, `assignedHost: string` | 注册确认；`assignedHost` 为 daemon 最终采用的宿主标识（重名时会被加后缀） |
| `pong` | `ts: number` | 心跳回应，原样回显宿主 `ts` |
| `host.evicted` | `reason: string` | 该宿主被强制注销（如协议版本不兼容），随后 daemon 断开该连接 |

## 6. 仲裁规则

**每宿主状态机**：

```text
idle ──agent.start──► working ──agent.end(success)──► celebrating(≤8s) ──► idle
                          │                                  │
                          └── 长时间无事件 ──► resting 轮换 ◄──┘
```

- `celebrating` 固定最长 8 秒，到点自动回 `idle`
- `tool.start` **不改变** working 状态，只更新气泡
- 长时间（默认 90s）无事件 → 进入 `resting`，在 resting 组动作间轮换

**跨宿主仲裁（last-event-wins）**：

1. 最近一次产生事件的宿主为「当前宿主」，决定宠物动作
2. 若多个宿主同时 working：宠物保持「打字」动画，气泡轮换显示最近工具调用
3. 聚合显示时气泡文本带宿主徽章前缀：`[pi] 读取文件`、`[dsh] bash`
4. 单个宿主时也显示徽章（保持一致，便于确认来源）

**气泡规则**：

- `ttlMs` 默认 `4000`，下限 `500`，上限 `30000`
- 去重键：`tool.start` 的气泡用 `toolName` 作 dedupeKey，同键在 TTL 内不重发
- 正文截断 48 字符，详情截断 120 字符，超出部分以 `…` 结尾
- 气泡优先级：`error` > `warning` > `success` > `tool` > `status` > `info`；低优先级不抢占未过期的更高优先级气泡
- 气泡放置：默认宠物上方；宠物贴屏幕顶边（≤ 8px slack）时翻到下方

## 7. 心跳与注销

- 宿主每 **5s** 发一次 `ping`；daemon 回 `pong`
- daemon **15s**（即 3 次心跳）未收到某宿主任何帧 → 判死并注销该宿主
- 连接断开（`EOF` / `ECONNRESET`）→ **立即**注销，不等心跳超时
- 注销即清空该宿主状态；若它是当前宿主，仲裁回退到次新的活跃宿主

## 8. 生命周期

- daemon 启动即创建 socket；若已存在可连通的 socket，直接退出（单例）
- 所有宿主连接断开 → linger **30 秒**（可配 `--resident` 常驻）→ 退出
- 宿主侧原则：扩展加载时连接 + `host.hello`；宿主进程退出时 socket 自然断开，**无需显式清理**
- 宿主重连：连接失败按指数退避重试，1s 起，上限 30s；连上后重新 `host.hello`

## 9. 动画名与素材格式

宠物包契约与素材格式的权威定义见 `docs/PET-PACK.md`（依据 Codex 官方源码核实）。本节只讲协议层关心的部分。

**素材是单张图集**：`spritesheet.webp`（透明 WebP），配 `pet.json` 里的网格与动画表。**不是 webm** —— VP9+alpha 的 webm 在 WKWebView 下完全不透明（实测依据见 `SPEC.md` §3.5）。

**动画名由 `pet.json` 的 `animations` 显式声明，不得从文件名或行号推断**：

- `animations` 是**具名 map**，键名任意，且允许 `custom:` 前缀扩展
- 每个动画用 `frames: Vec<usize>` 声明**精灵索引**（`row * columns + column`），**不是「帧数」**；帧可不连续、可跨行复用
- 网格（单格尺寸 / 列数 / 行数）由 `frame` 字段自定义，默认 `192×208` / 8 列 / 9 行，**没有「必须 1536×1872」的校验**
- 是否循环看 `loop_start`：`Some(i)` 从第 `i` 帧起循环（**不是从 0**）；`None` 表示一次性，播完交棒 `fallback`

所以内置包与外部包在 daemon 眼里**完全同构**：都是「一个动画名 → 一串带时长的精灵索引」。

### 9.1 动作组

组是**我们**的概念（Codex 格式里没有），定义在 `petdaemon.behavior.groups`（契约见 `docs/PET-PACK.md` §4.2）。组的语义与 `SPEC.md` §3.4 一致，组内多个动画按轮换规则选用。

### 9.2 内置包

下表为**内置包**的动画名（示例数据，**非固定词表**；外部包的动画名由自己的 `pet.json` 决定）：

| 组 | 内置默认动画名 |
|---|---|
| `idle` | `stoat_spin_color_hula_hoop` |
| `working` | `stoat_work_laptop_typing_desk_cushion` |
| `resting` | `stoat_sit_cushion_drink_tea_slow`、`stoat_sleep_lie_on_cushion`、`stoat_listen_music_headphones_nod`、`stoat_skip_rope_jump` |
| `feedback` | `stoat_stand_lift_barbell_one_hand_fast`、`stoat_wave_backflip_smoke_fade_exit` |

内置包恰好满足「动画名 = 素材文件名」，但这是**巧合，不是契约**——外部包的文件名与动画名可以完全无关，否则用户无法用自己命名的素材。

### 9.3 事件 → 动画映射

| 事件 | 播放 | 气泡 |
|---|---|---|
| `agent.start` | `working` 组默认 | `status`：开始工作 |
| `tool.start` | 保持当前 | `tool`：`{toolName}` 或宿主给的 `bubble` 文本 |
| `tool.end`(isError) | `feedback`：`stoat_wave_backflip_smoke_fade_exit` | `error` |
| `agent.end`(success) | `feedback`：`stoat_stand_lift_barbell_one_hand_fast` | `success`：任务完成 |
| `agent.end`(fail) | `feedback`：`stoat_wave_backflip_smoke_fade_exit` | `info`：任务失败 |
| 90s 无事件 | `resting` 组轮换 | — |

> 上表是**内置包** `pet.json` 里 `petdaemon.behavior.rules` 的等价描述（规则表契约见 `docs/PET-PACK.md` §4.3）。外部包在自己 manifest 里覆盖；daemon 侧只做**通用规则解释器**，不把这套映射写死在 Rust 代码里。
>
> 纯 Codex 包（无 `petdaemon` 键）走 `docs/PET-PACK.md` §4.4 的降级映射表。

## 10. 手工验证

```bash
SOCK="$HOME/Library/Application Support/pet-daemon/daemon.sock"
printf '%s\n' '{"v":1,"type":"host.hello","host":"test","pid":0}' | socat - UNIX-CONNECT:"$SOCK"
printf '%s\n' '{"v":1,"type":"agent.start","host":"test"}'      | socat - UNIX-CONNECT:"$SOCK"
printf '%s\n' '{"v":1,"type":"tool.start","host":"test","toolName":"bash","bubble":"跑测试"}' | socat - UNIX-CONNECT:"$SOCK"
printf '%s\n' '{"v":1,"type":"agent.end","host":"test","success":true}' | socat - UNIX-CONNECT:"$SOCK"
```

预期：切「打字」→ 出气泡 → 断开后 `agent.end` 举杠铃 → 连接关闭 → linger 30s 后 daemon 退出。
