//! litepet 入口。
//!
//! 架构与约束见 `SPEC.md`，宠物包契约见 `docs/PET-PACK.md`，通信协议见 `docs/PROTOCOL.md`。
//!
//! # 线程模型
//!
//! ```text
//! [宿主进程 pi/dsh] --HTTP POST /rpc--> (http.rs 工作线程 ×4) --+
//!                                                          |
//!                                            Mutex<Session> | 定时空转
//!                                                          v
//!                                                 (空转线程, tick)
//!                                                          |
//!                                                 DisplayDirective
//!                                                          v
//!                                        Tauri 事件 "display" → ui/pet.js
//! ```
//!
//! `Session` 是唯一的决策者，被工作线程与空转线程共享；
//! Tauri 主线程只跑事件循环与几个命令。

mod arbiter;
mod behavior;
mod config;
mod http;
mod jsonrpc;
mod pack;
mod protocol;
mod session;

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

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
    println!("litepet: 渲染层就绪，可用动画 {animations} 个");
}

/// 渲染层应用了一条指令后回报。
///
/// 这是验证「daemon → 事件 → webview → 实际切帧」这条链路的唯一手段：
/// webview 的 `console` 在终端里看不到，桌宠的典型故障恰好就是「窗口里什么都没有」，
/// 所以这里把渲染层确实执行了什么回写到 daemon 日志。
#[tauri::command]
fn renderer_applied(animation: String, bubble: Option<String>) {
    match bubble {
        Some(text) => println!("litepet: 渲染层已应用 动画={animation} 气泡={text}"),
        None => println!("litepet: 渲染层已应用 动画={animation}"),
    }
}

/// 选中要加载的包 id：优先配置指定，否则取 `pets/` 下第一个可用包。
fn pick_pack(root: &Path, preferred: Option<&str>) -> Option<String> {
    if let Some(id) = preferred {
        if root.join(id).join("pet.json").is_file() {
            return Some(id.to_string());
        }
        eprintln!("litepet: 配置的宠物包不可用，回退到自动挑选：{id}");
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

/// 读取配置并加载宠物包，顺带把本次启动要用的端口带出来。
fn try_init_pet() -> Result<(pack::LoadedPet, u16)> {
    let (cfg, created) = config::load_or_init()?;
    if created {
        println!(
            "litepet: 已初始化家目录 {}",
            config::home_dir()?.display()
        );
    }
    let root = config::pets_dir()?;
    let id = pick_pack(&root, cfg.pet.as_deref())
        .with_context(|| format!("{} 下没有可用宠物包（需含 pet.json）", root.display()))?;
    let loaded = pack::load(&root, &id)?;
    println!(
        "litepet: 已加载宠物包 {}（{}），{} 个动画，网格 {}x{}x{}x{}",
        loaded.info.id,
        loaded.info.display_name,
        loaded.info.animations.len(),
        loaded.info.frame.width,
        loaded.info.frame.height,
        loaded.info.frame.columns,
        loaded.info.frame.rows,
    );
    match loaded.behavior.as_ref().and_then(|value| value.get("behavior")) {
        Some(_) => println!("litepet: 已读取 litepet.behavior 扩展配置"),
        // 纯 Codex 包没有这个键，此时全走 §4.4 降级映射
        None => println!("litepet: 无 litepet.behavior 扩展，使用 Codex 降级映射"),
    }
    Ok((loaded, cfg.port))
}

/// 加载宠物包；失败只告警，不阻塞窗口启动。
///
/// 返回的端口在加载失败时没有意义（此时也不会有 HTTP 服务），
/// 用默认值填上只是让调用方不必处理 `Option`。
fn init_pet() -> (PetState, u16) {
    match try_init_pet() {
        Ok((loaded, port)) => (PetState(Mutex::new(Some(Arc::new(loaded)))), port),
        Err(err) => {
            eprintln!("litepet: 宠物包加载失败：{err:#}");
            (PetState(Mutex::new(None)), config::DEFAULT_PORT)
        }
    }
}

/// 启动 HTTP + JSON-RPC 服务。
///
/// 行为配置非法、生成 token 失败、写对接信息失败都只告警：宠物仍应作为
/// 「没人说话的桌宠」正常显示。**但端口抢占失败是例外**——那是单例约束，直接退出。
fn start_http(app: tauri::AppHandle, loaded: &pack::LoadedPet, port: u16, resident: bool) {
    let setup = session::Setup {
        pet_id: loaded.info.id.clone(),
        known: loaded.info.animations.keys().cloned().collect(),
        litepet: loaded.behavior.clone(),
        resident,
    };
    let session = match session::Session::new(setup) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("litepet: 行为配置非法，HTTP 服务未启动：{err:#}");
            return;
        }
    };
    if !session.has_rules() {
        println!("litepet: 该包无规则表，动画由 Codex 降级映射决定");
    }

    // 先占端口再写对接信息：绑定失败就别留下一个指向死端口的文件。
    let server = match http::listen(port) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("litepet: {err:#}");
            // 单例是硬性约束（SPEC §0 约束 2）：没抢到端口就不能再开一只宠物。
            // 如果只是打条日志就继续跑，结果是一个永远收不到任何事件的僵尸窗口，
            // 而且它看起来和正常实例一模一样，最难排查。所以这里必须退出。
            if http::instance_running(port) {
                println!("litepet: 已有实例在监听 127.0.0.1:{port}，本进程退出（单例）");
                std::process::exit(0);
            }
            eprintln!("litepet: 端口 {port} 上没有任何服务在监听，无法继续");
            std::process::exit(1);
        }
    };
    let token = match config::random_token() {
        Ok(token) => token,
        Err(err) => {
            eprintln!("litepet: {err:#}");
            return;
        }
    };
    match config::write_endpoint(protocol::PROTOCOL_VERSION, port, &token) {
        Ok(path) => println!(
            "litepet: 监听 http://127.0.0.1:{port}{}（对接信息 {}）",
            http::RPC_PATH,
            path.display()
        ),
        Err(err) => {
            eprintln!("litepet: {err:#}");
            return;
        }
    }

    let exit_app = app.clone();
    let hooks = Arc::new(http::Hooks {
        display: Box::new(move |directive| {
            if let Err(err) = app.emit("display", directive) {
                eprintln!("litepet: 推送渲染层失败：{err}");
            }
        }),
        exit: Box::new(move || {
            // 正常退出时收拾对接信息；失败只提示，不影响退出。
            if let Err(err) = config::remove_endpoint() {
                eprintln!("litepet: {err:#}");
            }
            exit_app.exit(0);
        }),
    });

    // 会话交给工作线程与空转线程共享，主线程不再碰它。
    http::serve(server, Arc::new(Mutex::new(session)), token, hooks);
}

/// 解析命令行：`--resident` 与 `--port`。
fn parse_args() -> (bool, Option<u16>) {
    let mut resident = false;
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--resident" => resident = true,
            "--port" => port = args.next().and_then(|raw| raw.parse().ok()),
            "--help" | "-h" => {
                println!("litepet [--resident] [--port <端口>]");
                std::process::exit(0);
            }
            other => {
                eprintln!("litepet: 未知参数 {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }
    (resident, port)
}

fn main() {
    let (resident, port) = parse_args();
    if resident {
        println!("litepet: 常驻模式，全部宿主断开后不退出");
    }
    tauri::Builder::default()
        .setup(move |app| {
            let (state, configured_port) = init_pet();
            let loaded = state
                .0
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(Arc::clone));
            app.manage(state);
            if let Some(loaded) = loaded {
                // 命令行优先于配置文件。
                let port = port.unwrap_or(configured_port);
                start_http(app.handle().clone(), &loaded, port, resident);
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
