//! JSON-RPC 2.0 线格式（对应 `docs/PROTOCOL.md` §1–§3）。
//!
//! 本模块只负责「纸条怎么写」：请求、通知、响应、错误对象与标准错误码。
//! 「纸条怎么送」（HTTP）由 HTTP 服务层负责，两者互不依赖——因此本模块不认识
//! 任何业务概念（没有宿主、没有宠物），只认识 JSON-RPC 2.0。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本字面量：请求与响应的 `jsonrpc` 字段必须等于它。
pub const VERSION: &str = "2.0";

/// 解析错误：收到的不是合法 JSON（JSON-RPC 2.0 §5.1）。
pub const PARSE_ERROR: i32 = -32700;
/// 无效请求：是合法 JSON，但不是合法的请求对象。
pub const INVALID_REQUEST: i32 = -32600;
/// 方法不存在。
pub const METHOD_NOT_FOUND: i32 = -32601;
/// 参数非法。
pub const INVALID_PARAMS: i32 = -32602;
/// 服务端内部错误。
pub const INTERNAL_ERROR: i32 = -32603;
/// 应用自定义错误：宿主尚未发送 `host/hello` 就调用了需要登记的方法。
pub const HOST_UNKNOWN: i32 = -32001;
/// 应用自定义错误：宿主声明的协议版本高于本机支持的上限。
pub const VERSION_UNSUPPORTED: i32 = -32002;

/// 请求对象。
///
/// `id` 缺省即为**通知**（JSON-RPC 2.0 §4.1）：服务端**不得**回复。
/// 高频遥测（`tool/start` 等）都走通知，因此热路径没有响应开销。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// 恒为 [`VERSION`]，由 [`parse`] 校验。
    pub jsonrpc: String,
    /// 方法名，形如 `tool/start`。
    pub method: String,
    /// 具名参数；缺省表示无参数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// 请求标识；缺省表示这是通知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl Request {
    /// 是否为通知（无 `id`）。
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// 响应里应当回填的 `id`：通知取 `Null`（JSON-RPC 2.0 §5）。
    pub fn response_id(&self) -> Value {
        self.id.clone().unwrap_or(Value::Null)
    }
}

/// 错误对象（JSON-RPC 2.0 §5.1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// 错误码，见本模块顶部的常量。
    pub code: i32,
    /// 一句话说明。
    pub message: String,
    /// 附加数据；缺省表示无。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// 构造错误对象。
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// 附加结构化信息（链式）。
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// 响应对象。
///
/// `result` 与 `error` **互斥**（JSON-RPC 2.0 §5）：成功时只有前者，失败时只有后者。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// 恒为 [`VERSION`]。
    pub jsonrpc: String,
    /// 成功结果；失败时必须不存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 错误对象；成功时必须不存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
    /// 与请求相同的标识；无法确定时为 `Null`。
    pub id: Value,
}

impl Response {
    /// 成功响应。
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: VERSION.to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    /// 失败响应。
    pub fn failure(id: Value, error: ErrorObject) -> Self {
        Self {
            jsonrpc: VERSION.to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }

    /// 序列化为 HTTP 响应体。
    pub fn to_body(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            // `Response` 只含 JSON 可表达的类型，序列化不会失败；真发生了也只能回一个
            // 合法的错误对象，绝不能 panic 把 daemon 带走。
            format!(
                r#"{{"jsonrpc":"{VERSION}","error":{{"code":{INTERNAL_ERROR},"message":"响应序列化失败"}},"id":null}}"#
            )
        })
    }
}

/// 解析请求体。
///
/// 批量请求（JSON 数组）按 JSON-RPC 2.0 §6 语法合法，但本协议不需要，因此拒绝。
/// 返回 `Err` 时调用方应回填 `id: Null`（JSON-RPC 2.0 §5：无法确定 `id` 时用 Null）。
pub fn parse(body: &str) -> Result<Request, ErrorObject> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| ErrorObject::new(PARSE_ERROR, format!("JSON 解析失败：{err}")))?;

    if value.is_array() {
        return Err(ErrorObject::new(INVALID_REQUEST, "不支持批量请求"));
    }

    let request: Request = serde_json::from_value(value)
        .map_err(|err| ErrorObject::new(INVALID_REQUEST, format!("请求对象非法：{err}")))?;

    if request.jsonrpc != VERSION {
        return Err(ErrorObject::new(
            INVALID_REQUEST,
            format!("jsonrpc 必须为 \"{VERSION}\"，收到 \"{}\"", request.jsonrpc),
        ));
    }

    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_request_with_id() {
        let body = r#"{"jsonrpc":"2.0","method":"pet/list","params":{},"id":1}"#;
        let request = parse(body).expect("应当解析成功");
        assert_eq!(request.method, "pet/list");
        assert_eq!(request.response_id(), Value::from(1));
        assert!(!request.is_notification());
    }

    #[test]
    fn request_without_id_is_a_notification() {
        let body = r#"{"jsonrpc":"2.0","method":"tool/start","params":{"toolName":"bash"}}"#;
        let request = parse(body).expect("应当解析成功");
        assert!(request.is_notification());
        // 通知的响应 id 必须是 Null（JSON-RPC 2.0 §5）。
        assert_eq!(request.response_id(), Value::Null);
    }

    #[test]
    fn parses_a_request_without_params() {
        let body = r#"{"jsonrpc":"2.0","method":"daemon/ping","id":"abc"}"#;
        let request = parse(body).expect("应当解析成功");
        assert_eq!(request.response_id(), Value::from("abc"));
        assert!(request.params.is_none());
    }

    #[test]
    fn invalid_json_reports_parse_error() {
        let error = parse("{not json").expect_err("应当解析失败");
        assert_eq!(error.code, PARSE_ERROR);
    }

    #[test]
    fn wrong_version_reports_invalid_request() {
        let body = r#"{"jsonrpc":"1.0","method":"pet/list"}"#;
        let error = parse(body).expect_err("应当拒绝 1.0");
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[test]
    fn missing_version_reports_invalid_request() {
        let body = r#"{"method":"pet/list"}"#;
        let error = parse(body).expect_err("应当拒绝缺失 jsonrpc");
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[test]
    fn missing_method_reports_invalid_request() {
        let body = r#"{"jsonrpc":"2.0","id":1}"#;
        let error = parse(body).expect_err("应当拒绝缺失 method");
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[test]
    fn batch_request_is_rejected() {
        let body = r#"[{"jsonrpc":"2.0","method":"pet/list","id":1}]"#;
        let error = parse(body).expect_err("不支持批量请求");
        assert_eq!(error.code, INVALID_REQUEST);
    }

    #[test]
    fn success_response_omits_error_member() {
        let response = Response::success(Value::from(1), serde_json::json!({"ok": true}));
        let body = response.to_body();
        assert!(body.contains(r#""result""#));
        assert!(!body.contains(r#""error""#), "成功响应不得含 error：{body}");
    }

    #[test]
    fn failure_response_omits_result_member() {
        let response = Response::failure(
            Value::Null,
            ErrorObject::new(METHOD_NOT_FOUND, "方法不存在")
                .with_data(serde_json::json!({"method":"nope"})),
        );
        let body = response.to_body();
        assert!(body.contains(r#""error""#));
        assert!(
            !body.contains(r#""result""#),
            "失败响应不得含 result：{body}"
        );
        assert!(body.contains(r#""id":null"#));
    }

    #[test]
    fn responses_survive_a_round_trip() {
        let response = Response::success(Value::from(7), serde_json::json!(["a", "b"]));
        let body = response.to_body();
        let parsed: Response = serde_json::from_str(&body).expect("应当能反序列化");
        assert_eq!(parsed, response);
    }

    #[test]
    fn a_notification_parses_without_an_id() {
        // 宿主发来的通知就是没有 id 字段的同一个对象。
        let body = r#"{"jsonrpc":"2.0","method":"agent/end","params":{"success":true}}"#;
        let request = parse(body).expect("应能解析通知");
        assert!(request.is_notification());
        assert_eq!(request.method, "agent/end");
        assert_eq!(request.response_id(), Value::Null, "通知的响应 id 是 null");
    }
}
