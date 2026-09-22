# 桌宠协议 v1（litepet）

> 状态：**已定稿**（M0 定契约，M2 换传输）｜ 实现互操作契约，任何适配器不得偏离本文
>
> 本文与 `SPEC.md` §2 一致。若两者冲突，以本文为准，并回改 SPEC。

## 0. 定位

`litepet` 是一个**单例守护进程**，在屏幕角落显示一只桌面宠物。宠物形象来自宠物包，可随时替换（`docs/PET-PACK.md`）。

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
| 鉴权 | `Authorization: Bearer <token>`，缺失或错误回 `401`；`auth.token` 为空时不要求任何头 |
| 请求体上限 | 64 KiB，超出回 `413` |

token **由用户自己定**，写在 `config.json` 的 `auth` 段里；daemon 不生成、不轮换。
这一段只有 `token` 一个字段——**填了就要鉴权，留空就不鉴权**，没有单独的开关：

```jsonc
// ~/.litepet/config.json
"auth": {
  "token": "my-own-secret-123"  // 留空 = 不鉴权
}
```

- 早先的做法是启动时随机生成 32 位十六进制串。改掉了：那要求宿主每次重启都回去重读端点文件，用户配好的值重启一次就没了。
- 再后来同时有过 `enabled` 开关和 `token` 两个字段。删掉了：`token` 空不空已经能表达「要不要鉴权」，多一个开关就多出一种自相矛盾的摆法（开着鉴权却没填 token），而那种摆法还得另外规定算哪边。
- 缺省（含升级上来的老配置）是空 token，即**不鉴权**。所以「没设防」这个状态会在启动日志里 `warn` 出来，设置页上也当场写着「本机谁都能连」。
- token 配得用不了（含空白或非 ASCII 字符）时，daemon **不拒绝启动**、也不退回不鉴权，而是**一个请求都不放**（回 `503`，理由写在响应里）。不拒绝启动：鉴权只管 `POST /rpc` 的准入，跟窗口、托盘、宠物渲染无关。不退回不鉴权：那是静悄悄地把接口敲开，而用户会以为自己已经设了防。
- token 只能用**可见 ASCII**。`Authorization` 头里出现非 ASCII 或空白字节，请求会连 `401` 都拿不到、而是被直接掐断连接（实测 curl 退出码 52），所以这种值在启动时就被拦下。
- 不鉴权的代价要清楚：接口仍只绑回环，但**同机上任何程序都能连**，包括你正在浏览的网页（浏览器向 `localhost` 发简单请求可以绕过 CORS 预检）。

### 1.1 端点发现

daemon 在**端口绑定成功之后**写出端点文件，宿主读它拿端口与 token：

| 项 | 值 |
|---|---|
| 路径 | `$LITEPET_HOME/daemon.json`，未设时回退 `~/.litepet/daemon.json` |
| 权限 | `0600`（只允许当前用户读，避免同机其他用户拿到 token） |
| 内容 | `{"protocolVersion": 1, "port": 4590, "token": "<用户自定的口令，不鉴权时为空串>"}` |

- daemon **正常退出时删除**该文件；进程被 `kill -9` 会留下陈旧文件，宿主应先尝试连接再决定是否重试
- 宿主**不该缓存**端口：文件正常退出时会被删掉、下次启动重写。token 在多次启动之间是稳定的，但同样从这份文件读最省事——它就在旁边
- 宿主判「能不能连」只看 `port` 是不是数字，**不要按 token 的真值判**：不鉴权时它是空串，按真值判会把正常状态误报成「文件残缺」
- 文件出现即代表可以连了，宿主侧推荐「轮询文件出现」而不是「盲等固定秒数」

### 1.2 为什么不用 Unix domain socket

早期设计（M0–M1）用 Unix socket + JSONL 帧，M2 改为 HTTP + JSON-RPC：

- **宿主侧零依赖**：pi 用 `fetch`、dsh 用标准库 HTTP 客户端即可，不必引 socket 库、不必自己处理帧边界与粘包
- **没有「连接」这层状态**：不存在「连上了但没注册」「连接半开」「EOF 与 ECONNRESET 区分」这些情形，宿主崩溃天然无副作用，不需要指数退避重连
- **可手工验证**：`curl` 一条命令就能发事件，不必装 socat（见 §11）
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
| `agent/settled` | 通知 | `sessionId?: string` | 该宿主的 agent **彻底结束，不会再自动继续**（见下） |
| `tool/start` | 通知 | `toolName: string`, `bubble?: string` | 工具开始；`bubble` 为可选展示文本，**建议 ≤ 48 字符**，超长由 daemon 截断 |
| `tool/end` | 通知 | `toolName: string`, `isError?: boolean` | 工具结束 |
| `pet/bubble` | 通知 | `kind: "info"\|"status"\|"tool"\|"success"\|"warning"\|"error"`（未知值降级，见下）, `text: string`, `ttlMs?: number` | 直接发一条气泡 |
| `daemon/ping` | 请求 | `ts: number`（epoch ms） | 心跳，建议 20s 一次 |
| `daemon/info` | 请求 | — | 查询 daemon 现状（调试与适配器自检用） |

- `sessionId` 仅用于日志，daemon 不做多会话区分（每宿主一个状态机）
- 未 `host/hello` 就发事件：通知被静默忽略，请求回 `-32001`
- `protocolVersion` **高于** daemon 支持（当前 `1`）→ `-32002`，该宿主不注册；**低于**则接受（前向兼容）

**`agent/settled` 与 `agent/end` 是两个语义，不能合并**。区别在**确定性**：一轮跑完之后，宿主可能自动重试、自动压缩后重试，或继续处理排队中的后续消息（pi 的 `agent_end` 即如此）。这种「本轮完了但还会自己接着干」的情形，`agent/end` 每次都发，`agent/settled` 只在确定不会再动时发。

- 它**不带 `success`**：宿主能观察到的只是「不会再自动继续」这一个事实，成功与否属于 `agent/end` 的语义。逼宿主在这里重报一次结果，等于逼它自己编一个值（pi 的 `agent_settled` 事件里就没有这个字段），而本协议明令禁止宿主发送不确信的信息。daemon 自己记着每个宿主最近一次 `agent/end` 的结果，用它决定停稳后是庆祝还是给失败脸色；宿主重连（再次 `host/hello`）时该记录作废。
- 没有规则命中它时，daemon 走包通用默认提醒（出声 + 系统通知）。提醒是 **daemon 本地副作用**，不出现在线格式里，细节见 `docs/PET-PACK.md` §4.5。

### 4.1 适配器独立演进产生的容错（v1 已实现）

适配器独立于 daemon 发版，所以 daemon **必须能接住它没见过的输入**：

| 输入 | 行为 |
|---|---|
| 未知 `method`（通知） | 静默忽略 |
| 未知 `method`（请求） | `-32601` |
| 未知 `params` 字段 | 忽略，不报错（未开 `deny_unknown_fields`） |
| 未知 `bubble.kind` | **不拒绝**，降级为最低优先级、不着色的普通气泡（daemon 回传 `kind: "unknown"`），并记一条 daemon 日志 |
| 缺必需字段 / 类型错 | `-32602`；若为通知则只记 daemon 日志（无回复通道） |

> 未知 `kind` 不拒绝是刻意的：`pet/bubble` 是通知，拒了它适配器作者只会看到「什么也没发生」。
> 但同一个未知 `kind` 写在**包配置** `litepet.behavior` 里是**加载期硬拒**的——线格式是别人的新版本，包配置是自己的声明，拼错不能静默。

## 5. daemon → 宿主

daemon 只在应答请求时说话，没有主动推送。

| method | `result` 字段 | 语义 |
|---|---|---|
| `host/hello` | `daemonVersion: string`, `petId: string`, `protocolVersion: number` | 注册确认 |
| `daemon/ping` | `host: string`, `protocolVersion: number`, `ts: number` | 心跳回应，原样回显宿主 `ts` |
| `daemon/info` | `daemonVersion`, `protocolVersion`, `petId`, `hostCount: number`, `pingIntervalMs: number`, `hostTimeoutMs: number` | daemon 现状 |

`daemon/info` 实际响应示例：

```json
{ "jsonrpc":"2.0", "result": {
  "daemonVersion": "0.1.0", "protocolVersion": 1, "petId": "xunjian-miao",
  "hostCount": 1, "pingIntervalMs": 20000, "hostTimeoutMs": 60000
}, "id": 4 }
```

> **旧版本文档里的 `host.welcome` / `pong` / `host.evicted` 三种推送报文已不存在**：前两者变成上面的请求应答，`host.evicted` 的用途由 `host/hello` 回 `-32002` 取代。

## 6. 仲裁规则

**每宿主状态机**：

```text
idle ──agent.start──► working ──agent.end(success)──► celebrating(≤8s) ──► idle
                          │              │                   │
                          │              └── 失败 ──► failed(≤8s) ┘
                          │                                  │
                          └── 长时间无事件 ──► resting 轮换 ◄──┘
```

- `celebrating` / `failed` 固定最长 8 秒（`FEEDBACK_MS`），到点自动回舞台动画；`agent/settled` 同样进这两个状态之一，若与 `agent/end` 算出的结果相同则不重复推送
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
- 气泡优先级：`error` > `warning` > `success` > `tool` > `status` > `info` > `unknown`；低优先级不抢占未过期的更高优先级气泡
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
- 所有宿主注销 → daemon **继续常驻**（不退出）：宠物窗口收进托盘，托盘菜单「显示宠物」可以再点出来；只有用户从托盘选「退出」才结束进程
- `--resident` 仍收但已无作用（宠物本来就是常驻的），保留只为不弄坏已有脚本
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

组是**我们**的概念（Codex 格式里没有），定义在 `litepet.behavior.groups`（契约见 `docs/PET-PACK.md` §4.2）。组的语义与 `SPEC.md` §3.4 一致，组内多个动画按轮换规则选用。

### 9.2 动画名从哪来

**没有固定词表。** daemon 只认组（§9.1），具体放哪个动画一律听包的：

1. `pet.json` 声明了 `animations` → 用包里的名字；
2. 没声明 → 用 `docs/PET-PACK.md` §3.7 那张 Codex 缺省表，按图集行号铺出 `idle` + 13 个行名（V2 图集再加 2 个注视方向）；
3. 组的名字（`idle` / `working` / `resting` / `feedback`）**不是动画名**，要经规则表或 §4.4 的降级映射翻译成动画名。

内置的巡检喵（`assets/pets/xunjian-miao/`）走的是第 2 条：它的 `pet.json` 里既没有 `animations` 也没有 `litepet` 键，所以它那 16 个动画名全是 Codex 行名，提醒也走降级映射。

**动画名跟素材文件名无关。** 外部包的文件名可以跟动画名完全对不上，否则用户没法用自己命名的素材。

### 9.3 事件 → 动画映射

| 事件 | 播放 | 气泡 |
|---|---|---|
| `agent/start` | `working` 组默认 | `status`：开始工作 |
| `tool/start` | 保持当前 | `tool`：`{toolName}` 或宿主给的 `bubble` 文本 |
| `tool/end`(isError) | `feedback` 组的失败动画 | `error`：`{toolName}` |
| `tool/end`（成功） | 保持当前 | **不出气泡**（`toolName` 是去重键，不是展示文本） |
| `agent/end`(success) | `feedback` 组的成功动画 | `success`：任务完成 |
| `agent/end`(fail) | `feedback` 组的失败动画 | `info`：任务失败 |
| `agent/settled` | `feedback` 组：上次 `agent/end` 成功则庆祝，失败则给失败脸色 | **不出气泡**（除非规则声明；但默认会出声 + 系统通知，见下） |
| 90s 无事件 | `resting` 组轮换 | — |

> 上表是**默认规则**的等价描述，任何包都能用自己 manifest 里的 `litepet.behavior.rules` 覆盖（规则表契约见 `docs/PET-PACK.md` §4.3）。daemon 侧只做**通用规则解释器**；Rust 里写死的只有 §4.4 那张降级映射表，而且只在包没声明规则表时才用。
>
> 纯 Codex 包（无 `litepet` 键）走 `docs/PET-PACK.md` §4.4 的降级映射表。

**第三个通道：提醒**。上表只管屏幕上的宠物与气泡；提醒（声音 / 系统通知 / 手机推送）是**独立的旁路**，用不用它由规则表的 `alert` 字段决定（`docs/PET-PACK.md` §4.5）。没有规则命中时，有两个包通用的默认提醒：`agent/settled`（出声 + 通知）与失败的 `agent/end`。提醒内容没有气泡可借时用事件自带的一句话（如 `agent.settled` → 「这一轮干完了」），所以提醒永远不会是空壳。

## 10. 客户端 → daemon：daemon 级控制方法

§4 与 §5 讲的是宿主（pi / dsh 适配器）与自己那只宠物的对话。这一节是**设置窗口与脚本**用的入口：读写配置、换宠物、试提醒。

**同一个 `/rpc` 端点、同一套信封与鉴权**（§1），不另外开端口。这些方法**不要求 `params.host`**，也**不注册宿主**——它们不碰会话状态机，因此不受 §6 仲裁影响。反过来它们影响所有宿主：`config/set` 改的是全局配置。

| method | 类型 | 参数 | `result` |
|---|---|---|---|
| `pet/list` | 请求 | — | `pets: PetSummary[]`, `current: string \| null` |
| `pet/select` | 请求 | `id: string`（目录名，或清单里的 `id`） | 新包的公开信息（`PetInfo`） |
| `config/get` | 请求 | — | `config`, `home`, `log`, `petsRoot`, `justCreated`, `version` |
| `config/set` | 请求 | 配置补丁（§10.4） | `config`（**归一化之后**）, `restartRequired: string[]` |
| `notify/test` | 请求 | — | `sound: boolean`, `desktop: boolean`, `push: boolean` |
| `notify/preview` | 请求 | `sound: string` | `sound`, `path: string \| null`, `layer`, `hint` |

字段名一律 **camelCase**（与 §4 的 `params` 一致）。

### 10.1 `pet/list`

```json
{ "jsonrpc":"2.0", "result": {
  "pets": [
    { "dir":"xunjian-miao", "id":"xunjian-miao", "displayName":"巡检喵",
      "description":"……", "spritesheetPath":"/Users/me/.litepet/pets/xunjian-miao/spritesheet.webp",
      "frame":{ "width":192, "height":208, "columns":8, "rows":9 },
      "problem": null }
  ],
  "current": "xunjian-miao"
}, "id": 5 }
```

**`dir` 与 `id` 是两个不同的东西**：`dir` 是目录名，也就是包的规范身份——`pet/select` 传的是它；`id` 是清单里的值，只用于展示。

两者可以不同：社区下载的包目录名常带后缀（`kun-signature.codex-pet/`），而清单里写的 `id` 是 `kun-signature`。daemon 两侧都认（先按目录名找，再扫目录比对清单 `id`）；几个包的清单 `id` 撞车时报错并列出候选目录，而不是猜一个。`current` 取的是清单 `id`。

**坏包也要列出来**，`problem` 里写坏在哪（清单读不通、图集缺、网格不合法），`spritesheetPath` / `frame` 为 `null`。用户明明装了它，凭空消失只会让人怀疑自己装错了地方。`spritesheetPath` 与 `frame` 是给设置页画首帧预览用的，缺一就画不出来，所以坏包要如实说缺哪个。

列表**按目录名排序**：`read_dir` 的顺序各文件系统不同，配置页每次刷新看到的顺序都变一遭会很难用。`petsRoot` 还不存在时返回空数组而不是报错。

### 10.2 `pet/select`

切当前宠物包，返回新包的公开信息（结构同 `PetInfo`：`id` / `displayName` / `description` / `spritesheetPath` / `frame` / `animations`）。

失败（包不存在、清单不合法、`litepet.behavior` 非法）回 `-32602`，且**整件事不发生**——不留「画面换了、动作却对不上」的宠物。顺序是有意的：先换会话里的规则表，再换渲染层那份。

换包有三个连带动作：渲染层重新加载图集（动画名集合多半变了）、提醒层重新解析音效（`alert.sound` 里的相对路径是相对**包目录**的）、把 `id` 记进配置好在下次启动时恢复。

### 10.3 `config/get`

```json
{ "jsonrpc":"2.0", "result": {
  "config": { "...": "见 docs/PET-PACK.md §4.5.2" },
  "home": "/Users/me/.litepet", "log": "/Users/me/.litepet/logs/litepet.log",
  "petsRoot": "/Users/me/.litepet/pets",
  "justCreated": false, "version": "0.1.0"
}, "id": 6 }
```

`home` / `log` / `petsRoot` 一并给出去，是为了让设置页能显示「东西都在哪」并能一键打开；`justCreated` 为 `true` 表示这次读配置时才刚生成默认文件（设置页可以据此说一句「已生成默认配置」）。配置读不出来回 `-32603`（内部错误），不是参数错。

### 10.4 `config/set`

参数就是一份**配置补丁**：扁平地给出**要改的顶层键**，值是该键的完整新值。

```json
{ "jsonrpc":"2.0", "method":"config/set", "id":7,
  "params": { "size": 240, "notify": { "enabled": true, "sound": { "enabled": true,
    "volume": 0.35, "files": { "done":"", "failed":"", "attention":"" } },
    "desktop": { "enabled": true }, "push": { "enabled": false, "provider":"bark",
    "deviceKey":"", "endpoint": null } },
    "auth": { "token": "" } } }
```

两条规则必须记牢，它们决定了调用方该怎么供货：

1. **没提到的顶层键保持原值**。所以设置页只发改动过的那几个键，不需要先 `config/get` 再整份回写。
2. **提到了的那个顶层键是整块替换**。只发 `notify.sound` 就会把 `notify.desktop` / `notify.push` 一起打回默认值——不是保留旧值。所以**改 `notify` 里的任何一项，都要带上完整的 `notify` 块**；改 `sound.files` 里的任何一个槽位，都要带上三个槽位。少发一个 `files` 就等于把它清空，用户挑半天的音效会在下一次保存时静静地消失（设置页的 `collect()` 因此总是凑齐整块，`scripts/check-ui.mjs` 里有一条断言盯着它）。

应答里的 `config` 是**归一化之后**的实际值，前端应当用它回填，而不是拿自己发出去的值当准（越界音量会被夹到范围内、空 `endpoint` 会变成 `null`、缺字段会补默认值）。字段不合法回 `-32602`；写盘失败回 `-32603`。

`restartRequired` 列出**要重启才生效的顶层键**，目前只可能是 `port`：监听早就绑好了。宠物包、窗口尺寸、提醒开关都能当场生效，daemon 不假装改不了的也已经改了。

### 10.5 `notify/test`

立刻发一条测试提醒，返回**哪些通道真的发出去了**：

```json
{ "jsonrpc":"2.0", "result": { "sound": true, "desktop": true, "push": false }, "id": 8 }
```

它走的就是真实提醒那条路（同一个 `dispatch`，过总开关与通道开关），只是事件与内容换成固定的测试用文本。另起一条测试专用路径只会验证出一条真实事件走不到的路。`push: false` 的常见原因是没填 Bark 密钥或那一项关着——这正是它存在的理由：「token 到底对不对」只有真发一次才知道，而推送凭据只存在 daemon 侧。

### 10.6 `notify/preview`

试听**一条音效写法**（写法与规则表里的 `alert.sound` 完全相同，见 `docs/PET-PACK.md` §4.5），只出声、不动通知与推送：

```json
{ "jsonrpc":"2.0", "method":"notify/preview", "id":9,
  "params": { "sound": "@done" } }
→ { "jsonrpc":"2.0", "result": {
      "sound": "@done",
      "path": "/Applications/LitePet.app/Contents/Resources/sounds/@done.wav",
      "layer": "bundled", "hint": null }, "id":9 }
```

- `path` 是**真实命中的那个文件**，`layer` 说它是哪一层给的：`user`（用户自选）→ `bundled`（应用自带兜底）→ `pack`（宠物包内）→ `system`（当前平台系统音效）。
- `path` 为 `null` 时说明没有可播的文件，`hint` 里带一条能照着做的建议。**这不是错误码**：找不到音效是常见情形（包从别的平台搬来、系统没装），回的是 `result` 而不是 `error`。
- **它不看总开关，也不看规则的 `alert`**：只回答「这个文件能不能响」。与 `notify/test` 的分工正在这里——后者回答「现在发得出什么」，前者回答「这个写法落到哪个文件上」。「用户选了文件却没声音」得当场能定位到底是文件的问题还是开关的问题。
- `sound` 是必填，且不能是空串（空串回 `-32602`）。

因为写法与规则表一致，它也能拿去另一台机器上验「这条规则配的音效在这台机器上能不能响」。

### 10.7 鉴权与错误

这六个方法与 §4 的事件走**同一个**准入：`config.json` 的 `auth.token` 留空则谁都能调，填了就要求每条请求都带 `Authorization: Bearer <token>`（§1）。设置窗口与 `curl` 都不例外——它改的是全局配置，不该比发事件更宽松。

| 情形 | 应答 |
|---|---|
| 缺 / 错的 `Authorization` | HTTP 401（`auth.token` 非空时） |
| `auth.token` 含空白或非 ASCII（永远配不上） | HTTP **503**，不是 401——问题在 daemon 这边没配好，401 会让人反复去检查宿主那侧的 token |
| 未知 `method` | `-32601` |
| `pet/select` 的包不存在 / 不合法，`notify/preview` 的 `sound` 为空，`config/set` 字段不合法 | `-32602` |
| 读配置、写配置、序列化失败 | `-32603` |

## 11. 手工验证

**推荐用仓库里的零依赖模拟器**（它自己会等 `daemon.json` 出现再连）：

```bash
node scripts/host-sim.mjs --host pi --step 800     # 演完整会话，每步间隔 800ms
node scripts/host-sim.mjs --step 0                 # 一口气跑完
node scripts/host-sim.mjs --keep-alive             # 演完保持心跳，观察宿主不掉线
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

预期：切「打字」→ 出气泡 → `agent/end` 举杠铃 → 道别后 daemon **不退出**：宠物收进托盘（`日志里出现「宿主全部断开，宠物收进托盘」`），进程还在，`daemon.json` 也还在。

> 验证渲染层是否真的收到了指令，看 daemon 的 stdout：每应用一条都会打
> `渲染层已应用 动画=... 气泡=...`。webview 的 console 在终端里看不到，
> 而桌宠的典型故障恰好就是「窗口里什么都没有」，所以这条日志是必要的。
