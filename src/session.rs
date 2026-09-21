//! 会话层：把 JSON-RPC 方法调用变成渲染指令，并负责 daemon 的生命周期（linger 退出）。
//!
//! 这一层是唯一知道「协议方法 × 宠物包规则 × 仲裁器」三者关系的地方，
//! 因此它是接进 Tauri 前最后一层可单测的逻辑（不碰网络、不碰 UI）。
//!
//! 调用方（HTTP 服务层）只需三步：
//! 1. 加锁拿到 `Session`；
//! 2. [`Session::call`] 得到一个 [`Answered`]；
//! 3. 把 `outcome` 推给渲染层、把 `result` 写回 HTTP 响应。
//!
//! 与具体请求无关的空转（气泡过期、进入 `resting`、linger 到期、回收死宿主）
//! 走 [`Session::tick`]，由后台线程按 [`Session::next_deadline`] 的节奏驱动。

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::alert::Request;
use crate::arbiter::{Arbiter, FEEDBACK_MS};
use crate::behavior::{Behavior, Event, Resolution};
use crate::jsonrpc::{
    ErrorObject, HOST_UNKNOWN, INVALID_PARAMS, METHOD_NOT_FOUND, VERSION_UNSUPPORTED,
};
use crate::protocol::{
    self, AgentEnd, AgentSettled, AgentStart, DisplayDirective, HostBye, HostHello, PetBubble,
    PingParams, ToolEnd, ToolStart,
};

/// 全部宿主注销后，daemon 继续存活多久（`docs/PROTOCOL.md` §8）。
pub const LINGER: Duration = Duration::from_secs(30);

/// 建议宿主发送 `daemon/ping` 的间隔（`docs/PROTOCOL.md` §2）。
///
/// HTTP 无连接，心跳是唯一的存活信号；建议间隔必须明显小于 [`HOST_TIMEOUT`]。
pub const PING_INTERVAL: Duration = Duration::from_secs(20);

/// 超过该时长没收到该宿主任何消息（含心跳），即认定它已死。
pub const HOST_TIMEOUT: Duration = Duration::from_secs(60);

/// 构造会话所需的、由外部注入的静态信息。
#[derive(Debug, Clone)]
pub struct Setup {
    /// 当前宠物包 id。
    pub pet_id: String,
    /// 该包声明的动画名集合。
    pub known: BTreeSet<String>,
    /// `pet.json` 里 `litepet` 扩展键的原值；`None` 表示纯 Codex 包。
    pub litepet: Option<Value>,
    /// 常驻模式：永不由 linger 触发退出。
    pub resident: bool,
}

/// 处理一条消息或一次空转的结果。
#[derive(Debug)]
pub enum Outcome {
    /// 显示内容没有变化，无需推给渲染层。
    Unchanged,
    /// 需要推给渲染层。
    Display(Box<DisplayDirective>),
    /// 应该退出进程。
    Exit,
}

/// 一次方法调用的结果。
#[derive(Debug)]
pub struct Answered {
    /// 该写进 JSON-RPC 响应的 `result`；通知也会算出来，调用方忽略即可。
    pub result: Value,
    /// 状态变化后该推给渲染层的指令。
    pub outcome: Outcome,
}

/// daemon 的会话状态。
pub struct Session {
    /// 多宿主仲裁器。
    arbiter: Arbiter,
    /// 宠物包的行为配置。
    behavior: Behavior,
    /// 该包声明的动画名集合。
    known: BTreeSet<String>,
    /// 「全部宿主已断开」的起点；`None` 表示当前有宿主，或还没有宿主连过。
    empty_since: Option<Instant>,
    /// 常驻模式：永不由 linger 触发退出。
    resident: bool,
    /// 当前宠物包 id，用于回答 `daemon/info`。
    pet_id: String,
    /// 上一次推送的指令，用于去重。
    last: Option<DisplayDirective>,
    /// 已经上报过退出。
    ///
    /// 退出是终态：主循环收到 `Exit` 后还要跑几拍才能真正结束进程，
    /// 没这个标志就会把「该退出了」反复上报、反复打日志。
    retired: bool,
    /// 待发提醒。会话层只负责「攒」，发送是 `main` 的事。
    ///
    /// 提醒可能很慢（要起子进程判定人在不在、要出网推手机），
    /// 所以绝不能在会话线程里直接发；而一次调用又可能同时要改显示又要提醒，
    /// 用一条队列比往 [`Outcome`] 里塞一个变体清楚。
    alerts: Vec<Request>,
    /// 每个宿主最近一次 `agent/end` 的结局。
    ///
    /// `agent/settled` 按协议不带 `success`（`docs/PROTOCOL.md` §5）：
    /// 宿主能观察到的只是「不会再自动继续」，逼它在这里重报一次结果
    /// 等于逼它编个值。所以结局由 daemon 自己记，用来决定这一步
    /// 到底该庆祝还是该给个失败的脸色。
    last_end: HashMap<String, bool>,
}

impl Session {
    /// 构造会话。
    pub fn new(setup: Setup) -> Result<Self> {
        let behavior = Behavior::new(setup.litepet.as_ref(), &setup.known)?;
        Ok(Self {
            arbiter: Arbiter::new(),
            behavior,
            known: setup.known,
            empty_since: None,
            resident: setup.resident,
            pet_id: setup.pet_id,
            last: None,
            retired: false,
            alerts: Vec::new(),
            last_end: HashMap::new(),
        })
    }

    /// 换一只宠物：保留宿主注册与计时，只换掉包的规则与动画表。
    ///
    /// 为什么不直接重建一个 [`Session`]：重建会把宿主全忘掉，正在跑的 pi/dsh
    /// 必须重新 `host/hello` 才能再驱动宠物——而用户换宠物包时并没有断连，
    /// 凭什么要它们重连。
    ///
    /// 失败时保持原样：先把新规则构造好再落进字段，不存在「换了一半」的状态。
    pub fn rebind(&mut self, setup: Setup) -> Result<()> {
        let behavior = Behavior::new(setup.litepet.as_ref(), &setup.known)?;
        self.behavior = behavior;
        self.known = setup.known;
        self.pet_id = setup.pet_id;
        // 去重缓存要清掉：新包的第一帧必须推出去，否则换了宠物画面还是旧的。
        self.last = None;
        Ok(())
    }

    /// 取走待发提醒。
    ///
    /// 用「取走」而不是「读一下」的语义：提醒是一次性的，
    /// 重复发会比漏发更让人恼火。
    pub fn take_alerts(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.alerts)
    }

    /// 该宠物包是否自带行为规则表。
    pub fn has_rules(&self) -> bool {
        self.behavior.has_rules()
    }

    /// 当前已登记宿主数。
    pub fn host_count(&self) -> usize {
        self.arbiter.host_count()
    }

    /// 处理一次 JSON-RPC 方法调用。
    ///
    /// 返回的 [`Answered`] 里既有给宿主的 `result`，也有该推给渲染层的指令。
    /// 通知失败时调用方**不得**回复（JSON-RPC 2.0 §4.1），只能记日志。
    pub fn call(
        &mut self,
        method: &str,
        params: Option<&Value>,
        now: Instant,
    ) -> Result<Answered, ErrorObject> {
        let raw = params.cloned().unwrap_or_else(|| json!({}));
        let result = self.apply(method, &raw, now)?;
        Ok(Answered {
            result,
            outcome: self.refresh(now),
        })
    }

    /// 空闲时推进时间：回收死宿主、气泡过期、反馈结束、进入 `resting`、linger 到期。
    ///
    /// 一旦上报过 `Exit`，后续空转一律返回 `Unchanged`。
    pub fn tick(&mut self, now: Instant) -> Outcome {
        if self.retired {
            return Outcome::Unchanged;
        }
        let reaped = self.arbiter.reap_dead(now, HOST_TIMEOUT);
        for host in &reaped {
            log::info!(
                "宿主 {host} 已 {}s 无消息，视为断开",
                HOST_TIMEOUT.as_secs()
            );
        }
        if !reaped.is_empty() {
            log::info!("当前宿主 {} 个", self.host_count());
            self.begin_linger_if_empty(now);
        }

        if self.linger_expired(now) {
            log::info!("已无宿主连接满 {}s，退出", LINGER.as_secs());
            self.retired = true;
            return Outcome::Exit;
        }
        self.refresh(now)
    }

    /// 距离下一次必须空转还差多久。
    pub fn next_deadline(&self, now: Instant) -> Duration {
        let mut next = self.arbiter.next_deadline(now, &self.behavior);
        if let Some(since) = self.empty_since {
            if !self.resident {
                let remaining = LINGER.saturating_sub(now.saturating_duration_since(since));
                if remaining < next {
                    next = remaining;
                }
            }
        }
        // 兜底：即使这一层判不出任何变化，也别让 tick 线程睡死。
        // 死宿主的回收精度也因此不会差于 1s。
        next.min(Duration::from_secs(1))
    }

    /// 分发一次调用。
    fn apply(&mut self, method: &str, raw: &Value, now: Instant) -> Result<Value, ErrorObject> {
        // 任何消息都先刷新存活时间：宿主崩溃时没机会发 `host/bye`，
        // 只能靠「太久没消息」把它回收掉，否则宠物会永远赖在屏幕上。
        self.arbiter.touch(host_of(raw), now);

        if needs_session(method) && !self.arbiter.is_registered(host_of(raw)) {
            // 通知没有回复通道，只能静默丢弃；`daemon/ping` 是请求，
            // 正好用来告诉宿主「你该补一次 host/hello 了」。
            return match method {
                protocol::method::DAEMON_PING => Err(unknown_host(host_of(raw))),
                _ => Ok(Value::Null),
            };
        }

        // 提醒要覆盖所有包，所以这里**不能**按 `has_rules()` 短路：
        // 纯 Codex 包走 `resolve` 的降级分支，拿的是跨包通用的默认提醒。
        // 早先按 `has_rules()` 短路，那些默认提醒就永远到不了提醒层。
        let kind = protocol::rule_event(method);
        let rules = kind.map(|kind| self.behavior.resolve(&Event::new(kind, raw.clone())));

        let outcome = match method {
            protocol::method::HOST_HELLO => self.hello(parse_params(raw)?, now),
            protocol::method::HOST_BYE => self.bye(parse_params(raw)?, now),
            protocol::method::DAEMON_PING => self.ping(parse_params(raw)?),
            protocol::method::AGENT_START => {
                self.agent_start(parse_params(raw)?, rules.as_ref(), now)
            }
            protocol::method::AGENT_END => self.agent_end(parse_params(raw)?, rules.as_ref(), now),
            protocol::method::AGENT_SETTLED => {
                self.agent_settled(parse_params(raw)?, rules.as_ref(), now)
            }
            protocol::method::TOOL_START => {
                self.tool_start(parse_params(raw)?, rules.as_ref(), now)
            }
            protocol::method::TOOL_END => self.tool_end(parse_params(raw)?, rules.as_ref(), now),
            protocol::method::PET_BUBBLE => self.bubble(parse_params(raw)?, rules.as_ref(), now),
            protocol::method::DAEMON_INFO => self.info(),
            other => Err(ErrorObject::new(
                METHOD_NOT_FOUND,
                format!("未知方法：{other}"),
            )),
        };
        // 只有这次调用真的成功了才提醒：参数写错的 `agent/end`
        // 不该把主人从桌子那头叫过来。
        if outcome.is_ok() {
            if let Some(kind) = kind {
                self.queue_alert(kind, rules.as_ref());
            }
        }
        outcome
    }

    /// `host/hello`：登记宿主，并告知本机协议版本。
    ///
    /// 重复打招呼按「重连」处理：状态归零重来（`docs/PROTOCOL.md` §4）。
    fn hello(&mut self, msg: HostHello, now: Instant) -> Result<Value, ErrorObject> {
        if msg.protocol_version > protocol::PROTOCOL_VERSION {
            // 读不懂新语义就明确报错，绝不猜测——猜错等于播错动画。
            return Err(ErrorObject::new(
                VERSION_UNSUPPORTED,
                format!(
                    "宿主声明的协议版本 {} 高于本机支持的 {}",
                    msg.protocol_version,
                    protocol::PROTOCOL_VERSION
                ),
            )
            .with_data(json!({ "supported": protocol::PROTOCOL_VERSION })));
        }

        log::info!(
            "宿主 {} 已接入（pid {}{}{}）",
            msg.host,
            msg.pid
                .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
            suffix("agent", msg.agent_version.as_deref()),
            suffix("client", msg.client_version.as_deref()),
        );
        self.arbiter.register(&msg.host, now);
        // 重连也算新会话：上一轮遗留的「结局」不能拿来解释新一轮的 `agent/settled`。
        self.last_end.remove(&msg.host);
        log::info!("当前宿主 {} 个", self.host_count());
        self.empty_since = None;
        Ok(protocol::hello_result(
            env!("CARGO_PKG_VERSION"),
            &self.pet_id,
        ))
    }

    /// `host/bye`：宿主正常退出。
    fn bye(&mut self, msg: HostBye, now: Instant) -> Result<Value, ErrorObject> {
        log::info!(
            "宿主 {} 已断开{}",
            msg.host,
            suffix("原因", msg.reason.as_deref()),
        );
        self.arbiter.unregister(&msg.host);
        log::info!("当前宿主 {} 个", self.host_count());
        self.begin_linger_if_empty(now);
        Ok(Value::Null)
    }

    /// `daemon/ping`：心跳。存活时间已在 [`Session::apply`] 里刷新过。
    fn ping(&mut self, msg: PingParams) -> Result<Value, ErrorObject> {
        Ok(protocol::pong_result(&msg.host, msg.ts))
    }

    /// `agent/start`：进入 `working`。
    fn agent_start(
        &mut self,
        msg: AgentStart,
        rules: Option<&Resolution>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        log::info!(
            "宿主 {} 开始工作{}",
            msg.host,
            parens("session", msg.session_id.as_deref()),
        );
        if let Some(summary) = msg.summary.as_deref() {
            log::info!("宿主 {} 任务：{summary}", msg.host);
        }
        self.arbiter.on_agent_start(&msg.host, now);
        // 状态迁移类规则：显式 play 持续到下次状态变迁。
        if let Some(play) = rules.and_then(|rules| rules.play.clone()) {
            self.arbiter.set_override(&msg.host, play, None);
        }
        Ok(Value::Null)
    }

    /// `agent/end`：成功/失败进入对应反馈。
    fn agent_end(
        &mut self,
        msg: AgentEnd,
        rules: Option<&Resolution>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        log::info!(
            "宿主 {} 结束工作（{}）{}",
            msg.host,
            if msg.success { "成功" } else { "失败" },
            parens("session", msg.session_id.as_deref()),
        );
        self.arbiter.on_agent_end(&msg.host, msg.success, now);
        self.last_end.insert(msg.host.clone(), msg.success);
        if let Some(play) = rules.and_then(|rules| rules.play.clone()) {
            self.arbiter.set_override(&msg.host, play, None);
        }
        Ok(Value::Null)
    }

    /// `agent/settled`：整轮结束且不会自动继续，进庆祝或失败反馈。
    fn agent_settled(
        &mut self,
        msg: AgentSettled,
        rules: Option<&Resolution>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        let last = self.last_end.get(&msg.host).copied();
        log::info!(
            "宿主 {} 已停稳，不会自动继续（{}）",
            msg.host,
            parens("session", msg.session_id.as_deref()),
        );
        self.arbiter.on_agent_settled(&msg.host, last, now);
        if let Some(play) = rules.and_then(|rules| rules.play.clone()) {
            self.arbiter.set_override(&msg.host, play, None);
        }
        Ok(Value::Null)
    }

    /// `tool/start`：工具开始。
    fn tool_start(
        &mut self,
        msg: ToolStart,
        rules: Option<&Resolution>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        self.transient_play(&msg.host, rules, now);
        let bubble = rules.as_ref().and_then(|rules| rules.bubble.as_ref());
        self.arbiter.on_tool_start(
            &msg.host,
            &msg.tool_name,
            bubble,
            msg.bubble.as_deref(),
            now,
        );
        Ok(Value::Null)
    }

    /// `tool/end`：工具结束。
    fn tool_end(
        &mut self,
        msg: ToolEnd,
        rules: Option<&Resolution>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        self.transient_play(&msg.host, rules, now);
        let bubble = rules.as_ref().and_then(|rules| rules.bubble.as_ref());
        self.arbiter.on_tool_end(
            &msg.host,
            &msg.tool_name,
            msg.is_error.unwrap_or(false),
            bubble,
            now,
        );
        Ok(Value::Null)
    }

    /// `pet/bubble`：宿主直接指定一条气泡。
    fn bubble(
        &mut self,
        msg: PetBubble,
        rules: Option<&Resolution>,
        now: Instant,
    ) -> Result<Value, ErrorObject> {
        self.transient_play(&msg.host, rules, now);
        if msg.kind == protocol::BubbleKind::Unknown {
            // 降级显示而不是拒绝（协议前向兼容），但必须留痕：
            // 适配器作者在对面看不到任何报错，这条日志是唯一的排错线索。
            log::info!(
                "宿主 {} 发来未知气泡类别，按最低优先级显示：{}",
                msg.host,
                msg.text
            );
        }
        self.arbiter
            .on_bubble(&msg.host, msg.kind, &msg.text, msg.ttl_ms, now);
        Ok(Value::Null)
    }

    /// `daemon/info`：让宿主自检「我连的是哪个 daemon、它加载了哪个宠物」。
    fn info(&self) -> Result<Value, ErrorObject> {
        Ok(json!({
            "protocolVersion": protocol::PROTOCOL_VERSION,
            "daemonVersion": env!("CARGO_PKG_VERSION"),
            "petId": self.pet_id,
            "hostCount": self.arbiter.host_count(),
            "resident": self.resident,
            "pingIntervalMs": PING_INTERVAL.as_millis() as u64,
            "hostTimeoutMs": HOST_TIMEOUT.as_millis() as u64,
        }))
    }

    /// 事件驱动的瞬时 `play`：与反馈窗口同长，到时自动回落舞台动画。
    fn transient_play(&mut self, host: &str, rules: Option<&Resolution>, now: Instant) {
        if let Some(play) = rules.and_then(|rules| rules.play.clone()) {
            let expires_at = now + Duration::from_millis(FEEDBACK_MS);
            self.arbiter.set_override(host, play, Some(expires_at));
        }
    }

    /// 把规则求值出的提醒排入队列。
    ///
    /// 提醒在这里统一排队，而不是在五个 `agent_*`/`tool_*` 分支里各写一遍：
    /// 要不要提醒只取决于规则表怎么说，与事件本身怎么处理无关。
    fn queue_alert(&mut self, kind: &str, rules: Option<&Resolution>) {
        let Some(spec) = rules.and_then(|rules| rules.alert.clone()) else {
            return;
        };
        // 空壳规格（既不响也不弹不推）直接剔掉，不把噪声带给提醒层。
        if spec.is_inert() {
            return;
        }
        let body = alert_body(kind, rules);
        log::info!("规则命中提醒：{kind}（{body}）");
        self.alerts.push(Request::new(spec, ALERT_TITLE, body));
    }

    /// 重算指令，只在真正变化时推送。
    fn refresh(&mut self, now: Instant) -> Outcome {
        let directive = self.arbiter.directive(now, &self.behavior, &self.known);
        if self.last.as_ref() == Some(&directive) {
            return Outcome::Unchanged;
        }
        self.last = Some(directive.clone());
        Outcome::Display(Box::new(directive))
    }

    /// 没有宿主了就启动 linger 倒计时。
    fn begin_linger_if_empty(&mut self, now: Instant) {
        if self.arbiter.is_empty() {
            self.empty_since = Some(now);
        }
    }

    /// linger 是否已到期——即「该退出了」。
    fn linger_expired(&self, now: Instant) -> bool {
        if self.resident {
            return false;
        }
        self.empty_since
            .is_some_and(|since| now.saturating_duration_since(since) >= LINGER)
    }
}

/// 系统通知的标题。
///
/// 宠物包不提供标题，所以用一个固定名。它不需要区分是谁发的：
/// 通知中心里能认出来源就够了，而包里多一个标题字段只是多一个会写错的地方。
const ALERT_TITLE: &str = "LitePet";

/// 把规则求值出的提醒排入队列。
///
/// 正文优先用气泡文字：规则作者写的那句话本来就是给人看的，
/// 而通知里空着正文比给一句废话更糟。
fn alert_body(kind: &str, rules: Option<&Resolution>) -> String {
    rules
        .and_then(|rules| rules.bubble.as_ref())
        .map(|bubble| bubble.text.clone())
        .unwrap_or_else(|| describe_event(kind).to_string())
}

/// 事件名 → 一句人话，用于没有气泡可借的提醒。
///
/// 名字与 [`crate::protocol::rule_event`] 给出的规则事件名一一对应。
fn describe_event(kind: &str) -> &'static str {
    match kind {
        "agent.settled" => "这一轮干完了",
        "agent.start" => "开始干活了",
        "agent.end" => "一轮结束",
        "tool.start" => "开始执行工具",
        "tool.end" => "工具执行完毕",
        "bubble" => "有新消息",
        // 新增了规则事件却忘了在这里补一句时的兜底，
        // 宁可发一条没信息量的通知，也不要静默地什么都不发。
        _ => "有新的进展",
    }
}

/// 这些方法只对「已经打过招呼的宿主」有意义。
fn needs_session(method: &str) -> bool {
    matches!(
        method,
        protocol::method::AGENT_START
            | protocol::method::AGENT_END
            | protocol::method::AGENT_SETTLED
            | protocol::method::TOOL_START
            | protocol::method::TOOL_END
            | protocol::method::PET_BUBBLE
            | protocol::method::DAEMON_PING
    )
}

/// 从参数里取宿主名；取不到就返回空串（必然不等于任何已登记宿主）。
fn host_of(raw: &Value) -> &str {
    raw.get("host").and_then(Value::as_str).unwrap_or_default()
}

/// 把参数解析成具体类型；缺参数或字段不合法都报 `-32602`。
fn parse_params<T: DeserializeOwned>(raw: &Value) -> Result<T, ErrorObject> {
    serde_json::from_value(raw.clone())
        .map_err(|err| ErrorObject::new(INVALID_PARAMS, format!("参数非法：{err}")))
}

/// 构造「宿主未登记」错误，并顺带告诉它该调哪个方法。
fn unknown_host(host: &str) -> ErrorObject {
    let message = if host.is_empty() {
        "缺少 host 参数，或尚未发送 host/hello".to_string()
    } else {
        format!("宿主 {host} 尚未发送 host/hello")
    };
    ErrorObject::new(HOST_UNKNOWN, message)
        .with_data(json!({ "method": protocol::method::HOST_HELLO }))
}

/// 拼一个「，agent 1.2」式的可选后缀；没有值就返回空串。
fn suffix(label: &str, value: Option<&str>) -> String {
    value.map_or_else(String::new, |value| format!("，{label} {value}"))
}

/// 拼一个「（session abc）」式的可选后缀；没有值就返回空串。
fn parens(label: &str, value: Option<&str>) -> String {
    value.map_or_else(String::new, |value| format!("（{label} {value}）"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::PARSE_ERROR;

    fn known() -> BTreeSet<String> {
        ["idle", "working", "rest_tea", "celebrate", "sad"]
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    }

    /// 造一份带规则表的宠物包行为配置（含 `litepet` 外壳）。
    fn rules() -> Value {
        json!({
            "schemaVersion": 1,
            "behavior": {
                "idleTimeoutMs": 90_000,
                "groups": {
                    "idle": ["idle"],
                    "working": ["working"],
                    "resting": ["rest_tea"],
                    "feedback": ["celebrate", "sad"]
                },
                "rules": [
                    { "on": "tool.start", "play": "celebrate",
                      "bubble": { "kind": "tool", "text": "跑 {toolName}" } }
                ]
            }
        })
    }

    fn session() -> Session {
        Session::new(Setup {
            pet_id: "xunjian-miao".to_string(),
            known: known(),
            litepet: Some(rules()),
            resident: false,
        })
        .expect("应能构造会话")
    }

    fn call(
        session: &mut Session,
        method: &str,
        params: Value,
        at: Instant,
    ) -> Result<Answered, ErrorObject> {
        session.call(method, Some(&params), at)
    }

    /// 调用一个应当成功的方法，忽略结果。
    fn send(session: &mut Session, method: &str, params: Value, at: Instant) -> Outcome {
        call(session, method, params, at)
            .unwrap_or_else(|err| panic!("{method} 不应失败：{}", err.message))
            .outcome
    }

    fn hello(host: &str) -> Value {
        json!({ "host": host, "protocolVersion": protocol::PROTOCOL_VERSION })
    }

    /// 取当前该显示的指令（会顺带空转一次）。
    fn current(session: &mut Session, now: Instant) -> DisplayDirective {
        match session.tick(now) {
            Outcome::Display(directive) => *directive,
            _ => session.last.clone().expect("应有上一次指令"),
        }
    }

    fn assert_alive(outcome: &Outcome) {
        assert!(!matches!(outcome, Outcome::Exit), "不应退出");
    }

    /// 纯 Codex 包的会话（没有 `litepet` 键）。
    ///
    /// 它和带规则表的包有一个关键差异：反馈动画走 §4.4 的**逐状态**降级映射
    /// （`Success`→`bounce`、`Failure`→`sad`、`Celebrate`→`wave`），
    /// 而不是四个状态共用 `feedback` 组轮换。所以「庆祝还是失败」
    /// 在这类包上真的看得见，也才是验证 `agent/settled` 取哪个状态的唯一载体。
    fn codex_session() -> Session {
        let known = [
            "idle", "running", "waiting", "bounce", "jumping", "sad", "failed", "wave", "waving",
        ]
        .iter()
        .map(|name| (*name).to_string())
        .collect();
        Session::new(Setup {
            pet_id: "xunjian-miao".to_string(),
            known,
            litepet: None,
            resident: false,
        })
        .expect("应能构造会话")
    }

    /// 取一次调用产生的动画名。
    fn animation(outcome: Outcome) -> Option<String> {
        match outcome {
            Outcome::Display(directive) => Some(directive.animation),
            _ => None,
        }
    }

    #[test]
    fn settled_without_a_prior_end_celebrates() {
        let now = Instant::now();
        let mut session = codex_session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        let outcome = send(
            &mut session,
            protocol::method::AGENT_SETTLED,
            json!({ "host": "pi" }),
            now,
        );

        // 没记到结局就按「完成」处理：`agent/settled` 本身就说明不用再等了。
        assert_eq!(animation(outcome).as_deref(), Some("wave"));
    }

    #[test]
    fn settled_after_a_failed_end_keeps_the_failure_face() {
        let now = Instant::now();
        let mut session = codex_session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        send(
            &mut session,
            protocol::method::AGENT_END,
            json!({ "host": "pi", "success": false }),
            now,
        );
        let outcome = send(
            &mut session,
            protocol::method::AGENT_SETTLED,
            json!({ "host": "pi" }),
            now,
        );

        // 关键：这里**不能**变。若把 `agent/settled` 一律当庆祝，
        // 刚失败的宠物会突然眉开眼笑；指令相同所以会话层去重，
        // 于是「没变」本身就是正确的观察结果。
        assert!(
            matches!(outcome, Outcome::Unchanged),
            "失败之后停稳应继续给失败脸色，而不是跳去庆祝"
        );
        assert_eq!(current(&mut session, now).animation, "sad");
    }

    #[test]
    fn reconnect_forgets_the_previous_outcome() {
        let now = Instant::now();
        let mut session = codex_session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        send(
            &mut session,
            protocol::method::AGENT_END,
            json!({ "host": "pi", "success": false }),
            now,
        );
        // 重连算新会话：上一轮遗留的失败结局不能拿来解释新一轮的停稳。
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        let outcome = send(
            &mut session,
            protocol::method::AGENT_SETTLED,
            json!({ "host": "pi" }),
            now,
        );

        assert_eq!(animation(outcome).as_deref(), Some("wave"));
    }

    #[test]
    fn hello_produces_first_directive() {
        let now = Instant::now();
        let mut session = session();
        let answered = call(&mut session, protocol::method::HOST_HELLO, hello("pi"), now)
            .expect("host/hello 应成功");
        assert!(
            matches!(answered.outcome, Outcome::Display(_)),
            "首帧应产生指令"
        );
        assert_eq!(
            answered.result["protocolVersion"],
            protocol::PROTOCOL_VERSION
        );
        assert_eq!(answered.result["petId"], "xunjian-miao");
        assert_eq!(session.host_count(), 1);
        assert_eq!(current(&mut session, now).animation, "idle");
    }

    /// 一个宿主都没连时，也必须先推一次初始状态（宠物要立即上屏）。
    #[test]
    fn first_tick_without_hosts_still_pushes_initial_state() {
        let now = Instant::now();
        let mut session = session();
        assert!(matches!(session.tick(now), Outcome::Display(_)));
        assert_eq!(current(&mut session, now).animation, "idle");
    }

    #[test]
    fn hello_rejects_newer_protocol_version() {
        let now = Instant::now();
        let mut session = session();
        let params = json!({ "host": "pi", "protocolVersion": protocol::PROTOCOL_VERSION + 1 });
        let err = call(&mut session, protocol::method::HOST_HELLO, params, now)
            .expect_err("更高版本应被拒绝");
        assert_eq!(err.code, VERSION_UNSUPPORTED);
        // 拒绝后不能留下半个宿主。
        assert_eq!(session.host_count(), 0);
    }

    #[test]
    fn unknown_method_reports_method_not_found() {
        let now = Instant::now();
        let mut session = session();
        let err = call(&mut session, "pet/teleport", json!({}), now).expect_err("应报未知方法");
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    #[test]
    fn malformed_params_report_invalid_params() {
        let now = Instant::now();
        let mut session = session();
        // 缺 host：必需字段。
        let err = call(&mut session, protocol::method::HOST_HELLO, json!({}), now)
            .expect_err("缺 host 应被判为参数非法");
        assert_eq!(err.code, INVALID_PARAMS);
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        // 缺 text：必需字段。
        let err = call(
            &mut session,
            protocol::method::PET_BUBBLE,
            json!({ "host": "pi", "kind": "info" }),
            now,
        )
        .expect_err("缺 text 应被判为参数非法");
        assert_eq!(err.code, INVALID_PARAMS);
        // text 类型错：同样归为参数非法。
        let err = call(
            &mut session,
            protocol::method::PET_BUBBLE,
            json!({ "host": "pi", "kind": "info", "text": 42 }),
            now,
        )
        .expect_err("text 为数字应被判为参数非法");
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[test]
    fn unknown_bubble_kind_is_shown_rather_than_rejected() {
        // 适配器独立演进：新 kind 必须能被接住，不能因未知而整条丢掉。
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        let bad = json!({ "host": "pi", "kind": "celebrate", "text": "交卷" });
        let answered =
            call(&mut session, protocol::method::PET_BUBBLE, bad, now).expect("未知 kind 不应被拒");
        let Outcome::Display(directive) = answered.outcome else {
            panic!("应推一条显示指令");
        };
        let bubble = directive.bubble.expect("应带气泡");
        assert_eq!(bubble.kind, "unknown");
        assert!(bubble.text.contains("交卷"), "正文应保留：{}", bubble.text);
    }

    #[test]
    fn parsed_error_code_is_available_for_transport_layer() {
        // 传输层自己解析请求体，这里只确认错误码常量可被引用。
        assert_eq!(PARSE_ERROR, -32700);
    }

    #[test]
    fn agent_lifecycle_drives_animation() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);

        send(
            &mut session,
            protocol::method::AGENT_START,
            json!({ "host": "pi" }),
            now,
        );
        assert_eq!(current(&mut session, now).animation, "working");

        let end = now + Duration::from_millis(50);
        send(
            &mut session,
            protocol::method::AGENT_END,
            json!({ "host": "pi", "success": true }),
            end,
        );
        assert_eq!(current(&mut session, end).animation, "celebrate");
    }

    #[test]
    fn rule_play_and_bubble_are_applied() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);

        send(
            &mut session,
            protocol::method::TOOL_START,
            json!({ "host": "pi", "toolName": "bash" }),
            now,
        );
        let directive = current(&mut session, now);
        assert_eq!(directive.animation, "celebrate");
        let bubble = directive.bubble.expect("应出气泡");
        assert_eq!(bubble.text, "[pi] 跑 bash");
    }

    #[test]
    fn transient_play_expires_back_to_stage() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        send(
            &mut session,
            protocol::method::TOOL_START,
            json!({ "host": "pi", "toolName": "bash" }),
            now,
        );
        assert_eq!(current(&mut session, now).animation, "celebrate");

        let later = now + Duration::from_millis(FEEDBACK_MS + 1);
        assert_eq!(current(&mut session, later).animation, "idle");
    }

    #[test]
    fn host_bubble_message_is_honoured() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        send(
            &mut session,
            protocol::method::PET_BUBBLE,
            json!({ "host": "pi", "kind": "error", "text": "炸了", "ttlMs": 1000 }),
            now,
        );
        let bubble = current(&mut session, now).bubble.expect("应出气泡");
        assert_eq!(bubble.kind, "error");
        assert_eq!(bubble.text, "[pi] 炸了");
    }

    #[test]
    fn telemetry_from_unknown_host_is_silently_ignored() {
        let now = Instant::now();
        let mut session = session();
        // 先吃掉首帧：宠物上屏本身就会产生一次 Display，与宿主无关。
        current(&mut session, now);
        // 通知没有回复通道，只能静默丢弃，且不得凭空创建宿主。
        let outcome = send(
            &mut session,
            protocol::method::TOOL_START,
            json!({ "host": "ghost", "toolName": "bash" }),
            now,
        );
        assert!(matches!(outcome, Outcome::Unchanged));
        assert_eq!(session.host_count(), 0);
    }

    #[test]
    fn ping_from_unknown_host_reports_host_unknown() {
        let now = Instant::now();
        let mut session = session();
        // ping 是请求，正好用来告诉宿主「你该补 host/hello 了」。
        let err = call(
            &mut session,
            protocol::method::DAEMON_PING,
            json!({ "host": "ghost" }),
            now,
        )
        .expect_err("未登记宿主的 ping 应报错");
        assert_eq!(err.code, HOST_UNKNOWN);
        assert_eq!(err.data.expect("应带 data")["method"], "host/hello");
    }

    #[test]
    fn ping_returns_protocol_version_and_echoes_ts() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        let answered = call(
            &mut session,
            protocol::method::DAEMON_PING,
            json!({ "host": "pi", "ts": 42 }),
            now,
        )
        .expect("已登记宿主的 ping 应成功");
        assert_eq!(
            answered.result["protocolVersion"],
            protocol::PROTOCOL_VERSION
        );
        assert_eq!(answered.result["ts"], 42);
    }

    /// 回归测试：心跳只代表「宿主还活着」，不代表「宠物在干活」。
    ///
    /// 如果心跳也刷新 `last_event`，宿主一直开着 pi 就会让宠物永远不进入 `resting`。
    #[test]
    fn pings_do_not_keep_the_pet_awake() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);

        // 两分钟内只发心跳，没有任何真实活动。
        let mut at = now;
        while at < now + Duration::from_secs(120) {
            at += PING_INTERVAL;
            send(
                &mut session,
                protocol::method::DAEMON_PING,
                json!({ "host": "pi", "ts": 1 }),
                at,
            );
        }

        // idleTimeoutMs = 90s：心跳不该把宠物留在 idle。
        assert_eq!(current(&mut session, at).animation, "rest_tea");
        assert_eq!(session.host_count(), 1, "持续心跳的宿主不该被回收");
    }

    #[test]
    fn bye_starts_linger_and_exits() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        assert_alive(&send(
            &mut session,
            protocol::method::HOST_BYE,
            json!({ "host": "pi" }),
            now,
        ));
        assert_eq!(session.host_count(), 0);
        assert_alive(&session.tick(now + LINGER - Duration::from_millis(1)));
        assert!(matches!(session.tick(now + LINGER), Outcome::Exit));
    }

    /// 退出是终态：主循环还要跑几拍才能真结束进程，不能把「该退出了」反复上报。
    #[test]
    fn exit_is_reported_only_once() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        send(
            &mut session,
            protocol::method::HOST_BYE,
            json!({ "host": "pi" }),
            now,
        );

        assert!(matches!(session.tick(now + LINGER), Outcome::Exit));
        for round in 1..=3 {
            let later = now + LINGER + Duration::from_secs(60 * round);
            assert!(
                matches!(session.tick(later), Outcome::Unchanged),
                "第 {round} 次重复空转不该再上报退出"
            );
        }
    }

    #[test]
    fn resident_mode_never_lingers_out() {
        let now = Instant::now();
        let mut session = Session::new(Setup {
            pet_id: "xunjian-miao".to_string(),
            known: known(),
            litepet: Some(rules()),
            resident: true,
        })
        .expect("应能构造");
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        assert_alive(&send(
            &mut session,
            protocol::method::HOST_BYE,
            json!({ "host": "pi" }),
            now,
        ));
        assert_alive(&session.tick(now + LINGER * 10));
    }

    #[test]
    fn reconnect_during_linger_cancels_exit() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        assert_alive(&send(
            &mut session,
            protocol::method::HOST_BYE,
            json!({ "host": "pi" }),
            now,
        ));
        // linger 途中重连 → 倒计时取消。
        assert_alive(&send(
            &mut session,
            protocol::method::HOST_HELLO,
            hello("dsh"),
            now + Duration::from_secs(20),
        ));
        assert_alive(&session.tick(now + Duration::from_secs(45)));
    }

    /// 宿主崩溃（不发 `host/bye`）：靠心跳超时回收，再走 linger 退出。
    #[test]
    fn silent_host_is_reaped_then_daemon_lingers_out() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        assert_eq!(session.host_count(), 1);

        let silent = now + HOST_TIMEOUT + Duration::from_secs(1);
        assert_alive(&session.tick(silent));
        assert_eq!(session.host_count(), 0, "静默宿主应被回收");
        assert!(matches!(session.tick(silent + LINGER), Outcome::Exit));
    }

    #[test]
    fn reaped_host_must_hello_again() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        let silent = now + HOST_TIMEOUT + Duration::from_secs(1);
        session.tick(silent);

        let err = call(
            &mut session,
            protocol::method::DAEMON_PING,
            json!({ "host": "pi" }),
            silent,
        )
        .expect_err("被回收后必须重新打招呼");
        assert_eq!(err.code, HOST_UNKNOWN);
    }

    #[test]
    fn daemon_without_any_host_does_not_exit() {
        let now = Instant::now();
        let mut session = session();
        // 从未有宿主连过：不作为「全部断开」处理。
        assert_alive(&session.tick(now + LINGER * 100));
    }

    #[test]
    fn repeated_identical_state_is_not_repushed() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        assert!(matches!(session.tick(now), Outcome::Unchanged));
    }

    #[test]
    fn daemon_info_reports_pet_and_host_count() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        let answered = call(&mut session, protocol::method::DAEMON_INFO, json!({}), now)
            .expect("daemon/info 应成功");
        assert_eq!(answered.result["petId"], "xunjian-miao");
        assert_eq!(
            answered.result["protocolVersion"],
            protocol::PROTOCOL_VERSION
        );
        assert_eq!(answered.result["hostCount"], 1);
        assert_eq!(answered.result["resident"], false);
    }

    #[test]
    fn next_deadline_stays_bounded() {
        let now = Instant::now();
        let mut session = session();
        send(&mut session, protocol::method::HOST_HELLO, hello("pi"), now);
        // 不给 tick 线程睡死的可能。
        assert!(session.next_deadline(now) <= Duration::from_secs(1));
    }
}
