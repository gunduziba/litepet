#!/usr/bin/env node
// 宿主模拟器：按 docs/PROTOCOL.md 演一遍完整会话，用来验证 daemon 侧行为。
//
// 它扮演一个真实宿主：读 ~/.litepet/daemon.json 拿到端口与 token，
// 然后往 http://127.0.0.1:<port>/rpc 发 JSON-RPC 2.0 请求。
//
// 用法：
//   node scripts/host-sim.mjs                      # 宿主名 pi，每步间隔 1.5s
//   node scripts/host-sim.mjs --host dsh
//   node scripts/host-sim.mjs --step 0             # 一口气跑完
//   node scripts/host-sim.mjs --keep-alive         # 演完不退出，持续心跳
//
// 它不只是「发完就退」：每步之间留出间隔，好让人眼确认宠物真的动了。

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/** 协议版本，与 src/protocol.rs 的 PROTOCOL_VERSION 保持一致。 */
const PROTOCOL_VERSION = 1;
/** 每步之间的默认间隔（ms）。 */
const DEFAULT_STEP_MS = 1500;
/** 默认宿主标识。 */
const DEFAULT_HOST = 'pi';
/** 心跳间隔（ms），协议建议 20s，模拟器用 5s 方便观察。 */
const PING_MS = 5000;
/** 等 daemon.json 出现的轮询间隔（ms）。 */
const POLL_MS = 100;
/** 等 daemon.json 出现的默认超时（ms）。 */
const DEFAULT_WAIT_MS = 20_000;

/** 协议里的方法名（与 src/protocol.rs 的 method 模块一致）。 */
const METHOD = {
  hostHello: 'host/hello',
  hostBye: 'host/bye',
  agentStart: 'agent/start',
  agentEnd: 'agent/end',
  agentSettled: 'agent/settled',
  toolStart: 'tool/start',
  toolEnd: 'tool/end',
  petBubble: 'pet/bubble',
  daemonPing: 'daemon/ping',
  daemonInfo: 'daemon/info',
};

/** 解析 `--key value` 形式的参数。 */
function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--help' || arg === '-h') {
      out.help = true;
    } else if (arg.startsWith('--')) {
      out[arg.slice(2)] = argv[i + 1] ?? '';
      i += 1;
    }
  }
  return out;
}

/** 家目录，与 src/config.rs 的 home_dir() 一致。 */
function homeDir() {
  return process.env.LITEPET_HOME || path.join(os.homedir(), '.litepet');
}

/** 对接信息文件路径，与 src/config.rs 的 endpoint_path() 一致。 */
function endpointPath() {
  return path.join(homeDir(), 'daemon.json');
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** 等 daemon.json 出现并读出内容。
 *
 * daemon 冷启动（Tauri 窗口 + 图集解析）可能要好几秒，固定 sleep 会随机失败。
 * 文件由 daemon 在「端口已绑定」之后才写，所以它出现就意味着可以连了。
 */
async function waitForEndpoint(file, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let lastError = '文件尚未出现';
  while (Date.now() < deadline) {
    try {
      const raw = fs.readFileSync(file, 'utf8');
      const parsed = JSON.parse(raw);
      // token 允许是空串：那是配置里 `auth.token` 为空（不鉴权），
      // 不是文件残缺。按真值判断会把「不鉴权」误报成「文件里缺 token」。
      if (typeof parsed.port === 'number' && typeof parsed.token === 'string') return parsed;
      lastError = '文件里缺 port 或 token 字段';
    } catch (err) {
      lastError = err.message;
    }
    await sleep(POLL_MS);
  }
  throw new Error(`等待 ${timeoutMs}ms 后仍读不到对接信息：${file}（${lastError}）`);
}

/** 一个 JSON-RPC 2.0 客户端。 */
class RpcClient {
  /** @param {string} url 端点地址 @param {string} token 访问密钥 */
  constructor(url, token) {
    this.url = url;
    this.token = token;
    /** 请求 id 计数器。 */
    this.nextId = 1;
  }

  /** 发一次调用；`notification` 为真时不带 id。 */
  async call(method, params = {}, { notification = false } = {}) {
    const body = { jsonrpc: '2.0', method, params };
    if (!notification) {
      body.id = this.nextId;
      this.nextId += 1;
    }
    const headers = { 'Content-Type': 'application/json' };
    // 不鉴权时 token 是空串，不发 `Authorization` 头。
    // 发一个 `Bearer ` 出去虽然也会被放行（daemon 那边直接不看这个头），
    // 但会让抓包和日志里多出一个假的凭据，误导以后查问题的人。
    if (this.token) headers.Authorization = `Bearer ${this.token}`;
    const response = await fetch(this.url, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });
    const text = await response.text();
    let parsed = null;
    try {
      parsed = text ? JSON.parse(text) : null;
    } catch {
      parsed = { raw: text };
    }
    return { status: response.status, kind: notification ? '通知' : '请求', body, parsed };
  }
}

/** 打印一次往返。 */
function trace({ kind, body, status, parsed }) {
  console.log(`→ [${kind}] ${JSON.stringify(body)}`);
  if (status === 204) {
    console.log('← 204 无响应体（通知按协议不回）');
  } else {
    console.log(`← ${status} ${JSON.stringify(parsed)}`);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const endpointFile = endpointPath();
  if (args.help) {
    console.log(`
用法：node scripts/host-sim.mjs [--host pi] [--step <ms>] [--wait <ms>] [--keep-alive]

  --host        宿主标识（默认 ${DEFAULT_HOST}）
  --step        每步间隔毫秒（默认 ${DEFAULT_STEP_MS}；设 0 可快速跑完）
  --wait        等对接信息出现的超时毫秒（默认 ${DEFAULT_WAIT_MS}）
  --keep-alive  演完后保持心跳，直到 Ctrl-C（用于观察宿主不掉线时的表现）

对接信息文件：${endpointFile}
`);
    return;
  }

  const host = args.host || DEFAULT_HOST;
  const step = args.step === undefined ? DEFAULT_STEP_MS : Number(args.step);
  const keepAlive = Boolean(args['keep-alive']);
  const waitMs = args.wait === undefined ? DEFAULT_WAIT_MS : Number(args.wait);

  const endpoint = await waitForEndpoint(endpointFile, waitMs);
  const url = `http://127.0.0.1:${endpoint.port}/rpc`;
  console.log(`已读取对接信息 ${endpointFile}`);
  console.log(`端点 ${url}（protocol=${endpoint.protocolVersion}，host=${host}）`);

  const client = new RpcClient(url, endpoint.token);

  /** 发一次请求并打印。 */
  const request = async (method, params) => {
    const outcome = await client.call(method, params);
    trace(outcome);
    return outcome.parsed;
  };

  /** 发一个通知并打印。 */
  const notify = async (method, params) => {
    trace(await client.call(method, params, { notification: true }));
  };

  // 握手必须是第一个请求（协议 §4.1）。
  await request(METHOD.hostHello, {
    host,
    protocolVersion: PROTOCOL_VERSION,
    pid: process.pid,
    agentVersion: 'sim-1.0.0',
    clientVersion: 'node-sim',
  });
  await sleep(step);

  // 一整轮 agent 生命周期 + 工具事件 + 自定气泡。
  await notify(METHOD.agentStart, { host, sessionId: 'sess-sim-1', summary: '验证桌宠联动' });
  await sleep(step);

  await notify(METHOD.toolStart, { host, toolName: 'bash', bubble: 'ls -la' });
  await sleep(step);

  await request(METHOD.daemonPing, { host, ts: Date.now() });
  await sleep(step);

  await notify(METHOD.toolEnd, { host, toolName: 'bash' });
  await sleep(step);

  await notify(METHOD.toolStart, { host, toolName: 'grep' });
  await sleep(step);

  await notify(METHOD.toolEnd, { host, toolName: 'grep', isError: true });
  await sleep(step);

  await notify(METHOD.petBubble, {
    host,
    kind: 'warning',
    text: '这是一条宿主自定气泡',
    ttlMs: 3000,
  });
  await sleep(step);

  // 未知通知必须被静默忽略（协议 §3 前向兼容）。
  await notify('future/unknown', { anything: true });
  await sleep(step);

  // 未知请求必须回 -32601，宿主才好发现自己写错了方法名。
  await request('pet/teleport', { host });
  await sleep(step);

  await request(METHOD.daemonInfo, {});
  await sleep(step);

  await notify(METHOD.agentEnd, { host, sessionId: 'sess-sim-1', success: true });
  await sleep(step * 2);

  // 真宿主的顺序是 agent.end 紧跟着 agent.settled（pi 的 agent_end → agent_settled）。
  // 两者都必须发：agent.end 只说明「本轮完了」，agent.settled 才说明「不会再自己接着干」。
  // 默认提醒（出声 + 系统通知）挂在后者上，所以漏掉它就等于没验证提醒链路。
  await notify(METHOD.agentSettled, { host, sessionId: 'sess-sim-1' });
  await sleep(step);

  if (keepAlive) {
    console.log('保持心跳中，Ctrl-C 结束');
    setInterval(() => {
      void request(METHOD.daemonPing, { host, ts: Date.now() });
    }, PING_MS);
    return;
  }

  console.log('道别（daemon 应立即注销本宿主）');
  await notify(METHOD.hostBye, { host, reason: '模拟器收工' });
}

main().catch((err) => {
  console.error(err.message ?? err);
  process.exit(1);
});
