# LitePet 🐾

LitePet 是一个专为 AI 编程工具（如 [pi](https://github.com/earendil-works/pi-coding-agent)、[dsh](https://github.com/deepseek-ai/deepseek-harness) 等）设计的**桌面宠物守护程序（Daemon）**。

它以轻量可爱的桌面宠物为载体，将 AI Coding Agent 的思考、工具调用与任务状态具象化展示在屏幕一角。当 Agent 忙碌时，宠物在屏幕前敲键盘；当调用工具时，宠物头顶会弹出实时气泡；任务彻底收工后，宠物还会为你庆祝并触发系统通知或手机推送。

---

## ✨ 核心特性

- **跨 Harness 共享单例**：多个终端或不同的 Agent 宿主（如同时开着多个 pi 终端或 dsh）接入同一个 LitePet 实例，智能仲裁当前展示状态，不会多开多个窗口打架。
- **流畅的轻量渲染**：基于 Tauri 2 构建，透明置顶小窗，基于 Animated WebP 精准切图渲染，资源占用极低。
- **协议中立与解耦**：宿主与宠物之间通过本地回环标准的 **HTTP + JSON-RPC 2.0** 通信，不强绑定任何特定工具的代码，极易扩展。
- **多通道任务提醒**：支持规则驱动的音效提醒（本地音频播放）、系统桌面通知与 Bark 手机推送。无论离开工位还是切换全屏，都能及时知道 Agent 何时真正收工。
- **可自定义宠物包**：支持换肤与多形象管理，遵循开放包规范，动画图集与触发规则皆可通过 `pet.json` 自由定制。

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
