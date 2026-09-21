//! 仲裁层：多宿主状态机 + 气泡队列（`docs/PROTOCOL.md` §5、§6）。
//!
//! 本模块只做决策，不做 IO、不碰时钟：所有时间点都由调用方以 `Instant` 传入，
//! 因此每条规则都能用确定的时刻单测。
//!
//! 职责边界（`docs/PET-PACK.md` §4.3）：
//! - 规则表决定「播什么动画」（`Behavior::resolve`）；
//! - 仲裁决定「谁说话、宠物处于哪个舞台」。两者互不干涉。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::behavior::{Behavior, BubbleSpec, Play, Stage};
use crate::protocol::{truncate, BubbleKind, DisplayBubble, DisplayDirective, BUBBLE_TEXT_LIMIT};

/// 反馈动画的展示时长（`docs/PROTOCOL.md` §5：`celebrating ≤ 8s`）。
pub const FEEDBACK_MS: u64 = 8_000;

/// 气泡缺省存活时长（`docs/PROTOCOL.md` §6）。
pub const DEFAULT_BUBBLE_TTL_MS: u64 = 4_000;

/// 气泡存活时长下限（`docs/PROTOCOL.md` §6）。
pub const MIN_BUBBLE_TTL_MS: u64 = 500;

/// 气泡存活时长上限（`docs/PROTOCOL.md` §6）。
pub const MAX_BUBBLE_TTL_MS: u64 = 30_000;

/// 多宿主同时活跃时，气泡每隔多久换一个宿主展示（`docs/PROTOCOL.md` §5）。
pub const BUBBLE_ROTATE_MS: u64 = 3_000;

/// 同一组内多个动画每隔多久轮换一次（`docs/PET-PACK.md` §4.2「组内按轮换规则选用」）。
pub const GROUP_ROTATE_MS: u64 = 5_000;

/// 规则表显式指定的播放目标。
///
/// `docs/PET-PACK.md` §4.3 只规定「省略 `play` = 不改动画」，未规定存活期。
/// 本实现定为：
/// - 伴随状态迁移的规则（`agent.start` / `agent.end`）—— 永久，直到下次状态变迁自动清除；
/// - 其他事件（`tool.*` / `bubble`）—— 瞬时，与反馈窗口同长，到时回落舞台动画。
///
/// 这样一条 `tool.start` 规则不会把宠物永久钉在某个动作上。
#[derive(Debug, Clone)]
struct Override {
    /// 播放目标。
    play: Play,
    /// `None` 表示永久；`Some` 表示到时失效。
    expires_at: Option<Instant>,
}

/// 单个宿主的状态机状态。`resting` 是全局概念，不存在于单宿主上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostStage {
    /// 空闲。
    Idle,
    /// agent 正在干活。
    Working,
    /// 反馈动画播放中，到 `feedback_until` 后回 `Idle`。
    Feedback(Stage),
}

impl HostStage {
    /// 转成行为层能理解的舞台；`Idle`/`Working` 之外只有反馈类。
    fn as_stage(self) -> Stage {
        match self {
            Self::Idle => Stage::Idle,
            Self::Working => Stage::Working,
            Self::Feedback(stage) => stage,
        }
    }
}

/// 一条存活着的气泡。
#[derive(Debug, Clone)]
struct LiveBubble {
    /// 类别（同时是优先级）。
    kind: BubbleKind,
    /// 正文，未加徽章前缀。
    text: String,
    /// 过期时刻。
    expires_at: Instant,
    /// 同键 TTL 内不重发的键（`tool.start` 用工具名）。
    dedupe_key: Option<String>,
}

/// 单个宿主的全部状态。
#[derive(Debug)]
struct HostEntry {
    /// 当前状态机状态。
    stage: HostStage,
    /// 反馈状态何时结束。
    feedback_until: Option<Instant>,
    /// 最近一次收到该宿主任何帧的时刻。
    /// 最近一次收到该宿主**真实活动**的时刻（不影响存活判定）。
    last_event: Instant,
    /// 最近一次收到该宿主**任何**消息（含心跳）的时刻，只用于回收死宿主。
    ///
    /// 与 `last_event` 分开的理由：心跳不算「干活」，如果心跳也刷新 `last_event`，
    /// 那么只要宿主定期 ping，`idle_for` 就永远不会超过 `idleTimeoutMs`，
    /// 宠物便再也进不了 `resting`。
    last_seen: Instant,
    /// 规则表显式指定的播放目标。
    override_play: Option<Override>,
    /// 当前气泡。
    bubble: Option<LiveBubble>,
}

/// 组轮换游标。
#[derive(Debug, Clone, Copy, Default)]
struct GroupCursor {
    /// 上次轮换时刻。
    since: Option<Instant>,
    /// 当前索引。
    index: usize,
}

/// 多宿主仲裁器。
#[derive(Debug, Default)]
pub struct Arbiter {
    /// 已注册宿主，键为宿主标识。
    hosts: BTreeMap<String, HostEntry>,
    /// 各组的轮换游标。
    cursors: BTreeMap<String, GroupCursor>,
    /// 气泡换宿主展示的游标。
    rotate_since: Option<Instant>,
    /// 气泡轮换到的序号。
    rotate_index: usize,
}

impl Arbiter {
    /// 新建仲裁器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个宿主（`host.hello`）。
    ///
    /// `pid` 与版本信息不进仲裁状态：它们只用于日志，由 [`crate::session`] 在
    /// 接入时直接打印，不必在这里冗余一份没人读的副本。
    pub fn register(&mut self, host: &str, now: Instant) {
        self.hosts.insert(
            host.to_string(),
            HostEntry {
                stage: HostStage::Idle,
                feedback_until: None,
                last_event: now,
                last_seen: now,
                override_play: None,
                bubble: None,
            },
        );
        self.rotate_since = Some(now);
    }

    /// 注销一个宿主（断连时立即调用，`docs/PROTOCOL.md` §7）。
    pub fn unregister(&mut self, host: &str) {
        self.hosts.remove(host);
    }

    /// 该宿主是否已登记。
    pub fn is_registered(&self, host: &str) -> bool {
        self.hosts.contains_key(host)
    }

    /// 当前已登记的宿主数。
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    /// 是否没有任何宿主。
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// `agent.start`：进入 working（状态变迁会清掉旧的显式 `play`）。
    pub fn on_agent_start(&mut self, host: &str, now: Instant) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.stage = HostStage::Working;
            entry.feedback_until = None;
            entry.override_play = None;
            entry.last_event = now;
        }
    }

    /// `agent.end`：成功/失败进入对应反馈，其余不动（`docs/PROTOCOL.md` §5）。
    pub fn on_agent_end(&mut self, host: &str, success: bool, now: Instant) {
        let stage = if success {
            Stage::Success
        } else {
            Stage::Failure
        };
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.stage = HostStage::Feedback(stage);
            entry.feedback_until = Some(now + Duration::from_millis(FEEDBACK_MS));
            entry.override_play = None;
            entry.last_event = now;
        }
    }

    /// `agent/settled`：整轮结束且不会自动继续，进庆祝（`docs/PROTOCOL.md` §5）。
    ///
    /// 它与 `agent.end` 是**两个语义**，不能合并：一轮里每次工具回来的 `agent/end`
    /// 都只算「这一步成了」，而 `agent/settled` 是「不用再等我了」。
    /// 合成一个的话，「在键盘前等下一轮」的人会看到宠物反复庆祝。
    pub fn on_agent_settled(&mut self, host: &str, success: Option<bool>, now: Instant) {
        // `None` 意思是宿主没报过 `agent/end`。这时候按「完成」处理：
        // `agent/settled` 本身就说明「不用再等我了」，给个失败的脸色是错的。
        let stage = match success {
            Some(false) => Stage::Failure,
            _ => Stage::Celebrate,
        };
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.stage = HostStage::Feedback(stage);
            entry.feedback_until = Some(now + Duration::from_millis(FEEDBACK_MS));
            entry.override_play = None;
            entry.last_event = now;
        }
    }

    /// `tool.start`：**不改变** working 状态，只更新气泡（`docs/PROTOCOL.md` §5）。
    ///
    /// `spec` 来自规则表，`fallback_text` 来自宿主自带的 `bubble` 字段。
    pub fn on_tool_start(
        &mut self,
        host: &str,
        tool_name: &str,
        spec: Option<&BubbleSpec>,
        fallback_text: Option<&str>,
        now: Instant,
    ) {
        let (kind, text) = match (spec, fallback_text) {
            (Some(spec), _) => (spec.kind, spec.text.clone()),
            (None, Some(text)) => (BubbleKind::Tool, text.to_string()),
            (None, None) => (BubbleKind::Tool, tool_name.to_string()),
        };
        self.set_bubble(
            host,
            kind,
            &text,
            DEFAULT_BUBBLE_TTL_MS,
            Some(tool_name),
            now,
        );
    }

    /// `tool.end`：只有两种情况下动气泡。
    ///
    /// `docs/PROTOCOL.md` §9.3 的事件表**只有 `tool.end`(isError) 一行**，没有「成功结束」行，
    /// 而 `SPEC.md:68` 的 `tool.end` 也只带 `toolName`——`toolName` 是**去重键**（`SPEC.md:85`），
    /// 不是展示文本。所以成功结束时**不动气泡**：否则会把 `tool.start` 带进来的、
    /// 信息量更大的 `bubble` 文本（如 `ls -la`）冲成工具名（如 `bash`）。
    pub fn on_tool_end(
        &mut self,
        host: &str,
        tool_name: &str,
        is_error: bool,
        spec: Option<&BubbleSpec>,
        now: Instant,
    ) {
        match (spec, is_error) {
            // 规则表给了气泡，规则优先。
            (Some(spec), _) => {
                self.set_bubble(
                    host,
                    spec.kind,
                    &spec.text,
                    DEFAULT_BUBBLE_TTL_MS,
                    Some(tool_name),
                    now,
                );
            }
            // `tool.end`(isError) → `error` 级气泡（`docs/PROTOCOL.md` §9.3）。
            // 无规则时只有工具名可作正文；`error` 优先级最高，可抢占未过期的低优先级气泡。
            (None, true) => {
                self.set_bubble(
                    host,
                    BubbleKind::Error,
                    tool_name,
                    DEFAULT_BUBBLE_TTL_MS,
                    None,
                    now,
                );
            }
            // 成功结束：只刷新存活时间并释放去重键（让该工具再次开始能重新出气泡）。
            (None, false) => self.release_bubble_dedupe(host, now),
        }
    }

    /// 释放当前气泡的去重键，并刷新宿主存活时间，不改气泡内容。
    fn release_bubble_dedupe(&mut self, host: &str, now: Instant) {
        let Some(entry) = self.hosts.get_mut(host) else {
            return;
        };
        entry.last_event = now;
        if let Some(bubble) = entry.bubble.as_mut() {
            bubble.dedupe_key = None;
        }
    }

    /// `bubble`：宿主直接发一条气泡。
    pub fn on_bubble(
        &mut self,
        host: &str,
        kind: BubbleKind,
        text: &str,
        ttl_ms: Option<u64>,
        now: Instant,
    ) {
        let ttl = ttl_ms
            .unwrap_or(DEFAULT_BUBBLE_TTL_MS)
            .clamp(MIN_BUBBLE_TTL_MS, MAX_BUBBLE_TTL_MS);
        self.set_bubble(host, kind, text, ttl, None, now);
    }

    /// 刷新存活时间：每条来自该宿主的消息（含心跳）都该调一次。
    ///
    /// 只影响「宿主是否还活着」，不产生任何显示变化：
    /// `resting` 判定看的是 `last_event`，心跳不会把睡着的宠物叫醒。
    pub fn touch(&mut self, host: &str, now: Instant) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.last_seen = now;
        }
    }

    /// 注销超时未联系的宿主，返回被注销的宿主名。
    ///
    /// HTTP 是无连接的：宿主崩掉时不会有机会发 `host/bye`，而「宿主还在」是
    /// linger 倒计时的前提。没有这个兜底，宠物会永远赖在屏幕上不退出
    /// （`docs/PROTOCOL.md` §7）。未登记的宿主不受影响。
    pub fn reap_dead(&mut self, now: Instant, timeout: Duration) -> Vec<String> {
        let dead: Vec<String> = self
            .hosts
            .iter()
            .filter(|(_, entry)| now.saturating_duration_since(entry.last_seen) >= timeout)
            .map(|(host, _)| host.clone())
            .collect();
        for host in &dead {
            self.hosts.remove(host);
        }
        dead
    }

    /// 规则表给出的显式播放目标；`expires_at` 为 `None` 表示持续到下次状态变迁。
    pub fn set_override(&mut self, host: &str, play: Play, expires_at: Option<Instant>) {
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.override_play = Some(Override { play, expires_at });
        }
    }

    /// 写入一条气泡，按 `docs/PROTOCOL.md` §6 处理去重与优先级。
    fn set_bubble(
        &mut self,
        host: &str,
        kind: BubbleKind,
        text: &str,
        ttl_ms: u64,
        dedupe_key: Option<&str>,
        now: Instant,
    ) {
        let Some(entry) = self.hosts.get_mut(host) else {
            return;
        };
        entry.last_event = now;
        if let Some(existing) = entry.bubble.as_ref() {
            let alive = existing.expires_at > now;
            if alive {
                // 同键 TTL 内不重发（`tool.start` 的 dedupeKey 语义）。
                if let (Some(existing_key), Some(new_key)) =
                    (existing.dedupe_key.as_deref(), dedupe_key)
                {
                    if existing_key == new_key {
                        return;
                    }
                }
                // 低优先级不抢占未过期的更高优先级。
                if existing.kind > kind {
                    return;
                }
            }
        }
        entry.bubble = Some(LiveBubble {
            kind,
            text: text.to_string(),
            expires_at: now + Duration::from_millis(ttl_ms),
            dedupe_key: dedupe_key.map(str::to_string),
        });
    }

    /// 推进时间：清理过期气泡、已结束的反馈状态与过期的显式 `play`。
    pub fn tick(&mut self, now: Instant) {
        for entry in self.hosts.values_mut() {
            if let Some(until) = entry.feedback_until {
                if until <= now {
                    entry.stage = HostStage::Idle;
                    entry.feedback_until = None;
                }
            }
            if entry
                .override_play
                .as_ref()
                .is_some_and(|over| over.expires_at.is_some_and(|at| at <= now))
            {
                entry.override_play = None;
            }
            if entry.bubble.as_ref().is_some_and(|b| b.expires_at <= now) {
                entry.bubble = None;
            }
        }
    }

    /// 距离最后一次任何事件过了多久。
    pub fn idle_for(&self, now: Instant) -> Duration {
        let last = self.hosts.values().map(|entry| entry.last_event).max();
        match last {
            // 一个宿主都没有时，用气泡轮换的起点兜底，避免立刻进入 resting。
            None => now.saturating_duration_since(self.rotate_since.unwrap_or(now)),
            Some(last) => now.saturating_duration_since(last),
        }
    }

    /// 当前宠物该处于哪个舞台（`docs/PROTOCOL.md` §5）。
    ///
    /// 只要还有宿主在 `working`，宠物就保持「干活」——这正是「多宿主同时 working
    /// 时宠物保持打字」的实现方式；否则按最近一次反馈；再否则超时进入 `resting`。
    fn global_stage(&self, now: Instant, behavior: &Behavior) -> Stage {
        let stages: Vec<HostStage> = self.hosts.values().map(|entry| entry.stage).collect();
        if stages.contains(&HostStage::Working) {
            return Stage::Working;
        }
        if let Some(stage) = self.latest_feedback() {
            return stage;
        }
        if self.idle_for(now) >= behavior.idle_timeout() {
            return Stage::Resting;
        }
        stages.first().map_or(Stage::Idle, |stage| stage.as_stage())
    }

    /// 最近一次进入反馈状态的宿主所对应的舞台。
    fn latest_feedback(&self) -> Option<Stage> {
        self.hosts
            .values()
            .filter_map(|entry| match entry.stage {
                HostStage::Feedback(stage) => entry.feedback_until.map(|until| (until, stage)),
                _ => None,
            })
            .max_by_key(|(until, _)| *until)
            .map(|(_, stage)| stage)
    }

    /// 某组当前的动画名；组不存在时返回 `None`。
    fn group_animation(
        &mut self,
        group: &str,
        behavior: &Behavior,
        known: &std::collections::BTreeSet<String>,
        now: Instant,
    ) -> Option<String> {
        let candidates: Vec<String> = match behavior.group_members(group) {
            Some(members) if !members.is_empty() => members.to_vec(),
            _ => known
                .iter()
                .filter(|name| name.as_str() == group)
                .cloned()
                .collect(),
        };
        if candidates.is_empty() {
            return None;
        }
        let cursor = self.cursors.entry(group.to_string()).or_default();
        let rotate = Duration::from_millis(GROUP_ROTATE_MS);
        match cursor.since {
            Some(since) if now.saturating_duration_since(since) >= rotate => {
                cursor.index = (cursor.index + 1) % candidates.len();
                cursor.since = Some(now);
            }
            None => cursor.since = Some(now),
            Some(_) => {}
        }
        candidates.get(cursor.index).cloned()
    }

    /// 计算当下该推给渲染层的指令。
    pub fn directive(
        &mut self,
        now: Instant,
        behavior: &Behavior,
        known: &std::collections::BTreeSet<String>,
    ) -> DisplayDirective {
        self.tick(now);

        // 规则表的显式 play 优先（`docs/PET-PACK.md` §4.3）。
        let explicit = self
            .hosts
            .values()
            .find_map(|entry| entry.override_play.as_ref().map(|over| over.play.clone()));
        let play = explicit.or_else(|| {
            let stage = self.global_stage(now, behavior);
            behavior.play_for(stage, known)
        });

        let animation = match play {
            Some(Play::Animation(name)) => Some(name),
            Some(Play::Group(group)) => self.group_animation(&group, behavior, known, now),
            None => None,
        }
        .unwrap_or_else(|| fallback_animation(known));

        DisplayDirective {
            animation,
            bubble: self.current_bubble(now),
        }
    }

    /// 挑出当前要展示的气泡：多宿主时按 `BUBBLE_ROTATE_MS` 轮换。
    fn current_bubble(&mut self, now: Instant) -> Option<DisplayBubble> {
        let mut live: Vec<(String, BubbleKind, String)> = Vec::new();
        for (host, entry) in &self.hosts {
            if let Some(bubble) = entry.bubble.as_ref() {
                if bubble.expires_at > now {
                    live.push((host.clone(), bubble.kind, bubble.text.clone()));
                }
            }
        }
        if live.is_empty() {
            self.rotate_since = Some(now);
            self.rotate_index = 0;
            return None;
        }
        let rotate = Duration::from_millis(BUBBLE_ROTATE_MS);
        match self.rotate_since {
            Some(since) if now.saturating_duration_since(since) >= rotate => {
                self.rotate_index = self.rotate_index.wrapping_add(1);
                self.rotate_since = Some(now);
            }
            None => self.rotate_since = Some(now),
            Some(_) => {}
        }
        let (host, kind, text) = &live[self.rotate_index % live.len()];
        // 徽章前缀不计入 48 字符正文限额（`docs/PROTOCOL.md` §6）。
        Some(DisplayBubble {
            kind: kind.as_str(),
            text: format!("[{host}] {}", truncate(text, BUBBLE_TEXT_LIMIT)),
        })
    }

    /// 距离下一次「不推进时间也会变化」还差多久。
    ///
    /// 用于让 event loop 能在空闲时也准时切到 `resting`、准时换气泡。
    pub fn next_deadline(&self, now: Instant, behavior: &Behavior) -> Duration {
        let mut next = behavior.idle_timeout();
        let mut shrink = |candidate: Duration| {
            if candidate < next {
                next = candidate;
            }
        };
        for entry in self.hosts.values() {
            if let Some(until) = entry.feedback_until {
                shrink(until.saturating_duration_since(now));
            }
            if let Some(at) = entry
                .override_play
                .as_ref()
                .and_then(|over| over.expires_at)
            {
                shrink(at.saturating_duration_since(now));
            }
            if let Some(bubble) = entry.bubble.as_ref() {
                shrink(bubble.expires_at.saturating_duration_since(now));
            }
            // 空闲到现在已过多久决定 resting 何时触发。
            let elapsed = now.saturating_duration_since(entry.last_event);
            shrink(behavior.idle_timeout().saturating_sub(elapsed));
        }
        if !self.hosts.is_empty() {
            shrink(Duration::from_millis(BUBBLE_ROTATE_MS));
        }
        next
    }
}

/// 兜底动画：包里一定有的名字，保证渲染层永远拿得到东西。
fn fallback_animation(known: &std::collections::BTreeSet<String>) -> String {
    if known.contains("idle") {
        return "idle".to_string();
    }
    known
        .iter()
        .next()
        .cloned()
        .unwrap_or_else(|| "idle".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::Behavior;
    use crate::protocol::BubbleKind;
    use std::collections::BTreeSet;

    fn known() -> BTreeSet<String> {
        [
            "idle",
            "working",
            "rest_tea",
            "rest_sleep",
            "celebrate",
            "sad",
        ]
        .iter()
        .map(|name| (*name).to_string())
        .collect()
    }

    fn behavior() -> Behavior {
        let config = serde_json::json!({
            "schemaVersion": 1,
            "behavior": {
                "idleTimeoutMs": 90_000,
                "groups": {
                    "idle": ["idle"],
                    "working": ["working"],
                    "resting": ["rest_tea", "rest_sleep"],
                    "feedback": ["celebrate", "sad"]
                }
            }
        });
        Behavior::new(Some(&config), &known()).expect("应能解析")
    }

    fn arbiter_with(host: &str, now: Instant) -> Arbiter {
        let mut arbiter = Arbiter::new();
        arbiter.register(host, now);
        arbiter
    }

    #[test]
    fn agent_start_switches_to_working() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        let known = known();

        assert_eq!(arbiter.directive(now, &behavior, &known).animation, "idle");
        arbiter.on_agent_start("pi", now + Duration::from_millis(10));
        let directive = arbiter.directive(now + Duration::from_millis(20), &behavior, &known);
        assert_eq!(directive.animation, "working");
    }

    #[test]
    fn feedback_returns_to_idle_after_deadline() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        let known = known();

        arbiter.on_agent_start("pi", now);
        arbiter.on_agent_end("pi", true, now + Duration::from_millis(10));
        let during = arbiter.directive(now + Duration::from_millis(20), &behavior, &known);
        assert_eq!(during.animation, "celebrate");

        let after = now + Duration::from_millis(10 + FEEDBACK_MS + 1);
        assert_eq!(
            arbiter.directive(after, &behavior, &known).animation,
            "idle"
        );
    }

    #[test]
    fn tool_start_does_not_leave_working() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        let known = known();

        arbiter.on_agent_start("pi", now);
        arbiter.on_tool_start("pi", "bash", None, None, now + Duration::from_millis(5));
        let directive = arbiter.directive(now + Duration::from_millis(10), &behavior, &known);
        assert_eq!(directive.animation, "working");
        assert_eq!(directive.bubble.expect("应带气泡").text, "[pi] bash");
    }

    #[test]
    fn any_working_host_keeps_pet_working() {
        let now = Instant::now();
        let mut arbiter = Arbiter::new();
        arbiter.register("pi", now);
        arbiter.register("dsh", now);
        let behavior = behavior();
        let known = known();

        arbiter.on_agent_start("pi", now);
        arbiter.on_agent_end("pi", true, now + Duration::from_millis(1));
        // pi 已进入反馈，dsh 仍在工作 → 宠物必须保持干活。
        arbiter.on_agent_start("dsh", now + Duration::from_millis(2));
        assert_eq!(
            arbiter
                .directive(now + Duration::from_millis(3), &behavior, &known)
                .animation,
            "working"
        );
    }

    #[test]
    fn idle_timeout_switches_to_resting_group() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        let known = known();

        let before = arbiter.directive(now + Duration::from_millis(89_999), &behavior, &known);
        assert_eq!(before.animation, "idle");
        let after = arbiter.directive(now + Duration::from_millis(90_001), &behavior, &known);
        assert!(
            after.animation.starts_with("rest_"),
            "实际为 {}",
            after.animation
        );
    }

    #[test]
    fn resting_group_rotates_over_time() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        let known = known();

        let base = now + Duration::from_millis(90_001);
        let first = arbiter.directive(base, &behavior, &known).animation;
        let second = arbiter
            .directive(
                base + Duration::from_millis(GROUP_ROTATE_MS + 1),
                &behavior,
                &known,
            )
            .animation;
        assert_ne!(first, second, "组内应轮换");
    }

    #[test]
    fn tool_start_dedupes_same_tool_within_ttl() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        arbiter.on_tool_start("pi", "bash", None, None, now);
        let first = arbiter.current_bubble(now).expect("应有一条气泡");
        // 尚未过期且同键 → 不重发（刷新后文本相同即视为未重发）。
        arbiter.on_tool_start("pi", "bash", None, None, now + Duration::from_millis(10));
        let second = arbiter
            .current_bubble(now + Duration::from_millis(10))
            .expect("仍有气泡");
        assert_eq!(first.text, second.text);

        // 不同工具名则覆盖。
        arbiter.on_tool_start("pi", "read", None, None, now + Duration::from_millis(20));
        let third = arbiter
            .current_bubble(now + Duration::from_millis(20))
            .expect("应有气泡");
        assert_eq!(third.text, "[pi] read");
    }

    /// 成功的 `tool.end` 不得把 `tool.start` 带进来的气泡冲成工具名。
    ///
    /// 实测回归：真机上曾观察到 `[pi] ls -la` 被 `tool.end` 覆盖成 `[pi] bash`。
    #[test]
    fn successful_tool_end_keeps_richer_bubble() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        arbiter.on_tool_start("pi", "bash", None, Some("ls -la"), now);
        arbiter.on_tool_end("pi", "bash", false, None, now + Duration::from_millis(10));
        let bubble = arbiter
            .current_bubble(now + Duration::from_millis(20))
            .expect("应有气泡");
        assert_eq!(bubble.text, "[pi] ls -la");
    }

    /// 失败的 `tool.end` 出 `error` 级气泡（`docs/PROTOCOL.md` §9.3）。
    #[test]
    fn failing_tool_end_raises_error_bubble() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        arbiter.on_tool_start("pi", "bash", None, Some("ls -la"), now);
        arbiter.on_tool_end("pi", "bash", true, None, now + Duration::from_millis(10));
        let bubble = arbiter
            .current_bubble(now + Duration::from_millis(20))
            .expect("应有气泡");
        assert_eq!(bubble.kind, "error");
        assert_eq!(bubble.text, "[pi] bash");
    }

    /// 成功的 `tool.end` 要释放去重键：同一工具再次开始应能重新出气泡。
    #[test]
    fn successful_tool_end_releases_dedupe_key() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        arbiter.on_tool_start("pi", "bash", None, Some("ls -la"), now);
        // TTL 内同键同文本：被去重，仍是同一条。
        arbiter.on_tool_start(
            "pi",
            "bash",
            None,
            Some("ls -la"),
            now + Duration::from_millis(5),
        );
        arbiter.on_tool_end("pi", "bash", false, None, now + Duration::from_millis(10));
        // 结束后同键再次开始：文本应能换成新的。
        arbiter.on_tool_start(
            "pi",
            "bash",
            None,
            Some("pytest"),
            now + Duration::from_millis(15),
        );
        let bubble = arbiter
            .current_bubble(now + Duration::from_millis(20))
            .expect("应有气泡");
        assert_eq!(bubble.text, "[pi] pytest");
    }

    /// 规则表给了气泡时，规则优先。
    #[test]
    fn rule_supplied_tool_end_bubble_wins() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let spec = BubbleSpec {
            kind: BubbleKind::Success,
            text: "工具完成".to_string(),
        };
        arbiter.on_tool_end("pi", "bash", false, Some(&spec), now);
        let bubble = arbiter.current_bubble(now).expect("应有气泡");
        assert_eq!(bubble.text, "[pi] 工具完成");
        assert_eq!(bubble.kind, "success");
    }

    #[test]
    fn higher_priority_bubble_is_not_preempted() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        arbiter.on_bubble("pi", BubbleKind::Error, "炸了", Some(10_000), now);
        arbiter.on_tool_start("pi", "bash", None, None, now + Duration::from_millis(1));
        let bubble = arbiter
            .current_bubble(now + Duration::from_millis(2))
            .expect("应有气泡");
        assert_eq!(bubble.text, "[pi] 炸了");
        assert_eq!(bubble.kind, "error");
    }

    #[test]
    fn bubble_text_is_truncated_to_limit() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let long = "字".repeat(BUBBLE_TEXT_LIMIT + 20);
        arbiter.on_bubble("pi", BubbleKind::Info, &long, None, now);
        let bubble = arbiter.current_bubble(now).expect("应有气泡");
        let body = bubble.text.trim_start_matches("[pi] ");
        assert_eq!(body.chars().count(), BUBBLE_TEXT_LIMIT);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn ttl_defaults_and_clamps() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        // 低于下限 → 抬到 500ms。
        arbiter.on_bubble("pi", BubbleKind::Info, "hi", Some(1), now);
        assert!(arbiter
            .current_bubble(now + Duration::from_millis(400))
            .is_some());
        assert!(arbiter
            .current_bubble(now + Duration::from_millis(600))
            .is_none());
    }

    #[test]
    fn bubbles_rotate_between_hosts() {
        let now = Instant::now();
        let mut arbiter = Arbiter::new();
        arbiter.register("pi", now);
        arbiter.register("dsh", now);
        arbiter.on_bubble("pi", BubbleKind::Info, "a", None, now);
        arbiter.on_bubble("dsh", BubbleKind::Info, "b", None, now);

        let first = arbiter.current_bubble(now).expect("应有气泡");
        let second = arbiter
            .current_bubble(now + Duration::from_millis(BUBBLE_ROTATE_MS + 1))
            .expect("应有气泡");
        assert_ne!(first.text, second.text, "应换宿主");
    }

    #[test]
    fn disconnect_unregisters_immediately() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        assert_eq!(arbiter.host_count(), 1);
        arbiter.unregister("pi");
        assert!(arbiter.is_empty());
        assert!(!arbiter.is_registered("pi"));
    }

    #[test]
    fn deadline_is_never_further_than_idle_timeout() {
        let now = Instant::now();
        let arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        assert!(arbiter.next_deadline(now, &behavior) <= behavior.idle_timeout());
    }

    #[test]
    fn unknown_animation_set_falls_back_to_first_entry() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let only_sad: BTreeSet<String> = ["sad"].iter().map(|s| (*s).to_string()).collect();
        // 纯 Codex 包且缺 idle：不能返回空动画名给渲染层。
        let behavior = Behavior::new(None, &only_sad).expect("应降级成功");
        assert_eq!(
            arbiter.directive(now, &behavior, &only_sad).animation,
            "sad"
        );
    }

    /// 心跳必须不能把睡着的宠物叫醒：`touch` 只动 `last_seen`。
    #[test]
    fn touch_does_not_postpone_resting() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let behavior = behavior();
        let known = known();

        // 模拟「宿主一直在线、但一直没干活」：每 20s 一次心跳，持续 2 分钟。
        let mut at = now;
        while at < now + Duration::from_secs(120) {
            at += Duration::from_secs(20);
            arbiter.touch("pi", at);
        }

        // idleTimeoutMs = 90s，心跳不该阻止进入 resting。
        assert_eq!(
            arbiter.directive(at, &behavior, &known).animation,
            "rest_tea"
        );
    }

    #[test]
    fn touch_keeps_host_alive() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        let timeout = Duration::from_secs(60);
        let at = now + Duration::from_secs(45);
        arbiter.touch("pi", at);
        assert!(arbiter
            .reap_dead(at + Duration::from_secs(10), timeout)
            .is_empty());
        assert_eq!(arbiter.host_count(), 1);
    }

    #[test]
    fn silent_host_is_reaped() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        arbiter.register("dsh", now);
        let timeout = Duration::from_secs(60);

        // dsh 在 30s 时还说了一句话，pi 一直沉默。
        arbiter.touch("dsh", now + Duration::from_secs(30));
        let dead = arbiter.reap_dead(now + Duration::from_secs(61), timeout);

        assert_eq!(dead, vec!["pi".to_string()]);
        assert!(!arbiter.is_registered("pi"));
        assert!(arbiter.is_registered("dsh"));
    }

    #[test]
    fn reaping_an_unknown_host_is_a_no_op() {
        let now = Instant::now();
        let mut arbiter = arbiter_with("pi", now);
        // 未登记的宿主不会被 touch 登记，也就没有条目可回收。
        arbiter.touch("ghost", now);
        // 超时给得远大于已过时间，让 pi 一定活下来：
        // 这样 `dead` 非空就只能是 ghost 被算进去了。
        let dead = arbiter.reap_dead(now + Duration::from_secs(600), Duration::from_secs(6_000));
        assert!(dead.is_empty(), "只应回收已登记的宿主，实际回收了 {dead:?}");
        assert_eq!(arbiter.host_count(), 1);
    }
}
