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

use anyhow::{Result, anyhow};
use serde_json::Value;
use tiny_http::{Header, Method, Request, Response as HttpResponse, Server, StatusCode};

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

/// 由调用方注入的副作用出口。
///
/// 用闭包而不是直接依赖 Tauri：本模块因此能在没有窗口的环境下单测。
pub struct Hooks {
    /// 会话产生了新指令。
    pub display: Box<dyn Fn(DisplayDirective) + Send + Sync>,
    /// 会话要求退出进程。
    pub exit: Box<dyn Fn() + Send + Sync>,
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
pub fn serve(server: Server, sessions: Arc<Mutex<Session>>, token: String, hooks: Arc<Hooks>) {
    let server = Arc::new(server);
    for _ in 0..WORKERS {
        let server = Arc::clone(&server);
        let sessions = Arc::clone(&sessions);
        let token = token.clone();
        let hooks = Arc::clone(&hooks);
        std::thread::spawn(move || loop {
            match server.recv_timeout(POLL) {
                Ok(Some(request)) => handle(request, &sessions, &token, &hooks),
                // 超时是正常的：空转交给 ticker 线程，这里只是回来看看有没有新请求。
                Ok(None) => {}
                Err(err) => {
                    eprintln!("litepet: 接收请求失败，该工作线程退出：{err}");
                    return;
                }
            }
        });
    }
    spawn_ticker(sessions, hooks);
}

/// 空转线程：按 [`Session::next_deadline`] 的节奏推进时间。
///
/// 气泡过期、进入 `resting`、linger 退出、回收死宿主都只依赖时间，
/// 没有任何请求会来触发它们，所以必须有一个自己的时钟。
fn spawn_ticker(sessions: Arc<Mutex<Session>>, hooks: Arc<Hooks>) {
    std::thread::spawn(move || loop {
        let deadline = match sessions.lock() {
            Ok(session) => session.next_deadline(Instant::now()),
            Err(_) => {
                eprintln!("litepet: 会话状态已损坏，空转线程退出");
                return;
            }
        };
        std::thread::sleep(deadline);
        let outcome = match sessions.lock() {
            Ok(mut session) => session.tick(Instant::now()),
            Err(_) => return,
        };
        // 退出是终态：请完退出就停表，不再空转。
        let retiring = matches!(outcome, Outcome::Exit);
        publish(outcome, &hooks);
        if retiring {
            return;
        }
    });
}

/// 处理一个 HTTP 请求。参数按值传入，因为 `respond` 会消费请求。
fn handle(
    mut request: Request,
    sessions: &Arc<Mutex<Session>>,
    token: &str,
    hooks: &Arc<Hooks>,
) {
    if request.url() != RPC_PATH {
        respond_text(request, 404, &format!("仅支持 POST {RPC_PATH}"));
        return;
    }
    if request.method() != &Method::Post {
        respond_text(request, 405, &format!("仅支持 POST {RPC_PATH}"));
        return;
    }
    if !authorized(&request, token) {
        respond_text(request, 401, "缺少或错误的 Authorization: Bearer <token>");
        return;
    }
    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err(reason) => {
            respond_text(request, 413, &reason);
            return;
        }
    };
    match dispatch(&body, sessions, hooks) {
        // 请求 → 回 JSON-RPC 响应；通知 → 204 无响应体（JSON-RPC 2.0 §4.1）。
        Some(reply) => respond_json(request, &reply),
        None => finish(request, HttpResponse::empty(StatusCode(204))),
    }
}

/// 解析并执行一次调用。
///
/// 返回 `None` 表示这是通知，不该有任何响应体。
/// 与 HTTP 无关，因此可以脱离网络单测。
fn dispatch(
    body: &str,
    sessions: &Arc<Mutex<Session>>,
    hooks: &Arc<Hooks>,
) -> Option<String> {
    let request = match jsonrpc::parse(body) {
        Ok(request) => request,
        // 连 id 都还没读到（JSON-RPC 2.0 §5：此时 id 必须是 null）。
        Err(error) => return Some(JsonResponse::failure(Value::Null, error).to_body()),
    };

    let notification = request.is_notification();
    let id = request.response_id();
    let answered = match sessions.lock() {
        Ok(mut session) => session.call(&request.method, request.params.as_ref(), Instant::now()),
        Err(_) => Err(ErrorObject::new(jsonrpc::INTERNAL_ERROR, "会话状态已损坏")),
    };

    match answered {
        Ok(answered) => {
            publish(answered.outcome, hooks);
            if notification {
                None
            } else {
                Some(JsonResponse::success(id, answered.result).to_body())
            }
        }
        Err(error) => {
            if notification {
                // 通知没有回复通道，只能记日志（JSON-RPC 2.0 §4.1）。
                eprintln!("litepet: 通知 {} 被拒：{}", request.method, error.message);
                None
            } else {
                Some(JsonResponse::failure(id, error).to_body())
            }
        }
    }
}

/// 把会话结果推给渲染层，必要时请求退出。
fn publish(outcome: Outcome, hooks: &Arc<Hooks>) {
    match outcome {
        Outcome::Unchanged => {}
        Outcome::Display(directive) => (hooks.display)(*directive),
        Outcome::Exit => (hooks.exit)(),
    }
}

/// 校验 `Authorization: Bearer <token>`。
fn authorized(request: &Request, token: &str) -> bool {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .and_then(|header| header.value.as_str().strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.trim(), token))
}

/// 定长比较，避免用响应时间把 token 一位位试出来。
///
/// token 本身是随机十六进制，长度不敏感，所以长度不同直接判否。
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
        eprintln!("litepet: 回响应失败：{err}");
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
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use std::net::TcpListener;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// 一套隔离的会话与出口，够写「发一张纸条、看收到什么」的用例。
    struct Harness {
        /// 被测会话。
        sessions: Arc<Mutex<Session>>,
        /// 只记录指令、不真退出的出口。
        hooks: Arc<Hooks>,
        /// 收到的动画名。
        seen: Arc<Mutex<Vec<String>>>,
        /// 是否被要求退出。
        exited: Arc<Mutex<bool>>,
    }

    impl Harness {
        /// 发一个请求/通知，返回响应体（通知返回 `None`）。
        fn post(&self, body: Value) -> Option<Value> {
            dispatch(&body.to_string(), &self.sessions, &self.hooks)
                .map(|text| serde_json::from_str(&text).expect("响应应当是合法 JSON"))
        }

        /// 收到的指令条数。
        fn pushed(&self) -> usize {
            self.seen.lock().expect("锁未中毒").len()
        }

        /// 是否被要求退出。
        fn exited(&self) -> bool {
            *self.exited.lock().expect("锁未中毒")
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
            resident: true,
        })
        .expect("应能构造会话");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let exited = Arc::new(Mutex::new(false));
        let exit_flag = Arc::clone(&exited);
        let hooks = Arc::new(Hooks {
            display: Box::new(move |directive| {
                sink.lock().expect("锁未中毒").push(directive.animation);
            }),
            exit: Box::new(move || {
                *exit_flag.lock().expect("锁未中毒") = true;
            }),
        });
        Harness {
            sessions: Arc::new(Mutex::new(session)),
            hooks,
            seen,
            exited,
        }
    }

    fn hello() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "host/hello",
            "params": { "host": "pi", "protocolVersion": PROTOCOL_VERSION },
            "id": 1
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
        assert!(!server.exited(), "打招呼不该要求退出");
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
    fn parse_error_replies_with_null_id() {        let server = harness();
        let reply = dispatch("{ 这不是 JSON", &server.sessions, &server.hooks)
            .expect("应当有响应");
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
        assert!(
            instance_running(port),
            "端口已在本进程手里，应当被认出来"
        );

        drop(listener);
        assert!(
            !instance_running(port),
            "监听已释放，不该再报「有人在监听」"
        );
    }
}
