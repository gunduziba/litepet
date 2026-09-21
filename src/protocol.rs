//! 桌宠协议 v1 的业务契约：方法名、参数类型、渲染指令。
//!
//! 权威定义见 `docs/PROTOCOL.md`。本模块**只管纸条上写什么**，不含任何状态，
//! 也不认识传输方式——「纸条怎么送」见 [`crate::jsonrpc`] 与 HTTP 服务层。
//!
//! 因此本模块可以完全脱离网络单测。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 协议版本，固定为 `1`（`docs/PROTOCOL.md` §3）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 气泡正文截断长度（`docs/PROTOCOL.md` §6）。
pub const BUBBLE_TEXT_LIMIT: usize = 48;

/// 气泡详情截断长度（`docs/PROTOCOL.md` §6）。
///
/// v1 的线格式里没有独立的 `detail` 字段（详情写在 `text` 里），
/// 该上限留给后续版本引入 `detail` 时使用，这里先作为协议契约常量保留。
#[allow(dead_code)]
pub const BUBBLE_DETAIL_LIMIT: usize = 120;

/// 全部方法名。
///
/// 命名遵循 JSON-RPC 惯例 `命名空间/动作`（与 LSP、MCP 一致），
/// 与旧的 `type: "tool.start"` 点分形式的对应关系见 `docs/PROTOCOL.md` §2。
pub mod method {
    /// `host/hello`：宿主登记。**请求**，返回 daemon 与宠物包信息。
    pub const HOST_HELLO: &str = "host/hello";
    /// `host/bye`：宿主正常退出。通知。
    pub const HOST_BYE: &str = "host/bye";
    /// `agent/start`：该宿主的 agent 开始干活。通知。
    pub const AGENT_START: &str = "agent/start";
    /// `agent/end`：该宿主的 agent 结束。通知。
    pub const AGENT_END: &str = "agent/end";
    /// `tool/start`：工具开始。通知。
    pub const TOOL_START: &str = "tool/start";
    /// `tool/end`：工具结束。通知。
    pub const TOOL_END: &str = "tool/end";
    /// `pet/bubble`：宿主直接指定一条气泡。通知。
    pub const PET_BUBBLE: &str = "pet/bubble";
    /// `daemon/ping`：心跳与存活探测。**请求**，返回 `pong`。
    pub const DAEMON_PING: &str = "daemon/ping";
    /// `daemon/info`：daemon 自身信息。**请求**。
    pub const DAEMON_INFO: &str = "daemon/info";
}

/// 方法名 → 规则表事件名（`docs/PET-PACK.md` §4.3）。
///
/// 规则表里的 `on` 字段故意写成点分形式而不是 JSON-RPC 方法名：
/// 包作者写的是「事件」，与线格式的方法名解耦，以后改传输层不用动宠物包。
/// 非事件类方法（`host/hello`、`daemon/info` 等）返回 `None`。
pub fn rule_event(method: &str) -> Option<&'static str> {
    match method {
        method::AGENT_START => Some("agent.start"),
        method::AGENT_END => Some("agent.end"),
        method::TOOL_START => Some("tool.start"),
        method::TOOL_END => Some("tool.end"),
        method::PET_BUBBLE => Some("bubble"),
        _ => None,
    }
}

/// `host/hello` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostHello {
    /// 宿主标识，如 `pi` / `dsh`。
    pub host: String,
    /// 宿主进程号，仅用于日志。
    #[serde(default)]
    pub pid: Option<u32>,
    /// 宿主 agent 版本，仅用于日志。
    #[serde(default)]
    pub agent_version: Option<String>,
    /// 宿主客户端版本，仅用于日志。
    #[serde(default)]
    pub client_version: Option<String>,
    /// 宿主声明的协议版本；高于本机上限时 daemon 报 `-32002`。
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
}

/// 缺省协议版本：不写即视为当前版本。
pub fn default_protocol_version() -> u32 {
    PROTOCOL_VERSION
}

/// `host/bye` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostBye {
    /// 宿主标识。
    pub host: String,
    /// 退出原因，仅用于日志。
    #[serde(default)]
    pub reason: Option<String>,
}

/// `agent/start` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStart {
    /// 宿主标识。
    pub host: String,
    /// 会话 id，仅用于日志。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 会话摘要，仅用于日志。
    #[serde(default)]
    pub summary: Option<String>,
}

/// `agent/end` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnd {
    /// 宿主标识。
    pub host: String,
    /// 是否成功。
    pub success: bool,
    /// 会话 id，仅用于日志。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `tool/start` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStart {
    /// 宿主标识。
    pub host: String,
    /// 工具名，同时用作气泡去重键。
    pub tool_name: String,
    /// 可选的展示文本。
    #[serde(default)]
    pub bubble: Option<String>,
}

/// `tool/end` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEnd {
    /// 宿主标识。
    pub host: String,
    /// 工具名。**这是去重键，不是展示文本**（`docs/PROTOCOL.md` §9.3）。
    pub tool_name: String,
    /// 是否出错。
    #[serde(default)]
    pub is_error: Option<bool>,
}

/// `pet/bubble` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetBubble {
    /// 宿主标识。
    pub host: String,
    /// 气泡类别。
    pub kind: BubbleKind,
    /// 正文。
    pub text: String,
    /// 存活时长，缺省 4000ms。
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

/// `daemon/ping` 的参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingParams {
    /// 宿主标识。
    pub host: String,
    /// 宿主本地时间戳（epoch ms），原样回显。
    #[serde(default)]
    pub ts: Option<i64>,
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

/// `host/hello` 的成功结果。
pub fn hello_result(daemon_version: &str, pet_id: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "daemonVersion": daemon_version,
        "petId": pet_id,
    })
}

/// `daemon/ping` 的成功结果，原样回显宿主 `ts`。
pub fn pong_result(host: &str, ts: Option<i64>) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "host": host,
        "ts": ts,
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

    fn params(text: &str) -> Value {
        serde_json::from_str(text).expect("测试参数应当是合法 JSON")
    }

    #[test]
    fn hello_parses_with_optional_fields_absent() {
        let msg: HostHello = serde_json::from_value(params(r#"{"host":"pi"}"#))
            .expect("应能解析");
        assert_eq!(msg.host, "pi");
        assert_eq!(msg.protocol_version, PROTOCOL_VERSION, "缺省应为当前版本");
        assert!(msg.pid.is_none());
    }

    #[test]
    fn hello_rejects_missing_host() {
        // 宿主是必需参数：缺失必须报错，否则后续所有消息都无法归属。
        assert!(serde_json::from_value::<HostHello>(params(r#"{"pid":1}"#)).is_err());
    }

    #[test]
    fn tool_start_parses_camel_case() {
        let msg: ToolStart =
            serde_json::from_value(params(r#"{"host":"pi","toolName":"bash","bubble":"跑测试"}"#))
                .expect("应能解析");
        assert_eq!(msg.tool_name, "bash");
        assert_eq!(msg.bubble.as_deref(), Some("跑测试"));
    }

    #[test]
    fn tool_end_parses_is_error() {
        let msg: ToolEnd = serde_json::from_value(params(
            r#"{"host":"pi","toolName":"bash","isError":true}"#,
        ))
        .expect("应能解析");
        assert_eq!(msg.is_error, Some(true));
    }

    #[test]
    fn bubble_kind_parses_from_lowercase() {
        let msg: PetBubble = serde_json::from_value(params(
            r#"{"host":"pi","kind":"warning","text":"注意"}"#,
        ))
        .expect("应能解析");
        assert_eq!(msg.kind, BubbleKind::Warning);
        assert_eq!(msg.kind.as_str(), "warning");
    }

    #[test]
    fn bubble_kind_rejects_unknown_value() {
        assert!(
            serde_json::from_value::<PetBubble>(params(
                r#"{"host":"pi","kind":"panic","text":"x"}"#
            ))
            .is_err()
        );
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
    fn rule_event_maps_methods_to_pack_vocabulary() {
        assert_eq!(rule_event(method::TOOL_START), Some("tool.start"));
        assert_eq!(rule_event(method::AGENT_END), Some("agent.end"));
        assert_eq!(rule_event(method::PET_BUBBLE), Some("bubble"));
        // 非事件类方法不参与规则匹配。
        assert_eq!(rule_event(method::HOST_HELLO), None);
        assert_eq!(rule_event(method::DAEMON_PING), None);
    }

    #[test]
    fn hello_result_carries_protocol_version() {
        let result = hello_result("0.1.0", "xunjian-miao");
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["petId"], "xunjian-miao");
    }

    #[test]
    fn pong_result_echoes_ts() {
        assert_eq!(pong_result("pi", Some(7))["ts"], 7);
        assert!(pong_result("pi", None)["ts"].is_null());
    }
}
