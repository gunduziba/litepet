//! HTTP + JSON-RPC 2.0 服务层（`docs/PROTOCOL.md` §1–§2）。
//!
//! 传输层是 HTTP/1.1：单个端点 `POST /rpc`，请求体是一个 JSON-RPC 2.0 对象，
//! 认证用 `Authorization: Bearer <token>`。
//!
//! # 为什么不是 socket
//!
//! HTTP 让宿主用任何语言、任何 HTTP 客户端就能接入，不需要按行分帧，
//! 也把「断连检测」换成了显式心跳（见 [`crate::session::HOST_TIMEOUT`]）。
//!
//! # 本模块的职责边界
//!
//! 只做「收纸条 → 交给 [`Session`] → 回纸条」，不含任何业务判断；
//! 渲染层与退出是通过 [`Hooks`] 注入的闭包，因此本模块不依赖 Tauri，可直接单测。

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::Value;
use tiny_http::{Header, Method, Request, Response as HttpResponse, Server, StatusCode};

use crate::alert::Request as AlertRequest;
use crate::config::AuthGate;
use crate::jsonrpc::{self, ErrorObject, Response as JsonResponse};
use crate::protocol::DisplayDirective;
use crate::session::{Outcome, Session};

/// JSON-RPC 端点路径；其它路径一律 404。
pub const RPC_PATH: &str = "/rpc";
/// 请求体上限，防止本地进程用一个超大 body 把 daemon 撑爆。
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// 处理请求的工作线程数。
const WORKERS: usize = 4;
/// 工作线程每次等待请求的时长，决定退出时的响应延迟。
const POLL: Duration = Duration::from_millis(500);
/// 探测已有实例的超时；回环连接不需要久等。
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// 鉴权没过时的应答。
const UNAUTHORIZED_REASON: &str = "缺少或错误的 Authorization: Bearer <token>";
/// token 配得用不了时的应答：说清楚问题在 daemon 这边，不在宿主那边。
const LOCKED_REASON: &str = "daemon 的 config.json 里 auth.token 含空白或非 ASCII 字符，这个值永远配不上，所有请求都被拒绝；请改成可见 ASCII 口令，或清空它表示不鉴权";

/// 由调用方注入的副作用出口。
///
/// 用闭包而不是直接依赖 Tauri：本模块因此能在没有窗口的环境下单测。
pub struct Hooks {
    /// 会话产生了新指令。
    pub display: Box<dyn Fn(DisplayDirective) + Send + Sync>,
    /// 当前登记的宿主数；`0` 表示没人连了。
    ///
    /// 桌宠靠它决定要不要把自己收进托盘（没人连的时候藏起来）。
    /// 每次推进（一个请求或一拍空转）都会报一次，实现方得自己认「变化」，
    /// 否则用户刚从托盘点出来的宠物会被下一拍空转立刻藏回去。
    pub hosts: Box<dyn Fn(usize) + Send + Sync>,
    /// 会话排出了一条待发提醒。
    ///
    /// 回调必须**立刻返回**：它跑在 HTTP 工作线程上，而发提醒本身可能很慢
    /// （判定人在不在要起子进程、推手机要出网）。
    /// 真正的实现（`main`）只负责把活丢给后台线程。
    pub alert: Box<dyn Fn(AlertRequest) + Send + Sync>,
}

/// `pet/list`：列出可选宠物包。
pub const PET_LIST: &str = "pet/list";
/// `pet/select`：切换当前宠物包。
pub const PET_SELECT: &str = "pet/select";
/// `config/get`：读出当前配置。
pub const CONFIG_GET: &str = "config/get";
/// `config/set`：写入配置。
pub const CONFIG_SET: &str = "config/set";
/// `notify/test`：立刻发一条测试提醒，返回哪些通道真的发出去了。
///
/// 为什么是 RPC 而不仅是托盘菜单里那一下：推送凭据只存在 daemon 侧，
/// 而「token 到底对不对」只有真发一次才知道。给它一个可脚本调的入口，
/// 另一台机器（Windows）上就能直接验。
pub const NOTIFY_TEST: &str = "notify/test";

/// `notify/preview`：试听一条音效写法，返回实际命中的文件与它的来路。
///
/// 与 `notify/test` 的分工：后者回答「现在发得出什么」（过开关与规则表），
/// 这个回答「这个文件能不能响」（过开关，只出声）。设置页选音效时需要的是后者——
/// 「选择了文件却没声音」得当场能定位是文件的问题还是开关的问题。
///
/// `sound` 与规则表里的 `alert.sound` 是同一种写法（`@done`、`sounds/a.wav`、`Glass`），
/// 因此也能拿去另一台机器上验「这条规则配的音效在这台机器上能不能响」。
pub const NOTIFY_PREVIEW: &str = "notify/preview";

/// daemon 级方法（`pet/*`、`config/*`）的出口。
///
/// 为什么不塞进 [`Session`]：两者改的是**不同的状态**。`Session` 管「谁连上来了、
/// 现在该播什么」，它被宿主的每次事件并发写；这里管「有哪些宠物包、配置是什么」，
/// 它被配置页写。合并成一把锁的后果是**配置页扫一次盘，宿主的每个事件都得排队**，
/// 而扫盘要读每个包的图集文件头。
///
/// 出入参都用 [`Value`] 而不是各自的类型：本模块因此不必认识 `pack` 与 `config`，
/// 也就不会把「配置该怎么写」的判断漏到这里来。
pub trait Daemon: Send + Sync {
    /// 列出可选宠物包；坏包也要列出来（带原因），不能凭空消失。
    fn pet_list(&self) -> Value;
    /// 切换当前宠物包；返回新包的公开信息。
    fn pet_select(&self, id: &str) -> Result<Value, ErrorObject>;
    /// 读出当前配置。
    fn config_get(&self) -> Result<Value, ErrorObject>;
    /// 写入配置；返回**归一化之后**的实际值（前端要用它回填，不能拿自己发的值当准）。
    fn config_set(&self, params: &Value) -> Result<Value, ErrorObject>;
    /// 发一条测试提醒，返回实际发出去了哪些通道。
    fn notify_test(&self) -> Result<Value, ErrorObject>;
    /// 试听一条音效写法（`sound` 的写法同 `alert.sound`），只出声。
    ///
    /// 返回实际命中的文件与来路；`path` 为 `null` 时说没有可播的文件（`hint` 里带原因）。
    fn notify_preview(&self, sound: &str) -> Result<Value, ErrorObject>;
}

/// 处理链路上所有共享依赖。
///
/// 打包成一个结构体而不是逐个往下传：从 `serve` 到 `dispatch` 要过四样东西，
/// 位置参数一多，读的人就得回去数顺序。
pub struct Shared {
    /// 会话状态，宿主事件都写它。
    pub sessions: Arc<Mutex<Session>>,
    /// `POST /rpc` 的准入策略；由 `config.json` 的 `auth.token` 空不空决定。
    pub gate: AuthGate,
    /// 渲染与提醒的出口。
    pub hooks: Arc<Hooks>,
    /// daemon 级方法的出口。
    pub daemon: Arc<dyn Daemon>,
}

/// 监听回环地址上的 `port`。
///
/// **只绑回环**：桌宠是本机进程的附属物，不该出现在局域网上。
pub fn listen(port: u16) -> Result<Server> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    Server::http(addr).map_err(|err| anyhow!("监听 {addr} 失败（端口被占用？）：{err}"))
}

/// 回环上是否已经有东西在监听该端口。
///
/// 用来区分两种绑定失败：**已有一个 daemon 在跑**（按单例约定安静退出），
/// 与**端口被别的程序占了**（该报错退出）。两者都不该再开一只宠物。
pub fn instance_running(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// 启动工作线程与空转线程，随后立即返回；服务在后台运行。
pub fn serve(server: Server, shared: Arc<Shared>) {
    let server = Arc::new(server);
    for _ in 0..WORKERS {
        let server = Arc::clone(&server);
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || loop {
            match server.recv_timeout(POLL) {
                Ok(Some(request)) => handle(request, &shared),
                // 超时是正常的：空转交给 ticker 线程，这里只是回来看看有没有新请求。
                Ok(None) => {}
                Err(err) => {
                    log::error!("接收请求失败，该工作线程退出：{err}");
                    return;
                }
            }
        });
    }
    spawn_ticker(Arc::clone(&shared.sessions), Arc::clone(&shared.hooks));
}

/// 空转线程：按 [`Session::next_deadline`] 的节奏推进时间。
///
/// 气泡过期、进入 `resting`、回收死宿主都只依赖时间，
/// 没有任何请求会来触发它们，所以必须有一个自己的时钟。
fn spawn_ticker(sessions: Arc<Mutex<Session>>, hooks: Arc<Hooks>) {
    std::thread::spawn(move || loop {
        let deadline = match sessions.lock() {
            Ok(session) => session.next_deadline(Instant::now()),
            Err(_) => {
                log::error!("会话状态已损坏，空转线程退出");
                return;
            }
        };
        std::thread::sleep(deadline);
        let (outcome, hosts) = match sessions.lock() {
            Ok(mut session) => {
                let outcome = session.tick(Instant::now());
                (outcome, session.host_count())
            }
            Err(_) => return,
        };
        publish(outcome, hosts, &hooks);
    });
}

/// 处理一个 HTTP 请求。参数按值传入，因为 `respond` 会消费请求。
fn handle(mut request: Request, shared: &Shared) {
    if request.url() != RPC_PATH {
        respond_text(request, 404, &format!("仅支持 POST {RPC_PATH}"));
        return;
    }
    if request.method() != &Method::Post {
        respond_text(request, 405, &format!("仅支持 POST {RPC_PATH}"));
        return;
    }
    // 借用在这里算完，后面 `respond_text` 要拿走 `request`。
    let verdict = {
        let presented = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Authorization"))
            .map(|header| header.value.as_str());
        admit(&shared.gate, presented)
    };
    if let Err((status, reason)) = verdict {
        respond_text(request, status, reason);
        return;
    }
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(reason) => {
            respond_text(request, 413, &reason);
            return;
        }
    };
    match dispatch(&body, shared) {
        // 请求 → 回 JSON-RPC 响应；通知 → 204 无响应体（JSON-RPC 2.0 §4.1）。
        Some(reply) => respond_json(request, &reply),
        None => finish(request, HttpResponse::empty(StatusCode(204))),
    }
}

/// 解析并执行一次调用。
///
/// 返回 `None` 表示这是通知，不该有任何响应体。
/// 与 HTTP 无关，因此可以脱离网络单测。
fn dispatch(body: &str, shared: &Shared) -> Option<String> {
    let request = match jsonrpc::parse(body) {
        Ok(request) => request,
        // 连 id 都还没读到（JSON-RPC 2.0 §5：此时 id 必须是 null）。
        Err(error) => return Some(JsonResponse::failure(Value::Null, error).to_body()),
    };

    // daemon 级方法先走：它们改的是配置与宠物包，不碰会话状态，
    // 所以既不必也不该去抢那把被宿主高频写的锁。
    if let Some(result) = call_daemon(
        shared.daemon.as_ref(),
        &request.method,
        request.params.as_ref(),
    ) {
        return reply(&request, result);
    }

    let (answered, alerts, hosts) = match shared.sessions.lock() {
        Ok(mut session) => {
            let answered = session.call(&request.method, request.params.as_ref(), Instant::now());
            // 提醒必须在同一个锁里取走：放到锁外再取的话，
            // 并发的一次调用会把别人的提醒捎带出去，或者把自己的吃掉。
            let alerts = session.take_alerts();
            (answered, alerts, session.host_count())
        }
        Err(_) => (
            Err(ErrorObject::new(jsonrpc::INTERNAL_ERROR, "会话状态已损坏")),
            Vec::new(),
            0,
        ),
    };

    match answered {
        Ok(answered) => {
            publish(answered.outcome, hosts, &shared.hooks);
            // 提醒排在回复之后：通知渠道再慢也不该拖慢宿主拿到的响应。
            for alert in alerts {
                (shared.hooks.alert)(alert);
            }
            reply(&request, Ok(answered.result))
        }
        Err(error) => reply(&request, Err(error)),
    }
}

/// 执行 daemon 级方法；返回 `None` 表示这不是 daemon 级方法，该交给 [`Session`]。
/// 按 method 名执行一次 daemon 级调用；`None` 表示这个方法不归 daemon 层。
///
/// `pub` 是因为设置页（Tauri 命令）也走这里：于是「哪些方法属于哪一层」
/// 只有一处定义，不会出现「HTTP 认得、设置窗口不认得」。
///
/// 注意它**只放行 daemon 层**。`host/*` 与 `agent/*` 是外部进程驱动宠物的入口，
/// 从窗口里调它们等于把 token 鉴权作废。
pub fn call_daemon(
    daemon: &dyn Daemon,
    method: &str,
    params: Option<&Value>,
) -> Option<Result<Value, ErrorObject>> {
    match method {
        PET_LIST => Some(Ok(daemon.pet_list())),
        PET_SELECT => Some(pet_id(params).and_then(|id| daemon.pet_select(&id))),
        CONFIG_GET => Some(daemon.config_get()),
        CONFIG_SET => Some(object_params(params, CONFIG_SET).and_then(|p| daemon.config_set(p))),
        NOTIFY_TEST => Some(daemon.notify_test()),
        NOTIFY_PREVIEW => Some(sound_spec(params).and_then(|sound| daemon.notify_preview(&sound))),
        _ => None,
    }
}

/// 把一次调用的结果变成响应体（通知则一律静默）。
///
/// 两条调用路径共用它，所以「通知被拒只能记日志」这条规矩只有一处实现。
fn reply(request: &jsonrpc::Request, result: Result<Value, ErrorObject>) -> Option<String> {
    match (result, request.is_notification()) {
        (Ok(result), false) => Some(JsonResponse::success(request.response_id(), result).to_body()),
        // 通知没有回复通道，只能记日志（JSON-RPC 2.0 §4.1）。
        (Ok(_), true) => None,
        (Err(error), false) => Some(JsonResponse::failure(request.response_id(), error).to_body()),
        (Err(error), true) => {
            log::warn!("通知 {} 被拒：{}", request.method, error.message);
            None
        }
    }
}

/// 取 `pet/select` 的 `id`。空字符串不算合法 id。
fn pet_id(params: Option<&Value>) -> Result<String, ErrorObject> {
    params
        .and_then(|params| params.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ErrorObject::new(
                jsonrpc::INVALID_PARAMS,
                format!("{PET_SELECT} 需要一个非空字符串参数 id"),
            )
        })
}

/// 取 `notify/preview` 的 `sound`。空字符串不算合法写法。
///
/// 不在这里校验写法是否认得：认不得的写法会解析不到文件，那正是调用方想知道的。
fn sound_spec(params: Option<&Value>) -> Result<String, ErrorObject> {
    params
        .and_then(|params| params.get("sound"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|sound| !sound.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ErrorObject::new(
                jsonrpc::INVALID_PARAMS,
                format!("{NOTIFY_PREVIEW} 需要一个非空字符串参数 sound"),
            )
        })
}

/// 要求参数必须是个 JSON 对象。
fn object_params<'a>(params: Option<&'a Value>, method: &str) -> Result<&'a Value, ErrorObject> {
    params.filter(|params| params.is_object()).ok_or_else(|| {
        ErrorObject::new(
            jsonrpc::INVALID_PARAMS,
            format!("{method} 需要一个对象参数"),
        )
    })
}

/// 把会话结果推给渲染层，并报一次当前宿主数。
///
/// 返回值里没有「退出」这一档：宠物是常驻的，会话层永远不会让它走。
fn publish(outcome: Outcome, hosts: usize, hooks: &Arc<Hooks>) {
    // 先报宿主数（要露脸就先露），再推显式指令。
    (hooks.hosts)(hosts);
    match outcome {
        Outcome::Unchanged => {}
        Outcome::Display(directive) => (hooks.display)(*directive),
    }
}

/// 按准入策略判定这次请求；`Err` 是要回给客户端的 (状态码, 说明)。
///
/// 只吃一个头值、与 HTTP 无关，所以三种策略都能直接单测。
fn admit(gate: &AuthGate, header: Option<&str>) -> Result<(), (u16, &'static str)> {
    match gate {
        AuthGate::Open => Ok(()),
        // 回 503 而不是 401：401 的意思是「你的凭据不对」，
        // 会让人反复去检查宿主那边的 token；真正的问题在 daemon 这边没配（或配错了）。
        AuthGate::Locked => Err((503, LOCKED_REASON)),
        AuthGate::Token(expected) if authorized(header, expected) => Ok(()),
        AuthGate::Token(_) => Err((401, UNAUTHORIZED_REASON)),
    }
}

/// 校验 `Authorization: Bearer <token>`；头缺失、少了 `Bearer ` 前缀、值不符都算否。
fn authorized(header: Option<&str>, expected: &str) -> bool {
    header
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.trim(), expected))
}

/// 定长比较，避免用响应时间把 token 一位位试出来。
///
/// 长度不同直接判否：这会把长度泄漏出去，但 token 由用户自定、长度本身不足以缩小
/// 搜索空间，换来的是比较过程与内容无关。
fn constant_time_eq(presented: &str, expected: &str) -> bool {
    if presented.len() != expected.len() {
        return false;
    }
    presented
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// 读请求体；超过 [`MAX_BODY_BYTES`] 就拒绝，不把整个 body 读进内存。
fn read_body(request: &mut Request) -> Result<String, String> {
    let mut body = String::new();
    let limit = MAX_BODY_BYTES as u64 + 1;
    request
        .as_reader()
        .take(limit)
        .read_to_string(&mut body)
        .map_err(|err| format!("读取请求体失败：{err}"))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("请求体超过 {MAX_BODY_BYTES} 字节"));
    }
    Ok(body)
}

/// 回一个纯文本响应（传输层错误：路径、动词、认证、体积）。
fn respond_text(request: Request, status: u16, text: &str) {
    let response = HttpResponse::from_string(text.to_string())
        .with_status_code(StatusCode(status))
        .with_header(content_type("text/plain; charset=utf-8"));
    finish(request, response);
}

/// 回一个 JSON-RPC 响应。
fn respond_json(request: Request, body: &str) {
    let response = HttpResponse::from_string(body.to_string())
        .with_status_code(StatusCode(200))
        .with_header(content_type("application/json; charset=utf-8"));
    finish(request, response);
}

/// 统一收尾：回响应并记录失败。
fn finish<R: Read>(request: Request, response: HttpResponse<R>) {
    if let Err(err) = request.respond(response) {
        log::error!("回响应失败：{err}");
    }
}

/// 造一个 `Content-Type` 头。
///
/// 参数都是本模块里的字面量，必然合法；真非法也没有合理的降级路径。
fn content_type(value: &str) -> Header {
    Header::from_bytes("Content-Type", value).expect("Content-Type 字面量应当合法")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::session::Setup;
    use serde_json::{json, Value};
    use std::collections::BTreeSet;
    use std::net::TcpListener;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// daemon 级方法的替身：只记录被调了什么，并按参数决定成败。
    struct TestDaemon {
        /// 记录到的调用，形如 `pet/select:miao`。
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl TestDaemon {
        fn record(&self, what: &str) {
            self.calls.lock().expect("锁未中毒").push(what.to_string());
        }
    }

    impl Daemon for TestDaemon {
        fn pet_list(&self) -> Value {
            self.record(PET_LIST);
            json!({ "pets": [{ "dir": "miao", "id": "miao", "displayName": "小喵" }] })
        }

        fn pet_select(&self, id: &str) -> Result<Value, ErrorObject> {
            self.record(&format!("{PET_SELECT}:{id}"));
            if id == "missing" {
                return Err(ErrorObject::new(jsonrpc::INVALID_PARAMS, "没有这个宠物包"));
            }
            Ok(json!({ "id": id }))
        }

        fn config_get(&self) -> Result<Value, ErrorObject> {
            self.record(CONFIG_GET);
            Ok(json!({ "port": 47654, "size": 256 }))
        }

        fn notify_test(&self) -> Result<Value, ErrorObject> {
            self.record(NOTIFY_TEST);
            Ok(json!({ "sound": null, "desktop": false, "push": false }))
        }

        fn notify_preview(&self, sound: &str) -> Result<Value, ErrorObject> {
            self.record(&format!("{NOTIFY_PREVIEW}:{sound}"));
            Ok(json!({
                "sound": sound,
                "path": "/tmp/@done.wav",
                "layer": "bundled",
                "hint": null,
            }))
        }

        fn config_set(&self, params: &Value) -> Result<Value, ErrorObject> {
            self.record(CONFIG_SET);
            if params.get("size").and_then(Value::as_u64) == Some(0) {
                return Err(ErrorObject::new(jsonrpc::INVALID_PARAMS, "尺寸必须是正数"));
            }
            Ok(params.clone())
        }
    }

    /// 一套隔离的会话与出口，够写「发一张纸条、看收到什么」的用例。
    struct Harness {
        /// 处理链路上所有共享依赖。
        shared: Arc<Shared>,
        /// 收到的动画名。
        seen: Arc<Mutex<Vec<String>>>,
        /// 上报过的宿主数（按先后顺序）。
        hosts: Arc<Mutex<Vec<usize>>>,
        /// 排出的提醒。
        alerts: Arc<Mutex<Vec<AlertRequest>>>,
        /// daemon 级方法收到的调用。
        daemon_calls: Arc<Mutex<Vec<String>>>,
    }

    impl Harness {
        /// 发一个请求/通知，返回响应体（通知返回 `None`）。
        fn post(&self, body: Value) -> Option<Value> {
            dispatch(&body.to_string(), &self.shared)
                .map(|text| serde_json::from_str(&text).expect("响应应当是合法 JSON"))
        }

        /// daemon 级方法收到的调用。
        fn daemon_calls(&self) -> Vec<String> {
            self.daemon_calls.lock().expect("锁未中毒").clone()
        }

        /// 从响应里取出 `result`。
        fn result_of(&self, body: Value) -> Value {
            self.post(body)
                .and_then(|reply| reply.get("result").cloned())
                .expect("应有 result")
        }

        /// 从响应里取出错误码。
        fn error_code_of(&self, body: Value) -> i64 {
            let reply = self.post(body).expect("应当有响应");
            reply
                .get("error")
                .and_then(|err| err.get("code"))
                .and_then(Value::as_i64)
                .expect("应有 error.code")
        }

        /// 收到的指令条数。
        fn pushed(&self) -> usize {
            self.seen.lock().expect("锁未中毒").len()
        }

        /// 排出的提醒。
        fn alerts(&self) -> Vec<AlertRequest> {
            self.alerts.lock().expect("锁未中毒").clone()
        }

        /// 上报过的宿主数。
        fn hosts_reported(&self) -> Vec<usize> {
            self.hosts.lock().expect("锁未中毒").clone()
        }
    }

    fn harness() -> Harness {
        let known: BTreeSet<String> = ["idle", "working", "rest_tea"]
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let session = Session::new(Setup {
            pet_id: "xunjian-miao".to_string(),
            known,
            litepet: None,
        })
        .expect("应能构造会话");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let alerts = Arc::new(Mutex::new(Vec::new()));
        let alert_sink = Arc::clone(&alerts);
        let hosts = Arc::new(Mutex::new(Vec::new()));
        let hosts_sink = Arc::clone(&hosts);
        let hooks = Arc::new(Hooks {
            display: Box::new(move |directive| {
                sink.lock().expect("锁未中毒").push(directive.animation);
            }),
            hosts: Box::new(move |count| {
                hosts_sink.lock().expect("锁未中毒").push(count);
            }),
            alert: Box::new(move |request| {
                alert_sink.lock().expect("锁未中毒").push(request);
            }),
        });
        let daemon_calls = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Shared {
            sessions: Arc::new(Mutex::new(session)),
            gate: AuthGate::Token(TOKEN.to_string()),
            hooks,
            daemon: Arc::new(TestDaemon {
                calls: Arc::clone(&daemon_calls),
            }),
        });
        Harness {
            shared,
            seen,
            hosts,
            alerts,
            daemon_calls,
        }
    }

    /// 纯 Codex 包也要能提醒：`agent/settled` 是跳包通用的语义。
    ///
    /// 这条行钉住一个真实回归：会话层曾按 `has_rules()` 短路，
    /// 于是没有规则表的包永远拿不到默认提醒——而绝大多数现成包都没有规则表。
    #[test]
    fn codex_only_pack_still_gets_the_default_alert() {
        let harness = harness();
        harness.post(hello());
        harness.post(json!({
            "jsonrpc": "2.0",
            "method": "agent/settled",
            "params": { "host": "pi", "sessionId": "s1" }
        }));

        let alerts = harness.alerts();
        assert_eq!(alerts.len(), 1, "agent/settled 应产生一条提醒");
        // 语义名，不是 `Glass`：后者在 Windows 上解析不到，会静默失声。
        assert_eq!(alerts[0].spec.sound.as_deref(), Some("@done"));
        assert_eq!(alerts[0].title, "LitePet");
        assert_eq!(
            alerts[0].body, "这一轮干完了",
            "没有气泡可借就用事件自带的描述"
        );
    }

    /// 失败的 `agent/end` 该提醒；成功的不该——屏幕上本来就在动，再响就成噪声了。
    #[test]
    fn only_a_failed_agent_end_alerts() {
        let harness = harness();
        harness.post(hello());
        for success in [true, false] {
            harness.post(json!({
                "jsonrpc": "2.0",
                "method": "agent/end",
                "params": { "host": "pi", "success": success }
            }));
        }

        let alerts = harness.alerts();
        assert_eq!(alerts.len(), 1, "两次表态里只该有一次提醒");
        assert_eq!(alerts[0].spec.sound.as_deref(), Some("@failed"));
    }

    /// 调用被拒就不该提醒：参数写错的 `agent/end` 不该把主人从桌子那头叫过来。
    #[test]
    fn a_rejected_call_does_not_alert() {
        let harness = harness();
        harness.post(hello());
        // `success` 缺失是反序列化失败，整个调用会被拒。
        harness.post(json!({
            "jsonrpc": "2.0",
            "method": "agent/end",
            "params": { "host": "pi" }
        }));
        assert!(harness.alerts().is_empty());
    }

    /// 没打过招呼的宿主发来的事件不算数，也不该提醒。
    #[test]
    fn an_unregistered_host_cannot_alert() {
        let harness = harness();
        harness.post(json!({
            "jsonrpc": "2.0",
            "method": "agent/settled",
            "params": { "host": "stranger", "sessionId": "s1" }
        }));
        assert!(harness.alerts().is_empty());
    }

    fn hello() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "host/hello",
            "params": { "host": "pi", "protocolVersion": PROTOCOL_VERSION },
            "id": 1
        })
    }

    fn bye() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "host/bye",
            "params": { "host": "pi" },
            "id": 2
        })
    }

    #[test]
    fn hello_request_returns_a_jsonrpc_result() {
        let server = harness();
        let reply = server.post(hello()).expect("请求应有响应");
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(reply.get("error").is_none(), "成功响应不该有 error");
        assert_eq!(server.pushed(), 1, "首帧应推一次指令");
    }

    /// 宿主数要推出去：桌宠靠它决定什么时候把自己收进托盘、什么时候再露回来。
    #[test]
    fn host_count_is_reported_after_hello_and_bye() {
        let server = harness();
        server.post(hello());
        assert_eq!(server.hosts_reported().last().copied(), Some(1));
        server.post(bye());
        assert_eq!(server.hosts_reported().last().copied(), Some(0));
    }

    #[test]
    fn notification_gets_no_response_body() {
        let server = harness();
        assert!(server.post(hello()).is_some(), "先打招呼");
        // 遥测走通知：不回响应体。
        let telemetry = json!({
            "jsonrpc": "2.0",
            "method": "agent/start",
            "params": { "host": "pi" }
        });
        assert!(server.post(telemetry).is_none());
    }

    #[test]
    fn parse_error_replies_with_null_id() {
        let server = harness();
        let reply = dispatch("{ 这不是 JSON", &server.shared).expect("应当有响应");
        let reply: Value = serde_json::from_str(&reply).expect("响应应当是合法 JSON");
        assert_eq!(reply["error"]["code"], jsonrpc::PARSE_ERROR);
        assert_eq!(reply["id"], Value::Null, "解析失败时 id 必须是 null");
    }

    #[test]
    fn unknown_method_reports_method_not_found() {
        let server = harness();
        let call = json!({
            "jsonrpc": "2.0", "method": "pet/teleport", "params": {}, "id": 7
        });
        let reply = server.post(call).expect("应当有响应");
        assert_eq!(reply["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        assert_eq!(reply["id"], 7, "失败响应仍要带上原 id");
        assert!(reply.get("result").is_none(), "失败响应不该有 result");
    }

    #[test]
    fn notification_failure_is_not_replied() {
        let server = harness();
        // 未打招呼就发遥测：既不报错也不回复。
        let telemetry = json!({
            "jsonrpc": "2.0", "method": "tool/start",
            "params": { "host": "ghost", "toolName": "bash" }
        });
        assert!(server.post(telemetry).is_none());
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_secret() {
        assert!(constant_time_eq(TOKEN, TOKEN));
        assert!(!constant_time_eq("wrong", TOKEN));
        assert!(!constant_time_eq("", TOKEN));
        // 只差最后一位也必须判否。
        let almost = format!("{}0", &TOKEN[..TOKEN.len() - 1]);
        assert!(!constant_time_eq(&almost, TOKEN));
    }

    /// 关掉鉴权后，任何请求都不再需要 `Authorization` 头。
    #[test]
    fn open_gate_admits_without_any_header() {
        assert!(admit(&AuthGate::Open, None).is_ok(), "没填 token 时缺头也要放行");
        assert!(admit(&AuthGate::Open, Some("Bearer 随便什么")).is_ok());
    }

    /// 开着鉴权却没有可用 token 时，**谁都进不来**。
    ///
    /// 这条行钉住一个真实的坑：空 token 不能当口令用。
    /// `Authorization: Bearer `（后面什么都没有）会被 `strip_prefix` + `trim`
    /// 变成空串，若拿它去和空 token 比就通过了——那是「看起来设了防、
    /// 其实谁都能进」，比不设防更坏。所以这里连同状态码一起钉死。
    #[test]
    fn locked_gate_admits_nobody() {
        for header in [None, Some("Bearer "), Some("Bearer anything")] {
            let err = admit(&AuthGate::Locked, header).expect_err("不该放行任何请求");
            assert_eq!(err.0, 503, "该报「服务不可用」而不是「凭据不对」");
        }
    }

    /// 开着鉴权时，缺头、少前缀、值不对一律不放行。
    #[test]
    fn token_gate_needs_the_exact_bearer() {
        let gate = AuthGate::Token(TOKEN.to_string());
        assert!(admit(&gate, Some(&format!("Bearer {TOKEN}"))).is_ok());
        assert!(
            admit(&gate, Some(&format!("Bearer  {TOKEN} "))).is_ok(),
            "两头多余空白应当容忍"
        );
        assert_eq!(admit(&gate, None).expect_err("缺头").0, 401);
        assert_eq!(admit(&gate, Some("Bearer wrong")).expect_err("值不对").0, 401);
        assert_eq!(
            admit(&gate, Some(TOKEN)).expect_err("少 `Bearer ` 前缀").0,
            401
        );
    }

    #[test]
    fn content_type_header_is_well_formed() {
        let header = content_type("application/json; charset=utf-8");
        assert_eq!(header.field.as_str().as_str(), "Content-Type");
        assert_eq!(header.value.as_str(), "application/json; charset=utf-8");
    }

    #[test]
    fn rpc_path_is_the_documented_one() {
        assert_eq!(RPC_PATH, "/rpc");
    }

    /// 单例判定靠的就是这个探测：占着的端口要能认出来，空着的不能误报。
    #[test]
    fn instance_running_recognises_a_taken_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("应能占一个临时端口");
        let port = listener.local_addr().expect("应有本地地址").port();
        assert!(instance_running(port), "端口已在本进程手里，应当被认出来");

        drop(listener);
        assert!(
            !instance_running(port),
            "监听已释放，不该再报「有人在监听」"
        );
    }

    /// daemon 级方法必须在**不碰会话锁**的前提下被答上来。
    ///
    /// 这条行钉住一个容易复发的退化：把 `pet/*`、`config/*` 顺手写进 `Session::call`，
    /// 于是配置页扫一次盘（要读每个包的图集文件头）就把宿主的每个事件都堵住。
    #[test]
    fn daemon_level_calls_do_not_touch_the_session() {
        let harness = harness();
        let pets = harness.result_of(json!({
            "jsonrpc": "2.0", "id": 1, "method": PET_LIST
        }));
        assert_eq!(pets["pets"][0]["id"], json!("miao"));
        let cfg = harness.result_of(json!({
            "jsonrpc": "2.0", "id": 2, "method": CONFIG_GET
        }));
        assert_eq!(cfg["port"], json!(47654));

        assert_eq!(
            harness.daemon_calls(),
            vec![PET_LIST.to_string(), CONFIG_GET.to_string()]
        );
        assert_eq!(harness.pushed(), 0, "daemon 级调用不该产生渲染指令");
    }

    /// `notify/preview` 的 sound 是必填且不能是空串。
    ///
    /// 空串不能放过去：它会安静地解析不到文件，而调用方拿到的是一个
    /// 看起来合法、但什么也没验证的响应。
    #[test]
    fn notify_preview_requires_a_non_empty_sound() {
        let harness = harness();
        for params in [json!({}), json!({ "sound": "" }), json!({ "sound": "  " })] {
            assert_eq!(
                harness.error_code_of(json!({
                    "jsonrpc": "2.0", "id": 1, "method": NOTIFY_PREVIEW, "params": params
                })),
                jsonrpc::INVALID_PARAMS as i64,
                "{params} 应被拒"
            );
        }
        assert!(harness.daemon_calls().is_empty(), "参数不合法就不该往下传");
    }

    /// 试听走的是 daemon 层，参数原样带到出口（写法不做校验，认不得的写法也是合法输入）。
    #[test]
    fn notify_preview_passes_the_sound_through() {
        let harness = harness();
        let result = harness.result_of(json!({
            "jsonrpc": "2.0", "id": 1, "method": NOTIFY_PREVIEW,
            "params": { "sound": "sounds/怪名字.wav" }
        }));
        assert_eq!(result["layer"], json!("bundled"));
        assert_eq!(
            harness.daemon_calls(),
            vec![format!("{NOTIFY_PREVIEW}:sounds/怪名字.wav")],
            "写法不该在网关这层被改写"
        );
        assert_eq!(harness.pushed(), 0, "试听不是渲染指令");
    }

    /// `pet/select` 的 id 是必填且不能是空串。
    #[test]
    fn pet_select_requires_a_non_empty_id() {
        let harness = harness();
        assert_eq!(
            harness.error_code_of(json!({
                "jsonrpc": "2.0", "id": 1, "method": PET_SELECT
            })),
            jsonrpc::INVALID_PARAMS as i64,
            "缺参数要报 Invalid params"
        );
        assert_eq!(
            harness.error_code_of(json!({
                "jsonrpc": "2.0", "id": 2, "method": PET_SELECT, "params": { "id": "" }
            })),
            jsonrpc::INVALID_PARAMS as i64,
            "空串不是合法 id"
        );
        assert!(harness.daemon_calls().is_empty(), "参数不合法就不该往下传");
    }

    /// 出口报的错要原样变成响应里的错误，不能被吞成成功。
    #[test]
    fn daemon_error_reaches_the_caller() {
        let harness = harness();
        assert_eq!(
            harness.error_code_of(json!({
                "jsonrpc": "2.0", "id": 1, "method": PET_SELECT, "params": { "id": "missing" }
            })),
            jsonrpc::INVALID_PARAMS as i64
        );

        let echoed = harness.result_of(json!({
            "jsonrpc": "2.0", "id": 2, "method": CONFIG_SET, "params": { "size": 256 }
        }));
        assert_eq!(echoed["size"], json!(256));
        assert_eq!(
            harness.error_code_of(json!({
                "jsonrpc": "2.0", "id": 3, "method": CONFIG_SET, "params": { "size": 0 }
            })),
            jsonrpc::INVALID_PARAMS as i64
        );
    }

    /// `config/set` 要的是对象；给个数组、给个数字、什么都不给都得拒掉。
    #[test]
    fn config_set_requires_an_object() {
        let harness = harness();
        for params in [None, Some(json!(["size", 256])), Some(json!(5))] {
            let mut body = json!({ "jsonrpc": "2.0", "id": 1, "method": CONFIG_SET });
            if let Some(params) = params {
                body["params"] = params;
            }
            assert_eq!(harness.error_code_of(body), jsonrpc::INVALID_PARAMS as i64);
        }
        assert!(harness.daemon_calls().is_empty());
    }

    /// daemon 级方法当通知发时同样没有响应体（JSON-RPC 2.0 §4.1）。
    #[test]
    fn daemon_level_notification_gets_no_body() {
        let harness = harness();
        assert!(harness
            .post(json!({ "jsonrpc": "2.0", "method": PET_LIST }))
            .is_none());
        assert_eq!(harness.daemon_calls(), vec![PET_LIST.to_string()]);
    }
}
