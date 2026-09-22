# LitePet 🐾

LitePet 是一个专为 AI 编程工具（如 [pi](https://github.com/earendil-works/pi-coding-agent)、[dsh](https://github.com/deepseek-ai/deepseek-harness) 等）设计的**桌面宠物守护程序（Daemon）**。

它以轻量可爱的桌面宠物为载体，将 AI Coding Agent 的思考、工具调用与任务状态具象化展示在屏幕一角。当 Agent 忙碌时，宠物在屏幕前敲键盘；当调用工具时，宠物头顶会弹出实时气泡；任务彻底收工后，宠物还会为你庆祝并触发系统通知或手机推送。

---

## ✨ 核心特性

- **跨 Harness 共享单例**：多个终端或不同的 Agent 宿主（如同时开着多个 pi 终端或 dsh）接入同一个 LitePet 实例，智能仲裁当前展示状态，不会多开多个窗口打架。
- **流畅的轻量渲染**：基于 Tauri 2 构建，透明置顶小窗，基于 Animated WebP 精准切图渲染，资源占用极低。
- **协议中立与解耦**：宿主与宠物之间通过本地回环标准的 **HTTP + JSON-RPC 2.0** 通信，不强绑定任何特定工具的代码，极易扩展。
- **多通道任务提醒**：支持规则驱动的音效提醒（本地音频播放）、系统桌面通知与 Bark 手机推送。无论离开工位还是切换全屏，都能及时知道 Agent 何时真正收工。
- **兼容 Codex 宠物包标准**：原生兼容官方与社区的 Codex 宠物图集规范（`spritesheet.webp` + `pet.json`），社区已有形象直接解压即用；同时独家扩展了 Agent 状态机、事件规则映射与多通道提醒（音效/通知/手机推送）。

---

## 🏗️ 架构与生态

LitePet 体系支持各类 Agent 宿主接入：

1. **LitePet (本仓库)**：桌面宠物守护进程（Rust + Tauri 2），负责窗口渲染、状态仲裁与事件提醒。
2. [**pi-plugins-litepet**](../pi-plugins-litepet/)：面向 pi TUI 的官方扩展插件（内置完整协议契约与异步事件派发，捕获会话与工具执行并同步至宠物）。
3. **其他宿主适配器**（如 dsh 插件等）：可按标准 HTTP + JSON-RPC 2.0 协议直接对接。

```text
┌─────────────────────────┐      ┌─────────────────────────┐
│        pi 终端          │      │     dsh 或其他宿主      │
│  (pi-plugins-litepet)   │      │       (适配器)          │
└────────────┬────────────┘      └────────────┬────────────┘
             │   HTTP + JSON-RPC 2.0 (回环)   │
             └──────────────────┬─────────────┘
                                ▼
                   ┌─────────────────────────┐
                   │    LitePet (桌面守护进程)  │
                   │  - 状态仲裁与多宿主管理  │
                   │  - 透明置顶渲染与气泡    │
                   │  - 音效/通知/手机推送    │
                   └─────────────────────────┘
```

---

## 🚀 快速开始

### 运行环境
- **操作系统**：macOS (Apple Silicon / Intel) 或 Windows
- **构建依赖**：Rust 1.87+、Node.js 20+

### 开发与本地运行

```bash
# 1. 克隆仓库并进入目录
cd litepet

# 2. 以开发模式运行（启动桌面小窗与 RPC 服务）
cargo tauri dev
```

启动成功后，屏幕角落会出现宠物小窗，系统托盘也会出现 LitePet 图标（点击可打开设置面板或重置位置）。

### 打包发布

```bash
cargo tauri build
```
打包产物位于 `target/release/bundle/`（macOS 为 `.app` / `.dmg`，Windows 为 `.exe` 安装包）。

---

## ⚙️ 配置与端点发现

LitePet 的运行时配置与工作目录默认位于 `~/.litepet/`（可通过环境变量 `LITEPET_HOME` 自定义）：

```text
~/.litepet/
├── config.json       # 用户持久化配置（鉴权口令、提醒开关等）
├── daemon.json       # 运行时自动生成的端点信息（退出时自动清理）
├── pets/             # 自定义宠物包存放目录
└── logs/             # 运行日志
```

### 宿主如何连接
守护程序成功启动后，会自动写出 `~/.litepet/daemon.json`：
```json
{
  "protocolVersion": 1,
  "port": 4590,
  "token": "你的鉴权口令（未设时为空串）"
}
```
适配器/插件只需在启动时读取该文件中的端口与 Token，即可通过 `POST http://127.0.0.1:<port>/rpc` 发送事件。

### 安全鉴权
在 `~/.litepet/config.json` 中可配置鉴权 Token：
```json
{
  "auth": {
    "token": "your-secret-token"
  }
}
```
- **留空（默认）**：免密连接，同机程序直接调用，无需设置鉴权头。
- **设置口令**：所有 RPC 请求必须附带 HTTP 头 `Authorization: Bearer your-secret-token`。

---

## 🔌 通信协议与事件契约

LitePet 与各类宿主（插件/适配器）之间基于标准的 **HTTP/1.1 + JSON-RPC 2.0** 协议通信，仅监听本机回环地址（127.0.0.1）。

### 核心机制
- **端点发现**：宿主启动时读取 `~/.litepet/daemon.json` 获取动态端口与鉴权 Token。
- **单向事件流**：状态事件（如 `agent/start`、`tool/start`、`agent/end` 等）采用异步通知机制（Notification），服务端即时返回 `204 No Content`，不阻塞宿主主执行流程。
- **多宿主仲裁**：同机多个 Agent 实例接入时，采用最新事件优先（Last-Event-Wins）策略调度动作与气泡，并显示对应宿主标识。

### 核心接口概览
| 方法名 | 交互类型 | 作用说明 |
|---|---|---|
| `host/hello` | 请求 (Request) | 宿主首次接入时必须调用，完成协议版本握手与注册 |
| `daemon/ping` | 请求 (Request) | 宿主心跳保活（建议每 20 秒一次） |
| `agent/start` | 通知 (Notification) | Agent 开始执行任务，桌宠切换至工作（打字）动画 |
| `tool/start` / `tool/end` | 通知 (Notification) | 工具调用起止，桌宠头顶展示实时执行气泡 |
| `agent/end` | 通知 (Notification) | 单轮会话结束，触发成功（举杠铃）或失败动作反馈 |
| `agent/settled` | 通知 (Notification) | Agent 任务彻底收工，触发音效、系统通知与手机推送 |
| `host/bye` | 通知 (Notification) | 宿主退出前主动注销，快速释放状态 |

👉 **关于数据包结构、完整方法参数、状态码与多语言调用示例，请查阅**：[LitePet 通信协议规范 (docs/PROTOCOL.md)](docs/PROTOCOL.md)。

---

## 📦 Codex 宠物包兼容与字段扩展

LitePet 原生兼容官方与社区的 **Codex 宠物包标准**（图集 `spritesheet.webp` + 清单 `pet.json`）。任何从社区（如 codex-pets.net）下载的现成宠物包无需二次修改，直接放入 `~/.litepet/pets/` 目录即可无缝加载运行。

### 1. 为什么能够双向无缝兼容？
- **对纯 Codex 宠物包开箱即用**：Codex 原版标准仅包含通用角色动作（如跳跃、走路等），LitePet 内置了**智能降级映射器**，自动将原生角色动作映射为 Agent 状态（例如将 `running` 映射为工作敲键盘、`waiting` 映射为闲置休息、`bounce` 映射为成功庆祝、`sad` 映射为失败脸色），并提供任务收工音效。
- **对 Codex 原生环境零干扰**：Codex 官方解析器未开启严格未知字段拦截。LitePet 的所有高级特性均收拢在 `pet.json` 内独立的 `"litepet"` 命名空间中。这意味着**同一份 `pet.json` 清单，既能在 Codex 中正常运行，也能在 LitePet 中展现丰富的 Agent 联动能力**。

### 2. `pet.json` 字段定义与扩展说明

#### ① 基础规范字段（Codex 原生标准）
| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 宠物全局唯一标识符 |
| `displayName` | string | 宠物的展示名称（如“巡检喵”） |
| `description` | string | 宠物简介与角色描述 |
| `spritesheetPath` | string | 精灵图集相对路径（通常为 `spritesheet.webp`） |
| `spriteVersionNumber` | number | 图集版本号（`1` 为 9 行，`2` 为 11 行） |
| `frame` | object | *可选*。自定义网格单格宽高与行列（如 `{ width: 192, height: 208, columns: 8, rows: 9 }`） |
| `animations` | object | *可选*。具名动画索引表，声明各动作所引用的精灵图单元格索引数组 `frames` 与 `fps` |

#### ② 行为与提醒扩展字段（LitePet 独家扩展 ✨）
> **注意**：Codex 原版规范仅定义了静态画面与角色动作，**完全不具备 Agent 状态感知、交互事件驱动与多通道提醒能力**。因此，桌面宠物的业务状态机、事件规则映射与通知机制，均由 LitePet 扩展实现并收拢在 `"litepet"` 字段中：

```jsonc
{
  // ... 上方为 Codex 原生基础字段 ...
  
  // ↓↓↓ LitePet 独家扩展字段（由 LitePet 读取，Codex 会自动安全忽略）
  "litepet": {
    "schemaVersion": 1,
    "displaySize": 240,            // 宠物小窗渲染像素边长（留空则按单格尺寸）
    "behavior": {                  // 【行为扩展】Agent 状态机与规则配置
      "idleTimeoutMs": 90000,      // 无交互事件超过该时长（毫秒）自动进入闲置休息轮换
      "groups": {                  // 动作语义组：将底层动画归类为 Agent 状态，支持同组轮换
        "idle": ["idle"],
        "working": ["typing", "coding"],
        "resting": ["sleep", "tea_break"],
        "feedback": ["celebrate", "barbell"]
      },
      "rules": [                   // 事件规则表：将 Agent 协议事件映射到动作、头顶气泡与提醒
        {
          "on": "agent.start",
          "play": "group:working",
          "bubble": { "kind": "status", "text": "开始工作啦" }
        },
        {
          "on": "tool.start",
          "bubble": { "kind": "tool", "text": "{toolName}" }
        },
        {
          "on": "agent.settled",
          "play": "group:feedback",
          "bubble": { "kind": "success", "text": "全部搞定！" },
          "alert": {               // 多通道提醒：音效、桌面通知、Bark 手机推送
            "sound": "@done",
            "desktop": true,
            "push": true,
            "text": "{note}"       // 动态获取宿主传入的收工总结
          }
        }
      ]
    }
  }
}
```

👉 **关于精灵索引网格计算、降级映射表与提醒配置的完整规范，请参阅**：[宠物包契约规范 (docs/PET-PACK.md)](docs/PET-PACK.md)。

---

## 🛠️ 调试与测试

仓库内置了模拟宿主会话的脚本，可用于独立验证守护进程：

```bash
# 模拟一次完整的 Agent 干活、调用工具、完成会话链路
node scripts/host-sim.mjs
```

---

## 📄 开源协议

本项目采用 [Apache-2.0 License](LICENSE) 协议开源。
部分核心算法与交互设计参考自 [Open Vetta](https://github.com/open-vetta)，详情见 [NOTICE](NOTICE)。
