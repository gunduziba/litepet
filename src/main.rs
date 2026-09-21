//! pet-daemon 入口。
//!
//! 架构与约束见 `SPEC.md`，宠物包契约见 `docs/PET-PACK.md`，通信协议见 `docs/PROTOCOL.md`。
//!
//! # 线程模型
//!
//! ```text
//! [宿主进程 pi/dsh] --JSONL--> (daemon.rs accept 线程) --mpsc--> (session 线程)
//!                                                                    |
//!                                                        DisplayDirective
//!                                                                    v
//!                                                   Tauri 事件 "display" → ui/pet.js
//! ```
//!
//! session 线程是唯一的决策者；Tauri 主线程只跑事件循环与 `pack_info` 命令。

mod arbiter;
mod behavior;
mod config;
mod daemon;
mod pack;
mod protocol;
mod session;

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{Emitter, Manager};

/// daemon 自身版本，随 `host.welcome` 上报。
const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 渲染层与 session 线程共用的宠物包。
///
/// 用 `Arc` 是因为包在窗口启动后不再变化，两个线程只读。
struct PetState(Mutex<Option<Arc<pack::LoadedPet>>>);

/// 渲染层启动时拉取宠物包信息。
#[tauri::command]
fn pack_info(state: tauri::State<'_, PetState>) -> std::result::Result<pack::PetInfo, String> {
    let guard = state.0.lock().map_err(|err| err.to_string())?;
    guard
        .as_ref()
        .map(|loaded| loaded.info.clone())
        .ok_or_else(|| "宠物包尚未加载，请检查 ~/.litepet/pets".to_string())
}

/// 渲染层就绪握手：前端加载完宠物包后调用，便于确认宠物确实已上屏。
#[tauri::command]
fn renderer_ready(animations: usize) {
    println!("pet-daemon: 渲染层就绪，可用动画 {animations} 个");
}

/// 渲染层应用了一条指令后回报。
///
/// 这是验证「daemon → 事件 → webview → 实际切帧」这条链路的唯一手段：
/// webview 的 `console` 在终端里看不到，桌宠的典型故障恰好就是「窗口里什么都没有」，
/// 所以这里把渲染层确实执行了什么回写到 daemon 日志。
#[tauri::command]
fn renderer_applied(animation: String, bubble: Option<String>) {
    match bubble {
        Some(text) => println!("pet-daemon: 渲染层已应用 动画={animation} 气泡={text}"),
        None => println!("pet-daemon: 渲染层已应用 动画={animation}"),
    }
}

/// 选中要加载的包 id：优先配置指定，否则取 `pets/` 下第一个可用包。
fn pick_pack(root: &Path, preferred: Option<&str>) -> Option<String> {
    if let Some(id) = preferred {
        if root.join(id).join("pet.json").is_file() {
            return Some(id.to_string());
        }
        eprintln!("pet-daemon: 配置的宠物包不可用，回退到自动挑选：{id}");
    }
    let mut ids: Vec<String> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().join("pet.json").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids.into_iter().next()
}

/// 读取配置并加载宠物包。
fn try_init_pet() -> Result<pack::LoadedPet> {
    let (cfg, created) = config::load_or_init()?;
    if created {
        println!(
            "pet-daemon: 已初始化家目录 {}",
            config::home_dir()?.display()
        );
    }
    let root = config::pets_dir()?;
    let id = pick_pack(&root, cfg.pet.as_deref())
        .with_context(|| format!("{} 下没有可用宠物包（需含 pet.json）", root.display()))?;
    let loaded = pack::load(&root, &id)?;
    println!(
        "pet-daemon: 已加载宠物包 {}（{}），{} 个动画，网格 {}x{}x{}x{}",
        loaded.info.id,
        loaded.info.display_name,
        loaded.info.animations.len(),
        loaded.info.frame.width,
        loaded.info.frame.height,
        loaded.info.frame.columns,
        loaded.info.frame.rows,
    );
    match loaded.behavior.as_ref().and_then(|value| value.get("behavior")) {
        Some(_) => println!("pet-daemon: 已读取 petdaemon.behavior 扩展配置"),
        // 纯 Codex 包没有这个键，此时全走 §4.4 降级映射
        None => println!("pet-daemon: 无 petdaemon.behavior 扩展，使用 Codex 降级映射"),
    }
    Ok(loaded)
}

/// 加载失败只告警，不阻塞窗口启动。
fn init_pet() -> PetState {
    match try_init_pet() {
        Ok(loaded) => PetState(Mutex::new(Some(Arc::new(loaded)))),
        Err(err) => {
            eprintln!("pet-daemon: 宠物包加载失败：{err:#}");
            PetState(Mutex::new(None))
        }
    }
}

/// 启动 socket 会话线程。
///
/// 失败只告警：宠物仍应作为「没人说话的桌宠」正常显示。
fn spawn_session(
    app: tauri::AppHandle,
    loaded: &pack::LoadedPet,
    socket: Option<std::path::PathBuf>,
    resident: bool,
) {
    let behavior = loaded.behavior.clone();
    let known: BTreeSet<String> = loaded.info.animations.keys().cloned().collect();
    let socket = match socket.map_or_else(config::socket_path, Ok) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("pet-daemon: 无法定位 socket 路径，socket 服务未启动：{err:#}");
            return;
        }
    };

    // 必须 `Session::new` 先于 `Daemon::spawn`：后者会因单例冲突而失败退出。
    let mut session = match session::Session::new(behavior.as_ref(), known, resident) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("pet-daemon: 行为配置非法，socket 服务未启动：{err:#}");
            return;
        }
    };
    if !session.has_rules() {
        println!("pet-daemon: 该包无规则表，动画由 Codex 降级映射决定");
    }

    std::thread::spawn(move || {
        let daemon = match daemon::Daemon::spawn_at(socket, DAEMON_VERSION) {
            Ok(daemon) => daemon,
            Err(err) => {
                eprintln!("pet-daemon: socket 服务启动失败：{err:#}");
                return;
            }
        };
        println!("pet-daemon: 监听 {}", daemon.socket_path().display());

        loop {
            let timeout = session.next_deadline(Instant::now());
            let outcome = match daemon.recv_timeout(timeout) {
                Ok(Some(msg)) => session.on_msg(msg, Instant::now()),
                Ok(None) => session.tick(Instant::now()),
                Err(err) => {
                    eprintln!("pet-daemon: socket 读取失败：{err:#}");
                    session::Outcome::Exit
                }
            };
            match outcome {
                session::Outcome::Display(directive) => {
                    if let Err(err) = app.emit("display", *directive) {
                        eprintln!("pet-daemon: 推送渲染层失败：{err}");
                        break;
                    }
                }
                session::Outcome::Unchanged => {}
                session::Outcome::Exit => {
                    app.exit(0);
                    return;
                }
            }
        }
        app.exit(0);
    });
}

/// 解析命令行：目前只有 `--resident`。
fn parse_args() -> (bool, Option<std::path::PathBuf>) {
    let mut resident = false;
    let mut socket = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--resident" => resident = true,
            "--socket" => socket = args.next().map(std::path::PathBuf::from),
            "--help" | "-h" => {
                println!("pet-daemon [--resident] [--socket <path>]");
                std::process::exit(0);
            }
            other => {
                eprintln!("pet-daemon: 未知参数 {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }
    (resident, socket)
}

fn main() {
    let (resident, mut socket) = parse_args();
    if resident {
        println!("pet-daemon: 常驻模式，全部宿主断开后不退出");
    }
    tauri::Builder::default()
        .setup(move |app| {
            let state = init_pet();
            let loaded = state
                .0
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(Arc::clone));
            app.manage(state);
            if let Some(loaded) = loaded {
                spawn_session(app.handle().clone(), &loaded, socket.take(), resident);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pack_info,
            renderer_ready,
            renderer_applied
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
