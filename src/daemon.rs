//! socket 服务层：Unix domain socket + JSONL 帧（`docs/PROTOCOL.md` §1–§4）。
//!
//! 本模块只负责「把字节变成消息」以及「把回包写回去」，不做任何状态机决策：
//! 决策在 [`crate::session`]。这样协议层可以脱离 Tauri 单测。
//!
//! 协议性质（`docs/PROTOCOL.md` §2）决定了这里的分工：
//! - `host.hello` / `ping` 是请求-应答，**在连接线程内直接回包**，不经过状态机；
//! - 其余全是单向事件通知，只上报、不回包，宿主不得等待响应。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::protocol::{self, Envelope, Hello};

/// 读超时：超过这个时间没收到**任何**帧即判定宿主死亡（`docs/PROTOCOL.md` §7，
/// 宿主每 5s 一次 `ping`，容忍 3 次丢失）。
pub const HOST_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// socket 文件权限：仅属主可读写。
const SOCKET_MODE: u32 = 0o600;

/// 没有 `host` 字段时在日志与回包里用的占位名。
const UNKNOWN_HOST: &str = "unknown";

/// socket 线程推给主线程的消息。
#[derive(Debug)]
pub enum DaemonMsg {
    /// 一条连接完成 `host.hello` 握手。
    Connected {
        /// 宿主标识。
        host: String,
        /// 宿主进程 pid。
        pid: u32,
        /// 宿主声明的 agent 版本。
        agent_version: Option<String>,
        /// 宿主声明的客户端版本。
        client_version: Option<String>,
        /// 握手完成时刻。
        at: Instant,
    },
    /// 已握手宿主发来的一帧（`ping` 也在内，用于刷新存活时间）。
    Frame {
        /// 宿主标识。
        host: String,
        /// 原始信封。
        envelope: Envelope,
        /// 收到时刻。
        at: Instant,
    },
    /// 宿主断开（`docs/PROTOCOL.md` §0：立即注销，不留僵尸状态）。
    Closed {
        /// 宿主标识。
        host: String,
    },
    /// 监听线程致命错误；主线程应随之退出。
    Failed {
        /// 失败原因。
        reason: String,
    },
}

/// 一条已建立连接的共享状态。
///
/// `UnixStream` 对 `&Self` 既实现了 `Read` 又实现了 `Write`，所以一个 fd
/// 就能读写共用，无需 `try_clone`；`Arc` 让注册表能跨线程踢掉被顶替的连接。
struct Conn {
    /// 连接本体。
    stream: UnixStream,
    /// 被新连接顶替的标记：置位后本连接不再上报 `Closed`，
    /// 否则会把刚接管的新宿主误注销。
    superseded: AtomicBool,
    /// 写锁：注册表线程（踢旧连接）与连接线程（回 `pong`）可能同时写，
    /// 保证一帧不会与另一帧交错。
    write_lock: Mutex<()>,
}

impl Conn {
    /// 写一帧 JSONL。
    fn write_frame(&self, frame: &Value, what: &str) -> Result<()> {
        let mut line = serde_json::to_string(frame).context("序列化帧失败")?;
        line.push('\n');
        let _guard = self.write_lock.lock().expect("写锁未中毒");
        let mut stream = &self.stream;
        stream
            .write_all(line.as_bytes())
            .with_context(|| format!("写 {what} 失败"))?;
        stream.flush().with_context(|| format!("flush {what} 失败"))?;
        Ok(())
    }

    /// 标记顶替，回 `host.evicted` 并关闭连接。
    fn supersede(&self, host: &str, reason: &str) {
        self.superseded.store(true, Ordering::Relaxed);
        let _ = self.write_frame(&protocol::evicted(reason, host), "host.evicted");
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// 宿主名 → 连接；用于同名重连时踢掉旧连接。
type Registry = Arc<Mutex<HashMap<String, Arc<Conn>>>>;

/// 已启动的 socket 服务。
pub struct Daemon {
    /// socket 文件路径。
    socket: PathBuf,
    /// 消息接收端。
    rx: Receiver<DaemonMsg>,
    /// 监听句柄；drop 即让 accept 线程结束。
    listener: Option<UnixListener>,
}

impl Daemon {
    /// 在指定路径启动服务；测试用。
    ///
    /// 单例锁：socket 文件已存在且能连通 → 说明已有 daemon 在跑，直接报错。
    pub fn spawn_at(socket: PathBuf, version: &str) -> Result<Self> {
        let listener = acquire_socket(&socket)?;
        let (tx, rx) = channel();
        let worker = listener.try_clone().context("复制监听句柄失败")?;
        let tx_failed = tx.clone();
        // 先转成 owned，避免闭包借用 `version`。
        let version = version.to_string();
        thread::Builder::new()
            .name("pet-daemon-accept".to_string())
            .spawn(move || {
                if let Err(err) = accept_loop(worker, tx, version) {
                    let _ = tx_failed.send(DaemonMsg::Failed {
                        reason: format!("{err:#}"),
                    });
                }
            })
            .context("启动监听线程失败")?;
        Ok(Self {
            socket,
            rx,
            listener: Some(listener),
        })
    }

    /// 带超时取一条消息：超时返回 `Ok(None)`，线程退出返回 `Err`。
    ///
    /// 事件循环靠它把 socket 等待与「定时空转」合成一个阻塞点。
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<DaemonMsg>> {
        match self.rx.recv_timeout(timeout) {
            Ok(msg) => Ok(Some(msg)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => bail!("socket 线程已退出"),
        }
    }

    /// socket 文件路径。
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // 关掉监听句柄即可让 accept 线程从 incoming() 退出。
        self.listener = None;
    }
}

/// 取得监听句柄，并实现单例锁与陈旧文件清理。
fn acquire_socket(socket: &Path) -> Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 socket 目录失败：{}", parent.display()))?;
    }
    if socket.exists() {
        match UnixStream::connect(socket) {
            // 能连上说明真有一个 daemon 在跑。
            Ok(stream) => {
                drop(stream);
                bail!("另一只 pet-daemon 已在运行（{}）", socket.display());
            }
            // 连不上说明是上次崩溃残留的陈旧文件，清掉重来。
            Err(_) => {
                std::fs::remove_file(socket)
                    .with_context(|| format!("清理陈旧 socket 失败：{}", socket.display()))?;
            }
        }
    }
    let listener = UnixListener::bind(socket).context("绑定 socket 失败")?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(SOCKET_MODE))
        .context("设置 socket 权限失败")?;
    Ok(listener)
}

/// 接受连接的主循环。
fn accept_loop(listener: UnixListener, tx: Sender<DaemonMsg>, version: String) -> Result<()> {
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU64::new(0));
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            // 单条连接失败不该拖垮整个 daemon。
            Err(err) => {
                eprintln!("pet-daemon: 接受连接失败：{err}");
                continue;
            }
        };
        let id = counter.fetch_add(1, Ordering::Relaxed);
        let tx = tx.clone();
        let registry = Arc::clone(&registry);
        let version = version.clone();
        let spawned = thread::Builder::new()
            .name(format!("pet-daemon-conn-{id}"))
            .spawn(move || {
                if let Err(err) = serve(stream, tx, registry, &version) {
                    eprintln!("pet-daemon: 连接 {id} 异常结束：{err:#}");
                }
            });
        if let Err(err) = spawned {
            eprintln!("pet-daemon: 启动连接线程失败：{err}");
        }
    }
    Ok(())
}

/// 单条连接的生命周期。
fn serve(
    stream: UnixStream,
    tx: Sender<DaemonMsg>,
    registry: Registry,
    version: &str,
) -> Result<()> {
    stream
        .set_read_timeout(Some(HOST_READ_TIMEOUT))
        .context("设置读超时失败")?;
    let conn = Arc::new(Conn {
        stream,
        superseded: AtomicBool::new(false),
        write_lock: Mutex::new(()),
    });

    let mut lines = BufReader::new(&conn.stream).lines();
    let Some(host) = handshake(&mut lines, &conn, &registry, &tx, version)? else {
        return Ok(());
    };

    for line in lines {
        let line = match line {
            Ok(line) => line,
            // 读超时 = 心跳超时判死；EOF 会走 None 分支。
            Err(err) if is_timeout(&err) => {
                eprintln!(
                    "pet-daemon: 宿主 {host} 心跳超时（{}s 无帧），断开",
                    HOST_READ_TIMEOUT.as_secs()
                );
                break;
            }
            Err(err) => {
                eprintln!("pet-daemon: 宿主 {host} 读取失败：{err}");
                break;
            }
        };
        let Some(envelope) = protocol::parse_frame(&line) else {
            // 空行/坏帧按协议静默忽略，不得断连、不得报错退出。
            continue;
        };
        if envelope.kind == "ping" {
            let ts = envelope
                .decode::<protocol::Ping>()
                .map(|ping| ping.ts)
                .unwrap_or_default();
            conn.write_frame(&protocol::pong(ts, &host), "pong")?;
        }
        if tx
            .send(DaemonMsg::Frame {
                host: host.clone(),
                envelope,
                at: Instant::now(),
            })
            .is_err()
        {
            // 主线程已退出，收尾即可。
            break;
        }
    }

    registry.lock().expect("注册表锁未中毒").remove(&host);
    if !conn.superseded.load(Ordering::Relaxed) {
        let _ = tx.send(DaemonMsg::Closed { host });
    }
    Ok(())
}

/// 处理第一帧：必须是 `host.hello`。
///
/// 返回 `Ok(None)` 表示连接已被拒绝（已发 `host.evicted`），调用方直接收尾。
fn handshake(
    lines: &mut std::io::Lines<BufReader<&UnixStream>>,
    conn: &Arc<Conn>,
    registry: &Registry,
    tx: &Sender<DaemonMsg>,
    version: &str,
) -> Result<Option<String>> {
    let Some(line) = lines.next() else {
        return Ok(None);
    };
    let line = line.context("读取首帧失败")?;
    let Some(envelope) = protocol::parse_frame(&line) else {
        evict(conn, UNKNOWN_HOST, "首帧不是合法 JSON");
        return Ok(None);
    };
    let claimed = envelope.host.as_deref().unwrap_or(UNKNOWN_HOST);
    if envelope.kind != "host.hello" {
        evict(conn, claimed, "连接后第一帧必须是 host.hello");
        return Ok(None);
    }
    if envelope.v != protocol::PROTOCOL_VERSION {
        evict(
            conn,
            claimed,
            &format!(
                "协议版本不受支持：收到 {}，本 daemon 只支持 {}",
                envelope.v,
                protocol::PROTOCOL_VERSION
            ),
        );
        return Ok(None);
    }
    let host = match envelope.host.as_deref().map(str::trim) {
        Some(host) if !host.is_empty() => host.to_string(),
        _ => {
            evict(conn, UNKNOWN_HOST, "host.hello 缺少非空的 host 字段");
            return Ok(None);
        }
    };
    let Some(hello) = envelope.decode::<Hello>() else {
        evict(conn, &host, "host.hello 的 pid 字段缺失或非法");
        return Ok(None);
    };

    // 同名宿主重连：踢掉旧连接，由新连接接管。
    // 注册表存的是**同一个** `Arc<Conn>`，所以 `supersede` 置位的正是
    // 本连接线程稍后会读的那个标记 ﹣ 旧连接因此不会误报 `Closed`。
    let previous = registry
        .lock()
        .expect("注册表锁未中毒")
        .insert(host.clone(), Arc::clone(conn));
    if let Some(old) = previous {
        eprintln!("pet-daemon: 宿主 {host} 重复连接，踢掉旧连接");
        old.supersede(&host, "同名宿主已有更新的连接接管，本连接被顶替");
    }

    conn.write_frame(
        &protocol::welcome(version, &host, &host),
        "host.welcome",
    )?;
    let _ = tx.send(DaemonMsg::Connected {
        host: host.clone(),
        pid: hello.pid,
        agent_version: hello.agent_version,
        client_version: hello.client_version,
        at: Instant::now(),
    });
    Ok(Some(host))
}

/// 发送 `host.evicted` 并关闭连接。
fn evict(conn: &Conn, host: &str, reason: &str) {
    eprintln!("pet-daemon: 拒绝连接（host={host}）：{reason}");
    let _ = conn.write_frame(&protocol::evicted(reason, host), "host.evicted");
    let _ = conn.stream.shutdown(std::net::Shutdown::Both);
}

/// 判断 IO 错误是否为读超时。
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}
