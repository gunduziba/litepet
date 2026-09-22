//! 行为层：把「协议事件 + 仲裁状态」翻译成「播哪个动画 + 出什么气泡」。
//!
//! 权威契约见 `docs/PET-PACK.md` §4。两条路径：
//!
//! - **规则表**（包里有 `litepet.behavior`）：按 `docs/PET-PACK.md` §4.3 求值，
//!   `on` 匹配协议事件 `type`，首个匹配的规则生效。
//! - **Codex 降级**（纯 Codex 包，无 `litepet` 键）：走 §4.4 的常量映射表。
//!
//! 本模块不含 IO 与计时，全部可单测。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::alert::sound::{SEMANTIC_DONE, SEMANTIC_FAILED};
use crate::alert::AlertSpec;
use crate::protocol::BubbleKind;

/// `behavior.idleTimeoutMs` 缺省值（`docs/PET-PACK.md` §4.2）。
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 90_000;

/// 我们这层扩展支持的 `schemaVersion`。
pub const SUPPORTED_SCHEMA_VERSION: u64 = 1;

/// 仲裁产出的舞台状态；组名与 `behavior.groups` 的键对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 空闲。
    Idle,
    /// agent 正在干活。
    Working,
    /// 长时间无事件。
    Resting,
    /// 正向反馈（成功）。
    Success,
    /// 负向反馈（失败）。
    Failure,
    /// 庆祝。
    ///
    /// 保留：`docs/PET-PACK.md` §4.4 的降级映射表把「庆祝」与「成功」列为两个
    /// 不同语义（分别映射 `wave`/`waving` 与 `bounce`/`jumping`）。当前状态机只
    /// 产出 `Success`（`docs/PROTOCOL.md` §5 的 `agent.end(success)`），本变体留给
    /// 后续区分「普通成功」与「整轮任务完成」时使用。
    #[allow(dead_code)]
    Celebrate,
}

impl Stage {
    /// 该舞台对应的组名（`docs/PROTOCOL.md` §9.1）。
    pub fn group(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Resting => "resting",
            Self::Success | Self::Failure | Self::Celebrate => "feedback",
        }
    }

    /// 降级映射表里的键（`docs/PET-PACK.md` §4.4）。
    fn fallback_key(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Resting => "resting",
            Self::Success => "feedback.success",
            Self::Failure => "feedback.failure",
            Self::Celebrate => "feedback.celebrate",
        }
    }
}

/// §4.4 降级映射：我们的状态 → Codex 包里的候选动画（按序取首个存在的）。
const CODEX_FALLBACK: &[(&str, &[&str])] = &[
    ("idle", &["idle"]),
    ("working", &["running"]),
    ("resting", &["waiting"]),
    ("feedback.success", &["bounce", "jumping"]),
    ("feedback.failure", &["sad", "failed"]),
    ("feedback.celebrate", &["wave", "waving"]),
];

/// 纯 Codex 包的默认规格：出声 + 系统通知 + 允许推手机。
///
/// 「是否真的推」不在这里定：那要看人到底在不在电脑前，是 [`crate::alert::plan`] 的事。
fn fallback_spec(sound: &str) -> AlertSpec {
    AlertSpec {
        sound: Some(sound.to_string()),
        desktop: true,
        push: true,
    }
}

/// 纯 Codex 包（没有 `litepet.behavior` 规则表）的默认提醒。
///
/// 规则表本就是可选的，但「整轮干完了」这类语义是跳包通用的。少了这几条，
/// 提醒链路对纯 Codex 包就是死的——用户会以为功能坏了，而不是以为“这个包没声”。
///
/// 音效用**语义名**而不是某个平台的具体名字。
///
/// 这一条不是风格问题：写 `"Glass"` 只有 macOS 认得，而解析链里
/// 「应用自带兑底」那一层是按语义名存的（`assets/sounds/@done.wav`），
/// 于是 Windows 上纯 Codex 包的整轮结束提醒会完全静音——
/// 而“没声音”也恰好是用户提单时报的那个问题。
/// 语义名的解析顺序见 [`crate::alert::sound::resolve`]。
fn fallback_alert(event: &Event<'_>) -> Option<AlertSpec> {
    match event.kind {
        // 整轮结束且不会自动继续：用户最需要被告知的一件事。
        event_names::AGENT_SETTLED => Some(fallback_spec(SEMANTIC_DONE)),
        // 一轮以失败告终。成功不提醒：屏幕上本来就在动，再响会很快变噪声。
        event_names::AGENT_END
            if event.fields.get("success").and_then(Value::as_bool) == Some(false) =>
        {
            Some(fallback_spec(SEMANTIC_FAILED))
        }
        _ => None,
    }
}

/// 事件名常量：直接用字符串字面量容易静静嗄死去，这里集中一处。
///
/// 值与 [`crate::protocol::rule_event`] 给出的规则事件名一致。
mod event_names {
    /// `agent/settled` → `agent.settled`。
    pub const AGENT_SETTLED: &str = "agent.settled";
    /// `agent/end` → `agent.end`。
    pub const AGENT_END: &str = "agent.end";
}

/// 规则表给出的播放目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Play {
    /// 播放具名动画（必须已在 `animations` 里声明）。
    Animation(String),
    /// 播放某个组，组内按轮换规则选用。
    Group(String),
}

/// 一条规则里的气泡声明。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BubbleSpec {
    /// 气泡类别，缺省 `info`。
    #[serde(default = "default_bubble_kind")]
    pub kind: BubbleKind,
    /// 正文，支持 `{字段名}` 插值。
    pub text: String,
}

/// `BubbleKind` 没有 `Default`，这里给出规则表用的缺省值。
fn default_bubble_kind() -> BubbleKind {
    BubbleKind::Info
}

/// 单条行为规则（`docs/PET-PACK.md` §4.3）。
#[derive(Debug, Clone, Deserialize)]
struct Rule {
    /// 协议事件 `type`。
    on: String,
    /// 对事件字段的等值/存在性匹配；缺省 = 总是匹配。
    #[serde(default)]
    when: Option<Value>,
    /// 播放目标；省略表示不改动画。
    #[serde(default)]
    play: Option<String>,
    /// 气泡；省略表示不出气泡。
    #[serde(default)]
    bubble: Option<BubbleSpec>,
    /// 提醒（声音/系统通知/手机推送）；省略表示这组规则不提醒。
    #[serde(default)]
    alert: Option<AlertSpec>,
}

/// 一条事件的规则求值结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// 显式指定的播放目标；`None` 表示不改当前动画。
    pub play: Option<Play>,
    /// 要出的气泡。
    pub bubble: Option<BubbleSpec>,
    /// 要发的提醒；`None` 表示这条规则不提醒。
    pub alert: Option<AlertSpec>,
}

/// 一次可被规则匹配的事件。
#[derive(Debug, Clone)]
pub struct Event<'a> {
    /// 协议事件 `type`。
    pub kind: &'a str,
    /// 事件负载，用于 `when` 匹配与 `{字段}` 插值。
    pub fields: Value,
}

impl<'a> Event<'a> {
    /// 从信封类型与负载构造。
    pub fn new(kind: &'a str, fields: Value) -> Self {
        Self { kind, fields }
    }
}

/// 已解析的行为配置。
#[derive(Debug, Clone)]
enum Mode {
    /// 包自带规则表。
    Rules {
        /// `behavior.idleTimeoutMs`。
        idle_timeout_ms: u64,
        /// 组名 → 动画名列表。
        groups: BTreeMap<String, Vec<String>>,
        /// 规则表，顺序敏感。
        rules: Vec<Rule>,
    },
    /// 纯 Codex 包，走 §4.4 常量映射。
    CodexFallback,
}

/// 行为层入口。
#[derive(Debug, Clone)]
pub struct Behavior {
    /// 当前生效的路径。
    mode: Mode,
}

impl Behavior {
    /// 从 `pet.json` 的 `litepet` 扩展构造（`docs/PET-PACK.md` §4.2）。
    ///
    /// `known` 为该包实际声明的动画名集合，用于校验 `play` 与 `groups` 的引用。
    /// 传 `None` 表示包里没有 `litepet` 键 → 走 Codex 降级。
    pub fn new(litepet: Option<&Value>, known: &BTreeSet<String>) -> Result<Self> {
        let Some(litepet) = litepet else {
            return Ok(Self {
                mode: Mode::CodexFallback,
            });
        };
        // 前向兼容必须硬失败：v1 的 daemon 读不懂 v2 的 `behavior` 语义，
        // 静默按 v1 解释会出错包的动画，不如直接报错让用户升级。
        let schema = litepet
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .unwrap_or(SUPPORTED_SCHEMA_VERSION);
        if schema > SUPPORTED_SCHEMA_VERSION {
            bail!(
                "litepet.schemaVersion={schema} 高于本 daemon 支持的 {SUPPORTED_SCHEMA_VERSION}，请升级 litepet"
            );
        }
        // 有 `litepet` 但没有 `behavior`：仍是纯 Codex 包，走降级。
        let Some(behavior) = litepet.get("behavior") else {
            return Ok(Self {
                mode: Mode::CodexFallback,
            });
        };
        let idle_timeout_ms = behavior
            .get("idleTimeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_MS);
        if idle_timeout_ms == 0 {
            bail!("behavior.idleTimeoutMs 必须大于 0");
        }
        let groups = parse_groups(behavior.get("groups"), known)?;
        let rules = parse_rules(behavior.get("rules"), known, &groups)?;
        Ok(Self {
            mode: Mode::Rules {
                idle_timeout_ms,
                groups,
                rules,
            },
        })
    }

    /// 无事件多久进入 `resting`。
    pub fn idle_timeout(&self) -> Duration {
        match &self.mode {
            Mode::Rules {
                idle_timeout_ms, ..
            } => Duration::from_millis(*idle_timeout_ms),
            Mode::CodexFallback => Duration::from_millis(DEFAULT_IDLE_TIMEOUT_MS),
        }
    }

    /// 该包是否自带规则表。
    pub fn has_rules(&self) -> bool {
        matches!(self.mode, Mode::Rules { .. })
    }

    /// 某个组的成员动画名；该组不存在（或走降级路径）时为 `None`。
    pub fn group_members(&self, group: &str) -> Option<&[String]> {
        match &self.mode {
            Mode::Rules { groups, .. } => groups.get(group).map(Vec::as_slice),
            Mode::CodexFallback => None,
        }
    }

    /// 按规则表求值一次事件（`docs/PET-PACK.md` §4.3：首个匹配生效）。
    ///
    /// 纯 Codex 包没有规则表，动画、气泡都不管，但仍会给出跨包通用的默认提醒。
    pub fn resolve(&self, event: &Event<'_>) -> Resolution {
        let Mode::Rules { rules, .. } = &self.mode else {
            return Resolution {
                play: None,
                bubble: None,
                alert: fallback_alert(event),
            };
        };
        rules
            .iter()
            .find(|rule| rule_matches(rule, event))
            .map(|rule| Resolution {
                play: rule.play.as_deref().and_then(parse_play),
                bubble: rule.bubble.as_ref().map(|spec| BubbleSpec {
                    kind: spec.kind,
                    text: interpolate(&spec.text, &event.fields),
                }),
                alert: rule.alert.clone(),
            })
            .unwrap_or_default()
    }

    /// 把仲裁出的舞台映射为播放目标。
    ///
    /// 规则表路径返回该舞台所属的组；Codex 降级路径返回 §4.4 映射表里首个存在的动画。
    pub fn play_for(&self, stage: Stage, known: &BTreeSet<String>) -> Option<Play> {
        match &self.mode {
            Mode::Rules { groups, .. } => groups
                .contains_key(stage.group())
                .then(|| Play::Group(stage.group().to_string())),
            Mode::CodexFallback => {
                let key = stage.fallback_key();
                let candidates = CODEX_FALLBACK
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, list)| *list)?;
                candidates
                    .iter()
                    .find(|name| known.contains(**name))
                    .map(|name| Play::Animation((*name).to_string()))
            }
        }
    }
}

/// 解析 `behavior.groups`，并校验每个动画都存在。
fn parse_groups(
    raw: Option<&Value>,
    known: &BTreeSet<String>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let Some(raw) = raw else {
        return Ok(BTreeMap::new());
    };
    let map: BTreeMap<String, Vec<String>> =
        serde_json::from_value(raw.clone()).context("behavior.groups 应为 组名 → 动画名数组")?;
    for (group, animations) in &map {
        if animations.is_empty() {
            bail!("behavior.groups.{group} 不能为空数组");
        }
        for animation in animations {
            if !known.contains(animation) {
                bail!("behavior.groups.{group} 引用了不存在的动画：{animation}");
            }
        }
    }
    Ok(map)
}

/// 解析 `behavior.rules`，并校验 `play` 与组引用。
fn parse_rules(
    raw: Option<&Value>,
    known: &BTreeSet<String>,
    groups: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<Rule>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let rules: Vec<Rule> =
        serde_json::from_value(raw.clone()).context("behavior.rules 应为规则数组")?;
    for rule in &rules {
        if rule.on.trim().is_empty() {
            bail!("behavior.rules 存在空的 on 字段");
        }
        if let Some(bubble) = rule.bubble.as_ref() {
            // 线格式允许未知类别（适配器可能先于 daemon 引入），但**包配置不允许**：
            // 规则里的 kind 是「声明」，拼错就该在加载期报错，而不是留一条永远只能
            // 降级显示、看起来却像没问题的规则（docs/PET-PACK.md §0.1）。
            if bubble.kind == BubbleKind::Unknown {
                bail!("规则 {on} 的 bubble.kind 不是已知类别", on = rule.on);
            }
        }
        if let Some(alert) = rule.alert.as_ref() {
            alert.validate(&format!("规则 {} 的", rule.on))?;
            // 三个通道全关的 `alert` 等于什么都没写。不报错的话它会静静消失，
            // 而作者以为自己在「暂时关掉提醒」，实际是埋了个永远不响的坑。
            if alert.is_inert() {
                bail!(
                    "规则 {on} 的 alert 三个通道全部关闭，等于没写；请删掉 alert 键",
                    on = rule.on
                );
            }
        }
        let Some(target) = rule.play.as_deref() else {
            continue;
        };
        match parse_play(target) {
            None => bail!("behavior.rules 的 play 不能为空"),
            Some(Play::Animation(name)) if !known.contains(&name) => {
                bail!("规则 {on} 的 play 引用了不存在的动画：{name}", on = rule.on)
            }
            Some(Play::Group(name)) if !groups.contains_key(&name) => {
                bail!("规则 {on} 的 play 引用了不存在的组：{name}", on = rule.on)
            }
            Some(_) => {}
        }
    }
    Ok(rules)
}

/// `play` 取值解析：`group:<名>` 或动画 id。
fn parse_play(raw: &str) -> Option<Play> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.strip_prefix("group:") {
        Some(group) if !group.trim().is_empty() => Some(Play::Group(group.trim().to_string())),
        Some(_) => None,
        None => Some(Play::Animation(trimmed.to_string())),
    }
}

/// `when` 匹配：每个键都要等值命中；值为 `null` 时只要求字段存在。
fn rule_matches(rule: &Rule, event: &Event<'_>) -> bool {
    if rule.on != event.kind {
        return false;
    }
    let Some(when) = rule.when.as_ref() else {
        return true;
    };
    let Some(expected) = when.as_object() else {
        return false;
    };
    expected
        .iter()
        .all(|(key, want)| match event.fields.get(key) {
            Some(actual) if want.is_null() => !actual.is_null(),
            Some(actual) => actual == want,
            None => false,
        })
}

/// `{字段名}` 插值；未知字段原样保留，便于发现拼写错误。
fn interpolate(template: &str, fields: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        match fields.get(key).and_then(scalar_text) {
            Some(value) => out.push_str(&value),
            None => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 把 JSON 标量取成展示文本；对象与数组不参与插值。
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一套够用的动画名集合，覆盖 Codex 默认表里的相关项。
    fn known() -> BTreeSet<String> {
        [
            "idle",
            "working",
            "rest_tea",
            "rest_sleep",
            "celebrate",
            "sad",
            "running",
            "waiting",
            "bounce",
            "jumping",
            "wave",
        ]
        .iter()
        .map(|name| (*name).to_string())
        .collect()
    }

    /// 把一段 `behavior` 配置包成完整的 `litepet` 扩展对象。
    fn litepet(behavior: Value) -> Value {
        json!({ "schemaVersion": 1, "behavior": behavior })
    }

    fn rules_behavior() -> Behavior {
        let behavior = json!({
            "idleTimeoutMs": 1234,
            "groups": {
                "idle": ["idle"],
                "working": ["working"],
                "resting": ["rest_tea", "rest_sleep"],
                "feedback": ["celebrate", "sad"]
            },
            "rules": [
                { "on": "agent.start", "play": "group:working",
                  "bubble": { "kind": "status", "text": "开始工作" } },
                { "on": "agent.end", "when": { "success": true },
                  "play": "celebrate", "bubble": { "kind": "success", "text": "任务完成" } },
                { "on": "agent.end", "when": { "success": false },
                  "play": "sad", "bubble": { "kind": "info", "text": "任务失败" } },
                { "on": "tool.start",
                  "bubble": { "kind": "tool", "text": "{toolName}" } }
            ]
        });
        Behavior::new(Some(&litepet(behavior)), &known()).expect("规则表应能解析")
    }

    #[test]
    fn absent_extension_uses_codex_fallback() {
        let behavior = Behavior::new(None, &known()).expect("应降级成功");
        assert!(!behavior.has_rules());
        assert_eq!(
            behavior.idle_timeout(),
            Duration::from_millis(DEFAULT_IDLE_TIMEOUT_MS)
        );
        let known = known();
        assert_eq!(
            behavior.play_for(Stage::Working, &known),
            Some(Play::Animation("running".to_string()))
        );
        assert_eq!(
            behavior.play_for(Stage::Success, &known),
            Some(Play::Animation("bounce".to_string()))
        );
        assert_eq!(
            behavior.play_for(Stage::Failure, &known),
            Some(Play::Animation("sad".to_string()))
        );
    }

    /// 纯 Codex 包的默认提醒必须用**语义名**，不能写平台的具体音效名。
    ///
    /// 这条是用例而不是注释就能说清的：解析链里「应用自带兑底」那一层是按
    /// 语义名存的（`assets/sounds/@done.wav`），写成 `"Glass"` 就绕过了它，
    /// 于是 Windows 上整轮结束会完全静音——用户报的“没声音”正是这个。
    #[test]
    fn fallback_alert_uses_semantic_names() {
        let behavior = Behavior::new(None, &known()).expect("应降级成功");
        let settled = behavior.resolve(&Event::new("agent.settled", json!({})));
        assert_eq!(
            settled.alert.and_then(|spec| spec.sound).as_deref(),
            Some("@done"),
            "整轮结束应走 @done（用户自选 → 自带兑底 → 系统）"
        );
        let failed = behavior.resolve(&Event::new("agent.end", json!({ "success": false })));
        assert_eq!(
            failed.alert.and_then(|spec| spec.sound).as_deref(),
            Some("@failed"),
        );
        // 成功不提醒：屏幕上本来就在动。
        let ok = behavior.resolve(&Event::new("agent.end", json!({ "success": true })));
        assert!(ok.alert.is_none());
    }

    /// 有规则表的包以规则表为准：默认提醒不该插进去。
    #[test]
    fn rules_pack_does_not_get_the_codex_alert() {
        let behavior = rules_behavior();
        let settled = behavior.resolve(&Event::new("agent.settled", json!({})));
        assert!(settled.alert.is_none(), "规则表未命中时不该拿默认提醒填坑");
    }

    #[test]
    fn codex_fallback_skips_missing_candidates() {
        // 只有 jumping，没有 bounce，应退到 jumping。
        let known: BTreeSet<String> = ["idle", "jumping"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let behavior = Behavior::new(None, &known).expect("应降级成功");
        assert_eq!(
            behavior.play_for(Stage::Success, &known),
            Some(Play::Animation("jumping".to_string()))
        );
        // 一个候选都没有时必须返回 None，而不是瞎播。
        assert_eq!(behavior.play_for(Stage::Working, &known), None);
    }

    #[test]
    fn rules_path_returns_group_for_stage() {
        let behavior = rules_behavior();
        assert!(behavior.has_rules());
        assert_eq!(behavior.idle_timeout(), Duration::from_millis(1234));
        let known = known();
        assert_eq!(
            behavior.play_for(Stage::Resting, &known),
            Some(Play::Group("resting".to_string()))
        );
        assert_eq!(
            behavior.play_for(Stage::Success, &known),
            Some(Play::Group("feedback".to_string()))
        );
    }

    #[test]
    fn first_matching_rule_wins_and_when_is_honoured() {
        let behavior = rules_behavior();
        let success = Event::new("agent.end", json!({ "success": true }));
        let resolution = behavior.resolve(&success);
        assert_eq!(
            resolution.play,
            Some(Play::Animation("celebrate".to_string()))
        );
        assert_eq!(resolution.bubble.expect("应出气泡").text, "任务完成");

        let failure = Event::new("agent.end", json!({ "success": false }));
        assert_eq!(
            behavior.resolve(&failure).play,
            Some(Play::Animation("sad".to_string()))
        );
    }

    #[test]
    fn rule_without_play_keeps_animation_and_interpolates_bubble() {
        let behavior = rules_behavior();
        let event = Event::new("tool.start", json!({ "toolName": "bash" }));
        let resolution = behavior.resolve(&event);
        assert_eq!(resolution.play, None, "省略 play 表示不改动画");
        let bubble = resolution.bubble.expect("应出气泡");
        assert_eq!(bubble.text, "bash");
        assert_eq!(bubble.kind, BubbleKind::Tool);
    }

    #[test]
    fn unmatched_event_yields_nothing() {
        let behavior = rules_behavior();
        let event = Event::new("bubble", json!({ "text": "hi" }));
        assert_eq!(behavior.resolve(&event), Resolution::default());
    }

    #[test]
    fn unknown_animation_in_rules_is_rejected() {
        let behavior = json!({
            "rules": [{ "on": "agent.start", "play": "nope" }]
        });
        let err = Behavior::new(Some(&litepet(behavior)), &known()).expect_err("应被拒");
        assert!(format!("{err}").contains("nope"), "实际为 {err}");
    }

    #[test]
    fn unknown_group_in_rules_is_rejected() {
        let behavior = json!({
            "rules": [{ "on": "agent.start", "play": "group:nope" }]
        });
        let err = Behavior::new(Some(&litepet(behavior)), &known()).expect_err("应被拒");
        assert!(format!("{err}").contains("不存在的组"), "实际为 {err}");
    }

    #[test]
    fn unknown_bubble_kind_in_rules_is_rejected() {
        // 线格式对未知 kind 是降级兼容，包配置则必须硬拒：
        // 前者是别人的新版本，后者是自己写的声明，拼错不能静默。
        let behavior = json!({
            "rules": [{ "on": "agent.start", "bubble": { "kind": "celebrate", "text": "交卷" } }]
        });
        let err = Behavior::new(Some(&litepet(behavior)), &known()).expect_err("应被拒");
        assert!(format!("{err}").contains("不是已知类别"), "实际为 {err}");
    }

    #[test]
    fn groups_referencing_missing_animation_are_rejected() {
        let behavior = json!({ "groups": { "idle": ["ghost"] } });
        let err = Behavior::new(Some(&litepet(behavior)), &known()).expect_err("应被拒");
        assert!(format!("{err}").contains("ghost"), "实际为 {err}");
    }

    #[test]
    fn empty_idle_timeout_is_rejected() {
        let behavior = json!({ "idleTimeoutMs": 0 });
        assert!(Behavior::new(Some(&litepet(behavior)), &known()).is_err());
    }

    /// 未来版本的 `schemaVersion` 必须硬失败，不能按 v1 静默解释。
    #[test]
    fn future_schema_version_is_rejected() {
        let config = json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION + 1,
            "behavior": { "groups": { "idle": ["idle"] } }
        });
        let err = Behavior::new(Some(&config), &known()).expect_err("应被拒");
        assert!(format!("{err}").contains("schemaVersion"), "实际为 {err}");
    }

    /// 缺 `schemaVersion` 视为当前版本；显式写当前版本也应通过。
    #[test]
    fn current_or_absent_schema_version_is_accepted() {
        let absent = json!({ "behavior": { "groups": { "idle": ["idle"] } } });
        assert!(Behavior::new(Some(&absent), &known()).is_ok());
        let current = json!({
            "schemaVersion": SUPPORTED_SCHEMA_VERSION,
            "behavior": { "groups": { "idle": ["idle"] } }
        });
        assert!(Behavior::new(Some(&current), &known()).is_ok());
    }

    /// 有 `litepet` 但无 `behavior`，仍应走 Codex 降级而不是报错。
    #[test]
    fn extension_without_behavior_falls_back() {
        let config = json!({ "schemaVersion": 1, "license": "MIT" });
        let behavior = Behavior::new(Some(&config), &known()).expect("应降级成功");
        assert!(!behavior.has_rules());
    }

    #[test]
    fn null_in_when_means_existence_only() {
        let behavior = json!({
            "rules": [{ "on": "tool.end", "when": { "isError": null }, "play": "sad" }]
        });
        let behavior = Behavior::new(Some(&litepet(behavior)), &known()).expect("应能解析");
        assert_eq!(
            behavior
                .resolve(&Event::new("tool.end", json!({ "isError": true })))
                .play,
            Some(Play::Animation("sad".to_string()))
        );
        // 字段缺失 → 不匹配。
        assert_eq!(
            behavior
                .resolve(&Event::new("tool.end", json!({ "toolName": "bash" })))
                .play,
            None
        );
    }
}
