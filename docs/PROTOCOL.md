# LitePet 通信协议规范 (Protocol Specification v1)

> 协议版本：`v1`  
> 传输标准：`HTTP/1.1 over Loopback` + `JSON-RPC 2.0`  
> 适用对象：LitePet 桌面守护进程与所有 Agent 宿主插件 / 适配器（如 `pi-plugins-litepet`、dsh 适配器等）

---

## 1. 架构定位

LitePet 是一个单例运行的桌面宠物守护进程（Daemon）。
宿主（如 pi、dsh）通过标准本地回环 HTTP 协议向 LitePet 发送会话事件，LitePet 负责根据事件渲染宠物动作、弹出状态气泡以及触发系统通知/手机推送。

- **协议中立**：守护进程解耦任何具体 Agent 框架的代码，仅依据本协议规范通信。
- **单例聚合**：支持同机多个宿主或多个终端实例并发接入，守护进程根据仲裁策略统一展示当前最活跃宿主的状态。
- **单向事件流**：大部分事件为非阻塞的异步通知，宿主派发后无需等待业务确认，保证宿主执行链路零阻塞。

---

## 2. 传输与安全

### 2.1 传输规范

| 配置项 | 规范要求 |
|---|---|
| 通信地址 | `http://127.0.0.1:<port>/rpc`（**仅限本地回环网络**，禁止对外网暴露） |
| HTTP 方法 | 仅接受 `POST`，其他方法返回 `405 Method Not Allowed` |
| 请求路径 | 仅接受 `/rpc`，其他路径返回 `404 Not Found` |
| 数据格式 | `Content-Type: application/json` |
| 包体上限 | 64 KiB，超出限制返回 `413 Payload Too Large` |

### 2.2 安全鉴权

- 鉴权密钥在 `~/.litepet/config.json` 的 `auth.token` 中定义。
- **留空（默认）**：免密通信，本机进程直接请求即可。
- **已设置口令**：请求时必须附带 HTTP 头 `Authorization: Bearer <token>`。缺失或口令错误将返回 `401 Unauthorized`。
- Token 仅支持可见 ASCII 字符。

---

## 3. 端点发现机制

守护进程在成功绑定本地端口后，会将端点元数据写入本地文件，宿主在启动时读取该文件完成连接配置：

- **文件路径**：`$LITEPET_HOME/daemon.json`（未设置时默认为 `~/.litepet/daemon.json`）
- **文件权限**：`0600`（仅当前系统用户可读写）
- **文件内容**：
  ```json
  {
    "protocolVersion": 1,
    "port": 4590,
    "token": ""
  }
  ```
- **生命周期**：守护进程正常退出时会自动清理该文件。宿主连接时只需读取 `port` 与 `token`。

---

## 4. 报文信封格式与交互机制

所有交互遵循 **JSON-RPC 2.0** 规范：

```json
{
  "jsonrpc": "2.0",
  "method": "<方法名称>",
  "params": { ... },
  "id": 1
}
```

### 4.1 请求与通知的区别

1. **请求（Request - 含有 `id` 字段）**：
   - 用于需要宿主与守护进程同步交互的方法（如注册握手 `host/hello`、心跳 `daemon/ping`、状态查询 `daemon/info` 等）。
   - HTTP 状态码为 `200 OK`，响应体包含 `result` 或 `error` 对象。

2. **通知（Notification - 不含 `id` 字段）**：
   - 用于所有单向事件流派发（如 `agent/start`、`tool/start`、`agent/end` 等）。
   - 守护进程接收后立即返回 `204 No Content`（无响应体）。
   - **宿主禁止同步阻塞等待业务处理完成**。

### 4.2 状态码与错误码

- **HTTP 状态码**：表征传输层状态（`200` 处理完成、`204` 通知已收、`401` 鉴权失败、`404` 路径错误、`405` 方法不支持、`413` 报文超限）。
- **JSON-RPC 业务错误码**（包含在 `200` 响应体的 `error` 中）：

| 错误码 | 含义 |
|---|---|
| `-32001` | 宿主未注册（未先执行 `host/hello` 即发送需要宿主身份的请求） |
| `-32002` | 协议版本不支持（客户端请求的 `protocolVersion` 高于守护进程支持的上限） |
| `-32601` | 方法不存在（在通知中遇到未知方法则静默忽略，保证前向兼容） |
| `-32602` | 参数非法或缺失必填字段 |
| `-32603` | 内部处理错误 |

---

## 5. 宿主生命周期与状态仲裁

### 5.1 生命周期链路

```text
宿主启动 ──► 读取 daemon.json ──► host/hello 注册
                                      │
                                      ▼
                        正常会话 (agent/start, tool/*, agent/end)
                                      │
                                      ▼
          定期保活 (daemon/ping 每 20s) ──► 退出时调用 host/bye 注销
```

- **保活机制**：宿主建议每 20 秒发送一次 `daemon/ping`。守护进程若超过 60 秒未收到某宿主的任何事件，将自动将其判定为超时下线并清理其状态。
- **主动断开**：宿主退出时建议发送 `host/bye` 通知，守护进程将立即注销该宿主。

### 5.2 状态流转与仲裁规则

守护进程为每个接入的宿主维护独立的状态机：

```text
idle ──agent/start──► working ──agent/end(success)──► celebrating(≤8s) ──► idle
                          │              │                   │
                          │              └── 失败 ──► failed(≤8s) ┘
                          │                                  │
                          └── 长时间无事件 ──► resting 轮换 ◄──┘
```

- **多宿主仲裁**：采用 **Last-Event-Wins** 机制。最新产生交互事件的宿主接管桌宠的视觉焦点与动画。
- **宿主徽章**：当显示气泡时，气泡前附带宿主标识前缀（如 `[pi] 读取文件`、`[dsh] 执行终端命令`）。

---

## 6. 宿主事件与方法定义 (Host -> Daemon)

所有宿主相关的 RPC 方法均要求 `params` 包含 `host` 字段（宿主标识字符串，如 `"pi"`、`"dsh"`），字段命名使用 **camelCase**。

| 方法名 | 交互类型 | 参数结构 (`params`) | 说明 |
|---|---|---|---|
| `host/hello` | **请求** | `host: string`<br>`protocolVersion: number`<br>`pid?: number`<br>`agentVersion?: string`<br>`clientVersion?: string` | **首次通信必须调用**。完成宿主登记与协议握手 |
| `host/bye` | 通知 | `host: string`<br>`reason?: string` | 宿主主动退出通知，立即注销该宿主 |
| `daemon/ping` | **请求** | `host: string`<br>`ts: number` | 心跳保活请求，建议每 20 秒调用一次 |
| `daemon/info` | **请求** | `host: string` | 查询守护进程当前状态、版本与活跃宿主数 |
| `agent/start` | 通知 | `host: string`<br>`sessionId?: string`<br>`summary?: string` | Agent 开始工作，宠物切换为打字动画 |
| `agent/end` | 通知 | `host: string`<br>`success: boolean`<br>`sessionId?: string`<br>`note?: string` | 单轮 Agent 结束。触发成功（举杠铃庆祝）或失败反馈 |
| `agent/settled` | 通知 | `host: string`<br>`sessionId?: string`<br>`note?: string` | Agent **彻底收工**（确认无自动重试或后续任务）。触发完成音效与通知推送 |
| `tool/start` | 通知 | `host: string`<br>`toolName: string`<br>`bubble?: string` | 工具调用开始。头顶弹出工具执行气泡 |
| `tool/end` | 通知 | `host: string`<br>`toolName: string`<br>`isError?: boolean` | 工具调用结束 |
| `pet/bubble` | 通知 | `host: string`<br>`kind: string`<br>`text: string`<br>`ttlMs?: number` | 直接指定头顶气泡。`kind` 支持 `info` / `status` / `tool` / `success` / `warning` / `error` |

### 核心区分：`agent/end` 与 `agent/settled`
- **`agent/end`**：表征单轮次执行完毕。若 Agent 宿主有自动重试、自动压缩后继续执行等机制，每轮都会派发 `agent/end`。
- **`agent/settled`**：表征整场任务完全结束并静止（Agent 不再自动继续）。**用于驱动声音提醒与手机推送**，避免在中途重试时误发完成提醒。

---

## 7. 桌面控制与管理接口 (Client -> Daemon)

用于设置面板、托盘功能和配置管理脚本。该类方法**无需 `host` 字段**，也不触碰宿主会话状态机。

| 方法名 | 交互类型 | 参数结构 (`params`) | 说明 |
|---|---|---|---|
| `pet/list` | **请求** | 无 | 获取已安装的宠物包列表及当前选中的包 |
| `pet/select` | **请求** | `id: string` | 切换当前桌面宠物形象（参数为宠物包目录名） |
| `config/get` | **请求** | 无 | 读取全局配置与当前运行路径信息 |
| `config/set` | **请求** | 配置补丁对象 | 更新全局配置（按顶层模块整块合并） |
| `notify/test` | **请求** | 无 | 触发测试提醒，返回实际可用的提醒通道状态 |
| `notify/preview` | **请求** | `sound: string` | 试听指定的音效配置写法，返回实际匹配的音频路径 |

---

## 8. 调用示例

### 8.1 Bash / cURL 验证

```bash
# 1. 从端点文件获取端口与 Token
PORT=$(node -e "console.log(require(process.env.HOME + '/.litepet/daemon.json').port)")
TOKEN=$(node -e "console.log(require(process.env.HOME + '/.litepet/daemon.json').token)")
RPC="http://127.0.0.1:$PORT/rpc"
AUTH_HEADER=""
[ -n "$TOKEN" ] && AUTH_HEADER="Authorization: Bearer $TOKEN"

# 2. 宿主注册 (请求)
curl -s -X POST "$RPC" \
  ${AUTH_HEADER:+-H "$AUTH_HEADER"} \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "host/hello",
    "id": 1,
    "params": {
      "host": "curl-client",
      "protocolVersion": 1
    }
  }'

# 3. 发送工具调用通知 (通知，HTTP 204 无返回体)
curl -s -w "\nHTTP Code: %{http_code}\n" -X POST "$RPC" \
  ${AUTH_HEADER:+-H "$AUTH_HEADER"} \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "tool/start",
    "params": {
      "host": "curl-client",
      "toolName": "bash",
      "bubble": "运行自动化测试"
    }
  }'
```

### 8.2 Node.js / TypeScript 简易客户端

```typescript
interface DaemonEndpoint {
  protocolVersion: number;
  port: number;
  token: string;
}

export class LitePetClient {
  constructor(private endpoint: DaemonEndpoint, private hostId: string) {}

  /** 发送单向异步通知（不等待业务结果，返回 204 即成功） */
  async notify(method: string, params: Record<string, unknown> = {}): Promise<void> {
    const headers: Record<string, string> = { 'Content-Type': 'application/json' };
    if (this.endpoint.token) {
      headers['Authorization'] = `Bearer ${this.endpoint.token}`;
    }

    await fetch(`http://127.0.0.1:${this.endpoint.port}/rpc`, {
      method: 'POST',
      headers,
      body: JSON.stringify({
        jsonrpc: '2.0',
        method,
        params: { host: this.hostId, ...params }
      })
    });
  }

  /** 发送同步请求并获取返回值 */
  async call<T>(method: string, params: Record<string, unknown> = {}, id = Date.now()): Promise<T> {
    const headers: Record<string, string> = { 'Content-Type': 'application/json' };
    if (this.endpoint.token) {
      headers['Authorization'] = `Bearer ${this.endpoint.token}`;
    }

    const res = await fetch(`http://127.0.0.1:${this.endpoint.port}/rpc`, {
      method: 'POST',
      headers,
      body: JSON.stringify({
        jsonrpc: '2.0',
        method,
        params: { host: this.hostId, ...params },
        id
      })
    });

    const data = await res.json();
    if (data.error) throw new Error(`RPC Error [${data.error.code}]: ${data.error.message}`);
    return data.result as T;
  }
}
```
