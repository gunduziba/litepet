#!/usr/bin/env node
// 宿主模拟器：按 docs/PROTOCOL.md 演一遍完整会话，用来验证 daemon 侧行为。
//
// 用法：
//   node scripts/host-sim.mjs                       # 默认 socket、宿主名 pi
//   node scripts/host-sim.mjs --host dsh
//   node scripts/host-sim.mjs --socket /tmp/x.sock --step 800
//
// 它不只是「发完就退」：每步之间留出间隔，好让人眼确认宠物真的动了。

import net from 'node:net';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

/** 协议版本，与 src/protocol.rs 的 PROTOCOL_VERSION 保持一致。 */
const PROTOCOL_VERSION = 1;
/** 默认心跳间隔（ms），协议 §7 要求宿主每 5s 一次 ping。 */
const DEFAULT_STEP_MS = 1500;
/** 默认宿主标识。 */
const DEFAULT_HOST = 'pi';
/** 等 socket 出现的轮询间隔（ms）。 */
const POLL_MS = 100;
/** 等 socket 出现的超时（ms）。 */
const DEFAULT_WAIT_MS = 20_000;

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

/** 默认 socket 路径，与 src/config.rs 的 socket_path() 一致。 */
function defaultSocket() {
  const home = process.env.LITEPET_HOME || path.join(os.homedir(), '.litepet');
  return path.join(home, 'daemon.sock');
}

/** 造一帧：自动补齐 `v` / `type` / `host`。 */
function frame(host, type, payload = {}) {
  return `${JSON.stringify({ v: PROTOCOL_VERSION, type, host, ...payload })}\n`;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** 等 socket 文件出现。
 *
 * daemon 冷启动（Tauri 窗口 + 图集解析）可能要好几秒，固定 sleep 会随机失败。
 */
async function waitForSocket(socket, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(socket)) return true;
    await sleep(POLL_MS);
  }
  return false;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(`
用法：node scripts/host-sim.mjs [--host pi] [--socket <path>] [--step <ms>] [--keep-alive]

  --host       宿主标识（默认 ${DEFAULT_HOST}）
  --socket     socket 路径（默认 ${defaultSocket()}）
  --step       每步间隔毫秒（默认 ${DEFAULT_STEP_MS}；设 0 可快速跑完）
  --wait       等 socket 出现的超时毫秒（默认 ${DEFAULT_WAIT_MS}）
  --keep-alive 演完后保持连接，直到 Ctrl-C（用于观察 linger 行为）
`);
    return;
  }

  const host = args.host || DEFAULT_HOST;
  const socket = args.socket || defaultSocket();
  const step = args.step === undefined ? DEFAULT_STEP_MS : Number(args.step);
  const keepAlive = Boolean(args['keep-alive']);
  const waitMs = args.wait === undefined ? DEFAULT_WAIT_MS : Number(args.wait);

  if (!(await waitForSocket(socket, waitMs))) {
    console.error(`等待 ${waitMs}ms 后仍未出现 socket：${socket}（daemon 未启动？）`);
    process.exit(1);
  }

  const stream = net.connect(socket);
  let buffer = '';

  stream.on('data', (chunk) => {
    buffer += chunk.toString('utf8');
    let index = buffer.indexOf('\n');
    while (index >= 0) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      if (line.trim()) console.log(`← ${line}`);
      index = buffer.indexOf('\n');
    }
  });
  stream.on('error', (err) => {
    console.error(`连接失败：${err.message}`);
    process.exitCode = 1;
  });

  await new Promise((resolve, reject) => {
    stream.once('connect', resolve);
    stream.once('error', reject);
  });
  console.log(`已连接 ${socket}（host=${host}）`);

  /** 发一帧并打印。 */
  const send = (type, payload) => {
    const line = frame(host, type, payload);
    console.log(`→ ${line.trim()}`);
    stream.write(line);
  };

  // 握手必须是第一帧（协议 §4.1）。
  send('host.hello', { pid: process.pid, agentVersion: 'sim-1.0.0', clientVersion: 'node-sim' });
  await sleep(step);

  // 一整轮 agent 生命周期 + 工具事件 + 自定气泡。
  send('agent.start', { sessionId: 'sess-sim-1', summary: '验证桌宠联动' });
  await sleep(step);

  send('tool.start', { toolName: 'bash', bubble: 'ls -la' });
  await sleep(step);

  send('ping', { ts: Date.now() });
  await sleep(step);

  send('tool.end', { toolName: 'bash' });
  await sleep(step);

  send('tool.start', { toolName: 'grep' });
  await sleep(step);

  send('tool.end', { toolName: 'grep', isError: true });
  await sleep(step);

  send('bubble', { kind: 'warning', text: '这是一条宿主自定气泡', ttlMs: 3000 });
  await sleep(step);

  // 未知 type 必须被静默忽略（协议 §3 前向兼容）。
  send('future.unknown', { anything: true });
  await sleep(step);

  send('agent.end', { sessionId: 'sess-sim-1', success: true });
  await sleep(step * 2);

  if (keepAlive) {
    console.log('保持连接中，Ctrl-C 结束');
    setInterval(() => send('ping', { ts: Date.now() }), 5000);
    return;
  }

  console.log('关闭连接（daemon 应立即注销本宿主，进入 linger）');
  stream.end();
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
