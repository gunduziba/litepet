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

mod alert;
mod arbiter;
mod behavior;
mod config;
mod http;
mod jsonrpc;
mod logging;
mod pack;
mod presence;
mod protocol;
mod session;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};

use alert::desktop::TauriNotifier;
use alert::push::{BarkPusher, Pusher, SilentPusher};
use alert::sound::RodioSpeaker;
use alert::{AlertSpec, Alerter, Channels, Request};
use config::NotifyConfig;
use jsonrpc::ErrorObject;
use presence::Presence;

/// 窗口标签：宠物主窗口（与 `tauri.conf.json` 里那条一致）。
const MAIN_WINDOW: &str = "pet";
/// 窗口标签：设置页（按需创建的独立窗口）。
const CONFIG_WINDOW: &str = "config";
/// 托盘图标 id。
const TRAY_ID: &str = "main";

/// 渲染层与 session 线程共用的宠物包。
///
/// 里层 `Arc` 让「换宠物」变成一次指针替换：会话线程与渲染层不会读到半个包。
/// 外层再包一层 `Arc<Mutex<…>>`，是为了让 [`DaemonState`] 与 Tauri `State` 里的
/// **是同一份**——换包必须两边同时可见，各存一份迟早不一致。
struct PetState(Arc<Mutex<Option<Arc<pack::LoadedPet>>>>);

/// daemon 级方法出口的共享句柄。
///
/// [`DaemonState`] 本身不 `Clone`（里面攢着 `AppHandle` 与几把锁），
/// 包一层 `Arc` 才能既给 HTTP 工作线程、又给 Tauri 命令用。
struct DaemonHandle(Arc<DaemonState>);

/// 设置页专用：直接调 daemon 级方法，不经过 HTTP。
///
/// 为什么不让设置页直接去 `fetch` 那个 HTTP 端口：那就得把 token 交给 webview，
/// 而 token 是给**外部进程**用的凭证。绕开它这里走同一条出口，
/// 于是「哪些方法属于哪一层」只有一处定义。
///
/// 刻意只放行 daemon 层（`pet/*`、`config/*`、`notify/test`）：`host/*` 与 `agent/*`
/// 是外部进程驱动宠物的入口，从窗口里把它们敞开等于把鉴权作废。
#[tauri::command]
fn local_call(
    daemon: tauri::State<'_, DaemonHandle>,
    method: String,
    params: Option<Value>,
) -> std::result::Result<Value, String> {
    match http::call_daemon(daemon.0.as_ref(), &method, params.as_ref()) {
        Some(Ok(value)) => Ok(value),
        Some(Err(err)) => Err(err.message),
        None => Err(format!("{method} 不是设置页可以调用的方法")),
    }
}

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
    log::info!("渲染层就绪，可用动画 {animations} 个");
}

/// 渲染层应用了一条指令后回报。
///
/// 这是验证「daemon → 事件 → webview → 实际切帧」这条链路的唯一手段：
/// webview 的 `console` 在终端里看不到，桌宠的典型故障恰好就是「窗口里什么都没有」，
/// 所以这里把渲染层确实执行了什么回写到 daemon 日志。
#[tauri::command]
fn renderer_applied(animation: String, bubble: Option<String>) {
    match bubble {
        Some(text) => log::info!("渲染层已应用 动画={animation} 气泡={text}"),
        None => log::info!("渲染层已应用 动画={animation}"),
    }
}

/// App 包里自带宠物的资源目录名。
///
/// `tauri.conf.json` 的 `bundle.resources` 把仓库的 `assets/pets` 铺到它的
/// `Contents/Resources/pets`，所以这里只写叶子名。资源是**散文件**而不是嵌进
/// 二进制：图集只有换包时才变，没必要每次升级都重新分发一份（`docs/PET-PACK.md` §7.3）。
const BUNDLED_PETS: &str = "pets";

/// 首次初始化家目录时，把 App 自带的宠物铺进 `~/.litepet/pets/`。
///
/// 只在**初始化**这一次做：用户把包删掉就是不想再看见它，每次启动都补回来会很烦。
/// 同理也**不覆盖**已有文件——用户可能换过图、改过名字，升级安装不该把他的改动冲掉。
fn seed_default_pets(app: &tauri::AppHandle) {
    let source = match app.path().resource_dir() {
        Ok(dir) => dir.join(BUNDLED_PETS),
        Err(err) => {
            log::warn!("取不到资源目录，自带的宠物不会被铺出来：{err}");
            return;
        }
    };
    let Ok(destination) = config::pets_dir() else {
        log::warn!("取不到宠物目录，自带的宠物不会被铺出来");
        return;
    };
    if !source.is_dir() {
        // 开发时直接 `cargo run`（没经打包流程）就会走到这里，不是错误。
        log::warn!("App 里没有自带宠物（{}），跳过", source.display());
        return;
    }
    match copy_missing_pack_files(&source, &destination) {
        Ok(0) => log::info!("自带宠物都已在位，未做改动"),
        Ok(count) => log::info!("已铺出 {count} 个自带宠物文件到 {}", destination.display()),
        Err(err) => log::warn!("铺出自带宠物失败：{err:#}"),
    }
}

/// 把 `source` 下的宠物包逐个补到 `destination`，只补**不存在**的文件，返回补了几个。
///
/// 跳过点开头的文件：`pets/` 里最常见的垃圾是 macOS 自动生成的 `.DS_Store`，
/// 它跟宠物包毫无关系，不该跟着资源进用户目录。
fn copy_missing_pack_files(source: &Path, destination: &Path) -> Result<usize> {
    let mut copied = 0;
    let packs = std::fs::read_dir(source)
        .with_context(|| format!("读自带宠物目录失败：{}", source.display()))?;
    for pack in packs.flatten() {
        if !pack.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let target_pack = destination.join(pack.file_name());
        let entries = std::fs::read_dir(pack.path())
            .with_context(|| format!("读自带宠物包失败：{}", pack.path().display()))?;
        for entry in entries.flatten() {
            let is_dotfile = entry.file_name().to_string_lossy().starts_with('.');
            let is_file = entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false);
            if is_dotfile || !is_file {
                continue;
            }
            let target = target_pack.join(entry.file_name());
            if target.exists() {
                continue;
            }
            std::fs::create_dir_all(&target_pack)
                .with_context(|| format!("创建宠物包目录失败：{}", target_pack.display()))?;
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("复制 {} 失败", entry.path().display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// 选中要加载的包 id：优先配置指定，否则取 `pets/` 下第一个可用包。
fn pick_pack(root: &Path, preferred: Option<&str>) -> Option<String> {
    if let Some(id) = preferred {
        if root.join(id).join("pet.json").is_file() {
            return Some(id.to_string());
        }
        log::warn!("配置的宠物包不可用，回退到自动挑选：{id}");
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
fn try_init_pet(app: &tauri::AppHandle) -> Result<(pack::LoadedPet, u16)> {
    let (cfg, created) = config::load_or_init()?;
    if created {
        log::info!("已初始化家目录 {}", config::home_dir()?.display());
        // 必须在 `pick_pack` 之前：首次启动时 `pets/` 是空的，先铺才有得挑。
        seed_default_pets(app);
    }
    let root = config::pets_dir()?;
    let id = pick_pack(&root, cfg.pet.as_deref())
        .with_context(|| format!("{} 下没有可用宠物包（需含 pet.json）", root.display()))?;
    let loaded = pack::load(&root, &id)?;
    log::info!(
        "已加载宠物包 {}（{}），{} 个动画，网格 {}x{}x{}x{}",
        loaded.info.id,
        loaded.info.display_name,
        loaded.info.animations.len(),
        loaded.info.frame.width,
        loaded.info.frame.height,
        loaded.info.frame.columns,
        loaded.info.frame.rows,
    );
    match loaded
        .behavior
        .as_ref()
        .and_then(|value| value.get("behavior"))
    {
        Some(_) => log::info!("已读取 litepet.behavior 扩展配置"),
        // 纯 Codex 包没有这个键，此时全走 §4.4 降级映射
        None => log::info!("无 litepet.behavior 扩展，使用 Codex 降级映射"),
    }
    Ok((loaded, cfg.port))
}

/// 加载宠物包；失败只告警，不阻塞窗口启动。
///
/// 返回的端口在加载失败时没有意义（此时也不会有 HTTP 服务），
/// 用默认值填上只是让调用方不必处理 `Option`。
fn init_pet(app: &tauri::AppHandle) -> (PetState, u16) {
    match try_init_pet(app) {
        Ok((loaded, port)) => (PetState(Arc::new(Mutex::new(Some(Arc::new(loaded))))), port),
        Err(err) => {
            log::error!("宠物包加载失败：{err:#}");
            (PetState(Arc::new(Mutex::new(None))), config::DEFAULT_PORT)
        }
    }
}

/// 启动 HTTP + JSON-RPC 服务，并把托盘挂上。
///
/// 行为配置非法、生成 token 失败、写对接信息失败都只告警：宠物仍应作为
/// 「没人说话的桌宠」正常显示。**但端口抢占失败是例外**——那是单例约束，直接退出。
fn start_http(app: tauri::AppHandle, state: &PetState, port: u16) {
    let loaded = state
        .0
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(Arc::clone));
    let Some(loaded) = loaded else {
        log::error!("宠物包未加载，HTTP 服务未启动");
        return;
    };
    let setup = session::Setup {
        pet_id: loaded.info.id.clone(),
        known: loaded.info.animations.keys().cloned().collect(),
        litepet: loaded.behavior.clone(),
    };
    let session = match session::Session::new(setup) {
        Ok(session) => session,
        Err(err) => {
            log::error!("行为配置非法，HTTP 服务未启动：{err:#}");
            return;
        }
    };
    if !session.has_rules() {
        log::info!("该包无规则表，动画由 Codex 降级映射决定");
    }

    // 先占端口再写对接信息：绑定失败就别留下一个指向死端口的文件。
    let server = match http::listen(port) {
        Ok(server) => server,
        Err(err) => {
            log::error!("{err:#}");
            // 单例是硬性约束（SPEC §0 约束 2）：没抢到端口就不能再开一只宠物。
            // 如果只是打条日志就继续跑，结果是一个永远收不到任何事件的僵尸窗口，
            // 而且它看起来和正常实例一模一样，最难排查。所以这里必须退出。
            if http::instance_running(port) {
                log::info!("已有实例在监听 127.0.0.1:{port}，本进程退出（单例）");
                std::process::exit(0);
            }
            log::error!("端口 {port} 上没有任何服务在监听，无法继续");
            std::process::exit(1);
        }
    };
    // 鉴权开关与 token 都取自配置：token 是用户自己定的，不再每次启动重新生成，
    // 于是宿主配好一次就不用再回去重读。状态已由 `main` 报过日志。
    let (cfg, _) = match config::load_or_init() {
        Ok(cfg) => cfg,
        Err(err) => {
            log::error!("{err:#}");
            return;
        }
    };
    // 端点文件里始终写配置里那个值（关掉鉴权时可能是空串），
    // 使宿主读到的内容在多次启动之间保持不变。
    let endpoint_token = cfg.auth.token.clone();
    let gate = cfg.auth_gate();
    match config::write_endpoint(protocol::PROTOCOL_VERSION, port, &endpoint_token) {
        Ok(path) => log::info!(
            "监听 http://127.0.0.1:{port}{}（对接信息 {}）",
            http::RPC_PATH,
            path.display()
        ),
        Err(err) => {
            log::error!("{err:#}");
            return;
        }
    }

    // 会话交给工作线程与空转线程共享，主线程只通过 daemon 出口碰它（换宠物时要改规则表）。
    let sessions = Arc::new(Mutex::new(session));
    let alerts: AlertSlot = Arc::new(Mutex::new(build_alerter(app.clone(), loaded.root.clone())));
    let daemon = Arc::new(DaemonState {
        app: app.clone(),
        pets_root: config::pets_dir().unwrap_or_else(|_| PathBuf::from("pets")),
        pet: Arc::clone(&state.0),
        sessions: Arc::clone(&sessions),
        alerts: Arc::clone(&alerts),
    });
    app.manage(DaemonHandle(Arc::clone(&daemon)));
    let presence = Arc::new(Presence::default());
    build_tray(&app, Arc::clone(&alerts), port, Arc::clone(&presence));

    // 两个钩子各要一个自己的 `AppHandle`（闭包只能各持一个）。
    let display_app = app.clone();
    let presence_app = app;
    let alert_slot = Arc::clone(&alerts);
    let hooks = Arc::new(http::Hooks {
        display: Box::new(move |directive| {
            if let Err(err) = display_app.emit("display", directive) {
                log::error!("推送渲染层失败：{err}");
            }
        }),
        // 没人连了就收进托盘，宿主连回来再露出来；判断都在 `apply_presence` 里。
        hosts: Box::new(move |count| apply_presence(&presence_app, &presence, count)),
        // 读槽而不是抱死一个 `Alerter`：设置页改完提醒配置会换一份进去，
        // 下一次提醒就该用新的——否则用户填完 token 还得重启才能试。
        alert: Box::new(move |request| match alert_slot.lock() {
            Ok(alerter) => alerter.fire(request),
            Err(_) => log::error!("提醒槽已损坏，本条提醒丢弃"),
        }),
    });

    let shared = Arc::new(http::Shared {
        sessions,
        gate,
        hooks,
        daemon: Arc::clone(&daemon) as Arc<dyn http::Daemon>,
    });
    http::serve(server, shared);
}

/// 提醒执行端；`config/set` 改完提醒配置会整体换一份放进这里。
///
/// 为什么是「整体换」而不是「就地改」：音量、各通道开关、推送凭据都是构造时定下的。
/// 用户刚填完推送密钥就会去按「测试提醒」，那时如果非要重启才生效，
/// 他只会以为是自己填错了 token。
type AlertSlot = Arc<Mutex<Arc<Alerter>>>;

/// daemon 级方法的实现（`pet/*`、`config/*`、`notify/test`）。
///
/// 它攢着切换宠物时要**同时改**的那两处（渲染层那份包、会话里的规则表），
/// 放在一起是为了让「换宠物」这件事只有一处实现。
struct DaemonState {
    app: tauri::AppHandle,
    /// 家目录下的 `pets/`。
    pets_root: PathBuf,
    /// 渲染层读的那份宠物包，与 [`PetState`] 是同一个锁。
    pet: Arc<Mutex<Option<Arc<pack::LoadedPet>>>>,
    /// 会话；换包只换它的规则表，不重建（见 `session::Session::rebind`）。
    sessions: Arc<Mutex<session::Session>>,
    /// 提醒执行端。
    alerts: AlertSlot,
}

impl DaemonState {
    /// 把选中的包记进配置，下次启动还是它。
    fn remember_pet(&self, id: &str) {
        let result = config::load_or_init().and_then(|(mut cfg, _)| {
            cfg.pet = Some(id.to_string());
            config::save(&cfg)
        });
        if let Err(err) = result {
            log::warn!("记住选中的宠物包失败，下次启动可能还是原来那只：{err:#}");
        }
    }

    /// 按当前配置重建提醒执行端。
    fn reload_alerts(&self) {
        let pack_root = self
            .pet
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|loaded| loaded.root.clone()))
            .unwrap_or_else(|| self.pets_root.clone());
        let fresh = build_alerter(self.app.clone(), pack_root);
        match self.alerts.lock() {
            Ok(mut slot) => *slot = fresh,
            Err(_) => log::error!("提醒槽已损坏，本次改动未生效"),
        }
    }

    /// 把与窗口有关的那几项配置立刻作用到窗口上。
    fn apply_window(&self, cfg: &config::Config) {
        // 不动位置：改尺寸时顺手把窗口挪走很难受。
        apply_window_config(&self.app, cfg, false);
    }
}

/// 把配置里的窗口项作用到窗口上。
///
/// `with_position` 决定要不要连位置一起摆过去：启动时要摆（重启后位置保留，
/// SPEC.md:368 是 M3 的验收项），`config/set` 时不摆——理由同上。
fn apply_window_config(app: &tauri::AppHandle, cfg: &config::Config, with_position: bool) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        log::warn!("找不到主窗口 {MAIN_WINDOW}，本次窗口相关改动未生效");
        return;
    };
    if let Err(err) = window.set_always_on_top(cfg.always_on_top) {
        log::warn!("设置置顶失败：{err}");
    }
    if let Err(err) = window.set_size(tauri::LogicalSize::new(cfg.size, cfg.size)) {
        log::warn!("设置窗口尺寸失败：{err}");
    }
    if !with_position {
        return;
    }
    let (Some(x), Some(y)) = (cfg.x, cfg.y) else {
        return;
    };
    // 位置：`Moved` 与 `outer_position()` 给的是同一套**物理**坐标，但这条路上有个坑——
    // `set_position(Physical(_))` 会先按 `window.scale_factor()` 换成逻辑坐标再交给系统，
    // 而这个读在窗口刚建好时是 1.0（真值 2.0），于是请求物理 300 得到物理 600。
    // 更糟的是 `Moved` 随后报 600并被写回配置，下次启动再翻一倍：窗口几轮后就飞出屏幕。
    // 所以改走 `LogicalPosition` 这条不做换算的路，缩放率从显示器上取——那是不依赖窗口
    // 是否已摆上屏幕的。（窗口若被拖到另一块不同缩放的屏上，用的仍是原来那块的值。）
    let scale = window
        .current_monitor()
        .or_else(|_| window.primary_monitor())
        .ok()
        .flatten()
        .map(|monitor| monitor.scale_factor())
        .unwrap_or(1.0);
    let logical = tauri::LogicalPosition::new(f64::from(x) / scale, f64::from(y) / scale);
    if let Err(err) = window.set_position(logical) {
        log::warn!("恢复窗口位置失败：{err}");
    }
}

/// 启动时按配置摆好窗口。
///
/// tauri.conf.json 里写死的 220 与 `alwaysOnTop: true` 只是兜底值：用户改过的尺寸和
/// 位置都在 config.json 里，不在启动时应用一次的话，改完设置一重启就全弹回去了。
fn apply_startup_window(app: &tauri::AppHandle) {
    match config::load_or_init() {
        Ok((cfg, _)) => apply_window_config(app, &cfg, true),
        Err(err) => log::warn!("读配置失败，窗口沿用 tauri.conf.json 的默认值：{err:#}"),
    }
}

/// 窗口被拖动后把位置写回配置（SPEC.md:368「重启后位置保留」）。
///
/// `Moved` 在拖动过程中每秒会来几十条，逐条写盘既浪费又没必要，所以交给一个后台线程
/// 合并：收到第一条后继续吃到「安静下来」为止，只落盘最后那个位置。
fn remember_window_position(app: tauri::AppHandle) {
    // 安静多久算落定。太短会写很多次，太长会让「快速拖完松手」的位置丢掉。
    const QUIET: Duration = Duration::from_millis(300);
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        log::warn!("找不到主窗口 {MAIN_WINDOW}，拖动后的位置不会被记住");
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<(i32, i32)>();
    window.on_window_event(move |event| {
        if let WindowEvent::Moved(position) = event {
            // 接收端已退出说明正在收尾，这里失败是正常的，不吵。
            let _ = tx.send((position.x, position.y));
        }
    });
    std::thread::spawn(move || {
        while let Ok(mut latest) = rx.recv() {
            while let Ok(next) = rx.recv_timeout(QUIET) {
                latest = next;
            }
            if let Err(err) = store_position(latest) {
                log::warn!("记住窗口位置失败：{err:#}");
            }
        }
    });
}

/// 写回窗口位置。读-改-写：只动 `x`/`y`，不碰同一时刻别人改过的字段。
fn store_position((x, y): (i32, i32)) -> Result<()> {
    let (mut cfg, _) = config::load_or_init()?;
    cfg.x = Some(x);
    cfg.y = Some(y);
    config::save(&cfg)
}

impl http::Daemon for DaemonState {
    fn pet_list(&self) -> Value {
        let pets = pack::list(&self.pets_root);
        let current = self
            .pet
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|loaded| loaded.info.id.clone()));
        match serde_json::to_value(&pets) {
            Ok(pets) => json!({ "pets": pets, "current": current }),
            Err(err) => {
                log::error!("宠物列表无法序列化：{err}");
                json!({ "pets": [], "current": current })
            }
        }
    }

    fn pet_select(&self, id: &str) -> std::result::Result<Value, ErrorObject> {
        let loaded = pack::load(&self.pets_root, id)
            .map_err(|err| ErrorObject::new(jsonrpc::INVALID_PARAMS, format!("{err:#}")))?;
        // 顺序是有意的：先换会话里的规则表，再换渲染层那份。
        // `rebind` 可能因为包的行为配置非法而失败，那时整件事都不该发生——
        // 否则会留下「画面换了、动作却对不上」的宠物。
        let setup = session::Setup {
            pet_id: loaded.info.id.clone(),
            known: loaded.info.animations.keys().cloned().collect(),
            litepet: loaded.behavior.clone(),
        };
        {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| ErrorObject::new(jsonrpc::INTERNAL_ERROR, "会话状态已损坏"))?;
            sessions.rebind(setup).map_err(|err| {
                ErrorObject::new(
                    jsonrpc::INVALID_PARAMS,
                    format!("{} 的行为配置非法：{err:#}", loaded.info.id),
                )
            })?;
        }
        let info = loaded.info.clone();
        if let Ok(mut guard) = self.pet.lock() {
            *guard = Some(Arc::new(loaded));
        }
        // 渲染层要重新加载图集：新包的动画名集合与旧包多半不同。
        if let Err(err) = self.app.emit("pet-changed", &info) {
            log::error!("通知渲染层换包失败，画面可能还是旧的：{err}");
        }
        // 音效是相对包目录解析的，换包后提醒也得跟着换。
        self.reload_alerts();
        log::info!("已切换到宠物包 {}", info.id);
        self.remember_pet(&info.id);
        serde_json::to_value(&info)
            .map_err(|err| ErrorObject::new(jsonrpc::INTERNAL_ERROR, format!("序列化失败：{err}")))
    }

    fn config_get(&self) -> std::result::Result<Value, ErrorObject> {
        let (cfg, created) = config::load_or_init().map_err(|err| {
            ErrorObject::new(jsonrpc::INTERNAL_ERROR, format!("读配置失败：{err:#}"))
        })?;
        // 路径一并给出去：设置页要显示「东西都在哪」并能一键打开。
        let home = config::home_dir()
            .map(|path| path.display().to_string())
            .ok();
        let log = logging::log_path()
            .map(|path| path.display().to_string())
            .ok();
        Ok(json!({
            "config": cfg,
            "home": home,
            "log": log,
            "petsRoot": self.pets_root.display().to_string(),
            "justCreated": created,
            "version": env!("CARGO_PKG_VERSION"),
        }))
    }

    fn config_set(&self, params: &Value) -> std::result::Result<Value, ErrorObject> {
        // 按补丁合并，而不是整份替换：设置页只发改动过的那几项，
        // 整份反序列化会把没提到的字段悄悄打回默认值。
        let (current, _) = config::load_or_init().map_err(|err| {
            ErrorObject::new(jsonrpc::INTERNAL_ERROR, format!("读配置失败：{err:#}"))
        })?;
        let mut merged = serde_json::to_value(&current).map_err(|err| {
            ErrorObject::new(jsonrpc::INTERNAL_ERROR, format!("序列化失败：{err}"))
        })?;
        if let Some(patch) = params.as_object() {
            for (key, value) in patch {
                merged[key] = value.clone();
            }
        }
        let mut cfg: config::Config = serde_json::from_value(merged).map_err(|err| {
            ErrorObject::new(jsonrpc::INVALID_PARAMS, format!("配置字段不合法：{err}"))
        })?;
        cfg.normalize();
        config::save(&cfg).map_err(|err| {
            ErrorObject::new(jsonrpc::INTERNAL_ERROR, format!("写配置失败：{err:#}"))
        })?;
        self.apply_window(&cfg);
        self.reload_alerts();
        // 端口改不了：监听早绑好了。如实告诉前端哪些要重启，不假装已生效。
        Ok(json!({ "config": cfg, "restartRequired": restart_required(&current, &cfg) }))
    }

    fn notify_test(&self) -> std::result::Result<Value, ErrorObject> {
        let alerter = self
            .alerts
            .lock()
            .map(|slot| Arc::clone(&slot))
            .map_err(|_| ErrorObject::new(jsonrpc::INTERNAL_ERROR, "提醒槽已损坏"))?;
        // 走的就是真实提醒那条路（同一个 `dispatch`）。另起一条测试专用路径
        // 只会验证出一条真实事件走不到的路。
        let actions = alerter.dispatch(&test_request());
        Ok(json!({
            "sound": actions.sound,
            "desktop": actions.desktop,
            "push": actions.push,
        }))
    }
}

/// 测试提醒的内容。
///
/// 音效用 `@attention` 而不是 `@done`：测试时人就在看，一个「办妥了」的提示
/// 反而会让人以为真的有任务结束了。
fn test_request() -> Request {
    Request::new(
        AlertSpec {
            sound: Some("@attention".to_string()),
            desktop: true,
            push: true,
        },
        "LitePet 测试提醒",
        "看到这条就说明这个通道是通的。",
    )
}

/// 列出哪些改动要重启才生效。
///
/// 只有端口：它决定了监听与 `daemon.json`，已经绑好了；
/// 宠物包、窗口尺寸、提醒开关都能当场生效。
fn restart_required(before: &config::Config, after: &config::Config) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if before.port != after.port {
        fields.push("port");
    }
    // 鉴权也是启动时定下的：`Shared.gate` 在 `serve` 时就交出去了，
    // 改完得重启才生效，不能假装已经生效——否则用户会以为立刻就不需要 token 了。
    if before.auth.token != after.auth.token {
        fields.push("auth");
    }
    fields
}

/// 挂托盘图标与菜单。
///
/// 无边框窗口没有标题栏与菜单，托盘的「显示宠物」是**唯一**能把藏起来的宠物
/// 找回来的入口。即便如此，图标没配好也不该让整个启动失败——只告警。
fn build_tray(app: &tauri::AppHandle, alerts: AlertSlot, port: u16, presence: Arc<Presence>) {
    let built = (|| -> Result<()> {
        let show = MenuItem::with_id(app, "show", "显示宠物", true, None::<&str>)?;
        let hide = MenuItem::with_id(app, "hide", "隐藏宠物", true, None::<&str>)?;
        let settings = MenuItem::with_id(app, "settings", "设置…", true, None::<&str>)?;
        let test = MenuItem::with_id(app, "test", "测试提醒", true, None::<&str>)?;
        let open_pets = MenuItem::with_id(app, "pets", "打开宠物目录", true, None::<&str>)?;
        let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
        let menu = Menu::with_items(
            app,
            &[
                &show,
                &hide,
                &PredefinedMenuItem::separator(app)?,
                &settings,
                &test,
                &open_pets,
                &PredefinedMenuItem::separator(app)?,
                &quit,
            ],
        )?;
        let icon = app.default_window_icon().cloned().context("没有默认图标")?;
        TrayIconBuilder::with_id(TRAY_ID)
            .icon(icon)
            .tooltip(format!("LitePet · 端口 {port}"))
            .menu(&menu)
            .on_menu_event(move |app, event| match event.id().as_ref() {
                "show" => show_pet_manually(app, &presence),
                "hide" => hide_pet_manually(app, &presence),
                "settings" => open_config_window(app),
                "test" => fire_test_alert(&alerts),
                "pets" => reveal_pets_dir(),
                "quit" => {
                    if let Err(err) = config::remove_endpoint() {
                        log::error!("{err:#}");
                    }
                    app.exit(0);
                }
                other => log::debug!("未处理的托盘菜单项 {other}"),
            })
            .build(app)?;
        Ok(())
    })();
    if let Err(err) = built {
        log::warn!("托盘不可用（宠物仍会显示）：{err:#}");
    }
}

/// 宿主数变化时的自动显隐（写在 `Hooks::hosts` 里，跑在 HTTP 工作线程上）。
///
/// 决策在 [`Presence::report`] 里，这里只管把结果落到窗口上。
fn apply_presence(app: &tauri::AppHandle, presence: &Presence, hosts: usize) {
    match presence.report(hosts) {
        Some(false) => {
            log::info!("宿主全部断开，宠物收进托盘");
            set_pet_visible(app, false);
        }
        Some(true) => {
            log::info!("有宿主接入，宠物显示出来");
            set_pet_visible(app, true);
        }
        None => {}
    }
}

/// 托盘「显示宠物」：清掉手动状态，控制权交回自动。
fn show_pet_manually(app: &tauri::AppHandle, presence: &Presence) {
    presence.show_manually();
    set_pet_visible(app, true);
}

/// 托盘「隐藏宠物」：这是「我说了算」，自动逻辑从此不再碰窗口。
fn hide_pet_manually(app: &tauri::AppHandle, presence: &Presence) {
    presence.hide_manually();
    set_pet_visible(app, false);
}

/// 显示/隐藏主窗口。
fn set_pet_visible(app: &tauri::AppHandle, visible: bool) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        log::warn!("找不到主窗口 {MAIN_WINDOW}");
        return;
    };
    let result = if visible {
        window.show()
    } else {
        window.hide()
    };
    if let Err(err) = result {
        log::warn!("切换宠物窗口可见性失败：{err}");
    }
}

/// 从托盘发一条测试提醒，结果写日志（托盘没有地方显示返回值）。
fn fire_test_alert(alerts: &AlertSlot) {
    let Ok(alerter) = alerts.lock().map(|slot| Arc::clone(&slot)) else {
        log::error!("提醒槽已损坏，无法测试");
        return;
    };
    let actions = alerter.dispatch(&test_request());
    log::info!(
        "测试提醒：声音={:?} 系统通知={} 推送={}",
        actions.sound,
        actions.desktop,
        actions.push
    );
}

/// 打开设置窗口；已经开着就拉到前台。
///
/// 单例是有意的：开两个设置窗口会让人怀疑「我改的到底是哪一个」。
fn open_config_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(CONFIG_WINDOW) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(
        app,
        CONFIG_WINDOW,
        tauri::WebviewUrl::App("config.html".into()),
    )
    .title("LitePet 设置")
    .inner_size(720.0, 560.0)
    .min_inner_size(560.0, 420.0)
    .resizable(true)
    .center()
    .build();
    if let Err(err) = built {
        log::error!("打开设置窗口失败：{err}");
    }
}

/// 在系统文件管理器里打开宠物目录。
///
/// 不引 opener 插件：三个平台各一条命令就够，而多一个插件就多一份权限面。
fn reveal_pets_dir() {
    let dir = match config::pets_dir() {
        Ok(dir) => dir,
        Err(err) => {
            log::error!("{err:#}");
            return;
        }
    };
    // 目录可能还不存在（刚装好、还没放包）：先建出来，否则文件管理器会报错。
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::warn!("创建宠物目录失败：{err:#}");
    }
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    match command.arg(&dir).spawn() {
        Ok(_) => log::info!("已打开宠物目录 {}", dir.display()),
        Err(err) => log::error!("打开宠物目录失败：{err}"),
    }
}

/// 按当前配置组装提醒执行端。
///
/// 提醒是**旁路**：任何一步配错（Bark 密钥写错、系统没装音效、用户关了通知）
/// 都只该让那一条提醒失效，绝不能拖累宠物本身。所以这里所有失败都降级为日志。
fn build_alerter(app: tauri::AppHandle, pack_root: PathBuf) -> Arc<Alerter> {
    let notify = config::load_or_init()
        .map(|(cfg, _)| cfg.notify)
        .unwrap_or_else(|err| {
            log::warn!("读取提醒配置失败，按默认值处理：{err:#}");
            NotifyConfig::default()
        });
    if !notify.enabled {
        log::info!("提醒总开关是关的（config.json 的 notify.enabled）");
    }
    let pusher: Arc<dyn Pusher> = match BarkPusher::from_config(&notify.push) {
        Ok(pusher) => Arc::new(pusher),
        Err(err) => {
            // 推送配错不该把声音和系统通知一起拖下水。
            log::warn!("手机推送不可用，只发本机提醒：{err:#}");
            Arc::new(SilentPusher)
        }
    };
    Arc::new(Alerter::new(
        notify,
        Some(pack_root),
        Channels {
            speaker: Arc::new(RodioSpeaker::spawn()),
            notifier: Arc::new(TauriNotifier::new(app)),
            pusher,
        },
    ))
}

/// 解析命令行：`--port`。
///
/// `--resident` 还收，但已经什么都不做——宠物本来就是常驻的（`docs/PROTOCOL.md` §8）。
/// 留着它是因为老的自启动项里还写着这个参数，报「未知参数」会打断启动。
fn parse_args() -> Option<u16> {
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // 过渡期兼容：不再有「非驻留」这种模式。
            "--resident" => {}
            "--port" => port = args.next().and_then(|raw| raw.parse().ok()),
            "--help" | "-h" => {
                println!("litepet [--port <端口>]");
                std::process::exit(0);
            }
            other => {
                eprintln!("litepet: 未知参数 {other}（--help 查看用法）");
                std::process::exit(2);
            }
        }
    }
    port
}

fn main() {
    let port = parse_args();
    // 日志尽早装上：后面的启动诊断都要落盘。双击启动的 GUI 没有终端，stderr 等于丢。
    if let Err(err) = logging::init() {
        eprintln!("litepet: 日志初始化失败，本次只输出到 stderr：{err:#}");
    }
    match logging::log_path() {
        Ok(path) => log::info!(
            "v{} 启动，日志写入 {}",
            env!("CARGO_PKG_VERSION"),
            path.display()
        ),
        Err(_) => log::info!("v{} 启动", env!("CARGO_PKG_VERSION")),
    }
    // 鉴权状态在启动时就报出来：它要么意味着接口对本机全开，要么意味着宿主一定连不上，
    // 两种都得让人知道。**不在这里拦启动**——鉴权只管 `POST /rpc` 的准入，
    // 跟窗口、托盘、宠物渲染无关（理由见 `config::AuthGate::Locked`）。
    match config::load_or_init().map(|(cfg, _)| cfg.auth_gate()) {
        Ok(config::AuthGate::Token(_)) => {
            log::info!("鉴权已启用，token 取自 config.json 的 auth.token");
        }
        Ok(config::AuthGate::Open) => log::warn!(
            "未设置 token（config.json 的 auth.token 为空）：接口不做鉴权，本机上任何程序都能连"
        ),
        Ok(config::AuthGate::Locked) => log::error!(
            "config.json 里的 auth.token 含空白或非 ASCII 字符，这个值永远配不上：所有 HTTP 请求都会被拒绝，宿主连不上。请到设置页或 config.json 改成可见 ASCII 口令，或清空它表示不鉴权"
        ),
        Err(err) => log::warn!("读配置失败，暂时无法确定鉴权状态：{err:#}"),
    }
    // 监听端口在 `setup` 里才定下来（要读配置），但退出清理在主线程的
    // `RunEvent::Exit` 上跑，两者之间只能靠一个共享值传递。
    let bound_port = Arc::new(AtomicU16::new(0));
    let port_shared = Arc::clone(&bound_port);
    let app = tauri::Builder::default()
        // 通知插件必须注册。`alert::desktop` 用的是 `app.notification()`，那个 API
        // 一开口就先取插件状态——没注册就 panic。此前它**从未被注册**，于是设置页一点
        // 「测试提醒」就 panic；而 panic 抛在 Tauri IPC 线程上会跨 FFI 边界，直接把
        // 整个进程带走，表现就是「程序直接退出」。
        .plugin(tauri_plugin_notification::init())
        .setup(move |app| {
            // 桌宠不需要 Dock 图标，也不需要在 Cmd+Tab 里露脸：托盘就是它的入口。
            // Accessory 正好去掉这两样。
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let (state, configured_port) = init_pet(app.handle());
            // 命令行优先于配置文件。
            let port = port.unwrap_or(configured_port);
            // 留给退出清理：它得知道对接文件里那个端口是不是自己写的。
            port_shared.store(port, Ordering::Relaxed);
            // `start_http` 内部会 `manage` 那个共享句柄，所以得先拿到 `&state`。
            start_http(app.handle().clone(), &state, port);
            app.manage(state);
            // 尺寸、置顶与位置都以 config.json 为准，tauri.conf.json 里那份只是兜底。
            apply_startup_window(app.handle());
            remember_window_position(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pack_info,
            renderer_ready,
            renderer_applied,
            local_call
        ])
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(err) => {
            // 走 `.expect` 的话这里会 panic，而 panic 不跑退出清理——端点文件会留在
            // 磁盘上指着一个根本不存在的端口，下次启动和宿主探测都会被它骗到。
            eprintln!("litepet: 界面初始化失败：{err}");
            log::error!("界面初始化失败：{err}");
            if let Err(err) = config::remove_endpoint() {
                log::warn!("清理对接信息失败：{err:#}");
            }
            std::process::exit(1);
        }
    };
    app.run(move |_handle, event| {
        if let tauri::RunEvent::Exit = event {
            let port = bound_port.load(Ordering::Relaxed);
            if port != 0 {
                match config::remove_endpoint_for(port) {
                    Ok(()) => log::info!("退出：已清理对接信息"),
                    Err(err) => log::warn!("退出时清理对接信息失败：{err:#}"),
                }
            }
        }
    });
}
