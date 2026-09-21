# 桌宠协议 v1（pet-daemon）

> 状态：**已定稿**（M0 定契约，M2 换传输）｜ 实现互操作契约，任何适配器不得偏离本文
>
> 本文与 `SPEC.md` §2 一致。若两者冲突，以本文为准，并回改 SPEC。

## 0. 定位

`pet-daemon` 是一个**单例守护进程**，在屏幕角落显示一只白鼬。

- daemon **不认识**任何宿主（pi / dsh / 其他）的代码，只认本协议
- 多个宿主**打同一个 daemon**，而不是各开一只宠物
- 宿主不再发消息 → 心跳超时 → daemon 注销该宿主，不留僵尸状态

## 1. 传输

**HTTP/1.1 over 回环地址**，报文是 **JSON-RPC 2.0**。

| 项 | 值 |
|---|---|
| 地址 | `http://127.0.0.1:<port>/rpc`（**只绑回环**，绝不出现在局域网上） |
| 方法 | 仅 `POST`；其他方法回 `405` |
| 路径 | 仅 `/rpc`；其他路径回 `404` |
| `Content-Type` | `application/json` |
| 鉴权 | `Authorization: Bearer <token>`，缺失或错误回 `401` |
| 请求体上限 | 64 KiB，超出回 `413` |

token 是启动时随机生成的 32 位十六进制串（128 位熵），与端口一起写在端点文件里。

### 1.1 端点发现

daemon 在**端口绑定成功之后**写出端点文件，宿主读它拿端口与 token：

| 项 | 值 |
|---|---|
| 路径 | `$LITEPET_HOME/daemon.json`，未设时回退 `~/.litepet/daemon.json` |
| 权限 | `0600`（只允许当前用户读，避免同机其他用户拿到 token） |
| 内容 | `{"protocolVersion": 1, "port": 4590, "token": "<32 位十六进制>"}` |

- daemon **正常退出时删除**该文件；进程被 `kill -9` 会留下陈旧文件，宿主应先尝试连接再决定是否重试
- 宿主**不该缓存**端口与 token：每次启动重新读文件
- 文件出现即代表可以连了，宿主侧推荐「轮询文件出现」而不是「盲等固定秒数」

### 1.2 为什么不用 Unix domain socket

早期设计（M0–M1）用 Unix socket + JSONL 帧，M2 改为 HTTP + JSON-RPC：

- **宿主侧零依赖**：pi 用 `fetch`、dsh 用标准库 HTTP 客户端即可，不必引 socket 库、不必自己处理帧边界与粘包
- **没有「连接」这层状态**：不存在「连上了但没注册」「连接半开」「EOF 与 ECONNRESET 区分」这些情形，宿主崩溃天然无副作用，不需要指数退避重连
- **可手工验证**：`curl` 一条命令就能发事件，不必装 socat（见 §10）
- **代价**（已知并接受）：每个事件一次 TCP 往返（回环上约几十微秒，相对 4s 的气泡生命周期可忽略）；daemon 不能主动推消息，只能应答（见 §2）

### 1.3 HTTP 状态码 vs JSON-RPC 错误码

两者是**两个层次**，不要混：

- **HTTP 状态码**表示传输层结果：`200` 请求已处理（**哪怕业务失败**，失败信息在 JSON-RPC 的 `error` 里）、`204` 通知已收到（按协议不回体）、`401/404/405/413` 是传输层拒绝
- **JSON-RPC 错误码**表示业务层结果，永远包在 `200` 响应体里

## 2. 协议性质（实现前必读）

**这不是纯 RPC，而是「单向事件流 + 少量查询」。**

- 大多数方法是**通知**（不带 `id`）：daemon 只回 `204`，宿主不得等待业务响应
- 只有下面 4 个是**请求**（带 `id`）：`host/hello`、`daemon/ping`、`daemon/info`，以及宿主自定义的探测调用
- daemon **不会主动推消息**给宿主：宠物状态由 daemon 独占管理，宿主不需要被回调
- 宿主死活**不靠连接断开**判定（HTTP 上没有长连接），只靠 §7 的心跳超时与显式 `host/bye`
- 严禁把 `agent/start`、`tool/start` 之类做成「发出去然后等 ack」的同步调用——它们连响应体都没有

## 3. JSON-RPC 信封

每一条报文都是 JSON-RPC 2.0 对象：

```json
{ "jsonrpc": "2.0", "method": "tool/start", "params": { "...": "..." }, "id": 7 }
```

- `jsonrpc` 必须是字面量 `"2.0"`
- `method` 是字符串，见 §4
- `params` 是对象（不含 `params` 视为空对象）
- `id`：**有则是请求，无则是通知**。通知永不回响应体
- 未知 `method`：
  - 出现在**通知**里 → **静默忽略**（前向兼容），不得报错退出
  - 出现在**请求**里 → 回 `-32601`

`id` 原样回显，类型不限（数字或字符串）。**`id` 为 `null` 表示无法识别原请求**：只有解析失败（`-32700`）与请求对象非法（`-32600`）两种情形会这样，因为连 `id` 都没解出来。

**不支持批量请求**（JSON-RPC 的顶层数组）：回 `-32600`，`message` 为「不支持批量请求」。宿主一次只发一个对象。

### 3.1 错误码

| 码 | 名 | 何时 |
|---|---|---|
| `-32700` | Parse error | 请求体不是合法 JSON（此时 `id` 必为 `null`） |
| `-32600` | Invalid request | 不是合法 JSON-RPC 2.0 对象（含批量数组请求） |
| `-32601` | Method not found | 请求里的 `method` 不认识 |
| `-32602` | Invalid params | `params` 缺字段或类型不对 |
| `-32603` | Internal error | daemon 内部异常 |
| `-32001` | Host unknown | 宿主未 `host/hello` 就发事件，或已被注销 |
| `-32002` | Version unsupported | `host/hello` 的协议版本高于 daemon 支持的 |

错误响应形如：

```json
{ "jsonrpc": "2.0", "error": { "code": -32601, "message": "未知方法：pet/teleport" }, "id": 3 }
```

`-32002` 额外带 `data.supported`，告知 daemon 支持的最高版本：

```json
{ "jsonrpc":"2.0", "error": { "code": -32002, "message": "...", "data": { "supported": 1 } }, "id": 1 }
```

## 4. 宿主 → daemon

所有 `params` 都要求 `host` 字段（宿主标识，推荐 `pi` / `dsh`；同时接入多个同类宿主时允许 `pi#2` 之类的后缀）。`params` 按 **camelCase** 命名。

| method | 类型 | 参数 | 语义 |
|---|---|---|---|
| `host/hello` | 请求 | `protocolVersion: number`, `pid?: number`, `agentVersion?: string`, `clientVersion?: string` | 注册宿主。**同一宿主的第一个调用必须是它** |
| `host/bye` | 通知 | `reason?: string` | 主动注销；daemon 立即注销该宿主，不等心跳超时 |
| `agent/start` | 通知 | `sessionId?: string`, `summary?: string` | 该宿主的 agent 开始干活 |
| `agent/end` | 通知 | `success: boolean`, `sessionId?: string` | 该宿主的 agent 结束 |
| `tool/start` | 通知 | `toolName: string`, `bubble?: string` | 工具开始；`bubble` 为可选展示文本，**建议 ≤ 48 字符**，超长由 daemon 截断 |
| `tool/end` | 通知 | `toolName: string`, `isError?: boolean` | 工具结束 |
| `pet/bubble` | 通知 | `kind: "info"\|"status"\|"tool"\|"success"\|"warning"\|"error"`, `text: string`, `ttlMs?: number` | 直接发一条气泡 |
| `daemon/ping` | 请求 | `ts: number`（epoch ms） | 心跳，建议 20s 一次 |
| `daemon/info` | 请求 | — | 查询 daemon 现状（调试与适配器自检用） |

- `sessionId` 仅用于日志，daemon 不做多会话区分（每宿主一个状态机）
- 未 `host/hello` 就发事件：通知被静默忽略，请求回 `-32001`
- `protocolVersion` **高于** daemon 支持（当前 `1`）→ `-32002`，该宿主不注册；**低于**则接受（前向兼容）

## 5. daemon → 宿主

daemon 只在应答请求时说话，没有主动推送。

| method | `result` 字段 | 语义 |
|---|---|---|
| `host/hello` | `daemonVersion: string`, `petId: string`, `protocolVersion: number` | 注册确认 |
| `daemon/ping` | `host: string`, `protocolVersion: number`, `ts: number` | 心跳回应，原样回显宿主 `ts` |
| `daemon/info` | `daemonVersion`, `protocolVersion`, `petId`, `resident: boolean`, `hostCount: number`, `pingIntervalMs: number`, `hostTimeoutMs: number` | daemon 现状 |

`daemon/info` 实际响应示例：

```json
{ "jsonrpc":"2.0", "result": {
  "daemonVersion": "0.1.0", "protocolVersion": 1, "petId": "xunjian-miao",
  "resident": false, "hostCount": 1, "pingIntervalMs": 20000, "hostTimeoutMs": 60000
}, "id": 4 }
```

> **旧版本文档里的 `host.welcome` / `pong` / `host.evicted` 三种推送报文已不存在**：前两者变成上面的请求应答，`host.evicted` 的用途由 `host/hello` 回 `-32002` 取代。

## 6. 仲裁规则

**每宿主状态机**：

```text
idle ──agent.start──► working ──agent.end(success)──► celebrating(≤8s) ──► idle
                          │                                  │
                          └── 长时间无事件 ──► resting 轮换 ◄──┘
```

- `celebrating` 固定最长 8 秒，到点自动回 `idle`
- `tool.start` **不改变** working 状态，只更新气泡
- 长时间（默认 90s，可被包的行为配置覆盖）无事件 → 进入 `resting`，在 resting 组动作间轮换

**跨宿主仲裁（last-event-wins）**：

1. 最近一次产生事件的宿主为「当前宿主」，决定宠物动作
2. 若多个宿主同时 working：宠物保持「打字」动画，气泡轮换显示最近工具调用
3. 聚合显示时气泡文本带宿主徽章前缀：`[pi] 读取文件`、`[dsh] bash`
4. 单个宿主时也显示徽章（保持一致，便于确认来源）

**气泡规则**：

- `ttlMs` 默认 `4000`，下限 `500`，上限 `30000`
- 去重键：`tool/start` 的气泡用 `toolName` 作 dedupeKey，同键在 TTL 内不重发
- 正文截断 48 字符，详情截断 120 字符，超出部分以 `…` 结尾
- 气泡优先级：`error` > `warning` > `success` > `tool` > `status` > `info`；低优先级不抢占未过期的更高优先级气泡
- 气泡放置：默认宠物上方；宠物贴屏幕顶边（≤ 8px slack）时翻到下方

## 7. 心跳与注销

| 项 | 值 | 来源 |
|---|---|---|
| 宿主心跳间隔 | 20s | `daemon/info.pingIntervalMs` |
| daemon 判死超时 | 60s | `daemon/info.hostTimeoutMs`，即 3 次心跳未到 |

- 宿主定期发 `daemon/ping`（建议 20s，**不要更稀**，否则会被判死）
- daemon 超过 60s 未收到某宿主任何调用 → 判死并注销
- 收到 `host/bye` → **立即**注销，不等超时
- 注销即清空该宿主状态；若它是当前宿主，仲裁回退到次新的活跃宿主
- 宿主进程崩溃**不会**立刻被注销：HTTP 上没有连接可断，只能等 60s 心跳超时

## 8. 生命周期

- daemon 启动即绑定回环端口；端口被占用视为「已有实例在跑」，新进程直接退出（单例）
- 绑定成功后写 `daemon.json`；退出时删除它
- 所有宿主注销 → linger **30 秒**（可配 `--resident` 常驻）→ 退出
- 宿主侧原则：进程启动时读 `daemon.json` + `host/hello`；退出时尽量发一条 `host/bye`（**发了能让宠物立刻回 idle，不发也只是等 60s 超时**）
- 宿主重启：重新读 `daemon.json`（端口可能变了），重新 `host/hello`

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
| `agent/start` | `working` 组默认 | `status`：开始工作 |
| `tool/start` | 保持当前 | `tool`：`{toolName}` 或宿主给的 `bubble` 文本 |
| `tool/end`(isError) | `feedback`：`stoat_wave_backflip_smoke_fade_exit` | `error`：`{toolName}` |
| `tool/end`（成功） | 保持当前 | **不出气泡**（`toolName` 是去重键，不是展示文本） |
| `agent/end`(success) | `feedback`：`stoat_stand_lift_barbell_one_hand_fast` | `success`：任务完成 |
| `agent/end`(fail) | `feedback`：`stoat_wave_backflip_smoke_fade_exit` | `info`：任务失败 |
| 90s 无事件 | `resting` 组轮换 | — |

> 上表是**内置包** `pet.json` 里 `petdaemon.behavior.rules` 的等价描述（规则表契约见 `docs/PET-PACK.md` §4.3）。外部包在自己 manifest 里覆盖；daemon 侧只做**通用规则解释器**，不把这套映射写死在 Rust 代码里。
>
> 纯 Codex 包（无 `petdaemon` 键）走 `docs/PET-PACK.md` §4.4 的降级映射表。

## 10. 手工验证

**推荐用仓库里的零依赖模拟器**（它自己会等 `daemon.json` 出现再连）：

```bash
node scripts/host-sim.mjs --host pi --step 800     # 演完整会话，每步间隔 800ms
node scripts/host-sim.mjs --step 0                 # 一口气跑完
node scripts/host-sim.mjs --keep-alive             # 演完保持心跳，观察 linger
```

手工注入用 `curl`（token 与端口从端点文件读）：

```bash
ENDPOINT="${LITEPET_HOME:-$HOME/.litepet}/daemon.json"
PORT=$(node -e "console.log(require('$HOME/.litepet/daemon.json').port)")
TOKEN=$(node -e "console.log(require('$HOME/.litepet/daemon.json').token)")
RPC="http://127.0.0.1:$PORT/rpc"

# 先注册（请求，会打印 daemon 的应答）
curl -s -X POST "$RPC" -H "Authorization: Bearer $TOKEN" -d '{
  "jsonrpc":"2.0","method":"host/hello","id":1,
  "params":{"host":"test","protocolVersion":1,"pid":0}
}'

# 再发事件（通知，HTTP 204 无响应体）
curl -s -w "%{http_code}\n" -X POST "$RPC" -H "Authorization: Bearer $TOKEN" -d '{
  "jsonrpc":"2.0","method":"tool/start",
  "params":{"host":"test","toolName":"bash","bubble":"跑测试"}
}'
```

> ⚠️ **每次 `curl` 都是独立请求，但这不是问题**——与旧的 socket 版本不同，HTTP 上
> 没有「连接」需要维持：宿主注册后，后续任意请求只要带着同一个 `host` 就会被认。
> （旧的 socket 版本里，四条独立 `socat` 命令只有第一条生效，其余落在未注册连接上
> 被静默忽略；换成 HTTP 后这个坑不存在了。）
>
> `curl` 也无法控制事件间隔（一次发一条），动作会一闪而过，要观察节奏就用模拟器。

预期：切「打字」→ 出气泡 → `agent/end` 举杠铃 → 之后 30s 无宿主 → daemon 退出并删掉 `daemon.json`。

> 验证渲染层是否真的收到了指令，看 daemon 的 stdout：每应用一条都会打
> `渲染层已应用 动画=... 气泡=...`。webview 的 console 在终端里看不到，
> 而桌宠的典型故障恰好就是「窗口里什么都没有」，所以这条日志是必要的。
