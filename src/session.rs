//! 会话层：把 socket 消息变成渲染指令，并负责 daemon 的生命周期（linger 退出）。
//!
//! 这一层是唯一知道「协议消息 × 宠物包规则 × 仲裁器」三者关系的地方，
//! 因此它是接进 Tauri 前最后一层可单测的逻辑（不碰 socket、不碰 UI）。

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use crate::arbiter::{Arbiter, FEEDBACK_MS};
use crate::behavior::{Behavior, Event};
use crate::daemon::DaemonMsg;
use crate::protocol::{AgentEnd, AgentStart, BubbleMsg, DisplayDirective, ToolEnd, ToolStart};

/// 全部宿主断开后，daemon 继续存活多久（`docs/PROTOCOL.md` §8）。
pub const LINGER: Duration = Duration::from_secs(30);

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
    /// 上一次推送的指令，用于去重。
    last: Option<DisplayDirective>,
}

impl Session {
    /// 构造会话。
    ///
    /// `petdaemon` 为 `pet.json` 里 `petdaemon` 扩展键的原值；`None` 表示纯 Codex 包。
    pub fn new(petdaemon: Option<&Value>, known: BTreeSet<String>, resident: bool) -> Result<Self> {
        Ok(Self {
            arbiter: Arbiter::new(),
            behavior: Behavior::new(petdaemon, &known)?,
            known,
            empty_since: None,
            resident,
            last: None,
        })
    }

    /// 该宠物包是否自带行为规则表。
    pub fn has_rules(&self) -> bool {
        self.behavior.has_rules()
    }

    /// 当前已连接宿主数。
    pub fn host_count(&self) -> usize {
        self.arbiter.host_count()
    }

    /// 处理一条 socket 消息。
    pub fn on_msg(&mut self, msg: DaemonMsg, now: Instant) -> Outcome {
        match msg {
            DaemonMsg::Connected {
                host,
                pid,
                agent_version,
                client_version,
                at,
            } => {
                println!(
                    "pet-daemon: 宿主 {host} 已接入（pid {pid}{}{}）",
                    agent_version
                        .as_deref()
                        .map(|version| format!("，agent {version}"))
                        .unwrap_or_default(),
                    client_version
                        .as_deref()
                        .map(|version| format!("，client {version}"))
                        .unwrap_or_default(),
                );
                self.arbiter.register(&host, at);
                println!("pet-daemon: 当前宿主 {} 个", self.host_count());
                self.empty_since = None;
                self.refresh(now)
            }
            DaemonMsg::Frame { host, envelope, at } => {
                self.handle_frame(&host, &envelope, at)
            }
            DaemonMsg::Closed { host } => {
                println!("pet-daemon: 宿主 {host} 已断开");
                self.arbiter.unregister(&host);
                println!("pet-daemon: 当前宿主 {} 个", self.host_count());
                if self.arbiter.is_empty() {
                    self.empty_since = Some(now);
                }
                self.refresh(now)
            }
            DaemonMsg::Failed { reason } => {
                eprintln!("pet-daemon: socket 服务失败，退出：{reason}");
                Outcome::Exit
            }
        }
    }

    /// 空闲时推进时间（气泡过期、反馈结束、进入 `resting`、linger 到期）。
    pub fn tick(&mut self, now: Instant) -> Outcome {
        if let Some(since) = self.empty_since {
            if !self.resident && now.saturating_duration_since(since) >= LINGER {
                println!(
                    "pet-daemon: 已无宿主连接满 {}s，退出",
                    LINGER.as_secs()
                );
                return Outcome::Exit;
            }
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
        // 兜底：即使这一层判不出任何变化，也别让 event loop 睡死。
        next.min(Duration::from_secs(1))
    }

    /// 把一帧交给规则表与仲裁器。
    fn handle_frame(&mut self, host: &str, envelope: &crate::protocol::Envelope, now: Instant) -> Outcome {
        if !self.arbiter.is_registered(host) {
            // 未握手宿主不该出现；忽略即可，不必报错。
            return Outcome::Unchanged;
        }
        let event = Event::new(&envelope.kind, envelope.payload.clone());
        let resolution = self.behavior.resolve(&event);

        match envelope.kind.as_str() {
            "agent.start" => {
                // `sessionId` / `summary` 只用于日志（`docs/PROTOCOL.md` §4）。
                if let Some(msg) = envelope.decode::<AgentStart>() {
                    println!(
                        "pet-daemon: 宿主 {host} 开始工作{}",
                        msg.session_id
                            .as_deref()
                            .map(|id| format!("（session {id}）"))
                            .unwrap_or_default()
                    );
                    if let Some(summary) = msg.summary.as_deref() {
                        println!("pet-daemon: 宿主 {host} 任务：{summary}");
                    }
                }
                self.arbiter.on_agent_start(host, now);
                // 状态迁移类规则：显式 play 持续到下次状态变迁。
                if let Some(play) = resolution.play {
                    self.arbiter.set_override(host, play, None);
                }
            }
            "agent.end" => {
                let parsed = envelope.decode::<AgentEnd>();
                let success = parsed.as_ref().map(|msg| msg.success).unwrap_or(false);
                if let Some(msg) = parsed.as_ref() {
                    println!(
                        "pet-daemon: 宿主 {host} 结束工作（{}）{}",
                        if success { "成功" } else { "失败" },
                        msg.session_id
                            .as_deref()
                            .map(|id| format!("（session {id}）"))
                            .unwrap_or_default()
                    );
                }
                self.arbiter.on_agent_end(host, success, now);
                if let Some(play) = resolution.play {
                    self.arbiter.set_override(host, play, None);
                }
            }
            "tool.start" => {
                let parsed = envelope.decode::<ToolStart>();
                let tool_name = parsed
                    .as_ref()
                    .map(|msg| msg.tool_name.as_str())
                    .unwrap_or_default();
                let hint = parsed.as_ref().and_then(|msg| msg.bubble.as_deref());
                self.apply_transient_play(host, resolution.play, now);
                self.arbiter.on_tool_start(
                    host,
                    tool_name,
                    resolution.bubble.as_ref(),
                    hint,
                    now,
                );
            }
            "tool.end" => {
                let parsed = envelope.decode::<ToolEnd>();
                let tool_name = parsed
                    .as_ref()
                    .map(|msg| msg.tool_name.as_str())
                    .unwrap_or_default();
                let is_error = parsed
                    .as_ref()
                    .and_then(|msg| msg.is_error)
                    .unwrap_or(false);
                self.apply_transient_play(host, resolution.play, now);
                self.arbiter
                    .on_tool_end(host, tool_name, is_error, resolution.bubble.as_ref(), now);
            }
            "bubble" => {
                let Some(msg) = envelope.decode::<BubbleMsg>() else {
                    // 字段不合法按协议静默忽略。
                    return Outcome::Unchanged;
                };
                self.apply_transient_play(host, resolution.play, now);
                self.arbiter
                    .on_bubble(host, msg.kind, &msg.text, msg.ttl_ms, now);
            }
            "ping" => {
                self.arbiter.on_ping(host, now);
                return Outcome::Unchanged;
            }
            _ => {
                // 未知 type 必须静默忽略（`docs/PROTOCOL.md` §3）。
                return Outcome::Unchanged;
            }
        }
        self.refresh(now)
    }

    /// 事件驱动的瞬时 `play`：与反馈窗口同长，到时自动回落舞台动画。
    fn apply_transient_play(&mut self, host: &str, play: Option<crate::behavior::Play>, now: Instant) {
        if let Some(play) = play {
            let expires_at = now + Duration::from_millis(FEEDBACK_MS);
            self.arbiter.set_override(host, play, Some(expires_at));
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Envelope, BubbleKind};
    use serde_json::json;

    fn known() -> BTreeSet<String> {
        ["idle", "working", "rest_tea", "celebrate", "sad"]
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    }

    /// 造一份带规则表的宠物包行为配置（含 `petdaemon` 外壳）。
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
        Session::new(Some(&rules()), known(), false).expect("应能构造会话")
    }

    /// 造一帧：`type` + 负载。
    fn frame(kind: &str, payload: Value) -> Envelope {
        let mut object = serde_json::Map::new();
        object.insert("v".to_string(), json!(1));
        object.insert("type".to_string(), json!(kind));
        if let Some(fields) = payload.as_object() {
            for (key, value) in fields {
                object.insert(key.clone(), value.clone());
            }
        } else {
            object.insert("payload".to_string(), payload);
        }
        serde_json::from_value(Value::Object(object)).expect("信封应能构造")
    }

    fn current(session: &mut Session, now: Instant) -> DisplayDirective {
        match session.refresh(now) {
            Outcome::Display(directive) => *directive,
            other => {
                // refresh 去重后可能返回 Unchanged，此时直接取内部状态。
                let _ = other;
                session.last.clone().expect("应有上一次指令")
            }
        }
    }

    fn connect(session: &mut Session, host: &str, now: Instant) -> Outcome {
        session.on_msg(
            DaemonMsg::Connected {
                host: host.to_string(),
                pid: 77,
                agent_version: None,
                client_version: None,
                at: now,
            },
            now,
        )
    }

    /// 断言结果不是退出。
    fn assert_alive(outcome: Outcome) {
        assert!(
            !matches!(outcome, Outcome::Exit),
            "不应退出，实际为 {outcome:?}"
        );
    }

    fn send(session: &mut Session, host: &str, envelope: Envelope, at: Instant) -> Outcome {
        session.on_msg(
            DaemonMsg::Frame {
                host: host.to_string(),
                envelope,
                at,
            },
            at,
        )
    }

    #[test]
    fn welcome_produces_first_directive() {
        let now = Instant::now();
        let mut session = session();
        let outcome = connect(&mut session, "pi", now);
        assert!(matches!(outcome, Outcome::Display(_)), "首帧应产生指令");
        assert_eq!(current(&mut session, now).animation, "idle");
        assert_eq!(session.host_count(), 1);
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
    fn agent_lifecycle_drives_animation() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);

        send(&mut session, "pi", frame("agent.start", json!({})), now);
        assert_eq!(current(&mut session, now).animation, "working");

        let end = now + Duration::from_millis(50);
        send(&mut session, "pi", frame("agent.end", json!({ "success": true })), end);
        assert_eq!(current(&mut session, end).animation, "celebrate");
    }

    #[test]
    fn rule_play_and_bubble_are_applied() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);

        send(
            &mut session,
            "pi",
            frame("tool.start", json!({ "toolName": "bash" })),
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
        connect(&mut session, "pi", now);
        send(
            &mut session,
            "pi",
            frame("tool.start", json!({ "toolName": "bash" })),
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
        connect(&mut session, "pi", now);
        send(
            &mut session,
            "pi",
            frame(
                "bubble",
                json!({ "kind": "error", "text": "炸了", "ttlMs": 1000 }),
            ),
            now,
        );
        let directive = current(&mut session, now);
        let bubble = directive.bubble.expect("应出气泡");
        assert_eq!(bubble.kind, "error");
        assert_eq!(bubble.text, "[pi] 炸了");
    }

    #[test]
    fn unknown_type_is_silently_ignored() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);
        let outcome = send(&mut session, "pi", frame("future.thing", json!({})), now);
        assert!(matches!(outcome, Outcome::Unchanged));
    }

    #[test]
    fn malformed_bubble_is_ignored() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);
        // kind 非法 → 解码失败 → 静默忽略。
        let outcome = send(
            &mut session,
            "pi",
            frame("bubble", json!({ "kind": "nope", "text": "x" })),
            now,
        );
        assert!(matches!(outcome, Outcome::Unchanged));
    }

    #[test]
    fn ping_does_not_change_display() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);
        let outcome = send(&mut session, "pi", frame("ping", json!({ "ts": 1 })), now);
        assert!(matches!(outcome, Outcome::Unchanged));
    }

    #[test]
    fn repeated_identical_state_is_not_repushed() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);
        assert!(matches!(session.tick(now), Outcome::Unchanged));
    }

    #[test]
    fn linger_exits_after_all_hosts_leave() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);
        let closed = session.on_msg(DaemonMsg::Closed { host: "pi".to_string() }, now);
        assert_alive(closed);
        assert_eq!(session.host_count(), 0);
        assert_alive(session.tick(now + LINGER - Duration::from_millis(1)));
        assert!(matches!(session.tick(now + LINGER), Outcome::Exit));
    }

    #[test]
    fn resident_mode_never_lingers_out() {
        let now = Instant::now();
        let mut session = Session::new(Some(&rules()), known(), true).expect("应能构造");
        connect(&mut session, "pi", now);
        assert_alive(session.on_msg(DaemonMsg::Closed { host: "pi".to_string() }, now));
        assert_alive(session.tick(now + LINGER * 10));
    }

    #[test]
    fn reconnect_during_linger_cancels_exit() {
        let now = Instant::now();
        let mut session = session();
        connect(&mut session, "pi", now);
        assert_alive(session.on_msg(DaemonMsg::Closed { host: "pi".to_string() }, now));
        // linger 途中重连 → 倒计时取消。
        assert_alive(connect(&mut session, "dsh", now + Duration::from_secs(20)));
        assert_alive(session.tick(now + Duration::from_secs(45)));
    }

    #[test]
    fn daemon_without_any_host_does_not_exit() {
        let now = Instant::now();
        let mut session = session();
        // 从未有宿主连过：不作为「全部断开」处理。
        assert_alive(session.tick(now + LINGER * 100));
    }

    #[test]
    fn socket_failure_exits() {
        let now = Instant::now();
        let mut session = session();
        let outcome = session.on_msg(
            DaemonMsg::Failed {
                reason: "boom".to_string(),
            },
            now,
        );
        assert!(matches!(outcome, Outcome::Exit));
    }

    #[test]
    fn frame_from_unregistered_host_is_ignored() {
        let now = Instant::now();
        let mut session = session();
        let outcome = send(&mut session, "ghost", frame("agent.start", json!({})), now);
        assert!(matches!(outcome, Outcome::Unchanged));
    }

    #[test]
    fn bubble_kind_from_protocol_round_trips() {
        // 守住 DisplayBubble.kind 与线格式一致（渲染层按它选样式）。
        assert_eq!(BubbleKind::Warning.as_str(), "warning");
    }
}
