//! 手机推送通道：目前实现 Bark 协议。
//!
//! Bark 的接口是**路径式**的：`POST {endpoint}/{key}/{title}/{body}`。
//! 标题与正文必须自己做百分号编码——中文标题不编码会直接被服务端拒掉，
//! 而这里的失联是无声的，所以宁可自己编码也不要依赖客户端「应该会处理」。
//!
//! 推送是三个通道里唯一会出网的，所以超时开得比较紧，而且它永远跑在
//! 后台线程里，绝不能让一次网络抖动卡住宠物。

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::PushConfig;

/// 单次推送的超时。
const TIMEOUT: Duration = Duration::from_secs(10);

/// 默认端点：官方 Bark 服务器。自建服务器在配置里覆盖。
pub const DEFAULT_BARK_ENDPOINT: &str = "https://api.day.app";

/// 我们认识的推送服务。
const BARK: &str = "bark";

/// 手机推送通道。
pub trait Pusher: Send + Sync {
    /// 推一条通知。
    fn push(&self, title: &str, body: &str) -> Result<()>;
}

/// 什么都不做的推送器。
#[derive(Debug, Default)]
pub struct SilentPusher;

impl Pusher for SilentPusher {
    fn push(&self, _title: &str, _body: &str) -> Result<()> {
        Ok(())
    }
}

/// Bark 推送。
#[derive(Debug, Clone)]
pub struct BarkPusher {
    /// 端点，已去掉末尾斜杠。
    endpoint: String,
    /// 设备密钥。
    device_key: String,
    /// 复用的 HTTP 客户端（内部有连接池与 TLS 会话）。
    agent: ureq::Agent,
}

impl BarkPusher {
    /// 按配置构造。
    pub fn from_config(cfg: &PushConfig) -> Result<Self> {
        if !cfg.provider.eq_ignore_ascii_case(BARK) {
            // 不静默忽略：配了不认识的 provider 却在后台什么都不做，
            // 用户只会以为「推送坏了」。
            bail!(
                "暂不支持推送服务 {provider}（目前只有 {BARK}）",
                provider = cfg.provider
            );
        }
        if cfg.device_key.trim().is_empty() {
            bail!("push.deviceKey 为空，无法推送");
        }
        let endpoint = cfg
            .endpoint
            .as_deref()
            .unwrap_or(DEFAULT_BARK_ENDPOINT)
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            endpoint,
            device_key: cfg.device_key.trim().to_string(),
            agent: ureq::Agent::new_with_defaults(),
        })
    }

    /// 拼出这次请求的完整地址。
    fn url(&self, title: &str, body: &str) -> String {
        format!(
            "{endpoint}/{key}/{title}/{body}",
            endpoint = self.endpoint,
            key = encode_segment(&self.device_key),
            title = encode_segment(title),
            body = encode_segment(body)
        )
    }
}

impl Pusher for BarkPusher {
    fn push(&self, title: &str, body: &str) -> Result<()> {
        let response = self
            .agent
            .post(&self.url(title, body))
            .config()
            .timeout_global(Some(TIMEOUT))
            .build()
            .send_empty()
            .with_context(|| format!("推送请求失败：{}", self.endpoint))?;
        // Bark 成功时回 200；这里只在明确失败时报错。
        if response.status().is_success() {
            Ok(())
        } else {
            Err(anyhow!("推送返回状态码 {}", response.status()))
        }
    }
}

/// 百分号编码一个 URL 路径段。
///
/// 保留 RFC 3986 的 unreserved 集合（字母、数字、`-._~`），其余字节全部编码。
/// 含 `/` 的正文会被编码成 `%2F`，这正是我们要的：否则正文里的斜杠会把
/// 路径切成两段，Bark 收到的是被截断的消息。
fn encode_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PushConfig {
        PushConfig {
            enabled: true,
            provider: BARK.to_string(),
            device_key: "my-key".to_string(),
            endpoint: None,
        }
    }

    #[test]
    fn unreserved_characters_survive() {
        assert_eq!(encode_segment("abc-._~XYZ019"), "abc-._~XYZ019");
    }

    #[test]
    fn slashes_in_the_body_are_encoded() {
        // 这条是有实际后果的：不编码的话路径会被切成两段。
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("sounds/done.wav"), "sounds%2Fdone.wav");
    }

    #[test]
    fn chinese_is_encoded_byte_wise() {
        // 「完成」的 UTF-8 是 E5AE8C E68890。
        assert_eq!(encode_segment("完成"), "%E5%AE%8C%E6%88%90");
    }

    #[test]
    fn spaces_and_punctuation_are_encoded() {
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("1+1=2"), "1%2B1%3D2");
        assert_eq!(encode_segment("?q=1"), "%3Fq%3D1");
    }

    #[test]
    fn empty_segment_stays_empty() {
        assert_eq!(encode_segment(""), "");
    }

    #[test]
    fn default_endpoint_has_no_trailing_slash() {
        let pusher = BarkPusher::from_config(&cfg()).expect("应能构造");
        assert!(pusher.endpoint.ends_with("api.day.app"));
        assert_eq!(
            pusher.url("完成", "pi 干完了"),
            "https://api.day.app/my-key/%E5%AE%8C%E6%88%90/pi%20%E5%B9%B2%E5%AE%8C%E4%BA%86"
        );
    }

    #[test]
    fn custom_endpoint_trailing_slash_is_trimmed() {
        let mut cfg = cfg();
        cfg.endpoint = Some("https://bark.example.com/".to_string());
        let pusher = BarkPusher::from_config(&cfg).expect("应能构造");
        assert_eq!(
            pusher.url("t", "b"),
            "https://bark.example.com/my-key/t/b",
            "末尾斜杠没去掉会拼出双斜杠"
        );
    }

    #[test]
    fn unknown_provider_is_rejected_loudly() {
        let mut cfg = cfg();
        cfg.provider = "serverchan".to_string();
        let error = BarkPusher::from_config(&cfg).expect_err("不该接受未知 provider");
        assert!(
            error.to_string().contains("serverchan"),
            "错误信息要点出哪个不支持"
        );
    }

    #[test]
    fn empty_device_key_is_rejected() {
        let mut cfg = cfg();
        cfg.device_key = "   ".to_string();
        assert!(BarkPusher::from_config(&cfg).is_err());
    }

    #[test]
    fn silent_pusher_never_goes_online() {
        assert!(SilentPusher.push("标题", "正文").is_ok());
    }
}
