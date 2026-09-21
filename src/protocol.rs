//! 桌宠协议 v1 的线格式：信封、宿主消息、daemon 回包、渲染指令。
//!
//! 权威定义见 `docs/PROTOCOL.md`。本模块只做序列化与解析，**不含任何状态**，
//! 因此可以脱离 socket 单测。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本，固定为 `1`（`docs/PROTOCOL.md` §3）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 气泡正文截断长度（`docs/PROTOCOL.md` §6）。
pub const BUBBLE_TEXT_LIMIT: usize = 48;

/// 气泡详情截断长度（`docs/PROTOCOL.md` §6）。
///
/// v1 的线格式里没有独立的 `detail` 字段（详情写在 `bubble.text` 里），
/// 该上限留给后续版本引入 `detail` 时使用，这里先作为协议契约常量保留。
#[allow(dead_code)]
pub const BUBBLE_DETAIL_LIMIT: usize = 120;

/// 一帧的通用信封：`{ "v": 1, "type": "...", "host": "..." }`。
///
/// 其余字段收进 `payload`，由各消息类型自行反序列化；未知字段不报错，
/// 保证前向兼容（`docs/PROTOCOL.md` §3）。
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    /// 协议版本。
    pub v: u32,
    /// 消息类型。
    #[serde(rename = "type")]
    pub kind: String,
    /// 宿主标识；连接建立后以 `host.hello` 里的值为准。
    pub host: Option<String>,
    /// 除 `v`/`type`/`host` 外的全部字段。
    #[serde(flatten)]
    pub payload: Value,
}

impl Envelope {
    /// 把负载解成具体消息；字段缺失或类型不符时返回 `None`（调用方按忽略处理）。
    pub fn decode<T: for<'de> Deserialize<'de>>(&self) -> Option<T> {
        serde_json::from_value(self.payload.clone()).ok()
    }
}

/// 解析一行 JSONL 帧。
///
/// 空行与非法 JSON 都返回 `None`：协议要求未知/坏帧静默忽略，不得断连。
pub fn parse_frame(line: &str) -> Option<Envelope> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// `host.hello`：连接后必须第一帧发送。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    /// 宿主进程号。
    pub pid: u32,
    /// 宿主 agent 版本，可选。
    #[serde(default)]
    pub agent_version: Option<String>,
    /// 宿主客户端版本，可选。
    #[serde(default)]
    pub client_version: Option<String>,
}

/// `agent.start`：该宿主的 agent 开始干活。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStart {
    /// 会话 id，仅用于日志与去重。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 会话摘要，可选。
    #[serde(default)]
    pub summary: Option<String>,
}

/// `agent.end`：该宿主的 agent 结束。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnd {
    /// 会话 id，仅用于日志与去重。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 是否成功。
    pub success: bool,
}

/// `tool.start`：工具开始。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStart {
    /// 工具名，同时用作气泡去重键。
    pub tool_name: String,
    /// 可选的展示文本。
    #[serde(default)]
    pub bubble: Option<String>,
}

/// `tool.end`：工具结束。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEnd {
    /// 工具名。
    pub tool_name: String,
    /// 是否出错。
    #[serde(default)]
    pub is_error: Option<bool>,
}

/// `ping`：心跳。
#[derive(Debug, Clone, Deserialize)]
pub struct Ping {
    /// 宿主本地时间戳（epoch ms），原样回显。
    pub ts: i64,
}

/// 气泡语义类别；`Ord` 即优先级，越大越优先（`docs/PROTOCOL.md` §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BubbleKind {
    /// 普通信息。
    Info,
    /// 状态变更。
    Status,
    /// 工具调用。
    Tool,
    /// 成功。
    Success,
    /// 警告。
    Warning,
    /// 错误。
    Error,
}

impl BubbleKind {
    /// 线格式里的小写名字，用于回传渲染层。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Status => "status",
            Self::Tool => "tool",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// `bubble`：宿主直接发一条气泡。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BubbleMsg {
    /// 气泡类别。
    pub kind: BubbleKind,
    /// 正文。
    pub text: String,
    /// 存活时长，缺省 4000ms。
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

/// daemon 推给渲染层的一条显示指令。
///
/// daemon 已经完成「组 → 具体动画名」的解析，渲染层只负责播放。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayDirective {
    /// 要播放的动画名（必须存在于宠物包的 `animations` 里）。
    pub animation: String,
    /// 当前要显示的气泡，无气泡时为 `null`。
    pub bubble: Option<DisplayBubble>,
}

/// 渲染层气泡内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayBubble {
    /// 气泡类别。
    pub kind: &'static str,
    /// 已带宿主徽章前缀、已截断的正文。
    pub text: String,
}

/// 组建 `host.welcome` 帧。
pub fn welcome(daemon_version: &str, assigned_host: &str, host: &str) -> Value {
    serde_json::json!({
        "v": PROTOCOL_VERSION,
        "type": "host.welcome",
        "host": host,
        "daemonVersion": daemon_version,
        "assignedHost": assigned_host,
    })
}

/// 组建 `pong` 帧，原样回显宿主 `ts`。
pub fn pong(ts: i64, host: &str) -> Value {
    serde_json::json!({
        "v": PROTOCOL_VERSION,
        "type": "pong",
        "host": host,
        "ts": ts,
    })
}

/// 组建 `host.evicted` 帧；发送后 daemon 会断开该连接。
pub fn evicted(reason: &str, host: &str) -> Value {
    serde_json::json!({
        "v": PROTOCOL_VERSION,
        "type": "host.evicted",
        "host": host,
        "reason": reason,
    })
}

/// 截断到 `limit` 字符（按 `char` 计，不切坏 UTF-8），超出以 `…` 结尾。
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    // 留一格给省略号，保证总长不超过 limit。
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_envelope_and_decodes_payload() {
        let line = r#"{"v":1,"type":"tool.start","host":"pi","toolName":"bash","bubble":"跑测试"}"#;
        let frame = parse_frame(line).expect("应能解析");
        assert_eq!(frame.v, PROTOCOL_VERSION);
        assert_eq!(frame.kind, "tool.start");
        assert_eq!(frame.host.as_deref(), Some("pi"));
        let msg: ToolStart = frame.decode().expect("应能解码");
        assert_eq!(msg.tool_name, "bash");
        assert_eq!(msg.bubble.as_deref(), Some("跑测试"));
    }

    #[test]
    fn unknown_type_still_parses() {
        // 协议要求未知类型静默忽略，因此解析必须成功。
        let frame = parse_frame(r#"{"v":1,"type":"future.thing","host":"dsh","extra":1}"#)
            .expect("未知类型仍应解析成功");
        assert_eq!(frame.kind, "future.thing");
        assert!(frame.decode::<ToolStart>().is_none(), "负载不符应返回 None");
    }

    #[test]
    fn malformed_and_blank_lines_are_ignored() {
        assert!(parse_frame("").is_none());
        assert!(parse_frame("   ").is_none());
        assert!(parse_frame("not json").is_none());
        assert!(parse_frame(r#"{"v":1}"#).is_none(), "缺 type 应失败");
    }

    #[test]
    fn hello_is_decoded_with_optional_fields() {
        let frame = parse_frame(r#"{"v":1,"type":"host.hello","host":"pi","pid":42}"#)
            .expect("应能解析");
        let hello: Hello = frame.decode().expect("应能解码");
        assert_eq!(hello.pid, 42);
        assert!(hello.agent_version.is_none());
    }

    #[test]
    fn bubble_kind_ordering_matches_priority_table() {
        assert!(BubbleKind::Error > BubbleKind::Warning);
        assert!(BubbleKind::Warning > BubbleKind::Success);
        assert!(BubbleKind::Success > BubbleKind::Tool);
        assert!(BubbleKind::Tool > BubbleKind::Status);
        assert!(BubbleKind::Status > BubbleKind::Info);
    }

    #[test]
    fn truncate_keeps_within_limit_and_marks_ellipsis() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 5), "abcd…");
        assert_eq!(truncate("abcdef", 5).chars().count(), 5);
        // 多字节字符不能被切坏。
        assert_eq!(truncate("中文测试文本", 4), "中文测…");
    }

    #[test]
    fn outgoing_frames_carry_protocol_version() {
        assert_eq!(welcome("0.1.0", "pi", "pi")["v"], PROTOCOL_VERSION);
        assert_eq!(pong(7, "pi")["ts"], 7);
        assert_eq!(evicted("bad version", "pi")["reason"], "bad version");
    }
}
