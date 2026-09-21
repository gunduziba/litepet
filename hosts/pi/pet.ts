// pi 扩展适配器：把 pi 的会话/工具事件转发给 pet-daemon（HTTP + JSON-RPC 2.0）。
//
// 装法：把本文件放到 `~/.pi/agent/extensions/pet.ts`（pi 全局自动发现，支持 `/reload`）。
//
// 设计要点（都是刻意的）：
// 1. **绝不阻塞 pi**：所有网络调用都是「发完就算」的通知，带 2s 超时且吞掉全部异常。
//    桌宠掉线是小事，拖慢 pi 是大事。
// 2. **用 `agent_settled` 而非 `agent_end`**：`agent_end` 只代表一次底层 run 结束，
//    pi 之后还可能自动重试/压缩续跑。宠物是状态展示，必须等真的不再自动继续。
// 3. **HTTP 无连接**：没有要维护的长连接，也没有重连逻辑。daemon 没起就下次事件再试。

import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/** 宿主标识，写进协议报文的 `host` 字段。 */
const HOST = "pi";

/** 协议版本，与 `src/protocol.rs` 的 `PROTOCOL_VERSION` 一致。 */
const PROTOCOL_VERSION = 1;

/** 对接信息文件名，与 `src/config.rs` 的 `ENDPOINT_FILE` 一致。 */
const ENDPOINT_FILE = "daemon.json";

/** 心跳间隔，与 `src/session.rs` 的 `PING_INTERVAL`（20s）一致。 */
const PING_INTERVAL_MS = 20_000;

/** 单次 HTTP 请求的超时。桌宠不能拖慢 pi，因此给得很短。 */
const REQUEST_TIMEOUT_MS = 2_000;

/** 气泡文本上限，与 `src/protocol.rs` 的 `BUBBLE_TEXT_LIMIT`（48）一致。 */
const BUBBLE_LIMIT = 48;

/** 读对接信息失败后，隔多久才允许再碰磁盘。避免每个事件都去读文件。 */
const PROBE_RETRY_MS = 5_000;

/** daemon 写出的对接信息。 */
interface Endpoint {
  /** 回环端口。 */
  port: number;
  /** 访问密钥。 */
  token: string;
  /** daemon 声明的协议版本。 */
  protocolVersion: number;
}

/** 当前生效的对接信息；`null` 表示尚未连上（或已道别）。 */
let endpoint: Endpoint | null = null;

/** 上次尝试读对接信息的时间戳（`Date.now()`）。 */
let lastProbeAt = 0;

/** 心跳定时器。 */
let pingTimer: ReturnType<typeof setInterval> | null = null;

/** 用户是否关掉了联动（`/pet off`）。 */
let enabled = true;

/** 最近一次底层 run 是否失败收场，由 `agent_end` 写入、`agent_settled` 读出。
 *
 * 两个事件分工的原因：`agent_end` 带 `messages`（能判定成败）但跑得太早（pi 还可能自动
 * 重试/压缩续跑），`agent_settled` 时机对但不带载荷。所以把判定结果暂存下来。 */
let lastRunFailed = false;

/** `~/.litepet` 目录，与 `src/config.rs` 的 `home_dir()` 一致。 */
function homeDir(): string {
  return process.env.LITEPET_HOME || join(homedir(), ".litepet");
}

/** 读 daemon 的对接信息；读不到就返回 `null`（不是异常）。 */
async function readEndpoint(): Promise<Endpoint | null> {
  try {
    const raw = await readFile(join(homeDir(), ENDPOINT_FILE), "utf8");
    const parsed = JSON.parse(raw) as Partial<Endpoint>;
    if (typeof parsed.port !== "number" || typeof parsed.token !== "string") {
      return null;
    }
    return {
      port: parsed.port,
      token: parsed.token,
      protocolVersion: parsed.protocolVersion ?? PROTOCOL_VERSION,
    };
  } catch {
    // 文件不存在 / 半截写入 / 权限不对——都不值得打扰用户。
    return null;
  }
}

/**
 * 确保有可用端点，必要时重新读一次对接信息。
 *
 * 这让「pi 先启动、daemon 后启动」也能自动接上，而不需要用户 `/reload`。
 * 失败时按 [`PROBE_RETRY_MS`] 节流，避免每次工具调用都去碰磁盘。
 *
 * @param now 当前时间戳，用于节流判断。
 * @returns 可用端点，或 `null`。
 */
async function ensureEndpoint(now: number): Promise<Endpoint | null> {
  if (endpoint !== null) {
    return endpoint;
  }
  if (now - lastProbeAt < PROBE_RETRY_MS) {
    return null;
  }
  lastProbeAt = now;
  endpoint = await readEndpoint();
  return endpoint;
}

/**
 * 发一次 JSON-RPC 调用。**永远不抛异常**。
 *
 * @param method 方法名。
 * @param params 参数对象。
 * @param request 为 `true` 时带上 `id`（期待应答），为 `false` 时是通知。
 * @returns 应答的 `result`，失败或无应答时为 `null`。
 */
async function rpc(
  method: string,
  params: Record<string, unknown>,
  request = false,
): Promise<unknown> {
  const now = Date.now();
  const target = await ensureEndpoint(now);
  if (target === null) {
    return null;
  }

  const body: Record<string, unknown> = { jsonrpc: "2.0", method, params };
  if (request) {
    body.id = now;
  }

  try {
    const response = await fetch(`http://127.0.0.1:${target.port}/rpc`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${target.token}`,
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    if (response.status === 401) {
      // token 变了（daemon 重启过）。丢掉端点，下次事件重新读对接信息。
      endpoint = null;
      return null;
    }
    if (!response.ok || !request) {
      return null;
    }
    const payload = (await response.json()) as { result?: unknown };
    return payload.result ?? null;
  } catch {
    // 连不上、超时、响应不是 JSON——一律静默。桌宠不值得打断 pi。
    return null;
  }
}

/** 发一个通知（不期待应答）。 */
function notify(method: string, params: Record<string, unknown>): Promise<unknown> {
  return rpc(method, params, false);
}

/** 发一个请求并取回 `result`。 */
function request(method: string, params: Record<string, unknown>): Promise<unknown> {
  return rpc(method, params, true);
}

/** 把文本压成单行并截到协议上限。 */
function toBubbleText(text: string): string {
  const oneLine = text.replace(/\s+/g, " ").trim();
  return oneLine.length > BUBBLE_LIMIT ? `${oneLine.slice(0, BUBBLE_LIMIT - 1)}…` : oneLine;
}

/**
 * 给一个工具调用生成气泡文本。
 *
 * 只展示**最能认出这件事**的那个参数（命令 / 路径 / 模式），而不是把整个参数
 * JSON 塞进去——气泡只有两行宽。返回 `undefined` 时 daemon 会退化成显示工具名。
 *
 * @param toolName 工具名。
 * @param args 工具参数（结构随工具而变，故按未知处理）。
 */
function bubbleFor(toolName: string, args: unknown): string | undefined {
  if (typeof args !== "object" || args === null) {
    return undefined;
  }
  const fields = args as Record<string, unknown>;
  const byTool: Record<string, string[]> = {
    bash: ["command"],
    read: ["path"],
    write: ["path"],
    edit: ["path"],
    grep: ["pattern", "path"],
    find: ["pattern", "path"],
  };
  const candidates = byTool[toolName];
  if (candidates === undefined) {
    return undefined;
  }
  for (const key of candidates) {
    const value = fields[key];
    if (typeof value === "string" && value.length > 0) {
      return toBubbleText(value);
    }
  }
  return undefined;
}

/**
 * 判断这一轮 agent 是否失败收场。
 *
 * 用最后一条 assistant 消息的 `stopReason` 判断（见 `docs/session-format.md:88`：
 * `"stop" | "length" | "toolUse" | "error" | "aborted"`）。
 * 认不出来时**报成功**：宠物「成功了」比宠物「出错了」更不容易误导用户。
 *
 * @param messages 本轮 run 的消息列表（来自 `agent_end` 的 `event.messages`）。
 */
function sessionSucceeded(messages: unknown): boolean {
  if (!Array.isArray(messages)) {
    return true;
  }
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const message = messages[i] as { role?: string; stopReason?: string } | null;
    if (message?.role !== "assistant") {
      continue;
    }
    return message.stopReason !== "error" && message.stopReason !== "aborted";
  }
  return true;
}

/** 会话标识：截取 session 文件名，没有就退化成 cwd。 */
function sessionIdOf(ctx: ExtensionContext): string | undefined {
  const file = ctx.sessionManager.getSessionFile();
  if (typeof file !== "string" || file.length === 0) {
    return undefined;
  }
  const name = file.split("/").pop();
  return name === undefined || name.length === 0 ? undefined : name;
}

/** 起心跳；重复调用无害。 */
function startHeartbeat(): void {
  if (pingTimer !== null) {
    return;
  }
  pingTimer = setInterval(() => {
    void notify("daemon/ping", { host: HOST, ts: Date.now() });
  }, PING_INTERVAL_MS);
  // 不要让心跳定时器拖住 pi 的退出。
  pingTimer.unref?.();
}

/** 停心跳并丢掉端点。 */
function stopHeartbeat(): void {
  if (pingTimer !== null) {
    clearInterval(pingTimer);
    pingTimer = null;
  }
  endpoint = null;
}

/** 注册全部事件与命令。 */
function register(pi: ExtensionAPI): void {
  pi.on("session_start", async (event, ctx) => {
    if (!enabled) {
      return;
    }
    const hello = await request("host/hello", {
      host: HOST,
      protocolVersion: PROTOCOL_VERSION,
      pid: process.pid,
      clientVersion: "pi-extension",
    });
    if (hello === null) {
      return;
    }
    startHeartbeat();
    if (ctx.hasUI && event.reason !== "startup") {
      ctx.ui.notify(`桌宠已接上（session ${event.reason}）`, "info");
    }
  });

  pi.on("agent_start", async () => {
    if (!enabled) {
      return;
    }
    lastRunFailed = false;
    await notify("agent/start", { host: HOST });
  });

  // `agent_end` 只用来判定成败：此刻上报会让宠物在 pi 还要自动续跑时就回到空闲。
  pi.on("agent_end", async (event) => {
    lastRunFailed = !sessionSucceeded(event.messages);
  });

  // `agent_settled` 才是「pi 不会再自动继续了」，是桌宠该动的时机。
  pi.on("agent_settled", async (_event, ctx) => {
    if (!enabled) {
      return;
    }
    await notify("agent/end", {
      host: HOST,
      sessionId: sessionIdOf(ctx),
      success: !lastRunFailed,
    });
  });

  pi.on("tool_execution_start", async (event) => {
    if (!enabled) {
      return;
    }
    const bubble = bubbleFor(event.toolName, event.args);
    await notify("tool/start", {
      host: HOST,
      toolName: event.toolName,
      ...(bubble === undefined ? {} : { bubble }),
    });
  });

  pi.on("tool_execution_end", async (event) => {
    if (!enabled) {
      return;
    }
    await notify("tool/end", {
      host: HOST,
      toolName: event.toolName,
      isError: event.isError === true,
    });
  });

  pi.on("session_shutdown", async () => {
    // 先道别再停心跳：道别本身也是一次通知，需要端点。
    await notify("host/bye", { host: HOST, reason: "pi session shutdown" });
    stopHeartbeat();
  });

  pi.registerCommand("pet", {
    description: "桌宠联动状态（/pet off 可暂停）",
    handler: async (args, ctx) => {
      const action = args.trim();
      if (action === "off") {
        enabled = false;
        await notify("host/bye", { host: HOST, reason: "user disabled" });
        stopHeartbeat();
      } else if (action === "on") {
        enabled = true;
        await request("host/hello", {
          host: HOST,
          protocolVersion: PROTOCOL_VERSION,
          pid: process.pid,
          clientVersion: "pi-extension",
        });
        startHeartbeat();
      } else if (action !== "") {
        ctx.ui.notify("用法：/pet [on|off]", "warning");
        return;
      }
      const info = await request("daemon/info", {});
      const state = enabled ? "已开启" : "已暂停";
      ctx.ui.notify(
        info === null ? `桌宠：${state}，daemon 未连接` : `桌宠：${state}，daemon=${JSON.stringify(info)}`,
        "info",
      );
    },
  });
}

/**
 * pi 扩展入口。
 *
 * 这个包是长驻的：`register` 里挂的事件处理器会在整个 pi 生命周期内生效，
 * 会话级资源（心跳定时器）由 `session_shutdown` 回收。
 */
export default function petExtension(pi: ExtensionAPI): void {
  register(pi);
}
