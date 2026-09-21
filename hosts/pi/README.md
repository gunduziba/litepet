# pi 适配器（`pet.ts`）

把 pi 的会话/工具事件转发给 `pet-daemon`。单文件、零运行时依赖，用的是 Node 内置
`fetch`，所以直接扔进 pi 的扩展目录就能跑。

## 安装

```sh
mkdir -p ~/.pi/agent/extensions
ln -sf "$PWD/hosts/pi/pet.ts" ~/.pi/agent/extensions/pet.ts
```

用符号链接而不是复制：改完 `/reload` 即可生效，不用重新拷贝。

## 用法

| 命令 | 作用 |
|---|---|
| `/pet` | 看联动状态与 daemon 的 `daemon/info` |
| `/pet off` | 暂停联动（同时向 daemon 道别） |
| `/pet on` | 恢复联动 |

## 事件映射

| pi 事件 | 协议方法 | 说明 |
|---|---|---|
| `session_start` | `host/hello`（请求） | 读 `~/.litepet/daemon.json` 后注册，并起 20s 心跳 |
| `agent_start` | `agent/start` | 宠物切「干活」动画 |
| `agent_end` | — | **只用来判定成败**，不上报（见下） |
| `agent_settled` | `agent/end` | 宠物回到空闲/举杠铃 |
| `tool_execution_start` | `tool/start` | 带气泡文本（命令 / 路径 / 模式） |
| `tool_execution_end` | `tool/end` | `isError` 决定是否红脸 |
| `session_shutdown` | `host/bye` | 主动注销，daemon 不必等心跳超时 |

### 为什么用 `agent_settled` 而不是 `agent_end`

`agent_end` 只代表**一次底层 run** 结束，pi 之后还可能自动重试、自动压缩后续跑。
桌宠是状态展示，如果在 `agent_end` 就报「结束」，宠物会在 pi 还在干活时先回到空闲。
`agent_settled` 才是「pi 不会再自动继续了」。

但 `agent_settled` **不带载荷**，无法判定成败；而 `agent_end` 带 `event.messages`。
所以两者的分工是：`agent_end` 把成败写入模块状态，`agent_settled` 读出来上报。

## 容错约定

**桌宠的问题绝不能变成 pi 的问题。** 因此：

- 所有网络调用 2s 超时，且吞掉全部异常（daemon 没起、中途被杀、返回非 JSON 都静默）
- 读 `daemon.json` 失败按 5s 节流重试，所以「pi 先起、daemon 后起」能自动接上，不需要 `/reload`
- HTTP 401（daemon 重启换了 token）会丢弃缓存的端点，下次事件自动重读

## 类型检查

`pet.ts` 的 `import type` 需要 `@earendil-works/pi-coding-agent` 的类型。pi 自带这个包，
但不在本项目依赖里，所以临时借一下：

```sh
cd hosts/pi
ln -sfn /path/to/pi-web/node_modules node_modules   # node_modules/ 已被 .gitignore 忽略
/path/to/pi-web/node_modules/.bin/tsc -p tsconfig.json
```
